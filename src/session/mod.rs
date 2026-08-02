//! Durable local sessions backed by append-only JSONL.

mod compaction;
mod event;
mod filesystem;
mod model;
mod projection;
mod recovery;

pub use model::{
    AssistantRecord, CompactionRecord, CreateSession, ExportFormat, ModelContext, RunId,
    RuntimeSnapshot, SessionFilter, SessionId, SessionSnapshot, SessionStatus, SessionSummary,
    ToolCallRecord, ToolResultRecord, Transcript, TranscriptItem, TranscriptKind, TurnFailure,
    TurnId,
};

use anyhow::{Context, Result, bail, ensure};
use event::{EventEnvelope, SessionCreated, SessionEvent};
use fs2::FileExt;
use projection::Projection;
use serde::Serialize;
use std::{
    collections::{HashMap, HashSet},
    fs::{self, File, OpenOptions},
    io::{BufWriter, Write},
    path::{Path, PathBuf},
};
use time::OffsetDateTime;
use uuid::Uuid;

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

const DEFAULT_TITLE: &str = "New session";

#[derive(Debug, Clone)]
pub struct SessionManager {
    root: PathBuf,
}

impl SessionManager {
    pub fn discover() -> Result<Self> {
        let root = match std::env::var_os("NOYA_DATA_DIR") {
            Some(path) => PathBuf::from(path),
            None => dirs::home_dir()
                .context("cannot determine the user home directory")?
                .join(".noya"),
        };
        Ok(Self::at(root))
    }

    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn create(&self, options: CreateSession) -> Result<Session> {
        self.create_with_parent(options, None, DEFAULT_TITLE.to_string())
    }

    fn create_with_parent(
        &self,
        options: CreateSession,
        parent_session_id: Option<SessionId>,
        title: String,
    ) -> Result<Session> {
        let workspace = options
            .workspace
            .canonicalize()
            .with_context(|| format!("canonicalize workspace {}", options.workspace.display()))?;
        self.ensure_root()?;
        let id = SessionId::new();
        let directory = self.sessions_dir().join(id.to_string());
        create_private_dir(&directory)?;
        let lock = acquire_lock(&directory)?;
        let events_path = directory.join("events.jsonl");
        let log = create_private_file(&events_path)?;
        let now = OffsetDateTime::now_utc();
        let created = SessionCreated {
            title,
            workspace,
            model: options.model,
            model_id: options.model_id,
            created_at: now,
            parent_session_id,
        };
        let envelope = EventEnvelope::new(id, 1, None, None, SessionEvent::SessionCreated(created));
        let projection = Projection::from_first(&envelope)?;
        let mut session = Session {
            id,
            directory: Some(directory),
            log: Some(log),
            lock: Some(lock),
            projection,
            last_seq: 0,
            draft_last_write: None,
            draft_last_bytes: 0,
            current_run_id: None,
        };
        session.write_envelope(&envelope, true)?;
        session.last_seq = 1;
        session.write_meta()?;
        Ok(session)
    }

