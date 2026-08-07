use super::{
    compaction::KEEP_RECENT_TURNS,
    event::{EventEnvelope, SCHEMA_VERSION, SessionEvent},
    model::{
        ActiveSkillRecord, CompactionPlan, ModelContext, SessionStatus, SessionSummary, Transcript,
        SessionTree, SessionTreeNode, TranscriptItem, TranscriptKind, TurnId,
    },
};
use crate::llm::ChatMessage;
use anyhow::{Context, Result, bail, ensure};
use std::collections::HashSet;

#[derive(Debug, Clone)]
pub(super) struct Projection {
    pub meta: SessionSummary,
    committed: Vec<StoredMessage>,
    completed: Vec<CompletedTurn>,
    active: Option<ActiveTurn>,
    transcript: Transcript,
    compaction_summary: Option<String>,
    retry_input: Option<String>,
    tree: SessionTree,
}

#[derive(Debug, Clone)]
struct StoredMessage {
    seq: u64,
    message: ChatMessage,
}

#[derive(Debug, Clone)]
struct ActiveTurn {
    id: TurnId,
    started_seq: u64,
    input: String,
    messages: Vec<StoredMessage>,
    unresolved: HashSet<String>,
    started: HashSet<String>,
}

#[derive(Debug, Clone)]
struct CompletedTurn {
    id: TurnId,
    started_seq: u64,
    completed_seq: u64,
    messages: Vec<StoredMessage>,
}

impl Projection {
    pub fn from_first(first: &EventEnvelope) -> Result<Self> {
        ensure!(
            first.schema_version == SCHEMA_VERSION,
            "unsupported session schema version {}",
            first.schema_version
        );
        ensure!(first.seq == 1, "session must start at sequence 1");
        let SessionEvent::SessionCreated(created) = &first.event else {
            bail!("first session event must be session_created");
        };
        Ok(Self {
            meta: SessionSummary {
                schema_version: SCHEMA_VERSION,
                session_id: first.session_id,
                title: created.title.clone(),
                workspace: created.workspace.clone(),
                model: created.model.clone(),
                model_id: created.model_id.clone(),
                created_at: created.created_at,
                updated_at: first.timestamp,
                status: SessionStatus::Idle,
                last_seq: first.seq,
                completed_turns: 0,
                context_epoch: 0,
                compaction_through_seq: None,
                parent_session_id: created.parent_session_id,
                active_branch_id: None,
                active_head_seq: 1,
                branch_count: 0,
                archived: false,
                active_skills: Vec::new(),
            },
            committed: Vec::new(),
            completed: Vec::new(),
            active: None,
            transcript: Transcript::default(),
            compaction_summary: None,
            retry_input: None,
            tree: SessionTree {
                nodes: vec![SessionTreeNode {
                    seq: 1,
                    parent_seq: None,
                    event_type: "session_created".to_string(),
                    turn_id: first.turn_id,
                }],
                active_head_seq: 1,
                ..SessionTree::default()
            },
        })
    }

    pub fn replay(events: &[EventEnvelope]) -> Result<Self> {
        let first = events.first().context("session log is empty")?;
        let tree = SessionTree::replay(events)?;
        let active_path = tree.active_path().into_iter().collect::<HashSet<_>>();
        let mut projection = Self::from_first(first)?;
        let mut expected = 2;
        for event in &events[1..] {
            ensure!(
                event.schema_version == SCHEMA_VERSION,
                "unsupported session schema version {}",
                event.schema_version
            );
            ensure!(
                event.session_id == projection.meta.session_id,
                "session ID changed inside log"
            );
            ensure!(
                event.seq == expected,
                "session sequence mismatch: expected {expected}, got {}",
                event.seq
            );
            if active_path.contains(&event.seq) {
                projection.apply(event)?;
            }
            expected += 1;
        }
        projection.tree = tree.clone();
        projection.meta.last_seq = events.last().map_or(1, |event| event.seq);
        projection.meta.updated_at = events.last().map_or(first.timestamp, |event| event.timestamp);
        projection.meta.active_branch_id = tree.active_branch_id;
        projection.meta.active_head_seq = tree.active_head_seq;
        projection.meta.branch_count = tree.branches.len();
        Ok(projection)
    }

