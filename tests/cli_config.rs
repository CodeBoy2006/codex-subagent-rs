//! Optional real-CLI probe: isolated home, configuration RPC only, no authentication or model turn.
use codex_subagent_rs::{
    config::RunArgs,
    process::{self, Control},
    runner::Invocation,
    session::Settings,
};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::{process::Command, sync::mpsc, time::Instant};

#[tokio::test]
#[ignore = "Set CODEX_NATIVE_TEST_BIN to the native Codex CLI; performs no model call"]
async fn native_agent_enablement_preserves_existing_agent_policy() {
    let binary =
        std::env::var_os("CODEX_NATIVE_TEST_BIN").expect("CODEX_NATIVE_TEST_BIN is required");
    for v2 in [false, true] {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join("config.toml"), format!(
            "[agents]\nenabled=false\nmax_concurrent_threads_per_session=12\nmax_depth=2\ndefault_subagent_model=\"gpt-6-luna\"\ndefault_subagent_reasoning_effort=\"high\"\n[features]\nmulti_agent=false\nmulti_agent_v2={{enabled={v2},max_concurrent_threads_per_session=25}}\n"
        )).unwrap();
        let inv = Invocation {
            settings: Settings {
                model: "gpt-6.1-sol".into(),
                effort: "xhigh".into(),
                sandbox: "read-only".into(),
                cwd: home.path().into(),
                skip_git_repo_check: true,
            },
            args: RunArgs::default(),
            inheritance: Default::default(),
            prompt: String::new(),
        };
        let argv = inv.argv(None).unwrap();
        let mut command = Command::new(&binary);
        command
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", home.path())
            .env("CODEX_HOME", home.path())
            .current_dir(home.path())
            .args(["app-server", "--listen", "stdio://"]);
        for pair in argv.windows(2).filter(|p| p[0] == "--config") {
            command.args(pair);
        }
        let (tx, rx) = mpsc::unbounded_channel();
        tx.send(format!("{}\n", json!({"id":1,"method":"initialize","params":{"clientInfo":{"name":"native-config-test","version":"1"}}})).into_bytes()).unwrap();
        let mut tx = Some(tx);
        let mut buffer = vec![];
        let mut config = None;
        let exit = process::execute(&mut command, rx, Instant::now()+Duration::from_secs(15), &Control::default(), |stderr, bytes| {
            if stderr { return Ok(()); }
            buffer.extend_from_slice(bytes);
            while let Some(end) = buffer.iter().position(|b| *b == b'\n') {
                let line: Vec<_> = buffer.drain(..=end).collect();
                let response: Value = serde_json::from_slice(&line)?;
                if response["id"] == 1 {
                    assert!(response.get("error").is_none(), "{response}");
                    tx.as_ref().unwrap().send(format!("{}\n{}\n", json!({"method":"initialized"}), json!({"id":2,"method":"config/read","params":{"includeLayers":false,"cwd":home.path()}})).into_bytes())?;
                }
                if response["id"] == 2 {
                    assert!(response.get("error").is_none(), "{response}");
                    config = Some(response["result"]["config"].clone()); tx.take();
                }
            }
            Ok(())
        }).await.unwrap();
        assert!(exit.stop.is_none() && exit.status.unwrap().success());
        let c = config.unwrap();
        assert_eq!(c["agents"]["enabled"], true);
        assert_eq!(c["agents"]["max_concurrent_threads_per_session"], 12);
        assert_eq!(c["agents"]["max_depth"], 2);
        assert_eq!(c["agents"]["default_subagent_model"], "gpt-6-luna");
        assert_eq!(c["agents"]["default_subagent_reasoning_effort"], "high");
        assert_eq!(c["features"]["multi_agent"], true);
        assert_eq!(c["features"]["multi_agent_v2"]["enabled"], v2);
        assert_eq!(
            c["features"]["multi_agent_v2"]["max_concurrent_threads_per_session"],
            25
        );
        assert_eq!(
            c["features"]["multi_agent_v2"]["disable_native_tools"],
            false
        );
    }
}
