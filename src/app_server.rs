use crate::{
    config::Config,
    process::{self, Control},
};
use anyhow::{Result, anyhow, ensure};
use serde_json::{Value, json};
use std::{path::Path, time::Duration};
use tokio::{process::Command, sync::mpsc, time::Instant};

/// One read-only RPC, using Codex's own configuration and authentication.
pub async fn read(
    config: &Config,
    cwd: &Path,
    method: &str,
    params: Value,
    control: &Control,
) -> Result<Value> {
    let (tx, rx) = mpsc::unbounded_channel();
    tx.send(format!("{}\n", json!({"id":1,"method":"initialize","params":{"clientInfo":{"name":"codex-subagent-rs","version":env!("CARGO_PKG_VERSION")}}})).into_bytes())?;
    let mut tx = Some(tx);
    let mut buffer = vec![];
    let mut response = None;
    let exit = process::execute(
        Command::new(&config.codex)
            .args(["app-server", "--listen", "stdio://"])
            .current_dir(cwd),
        rx,
        Instant::now() + Duration::from_secs(15),
        control,
        |err, bytes| {
            if err {
                return Ok(());
            }
            buffer.extend_from_slice(bytes);
            ensure!(
                buffer.len() <= 32 * 1024 * 1024,
                "{method} output too large"
            );
            while let Some(i) = buffer.iter().position(|b| *b == b'\n') {
                let line: Vec<_> = buffer.drain(..=i).collect();
                let v: Value = serde_json::from_slice(&line)?;
                if v["id"] == 1 {
                    ensure!(
                        v.get("error").is_none(),
                        "initialize failed: {}",
                        v["error"]
                    );
                    if let Some(sender) = &tx {
                        sender.send(
                            format!(
                                "{}\n{}\n",
                                json!({"method":"initialized"}),
                                json!({"id":2,"method":method,"params":params})
                            )
                            .into_bytes(),
                        )?;
                    }
                }
                if v["id"] == 2 {
                    response = Some(v);
                    tx.take();
                }
            }
            Ok(())
        },
    )
    .await?;
    ensure!(
        exit.stop.is_none() && exit.status.is_some_and(|s| s.success()),
        "{method} process failed (stop: {:?})",
        exit.stop
    );
    let response = response.ok_or_else(|| anyhow!("{method} returned no response"))?;
    ensure!(
        response.get("error").is_none(),
        "{method} failed: {}",
        response["error"]
    );
    response
        .get("result")
        .cloned()
        .ok_or_else(|| anyhow!("{method} returned no result"))
}