    pub fn apply(&mut self, envelope: &EventEnvelope) -> Result<()> {
        if envelope.seq == 1 {
            return Ok(());
        }
        ensure!(
            envelope.seq > self.meta.last_seq,
            "session sequence must increase"
        );
        match &envelope.event {
            SessionEvent::SessionCreated(_) => bail!("session_created can only be the first event"),
            SessionEvent::RuntimeStarted(snapshot) => {
                ensure!(
                    snapshot.workspace == self.meta.workspace,
                    "runtime workspace does not match session"
                );
            }
            SessionEvent::SkillActivated { name, source, digest, order } => {
                ensure!(self.active.is_none(), "cannot activate a Skill during an active turn");
                ensure!(!name.trim().is_empty(), "Skill name cannot be empty");
                ensure!(!digest.trim().is_empty(), "Skill digest cannot be empty");
                ensure!(
                    self.meta.active_skills.iter().all(|active| active.name != *name),
                    "Skill {name} is already active"
                );
                self.meta.active_skills.push(ActiveSkillRecord {
                    name: name.clone(),
                    source: source.clone(),
                    digest: digest.clone(),
                    order: *order,
                });
                self.meta.active_skills.sort_by_key(|active| active.order);
            }
            SessionEvent::SkillDeactivated { name } => {
                ensure!(self.active.is_none(), "cannot deactivate a Skill during an active turn");
                self.meta.active_skills.retain(|active| active.name != *name);
            }
            SessionEvent::TitleChanged { title } => self.meta.title = title.clone(),
            SessionEvent::ModelChanged { model, model_id } => {
                self.meta.model = model.clone();
                self.meta.model_id = model_id.clone();
            }
            SessionEvent::TurnStarted(user) => {
                ensure!(self.active.is_none(), "cannot start a second active turn");
                let turn_id = required_turn_id(envelope)?;
                let message = ChatMessage {
                    role: "user".to_string(),
                    content: user.content.clone(),
                    reasoning_content: None,
                    tool_call_id: None,
                    tool_calls: None,
                };
                self.transcript.items.push(TranscriptItem {
                    id: user.message_id,
                    kind: TranscriptKind::User,
                    content: user.content.clone(),
                    turn_id: Some(turn_id),
                    tool_call_id: None,
                    interrupted: false,
                });
                self.active = Some(ActiveTurn {
                    id: turn_id,
                    started_seq: envelope.seq,
                    input: user.content.clone(),
                    messages: vec![StoredMessage {
                        seq: envelope.seq,
                        message,
                    }],
                    unresolved: HashSet::new(),
                    started: HashSet::new(),
                });
                self.meta.status = SessionStatus::Running;
            }
            SessionEvent::AssistantCompleted(assistant) => {
                let turn_id = required_turn_id(envelope)?;
                let active = self.active_mut(turn_id)?;
                ensure!(
                    active.unresolved.is_empty(),
                    "assistant response arrived before prior tool group completed"
                );
                for call in &assistant.tool_calls {
                    ensure!(
                        !call.id.trim().is_empty(),
                        "assistant tool call is missing ID"
                    );
                    ensure!(
                        active.unresolved.insert(call.id.clone()),
                        "duplicate tool call ID {}",
                        call.id
                    );
                }
                active.messages.push(StoredMessage {
                    seq: envelope.seq,
                    message: ChatMessage {
                        role: "assistant".to_string(),
                        content: assistant.content.clone(),
                        reasoning_content: assistant.reasoning_content.clone(),
                        tool_call_id: None,
                        tool_calls: (!assistant.tool_calls.is_empty())
                            .then_some(assistant.tool_calls.clone()),
                    },
                });
                if !assistant.content.is_empty() {
                    self.transcript.items.push(TranscriptItem {
                        id: assistant.message_id,
                        kind: TranscriptKind::Agent,
                        content: assistant.content.clone(),
                        turn_id: Some(turn_id),
                        tool_call_id: None,
                        interrupted: false,
                    });
                }
            }
            SessionEvent::ToolStarted(call) => {
                let turn_id = required_turn_id(envelope)?;
                let active = self.active_mut(turn_id)?;
                ensure!(
                    active.unresolved.contains(&call.call_id),
                    "tool call {} was not declared by assistant",
                    call.call_id
                );
                ensure!(
                    active.started.insert(call.call_id.clone()),
                    "tool call {} started twice",
                    call.call_id
                );
                self.transcript.items.push(TranscriptItem {
                    id: envelope.event_id,
                    kind: TranscriptKind::Tool,
                    content: format!("{} {}", call.name, compact_json(&call.arguments)),
                    turn_id: Some(turn_id),
                    tool_call_id: Some(call.call_id.clone()),
                    interrupted: false,
                });
            }
            SessionEvent::ToolFinished(result) => {
                let turn_id = required_turn_id(envelope)?;
                let active = self.active_mut(turn_id)?;
                ensure!(
                    active.started.remove(&result.call_id),
                    "tool call {} finished before it started",
                    result.call_id
                );
                ensure!(
                    active.unresolved.remove(&result.call_id),
                    "tool call {} has no unresolved assistant call",
                    result.call_id
                );
                active.messages.push(StoredMessage {
                    seq: envelope.seq,
                    message: ChatMessage {
                        role: "tool".to_string(),
                        content: serde_json::to_string(&result.result)?,
                        reasoning_content: None,
                        tool_call_id: Some(result.call_id.clone()),
                        tool_calls: None,
                    },
                });
                if let Some(item) = self
                    .transcript
                    .items
                    .iter_mut()
                    .rev()
                    .find(|item| item.tool_call_id.as_deref() == Some(&result.call_id))
                {
                    let marker = if result.success {
                        "completed"
                    } else {
                        "failed"
                    };
                    item.content =
                        format!("{} {marker}: {}", result.name, compact_json(&result.result));
                }
            }
            SessionEvent::TurnCompleted => {
                let turn_id = required_turn_id(envelope)?;
                let active = self.active.take().context("no active turn to complete")?;
                ensure!(
                    active.id == turn_id,
                    "completed turn ID does not match active turn"
                );
                ensure!(
                    active.unresolved.is_empty(),
                    "cannot complete turn with unresolved tool calls"
                );
                self.committed.extend(active.messages.clone());
                self.completed.push(CompletedTurn {
                    id: active.id,
                    started_seq: active.started_seq,
                    completed_seq: envelope.seq,
                    messages: active.messages,
                });
                self.meta.completed_turns += 1;
                self.meta.status = SessionStatus::Idle;
                self.retry_input = None;
            }
            SessionEvent::TurnFailed(failure) => {
                let active = self.take_terminal_turn(envelope)?;
                self.retry_input = Some(active.input);
                self.meta.status = SessionStatus::Idle;
                self.transcript.items.push(TranscriptItem {
                    id: envelope.event_id,
                    kind: TranscriptKind::Error,
                    content: failure.message.clone(),
                    turn_id: envelope.turn_id,
                    tool_call_id: None,
                    interrupted: true,
                });
            }
            SessionEvent::TurnCancelled { reason } => {
                let active = self.take_terminal_turn(envelope)?;
                self.retry_input = Some(active.input);
                self.meta.status = SessionStatus::Idle;
                self.transcript.items.push(TranscriptItem {
                    id: envelope.event_id,
                    kind: TranscriptKind::System,
                    content: format!("Turn cancelled: {reason}"),
                    turn_id: envelope.turn_id,
                    tool_call_id: None,
                    interrupted: true,
                });
            }
            SessionEvent::TurnInterrupted {
                reason,
                partial_output,
            } => {
                let active = self.take_terminal_turn(envelope)?;
                self.retry_input = Some(active.input);
                self.meta.status = SessionStatus::Interrupted;
                if let Some(content) = partial_output {
                    self.transcript.items.push(TranscriptItem {
                        id: envelope.event_id,
                        kind: TranscriptKind::Agent,
                        content: content.clone(),
                        turn_id: envelope.turn_id,
                        tool_call_id: None,
                        interrupted: true,
                    });
                }
                self.transcript.items.push(TranscriptItem {
                    id: envelope.event_id,
                    kind: TranscriptKind::System,
                    content: format!("Turn interrupted: {reason}"),
                    turn_id: envelope.turn_id,
                    tool_call_id: None,
                    interrupted: true,
                });
            }
            SessionEvent::ContextReset { new_epoch } => {
                ensure!(
                    self.active.is_none(),
                    "cannot reset context with active turn"
                );
                ensure!(
                    *new_epoch == self.meta.context_epoch + 1,
                    "context epoch must increment by one"
                );
                self.meta.context_epoch = *new_epoch;
                self.committed.clear();
                self.completed.clear();
                self.compaction_summary = None;
                self.meta.compaction_through_seq = None;
                self.transcript.items.push(TranscriptItem {
                    id: envelope.event_id,
                    kind: TranscriptKind::System,
                    content: "Context reset.".to_string(),
                    turn_id: None,
                    tool_call_id: None,
                    interrupted: false,
                });
            }
            SessionEvent::ContextCompacted(compaction) => {
                ensure!(
                    self.active.is_none(),
                    "cannot compact context with active turn"
                );
                ensure!(
                    !compaction.summary.trim().is_empty(),
                    "compaction summary cannot be empty"
                );
                let cutoff = self
                    .completed
                    .iter()
                    .position(|turn| {
                        turn.id == compaction.through_turn_id
                            && turn.completed_seq == compaction.through_seq
                    })
                    .context("compaction cutoff is not a completed turn boundary")?;
                let expected_keep = self.completed.get(cutoff + 1).map(|turn| turn.id);
                ensure!(
                    expected_keep == compaction.keep_from_turn_id,
                    "compaction keep_from turn does not follow cutoff"
                );
                self.committed
                    .retain(|message| message.seq > compaction.through_seq);
                self.completed
                    .retain(|turn| turn.completed_seq > compaction.through_seq);
                self.compaction_summary = Some(compaction.summary.clone());
                self.meta.compaction_through_seq = Some(compaction.through_seq);
                self.transcript.items.push(TranscriptItem {
                    id: envelope.event_id,
                    kind: TranscriptKind::System,
                    content: "Context compacted.".to_string(),
                    turn_id: None,
                    tool_call_id: None,
                    interrupted: false,
                });
            }
            SessionEvent::SessionForked {
                parent_session_id, ..
            } => {
                self.meta.parent_session_id = Some(*parent_session_id);
            }
            SessionEvent::BranchCreated { .. }
            | SessionEvent::BranchSelected { .. } => {
                ensure!(self.active.is_none(), "cannot change branches during an active turn");
            }
            SessionEvent::BranchSummary { summary, .. } => {
                ensure!(self.active.is_none(), "cannot add a branch summary during an active turn");
                ensure!(!summary.trim().is_empty(), "branch summary cannot be empty");
                self.committed.push(StoredMessage {
                    seq: envelope.seq,
                    message: ChatMessage {
                        role: "system".to_string(),
                        content: format!("Summary from another branch:\n{summary}"),
                        reasoning_content: None,
                        tool_call_id: None,
                        tool_calls: None,
                    },
                });
                self.transcript.items.push(TranscriptItem {
                    id: envelope.event_id,
                    kind: TranscriptKind::System,
                    content: "Branch summary added.".to_string(),
                    turn_id: None,
                    tool_call_id: None,
                    interrupted: false,
                });
            }
            SessionEvent::SessionArchived => {
                ensure!(self.active.is_none(), "cannot archive an active session");
                self.meta.archived = true;
                self.meta.status = SessionStatus::Archived;
            }
        }
        self.meta.updated_at = envelope.timestamp;
        self.meta.last_seq = envelope.seq;
        Ok(())
    }

