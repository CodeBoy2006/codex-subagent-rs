use crate::process::{Control, capture};
use anyhow::{Context, Result, ensure};
use notify::{RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    fs::{self, DirBuilder},
    io::Write,
    os::unix::fs::DirBuilderExt,
    path::{Path, PathBuf},
};
use tokio::{process::Command, sync::mpsc, task::JoinSet};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Running,
    Completed,
    Failed,
    Cancelled,
    Interrupted,
}
impl State {
    pub fn terminal(self) -> bool {
        self != Self::Running
    }
}
impl std::fmt::Display for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Interrupted => "interrupted",
        })
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Event {
    pub job_id: String,
    pub conversation_id: String,
    pub state: State,
    pub finished_at: i64,
}
pub fn uuid(s: &str) -> bool {
    s.len() == 36 && Uuid::parse_str(s).is_ok()
}
pub fn directory(path: &Path) -> Result<()> {
    ensure!(path.is_absolute(), "Completion directory must be absolute");
    DirBuilder::new().recursive(true).mode(0o700).create(path)?;
    Ok(())
}
pub(crate) fn atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = tempfile::NamedTempFile::new_in(path.parent().unwrap())?;
    file.write_all(bytes)?;
    file.persist(path)?;
    Ok(())
}
pub fn publish(dir: &Path, event: &Event) -> Result<()> {
    validate(event)?;
    directory(dir)?;
    atomic(
        &dir.join(format!("{}.pending.json", event.job_id)),
        &serde_json::to_vec(event)?,
    )
}
fn validate(e: &Event) -> Result<()> {
    ensure!(
        uuid(&e.job_id) && uuid(&e.conversation_id) && e.state.terminal(),
        "Invalid completion event"
    );
    Ok(())
}
pub fn bind_hook(event: &Value) -> Result<Value> {
    let call = &event["toolCall"];
    let args = &call["args"];
    if call["name"] == "call_mcp_tool"
        && args["ServerName"] == "codex"
        && args["ToolName"] == "codex_delegate"
        && args["Arguments"]["mode"] == "background"
    {
        let id = event["conversationId"]
            .as_str()
            .filter(|s| uuid(s))
            .context("Missing originating conversation UUID")?;
        let mut arguments = args["Arguments"]
            .as_object()
            .context("Arguments must be an object")?
            .clone();
        arguments.insert("reply_to".into(), json!(id));
        return Ok(json!({"decision":"ask","overwrite":{"Arguments":arguments}}));
    }
    Ok(json!({"decision":"ask"}))
}
async fn deliver(path: PathBuf, control: Control) -> Result<()> {
    let dir = path.parent().context("Missing event directory")?;
    let event: Event = serde_json::from_slice(&tokio::fs::read(&path).await?)?;
    validate(&event)?;
    ensure!(
        path.file_name().and_then(|n| n.to_str())
            == Some(&format!("{}.pending.json", event.job_id)),
        "Event filename mismatch"
    );
    let message = format!(
        "Codex completion event {}: {}.\nRead codex/codex_job_result with job_id \"{}\" once to integrate or report the outcome. Ignore duplicate notifications for an already-handled job. Do not resume cancelled jobs.",
        event.job_id, event.state, event.job_id
    );
    let binary = std::env::var_os("ANTIGRAVITY_AGENTAPI_EXE");
    let mut command = Command::new(
        binary
            .as_deref()
            .unwrap_or(std::ffi::OsStr::new("agentapi")),
    );
    if binary.is_some() {
        command.arg("agentapi");
    }
    command.args([
        "send-message",
        "--title=Codex task finished",
        &event.conversation_id,
        &message,
    ]);
    capture(&mut command, 30, &control, 1024 * 1024)
        .await?
        .text()?;
    // shortcut: a crash between send and rename can redeliver; consumers deduplicate by job ID.
    tokio::fs::rename(&path, dir.join(format!("{}.sent.json", event.job_id))).await?;
    eprintln!(
        "Delivered {} ({}) to {}",
        event.job_id, event.state, event.conversation_id
    );
    Ok(())
}
fn pending(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_suffix(".pending.json"))
        .is_some_and(uuid)
}
pub async fn sidecar(dir: &Path, stop: CancellationToken) -> Result<()> {
    directory(dir)?;
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut watcher = notify::recommended_watcher(move |event| {
        let _ = tx.send(event);
    })?;
    // Subscribe before scanning so completion during startup cannot be lost.
    watcher.watch(dir, RecursiveMode::NonRecursive)?;
    let mut in_flight = HashSet::new();
    let mut jobs = JoinSet::new();
    let enqueue = |path: PathBuf,
                   in_flight: &mut HashSet<PathBuf>,
                   jobs: &mut JoinSet<(PathBuf, Result<()>)>| {
        if pending(&path) && in_flight.insert(path.clone()) {
            jobs.spawn(async move {
                let result = deliver(path.clone(), Control::default()).await;
                (path, result)
            });
        }
    };
    for entry in fs::read_dir(dir)? {
        enqueue(entry?.path(), &mut in_flight, &mut jobs);
    }
    eprintln!("Watching Codex completion events in {}", dir.display());
    loop {
        tokio::select! {
            _=stop.cancelled()=>break,
            event=rx.recv()=>match event {
                Some(Ok(event))=> {
                    if matches!(event.kind,notify::EventKind::Create(_)|notify::EventKind::Modify(_)) {
                        for path in event.paths { enqueue(path,&mut in_flight,&mut jobs); }
                    }
                }
                Some(Err(e))=>return Err(e.into()),
                None=>break,
            },
            result=jobs.join_next(), if !jobs.is_empty()=> {
                let (path,result)=result.unwrap()?; in_flight.remove(&path);
                if let Err(e)=result
                    && e.downcast_ref::<std::io::Error>().is_none_or(|e| e.kind() != std::io::ErrorKind::NotFound) { eprintln!("Completion delivery failed for {}: {e:#}. Pending event retained; restart Sidecar to retry.",path.display()); }
            }
        }
    }
    drop(watcher);
    while let Some(result) = jobs.join_next().await {
        let (path, result) = result?;
        if let Err(e) = result {
            eprintln!(
                "Completion delivery failed for {}: {e:#}; event retained",
                path.display()
            );
        }
    }
    Ok(())
}
