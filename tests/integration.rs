#![cfg(feature = "test-fixtures")]
use codex_subagent_rs::{
    completion::{self, Event, State},
    config::NATIVE_AGENT_POLICY,
};
use nix::{
    sys::signal::{Signal, kill},
    unistd::Pid,
};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines},
    process::{Child, ChildStdin, ChildStdout, Command},
    time::{Instant, sleep, timeout},
};

const BIN: &str = env!("CARGO_BIN_EXE_codex-subagent-rs");
const FAKE: &str = env!("CARGO_BIN_EXE_fixture-codex");
struct Client {
    child: Child,
    input: Option<ChildStdin>,
    output: Lines<BufReader<ChildStdout>>,
    next: u64,
    root: PathBuf,
    notifications: Vec<Value>,
}
impl Client {
    async fn start(root: &Path) -> Self {
        let mut child = isolated(BIN, root)
            .arg("mcp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut c = Self {
            input: child.stdin.take(),
            output: BufReader::new(child.stdout.take().unwrap()).lines(),
            child,
            next: 1,
            root: root.into(),
            notifications: vec![],
        };
        let initialized=c.request("initialize",json!({"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"isolated-test","version":"1"}})).await;
        assert!(
            initialized["result"]["capabilities"]["tools"].is_object(),
            "{initialized}"
        );
        c.send(json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
            .await;
        c
    }
    async fn send(&mut self, v: Value) {
        self.input
            .as_mut()
            .unwrap()
            .write_all(format!("{v}\n").as_bytes())
            .await
            .unwrap();
        self.input.as_mut().unwrap().flush().await.unwrap();
    }
    async fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next;
        self.next += 1;
        self.send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .await;
        timeout(Duration::from_secs(25), async {
            loop {
                let line = self
                    .output
                    .next_line()
                    .await
                    .unwrap()
                    .expect("MCP exited before response");
                let v: Value = serde_json::from_str(&line).unwrap();
                if v["id"] == id {
                    return v;
                }
                self.notifications.push(v);
            }
        })
        .await
        .expect("MCP request timed out")
    }
    async fn call(&mut self, name: &str, args: Value) -> Value {
        let v = self
            .request(
                "tools/call",
                json!({"name":name,"arguments":args,"_meta":{"progressToken":"test-progress"}}),
            )
            .await;
        assert!(v.get("error").is_none(), "unexpected protocol error: {v}");
        v["result"].clone()
    }
    async fn delegate(&mut self, case: &str, extra: Value) -> Value {
        let mut args = json!({"prompt":format!("CASE:{case}"),"working_dir":self.root,"skip_git_repo_check":true});
        args.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        self.call("codex_delegate", args).await
    }
    async fn stop(mut self) {
        drop(self.input.take());
        assert!(
            timeout(Duration::from_secs(3), self.child.wait())
                .await
                .unwrap()
                .unwrap()
                .success()
        );
    }
    async fn finish_job(&mut self, id: &str) -> Value {
        timeout(Duration::from_secs(12), async {
            loop {
                let status = self
                    .call(
                        "codex_job_status",
                        json!({"job_id":id,"include_activity":true}),
                    )
                    .await;
                let s = text(&status);
                if !s.contains("state: running") && !s.contains("still stopping") {
                    return self.call("codex_job_result", json!({"job_id":id})).await;
                }
                sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap()
    }
}
impl Drop for Client {
    fn drop(&mut self) {
        if let Some(pid) = self.child.id() {
            let _ = kill(Pid::from_raw(pid as i32), Signal::SIGTERM);
        }
    }
}
fn isolated(binary: &str, root: &Path) -> Command {
    let mut cmd = Command::new(binary);
    cmd.env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", root)
        .env("FIXTURE_ROOT", root)
        .env("CODEX_HOME", root.join("codex-home"))
        .env("CODEX_BIN", FAKE)
        .env("CODEX_SUBAGENT_DEFAULT_MODEL", "alpha")
        .env("CODEX_SUBAGENT_DEFAULT_EFFORT", "high")
        .env("CODEX_SUBAGENT_ALLOWED_MODELS", "alpha,beta")
        .env("CODEX_SUBAGENT_MAX_EFFORT", "xhigh")
        .env("CODEX_SUBAGENT_COMPLETION_DIR", root.join("events"))
        .current_dir(root);
    cmd
}
fn text(v: &Value) -> String {
    v["content"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|b| b["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}
fn success(v: &Value) {
    assert_ne!(v["isError"], true, "{}", text(v));
}
fn thread(v: &Value) -> String {
    text(v)
        .split("thread_id=")
        .nth(1)
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .trim()
        .to_owned()
}
fn job(v: &Value) -> String {
    success(v);
    text(v)
        .split("Started background delegation ")
        .nth(1)
        .unwrap()
        .split('.')
        .next()
        .unwrap()
        .to_owned()
}
fn calls(root: &Path) -> Vec<Value> {
    std::fs::read_to_string(root.join("calls.jsonl"))
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}
async fn wait_file(path: &Path) {
    timeout(Duration::from_secs(8), async {
        while !path.exists() {
            sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
}
fn background() -> Value {
    json!({"mode":"background"})
}

#[tokio::test]
async fn eight_tools_and_job_recovery_through_real_stdio() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path().canonicalize().unwrap();
    let mut c = Client::start(&root).await;
    let tools = c.request("tools/list", json!({})).await;
    let mut names: Vec<_> = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    names.sort();
    assert_eq!(
        names,
        [
            "codex_delegate",
            "codex_doctor",
            "codex_follow_up",
            "codex_job_cancel",
            "codex_job_result",
            "codex_job_status",
            "codex_quota",
            "list_codex_models"
        ]
    );
    let delegate = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "codex_delegate")
        .unwrap();
    for field in [
        "system_instructions",
        "context",
        "target_files",
        "acceptance_criteria",
    ] {
        assert!(delegate["inputSchema"]["properties"].get(field).is_none());
    }
    success(
        &c.call("codex_doctor", json!({"refresh":true,"working_dir":root}))
            .await,
    );
    std::fs::write(root.join("fail-login"), "").unwrap();
    let models = c
        .call("list_codex_models", json!({"working_dir":root}))
        .await;
    success(&models);
    assert!(text(&models).contains("32000 tokens"));
    assert!(text(&models).contains("64000 tokens"));
    assert!(!text(&models).contains("hidden"));
    assert!(!calls(&root).iter().any(|v| v["args"][0] == "app-server"));
    let catalog = calls(&root)
        .into_iter()
        .find(|v| v["args"][0] == "debug")
        .unwrap();
    assert_eq!(catalog["cwd"], root.to_string_lossy().as_ref());
    let before = calls(&root).len();
    for (name, args, error) in [
        (
            "codex_delegate",
            json!({"prompt":" \t\n "}),
            "prompt must not be empty",
        ),
        (
            "codex_follow_up",
            json!({"thread_id":"thread-1","prompt":" \t\n "}),
            "prompt must not be empty",
        ),
    ] {
        let invalid = c.call(name, args).await;
        assert_eq!(invalid["isError"], true);
        assert!(text(&invalid).contains(error));
    }
    assert_eq!(calls(&root).len(), before);
    std::fs::write(root.join("fail-plugin"), "").unwrap();
    let task =
        "CASE:native\nContext: context marker\nTarget files: target\nAcceptance criteria: checked";
    let result = c
        .delegate("native", json!({"prompt":task,"auto_approve":true}))
        .await;
    success(&result);
    assert!(text(&result).contains("PASS 中文 🦀"));
    assert!(text(&result).contains("requested model=alpha"));
    assert!(text(&result).contains("this call: total 110 | input 100 (cached 20, uncached 80, cache write 4) | output 10 (reasoning 2)"));
    assert!(text(&result).contains("parent thread only; native subagents excluded"));
    assert!(!text(&result).contains("CHILD_REPORT_NOT_PARENT_ANSWER"));
    assert!(c.notifications.iter().any(|v| {
        v["params"]["message"]
            .as_str()
            .is_some_and(|s| s.contains("Native subagents: spawn_agent"))
    }));
    let smuggled = c.delegate("success", json!({"thread_id":"--help"})).await;
    success(&smuggled);
    let id = thread(&result);
    let prompt = std::fs::read_to_string(root.join(format!("{id}.prompt"))).unwrap();
    assert_eq!(prompt, format!("{NATIVE_AGENT_POLICY}\n\n{task}"));
    let follow = c
        .call(
            "codex_follow_up",
            json!({"thread_id":id,"prompt":"continue"}),
        )
        .await;
    success(&follow);
    assert!(text(&follow).contains("this call: total 110 | input 100"));
    assert!(text(&follow).contains("thread cumulative: total 220 | input 200"));
    let follow_prompt = std::fs::read_to_string(root.join(format!("{id}.prompt"))).unwrap();
    assert!(
        follow_prompt.contains("Use Codex's native subagents according to the global AGENTS.md.")
    );
    assert!(!calls(&root).iter().any(|v| v["args"][0] == "plugin"));
    let schema = json!({"type":"object","properties":{"answer":{"type":"string"}},"required":["answer"],"additionalProperties":false});
    let retry = c
        .delegate(
            "capacity",
            json!({"output_schema":schema,"timeout_seconds":10}),
        )
        .await;
    success(&retry);
    assert!(text(&retry).contains("Automatically continued"));
    assert!(text(&retry).contains("BEGIN STRUCTURED RESULT"));
    assert!(text(&retry).contains("completed-once"));
    let rid = thread(&retry);
    let resume = calls(&root)
        .into_iter()
        .find(|v| v["args"][1] == "resume" && v["args"][2] == rid)
        .unwrap();
    let argv = resume["args"].as_array().unwrap();
    assert!(argv.contains(&json!("--output-schema")));
    assert!(argv.contains(&json!("sandbox_mode=\"read-only\"")));
    assert!(argv.contains(&json!("mcp_servers.self.enabled=false")));
    assert!(argv.contains(&json!("features.plugins=false")));
    assert!(argv.contains(&json!("agents.enabled=true")));
    assert!(argv.contains(&json!("features.multi_agent=true")));
    assert!(
        !argv
            .iter()
            .filter_map(Value::as_str)
            .any(|s| s.contains("max_concurrent_threads")
                || s.starts_with("agents.default_subagent_"))
    );
    let retry_prompt = std::fs::read_to_string(root.join(format!("{rid}.prompt"))).unwrap();
    assert!(
        retry_prompt.contains("Use Codex's native subagents according to the global AGENTS.md.")
    );
    let bad = c.delegate("failed", json!({"output_schema":schema})).await;
    assert_eq!(bad["isError"], true);
    assert!(text(&bad).contains("thread cumulative: unknown (not reported by Codex)"));
    assert!(!text(&bad).contains("BEGIN STRUCTURED RESULT"));
    let bad = c
        .delegate("bad-schema", json!({"output_schema":schema}))
        .await;
    assert_eq!(bad["isError"], true);
    let before = calls(&root).len();
    let bad = c
        .delegate("success", json!({"sandbox":"danger-full-access"}))
        .await;
    assert_eq!(bad["isError"], true);
    assert_eq!(calls(&root).len(), before);
    std::fs::write(root.join("fail-mcp"), "").unwrap();
    let bad = c.delegate("success", json!({})).await;
    assert_eq!(bad["isError"], true);
    std::fs::remove_file(root.join("fail-mcp")).unwrap();
    let origin = uuid::Uuid::new_v4().to_string();
    let a = job(&c
        .delegate("success", json!({"mode":"background","reply_to":origin}))
        .await);
    let b = job(&c
        .delegate(
            "failed",
            json!({"mode":"background","reply_to":uuid::Uuid::new_v4().to_string()}),
        )
        .await);
    let background_result = c.finish_job(&a).await;
    success(&background_result);
    assert!(text(&background_result).contains("this call: total 110"));
    let failed_background_result = c.finish_job(&b).await;
    assert_eq!(failed_background_result["isError"], true);
    let pending = root.join("events").join(format!("{a}.pending.json"));
    wait_file(&pending).await;
    let e: Event = serde_json::from_slice(&std::fs::read(pending).unwrap()).unwrap();
    assert_eq!(e.conversation_id, origin);
    assert_eq!(e.state, State::Completed);
    let no_callback = job(&c.delegate("success", background()).await);
    let no_callback_report = c.finish_job(&no_callback).await;
    success(&no_callback_report);
    assert!(
        !root
            .join("events")
            .join(format!("{no_callback}.pending.json"))
            .exists()
    );
    for (job_id, report, state) in [
        (&a, text(&background_result), "completed"),
        (&b, text(&failed_background_result), "failed"),
        (&no_callback, text(&no_callback_report), "completed"),
    ] {
        let saved: Value = serde_json::from_slice(
            &std::fs::read(root.join("events").join(format!("{job_id}.job.json"))).unwrap(),
        )
        .unwrap();
        assert_eq!(saved["job_id"], job_id.as_str());
        assert!(saved["thread_id"].is_string());
        assert_eq!(saved["state"], state);
        assert_eq!(saved["report"], report);
        assert!(saved.get("cancel").is_none());
        assert!(!root.join("events").join(format!("{job_id}.txt")).exists());
    }
    c.stop().await;
    let mut restarted = Client::start(&root).await;
    let saved = restarted
        .call("codex_job_result", json!({"job_id":a}))
        .await;
    success(&saved);
    assert_eq!(saved, background_result);
    for (job_id, state) in [
        (&a, "completed"),
        (&b, "failed"),
        (&no_callback, "completed"),
    ] {
        let status = restarted
            .call("codex_job_status", json!({"job_id":job_id}))
            .await;
        success(&status);
        assert!(text(&status).contains(&format!("state: {state}")));
    }
    let list = restarted.call("codex_job_status", json!({})).await;
    for job_id in [&a, &b, &no_callback] {
        assert!(text(&list).contains(job_id));
    }
    let saved = restarted
        .call("codex_job_result", json!({"job_id":no_callback}))
        .await;
    assert_eq!(saved, no_callback_report);
    std::fs::remove_file(root.join("events").join(format!("{a}.pending.json"))).unwrap();
    assert_eq!(
        restarted
            .call("codex_job_result", json!({"job_id":a}))
            .await,
        background_result
    );
    std::fs::remove_file(root.join("events").join(format!("{a}.job.json"))).unwrap();
    let missing = restarted
        .call("codex_job_result", json!({"job_id":a}))
        .await;
    assert_eq!(missing["isError"], true);
    assert!(text(&missing).contains("Unknown job id"));
    assert_eq!(
        restarted
            .call("codex_job_result", json!({"job_id":b}))
            .await["isError"],
        true
    );
    let recovered = restarted
        .call(
            "codex_follow_up",
            json!({"thread_id":id,"prompt":"after restart"}),
        )
        .await;
    success(&recovered);
    let read_requests = std::fs::read_to_string(root.join("requests.jsonl")).unwrap();
    assert!(
        read_requests
            .lines()
            .map(|l| serde_json::from_str::<Value>(l).unwrap())
            .any(|v| v["method"] == "thread/read" && v["params"]["threadId"] == id)
    );
    let resumed = calls(&root)
        .into_iter()
        .rev()
        .find(|v| v["args"][0] == "exec")
        .unwrap();
    assert_eq!(resumed["cwd"], root.to_string_lossy().as_ref());
    assert!(
        resumed["args"]
            .as_array()
            .unwrap()
            .contains(&json!("alpha"))
    );
    assert!(text(&recovered).contains("this call: unknown"));
    assert!(text(&recovered).contains("thread cumulative: total 220"));
    // Native metadata locates a worktree for capacity continuation and later follow-up.
    let worktree = restarted
        .delegate(
            "capacity",
            json!({"use_worktree":true,"timeout_seconds":10}),
        )
        .await;
    success(&worktree);
    let worktree_id = thread(&worktree);
    let resumed = calls(&root)
        .into_iter()
        .rev()
        .find(|v| v["args"][1] == "resume" && v["args"][2] == worktree_id)
        .unwrap();
    assert_eq!(
        resumed["cwd"],
        root.join("worktree").to_string_lossy().as_ref()
    );
    let follow = restarted
        .call(
            "codex_follow_up",
            json!({"thread_id":worktree_id,"prompt":"continue"}),
        )
        .await;
    success(&follow);
    assert_eq!(
        calls(&root).last().unwrap()["cwd"],
        root.join("worktree").to_string_lossy().as_ref()
    );
    assert_eq!(
        calls(&root)
            .iter()
            .filter(|v| v["args"][0] == "login")
            .count(),
        1
    );

    std::fs::write(root.join("fail-catalog"), "").unwrap();
    let failed_catalog = restarted
        .call(
            "list_codex_models",
            json!({"working_dir":root,"refresh":true}),
        )
        .await;
    assert_eq!(failed_catalog["isError"], true);
    assert!(text(&failed_catalog).contains("fixture catalog unavailable"));
    assert!(!text(&failed_catalog).contains("gpt-5.6"));
    restarted.stop().await;
}

#[tokio::test]
async fn persisted_job_survives_owner_exit_without_restarting_work() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path().canonicalize().unwrap();
    let mut owner = Client::start(&root).await;
    // An unwritable destination must fail before starting a model process.
    std::fs::write(root.join("events"), "not a directory").unwrap();
    let failed = owner.delegate("success", background()).await;
    assert_eq!(failed["isError"], true);
    assert!(text(&failed).contains("nothing was started"));
    assert!(!calls(&root).iter().any(|v| v["args"][0] == "exec"));
    std::fs::remove_file(root.join("events")).unwrap();

    let id = job(&owner.delegate("hold", background()).await);
    let record = root.join("events").join(format!("{id}.job.json"));
    let saved: Value = timeout(Duration::from_secs(5), async {
        loop {
            let saved: Value = serde_json::from_slice(&std::fs::read(&record).unwrap()).unwrap();
            if saved["thread_id"].is_string() {
                break saved;
            }
            sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    let thread_id = saved["thread_id"].as_str().unwrap();
    let pid_file = root.join(format!("{thread_id}.pid"));
    wait_file(&pid_file).await;
    let codex_pid: i32 = std::fs::read_to_string(pid_file).unwrap().parse().unwrap();

    let mut reader = Client::start(&root).await;
    let status = reader.call("codex_job_status", json!({"job_id":id})).await;
    assert!(text(&status).contains("state: running"));
    let cancelled = reader.call("codex_job_cancel", json!({"job_id":id})).await;
    assert_eq!(cancelled["isError"], true);
    assert!(text(&cancelled).contains("another MCP process"));
    reader.stop().await;

    owner.child.start_kill().unwrap();
    owner.child.wait().await.unwrap();
    // This fixture is the sole process in its group; clean up the intentionally orphaned run.
    kill(Pid::from_raw(-codex_pid), Signal::SIGKILL).unwrap();
    let mut restarted = Client::start(&root).await;
    let status = restarted
        .call("codex_job_status", json!({"job_id":id}))
        .await;
    success(&status);
    assert!(text(&status).contains("state: interrupted"));
    assert!(text(&status).contains(thread_id));
    let result = restarted
        .call("codex_job_result", json!({"job_id":id}))
        .await;
    assert_eq!(result["isError"], true);
    assert!(text(&result).contains("before saving a final result"));
    assert!(text(&result).contains(thread_id));
    assert_eq!(
        calls(&root)
            .iter()
            .filter(|v| v["args"][0] == "exec")
            .count(),
        1
    );
    restarted.stop().await;
}

#[tokio::test]
async fn quota_reports_remaining_windows_without_a_model_call() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path().canonicalize().unwrap();
    let mut c = Client::start(&root).await;
    let response = root.join("quota-response.json");
    let bucket = json!({"limitId":"codex","planType":"pro","primary":{"usedPercent":25,"windowDurationMins":300,"resetsAt":1730947200},"secondary":{"usedPercent":60,"windowDurationMins":10080,"resetsAt":null},"credits":{"balance":"DO_NOT_RETURN_MONEY"}});
    std::fs::write(&response, json!({"id":2,"result":{"rateLimits":bucket,"ordinaryUsageAllowed":false,"rateLimitsByLimitId":{"codex":bucket,"other":{"limitName":"Other models","primary":{"usedPercent":105},"rateLimitReachedType":"rate_limit_reached","spendControlReached":true,"individualLimit":{"remainingPercent":30,"resetsAt":1730947200}}}}}).to_string()).unwrap();
    let quota = c.call("codex_quota", json!({"working_dir":root})).await;
    success(&quota);
    let report = text(&quota);
    assert!(
        report
            .contains("75% remaining (25% used) | window: 300 min | resets: 2024-11-07T02:40:00Z")
    );
    assert!(report.contains("40% remaining (60% used) | window: 10080 min | resets: unknown"));
    assert_eq!(report.matches("75% remaining").count(), 1);
    assert!(report.contains("0% remaining (105% used)"));
    assert!(report.contains("secondary: unknown (not reported)"));
    assert!(report.contains("ordinary usage allowed: false"));
    assert!(report.contains("spend control reached: true"));
    assert!(report.contains("individual limit: 30% remaining"));
    assert!(!report.contains("DO_NOT_RETURN_MONEY"));
    let request: Value =
        serde_json::from_slice(&std::fs::read(root.join("quota-request.json")).unwrap()).unwrap();
    assert_eq!(request["method"], "account/rateLimits/read");
    assert_eq!(request["params"], json!({"excludeResetCreditDetails":true}));
    assert!(
        calls(&root)
            .iter()
            .all(|v| v["args"][0] == "app-server" && v["cwd"] == root.to_string_lossy().as_ref())
    );

    // Legacy single-bucket response and absent windows remain distinct from unlimited quota.
    std::fs::write(
        &response,
        json!({"id":2,"result":{"rateLimits":{}}}).to_string(),
    )
    .unwrap();
    let quota = c.call("codex_quota", json!({})).await;
    success(&quota);
    assert!(text(&quota).contains("primary: unknown"));
    assert!(!text(&quota).contains("100% remaining"));
    std::fs::write(
        &response,
        json!({"id":2,"result":{"rateLimits":bucket}}).to_string(),
    )
    .unwrap();
    let quota = c.call("codex_quota", json!({})).await;
    success(&quota);
    assert!(text(&quota).contains("75% remaining"));

    std::fs::write(&response, json!({"id":2,"error":{"code":-32600,"message":"ChatGPT authentication required; API-key login cannot read this quota"}}).to_string()).unwrap();
    let quota = c.call("codex_quota", json!({})).await;
    assert_eq!(quota["isError"], true);
    assert!(text(&quota).contains("ChatGPT authentication required"));
    std::fs::write(
        &response,
        json!({"id":2,"result":{"rateLimits":{"primary":{}}}}).to_string(),
    )
    .unwrap();
    let quota = c.call("codex_quota", json!({})).await;
    assert_eq!(quota["isError"], true);
    assert!(text(&quota).contains("usedPercent"));
    c.stop().await;
}

#[tokio::test]
async fn cancellation_timeout_and_shutdown_reap_descendant_groups() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path().canonicalize().unwrap();
    let mut c = Client::start(&root).await;
    let origin = uuid::Uuid::new_v4().to_string();
    let id = job(&c
        .delegate("slow", json!({"mode":"background","reply_to":origin}))
        .await);
    let child = wait_child(&root).await;
    let status = c.call("codex_job_status", json!({"job_id":id})).await;
    assert!(text(&status).contains("running"));
    assert!(text(&status).contains("Native subagents: spawn_agent completed"));
    success(&c.call("codex_job_cancel", json!({"job_id":id})).await);
    let partial = c.finish_job(&id).await;
    assert_eq!(partial["isError"], true);
    assert!(text(&partial).contains("cancelled"));
    assert!(text(&partial).contains(" | thread_id="));
    assert!(!text(&partial).contains("Continue with codex_follow_up"));
    assert_dead(child).await;
    let saved: Value = serde_json::from_slice(
        &std::fs::read(root.join("events").join(format!("{id}.job.json"))).unwrap(),
    )
    .unwrap();
    assert_eq!(saved["state"], "cancelled");
    assert_eq!(saved["report"], text(&partial));
    assert!(!root.join("events").join(format!("{id}.txt")).exists());
    let partial = c.delegate("slow", json!({"timeout_seconds":1})).await;
    assert_eq!(partial["isError"], true);
    assert!(text(&partial).contains("timeout"));
    // A capacity wait consumes the same deadline and never restarts the original task.
    let before = calls(&root)
        .iter()
        .filter(|v| v["args"][0] == "exec")
        .count();
    let result = c
        .delegate("always-capacity", json!({"timeout_seconds":1}))
        .await;
    assert_eq!(result["isError"], true);
    assert!(text(&result).contains("timeout"));
    let after = calls(&root)
        .iter()
        .filter(|v| v["args"][0] == "exec")
        .count();
    assert_eq!(after - before, 1);
    for file in std::fs::read_dir(&root).unwrap() {
        let p = file.unwrap().path();
        if p.extension().is_some_and(|s| s == "child") {
            std::fs::remove_file(p).unwrap();
        }
    }
    let _ = job(&c.delegate("slow", background()).await);
    let child = wait_child(&root).await;
    let started = Instant::now();
    c.stop().await;
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_dead(child).await;
    let mut restarted = Client::start(&root).await;
    let status = restarted
        .call("codex_job_status", json!({"job_id":id}))
        .await;
    assert!(text(&status).contains("state: cancelled"));
    let saved = restarted
        .call("codex_job_result", json!({"job_id":id}))
        .await;
    assert_eq!(saved["isError"], true);
    assert!(text(&saved).contains("cancelled"));
    restarted.stop().await;
}
async fn wait_child(root: &Path) -> i32 {
    timeout(Duration::from_secs(5), async {
        loop {
            for file in std::fs::read_dir(root).unwrap() {
                let p = file.unwrap().path();
                if p.extension().is_some_and(|s| s == "child") {
                    return std::fs::read_to_string(p).unwrap().parse().unwrap();
                }
            }
            sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap()
}
async fn assert_dead(pid: i32) {
    timeout(Duration::from_secs(3), async {
        while kill(Pid::from_raw(pid), None).is_ok() {
            sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("owned descendant survived cancellation");
}

#[tokio::test]
async fn sidecar_replays_failures_routes_origins_and_does_not_replay_sent() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path().canonicalize().unwrap();
    let dir = root.join("events");
    let a = Event {
        job_id: uuid::Uuid::new_v4().to_string(),
        conversation_id: uuid::Uuid::new_v4().to_string(),
        state: State::Completed,
        finished_at: 1,
    };
    completion::publish(&dir, &a).unwrap();
    std::fs::write(root.join("fail-send"), "").unwrap();
    let mut child = isolated(BIN, &root)
        .env("ANTIGRAVITY_AGENTAPI_EXE", FAKE)
        .arg("sidecar")
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    timeout(Duration::from_secs(5), async {
        loop {
            if root.join("calls.jsonl").exists()
                && calls(&root).iter().any(|c| c["args"][0] == "agentapi")
            {
                break;
            }
            sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    kill(Pid::from_raw(child.id().unwrap() as i32), Signal::SIGTERM).unwrap();
    timeout(Duration::from_secs(3), child.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(dir.join(format!("{}.pending.json", a.job_id)).exists());
    std::fs::remove_file(root.join("fail-send")).unwrap();
    let mut child = isolated(BIN, &root)
        .env("ANTIGRAVITY_AGENTAPI_EXE", FAKE)
        .arg("sidecar")
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    wait_file(&dir.join(format!("{}.sent.json", a.job_id))).await;
    let b = Event {
        job_id: uuid::Uuid::new_v4().to_string(),
        conversation_id: uuid::Uuid::new_v4().to_string(),
        state: State::Failed,
        finished_at: 2,
    };
    completion::publish(&dir, &b).unwrap();
    wait_file(&dir.join(format!("{}.sent.json", b.job_id))).await;
    kill(Pid::from_raw(child.id().unwrap() as i32), Signal::SIGTERM).unwrap();
    timeout(Duration::from_secs(3), child.wait())
        .await
        .unwrap()
        .unwrap();
    let sent = std::fs::read_to_string(root.join("sent.jsonl")).unwrap();
    let messages: Vec<Value> = sent
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0][3], a.conversation_id);
    assert_eq!(messages[1][3], b.conversation_id);
    for (message, event) in messages.iter().zip([&a, &b]) {
        assert_eq!(
            message[4],
            format!(
                "Codex completion event {}: {}.\nRead codex/codex_job_result with job_id \"{}\" once to integrate or report the outcome. Ignore duplicate notifications for an already-handled job. Do not resume cancelled jobs.",
                event.job_id, event.state, event.job_id
            )
        );
    }
    let mut child = isolated(BIN, &root)
        .env("ANTIGRAVITY_AGENTAPI_EXE", FAKE)
        .arg("sidecar")
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    sleep(Duration::from_millis(250)).await;
    kill(Pid::from_raw(child.id().unwrap() as i32), Signal::SIGTERM).unwrap();
    timeout(Duration::from_secs(3), child.wait())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(root.join("sent.jsonl")).unwrap(),
        sent
    );
}

#[tokio::test]
async fn request_cancellation_host_signal_and_hook_entrypoint() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path().canonicalize().unwrap();
    let origin = uuid::Uuid::new_v4().to_string();
    let event = json!({"conversationId":origin,"toolCall":{"name":"call_mcp_tool","args":{"ServerName":"codex","ToolName":"codex_delegate","Arguments":{"prompt":"task","mode":"background","reply_to":"wrong-origin"}}}});
    let mut hook = isolated(BIN, &root)
        .arg("hook")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    hook.stdin
        .take()
        .unwrap()
        .write_all(event.to_string().as_bytes())
        .await
        .unwrap();
    let output = hook.wait_with_output().await.unwrap();
    assert!(output.status.success());
    let bound: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(bound["overwrite"]["Arguments"]["reply_to"], origin);
    assert_eq!(bound["decision"], "ask");
    let mut c = Client::start(&root).await;
    let request = c.next;
    c.next += 1;
    c.send(json!({"jsonrpc":"2.0","id":request,"method":"tools/call","params":{"name":"codex_delegate","arguments":{"prompt":"CASE:slow","working_dir":root,"skip_git_repo_check":true}}})).await;
    let pid = wait_child(&root).await;
    c.send(json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":request,"reason":"test cancellation"}})).await;
    timeout(Duration::from_secs(8), async {
        while kill(Pid::from_raw(pid), None).is_ok() {
            sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("MCP cancellation left a descendant alive");
    for file in std::fs::read_dir(&root).unwrap() {
        let p = file.unwrap().path();
        if p.extension().is_some_and(|s| s == "child") {
            std::fs::remove_file(p).unwrap();
        }
    }
    let _ = job(&c.delegate("slow", background()).await);
    let pid = wait_child(&root).await;
    // Keep stdin open: macOS's blocking stdin must not hold up SIGTERM shutdown.
    let start = Instant::now();
    kill(Pid::from_raw(c.child.id().unwrap() as i32), Signal::SIGTERM).unwrap();
    assert!(
        timeout(Duration::from_secs(2), c.child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    assert!(start.elapsed() < Duration::from_secs(2));
    assert_dead(pid).await;
}
