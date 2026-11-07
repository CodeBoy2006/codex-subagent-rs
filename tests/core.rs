use codex_subagent_rs::{
    catalog,
    completion::{self, Event, State},
    config::{Config, NATIVE_AGENT_POLICY, RunArgs},
    inheritance,
    runner::{Invocation, JsonLines, RunResult},
    session::Settings,
};
use serde_json::json;
use std::collections::HashMap;
fn config(pairs: &[(&str, &str)]) -> Config {
    Config::from_map(
        &pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect::<HashMap<_, _>>(),
    )
}
#[test]
fn policy_boundaries_and_effort_resolution() {
    let invalid = config(&[
        ("CODEX_SUBAGENT_ALLOWED_MODELS", "alpha,,beta"),
        ("CODEX_SUBAGENT_DEFAULT_SANDBOX", "danger-full-access"),
    ]);
    assert!(invalid.validate().is_err());
    let c = config(&[
        ("CODEX_SUBAGENT_ALLOWED_MODELS", "alpha"),
        ("CODEX_SUBAGENT_MAX_EFFORT", "high"),
    ]);
    assert_eq!(c.implied_model(), Some("alpha"));
    assert!(c.sandbox(Some("danger-full-access")).is_err());
    let m=catalog::parse_catalog(&json!({"models":[{"slug":"alpha","visibility":"list","supported_reasoning_levels":["low","high","xhigh"],"default_reasoning_level":"high"}]}).to_string()).unwrap().remove(0);
    assert_eq!(
        catalog::resolve_effort(&m, Some("ultra"), Some("high"))
            .unwrap()
            .0,
        "high"
    );
    assert!(catalog::resolve_effort(&m, Some("high"), Some("none")).is_err());
}
#[test]
fn inheritance_preserves_ceiling_and_recursion_guard() {
    let mcp = json!([{"name":"self","transport":{"command":"codex-subagent-rs"}},{"name":"docs","transport":{"command":"documentation-server"}}]);
    let c = config(&[
        ("CODEX_SUBAGENT_MCP_SERVERS", "all"),
        ("CODEX_SUBAGENT_PLUGINS", "keep@local"),
    ]);
    let plugins = json!({"installed":[{"pluginId":"keep@local","installed":true,"enabled":true},{"pluginId":"drop@local","installed":true,"enabled":true}]});
    let resolved = inheritance::resolve(&c, Ok(mcp.clone()), Ok(plugins)).unwrap();
    assert!(
        resolved
            .args
            .contains(&"mcp_servers.self.enabled=false".into())
    );
    assert!(
        !resolved
            .args
            .contains(&"mcp_servers.docs.enabled=false".into())
    );
    assert!(
        resolved
            .args
            .contains(&"plugins.drop@local.enabled=false".into())
    );
    let c = config(&[]);
    assert!(inheritance::resolve(&c, Err(anyhow::anyhow!("failed")), Ok(json!({}))).is_err());
    assert!(
        inheritance::resolve(&c, Ok(json!([{"name":"cannot.disable"}])), Ok(json!({}))).is_err()
    );
    let resolved = inheritance::resolve(&c, Ok(mcp), Err(anyhow::anyhow!("unavailable"))).unwrap();
    assert!(resolved.args.contains(&"features.plugins=false".into()));
    assert!(resolved.args.contains(&"features.apps=false".into()));
}
#[test]
fn recursion_guard_identifies_only_the_current_program() {
    let c = config(&[("CODEX_SUBAGENT_MCP_SERVERS", "all")]);
    for (transport, blocked) in [
        (json!({"command":"/opt/bin/codex-subagent-rs"}), true),
        (
            json!({"command":"wrapper","args":["/opt/bin/codex-subagent-rs"]}),
            true,
        ),
        (json!({"command":std::env::current_exe().unwrap()}), true),
        (json!({"command":"other-server"}), false),
        (json!({"command":"codex-subagent-rs-helper"}), false),
        (json!({"command":"/opt/codex-subagent-rs/helper"}), false),
    ] {
        let resolved = inheritance::resolve(
            &c,
            Ok(json!([{"name":"candidate","transport":transport}])),
            Ok(json!({})),
        )
        .unwrap();
        assert_eq!(
            resolved
                .args
                .iter()
                .any(|arg| arg == "mcp_servers.candidate.enabled=false"),
            blocked,
            "{transport}"
        );
    }
}
#[test]
fn argument_boundaries_and_native_execution_options() {
    let root = tempfile::tempdir().unwrap();
    let args = RunArgs {
        prompt: "$(touch do-not-run)".into(),
        thread_id: Some("--help".into()),
        output_schema: Some(json!({"type":"object"})),
        ..Default::default()
    };
    assert!(args.validate(true).is_err());
    let mut inv = Invocation {
        settings: Settings {
            model: "alpha".into(),
            effort: "high".into(),
            sandbox: "read-only".into(),
            cwd: root.path().into(),
            skip_git_repo_check: true,
        },
        args,
        inheritance: Default::default(),
        prompt: String::new(),
    };
    assert!(inv.argv(None).is_err());
    inv.args.thread_id = Some("thread-1".into());
    let argv = inv
        .argv(Some(root.path().join("schema.json").as_path()))
        .unwrap();
    assert_eq!(&argv[..4], ["exec", "resume", "thread-1", "--json"]);
    assert!(!argv.iter().any(|s| s == "--sandbox"));
    assert!(!argv.iter().any(|s| s.contains("touch")));
    for expected in [
        "agents.enabled=true",
        "features.multi_agent=true",
        "features.multi_agent_v2.disable_native_tools=false",
    ] {
        assert!(argv.windows(2).any(|p| p == ["--config", expected]));
    }
    assert!(argv.contains(&"--output-schema".into()));
    assert!(argv.contains(&"sandbox_mode=\"read-only\"".into()));
    inv.args.thread_id = None;
    let fresh = inv.argv(None).unwrap();
    assert!(fresh.windows(2).any(|p| p == ["--sandbox", "read-only"]));
    assert!(inv.args.prompt_text().contains("$(touch do-not-run)"));
    inv.args.output_schema = Some(json!({"description":"x".repeat(65536)}));
    assert!(inv.args.validate(false).is_err());
}
#[test]
fn prompts_reject_blank_tasks_and_prepend_only_native_policy() {
    let mut args = RunArgs {
        prompt: " \t\n ".into(),
        ..Default::default()
    };
    assert!(args.validate(false).is_err());
    args.prompt = " task\nContext: prior findings\nFiles: src/config.rs\nDone: tests pass ".into();
    assert!(args.validate(false).is_ok());
    assert_eq!(
        args.prompt_text(),
        format!(
            "{NATIVE_AGENT_POLICY}\n\ntask\nContext: prior findings\nFiles: src/config.rs\nDone: tests pass"
        )
    );
}
#[test]
fn cancelled_results_preserve_thread_without_suggesting_follow_up() {
    let inv = Invocation {
        settings: Settings {
            model: "alpha".into(),
            effort: "high".into(),
            sandbox: "read-only".into(),
            cwd: std::env::current_dir().unwrap(),
            skip_git_repo_check: true,
        },
        args: RunArgs::default(),
        inheritance: Default::default(),
        prompt: String::new(),
    };
    let mut result = RunResult {
        exit_code: Some(0),
        final_message: "done".into(),
        thread_id: Some("thread-1".into()),
        ..Default::default()
    };
    assert!(result.render(&inv, &[]).contains(
        " | thread_id=thread-1\nContinue with codex_follow_up using thread_id \"thread-1\"."
    ));
    result.cancelled = true;
    let report = result.render(&inv, &[]);
    assert!(report.contains(" | thread_id=thread-1"));
    assert!(!report.contains("Continue with codex_follow_up"));
}
#[test]
fn jsonl_handles_utf8_split_and_recovers_after_oversize() {
    let mut parser = JsonLines::default();
    let mut events = vec![];
    let bytes = format!(
        "notice\n{}\n",
        json!({"type":"item.completed","item":{"text":"中文🦀"}})
    )
    .into_bytes();
    for chunk in bytes.chunks(2) {
        parser.push(chunk, |v| events.push(v));
    }
    parser.push(&vec![b'x'; 1024 * 1024 + 1], |v| events.push(v));
    parser.push(b"\n{\"type\":\"turn.completed\"}", |v| events.push(v));
    parser.finish(|v| events.push(v));
    assert!(parser.truncated);
    assert_eq!(events.len(), 2);
    assert_eq!(events[0]["item"]["text"], "中文🦀");
}
#[test]
fn failed_or_invalid_structured_output_cannot_masquerade_as_success() {
    let mut result = RunResult {
        exit_code: Some(0),
        final_message: "looks successful".into(),
        turn_failure: Some("failed".into()),
        ..Default::default()
    };
    assert!(result.failure().is_some());
    result.turn_failure = None;
    result.structured = true;
    assert!(result.failure().is_some());
    result.final_message = "{}".into();
    assert!(result.failure().is_none());
    result.truncated = true;
    assert!(result.failure().is_some());
}
#[test]
fn hook_and_completion_event_publication() {
    let origin = uuid::Uuid::new_v4().to_string();
    let job = uuid::Uuid::new_v4().to_string();
    let dir = tempfile::tempdir().unwrap();
    let mut event = json!({"conversationId":origin,"toolCall":{"name":"call_mcp_tool","args":{"ServerName":"codex","ToolName":"codex_delegate","Arguments":{"mode":"background","prompt":"task","reply_to":"untrusted"}}}});
    let bound = completion::bind_hook(&event).unwrap();
    assert_eq!(bound["decision"], "ask");
    assert_eq!(bound["overwrite"]["Arguments"]["reply_to"], origin);
    assert_eq!(bound["overwrite"]["Arguments"]["prompt"], "task");
    event["toolCall"]["args"]["ServerName"] = json!("other");
    assert_eq!(
        completion::bind_hook(&event).unwrap(),
        json!({"decision":"ask"})
    );
    let completion_event = Event {
        job_id: job.clone(),
        conversation_id: origin,
        state: State::Failed,
        finished_at: 1,
    };
    let events = dir.path().join("events");
    let mut invalid = completion_event.clone();
    invalid.state = State::Running;
    assert!(completion::publish(&events, &invalid).is_err());
    assert!(!events.exists());
    invalid = completion_event.clone();
    invalid.job_id = "../invalid".into();
    assert!(completion::publish(&events, &invalid).is_err());
    invalid = completion_event.clone();
    invalid.conversation_id = "invalid".into();
    assert!(completion::publish(&events, &invalid).is_err());
    completion::publish(&events, &completion_event).unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(
            &std::fs::read(events.join(format!("{job}.pending.json"))).unwrap()
        )
        .unwrap(),
        serde_json::to_value(&completion_event).unwrap()
    );
    assert_eq!(std::fs::read_dir(&events).unwrap().count(), 1);
    assert!(!events.join(format!("{job}.txt")).exists());
}
