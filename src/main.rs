use anyhow::{Result, bail};
use codex_subagent_rs::{completion, config::Config, service::App};
use rmcp::ServiceExt;
use std::{
    io::{Read, Write},
    path::PathBuf,
};
use tokio_util::sync::CancellationToken;

async fn signal() {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("install SIGTERM handler");
    tokio::select! { _=tokio::signal::ctrl_c()=>{},_=term.recv()=>{} }
}
fn main() -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(run());
    // Tokio's stdin read is blocking on macOS; a host signal need not close that pipe.
    runtime.shutdown_timeout(std::time::Duration::from_millis(50));
    result
}
async fn run() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let mode = args.next().unwrap_or("mcp".into());
    if mode == "--version" {
        println!("codex-subagent-rs {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if mode == "--help" || mode == "help" {
        println!(
            "codex-subagent-rs [mcp|hook|sidecar] [--events-dir ABSOLUTE_PATH]\n\nDefault: serve eight Codex tools over MCP stdio.\nhook: bind Antigravity's original conversation from a JSON event on stdin.\nsidecar: watch saved completions and notify their originating conversations.\n\nCODEX_BIN selects the existing Codex CLI. CODEX_SUBAGENT_* variables configure\nmodel, effort, sandbox and inherited-tool policies. CODEX_SUBAGENT_COMPLETION_DIR\nselects the shared event directory; default ~/.local/share/codex-subagent-rs/completion/events.\nNo mode installs itself or edits client configuration."
        );
        return Ok(());
    }
    let mut config = Config::from_env();
    while let Some(arg) = args.next() {
        if arg == "--events-dir" {
            config.events = PathBuf::from(
                args.next()
                    .ok_or_else(|| anyhow::anyhow!("--events-dir needs a path"))?,
            );
        } else {
            bail!("Unknown argument {arg}; use --help");
        }
    }
    anyhow::ensure!(
        config.events.is_absolute(),
        "Completion directory must be absolute"
    );
    match mode.as_str() {
        "hook" => {
            let mut bytes = vec![];
            std::io::stdin().read_to_end(&mut bytes)?;
            let result = completion::bind_hook(&serde_json::from_slice(&bytes)?)?;
            serde_json::to_writer(std::io::stdout(), &result)?;
            std::io::stdout().write_all(b"\n")?;
        }
        "sidecar" => {
            let stop = CancellationToken::new();
            let notify = stop.clone();
            let signal_task = tokio::spawn(async move {
                signal().await;
                notify.cancel();
            });
            let result = completion::sidecar(&config.events, stop).await;
            signal_task.abort();
            result?;
        }
        "mcp" => {
            let app = App::new(config)?;
            let service = tokio::select! {service=app.clone().serve(rmcp::transport::stdio())=>service?,_=signal()=>return Ok(())};
            eprintln!(
                "codex-subagent-rs {} running on stdio",
                env!("CARGO_PKG_VERSION")
            );
            tokio::select! {_=service.waiting()=>{},_=signal()=>{}}
            app.shutdown().await;
        }
        _ => bail!("Unknown mode {mode}; use --help"),
    }
    Ok(())
}