    pub fn fork(&self, id: SessionId, through_seq: Option<u64>) -> Result<Session> {
        let source_directory = self.find_session_dir(id)?;
        let envelopes = filesystem::load_read_only(&source_directory.join("events.jsonl"))?;
        let source = Projection::replay(&envelopes)?;
        let cutoff = match through_seq {
            Some(seq) => seq,
            None => envelopes
                .iter()
                .rev()
                .find(|event| matches!(event.event, SessionEvent::TurnCompleted))
                .map(|event| event.seq)
                .context("session has no completed turn to fork")?,
        };
        let cutoff_event = envelopes
            .iter()
            .find(|event| event.seq == cutoff)
            .context("fork cutoff sequence does not exist")?;
        ensure!(
            matches!(&cutoff_event.event, SessionEvent::TurnCompleted),
            "fork cutoff must be a completed turn sequence"
        );
        let completed_turns = envelopes
            .iter()
            .take_while(|event| event.seq <= cutoff)
            .filter_map(|event| {
                matches!(&event.event, SessionEvent::TurnCompleted)
                    .then_some(event.turn_id)
                    .flatten()
            })
            .collect::<HashSet<_>>();
        let summary = source.summary();
        let mut child = self.create_with_parent(
            CreateSession {
                workspace: summary.workspace,
                model: summary.model,
                model_id: summary.model_id,
            },
            Some(id),
            summary.title,
        )?;
        child.append(
            SessionEvent::SessionForked {
                parent_session_id: id,
                through_seq: cutoff,
            },
            None,
            true,
        )?;
        let mut completed_sequences = HashMap::new();
        for envelope in envelopes
            .into_iter()
            .skip(1)
            .take_while(|event| event.seq <= cutoff)
        {
            let turn_is_completed = envelope
                .turn_id
                .is_some_and(|turn_id| completed_turns.contains(&turn_id));
            let is_turn_completed_event = matches!(&envelope.event, SessionEvent::TurnCompleted);
            let copied = match envelope.event {
                SessionEvent::TitleChanged { title } => Some(SessionEvent::TitleChanged { title }),
                SessionEvent::ModelChanged { model, model_id } => {
                    Some(SessionEvent::ModelChanged { model, model_id })
                }
                SessionEvent::ContextReset { new_epoch } => {
                    Some(SessionEvent::ContextReset { new_epoch })
                }
                SessionEvent::ContextCompacted(mut compaction) => {
                    compaction.through_seq = *completed_sequences
                        .get(&compaction.through_turn_id)
                        .context("fork cannot map compaction cutoff")?;
                    Some(SessionEvent::ContextCompacted(compaction))
                }
                SessionEvent::TurnStarted(record) if turn_is_completed => {
                    Some(SessionEvent::TurnStarted(record))
                }
                SessionEvent::AssistantCompleted(record) if turn_is_completed => {
                    Some(SessionEvent::AssistantCompleted(record))
                }
                SessionEvent::ToolStarted(record) if turn_is_completed => {
                    Some(SessionEvent::ToolStarted(record))
                }
                SessionEvent::ToolFinished(record) if turn_is_completed => {
                    Some(SessionEvent::ToolFinished(record))
                }
                SessionEvent::TurnCompleted if turn_is_completed => {
                    Some(SessionEvent::TurnCompleted)
                }
                _ => None,
            };
            if let Some(event) = copied {
                let child_seq = child.append(event, envelope.turn_id, true)?;
                if is_turn_completed_event && let Some(turn_id) = envelope.turn_id {
                    completed_sequences.insert(turn_id, child_seq);
                }
            }
        }
        Ok(child)
    }

    pub fn open(&self, id: SessionId) -> Result<Session> {
        let directory = self.find_session_dir(id)?;
        let lock = acquire_lock(&directory)?;
        let events_path = directory.join("events.jsonl");
        let envelopes = filesystem::load_and_repair(&events_path)?;
        let projection = Projection::replay(&envelopes)?;
        ensure!(
            projection.meta.session_id == id,
            "session ID does not match directory"
        );
        let log = open_append_private_file(&events_path)?;
        let last_seq = envelopes.last().map_or(0, |event| event.seq);
        let mut session = Session {
            id,
            directory: Some(directory),
            log: Some(log),
            lock: Some(lock),
            projection,
            last_seq,
            draft_last_write: None,
            draft_last_bytes: 0,
            current_run_id: None,
        };
        session.recover_unfinished_turn()?;
        session.write_meta()?;
        Ok(session)
    }

    pub fn latest(&self, workspace: &Path) -> Result<Option<SessionSummary>> {
        let workspace = workspace
            .canonicalize()
            .with_context(|| format!("canonicalize workspace {}", workspace.display()))?;
        Ok(self
            .list(SessionFilter {
                workspace: Some(workspace),
                include_archived: false,
            })?
            .into_iter()
            .next())
    }

    pub fn list(&self, filter: SessionFilter) -> Result<Vec<SessionSummary>> {
        let mut summaries = Vec::new();
        self.collect_summaries(&self.sessions_dir(), false, &filter, &mut summaries)?;
        if filter.include_archived {
            self.collect_summaries(&self.archive_dir(), true, &filter, &mut summaries)?;
        }
        summaries.sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
        Ok(summaries)
    }

