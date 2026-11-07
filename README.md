# codex-subagent-rs

Delegate a task to your existing Codex CLI, continue its conversation, or run it in the background and receive the result in the original Antigravity chat. One native executable provides the MCP server, the Antigravity Hook, and the completion Sidecar.

The server provides eight tools for delegation, follow-ups, background jobs, model discovery and usage queries. It uses `codex exec`; Codex still owns authentication, model calls, sandboxing, tools, and conversation history.

## Build and test without changing your setup

Requires Rust 1.88+ and a Unix host. macOS is the tested platform; Windows is not supported by this release. Build the native binary:

```sh
cargo build --release --locked --bin codex-subagent-rs
./target/release/codex-subagent-rs --help
```

Run the isolated tests:

```sh
cargo test --all-features --locked
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo fmt --check
```

The `test-fixtures` feature builds a fake Codex executable. Integration tests clear their subprocess environments, use temporary homes, and substitute both Codex and the Antigravity message sender. They make no model calls, send no real notifications, and do not read your Codex credentials. On macOS, process-tree and filesystem-watcher tests need permission to inspect their own descendants and watch temporary directories.

No command installs a service, changes a client configuration, or restarts an existing MCP. The initial delivery is implementation-and-test only; authenticated Codex and live Antigravity acceptance remain a separate step before switching clients.

## Use the three entrypoints

| Command | Input and output |
| --- | --- |
| `codex-subagent-rs mcp` | MCP JSON-RPC on stdin/stdout; diagnostics on stderr. This is the default mode. |
| `codex-subagent-rs hook` | One Antigravity tool event on stdin; an approval decision JSON object on stdout. |
| `codex-subagent-rs sidecar` | Watches completion files and invokes Antigravity's `agentapi send-message`. Logs to stderr. |

Set `CODEX_BIN` to the native Codex executable when your GUI app does not inherit the right `PATH`.

MCP and Sidecar must share an absolute completion directory. Set `CODEX_SUBAGENT_COMPLETION_DIR`, or pass `--events-dir ABSOLUTE_PATH` to both. The default is:

```text
~/.local/share/codex-subagent-rs/completion/events
```

The Hook does not read or write completion files. It matches only server `codex`, tool `codex_delegate`, and `mode: "background"`; it preserves `decision: "ask"` and replaces `reply_to` with the host's conversation ID.

The Sidecar uses `agentapi` from `PATH`. If Antigravity supplies `ANTIGRAVITY_AGENTAPI_EXE`, it invokes that binary with `agentapi send-message ...`. Register the Sidecar with the host when switching; running the MCP alone does not start notifications.

### Native delegation follows the global AGENTS.md

Native subagent tools are enabled for new tasks, follow-ups and capacity continuations. A single prompt instruction says to follow the global `AGENTS.md` for subagent orchestration; Codex's built-in instruction loading handles the file. Project instructions continue to govern repository work.

The MCP adds **no child concurrency cap, model allow-list, effort ceiling or child-model default**. Existing Codex `[agents]` settings and custom roles remain in effect; CLI runtime limits still apply. The server's existing model/effort policy validates the **parent delegation**, not its native children. The eight-background-job limit also remains an outer-job limit.

Self-recursive MCP delegation stays disabled. The guard checks MCP commands and arguments for the `codex-subagent-rs` executable name or the current executable path. Other servers follow `CODEX_SUBAGENT_MCP_SERVERS` without special treatment. Native collaboration events, when emitted by Codex, appear in blocking progress and background-job activity; child reports do not overwrite the parent's final answer. Native threads remain owned by Codex, within the same CLI invocation and cancellation lifecycle.

The enabling flags were checked against Codex CLI 0.165.0. To repeat the configuration-only check with your native binary, set `CODEX_NATIVE_TEST_BIN`, then run:

```sh
cargo test --locked --test cli_config -- --ignored
```

This check uses temporary Codex homes, verifies both native backends' existing settings are preserved, and starts no model turn. It is skipped in the default test suite because a real Codex installation is not required for ordinary development.

### The eight tools

