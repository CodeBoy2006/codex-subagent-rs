use crate::{app_server, config::Config, process::Control};
use anyhow::Result;
use chrono::{DateTime, SecondsFormat, Utc};
use serde::Deserialize;
use serde_json::json;
use std::{collections::BTreeMap, path::Path};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Quota {
    rate_limits: Bucket,
    rate_limits_by_limit_id: Option<BTreeMap<String, Bucket>>,
    ordinary_usage_allowed: Option<bool>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Bucket {
    limit_id: Option<String>,
    limit_name: Option<String>,
    plan_type: Option<String>,
    primary: Option<Window>,
    secondary: Option<Window>,
    rate_limit_reached_type: Option<String>,
    spend_control_reached: Option<bool>,
    individual_limit: Option<IndividualLimit>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Window {
    used_percent: i32,
    window_duration_mins: Option<i64>,
    resets_at: Option<i64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct IndividualLimit {
    remaining_percent: i32,
    resets_at: i64,
}

fn reset_time(seconds: Option<i64>) -> String {
    seconds
        .and_then(|s| DateTime::<Utc>::from_timestamp(s, 0))
        .map(|t| t.to_rfc3339_opts(SecondsFormat::Secs, true))
        .unwrap_or_else(|| "unknown".into())
}

impl Bucket {
    fn render(&self, id: &str) -> String {
        let mut lines = vec![format!(
            "\n{id}{} | plan: {}",
            self.limit_name
                .as_ref()
                .filter(|s| s.as_str() != id)
                .map(|s| format!(" ({s})"))
                .unwrap_or_default(),
            self.plan_type.as_deref().unwrap_or("unknown")
        )];
        for (name, window) in [("primary", &self.primary), ("secondary", &self.secondary)] {
            lines.push(match window {
                Some(w) => format!(
                    "  {name}: {}% remaining ({}% used) | window: {} | resets: {}",
                    (100i64 - i64::from(w.used_percent)).clamp(0, 100),
                    w.used_percent,
                    w.window_duration_mins
                        .map(|m| format!("{m} min"))
                        .unwrap_or_else(|| "unknown".into()),
                    reset_time(w.resets_at)
                ),
                None => format!("  {name}: unknown (not reported)"),
            });
        }
        if let Some(limit) = &self.individual_limit {
            lines.push(format!(
                "  individual limit: {}% remaining | resets: {}",
                limit.remaining_percent,
                reset_time(Some(limit.resets_at))
            ));
        }
        if let Some(reached) = &self.rate_limit_reached_type {
            lines.push(format!("  limit reached: {reached}"));
        }
        if let Some(reached) = self.spend_control_reached {
            lines.push(format!("  spend control reached: {reached}"));
        }
        lines.join("\n")
    }
}

pub async fn read(config: &Config, cwd: &Path, control: &Control) -> Result<String> {
    let response = app_server::read(
        config,
        cwd,
        "account/rateLimits/read",
        json!({"excludeResetCreditDetails":true}),
        control,
    )
    .await?;
    let quota: Quota = serde_json::from_value(response)?;
    let mut text = format!(
        "Codex remaining usage quota\nchecked_at: {}\nordinary usage allowed: {}",
        Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
        quota
            .ordinary_usage_allowed
            .map(|v| v.to_string())
            .unwrap_or_else(|| "unknown".into())
    );
    if let Some(buckets) = quota.rate_limits_by_limit_id.filter(|b| !b.is_empty()) {
        for (id, bucket) in buckets {
            text.push_str(&bucket.render(&id));
        }
    } else {
        text.push_str(
            &quota
                .rate_limits
                .render(quota.rate_limits.limit_id.as_deref().unwrap_or("codex")),
        );
    }
    Ok(text)
}