    pub fn resolve_prefix(&self, prefix: &str, include_archived: bool) -> Result<SessionId> {
        let prefix = prefix.trim();
        ensure!(!prefix.is_empty(), "session ID prefix cannot be empty");
        let matches = self
            .list(SessionFilter {
                workspace: None,
                include_archived,
            })?
            .into_iter()
            .filter(|summary| summary.session_id.to_string().starts_with(prefix))
            .collect::<Vec<_>>();
        match matches.as_slice() {
            [summary] => Ok(summary.session_id),
            [] => bail!("no session matches '{prefix}'"),
            many => bail!(
                "session prefix '{prefix}' is ambiguous: {}",
                many.iter()
                    .map(|summary| summary.session_id.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }

    pub fn archive(&self, id: SessionId) -> Result<()> {
        let mut session = self.open(id)?;
        ensure!(!session.summary().archived, "session is already archived");
        session.append(SessionEvent::SessionArchived, None, true)?;
        let source = session
            .directory()
            .context("persistent session has no directory")?
            .to_path_buf();
        drop(session);
        create_private_dir(&self.archive_dir())?;
        let destination = self.archive_dir().join(id.to_string());
        ensure!(!destination.exists(), "archive destination already exists");
        fs::rename(&source, &destination).with_context(|| {
            format!(
                "archive session {} to {}",
                source.display(),
                destination.display()
            )
        })?;
        Ok(())
    }

    pub fn export(&self, id: SessionId, format: ExportFormat) -> Result<String> {
        let directory = self.find_session_dir(id)?;
        match format {
            ExportFormat::Jsonl => fs::read_to_string(directory.join("events.jsonl"))
                .with_context(|| format!("read session {id}")),
            ExportFormat::Markdown => {
                let envelopes = filesystem::load_read_only(&directory.join("events.jsonl"))?;
                let projection = Projection::replay(&envelopes)?;
                Ok(projection.transcript().to_markdown(&projection.summary()))
            }
        }
    }

    pub fn show(&self, id: SessionId) -> Result<SessionSnapshot> {
        let directory = self.find_session_dir(id)?;
        let envelopes = filesystem::load_read_only(&directory.join("events.jsonl"))?;
        let projection = Projection::replay(&envelopes)?;
        Ok(SessionSnapshot {
            summary: projection.summary(),
            transcript: projection.transcript(),
        })
    }

    fn ensure_root(&self) -> Result<()> {
        create_private_dir(&self.root)?;
        create_private_dir(&self.sessions_dir())?;
        create_private_dir(&self.archive_dir())
    }

    fn sessions_dir(&self) -> PathBuf {
        self.root.join("sessions")
    }

    fn archive_dir(&self) -> PathBuf {
        self.root.join("archive")
    }

    fn find_session_dir(&self, id: SessionId) -> Result<PathBuf> {
        let active = self.sessions_dir().join(id.to_string());
        if active.is_dir() {
            return Ok(active);
        }
        let archived = self.archive_dir().join(id.to_string());
        if archived.is_dir() {
            return Ok(archived);
        }
        bail!("session {id} was not found")
    }

    fn collect_summaries(
        &self,
        parent: &Path,
        archived: bool,
        filter: &SessionFilter,
        output: &mut Vec<SessionSummary>,
    ) -> Result<()> {
        if !parent.is_dir() {
            return Ok(());
        }
        for entry in fs::read_dir(parent).with_context(|| format!("read {}", parent.display()))? {
            let path = entry?.path();
            if !path.is_dir() {
                continue;
            }
            let mut summary = match read_meta(&path.join("meta.json")) {
                Ok(summary) => summary,
                Err(_) => {
                    let events = filesystem::load_read_only(&path.join("events.jsonl"))?;
                    Projection::replay(&events)?.summary()
                }
            };
            summary.archived = archived || summary.archived;
            if filter
                .workspace
                .as_ref()
                .is_some_and(|workspace| workspace != &summary.workspace)
            {
                continue;
            }
            output.push(summary);
        }
        Ok(())
    }
}

#[derive(Debug)]
pub struct Session {
    id: SessionId,
    directory: Option<PathBuf>,
    log: Option<File>,
    lock: Option<File>,
    projection: Projection,
    last_seq: u64,
    draft_last_write: Option<std::time::Instant>,
    draft_last_bytes: usize,
    current_run_id: Option<RunId>,
}

impl Session {
    pub(crate) fn ephemeral(workspace: PathBuf, model: String, model_id: String) -> Result<Self> {
        let workspace = workspace.canonicalize()?;
        let id = SessionId::new();
        let event = EventEnvelope::new(
            id,
            1,
            None,
            None,
            SessionEvent::SessionCreated(SessionCreated {
                title: DEFAULT_TITLE.to_string(),
                workspace,
                model,
                model_id,
                created_at: OffsetDateTime::now_utc(),
                parent_session_id: None,
            }),
        );
        Ok(Self {
            id,
            directory: None,
            log: None,
            lock: None,
            projection: Projection::from_first(&event)?,
            last_seq: 1,
            draft_last_write: None,
            draft_last_bytes: 0,
            current_run_id: None,
        })
    }

    pub fn id(&self) -> &SessionId {
        &self.id
    }

    pub fn summary(&self) -> SessionSummary {
        self.projection.summary()
    }

    pub fn transcript(&self) -> Transcript {
        self.projection.transcript()
    }

    pub fn context(&self) -> ModelContext {
        self.projection.context()
    }

    pub fn directory(&self) -> Option<&Path> {
        self.directory.as_deref()
    }

    pub fn log_path(&self) -> Option<PathBuf> {
        self.directory
            .as_ref()
            .map(|path| path.join("events.jsonl"))
    }

    pub fn rename(&mut self, title: impl Into<String>) -> Result<()> {
        let title = title.into();
        let title = title.trim();
        ensure!(!title.is_empty(), "session title cannot be empty");
        ensure!(
            title.chars().count() <= 120,
            "session title cannot exceed 120 characters"
        );
        ensure!(
            !title.chars().any(char::is_control),
            "session title cannot contain control characters"
        );
        self.append(
            SessionEvent::TitleChanged {
                title: title.to_string(),
            },
            None,
            true,
        )?;
        Ok(())
    }

    pub(crate) fn start_runtime(&mut self, snapshot: RuntimeSnapshot) -> Result<RunId> {
        ensure!(
            snapshot.workspace == self.projection.meta.workspace,
            "session workspace does not match runtime workspace"
        );
        let run_id = RunId::new();
        self.append_with_ids(
            SessionEvent::RuntimeStarted(snapshot),
            Some(run_id),
            None,
            true,
        )?;
        self.current_run_id = Some(run_id);
        Ok(run_id)
    }

    pub(crate) fn change_model(&mut self, model: String, model_id: String) -> Result<()> {
        ensure!(
            !self.projection.has_active_turn(),
            "cannot change model during an active turn"
        );
        if self.projection.meta.model == model && self.projection.meta.model_id == model_id {
            return Ok(());
        }
        self.append(SessionEvent::ModelChanged { model, model_id }, None, true)?;
        Ok(())
    }

    pub(crate) fn begin_turn(&mut self, input: impl Into<String>) -> Result<TurnId> {
        let input = input.into();
        ensure!(!input.trim().is_empty(), "turn input cannot be empty");
        ensure!(
            !self.projection.has_active_turn(),
            "session already has an active turn"
        );
        if self.projection.meta.completed_turns == 0 && self.projection.meta.title == DEFAULT_TITLE
        {
            self.rename(default_title(&input))?;
        }
        let turn_id = TurnId::new();
        self.append(
            SessionEvent::TurnStarted(model::UserMessageRecord {
                message_id: Uuid::new_v4(),
                content: input,
            }),
            Some(turn_id),
            true,
        )?;
        Ok(turn_id)
    }

    pub(crate) fn record_assistant(&mut self, response: AssistantRecord) -> Result<()> {
        let turn_id = self.projection.active_turn_id()?;
        self.append(
            SessionEvent::AssistantCompleted(response),
            Some(turn_id),
            false,
        )?;
        self.clear_draft()?;
        Ok(())
    }

    pub(crate) fn record_tool_started(&mut self, call: ToolCallRecord) -> Result<()> {
        let turn_id = self.projection.active_turn_id()?;
        self.append(SessionEvent::ToolStarted(call), Some(turn_id), true)?;
        Ok(())
    }

    pub(crate) fn record_tool_finished(&mut self, result: ToolResultRecord) -> Result<()> {
        let turn_id = self.projection.active_turn_id()?;
        self.append(SessionEvent::ToolFinished(result), Some(turn_id), true)?;
        Ok(())
    }

    pub(crate) fn finish_turn(&mut self, turn_id: &TurnId) -> Result<()> {
        ensure!(
            self.projection.active_turn_id()? == *turn_id,
            "turn ID does not match active turn"
        );
        self.append(SessionEvent::TurnCompleted, Some(*turn_id), true)?;
        self.clear_draft()?;
        Ok(())
    }

    pub(crate) fn fail_turn(&mut self, turn_id: &TurnId, failure: TurnFailure) -> Result<()> {
        ensure!(
            self.projection.active_turn_id()? == *turn_id,
            "turn ID does not match active turn"
        );
        self.append(SessionEvent::TurnFailed(failure), Some(*turn_id), true)?;
        self.clear_draft()?;
        Ok(())
    }

    pub(crate) fn cancel_turn(&mut self, turn_id: &TurnId, reason: String) -> Result<()> {
        ensure!(
            self.projection.active_turn_id()? == *turn_id,
            "turn ID does not match active turn"
        );
        self.append(SessionEvent::TurnCancelled { reason }, Some(*turn_id), true)?;
        self.clear_draft()?;
        Ok(())
    }

    pub(crate) fn reset_context(&mut self) -> Result<()> {
        ensure!(
            !self.projection.has_active_turn(),
            "cannot reset context during an active turn"
        );
        let new_epoch = self.projection.meta.context_epoch.saturating_add(1);
        self.append(SessionEvent::ContextReset { new_epoch }, None, true)?;
        Ok(())
    }

    pub(crate) fn apply_compaction(&mut self, compaction: CompactionRecord) -> Result<()> {
        ensure!(
            !self.projection.has_active_turn(),
            "cannot compact during an active turn"
        );
        self.append(SessionEvent::ContextCompacted(compaction), None, true)?;
        Ok(())
    }

    pub(crate) fn compaction_plan(&self) -> Option<model::CompactionPlan> {
        self.projection.compaction_plan()
    }

    pub(crate) fn should_auto_compact(&self, context_window: usize) -> bool {
        compaction::threshold_reached(self.context().estimated_tokens, context_window)
    }

    pub(crate) fn retry_input(&self) -> Option<String> {
        self.projection.retry_input()
    }

    pub(crate) fn checkpoint_draft(
        &mut self,
        run_id: RunId,
        turn_id: TurnId,
        message_id: Uuid,
        content: &str,
        force: bool,
    ) -> Result<()> {
        let Some(directory) = &self.directory else {
            return Ok(());
        };
        let should_write = force
            || content.len().saturating_sub(self.draft_last_bytes) >= recovery::DRAFT_BYTE_INTERVAL
            || self
                .draft_last_write
                .is_none_or(|last| last.elapsed() >= recovery::DRAFT_INTERVAL);
        if !should_write {
            return Ok(());
        }
        let draft =
            recovery::ActiveDraft::new(self.id, run_id, turn_id, message_id, content.to_string());
        write_json_atomic(&directory.join("active.json"), &draft)?;
        self.draft_last_write = Some(std::time::Instant::now());
        self.draft_last_bytes = content.len();
        Ok(())
    }

    fn append(&mut self, event: SessionEvent, turn_id: Option<TurnId>, sync: bool) -> Result<u64> {
        self.append_with_ids(event, self.current_run_id, turn_id, sync)
    }

    fn append_with_ids(
        &mut self,
        event: SessionEvent,
        run_id: Option<RunId>,
        turn_id: Option<TurnId>,
        sync: bool,
    ) -> Result<u64> {
        let seq = self.last_seq.saturating_add(1);
        let envelope = EventEnvelope::new(self.id, seq, run_id, turn_id, event);
        let mut next = self.projection.clone();
        next.apply(&envelope)?;
        self.write_envelope(&envelope, sync)?;
        self.projection = next;
        self.last_seq = seq;
        if let Err(error) = self.write_meta() {
            tracing::warn!(%error, session_id = %self.id, "session metadata update failed; events remain durable");
        }
        Ok(seq)
    }

    fn write_envelope(&mut self, envelope: &EventEnvelope, sync: bool) -> Result<()> {
        let Some(log) = self.log.as_mut() else {
            return Ok(());
        };
        serde_json::to_writer(&mut *log, envelope).context("encode session event")?;
        log.write_all(b"\n")
            .context("append session event newline")?;
        log.flush().context("flush session event")?;
        if sync {
            log.sync_data().context("sync session event")?;
        }
        Ok(())
    }

    fn write_meta(&self) -> Result<()> {
        let Some(directory) = &self.directory else {
            return Ok(());
        };
        write_json_atomic(&directory.join("meta.json"), &self.projection.summary())
    }

    fn recover_unfinished_turn(&mut self) -> Result<()> {
        let Some(turn_id) = self.projection.active_turn_id().ok() else {
            self.clear_draft()?;
            return Ok(());
        };
        let partial_output = self
            .directory
            .as_ref()
            .and_then(|directory| recovery::partial_output(directory, self.id, turn_id));
        self.append(
            SessionEvent::TurnInterrupted {
                reason: "process_terminated".to_string(),
                partial_output,
            },
            Some(turn_id),
            true,
        )?;
        self.clear_draft()
    }

    fn clear_draft(&mut self) -> Result<()> {
        self.draft_last_write = None;
        self.draft_last_bytes = 0;
        let Some(directory) = &self.directory else {
            return Ok(());
        };
        let path = directory.join("active.json");
        if path.exists() {
            fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
        }
        Ok(())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Some(lock) = &self.lock {
            let _ = FileExt::unlock(lock);
        }
    }
}

fn acquire_lock(directory: &Path) -> Result<File> {
    let path = directory.join("session.lock");
    let lock = open_private_rw_file(&path)?;
    lock.try_lock_exclusive().with_context(|| {
        format!(
            "session is already open by another Noya process: {}",
            directory.display()
        )
    })?;
    Ok(lock)
}

fn create_private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path).with_context(|| format!("create directory {}", path.display()))?;
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn create_private_file(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.create_new(true).read(true).write(true);
    #[cfg(unix)]
    options.mode(0o600);
    options
        .open(path)
        .with_context(|| format!("create {}", path.display()))
}

fn open_append_private_file(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).append(true);
    let file = options
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;
    #[cfg(unix)]
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

fn open_private_rw_file(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true);
    #[cfg(unix)]
    options.mode(0o600);
    let file = options
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;
    #[cfg(unix)]
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

fn write_json_atomic(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path.parent().context("atomic JSON path has no parent")?;
    let temporary = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy(),
        Uuid::new_v4()
    ));
    let file = create_private_file(&temporary)?;
    let mut writer = BufWriter::new(file);
    serde_json::to_writer_pretty(&mut writer, value)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    writer.get_ref().sync_all()?;
    fs::rename(&temporary, path).with_context(|| format!("replace {}", path.display()))?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

fn read_meta(path: &Path) -> Result<SessionSummary> {
    let content = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_str(&content).with_context(|| format!("decode {}", path.display()))
}

fn default_title(input: &str) -> String {
    let compact = input.split_whitespace().collect::<Vec<_>>().join(" ");
    compact.chars().take(60).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{CalledFunction, ToolCall};

    fn create_test_session(manager: &SessionManager, workspace: &tempfile::TempDir) -> Session {
        manager
            .create(CreateSession {
                workspace: workspace.path().to_path_buf(),
                model: "qwen".to_string(),
                model_id: "qwen3-coder-plus".to_string(),
            })
            .unwrap()
    }

    #[test]
    fn completed_tool_turn_reopens_as_provider_valid_context() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let manager = SessionManager::at(directory.path());
        let mut session = manager
            .create(CreateSession {
                workspace: workspace.path().to_path_buf(),
                model: "qwen".to_string(),
                model_id: "qwen3-coder-plus".to_string(),
            })
            .unwrap();
        let session_id = *session.id();

        let turn_id = session.begin_turn("inspect README").unwrap();
        session
            .record_assistant(AssistantRecord {
                message_id: uuid::Uuid::new_v4(),
                content: String::new(),
                reasoning_content: None,
                tool_calls: vec![ToolCall {
                    id: "call-1".to_string(),
                    r#type: "function".to_string(),
                    function: CalledFunction {
                        name: "read_file".to_string(),
                        arguments: "{\"path\":\"README.md\"}".to_string(),
                    },
                }],
            })
            .unwrap();
        session
            .record_tool_started(ToolCallRecord {
                call_id: "call-1".to_string(),
                name: "read_file".to_string(),
                arguments: serde_json::json!({"path": "README.md"}),
            })
            .unwrap();
        session
            .record_tool_finished(ToolResultRecord {
                call_id: "call-1".to_string(),
                name: "read_file".to_string(),
                result: serde_json::json!({"content": "Noya"}),
                success: true,
                duration_ms: 3,
            })
            .unwrap();
        session
            .record_assistant(AssistantRecord {
                message_id: uuid::Uuid::new_v4(),
                content: "README inspected.".to_string(),
                reasoning_content: None,
                tool_calls: Vec::new(),
            })
            .unwrap();
        session.finish_turn(&turn_id).unwrap();
        drop(session);

        let reopened = manager.open(session_id).unwrap();
        let messages = reopened.context().messages;
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0].role, "user");
        assert_eq!(messages[1].tool_calls.as_ref().unwrap()[0].id, "call-1");
        assert_eq!(messages[2].role, "tool");
        assert_eq!(messages[2].tool_call_id.as_deref(), Some("call-1"));
        assert_eq!(messages[3].content, "README inspected.");
    }

    #[test]
    fn compaction_keeps_transcript_and_only_projects_summary_plus_four_recent_turns() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let manager = SessionManager::at(directory.path());
        let mut session = manager
            .create(CreateSession {
                workspace: workspace.path().to_path_buf(),
                model: "qwen".to_string(),
                model_id: "qwen3-coder-plus".to_string(),
            })
            .unwrap();
        let session_id = *session.id();
        for index in 0..6 {
            let turn_id = session.begin_turn(format!("question {index}")).unwrap();
            session
                .record_assistant(AssistantRecord {
                    message_id: Uuid::new_v4(),
                    content: format!("answer {index}"),
                    reasoning_content: None,
                    tool_calls: Vec::new(),
                })
                .unwrap();
            session.finish_turn(&turn_id).unwrap();
        }
        let transcript_before = session.transcript().items.len();
        let plan = session.compaction_plan().unwrap();
        session
            .apply_compaction(CompactionRecord {
                summary: "The first two questions were answered.".to_string(),
                through_seq: plan.through_seq,
                through_turn_id: plan.through_turn_id,
                keep_from_turn_id: plan.keep_from_turn_id,
                source_token_estimate: plan.source_token_estimate,
                summary_model: "qwen3-coder-plus".to_string(),
            })
            .unwrap();
        drop(session);

        let reopened = manager.open(session_id).unwrap();
        assert_eq!(reopened.transcript().items.len(), transcript_before + 1);
        let context = reopened.context().messages;
        assert_eq!(context.len(), 9);
        assert!(context[0].content.contains("first two questions"));
        assert_eq!(context[1].content, "question 2");
        assert_eq!(context[8].content, "answer 5");
    }

    #[test]
    fn fork_is_independent_and_excludes_uncompleted_turns() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let manager = SessionManager::at(directory.path());
        let mut parent = manager
            .create(CreateSession {
                workspace: workspace.path().to_path_buf(),
                model: "kimi".to_string(),
                model_id: "kimi-k3".to_string(),
            })
            .unwrap();
        let parent_id = *parent.id();
        for index in 0..2 {
            let turn_id = parent.begin_turn(format!("question {index}")).unwrap();
            parent
                .record_assistant(AssistantRecord {
                    message_id: Uuid::new_v4(),
                    content: format!("answer {index}"),
                    reasoning_content: None,
                    tool_calls: Vec::new(),
                })
                .unwrap();
            parent.finish_turn(&turn_id).unwrap();
        }
        let failed = parent.begin_turn("do not copy").unwrap();
        parent
            .fail_turn(
                &failed,
                TurnFailure {
                    message: "failed".to_string(),
                    recoverable: true,
                },
            )
            .unwrap();
        drop(parent);

        let child = manager.fork(parent_id, None).unwrap();
        let child_id = *child.id();
        assert_eq!(child.summary().parent_session_id, Some(parent_id));
        assert_eq!(child.context().messages.len(), 4);
        assert!(
            !child
                .transcript()
                .items
                .iter()
                .any(|item| item.content.contains("do not copy"))
        );
        drop(child);

        let parent = manager.open(parent_id).unwrap();
        assert!(
            parent
                .transcript()
                .items
                .iter()
                .any(|item| item.content.contains("do not copy"))
        );
        drop(parent);
        let child = manager.open(child_id).unwrap();
        assert_eq!(child.context().messages[0].content, "question 0");
    }

    #[test]
    fn session_lock_rejects_a_second_writer() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let manager = SessionManager::at(directory.path());
        let session = create_test_session(&manager, &workspace);

        let error = manager.open(*session.id()).unwrap_err().to_string();

        assert!(error.contains("already open by another Noya process"));
    }

    #[test]
    fn unfinished_turn_recovers_partial_draft_without_polluting_context() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let manager = SessionManager::at(directory.path());
        let mut session = create_test_session(&manager, &workspace);
        let session_id = *session.id();
        let turn_id = session.begin_turn("unfinished request").unwrap();
        session
            .checkpoint_draft(
                RunId::new(),
                turn_id,
                Uuid::new_v4(),
                "partial answer",
                true,
            )
            .unwrap();
        drop(session);

        let reopened = manager.open(session_id).unwrap();

        assert!(reopened.context().messages.is_empty());
        assert_eq!(
            reopened.retry_input().as_deref(),
            Some("unfinished request")
        );
        assert!(
            reopened
                .transcript()
                .items
                .iter()
                .any(|item| item.content == "partial answer" && item.interrupted)
        );
        assert!(!reopened.directory().unwrap().join("active.json").exists());
    }

    #[test]
    fn torn_tail_is_backed_up_and_repaired_but_middle_corruption_is_rejected() {
        use std::io::Write as _;

        let directory = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let manager = SessionManager::at(directory.path());
        let session = create_test_session(&manager, &workspace);
        let session_id = *session.id();
        let log_path = session.log_path().unwrap();
        drop(session);
        std::fs::OpenOptions::new()
            .append(true)
            .open(&log_path)
            .unwrap()
            .write_all(b"{\"schema_version\":")
            .unwrap();

        let repaired = manager.open(session_id).unwrap();
        assert_eq!(repaired.summary().last_seq, 1);
        assert!(log_path.with_extension("jsonl.repair").exists());
        drop(repaired);

        let original = std::fs::read_to_string(&log_path).unwrap();
        std::fs::write(&log_path, format!("not-json\n{original}")).unwrap();
        let error = manager.open(session_id).unwrap_err().to_string();
        assert!(error.contains("decode session event"));
    }

    #[test]
    fn missing_metadata_is_rebuilt_and_archive_removes_session_from_latest() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let manager = SessionManager::at(directory.path());
        let session = create_test_session(&manager, &workspace);
        let session_id = *session.id();
        let meta_path = session.directory().unwrap().join("meta.json");
        drop(session);
        std::fs::remove_file(&meta_path).unwrap();

        let reopened = manager.open(session_id).unwrap();
        assert!(meta_path.exists());
        drop(reopened);
        manager.archive(session_id).unwrap();

        assert!(manager.latest(workspace.path()).unwrap().is_none());
        assert_eq!(
            manager
                .list(SessionFilter {
                    workspace: Some(workspace.path().canonicalize().unwrap()),
                    include_archived: true,
                })
                .unwrap()
                .len(),
            1
        );
    }

    #[cfg(unix)]
    #[test]
    fn session_directories_and_files_are_private() {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let manager = SessionManager::at(directory.path().join("data"));
        let session = create_test_session(&manager, &workspace);
        let session_dir = session.directory().unwrap();

        assert_eq!(
            std::fs::metadata(manager.root())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(session_dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        for name in ["events.jsonl", "meta.json", "session.lock"] {
            assert_eq!(
                std::fs::metadata(session_dir.join(name))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }
}
