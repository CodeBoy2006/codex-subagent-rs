use crate::{
    config::{Config, effort_rank},
    process::{self, Control, capture},
};
use anyhow::{Result, anyhow, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{process::Command, sync::Mutex, time::Instant};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Model {
    pub slug: String,
    pub display_name: String,
    pub description: String,
    pub default_effort: String,
    pub efforts: Vec<String>,
    pub context_window: Option<u64>,
    pub priority: u64,
    pub images: bool,
    pub search: bool,
}
#[derive(Clone)]
pub struct Diagnosis {
    pub status: &'static str,
    pub version: String,
    pub authenticated: Option<bool>,
    pub message: String,
}
impl Diagnosis {
    pub fn usable(&self) -> bool {
        matches!(self.status, "ok" | "unknown")
    }
    pub fn render(&self) -> String {
        format!(
            "status: {}\nversion: {}\nsigned in: {}\n{}",
            self.status,
            self.version,
            self.authenticated
                .map(|b| if b { "yes" } else { "no" })
                .unwrap_or("unknown"),
            self.message
        )
    }
}
#[derive(Default)]
pub struct Catalogs {
    catalog: Mutex<HashMap<PathBuf, (Instant, Vec<Model>)>>,
    diagnosis: Mutex<HashMap<PathBuf, (Instant, Diagnosis)>>,
}
fn cache_get<T: Clone>(
    map: &HashMap<PathBuf, (Instant, T)>,
    cwd: &Path,
    refresh: bool,
) -> Option<T> {
    map.get(cwd)
        .filter(|(t, _)| !refresh && t.elapsed() < Duration::from_secs(600))
        .map(|(_, v)| v.clone())
}
fn cache_put<T>(map: &mut HashMap<PathBuf, (Instant, T)>, cwd: &Path, value: T) {
    if map.len() >= 32
        && !map.contains_key(cwd)
        && let Some(key) = map
            .iter()
            .min_by_key(|(_, (t, _))| t)
            .map(|(k, _)| k.clone())
    {
        map.remove(&key);
    }
    map.insert(cwd.to_owned(), (Instant::now(), value));
}
impl Catalogs {
    pub async fn doctor(
        &self,
        config: &Config,
        cwd: &Path,
        refresh: bool,
        control: &Control,
    ) -> Diagnosis {
        if let Some(v) = cache_get(&*self.diagnosis.lock().await, cwd, refresh) {
            return v;
        }
        let mut d = Diagnosis {
            status: "missing",
            version: "not detected".into(),
            authenticated: None,
            message: "Install the Codex CLI, or set CODEX_BIN to its native executable.".into(),
        };
        let version = capture(
            Command::new(&config.codex)
                .arg("--version")
                .current_dir(cwd),
            10,
            control,
            65536,
        )
        .await
        .and_then(|r| r.text());
        let Ok(version) = version else {
            d.message = format!("{} {}", d.message, version.unwrap_err());
            return d;
        };
        d.version = version.trim().into();
        match capture(
            Command::new(&config.codex)
                .args(["login", "status"])
                .current_dir(cwd),
            10,
            control,
            65536,
        )
        .await
        {
            Ok(r) if r.success() => {
                d.status = "ok";
                d.authenticated = Some(true);
                d.message = "Codex is available.".into();
            }
            Ok(r) if r.exit.stop == Some(process::Stop::Timeout) => {
                d.status = "unknown";
                d.message = "Login probe timed out; authentication is unconfirmed.".into();
            }
            Ok(r)
                if r.stderr
                    .lines()
                    .any(|s| s.starts_with("Error loading configuration:")) =>
            {
                d.status = "config-error";
                d.message = r.stderr;
            }
            Ok(r) => {
                d.status = "unauthenticated";
                d.authenticated = Some(false);
                d.message = format!("Run codex login. {}", r.stderr);
            }
            Err(e) => {
                d.status = "unknown";
                d.message = format!("Login status could not be checked: {e}");
            }
        }
        if d.status == "ok" {
            cache_put(&mut *self.diagnosis.lock().await, cwd, d.clone());
        }
        d
    }
    pub async fn get(
        &self,
        config: &Config,
        cwd: &Path,
        refresh: bool,
        control: &Control,
    ) -> Result<Vec<Model>> {
        if let Some(c) = cache_get(&*self.catalog.lock().await, cwd, refresh) {
            return Ok(c);
        }
        let text = capture(
            Command::new(&config.codex)
                .args(["debug", "models"])
                .current_dir(cwd),
            15,
            control,
            32 * 1024 * 1024,
        )
        .await?
        .text()?;
        let models = parse_catalog(&text)?;
        cache_put(&mut *self.catalog.lock().await, cwd, models.clone());
        Ok(models)
    }
}
pub fn parse_catalog(text: &str) -> Result<Vec<Model>> {
    let v: Value = serde_json::from_str(text)?;
    let mut models = vec![];
    for m in v["models"]
        .as_array()
        .ok_or_else(|| anyhow!("Catalog has no models array"))?
    {
        if m["visibility"] != "list" {
            continue;
        }
        let Some(slug) = m["slug"].as_str().filter(|s| !s.is_empty()) else {
            continue;
        };
        let mut efforts: Vec<String> = m["supported_reasoning_levels"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|e| e.as_str().or_else(|| e["effort"].as_str()))
            .filter(|e| effort_rank(e).is_some())
            .map(str::to_owned)
            .collect();
        efforts.sort_by_key(|e| effort_rank(e));
        efforts.dedup();
        let default = m["default_reasoning_level"]
            .as_str()
            .filter(|e| effort_rank(e).is_some())
            .or_else(|| efforts.first().map(String::as_str))
            .unwrap_or("medium")
            .to_owned();
        if efforts.is_empty() {
            efforts.push(default.clone());
        }
        models.push(Model {
            slug: slug.into(),
            display_name: m["display_name"].as_str().unwrap_or(slug).into(),
            description: m["description"].as_str().unwrap_or("").into(),
            default_effort: default,
            efforts,
            context_window: m["context_window"].as_u64().filter(|n| *n > 0),
            priority: m["priority"].as_u64().unwrap_or(u64::MAX),
            images: m["input_modalities"]
                .as_array()
                .is_some_and(|a| a.iter().any(|v| v == "image")),
            search: m["supports_search_tool"] == true,
        });
    }
    models.sort_by(|a, b| a.priority.cmp(&b.priority).then(a.slug.cmp(&b.slug)));
    ensure!(!models.is_empty(), "Catalog contains no visible models");
    Ok(models)
}
pub fn resolve_effort(
    model: &Model,
    requested: Option<&str>,
    ceiling: Option<&str>,
) -> Result<(String, Option<String>)> {
    let wanted = requested.unwrap_or(&model.default_effort);
    let rank = effort_rank(wanted).ok_or_else(|| anyhow!("Unknown reasoning effort: {wanted}"))?;
    let chosen = model
        .efforts
        .iter()
        .filter(|e| ceiling.is_none_or(|c| effort_rank(e) <= effort_rank(c)))
        .min_by_key(|e| effort_rank(e).unwrap().abs_diff(rank))
        .ok_or_else(|| {
            anyhow!(
                "{} has no supported effort under ceiling {:?}",
                model.slug,
                ceiling
            )
        })?;
    Ok((
        chosen.clone(),
        (chosen != wanted).then(|| {
            format!(
                "{} cannot use {wanted} under the supported levels and ceiling; using {chosen}",
                model.slug
            )
        }),
    ))
}