    pub fn summary(&self) -> SessionSummary {
        self.meta.clone()
    }

    pub fn tree(&self) -> SessionTree {
        self.tree.clone()
    }

    pub(super) fn set_tree(&mut self, tree: SessionTree) {
        self.meta.active_branch_id = tree.active_branch_id;
        self.meta.active_head_seq = tree.active_head_seq;
        self.meta.branch_count = tree.branches.len();
        self.tree = tree;
    }

    pub fn active_skills(&self) -> Vec<ActiveSkillRecord> {
        self.meta.active_skills.clone()
    }

    pub fn transcript(&self) -> Transcript {
        self.transcript.clone()
    }

    pub fn context(&self) -> ModelContext {
        let mut messages = Vec::new();
        if let Some(summary) = &self.compaction_summary {
            messages.push(ChatMessage {
                role: "system".to_string(),
                content: format!("Summary of earlier session context:\n{summary}"),
                reasoning_content: None,
                tool_call_id: None,
                tool_calls: None,
            });
        }
        messages.extend(self.committed.iter().map(|stored| stored.message.clone()));
        if let Some(active) = &self.active {
            messages.extend(active.messages.iter().map(|stored| stored.message.clone()));
        }
        let estimated_tokens = messages.iter().map(estimate_message_tokens).sum();
        ModelContext {
            messages,
            estimated_tokens,
        }
    }

