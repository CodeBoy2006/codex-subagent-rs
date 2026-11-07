use crate::{
    catalog::{self, Catalogs},
    completion::{self, Event, State},
    config::{Config, RunArgs, directory},
    inheritance,
    process::Control,
    quota,
    runner::{self, Invocation, Progress, RunResult, Usage},
    session::{self, Settings},
};
use anyhow::{Result, anyhow, bail, ensure};
use nix::{errno::Errno, sys::signal::kill, unistd::Pid};
use rmcp::{ErrorData, RoleServer, ServerHandler, model::*, service::RequestContext};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::{Semaphore, mpsc, oneshot};
use tokio_util::{sync::CancellationToken, task::TaskTracker};
use uuid::Uuid;

#[derive(Clone)]
pub struct App(Arc<Inner>);
struct Inner {
    config: Config,
    catalogs: Catalogs,
    tools: Vec<Tool>,
    jobs: Mutex<HashMap<String, Job>>,
    threads: Mutex<VecDeque<(String, Settings, Option<Usage>)>>,
    slots: Arc<Semaphore>,
    pub shutdown: CancellationToken,
    tasks: TaskTracker,
}
#[derive(Serialize, Deserialize)]
struct Job {
    job_id: String,
    owner_pid: u32,
    reply_to: Option<String>,
    state: State,
    started: i64,
    finished: Option<i64>,
    #[serde(skip)]
    finished_clock: Option<Instant>,
    model: String,
    effort: String,
    #[serde(rename = "thread_id")]
    thread: Option<String>,
    commands: u64,
    activity: VecDeque<String>,
    #[serde(skip)]
    cancel: CancellationToken,
    report: Option<String>,
    error: Option<String>,
    notification_error: Option<String>,
    #[serde(skip)]
    persistence_error: Option<String>,
}
impl Job {
    fn save(&mut self, dir: &Path) -> Result<()> {
        self.persistence_error = None;
        let result = (|| {
            completion::directory(dir)?;
            completion::atomic(
                &dir.join(format!("{}.job.json", self.job_id)),
                &serde_json::to_vec(self)?,
            )
        })();
        if let Err(e) = &result {
            self.persistence_error = Some(format!("{e:#}"));
        }
        result
    }
    fn save_or_log(&mut self, dir: &Path) {
        if let Err(e) = self.save(dir) {
            eprintln!("Could not save job {}: {e:#}", self.job_id);
        }
    }
    fn read(dir: &Path, id: &str) -> Result<Option<Self>> {
        if !completion::uuid(id) {
            return Ok(None);
        }
        let path = dir.join(format!("{id}.job.json"));
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let mut job: Self = serde_json::from_slice(&bytes)
            .with_context(|| format!("Invalid job record {}", path.display()))?;
        ensure!(
            job.job_id == id && job.owner_pid > 0 && job.owner_pid <= i32::MAX as u32,
            "Invalid job record identity"
        );
        // A different live MCP may share the directory; never adopt or cancel its process.
        if job.finished.is_none()
            && kill(Pid::from_raw(job.owner_pid as i32), None) == Err(Errno::ESRCH)
        {
            job.state = State::Interrupted;
            job.error = Some("The owning MCP exited before saving a final result.".into());
            job.report = Some(format!(
                "{}\njob_id: {id}\nthread_id: {}\nThe task was not automatically resumed.",
                job.error.as_deref().unwrap(),
                job.thread.as_deref().unwrap_or("not reported")
            ));
        }
        Ok(Some(job))
    }
    fn summary(&self) -> String {
        format!(
            "{} — {}{} ({} at {})",
            self.job_id,
            self.state,
            if self.state == State::Cancelled && self.finished.is_none() {
                ", stopping"
            } else {
                ""
            },
            self.model,
            self.effort
        )
    }
    fn result(&self) -> Result<Report> {
        ensure!(
            self.finished.is_some() || self.state == State::Interrupted,
            "Job is still running or stopping; result is not ready"
        );
        let mut text = self.report.clone().context("Job produced no result")?;
        if let Some(error) = &self.persistence_error {
            text.push_str(&format!("\nJob persistence failed: {error}"));
        }
        Ok(Report {
            text,
            error: self.state != State::Completed || self.persistence_error.is_some(),
        })
    }
}
fn saved_job_ids(dir: &Path) -> Result<Vec<String>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(e.into()),
    };
    let mut ids = vec![];
    for entry in entries {
        let name = entry?.file_name();
        if let Some(id) = name
            .to_str()
            .and_then(|n| n.strip_suffix(".job.json"))
            .filter(|id| completion::uuid(id))
        {
            ids.push(id.to_owned());
        }
    }
    Ok(ids)
}
fn evict(jobs: &mut HashMap<String, Job>) {
    jobs.retain(|_, j| {
        j.finished_clock
            .is_none_or(|t| t.elapsed() < Duration::from_secs(3600))
    });
    let mut finished: Vec<_> = jobs
        .iter()
        .filter_map(|(id, j)| j.finished_clock.map(|t| (id.clone(), t)))
        .collect();
    finished.sort_by_key(|(_, t)| *t);
    for (id, _) in finished.iter().take(finished.len().saturating_sub(100)) {
        jobs.remove(id);
    }
}
#[derive(Deserialize, Default)]
struct InspectArgs {
    #[serde(default)]
    refresh: bool,
    working_dir: Option<PathBuf>,
}
#[derive(Deserialize, Default)]
struct JobArgs {
    job_id: Option<String>,
    #[serde(default)]
    include_activity: bool,
}
struct Report {
    text: String,
    error: bool,
}
impl Report {
    fn ok(text: String) -> Self {
        Self { text, error: false }
    }
}
impl App {
    pub fn new(config: Config) -> Result<Self> {
        let schemas = [
            include_str!("../schemas/codex_doctor.json"),
            include_str!("../schemas/codex_quota.json"),
            include_str!("../schemas/list_codex_models.json"),
            include_str!("../schemas/codex_delegate.json"),
            include_str!("../schemas/codex_follow_up.json"),
            include_str!("../schemas/codex_job_status.json"),
            include_str!("../schemas/codex_job_result.json"),
            include_str!("../schemas/codex_job_cancel.json"),
        ];
        let tools = schemas
            .into_iter()
            .map(|schema| -> Result<Tool> {
                let v: Value = serde_json::from_str(schema)?;
                let name = v["name"].as_str().unwrap();
                let executes = matches!(name, "codex_delegate" | "codex_follow_up");
                let destructive = name == "codex_job_cancel";
                Ok(Tool::new(
                    name.to_owned(),
                    v["description"].as_str().unwrap().to_owned(),
                    v["parameters"].as_object().unwrap().clone(),
                )
                .with_annotations(ToolAnnotations::from_raw(
                    None,
                    Some(!executes && !destructive),
                    destructive.then_some(true),
                    None,
                    Some(executes || name == "codex_quota"),
                )))
            })
            .collect::<Result<_>>()?;
        Ok(Self(Arc::new(Inner {
            config,
            catalogs: Catalogs::default(),
            tools,
            jobs: Mutex::new(HashMap::new()),
            threads: Mutex::new(VecDeque::new()),
            slots: Arc::new(Semaphore::new(8)),
            shutdown: CancellationToken::new(),
            tasks: TaskTracker::new(),
        })))
    }
    pub async fn shutdown(&self) {
        self.0.shutdown.cancel();
        self.0.tasks.close();
        if tokio::time::timeout(Duration::from_millis(400), self.0.tasks.wait())
            .await
            .is_err()
        {
            eprintln!("Shutdown deadline reached; some task cleanup was not confirmed");
        }
    }
    fn cwd(path: Option<PathBuf>) -> Result<PathBuf> {
        let p = path.unwrap_or(std::env::current_dir()?);
        directory(&p)?;
        Ok(p)
    }
    fn control(&self, cancel: CancellationToken) -> Control {
        Control {
            cancel,
            shutdown: self.0.shutdown.clone(),
        }
    }
    async fn prepare(
        &self,
        mut args: RunArgs,
        followup: bool,
        control: &Control,
    ) -> Result<(Invocation, Vec<String>)> {
        let config = &self.0.config;
        config.validate()?;
        args.validate(followup)?;
        let mut notes = vec![];
        if followup {
            let id = args.thread_id.as_ref().unwrap();
            let remembered = self
                .0
                .threads
                .lock()
                .unwrap()
                .iter()
                .find(|(t, _, _)| t == id)
                .map(|(_, s, _)| s.clone());
            if let Some(p) = remembered {
                let keeps = args.model.as_deref().is_none_or(|m| m == p.model);
                if args.reasoning_effort.is_none() && keeps {
                    args.reasoning_effort = Some(p.effort);
                }
                args.model = args.model.or(Some(p.model));
                args.working_dir = args.working_dir.or(Some(p.cwd));
                args.skip_git_repo_check = p.skip_git_repo_check;
            } else if args.model.is_none()
                || args.working_dir.is_none()
                || args.reasoning_effort.is_none()
            {
                let cwd = Self::cwd(args.working_dir.clone())?;
                match session::read(config, &cwd, id, control).await {
                    Ok(previous) => {
                        let keeps = args
                            .model
                            .as_deref()
                            .is_none_or(|m| previous.model.as_deref() == Some(m));
                        if args.reasoning_effort.is_none() && keeps {
                            args.reasoning_effort = previous.reasoning_effort;
                        }
                        args.model = args.model.or(previous.model);
                        args.working_dir = args.working_dir.or(Some(previous.cwd));
                    }
                    Err(e) => notes.push(format!(
                        "Could not recover thread settings: {e}; using supplied or server defaults"
                    )),
                }
            }
        }
        let cwd = Self::cwd(args.working_dir.clone())?;
        let sandbox = config.sandbox(args.sandbox.as_deref())?;
        let catalog = self.0.catalogs.get(config, &cwd, false, control).await?;
        let chosen = args.model.as_deref().or_else(|| config.implied_model());
        let chosen = chosen.ok_or_else(|| {
            anyhow!("No model specified or configured; pass model or call list_codex_models")
        })?;
        ensure!(
            config.permits_model(chosen),
            "Model {chosen} is outside CODEX_SUBAGENT_ALLOWED_MODELS"
        );
        let model = catalog
            .iter()
            .find(|m| m.slug == chosen)
            .ok_or_else(|| anyhow!("Unknown model {chosen}; call list_codex_models"))?;
        let (effort, note) = catalog::resolve_effort(
            model,
            args.reasoning_effort
                .as_deref()
                .or(config.default_effort.as_deref()),
            config.max_effort.as_deref(),
        )?;
        if let Some(n) = note {
            notes.push(n);
        }
        if args.model.is_none() {
            notes.push(format!("Using configured default model {}", model.slug));
        }
        if args.auto_approve && sandbox == "read-only" {
            notes.push("auto_approve ignored in read-only mode".into());
        }
        let settings = Settings {
            model: model.slug.clone(),
            effort,
            sandbox,
            cwd,
            skip_git_repo_check: args.skip_git_repo_check,
        };
        let inherited = inheritance::inspect(config, &settings.cwd, control).await?;
        let prompt = args.prompt_text();
        Ok((
            Invocation {
                settings,
                args,
                inheritance: inherited,
                prompt,
            },
            notes,
        ))
    }
    fn remember(&self, result: &mut RunResult, inv: &Invocation) {
        if let Some(id) = &result.thread_id {
            let mut threads = self.0.threads.lock().unwrap();
            if result.turn_usage.is_none() {
                result.turn_usage = result
                    .usage
                    .as_ref()
                    .zip(
                        threads
                            .iter()
                            .find(|(t, _, _)| t == id)
                            .and_then(|(_, _, u)| u.as_ref()),
                    )
                    .and_then(|(u, p)| u.subtract(p));
            }
            threads.retain(|(t, _, _)| t != id);
            if inv.args.use_worktree && result.working_dir.is_none() {
                return;
            }
            let mut settings = inv.settings.clone();
            if inv.args.use_worktree
                && let Some(cwd) = result.working_dir.clone()
            {
                settings.cwd = cwd;
            }
            threads.push_back((id.clone(), settings, result.usage.clone()));
            if threads.len() > 500 {
                threads.pop_front();
            }
        }
    }
    async fn execute(
        &self,
        inv: Invocation,
        notes: Vec<String>,
        context: &RequestContext<RoleServer>,
    ) -> Result<Report> {
        if inv.args.mode.as_deref() == Some("background") {
            let permit = self.0.slots.clone().try_acquire_owned().map_err(|_| {
                anyhow!("Too many background delegations (limit 8); wait or cancel a job")
            })?;
            let id = Uuid::new_v4().to_string();
            let cancel = CancellationToken::new();
            {
                let mut jobs = self.0.jobs.lock().unwrap();
                evict(&mut jobs);
                let mut job = Job {
                    job_id: id.clone(),
                    owner_pid: std::process::id(),
                    reply_to: inv.args.reply_to.clone(),
                    state: State::Running,
                    started: chrono::Utc::now().timestamp_millis(),
                    finished: None,
                    finished_clock: None,
                    model: inv.settings.model.clone(),
                    effort: inv.settings.effort.clone(),
                    thread: None,
                    commands: 0,
                    activity: VecDeque::new(),
                    cancel: cancel.clone(),
                    report: None,
                    error: None,
                    notification_error: None,
                    persistence_error: None,
                };
                job.save(&self.0.config.events)
                    .context("Could not persist new job; nothing was started")?;
                jobs.insert(id.clone(), job);
            }
            let app = self.clone();
            let job_id = id.clone();
            let callback = inv.args.reply_to.clone();
            let has_callback = callback.is_some();
            self.0.tasks.spawn(async move {
                let _permit = permit;
                let app_progress = app.clone();
                let progress_id = job_id.clone();
                let progress: Progress = Arc::new(move |event, message| {
                    let mut jobs = app_progress.0.jobs.lock().unwrap();
                    if let Some(job) = jobs.get_mut(&progress_id) {
                        let new_thread = event["type"] == "thread.started"
                            && event["thread_id"].as_str() != job.thread.as_deref();
                        if new_thread {
                            job.thread = event["thread_id"].as_str().map(str::to_owned);
                        }
                        if event["type"] == "item.completed"
                            && event["item"]["type"] == "command_execution"
                        {
                            job.commands += 1;
                        }
                        job.activity.push_back(message.into());
                        if job.activity.len() > 30 {
                            job.activity.pop_front();
                        }
                        if new_thread {
                            job.save_or_log(&app_progress.0.config.events);
                        }
                    }
                });
                let mut result =
                    runner::run(&app.0.config, &inv, &app.control(cancel.clone()), progress).await;
                app.remember(&mut result, &inv);
                let finished = chrono::Utc::now().timestamp_millis();
                // Cancellation and terminal publication share the registry lock: the first wins.
                let (state, saved) = {
                    let mut jobs = app.0.jobs.lock().unwrap();
                    result.cancelled |= cancel.is_cancelled();
                    let state = if result.cancelled {
                        State::Cancelled
                    } else if result.failure().is_some() {
                        State::Failed
                    } else {
                        State::Completed
                    };
                    let report = result.render(&inv, &notes);
                    let mut saved = false;
                    if let Some(job) = jobs.get_mut(&job_id) {
                        job.state = state;
                        job.finished = Some(finished);
                        job.finished_clock = Some(Instant::now());
                        job.error = result.failure();
                        job.thread = result.thread_id;
                        job.report = Some(report);
                        saved = job.save(&app.0.config.events).is_ok();
                        if let Some(e) = &job.persistence_error {
                            eprintln!("Could not save job {}: {e}", job.job_id);
                        }
                    }
                    (state, saved)
                };
                if saved && let Some(conversation_id) = callback {
                    let event = Event {
                        job_id: job_id.clone(),
                        conversation_id,
                        state,
                        finished_at: finished,
                    };
                    if let Err(error) = completion::publish(&app.0.config.events, &event) {
                        eprintln!("Completion event could not be saved for {job_id}: {error:#}");
                        if let Some(job) = app.0.jobs.lock().unwrap().get_mut(&job_id) {
                            job.notification_error = Some(format!("{error:#}"));
                            job.save_or_log(&app.0.config.events);
                        }
                    }
                }
            });
            return Ok(Report::ok(format!(
                "Started background delegation {id}. {}",
                if has_callback {
                    "Completion delivery is registered for the originating Antigravity conversation. Continue independent work; otherwise end this turn and wait for the completion message. Do not set timers or poll codex_job_status."
                } else {
                    "Poll codex_job_status, then read codex_job_result."
                }
            )));
        }
        let (tx, rx) = oneshot::channel();
        let (progress_tx, mut progress_rx) = mpsc::channel(32);
        let app = self.clone();
        let control = self.control(context.ct.clone());
        self.0.tasks.spawn(async move {
            let progress: Progress = Arc::new(move |_, message| {
                let _ = progress_tx.try_send(message.to_owned());
            });
            let mut result = runner::run(&app.0.config, &inv, &control, progress).await;
            app.remember(&mut result, &inv);
            let report = Report {
                error: result.failure().is_some(),
                text: result.render(&inv, &notes),
            };
            let _ = tx.send(report);
        });
        let mut rx = rx;
        let token = context.meta.get_progress_token();
        let mut count = 0f64;
        let mut progress_closed = false;
        loop {
            tokio::select! {
                result=&mut rx=>return result.map_err(|_|anyhow!("Execution task stopped without a result")),
                message=progress_rx.recv(),if !progress_closed=>match message {
                    Some(message)=>if let Some(token)=&token {count+=1.;let mut params=ProgressNotificationParam::new(token.clone(),count);params.message=Some(message);let _=context.peer.notify_progress(params).await;},
                    None=>progress_closed=true,
                }
            }
        }
    }
    fn job(&self, name: &str, args: JobArgs) -> Result<Report> {
        let dir = &self.0.config.events;
        let mut jobs = self.0.jobs.lock().unwrap();
        evict(&mut jobs);
        let Some(id) = args.job_id else {
            ensure!(name == "codex_job_status", "job_id is required");
            let mut summaries: BTreeMap<_, _> = jobs
                .iter()
                .map(|(id, job)| (id.clone(), job.summary()))
                .collect();
            for id in saved_job_ids(dir)? {
                if !jobs.contains_key(&id)
                    && let Some(job) = Job::read(dir, &id)?
                {
                    summaries.insert(id, job.summary());
                }
            }
            return Ok(Report::ok(if summaries.is_empty() {
                "No background delegations.".into()
            } else {
                summaries.into_values().collect::<Vec<_>>().join("\n")
            }));
        };
        let owned = jobs.contains_key(&id);
        let mut saved;
        let job = if let Some(job) = jobs.get_mut(&id) {
            job
        } else {
            saved = Job::read(dir, &id)?.ok_or_else(|| anyhow!("Unknown job id {id}"))?;
            &mut saved
        };
        if name == "codex_job_cancel" {
            if job.state == State::Running {
                ensure!(
                    owned,
                    "Job is running in another MCP process; cancel it there"
                );
                job.cancel.cancel();
                job.state = State::Cancelled;
                job.save(dir)
                    .context("Cancellation requested, but its state could not be saved")?;
            }
            return Ok(Report::ok(format!("Job {id} is now {}.", job.state)));
        }
        if name == "codex_job_result" {
            return job.result();
        }
        let mut lines = vec![
            format!("job_id: {id}"),
            format!(
                "state: {}{}",
                job.state,
                if job.state == State::Cancelled && job.finished.is_none() {
                    " (Codex is still stopping)"
                } else {
                    ""
                }
            ),
            format!("model: {} at {}", job.model, job.effort),
            format!("started: {}", job.started),
            if job.state == State::Interrupted {
                "duration: unknown (owner exited)".into()
            } else {
                format!(
                    "duration: {}ms",
                    job.finished
                        .unwrap_or_else(|| chrono::Utc::now().timestamp_millis())
                        - job.started
                )
            },
            format!("commands run: {}", job.commands),
            format!(
                "thread_id: {}",
                job.thread.as_deref().unwrap_or("not yet reported")
            ),
            format!(
                "last activity: {}",
                job.activity
                    .back()
                    .map(String::as_str)
                    .unwrap_or("No activity reported yet")
            ),
        ];
        if args.include_activity {
            lines.extend(job.activity.iter().cloned());
        }
        if let Some(e) = &job.error {
            lines.push(format!("error: {e}"));
        }
        if let Some(e) = &job.persistence_error {
            lines.push(format!("Job persistence failed: {e}"));
        }
        if let Some(e) = &job.notification_error {
            lines.push(format!("Completion delivery failed: {e}"));
        }
        Ok(Report::ok(lines.join("\n")))
    }
    async fn dispatch(
        &self,
        name: &str,
        args: Value,
        context: &RequestContext<RoleServer>,
    ) -> Result<Report> {
        let config = &self.0.config;
        let control = self.control(context.ct.clone());
        match name {
            "codex_doctor" => {
                let args: InspectArgs = serde_json::from_value(args)?;
                let cwd = Self::cwd(args.working_dir)?;
                let d = self
                    .0
                    .catalogs
                    .doctor(config, &cwd, args.refresh, &control)
                    .await;
                Ok(Report {
                    error: !d.usable() || !config.errors.is_empty(),
                    text: format!(
                        "{}\nchecked in: {}\ncodex binary: {}\n{}",
                        config.errors.join("\n"),
                        cwd.display(),
                        config.codex.display(),
                        d.render()
                    ),
                })
            }
            "codex_quota" => {
                let args: InspectArgs = serde_json::from_value(args)?;
                let cwd = Self::cwd(args.working_dir)?;
                Ok(Report::ok(quota::read(config, &cwd, &control).await?))
            }
            "list_codex_models" => {
                config.validate()?;
                let args: InspectArgs = serde_json::from_value(args)?;
                let cwd = Self::cwd(args.working_dir)?;
                let catalog = self
                    .0
                    .catalogs
                    .get(config, &cwd, args.refresh, &control)
                    .await?;
                let mut lines = vec![];
                for m in catalog {
                    lines.push(format!("{} — {}{}\n  {}\n  reasoning efforts: {} (default: {})\n  model_context_window: {} tokens\n  images: {} | web search: {}",m.slug,m.display_name,if config.permits_model(&m.slug){""}else{" [BLOCKED]"},m.description,m.efforts.join(", "),m.default_effort,m.context_window.map(|n|n.to_string()).unwrap_or("unknown".into()),m.images,m.search));
                }
                Ok(Report::ok(lines.join("\n\n")))
            }
            "codex_delegate" | "codex_follow_up" => {
                let args: RunArgs = serde_json::from_value(args)?;
                let (inv, notes) = self
                    .prepare(args, name == "codex_follow_up", &control)
                    .await?;
                self.execute(inv, notes, context).await
            }
            "codex_job_status" | "codex_job_result" | "codex_job_cancel" => {
                self.job(name, serde_json::from_value(args)?)
            }
            _ => bail!("Unknown tool: {name}"),
        }
    }
}
use anyhow::Context as _;
impl ServerHandler for App {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build()).with_server_info(
            Implementation::new("codex-subagent-rs", env!("CARGO_PKG_VERSION")),
        )
    }
    fn get_tool(&self, name: &str) -> Option<Tool> {
        self.0.tools.iter().find(|t| t.name == name).cloned()
    }
    async fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> std::result::Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(self.0.tools.clone()))
    }
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> std::result::Result<CallToolResponse, ErrorData> {
        let mut arguments = request.arguments.unwrap_or_default();
        if let Some(tool) = self.get_tool(&request.name) {
            let properties = tool.input_schema["properties"].as_object().unwrap();
            // Strip unknown keys per tool so one tool cannot smuggle another's options.
            arguments.retain(|key, _| properties.contains_key(key));
        }
        let result = if arguments.values().any(Value::is_null) {
            Err(anyhow!("Optional parameters must be omitted, not null"))
        } else {
            self.dispatch(&request.name, Value::Object(arguments), &context)
                .await
        };
        let result = match result {
            Ok(r) if !r.error => CallToolResult::success(vec![ContentBlock::text(r.text)]),
            Ok(r) => CallToolResult::error(vec![ContentBlock::text(r.text)]),
            Err(e) => CallToolResult::error(vec![ContentBlock::text(format!("{e:#}"))]),
        };
        Ok(result.into())
    }
}