| Tool | Purpose |
| --- | --- |
| `codex_quota` | Fetch remaining usage percentages and reset times without a model call. |
| `codex_doctor` | Diagnose executable, version, login status and configuration on demand. |
| `list_codex_models` | List live models, efforts and context windows from the CLI catalog. |
| `codex_delegate` | Execute a self-contained task, blocking or in the background. |
| `codex_follow_up` | Continue a Codex `thread_id`, preserving its model, effort and directory unless overridden. |
| `codex_job_status` | Inspect a background job, or list known jobs when `job_id` is omitted. |
| `codex_job_result` | Read a finished job, including persisted callback reports after an MCP restart. |
| `codex_job_cancel` | Request cancellation and terminate the job's process tree. |

Tool descriptions and input schemas have one source in [schemas](schemas/). Runtime validation rejects invalid directories, thread IDs, schema sizes, model choices and policy violations before executing a model. Unknown fields are stripped per tool; they cannot switch a delegation into a follow-up.

A blocking `codex_delegate` call needs a `prompt` and either an explicit `model` or a configured default. Put the task, context, relevant paths and completion criteria together in `prompt`. Supply an absolute `working_dir` when the task belongs to a particular project. `output_schema` is passed directly to Codex’s native structured-output option; valid JSON is exposed in a delimited result only if the entire run succeeded. The delegated prompt is the global-subagent instruction followed by the caller's trimmed `prompt`.

A background call returns a `job_id`. If it says completion delivery is registered, let the Sidecar notify the originating chat; otherwise use `codex_job_status` and then `codex_job_result`. Continue a conversation with its **`thread_id`**, not its `job_id`.

Follow-ups recover model, effort and directory from memory or Codex’s `thread/read` API after restart. The wrapper does not scan private session files. Settings in reports are requested settings; Codex enforces permissions. Worktree paths are read from native thread metadata when needed.

Model listing uses each model's `context_window` from the installed CLI catalog, not a configuration override; missing values remain `unknown`. Catalog failures return the CLI error rather than a hardcoded substitute. Sign-in checks run only when `codex_doctor` is called.

### Token usage in task results

Blocking results, background results and saved completion reports include a token-usage block: total, input, cached/uncached input, cache-write input, output and reasoning output. Total is input + output; cached input and reasoning output are breakdowns, not extra tokens to add.

The report distinguishes **this call** from **thread cumulative**. Codex 0.165.0 emits cumulative usage on resume; the MCP subtracts its previous snapshot for a known thread. After an MCP restart or when a comparable snapshot is unavailable, this-call usage is `unknown`. Missing usage, including interrupted runs without a completion event, is not treated as zero.

These counts cover the **parent thread only**. Native child-agent usage is not included in Codex's root JSONL usage event and is not aggregated by this wrapper.

### Query remaining usage quota separately

Call `codex_quota` with no arguments, or use the same directory as your delegation:

```json
{"working_dir":"/absolute/path/to/project"}
```

The tool queries Codex's `account/rateLimits/read` on every call and returns each reported bucket's remaining percentage, window duration and UTC reset time. It also shows the backend's usage-allowed and limit-reached states when available; a positive percentage alone does not mean a request is permitted. Window lengths come from the account, rather than being fixed at five hours or one week.

It reuses Codex's sign-in and never starts a model turn. Unreported windows remain `unknown`; authentication, network and unsupported-endpoint errors are surfaced. It does not return money/credits, consume a quota reset, or estimate how many tokens remain. API-key-only accounts cannot use the ChatGPT quota endpoint.

### Environment policy