    pub fn has_active_turn(&self) -> bool {
        self.active.is_some()
    }

    pub fn active_turn_id(&self) -> Result<TurnId> {
        self.active
            .as_ref()
            .map(|turn| turn.id)
            .context("session has no active turn")
    }

    pub fn retry_input(&self) -> Option<String> {
        self.retry_input.clone()
    }

    pub fn compaction_plan(&self) -> Option<CompactionPlan> {
        if self.active.is_some() || self.completed.len() <= KEEP_RECENT_TURNS {
            return None;
        }
        let compact_count = self.completed.len() - KEEP_RECENT_TURNS;
        let through = &self.completed[compact_count - 1];
        let keep_from_turn_id = self.completed.get(compact_count).map(|turn| turn.id);
        let mut source = String::new();
        if let Some(summary) = &self.compaction_summary {
            source.push_str("Previous summary:\n");
            source.push_str(summary);
            source.push_str("\n\nNew completed turns:\n");
        }
        for turn in self.completed.iter().take(compact_count) {
            source.push_str(&format!(
                "\nTurn {} (seq {}-{}):\n",
                turn.id, turn.started_seq, turn.completed_seq
            ));
            for stored in &turn.messages {
                source.push_str(&stored.message.role);
                source.push_str(": ");
                source.push_str(&stored.message.content);
                if let Some(calls) = &stored.message.tool_calls {
                    source.push_str("\ntool_calls: ");
                    source.push_str(&serde_json::to_string(calls).unwrap_or_default());
                }
                source.push('\n');
            }
        }
        let source_token_estimate = source.chars().count().div_ceil(4).max(1);
        Some(CompactionPlan {
            source,
            through_seq: through.completed_seq,
            through_turn_id: through.id,
            keep_from_turn_id,
            source_token_estimate,
        })
    }

