use crate::{app_server, config::Config, process::Control};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Thread {
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub cwd: PathBuf,
}

pub async fn read(config: &Config, cwd: &Path, id: &str, control: &Control) -> Result<Thread> {
    let response = app_server::read(
        config,
        cwd,
        "thread/read",
        json!({"threadId": id, "includeTurns": false}),
        control,
    )
    .await?;
    Ok(serde_json::from_value(response["thread"].clone())?)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Settings {
    pub model: String,
    pub effort: String,
    pub cwd: PathBuf,
    pub sandbox: String,
    pub skip_git_repo_check: bool,
}
