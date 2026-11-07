use anyhow::{Context, Result, ensure};
use nix::{
    sys::signal::{Signal, killpg},
    unistd::Pid,
};
use std::{
    collections::HashSet,
    io,
    process::{ExitStatus, Stdio},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::{Child, Command},
    sync::mpsc,
    time::{Instant, sleep_until},
};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Default)]
pub struct Control {
    pub cancel: CancellationToken,
    pub shutdown: CancellationToken,
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Stop {
    Cancelled,
    Timeout,
}
pub struct Exit {
    pub status: Option<ExitStatus>,
    pub stop: Option<Stop>,
}

struct Process {
    child: Child,
    groups: HashSet<i32>,
    armed: bool,
}
impl Process {
    fn signal(&self, signal: Signal) {
        for group in &self.groups {
            let _ = killpg(Pid::from_raw(*group), signal);
        }
    }
    async fn discover_descendants(&mut self) {
        let Some(root) = self.child.id() else {
            return;
        };
        let mut command = Command::new("/bin/ps");
        command
            .args(["-A", "-o", "pid=,ppid=,pgid="])
            .kill_on_drop(true);
        let output = match tokio::time::timeout(Duration::from_millis(200), command.output()).await
        {
            Ok(Ok(output)) if output.status.success() => output,
            result => {
                eprintln!(
                    "Could not inspect descendants of {root}; stopping known process groups only: {result:?}"
                );
                return;
            }
        };
        let rows: Vec<Vec<i32>> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(|l| {
                l.split_whitespace()
                    .filter_map(|n| n.parse().ok())
                    .collect()
            })
            .filter(|v: &Vec<i32>| v.len() == 3)
            .collect();
        let own_group = nix::unistd::getpgrp().as_raw();
        let mut descendants = HashSet::from([root as i32]);
        loop {
            let before = descendants.len();
            for row in &rows {
                if descendants.contains(&row[1]) {
                    descendants.insert(row[0]);
                    if row[2] > 1 && row[2] != own_group {
                        self.groups.insert(row[2]);
                    }
                }
            }
            if descendants.len() == before {
                break;
            }
        }
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        if self.armed {
            self.signal(Signal::SIGKILL);
            let _ = self.child.start_kill();
        }
    }
}
async fn read_pipe(
    mut stream: impl AsyncRead + Unpin,
    stderr: bool,
    tx: mpsc::Sender<(bool, io::Result<Vec<u8>>)>,
) {
    let mut buffer = [0; 8192];
    loop {
        match stream.read(&mut buffer).await {
            Ok(0) => break,
            Ok(n) => {
                if tx.send((stderr, Ok(buffer[..n].to_vec()))).await.is_err() {
                    break;
                }
            }
            Err(e) => {
                let _ = tx.send((stderr, Err(e))).await;
                break;
            }
        }
    }
}

/// Owns a whole invocation until both its process and pipes are finished.
/// Callers keep this future alive on request cancellation so the graceful stop can finish.
pub async fn execute(
    command: &mut Command,
    mut input: mpsc::UnboundedReceiver<Vec<u8>>,
    deadline: Instant,
    control: &Control,
    mut output: impl FnMut(bool, &[u8]) -> Result<()>,
) -> Result<Exit> {
    ensure!(
        !control.cancel.is_cancelled() && !control.shutdown.is_cancelled(),
        "Invocation cancelled before start"
    );
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    let child = command.spawn().context("Could not start executable")?;
    let mut process = Process {
        groups: HashSet::from([child.id().unwrap() as i32]),
        child,
        armed: true,
    };
    let mut stdin = process.child.stdin.take().unwrap();
    let writer = tokio::spawn(async move {
        while let Some(bytes) = input.recv().await {
            if let Err(e) = stdin.write_all(&bytes).await {
                if e.kind() == io::ErrorKind::BrokenPipe {
                    return Ok(());
                }
                return Err(e);
            }
        }
        match stdin.shutdown().await {
            Err(e) if e.kind() != io::ErrorKind::BrokenPipe => Err(e),
            _ => Ok(()),
        }
    });
    let (tx, mut rx) = mpsc::channel(16);
    let stdout = tokio::spawn(read_pipe(
        process.child.stdout.take().unwrap(),
        false,
        tx.clone(),
    ));
    let stderr = tokio::spawn(read_pipe(process.child.stderr.take().unwrap(), true, tx));
    let mut status = None;
    let mut pipes_closed = false;
    let mut stop = None;
    let mut forced = false;
    let mut kill_at = deadline;
    let mut drain_until = deadline;
    let mut error = None;
    let mut shutdown_seen = false;
    loop {
        if status.is_some() && pipes_closed && (stop.is_none() || forced) {
            break;
        }
        tokio::select! {
            biased;
            _ = control.shutdown.cancelled(), if !shutdown_seen => {
                shutdown_seen = true;
                let due = Instant::now() + Duration::from_millis(250);
                if stop.is_none() { stop = Some(Stop::Cancelled); process.discover_descendants().await; process.signal(Signal::SIGINT); }
                kill_at = if forced { Instant::now() } else { due.min(kill_at) };
            }
            _ = control.cancel.cancelled(), if stop.is_none() => {
                stop = Some(Stop::Cancelled);
                kill_at = Instant::now() + Duration::from_secs(5);
                process.discover_descendants().await;
                process.signal(Signal::SIGINT);
            }
            _ = sleep_until(deadline), if stop.is_none() => {
                stop = Some(Stop::Timeout);
                kill_at = Instant::now() + Duration::from_secs(5);
                process.discover_descendants().await;
                process.signal(Signal::SIGINT);
            }
            _ = sleep_until(kill_at), if stop.is_some() && !forced => {
                if !shutdown_seen { process.discover_descendants().await; }
                process.signal(Signal::SIGKILL);
                forced = true;
                drain_until = Instant::now() + Duration::from_millis(if shutdown_seen { 40 } else { 1000 });
            }
            _ = sleep_until(drain_until), if forced => { break; }
            result = process.child.wait(), if status.is_none() => {
                match result { Ok(s) => status = Some(s), Err(e) => { error = Some(e.into()); break; } }
            }
            chunk = rx.recv(), if !pipes_closed => {
                match chunk {
                    Some((is_err, Ok(bytes))) => if let Err(e) = output(is_err, &bytes) { error = Some(e); break; },
                    Some((_, Err(e))) => { error = Some(e.into()); break; },
                    None => pipes_closed = true,
                }
            }
        }
    }
    if error.is_some() {
        process.discover_descendants().await;
        process.signal(Signal::SIGKILL);
    }
    if writer.is_finished() {
        match writer.await {
            Ok(Err(e)) if error.is_none() => error = Some(e.into()),
            Err(e) if error.is_none() => error = Some(e.into()),
            _ => {}
        }
    } else {
        writer.abort();
    }
    stdout.abort();
    stderr.abort();
    if status.is_none() {
        process.signal(Signal::SIGKILL);
        status = tokio::time::timeout(Duration::from_secs(1), process.child.wait())
            .await
            .ok()
            .transpose()?
            .or(status);
    }
    process.armed = false;
    if let Some(e) = error {
        return Err(e);
    }
    Ok(Exit { status, stop })
}
pub fn input(bytes: impl Into<Vec<u8>>) -> mpsc::UnboundedReceiver<Vec<u8>> {
    let (tx, rx) = mpsc::unbounded_channel();
    let _ = tx.send(bytes.into());
    rx
}
pub struct Capture {
    pub stdout: Vec<u8>,
    pub stderr: String,
    pub exit: Exit,
}
pub async fn capture(
    command: &mut Command,
    seconds: u64,
    control: &Control,
    limit: usize,
) -> Result<Capture> {
    let mut stdout = vec![];
    let mut stderr = vec![];
    let exit = execute(
        command,
        input(vec![]),
        Instant::now() + Duration::from_secs(seconds),
        control,
        |err, bytes| {
            let buffer = if err { &mut stderr } else { &mut stdout };
            ensure!(
                buffer.len() + bytes.len() <= limit,
                "Command output exceeds {limit} bytes"
            );
            buffer.extend_from_slice(bytes);
            Ok(())
        },
    )
    .await?;
    Ok(Capture {
        stdout,
        stderr: String::from_utf8_lossy(&stderr).trim().into(),
        exit,
    })
}
impl Capture {
    pub fn success(&self) -> bool {
        self.exit.stop.is_none() && self.exit.status.is_some_and(|s| s.success())
    }
    pub fn text(self) -> Result<String> {
        ensure!(
            self.success(),
            "Command failed (exit {:?}, stop {:?}): {}",
            self.exit.status.and_then(|s| s.code()),
            self.exit.stop,
            self.stderr
        );
        Ok(String::from_utf8(self.stdout)?)
    }
}
