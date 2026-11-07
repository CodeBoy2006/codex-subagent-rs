//! Isolated process fixture; never calls a model or reads the user's Codex home.
use serde_json::{Value, json};
use std::{
    fs,
    io::{self, BufRead, Read, Write},
    os::unix::process::CommandExt,
    path::PathBuf,
    process::{Command, Stdio},
};
fn emit(v: Value) {
    println!("{v}");
    io::stdout().flush().unwrap();
}
fn arg(args: &[String], key: &str) -> Option<String> {
    args.windows(2).find(|p| p[0] == key).map(|p| p[1].clone())
}
fn collab(phase: &str, tool: &str, status: &str, parent: &str, child: &str, state: &str) {
    emit(json!({"type": phase, "item": {
        "id": format!("collab-{tool}"), "type":"collab_tool_call", "tool":tool,
        "sender_thread_id":parent, "receiver_thread_ids":[child], "status":status,
        "agents_states":{child:{"status":state,"message":"CHILD_REPORT_NOT_PARENT_ANSWER"}}
    }}));
}
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let root = PathBuf::from(
        std::env::var_os("FIXTURE_ROOT").expect("fixture needs isolated FIXTURE_ROOT"),
    );
    fs::create_dir_all(&root).unwrap();
    let mut log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(root.join("calls.jsonl"))
        .unwrap();
    log.write_all(
        format!(
            "{}\n",
            json!({"args":args,"cwd":std::env::current_dir().unwrap()})
        )
        .as_bytes(),
    )
    .unwrap();
    match args.first().map(String::as_str).unwrap_or("") {
        "--version" => println!("codex-cli 0.165.0"),
        "login" => {
            if root.join("fail-login").exists() {
                eprintln!("fixture login status unavailable");
                std::process::exit(1);
            }
            println!("Logged in using fixture");
        }
        "debug" => {
            if root.join("fail-catalog").exists() {
                eprintln!("fixture catalog unavailable");
                std::process::exit(1);
            }
            emit(json!({"models":[
                {"slug":"alpha","display_name":"Alpha","description":"test model","visibility":"list","supported_reasoning_levels":[{"effort":"low"},{"effort":"high"},{"effort":"xhigh"}],"default_reasoning_level":"high","context_window":32000,"priority":1,"input_modalities":["image"],"supports_search_tool":true},
                {"slug":"beta","visibility":"list","supported_reasoning_levels":["low","high"],"default_reasoning_level":"low","context_window":64000,"priority":2},
                {"slug":"hidden","visibility":"hide"}
            ]}));
        }
        "app-server" => {
            for line in io::stdin().lock().lines() {
                let v: Value = serde_json::from_str(&line.unwrap()).unwrap();
                if v["id"] == 1 {
                    emit(json!({"id":1,"result":{}}));
                }
                if v["id"] == 2 {
                    let mut requests = fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(root.join("requests.jsonl"))
                        .unwrap();
                    requests.write_all(format!("{v}\n").as_bytes()).unwrap();
                    if v["method"] == "thread/read" {
                        assert_eq!(v["params"]["includeTurns"], false);
                        let id = v["params"]["threadId"].as_str().unwrap();
                        match fs::read(root.join(format!("{id}.metadata.json"))) {
                            Ok(bytes) => emit(
                                json!({"id":2,"result":{"thread":serde_json::from_slice::<Value>(&bytes).unwrap()}}),
                            ),
                            Err(_) => emit(
                                json!({"id":2,"error":{"code":-32600,"message":"thread metadata unavailable"}}),
                            ),
                        }
                        continue;
                    }
                    if v["method"] == "account/rateLimits/read" {
                        fs::write(root.join("quota-request.json"), v.to_string()).unwrap();
                        let response: Value = serde_json::from_slice(
                            &fs::read(root.join("quota-response.json")).unwrap(),
                        )
                        .unwrap();
                        emit(response);
                        continue;
                    }
                    emit(
                        json!({"id":2,"error":{"code":-32601,"message":"unsupported fixture method"}}),
                    );
                }
            }
        }
        "mcp" => {
            if root.join("fail-mcp").exists() {
                eprintln!("fixture inventory failed");
                std::process::exit(1);
            }
            let value=fs::read(root.join("mcp.json")).ok().map(|b|serde_json::from_slice::<Value>(&b).unwrap()).unwrap_or(json!([{"name":"self","transport":{"command":"codex-subagent-rs"}},{"name":"docs","transport":{"command":"documentation-server"}}]));
            emit(value);
        }
        "plugin" => {
            if root.join("fail-plugin").exists() {
                eprintln!("fixture marketplace unavailable");
                std::process::exit(1);
            }
            emit(
                json!({"installed":[{"pluginId":"keep@local","installed":true,"enabled":true},{"pluginId":"drop@local","installed":true,"enabled":true}]}),
            );
        }
        "agentapi" => {
            if root.join("fail-send").exists() {
                eprintln!("fixture send failed");
                std::process::exit(1);
            }
            let mut f = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(root.join("sent.jsonl"))
                .unwrap();
            f.write_all(format!("{}\n", json!(args)).as_bytes())
                .unwrap();
            println!("accepted");
        }
        "exec" => {
            let mut prompt = String::new();
            io::stdin().read_to_string(&mut prompt).unwrap();
            let resume = args.get(1).is_some_and(|s| s == "resume");
            let thread = if resume {
                args[2].clone()
            } else {
                uuid::Uuid::new_v4().to_string()
            };
            let case = if resume {
                fs::read_to_string(root.join(format!("{thread}.case"))).unwrap_or("success".into())
            } else {
                prompt
                    .split("CASE:")
                    .nth(1)
                    .unwrap_or("success")
                    .split_whitespace()
                    .next()
                    .unwrap_or("success")
                    .to_owned()
            };
            fs::write(root.join(format!("{thread}.case")), &case).unwrap();
            fs::write(root.join(format!("{thread}.prompt")), &prompt).unwrap();
            let model = arg(&args, "--model").unwrap_or("alpha".into());
            let setting = |key: &str| {
                args.windows(2)
                    .find(|p| p[0] == "--config" && p[1].starts_with(&format!("{key}=")))
                    .map(|p| p[1].split_once('=').unwrap().1.trim_matches('"').to_owned())
            };
            let cwd = if args.iter().any(|a| a == "--worktree") {
                let cwd = root.join("worktree");
                fs::create_dir_all(&cwd).unwrap();
                cwd
            } else {
                std::env::current_dir().unwrap()
            };
            fs::write(root.join(format!("{thread}.metadata.json")), json!({"model":model,"reasoningEffort":setting("model_reasoning_effort"),"cwd":cwd}).to_string()).unwrap();
            emit(json!({"type":"thread.started","thread_id":thread}));
            emit(json!({"type":"turn.started"}));
            if case == "hold" {
                fs::write(
                    root.join(format!("{thread}.pid")),
                    std::process::id().to_string(),
                )
                .unwrap();
                std::thread::sleep(std::time::Duration::from_secs(60));
                return;
            }
            if case == "native" || case == "slow" {
                let child = uuid::Uuid::new_v4().to_string();
                collab(
                    "item.started",
                    "spawn_agent",
                    "in_progress",
                    &thread,
                    &child,
                    "pending_init",
                );
                collab(
                    "item.completed",
                    "spawn_agent",
                    "completed",
                    &thread,
                    &child,
                    "running",
                );
                if case == "native" {
                    collab(
                        "item.updated",
                        "wait",
                        "in_progress",
                        &thread,
                        &child,
                        "running",
                    );
                    collab(
                        "item.completed",
                        "wait",
                        "completed",
                        &thread,
                        &child,
                        "completed",
                    );
                    collab(
                        "item.completed",
                        "close_agent",
                        "completed",
                        &thread,
                        &child,
                        "shutdown",
                    );
                }
            }
            if case == "slow" {
                let mut child = Command::new("/bin/sh")
                    .args(["-c", "trap '' INT TERM; while :; do sleep 1; done"])
                    .stdin(Stdio::null())
                    .process_group(0)
                    .spawn()
                    .unwrap();
                fs::write(root.join(format!("{thread}.child")), child.id().to_string()).unwrap();
                let _ = child.wait();
                return;
            }
            if case == "capacity" && !resume || case == "always-capacity" {
                emit(
                    json!({"type":"item.completed","item":{"type":"command_execution","command":"completed-once","exit_code":0}}),
                );
                emit(
                    json!({"type":"turn.failed","error":{"message":codex_subagent_rs::runner::CAPACITY}}),
                );
                std::process::exit(1);
            }
            if case == "failed" {
                emit(
                    json!({"type":"item.completed","item":{"type":"agent_message","text":"Partial answer"}}),
                );
                emit(json!({"type":"turn.failed","error":{"message":"fixture turn failure"}}));
                return;
            }
            if case == "bad-schema" {
                emit(
                    json!({"type":"item.completed","item":{"type":"agent_message","text":"not JSON"}}),
                );
                return;
            }
            emit(
                json!({"type":"item.completed","item":{"type":"command_execution","command":"printf safe","exit_code":0}}),
            );
            emit(
                json!({"type":"item.completed","item":{"type":"file_change","changes":[{"kind":"edit","path":"fixture.txt"}]}}),
            );
            let answer = if arg(&args, "--output-schema").is_some() {
                r#"{"answer":"ok"}"#
            } else {
                "PASS 中文 🦀"
            };
            let bytes = format!(
                "{}\n",
                json!({"type":"item.completed","item":{"type":"agent_message","text":answer}})
            )
            .into_bytes();
            for chunk in bytes.chunks(7) {
                io::stdout().write_all(chunk).unwrap();
                io::stdout().flush().unwrap();
            }
            emit(
                json!({"type":"turn.completed","usage":{"input_tokens":if resume{200}else{100},"cached_input_tokens":if resume{40}else{20},"cache_write_input_tokens":if resume{8}else{4},"output_tokens":if resume{20}else{10},"reasoning_output_tokens":if resume{4}else{2}}}),
            );
        }
        _ => {
            eprintln!("unknown fixture command {args:?}");
            std::process::exit(2);
        }
    }
}
