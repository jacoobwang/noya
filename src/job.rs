//! Durable background Jobs and bounded Worker scheduling.

use crate::{
    Agent, AgentConfig, AgentDiagnostics, AgentEvent, ApprovalDecision, ApprovalPrompt,
    ApprovalRequest,
    LlmClient,
    model::RuntimeModelConfig,
    session::{CreateSession, SessionId, SessionManager, SessionSnapshot},
};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{HashMap, VecDeque},
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::PathBuf,
    sync::Arc,
};
use time::OffsetDateTime;
use tokio::sync::{Mutex, Notify, broadcast, mpsc, oneshot};
use uuid::Uuid;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

const JOB_SCHEMA_VERSION: u32 = 1;
const DEFAULT_MAX_WORKERS: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Queued,
    Running,
    WaitingApproval,
    Cancelling,
    Completed,
    Failed,
    Cancelled,
    Interrupted,
}

impl JobStatus {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Cancelled | Self::Interrupted
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobApproval {
    pub request_id: String,
    pub call_id: String,
    pub tool_name: String,
    pub arguments: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobSummary {
    pub schema_version: u32,
    pub job_id: Uuid,
    pub source_session_id: SessionId,
    pub session_id: SessionId,
    pub workspace: PathBuf,
    pub prompt: String,
    pub status: JobStatus,
    pub retry_of: Option<Uuid>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
    #[serde(default)]
    pub completed_at: Option<OffsetDateTime>,
    #[serde(default)]
    pub output: String,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub approval: Option<JobApproval>,
    #[serde(default)]
    pub diagnostics: Option<AgentDiagnostics>,
    #[serde(default)]
    pub last_sequence: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum JobEvent {
    StateChanged {
        from: Option<JobStatus>,
        to: JobStatus,
        reason: Option<String>,
    },
    Agent(AgentEvent),
    ApprovalRequired(JobApproval),
    Completed {
        output: String,
        diagnostics: AgentDiagnostics,
    },
    Failed {
        message: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobEventRecord {
    pub schema_version: u32,
    pub sequence: u64,
    #[serde(with = "time::serde::rfc3339")]
    pub timestamp: OffsetDateTime,
    pub event: JobEvent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobSnapshot {
    pub summary: JobSummary,
    pub session: Option<SessionSnapshot>,
    pub events: Vec<JobEventRecord>,
}

#[derive(Clone)]
pub struct JobManagerConfig {
    pub data_root: PathBuf,
    pub default_source_session_id: SessionId,
    pub model: RuntimeModelConfig,
    pub agent_config: AgentConfig,
    pub max_workers: usize,
}

#[derive(Clone)]
struct JobStore {
    root: PathBuf,
}

impl JobStore {
    fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn jobs_root(&self) -> PathBuf {
        self.root.join("jobs")
    }

    fn job_dir(&self, job_id: Uuid) -> PathBuf {
        self.jobs_root().join(job_id.to_string())
    }

    fn meta_path(&self, job_id: Uuid) -> PathBuf {
        self.job_dir(job_id).join("meta.json")
    }

    fn events_path(&self, job_id: Uuid) -> PathBuf {
        self.job_dir(job_id).join("events.jsonl")
    }

    fn ensure_root(&self) -> Result<()> {
        fs::create_dir_all(self.jobs_root())?;
        #[cfg(unix)]
        fs::set_permissions(self.jobs_root(), fs::Permissions::from_mode(0o700))?;
        Ok(())
    }

    fn create(&self, summary: &JobSummary) -> Result<()> {
        self.ensure_root()?;
        let directory = self.job_dir(summary.job_id);
        fs::create_dir_all(&directory)
            .with_context(|| format!("create Job directory {}", directory.display()))?;
        #[cfg(unix)]
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
        self.write_meta(summary)?;
        let events = File::create(self.events_path(summary.job_id))?;
        #[cfg(unix)]
        events.set_permissions(fs::Permissions::from_mode(0o600))?;
        Ok(())
    }

    fn write_meta(&self, summary: &JobSummary) -> Result<()> {
        let path = self.meta_path(summary.job_id);
        let temporary = path.with_extension("json.tmp");
        let encoded = serde_json::to_vec_pretty(summary)?;
        fs::write(&temporary, encoded)?;
        #[cfg(unix)]
        fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))?;
        fs::rename(temporary, path)?;
        Ok(())
    }

    fn append(&self, summary: &mut JobSummary, event: JobEvent) -> Result<JobEventRecord> {
        let sequence = summary.last_sequence.saturating_add(1);
        let record = JobEventRecord {
            schema_version: JOB_SCHEMA_VERSION,
            sequence,
            timestamp: OffsetDateTime::now_utc(),
            event,
        };
        let encoded = serde_json::to_string(&record)?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.events_path(summary.job_id))?;
        file.write_all(encoded.as_bytes())?;
        file.write_all(b"\n")?;
        file.sync_data()?;
        summary.last_sequence = sequence;
        summary.updated_at = record.timestamp;
        self.write_meta(summary)?;
        Ok(record)
    }

    fn load_summaries(&self) -> Result<Vec<JobSummary>> {
        self.ensure_root()?;
        let mut summaries = Vec::new();
        for entry in fs::read_dir(self.jobs_root())? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let path = entry.path().join("meta.json");
            let bytes = fs::read(&path)
                .with_context(|| format!("read Job metadata {}", path.display()))?;
            let summary: JobSummary = serde_json::from_slice(&bytes)
                .with_context(|| format!("decode Job metadata {}", path.display()))?;
            ensure!(
                summary.schema_version == JOB_SCHEMA_VERSION,
                "unsupported Job schema version {}",
                summary.schema_version
            );
            summaries.push(summary);
        }
        summaries.sort_by(|left, right| left.created_at.cmp(&right.created_at));
        Ok(summaries)
    }

    fn load_events(&self, job_id: Uuid, after: u64) -> Result<Vec<JobEventRecord>> {
        let path = self.events_path(job_id);
        if !path.exists() {
            return Ok(Vec::new());
        }
        let file = File::open(&path)?;
        let mut events = Vec::new();
        for line in BufReader::new(file).lines() {
            let line = line?;
            let record: JobEventRecord = serde_json::from_str(&line)?;
            if record.sequence > after {
                events.push(record);
            }
        }
        Ok(events)
    }
}

#[derive(Clone)]
pub struct JobManager {
    inner: Arc<JobManagerInner>,
}

struct JobManagerInner {
    config: JobManagerConfig,
    store: JobStore,
    sessions: SessionManager,
    state: Mutex<ManagerState>,
    notify: Notify,
    events: broadcast::Sender<JobEventNotification>,
}

struct ManagerState {
    jobs: HashMap<Uuid, JobSummary>,
    queue: VecDeque<Uuid>,
    workers: HashMap<SessionId, WorkerHandle>,
    approvals: HashMap<Uuid, oneshot::Sender<ApprovalDecision>>,
}

struct WorkerHandle {
    command_tx: mpsc::UnboundedSender<WorkerCommand>,
    busy: bool,
}

enum WorkerCommand {
    Run { job_id: Uuid, prompt: String },
    Cancel,
    Shutdown,
}

enum WorkerMessage {
    AgentEvent { job_id: Uuid, event: AgentEvent },
    Approval {
        job_id: Uuid,
        request: ApprovalRequest,
        respond: oneshot::Sender<ApprovalDecision>,
    },
    Finished {
        job_id: Uuid,
        result: Result<AgentDiagnostics, String>,
        output: String,
        cancelled: bool,
    },
}

#[derive(Debug, Clone)]
pub struct JobEventNotification {
    pub job_id: Uuid,
    pub record: JobEventRecord,
}

impl JobManager {
    fn max_workers(&self) -> usize {
        if self.inner.config.max_workers == 0 {
            DEFAULT_MAX_WORKERS
        } else {
            self.inner.config.max_workers
        }
    }

    pub fn new(config: JobManagerConfig, sessions: SessionManager) -> Result<Self> {
        let store = JobStore::new(config.data_root.clone());
        let mut jobs = HashMap::new();
        let mut queue = VecDeque::new();
        for mut summary in store.load_summaries()? {
            if matches!(
                summary.status,
                JobStatus::Running | JobStatus::WaitingApproval | JobStatus::Cancelling
            ) {
                let previous = summary.status;
                summary.status = JobStatus::Interrupted;
                summary.approval = None;
                summary.completed_at = Some(OffsetDateTime::now_utc());
                store.append(
                    &mut summary,
                    JobEvent::StateChanged {
                        from: Some(previous),
                        to: JobStatus::Interrupted,
                        reason: Some("daemon restarted before Job completed".to_string()),
                    },
                )?;
            }
            if summary.status == JobStatus::Queued {
                queue.push_back(summary.job_id);
            }
            jobs.insert(summary.job_id, summary);
        }
        let (events, _) = broadcast::channel(512);
        Ok(Self {
            inner: Arc::new(JobManagerInner {
                config,
                store,
                sessions,
                state: Mutex::new(ManagerState {
                    jobs,
                    queue,
                    workers: HashMap::new(),
                    approvals: HashMap::new(),
                }),
                notify: Notify::new(),
                events,
            }),
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<JobEventNotification> {
        self.inner.events.subscribe()
    }

    pub async fn start(&self) {
        let manager = self.clone();
        tokio::spawn(async move {
            manager.scheduler_loop().await;
        });
    }

    async fn scheduler_loop(&self) {
        loop {
            if let Err(error) = self.dispatch_queued().await {
                tracing::error!(%error, "Job scheduler dispatch failed");
            }
            self.inner.notify.notified().await;
        }
    }

    async fn dispatch_queued(&self) -> Result<()> {
        loop {
            let candidate = {
                let mut state = self.inner.state.lock().await;
                let max_workers = self.max_workers();
                let selected = state.queue.iter().copied().find(|job_id| {
                    state
                        .jobs
                        .get(job_id)
                        .is_some_and(|job| {
                            !state
                                .workers
                                .get(&job.session_id)
                                .is_some_and(|worker| worker.busy)
                        })
                });
                let Some(job_id) = selected else { return Ok(()); };
                let job = state.jobs.get(&job_id).context("queued Job disappeared")?.clone();
                let needs_worker = !state.workers.contains_key(&job.session_id);
                if needs_worker && state.workers.len() >= max_workers {
                    let evict_session = state
                        .workers
                        .iter()
                        .find(|(session_id, worker)| {
                            !worker.busy
                                && !state.queue.iter().any(|queued_id| {
                                    state
                                        .jobs
                                        .get(queued_id)
                                        .is_some_and(|queued| queued.session_id == **session_id)
                                })
                        })
                        .map(|(session_id, _)| *session_id);
                    let Some(evict_session) = evict_session else {
                        return Ok(());
                    };
                    if let Some(worker) = state.workers.remove(&evict_session) {
                        let _ = worker.command_tx.send(WorkerCommand::Shutdown);
                    }
                }
                state.queue.retain(|queued| *queued != job_id);
                (job, needs_worker)
            };

            let (job, needs_worker) = candidate;
            if needs_worker {
                match self.create_worker(job.session_id).await {
                    Ok(worker) => {
                        let command_tx = worker.command_tx.clone();
                        let mut state = self.inner.state.lock().await;
                        state.workers.insert(job.session_id, worker);
                        self.start_job_locked(&mut state, &job)?;
                        command_tx
                            .send(WorkerCommand::Run {
                                job_id: job.job_id,
                                prompt: job.prompt.clone(),
                            })
                            .map_err(|_| anyhow::anyhow!("Job Worker is unavailable"))?;
                    }
                    Err(error) => {
                        let mut state = self.inner.state.lock().await;
                        self.finish_failed_locked(&mut state, job.job_id, error.to_string())?;
                    }
                }
            } else {
                let command_tx = {
                    let mut state = self.inner.state.lock().await;
                    self.start_job_locked(&mut state, &job)?;
                    state
                        .workers
                        .get(&job.session_id)
                        .context("Job Worker disappeared")?
                        .command_tx
                        .clone()
                };
                command_tx
                    .send(WorkerCommand::Run {
                        job_id: job.job_id,
                        prompt: job.prompt.clone(),
                    })
                    .map_err(|_| anyhow::anyhow!("Job Worker is unavailable"))?;
            }
        }
    }

    async fn create_worker(&self, session_id: SessionId) -> Result<WorkerHandle> {
        let session = self.inner.sessions.open(session_id)?;
        let workspace = session.summary().workspace.clone();
        let model = self.inner.config.model.clone();
        let mut agent_config = self.inner.config.agent_config.clone();
        agent_config.workspace = workspace;
        let llm = LlmClient::with_settings(
            reqwest::Client::new(),
            model.base_url,
            model.api_key,
            model.model_id.clone(),
            model.protocol,
            model.authentication,
        )
        .with_custom_temperature(model.model.supports_custom_temperature());
        let agent = Agent::with_session_for_model(agent_config, llm, session, model.model.to_string())?;
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let (message_tx, mut message_rx) = mpsc::unbounded_channel();
        let manager = self.clone();
        tokio::spawn(async move {
            worker_loop(agent, command_rx, message_tx).await;
        });
        tokio::spawn(async move {
            while let Some(message) = message_rx.recv().await {
                manager.handle_worker_message(message).await;
            }
        });
        Ok(WorkerHandle {
            command_tx,
            busy: false,
        })
    }

    fn start_job_locked(&self, state: &mut ManagerState, job: &JobSummary) -> Result<()> {
        let summary = state.jobs.get_mut(&job.job_id).context("Job not found")?;
        let from = summary.status;
        summary.status = JobStatus::Running;
        summary.approval = None;
        let record = self.inner.store.append(
            summary,
            JobEvent::StateChanged {
                from: Some(from),
                to: JobStatus::Running,
                reason: None,
            },
        )?;
        self.publish(job.job_id, record);
        state
            .workers
            .get_mut(&job.session_id)
            .context("Worker not found")?
            .busy = true;
        Ok(())
    }

    async fn handle_worker_message(&self, message: WorkerMessage) {
        let result = match message {
            WorkerMessage::AgentEvent { job_id, event } => self.record_agent_event(job_id, event).await,
            WorkerMessage::Approval {
                job_id,
                request,
                respond,
            } => self.record_approval(job_id, request, respond).await,
            WorkerMessage::Finished {
                job_id,
                result,
                output,
                cancelled,
            } => self.finish_job(job_id, result, output, cancelled).await,
        };
        if let Err(error) = result {
            tracing::error!(%error, "Job event handling failed");
        }
        self.inner.notify.notify_one();
    }

    async fn record_agent_event(&self, job_id: Uuid, event: AgentEvent) -> Result<()> {
        let mut state = self.inner.state.lock().await;
        let summary = state.jobs.get_mut(&job_id).context("Job not found")?;
        let record = self
            .inner
            .store
            .append(summary, JobEvent::Agent(event.clone()))?;
        if let AgentEvent::TextDelta { chunk, .. } = event {
            summary.output.push_str(&chunk);
            self.inner.store.write_meta(summary)?;
        }
        self.publish(job_id, record);
        Ok(())
    }

    async fn record_approval(
        &self,
        job_id: Uuid,
        request: ApprovalRequest,
        respond: oneshot::Sender<ApprovalDecision>,
    ) -> Result<()> {
        let approval = JobApproval {
            request_id: request.request_id,
            call_id: request.call_id,
            tool_name: request.tool_name,
            arguments: request.arguments,
        };
        let mut state = self.inner.state.lock().await;
        state.approvals.insert(job_id, respond);
        let summary = state.jobs.get_mut(&job_id).context("Job not found")?;
        let from = summary.status;
        summary.status = JobStatus::WaitingApproval;
        let state_record = self.inner.store.append(
            summary,
            JobEvent::StateChanged {
                from: Some(from),
                to: JobStatus::WaitingApproval,
                reason: Some(format!("approval required for {}", approval.tool_name)),
            },
        )?;
        self.publish(job_id, state_record);
        summary.approval = Some(approval.clone());
        let record = self.inner.store.append(
            summary,
            JobEvent::ApprovalRequired(approval),
        )?;
        self.publish(job_id, record);
        Ok(())
    }

    async fn finish_job(
        &self,
        job_id: Uuid,
        result: Result<AgentDiagnostics, String>,
        output: String,
        cancelled: bool,
    ) -> Result<()> {
        let mut state = self.inner.state.lock().await;
        let job = state.jobs.get(&job_id).context("Job not found")?;
        let session_id = job.session_id;
        let summary = state.jobs.get_mut(&job_id).context("Job not found")?;
        summary.output = output.clone();
        summary.approval = None;
        summary.completed_at = Some(OffsetDateTime::now_utc());
        let event = match result {
            Ok(diagnostics) if !cancelled => {
                summary.status = JobStatus::Completed;
                summary.diagnostics = Some(diagnostics.clone());
                JobEvent::Completed { output, diagnostics }
            }
            Ok(diagnostics) => {
                summary.status = JobStatus::Cancelled;
                summary.diagnostics = Some(diagnostics);
                JobEvent::StateChanged {
                    from: Some(JobStatus::Cancelling),
                    to: JobStatus::Cancelled,
                    reason: Some("cancelled by user".to_string()),
                }
            }
            Err(message) if cancelled => {
                summary.status = JobStatus::Cancelled;
                summary.error = Some(message.clone());
                JobEvent::StateChanged {
                    from: Some(JobStatus::Cancelling),
                    to: JobStatus::Cancelled,
                    reason: Some(message),
                }
            }
            Err(message) => {
                summary.status = JobStatus::Failed;
                summary.error = Some(message.clone());
                JobEvent::Failed { message }
            }
        };
        let record = self.inner.store.append(summary, event)?;
        self.publish(job_id, record);
        if let Some(worker) = state.workers.get_mut(&session_id) {
            worker.busy = false;
        }
        state.approvals.remove(&job_id);
        Ok(())
    }

    fn finish_failed_locked(&self, state: &mut ManagerState, job_id: Uuid, message: String) -> Result<()> {
        let summary = state.jobs.get_mut(&job_id).context("Job not found")?;
        summary.status = JobStatus::Failed;
        summary.error = Some(message.clone());
        summary.completed_at = Some(OffsetDateTime::now_utc());
        let record = self
            .inner
            .store
            .append(summary, JobEvent::Failed { message })?;
        self.publish(job_id, record);
        Ok(())
    }

    fn publish(&self, job_id: Uuid, record: JobEventRecord) {
        let _ = self.inner.events.send(JobEventNotification { job_id, record });
    }

    pub async fn submit(
        &self,
        source_session_id: Option<SessionId>,
        prompt: String,
        retry_of: Option<Uuid>,
    ) -> Result<JobSummary> {
        ensure!(!prompt.trim().is_empty(), "Job prompt cannot be empty");
        let source_session_id = source_session_id.unwrap_or(self.inner.config.default_source_session_id);
        let source = self.inner.sessions.show(source_session_id)?.summary;
        let session_id = if let Some(retry_of) = retry_of {
            let source_job = self.get_summary(retry_of).await?;
            source_job.session_id
        } else {
            let child = self.inner.sessions.fork(source_session_id, None).or_else(|_| {
                self.inner.sessions.create(CreateSession {
                    workspace: source.workspace.clone(),
                    model: source.model.clone(),
                    model_id: source.model_id.clone(),
                })
            })?;
            let id = *child.id();
            drop(child);
            id
        };
        let now = OffsetDateTime::now_utc();
        let summary = JobSummary {
            schema_version: JOB_SCHEMA_VERSION,
            job_id: Uuid::new_v4(),
            source_session_id,
            session_id,
            workspace: source.workspace,
            prompt,
            status: JobStatus::Queued,
            retry_of,
            created_at: now,
            updated_at: now,
            completed_at: None,
            output: String::new(),
            error: None,
            approval: None,
            diagnostics: None,
            last_sequence: 0,
        };
        self.inner.store.create(&summary)?;
        let mut state = self.inner.state.lock().await;
        let mut summary = summary;
        let record = self.inner.store.append(
            &mut summary,
            JobEvent::StateChanged {
                from: None,
                to: JobStatus::Queued,
                reason: None,
            },
        )?;
        self.publish(summary.job_id, record);
        state.queue.push_back(summary.job_id);
        state.jobs.insert(summary.job_id, summary.clone());
        self.inner.notify.notify_one();
        Ok(summary)
    }

    pub async fn list(&self) -> Vec<JobSummary> {
        let state = self.inner.state.lock().await;
        let mut jobs = state.jobs.values().cloned().collect::<Vec<_>>();
        jobs.sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
        jobs
    }

    pub async fn get_summary(&self, job_id: Uuid) -> Result<JobSummary> {
        self.inner
            .state
            .lock()
            .await
            .jobs
            .get(&job_id)
            .cloned()
            .with_context(|| format!("Job {job_id} was not found"))
    }

    pub async fn snapshot(&self, job_id: Uuid, after: u64) -> Result<JobSnapshot> {
        let summary = self.get_summary(job_id).await?;
        let session = self.inner.sessions.show(summary.session_id).ok();
        let events = self.inner.store.load_events(job_id, after)?;
        Ok(JobSnapshot {
            summary,
            session,
            events,
        })
    }

    pub async fn cancel(&self, job_id: Uuid) -> Result<JobSummary> {
        let command = {
            let mut state = self.inner.state.lock().await;
            let current = state.jobs.get(&job_id).cloned().context("Job not found")?;
            match current.status {
                JobStatus::Queued => {
                    state.queue.retain(|id| *id != job_id);
                    let summary = state.jobs.get_mut(&job_id).context("Job not found")?;
                    summary.status = JobStatus::Cancelled;
                    summary.completed_at = Some(OffsetDateTime::now_utc());
                    let record = self.inner.store.append(
                        summary,
                        JobEvent::StateChanged {
                            from: Some(JobStatus::Queued),
                            to: JobStatus::Cancelled,
                            reason: Some("cancelled by user".to_string()),
                        },
                    )?;
                    self.publish(job_id, record);
                    return Ok(summary.clone());
                }
                JobStatus::WaitingApproval => {
                    if let Some(respond) = state.approvals.remove(&job_id) {
                        let _ = respond.send(ApprovalDecision::Reject);
                    }
                    let command = state
                        .workers
                        .get(&current.session_id)
                        .map(|worker| worker.command_tx.clone());
                    let summary = state.jobs.get_mut(&job_id).context("Job not found")?;
                    summary.status = JobStatus::Cancelling;
                    let record = self.inner.store.append(
                        summary,
                        JobEvent::StateChanged {
                            from: Some(JobStatus::WaitingApproval),
                            to: JobStatus::Cancelling,
                            reason: Some("cancellation requested".to_string()),
                        },
                    )?;
                    self.publish(job_id, record);
                    command
                }
                JobStatus::Running => {
                    let summary = state.jobs.get_mut(&job_id).context("Job not found")?;
                    summary.status = JobStatus::Cancelling;
                    let record = self.inner.store.append(
                        summary,
                        JobEvent::StateChanged {
                            from: Some(JobStatus::Running),
                            to: JobStatus::Cancelling,
                            reason: Some("cancellation requested".to_string()),
                        },
                    )?;
                    self.publish(job_id, record);
                    state
                        .workers
                        .get(&current.session_id)
                        .map(|worker| worker.command_tx.clone())
                }
                JobStatus::Cancelling => None,
                _ => bail!("Job {job_id} cannot be cancelled from {:?}", current.status),
            }
        };
        if let Some(command) = command {
            command
                .send(WorkerCommand::Cancel)
                .map_err(|_| anyhow::anyhow!("Job Worker is unavailable"))?;
        }
        self.get_summary(job_id).await
    }

    pub async fn approve(&self, job_id: Uuid, decision: ApprovalDecision) -> Result<JobSummary> {
        let respond = self
            .inner
            .state
            .lock()
            .await
            .approvals
            .remove(&job_id)
            .with_context(|| format!("Job {job_id} has no pending approval"))?;
        respond
            .send(decision)
            .map_err(|_| anyhow::anyhow!("Job approval request expired"))?;
        Ok(self.get_summary(job_id).await?)
    }

    pub async fn retry(&self, job_id: Uuid) -> Result<JobSummary> {
        let original = self.get_summary(job_id).await?;
        ensure!(
            matches!(
                original.status,
                JobStatus::Failed | JobStatus::Interrupted | JobStatus::Cancelled
            ),
            "Job {job_id} cannot be retried from {:?}",
            original.status
        );
        self.submit(Some(original.source_session_id), original.prompt, Some(job_id))
            .await
    }

    pub async fn daemon_status(&self) -> JobDaemonStatus {
        let state = self.inner.state.lock().await;
        let jobs = state.jobs.values().cloned().collect::<Vec<_>>();
        JobDaemonStatus {
            generation: Uuid::nil(),
            capacity: self.max_workers(),
            running: state.workers.values().filter(|worker| worker.busy).count(),
            queued: jobs.iter().filter(|job| job.status == JobStatus::Queued).count(),
            jobs,
        }
    }

    pub async fn stop(&self) {
        let workers = {
            let state = self.inner.state.lock().await;
            state
                .workers
                .values()
                .map(|worker| worker.command_tx.clone())
                .collect::<Vec<_>>()
        };
        for worker in workers {
            let _ = worker.send(WorkerCommand::Shutdown);
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobDaemonStatus {
    pub generation: Uuid,
    pub capacity: usize,
    pub running: usize,
    pub queued: usize,
    pub jobs: Vec<JobSummary>,
}

async fn worker_loop(
    agent: Agent,
    mut command_rx: mpsc::UnboundedReceiver<WorkerCommand>,
    message_tx: mpsc::UnboundedSender<WorkerMessage>,
) {
    let mut agent_slot = Some(agent);
    while let Some(command) = command_rx.recv().await {
        let WorkerCommand::Run { job_id, prompt } = command else {
            if matches!(command, WorkerCommand::Shutdown) {
                break;
            }
            continue;
        };
        let (approval_tx, mut approval_rx) = mpsc::unbounded_channel();
        let control = crate::TurnControl::interactive(approval_tx);
        let active_control = control.clone();
        let events = message_tx.clone();
        let current_agent = agent_slot.take().expect("Worker agent is available");
        let mut output = String::new();
        let mut task = tokio::spawn(async move {
            let mut agent = current_agent;
            let result = agent
                .turn_with_control(
                    prompt,
                    |event| {
                        if let AgentEvent::TextDelta { chunk, .. } = &event {
                            output.push_str(chunk);
                        }
                        let _ = events.send(WorkerMessage::AgentEvent { job_id, event });
                    },
                    control,
                )
                .await;
            (agent, result, output)
        });
        let mut cancelled = false;
        let mut pending_approval: Option<oneshot::Sender<ApprovalDecision>> = None;
        let result;
        let final_output;
        loop {
            tokio::select! {
                joined = &mut task => {
                    match joined {
                        Ok((returned_agent, turn_result, output)) => {
                            let returned_diagnostics = returned_agent.diagnostics();
                            result = turn_result
                                .map(|_| returned_diagnostics)
                                .map_err(|error| error.to_string());
                            final_output = output;
                            agent_slot = Some(returned_agent);
                        }
                        Err(error) => {
                            result = Err(format!("Job Worker task failed: {error}"));
                            final_output = String::new();
                        }
                    }
                    break;
                }
                Some(prompt) = approval_rx.recv() => {
                    let ApprovalPrompt { request, respond } = prompt;
                    pending_approval = Some(respond);
                    let _ = message_tx.send(WorkerMessage::Approval {
                        job_id,
                        request,
                        respond: pending_approval.take().expect("approval responder is available"),
                    });
                }
                Some(command) = command_rx.recv() => match command {
                    WorkerCommand::Cancel => {
                        cancelled = true;
                        active_control.cancel();
                        if let Some(respond) = pending_approval.take() {
                            let _ = respond.send(ApprovalDecision::Reject);
                        }
                    }
                    WorkerCommand::Shutdown => {
                        cancelled = true;
                        active_control.cancel();
                        if let Some(respond) = pending_approval.take() {
                            let _ = respond.send(ApprovalDecision::Reject);
                        }
                    }
                    WorkerCommand::Run { .. } => {}
                },
                else => {
                    cancelled = true;
                    active_control.cancel();
                }
            }
        }
        let _ = message_tx.send(WorkerMessage::Finished {
            job_id,
            result,
            output: final_output,
            cancelled,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_job_states_are_not_active() {
        assert!(JobStatus::Completed.is_terminal());
        assert!(JobStatus::Interrupted.is_terminal());
        assert!(!JobStatus::Running.is_terminal());
        assert!(!JobStatus::Cancelling.is_terminal());
    }

    #[test]
    fn job_event_records_are_json_serializable() {
        let event = JobEventRecord {
            schema_version: JOB_SCHEMA_VERSION,
            sequence: 1,
            timestamp: OffsetDateTime::now_utc(),
            event: JobEvent::StateChanged {
                from: None,
                to: JobStatus::Queued,
                reason: None,
            },
        };
        let encoded = serde_json::to_string(&event).unwrap();
        let decoded: JobEventRecord = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.sequence, 1);
    }

    #[test]
    fn job_store_round_trips_metadata_and_events() {
        let directory = tempfile::tempdir().unwrap();
        let store = JobStore::new(directory.path());
        let now = OffsetDateTime::now_utc();
        let mut summary = JobSummary {
            schema_version: JOB_SCHEMA_VERSION,
            job_id: Uuid::new_v4(),
            source_session_id: SessionId::new(),
            session_id: SessionId::new(),
            workspace: PathBuf::from("/repo"),
            prompt: "background work".to_string(),
            status: JobStatus::Queued,
            retry_of: None,
            created_at: now,
            updated_at: now,
            completed_at: None,
            output: String::new(),
            error: None,
            approval: None,
            diagnostics: None,
            last_sequence: 0,
        };
        store.create(&summary).unwrap();
        store
            .append(
                &mut summary,
                JobEvent::StateChanged {
                    from: None,
                    to: JobStatus::Queued,
                    reason: None,
                },
            )
            .unwrap();
        let loaded = store.load_summaries().unwrap();
        assert_eq!(loaded[0].job_id, summary.job_id);
        assert_eq!(loaded[0].last_sequence, 1);
        assert_eq!(store.load_events(summary.job_id, 0).unwrap().len(), 1);
    }
}