| Variable | Default / behavior |
| --- | --- |
| `CODEX_BIN` | `codex` on `PATH` |
| `CODEX_HOME` | Passed through to Codex; normally `~/.codex` |
| `CODEX_SUBAGENT_DEFAULT_MODEL` | Unset; a single-entry allow-list can supply the default |
| `CODEX_SUBAGENT_ALLOWED_MODELS` | Unrestricted; comma-separated model slugs otherwise |
| `CODEX_SUBAGENT_DEFAULT_EFFORT` | Model default |
| `CODEX_SUBAGENT_MAX_EFFORT` | Unrestricted; clamps requested effort to supported levels below the ceiling |
| `CODEX_SUBAGENT_DEFAULT_SANDBOX` | `read-only` |
| `CODEX_SUBAGENT_MAX_SANDBOX` | `workspace-write`; requests exceeding it are refused |
| `CODEX_SUBAGENT_MCP_SERVERS` | `none`; accepts `all` or comma-separated names; self-delegation entries are disabled |
| `CODEX_SUBAGENT_PLUGINS` | `none`; accepts `all` or comma-separated plugin IDs |
| `CODEX_SUBAGENT_APPS` | `off`; accepts `on` |
| `CODEX_SUBAGENT_COMPLETION_DIR` | Private event directory described above |
| `ANTIGRAVITY_AGENTAPI_EXE` | Sidecar-only host executable override |

Supported effort vocabulary: `none`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max`, `ultra`. Actual model support comes from the live catalog. Invalid environment settings remain visible through `codex_doctor`; they refuse new delegation rather than silently relaxing policy.

## Completion and recovery

Every background job saves `<job_id>.job.json` in the existing completion directory, with or without `reply_to`. This is the single persisted report record; it contains the job/thread mapping, state, timestamps, model/effort, callback destination and final report. The record is written before returning the job ID, updated when Codex reports its thread ID or cancellation is requested, and finalized after process cleanup. Writes use atomic replacement.

After an MCP restart, `codex_job_status` can list these jobs or inspect one, and `codex_job_result` returns their saved results. An unfinished record whose owning MCP process has exited is reported as `interrupted`, with its thread ID and an explanation that no final result was saved. It is not automatically resumed. Jobs still owned by another live MCP can be inspected but must be cancelled through that MCP.

Callback jobs additionally track notification delivery with:

- `<job_id>.pending.json`: an event ready for delivery.
- `<job_id>.sent.json`: the renamed event after Antigravity accepted the notification.

The MCP saves the final job record before publishing the pending event. Events contain notification metadata, not a duplicate report; the recipient reads the report through `codex_job_result`. The Sidecar subscribes before its startup scan, so it can recover pending events without missing new completions. Failed sends stay pending; restart the Sidecar to retry. A crash after sending but before renaming can produce a duplicate; the notification carries a stable job ID for deduplication. A `sent` event proves acceptance by the host, not that the recipient has processed it.

Saved job records survive restart and in-memory eviction; they are retained until removed from the directory. The in-memory cache still keeps finished jobs for at most one hour and 100 records. Up to eight background executions run concurrently; a cancelled task retains its slot until cleanup finishes. Persistence restores queries, not running processes. Do not reload or replace a server with running or stopping jobs.

If the initial record cannot be saved, the job does not start. Later write failures are logged and shown in job status/results; the previous atomic snapshot remains available. Recent activity is saved at state/thread transitions, not on every progress message.

## Safety and compatibility

- Prompts go through stdin, and executable arguments are passed as an array. No shell command is constructed from the caller's prompt.
- Codex's sandbox protects its shell execution; inherited MCP/plugin tools have their own permissions. They remain disabled unless explicitly allowed.
- Read-only execution does not promise that sensitive files are unreadable. Review delegated output as data, and keep secrets out of a task's environment when that boundary matters.
- Cancellation and timeout send `SIGINT`, then force remaining owned process groups after five seconds. Server shutdown shortens the force deadline to 250 ms. A timeout result is returned after cleanup, so wall time can exceed the execution budget by the cleanup interval.
- Detached descendants that have already escaped ancestry before discovery cannot be guaranteed stopped. Failed process-table access is logged; process-group termination still applies.
- The MCP validates requested model/effort/sandbox limits before launch; Codex enforces the selected sandbox and approvals.
- Follow-ups on the same Codex thread should be sequential. Background concurrency is for independent sessions, not concurrent mutation of one thread.

See [architecture and runtime behavior](docs/ARCHITECTURE.md) for ownership, implementation details and the pre-switch acceptance checklist.

## License

Licensed under the [MIT License](LICENSE).
