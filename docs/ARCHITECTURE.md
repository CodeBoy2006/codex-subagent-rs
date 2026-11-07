# Architecture and runtime behavior

## Keep Codex as the engine

The MCP-facing API exposes eight tools through a native Rust server. Execution uses `codex exec --json` or `codex exec resume <thread_id> --json`. Authentication, agent tools, sandboxing and conversation history remain Codex responsibilities. Read-only app-server exchanges read thread metadata and account usage quotas; they do not start model turns.

One binary has three independent entrypoints. Hook invocations are short-lived. The MCP owns running tasks. The Sidecar owns notification delivery and remains independent of the MCP so saved reports can be delivered after an MCP exit. There is no HTTP listener, broker or database.

## Ownership and state

- **`service`** registers the eight tools, validates tool-specific inputs, resolves policy and retains bounded job/thread registries. Each background job is also stored in `<job_id>.job.json` in the completion directory. Blocking and background calls both use `runner`. A task tracker owns executing futures independently of the client's request future. Cancellation, job-file updates and terminal publication share the registry lock, so cancellation cannot race a completed report back into success.
- **`process`** owns each subprocess, stdin writer and stdout/stderr readers. Every process starts in its own POSIX group. On cancellation it snapshots descendant groups while ancestry still exists, sends `SIGINT`, and escalates to `SIGKILL`. A global shutdown token shortens that same lifecycle rather than building a separate kill path. The background semaphore is held through cleanup and report publication.
- **`runner`** constructs argv, parses bounded JSONL, accumulates observable results and performs capacity continuation under one absolute deadline. Exit status, turn failure, cancellation, timeout, missing answers, invalid structured output use one result predicate.
- **`catalog` / `inheritance` / `config`** handle live capabilities and explicit policy. Catalog/diagnosis caches are bounded and keyed by working directory. Doctor probes are on demand; catalog errors are returned without a static fallback. Inheritance is inspected on every run; it is not cached. Failed restricted MCP inventory refuses the run. Restricted plugin-inventory failure disables plugins and is reported.
- **`app_server` / `quota`** reuse one bounded initialize → initialized → read RPC exchange. Quota queries are uncached and use Codex-managed authentication; the MCP never reads credentials. Multi-bucket quota data takes precedence over the legacy single-bucket view. Remaining percentages are derived from used percentages; missing windows stay unknown. Backend permission/limit states are retained separately from percentages.
- **`session`** reads saved model/effort/cwd through Codex’s metadata-only `thread/read` API. It is used on an in-memory registry miss and to locate a managed worktree. Codex owns session discovery and storage formats. Permission enforcement remains in Codex; the wrapper validates its explicit ceilings and passes the corresponding CLI options.
- **`completion`** preserves the Hook input/output shape, atomically publishes `<job_id>.pending.json` events and watches the event directory. The Sidecar sends each event's notification through `agentapi`, then renames it to `<job_id>.sent.json`. Events track callback delivery state and contain no report. The Sidecar keeps an in-flight set, so repeated filesystem notifications do not send the same pending event concurrently.

`job_id`, Codex `thread_id`, and Antigravity `conversationId` are separate identifiers. A job may contain several CLI attempts, but capacity continuation always resumes one Codex thread. `<job_id>.job.json` is the single persisted report store, holding the mapping, state and final report independently of notification delivery. Runtime cancellation handles and monotonic cache timestamps are excluded from serialization.

The initial record must commit before the task is spawned. Thread assignment, cancellation and completion update the same file using the existing atomic-write helper; the final save precedes callback event publication. Status queries merge live jobs with saved records; both status and result queries use the job record as their only disk lookup path. Saved records are read on demand, rather than rehydrating execution handles or scheduling work at startup. An unfinished record is presented as interrupted when its owner PID no longer exists; a live foreign owner is never adopted or signalled. This is local process ownership, not a distributed scheduler. Activity is checkpointed with those transitions. Disk records are not subject to in-memory cache eviction.

## Native subagents use the current global policy

The wrapper enables `agents.enabled` and `features.multi_agent`, and clears `features.multi_agent_v2.disable_native_tools` for each invocation. It leaves the backend choice, concurrency/depth limits, custom roles, and child model/effort defaults unchanged. No extra child policy is copied from the MCP's parent-task allow-list or effort ceiling.

The shared native-agent prompt contains one instruction to follow the global `AGENTS.md` for subagent orchestration. New tasks, follow-ups and capacity continuation reuse it; Codex handles instruction loading. Project instructions continue to govern repository work. The inherited-MCP self-reference guard remains in force.

`collab_tool_call` start/update/completion events are forwarded as progress and background activity. Failed collaboration calls are reported as notices, not automatically treated as a failed parent task, since the parent can recover. The parent thread ID, final message and usage remain separate from child state. Codex owns native thread shutdown; the wrapper's existing process cleanup covers their local command descendants.

The real-CLI configuration probe verifies that enabling native tools preserves deliberately different child models/efforts and both V1/V2 concurrency settings. Isolated stdio tests cover delegation progress, resumption, capacity continuation, cancellation and process-tree cleanup; they do not invoke a live model.

