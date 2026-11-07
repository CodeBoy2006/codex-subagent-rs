use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

pub const NATIVE_AGENT_POLICY: &str = include_str!("../assets/native-agents.txt");

pub const EFFORTS: [&str; 8] = [
    "none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra",
];
pub const SANDBOXES: [&str; 3] = ["read-only", "workspace-write", "danger-full-access"];
pub fn effort_rank(s: &str) -> Option<usize> {
    EFFORTS.iter().position(|v| *v == s)
}
pub fn sandbox_rank(s: &str) -> Option<usize> {
    SANDBOXES.iter().position(|v| *v == s)
}

#[derive(Clone, Debug, PartialEq)]
pub enum Inherit {
    None,
    All,
    Names(Vec<String>),
}
impl Inherit {
    pub fn permits(&self, name: &str) -> bool {
        matches!(self, Self::All) || matches!(self, Self::Names(v) if v.iter().any(|s| s == name))
    }
    fn parse(s: &str) -> Result<Self> {
        match s {
            "" | "none" => Ok(Self::None),
            "all" => Ok(Self::All),
            _ => {
                let names = parse_list(s)?;
                ensure!(
                    !names.iter().any(|n| n == "all" || n == "none"),
                    "none/all cannot be mixed with names"
                );
                Ok(Self::Names(names))
            }
        }
    }
}
fn parse_list(s: &str) -> Result<Vec<String>> {
    if s.is_empty() {
        return Ok(vec![]);
    }
    let names: Vec<_> = s.split(',').map(str::trim).map(str::to_owned).collect();
    ensure!(
        !names.iter().any(String::is_empty),
        "list has an empty entry"
    );
    Ok(names)
}

