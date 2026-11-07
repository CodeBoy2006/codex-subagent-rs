use crate::{
    config::{Config, Inherit},
    process::{Control, capture},
};
use anyhow::{Result, anyhow, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;
use tokio::process::Command;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Inheritance {
    pub args: Vec<String>,
    pub report: String,
}
fn config_arg(args: &mut Vec<String>, key: String) {
    args.extend(["--config".into(), key]);
}
fn addressable(s: &str) -> bool {
    !s.is_empty() && !s.contains(['.', '='])
}
fn is_self(entry: &Value) -> bool {
    let exe = std::env::current_exe().ok();
    let transport = &entry["transport"];
    std::iter::once(&transport["command"])
        .chain(transport["args"].as_array().into_iter().flatten())
        .filter_map(Value::as_str)
        .any(|v| {
            let path = v.replace('\\', "/").to_lowercase();
            path.rsplit('/').next() == Some("codex-subagent-rs")
                || exe.as_deref() == Some(Path::new(v))
        })
}
pub fn resolve(config: &Config, mcp: Result<Value>, plugins: Result<Value>) -> Result<Inheritance> {
    let mut out = Inheritance::default();
    let mut notes = vec![];
    let mcp_names = match mcp.and_then(|v| {
        let list = v
            .as_array()
            .ok_or_else(|| anyhow!("MCP inventory is not an array"))?;
        list.iter()
            .map(|e| {
                let n = e["name"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| anyhow!("MCP entry has no readable name"))?;
                Ok((n.to_owned(), is_self(e)))
            })
            .collect::<Result<Vec<_>>>()
    }) {
        Ok(names) => {
            let mut allowed = vec![];
            for (name, own) in names {
                if !own && config.mcp.permits(&name) {
                    allowed.push(name);
                } else if addressable(&name) {
                    config_arg(&mut out.args, format!("mcp_servers.{name}.enabled=false"));
                } else if own {
                    notes.push(format!(
                        "Recursion guard could not disable own entry {name:?}"
                    ));
                } else {
                    anyhow::bail!(
                        "MCP entry {name:?} cannot be disabled by a dotted config key; nothing was run"
                    );
                }
            }
            format!("{allowed:?}")
        }
        Err(e) => {
            ensure!(
                config.mcp == Inherit::All,
                "Cannot list inherited MCP servers; restricted run refused: {e}"
            );
            notes.push(format!(
                "MCP listing failed; recursion guard unconfirmed: {e}"
            ));
            "all".into()
        }
    };
    let plugin_names = if config.plugins == Inherit::None {
        config_arg(&mut out.args, "features.plugins=false".into());
        "none".into()
    } else {
        let listed = plugins.and_then(|v| {
            let list = v["installed"]
                .as_array()
                .ok_or_else(|| anyhow!("Plugin inventory has no installed array"))?;
            let mut names = vec![];
            for e in list {
                let id = e["pluginId"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| anyhow!("Plugin has no readable id"))?;
                let installed = e["installed"]
                    .as_bool()
                    .ok_or_else(|| anyhow!("Plugin state missing"))?;
                let enabled = e["enabled"]
                    .as_bool()
                    .ok_or_else(|| anyhow!("Plugin state missing"))?;
                if installed && enabled {
                    names.push(id.to_owned());
                }
            }
            Ok(names)
        });
        match listed {
            Ok(names) => {
                let disabled: Vec<_> = names
                    .iter()
                    .filter(|n| !config.plugins.permits(n))
                    .collect();
                if disabled.iter().any(|n| !addressable(n)) {
                    config_arg(&mut out.args, "features.plugins=false".into());
                    notes.push(
                        "Plugin id cannot be disabled individually; all plugins disabled".into(),
                    );
                    "none".into()
                } else {
                    for id in disabled {
                        config_arg(&mut out.args, format!("plugins.{id}.enabled=false"));
                    }
                    format!(
                        "{:?}",
                        names
                            .iter()
                            .filter(|n| config.plugins.permits(n))
                            .collect::<Vec<_>>()
                    )
                }
            }
            Err(e) => {
                notes.push(format!("Plugin listing failed: {e}"));
                if config.plugins == Inherit::All {
                    "all".into()
                } else {
                    config_arg(&mut out.args, "features.plugins=false".into());
                    "none".into()
                }
            }
        }
    };
    if !config.apps {
        config_arg(&mut out.args, "features.apps=false".into());
    }
    out.report = format!(
        "Allowed from Codex: MCP servers {mcp_names}; plugins {plugin_names}; apps {}. {}",
        if config.apps { "on" } else { "off" },
        notes.join("; ")
    );
    Ok(out)
}
pub async fn inspect(config: &Config, cwd: &Path, control: &Control) -> Result<Inheritance> {
    async fn list(config: &Config, cwd: &Path, args: &[&str], control: &Control) -> Result<Value> {
        let text = capture(
            Command::new(&config.codex).args(args).current_dir(cwd),
            60,
            control,
            1024 * 1024,
        )
        .await?
        .text()?;
        Ok(serde_json::from_str(&text)?)
    }
    let (mcp, plugins) = tokio::join!(
        list(
            config,
            cwd,
            &[
                "mcp",
                "list",
                "--json",
                "--config",
                "features.plugins=false"
            ],
            control
        ),
        async {
            if config.plugins == Inherit::None {
                Ok(serde_json::json!({"installed":[]}))
            } else {
                list(config, cwd, &["plugin", "list", "--json"], control).await
            }
        }
    );
    resolve(config, mcp, plugins)
}