Source contract: [Codex 0.165.0 agent configuration](https://github.com/openai/codex/blob/rust-v0.165.0/codex-rs/config/src/config_toml.rs), [child configuration precedence](https://github.com/openai/codex/blob/rust-v0.165.0/codex-rs/core/src/agent/child_config.rs), and [exec collaboration events](https://github.com/openai/codex/blob/rust-v0.165.0/codex-rs/exec/src/exec_events.rs).

## Reuse native execution mechanisms

The `prompt` parameter carries the complete task, including context, relevant paths and completion criteria. The wrapper prepends only the global-subagent instruction to the trimmed prompt, without separate prompt blocks or redundant input fields. Codex supplies its own operating instructions, sandbox guidance and `--output-schema` handling. Tool descriptions come directly from the schema files.

`codex exec resume` resolves model, effort and cwd from its invocation config rather than automatically reusing the saved values. The wrapper therefore keeps the existing follow-up contract by reading missing settings from `thread/read`; this endpoint also provides the worktree directory for capacity continuation. Its metadata describes saved/current configuration, not execution telemetry. Reports label CLI settings as requested rather than reconstructing an applied-state audit from private rollouts.

Source: [Codex resume parameter construction](https://github.com/openai/codex/blob/rust-v0.165.0/codex-rs/exec/src/lib.rs#L1469-L1495) and [native thread metadata](https://github.com/openai/codex/blob/rust-v0.165.0/codex-rs/app-server-protocol/src/protocol/v2/thread_data.rs#L242-L264).

## Task token accounting

Codex 0.165.0's exec processor maps `turn.completed.usage` from the primary thread's cumulative total. The wrapper keeps that snapshot and subtracts the prior in-memory snapshot on follow-up; a new task uses its cumulative total as this-call usage. Capacity continuation stays on the same thread, so snapshots are not added and double-counted. Unknown or decreasing counters cannot produce a per-call delta. After restart, a follow-up can still report the CLI total while its per-call delta is unknown.

All result paths use the same rendering, including saved callback reports. The output includes total input + output and the reported cache/reasoning breakdowns. Native child threads are excluded from this accounting because exec filters token notifications to its primary thread. Failed/cancelled runs may have no usage event; those report unknown, not zero. Codex itself may emit zero counters when it has no matching update; these remain reported values rather than an independently verified billing record.

Source: [exec cumulative usage mapping](https://github.com/openai/codex/blob/rust-v0.165.0/codex-rs/exec/src/event_processor_with_jsonl_output.rs#L118-L128), [primary-thread usage filter](https://github.com/openai/codex/blob/rust-v0.165.0/codex-rs/exec/src/lib.rs#L1718-L1720), and [official account quota API](https://learn.chatgpt.com/docs/app-server#6-rate-limits-chatgpt). Quota fields and the request's `excludeResetCreditDetails` flag were checked against the schema generated by the installed CLI 0.165.0.

## Runtime behavior and verification

| Behavior | Implementation | Acceptance evidence |
| --- | --- | --- |
| Context display | `list_codex_models` uses each parsed catalog model's `context_window` directly; missing values remain unknown. No configuration RPC is needed. | Stdio integration checks distinct catalog windows, catalog cwd propagation and no app-server invocation during listing. |
| Inventory timeout | MCP/plugin listing uses a 60-second process budget, including the self-reference inventory. | Shared process timeout path is exercised by the runner tests; the inventory call site's 60-second value is inspected, without a one-minute sleep test. |
| Capacity resume | Recognized Codex capacity failure, three-second wait, same thread and policy, remaining original budget. Worktrees resume in their existing observed directory. | Stdio test checks continuation argv, output schema and final result; timeout test proves the retry wait does not reset the deadline. |
| Plugin inventory | Default `none` skips plugin listing while still listing MCP servers and disabling inherited tools. | Fake marketplace fails if queried; delegation succeeds without querying it. Policy tests check allow-list overrides and fail-closed behavior. |
| Completion events | Atomic job-record save before event publication, original-chat Hook binding, filesystem Sidecar, report lookup from job records only. | Tests cover no duplicate report files, result recovery without callback events, two origins, failure/cancellation, restart recovery, failed-send replay, no replay of sent events, and the executable Hook. |

## Implementation choices

- One Rust executable provides the MCP, Hook and Sidecar entrypoints.
- The event directory is project-specific and can be overridden with an absolute path.
- Results use MCP text content and `isError`, with delimiters around structured output. Schema files define accepted tool inputs; job IDs and callback event shapes remain stable. Doctor reports the installed CLI version and sign-in status.
- A background cancellation keeps its concurrency permit until the process tree is cleaned up. Cancellation/timeout return partial output after cleanup rather than publishing a terminal report before forced termination.
- Only Unix platforms are implemented. Windows process trees require a separate platform implementation, not a fallback that kills only the CLI leader.

## Checks before switching a live client

The isolated suite exercises actual stdio framing, spawned processes and filesystem notifications against a fake CLI and sender. It does not certify the current provider's live model behavior or the host's actual message delivery.

When a switch is authorized:

1. Wait for the current MCP's running and stopping jobs to finish; retain its configuration and saved reports.
2. Run a bounded read-only task through the Rust server with the intended Codex binary, then a follow-up on that thread. Ask for one native child under the current global AGENTS.md policy; confirm its completion, the requested settings and the final parent result.
3. Exercise one background completion through Antigravity's real Hook and Sidecar; confirm the report reaches the original conversation once and can be fetched.
4. Change only the intended client's launch entry and associated Hook/Sidecar commands. If multiple clients share launch configuration, avoid changes that affect other clients.

No step in this checklist is performed by building, testing or running `--help`.