#[derive(Clone, Debug)]
pub struct Config {
    pub codex: PathBuf,
    pub events: PathBuf,
    pub default_model: Option<String>,
    pub default_effort: Option<String>,
    pub allowed_models: Vec<String>,
    pub max_effort: Option<String>,
    pub default_sandbox: String,
    pub max_sandbox: String,
    pub mcp: Inherit,
    pub plugins: Inherit,
    pub apps: bool,
    pub errors: Vec<String>,
}
impl Config {
    pub fn from_env() -> Self {
        Self::from_map(&std::env::vars().collect())
    }
    pub fn from_map(env: &HashMap<String, String>) -> Self {
        let value = |name: &str| env.get(name).map(|s| s.trim()).unwrap_or("");
        let mut errors = vec![];
        let mut choice = |name: &str, allowed: &[&str]| {
            let v = value(name);
            if v.is_empty() {
                None
            } else if allowed.contains(&v) {
                Some(v.to_owned())
            } else {
                errors.push(format!("{name}: expected {}", allowed.join(", ")));
                None
            }
        };
        let default_effort = choice("CODEX_SUBAGENT_DEFAULT_EFFORT", &EFFORTS);
        let max_effort = choice("CODEX_SUBAGENT_MAX_EFFORT", &EFFORTS);
        let default_sandbox =
            choice("CODEX_SUBAGENT_DEFAULT_SANDBOX", &SANDBOXES).unwrap_or("read-only".into());
        let max_sandbox =
            choice("CODEX_SUBAGENT_MAX_SANDBOX", &SANDBOXES).unwrap_or("workspace-write".into());
        let apps = choice("CODEX_SUBAGENT_APPS", &["on", "off"]).as_deref() == Some("on");
        let default_model = (!value("CODEX_SUBAGENT_DEFAULT_MODEL").is_empty())
            .then(|| value("CODEX_SUBAGENT_DEFAULT_MODEL").to_owned());
        let allowed_models =
            parse_list(value("CODEX_SUBAGENT_ALLOWED_MODELS")).unwrap_or_else(|e| {
                errors.push(format!("CODEX_SUBAGENT_ALLOWED_MODELS: {e}"));
                vec![]
            });
        let mut policy = |key: &str| {
            Inherit::parse(value(key)).unwrap_or_else(|e| {
                errors.push(format!("{key}: {e}"));
                Inherit::None
            })
        };
        let mcp = policy("CODEX_SUBAGENT_MCP_SERVERS");
        let plugins = policy("CODEX_SUBAGENT_PLUGINS");
        if default_model
            .as_ref()
            .is_some_and(|m| !allowed_models.is_empty() && !allowed_models.contains(m))
        {
            errors.push("Default model is outside CODEX_SUBAGENT_ALLOWED_MODELS".into());
        }
        if default_effort
            .as_deref()
            .zip(max_effort.as_deref())
            .is_some_and(|(d, m)| effort_rank(d) > effort_rank(m))
        {
            errors.push("Default effort exceeds CODEX_SUBAGENT_MAX_EFFORT".into());
        }
        if sandbox_rank(&default_sandbox) > sandbox_rank(&max_sandbox) {
            errors.push("Default sandbox exceeds CODEX_SUBAGENT_MAX_SANDBOX".into());
        }
        let user_home = PathBuf::from(value("HOME"));
        let path = |key: &str, fallback: PathBuf| {
            if value(key).is_empty() {
                fallback
            } else {
                PathBuf::from(value(key))
            }
        };
        let events = path(
            "CODEX_SUBAGENT_COMPLETION_DIR",
            user_home.join(".local/share/codex-subagent-rs/completion/events"),
        );
        if !events.is_absolute() {
            errors.push("CODEX_SUBAGENT_COMPLETION_DIR must be absolute".into());
        }
        Self {
            codex: path("CODEX_BIN", "codex".into()),
            events,
            default_model,
            default_effort,
            allowed_models,
            max_effort,
            default_sandbox,
            max_sandbox,
            mcp,
            plugins,
            apps,
            errors,
        }
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.errors.is_empty(),
            "MCP configuration errors:\n{}",
            self.errors.join("\n")
        );
        Ok(())
    }
    pub fn permits_model(&self, model: &str) -> bool {
        self.allowed_models.is_empty() || self.allowed_models.iter().any(|m| m == model)
    }
    pub fn implied_model(&self) -> Option<&str> {
        self.default_model
            .as_deref()
            .or_else(|| (self.allowed_models.len() == 1).then(|| self.allowed_models[0].as_str()))
    }
    pub fn sandbox(&self, requested: Option<&str>) -> Result<String> {
        let s = requested.unwrap_or(&self.default_sandbox);
        let rank = sandbox_rank(s).ok_or_else(|| anyhow::anyhow!("Unknown sandbox: {s}"))?;
        ensure!(
            rank <= sandbox_rank(&self.max_sandbox).unwrap(),
            "Sandbox {s} exceeds ceiling {}",
            self.max_sandbox
        );
        Ok(s.to_owned())
    }
}
pub fn directory(path: &Path) -> Result<()> {
    ensure!(
        path.is_absolute(),
        "Directory must be absolute: {}",
        path.display()
    );
    ensure!(
        path.is_dir(),
        "Directory does not exist or is not a directory: {}",
        path.display()
    );
    Ok(())
}
pub fn valid_thread(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 200
        && s.as_bytes()[0].is_ascii_alphanumeric()
        && s.bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct RunArgs {
    pub prompt: String,
    pub thread_id: Option<String>,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub working_dir: Option<PathBuf>,
    pub sandbox: Option<String>,
    #[serde(default)]
    pub auto_approve: bool,
    #[serde(default)]
    pub add_dirs: Vec<PathBuf>,
    #[serde(default)]
    pub use_worktree: bool,
    #[serde(default)]
    pub web_search: bool,
    #[serde(default)]
    pub skip_git_repo_check: bool,
    pub timeout_seconds: Option<u64>,
    pub mode: Option<String>,
    pub reply_to: Option<String>,
    pub output_schema: Option<serde_json::Value>,
}
impl RunArgs {
    pub fn validate(&self, followup: bool) -> Result<()> {
        ensure!(!self.prompt.trim().is_empty(), "prompt must not be empty");
        if let Some(p) = &self.working_dir {
            directory(p)?;
        }
        for p in &self.add_dirs {
            directory(p)?;
        }
        if let Some(e) = &self.reasoning_effort {
            ensure!(effort_rank(e).is_some(), "Unknown reasoning_effort: {e}");
        }
        if let Some(t) = self.timeout_seconds {
            ensure!((1..=7200).contains(&t), "timeout_seconds must be 1..=7200");
        }
        if let Some(m) = &self.mode {
            ensure!(m == "blocking" || m == "background", "Unknown mode: {m}");
        }
        if followup {
            ensure!(
                self.thread_id.as_deref().is_some_and(valid_thread),
                "Invalid thread_id"
            );
            ensure!(
                !self.auto_approve,
                "auto_approve is not supported on follow-ups; nothing was run"
            );
        }
        if let Some(id) = &self.reply_to {
            ensure!(
                self.mode.as_deref() == Some("background") && !followup,
                "reply_to requires a background delegation"
            );
            ensure!(
                crate::completion::uuid(id),
                "reply_to must be an Antigravity conversation UUID"
            );
        }
        if let Some(schema) = &self.output_schema {
            if !schema.is_object() {
                bail!("output_schema must be a JSON object");
            }
            ensure!(
                serde_json::to_vec(schema)?.len() <= 65536,
                "output_schema exceeds 64 KiB"
            );
        }
        Ok(())
    }
    pub fn prompt_text(&self) -> String {
        format!("{NATIVE_AGENT_POLICY}\n\n{}", self.prompt.trim())
    }
}