    fn active_mut(&mut self, turn_id: TurnId) -> Result<&mut ActiveTurn> {
        let active = self.active.as_mut().context("session has no active turn")?;
        ensure!(
            active.id == turn_id,
            "event turn ID does not match active turn"
        );
        Ok(active)
    }

    fn take_terminal_turn(&mut self, envelope: &EventEnvelope) -> Result<ActiveTurn> {
        let turn_id = required_turn_id(envelope)?;
        let active = self.active.take().context("session has no active turn")?;
        ensure!(
            active.id == turn_id,
            "terminal turn ID does not match active turn"
        );
        Ok(active)
    }
}

fn required_turn_id(envelope: &EventEnvelope) -> Result<TurnId> {
    envelope.turn_id.context("turn event is missing turn_id")
}

fn compact_json(value: &serde_json::Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| value.to_string())
}

fn estimate_message_tokens(message: &ChatMessage) -> usize {
    let mut characters = message.content.chars().count();
    characters += message
        .reasoning_content
        .as_deref()
        .map_or(0, |reasoning| reasoning.chars().count());
    characters += message
        .tool_call_id
        .as_deref()
        .map_or(0, |call_id| call_id.chars().count());
    characters += message.tool_calls.as_ref().map_or(0, |calls| {
        serde_json::to_string(calls).map_or(0, |encoded| encoded.chars().count())
    });
    characters.div_ceil(4).max(1)
}
