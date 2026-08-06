use super::model::{
    AssistantRecord, CompactionRecord, RunId, RuntimeSnapshot, SessionId, ToolCallRecord,
    ToolResultRecord, TurnFailure, TurnId, UserMessageRecord,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use time::OffsetDateTime;
use uuid::Uuid;

pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventEnvelope {
    pub schema_version: u32,
    pub seq: u64,
    pub event_id: Uuid,
    pub session_id: SessionId,
    pub run_id: Option<RunId>,
    pub turn_id: Option<TurnId>,
    #[serde(with = "time::serde::rfc3339")]
    pub timestamp: OffsetDateTime,
    pub event: SessionEvent,
}

impl EventEnvelope {
    pub fn new(
        session_id: SessionId,
        seq: u64,
        run_id: Option<RunId>,
        turn_id: Option<TurnId>,
        event: SessionEvent,
    ) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            seq,
            event_id: Uuid::new_v4(),
            session_id,
            run_id,
            turn_id,
            timestamp: OffsetDateTime::now_utc(),
            event,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum SessionEvent {
    SessionCreated(SessionCreated),
    RuntimeStarted(RuntimeSnapshot),
    SkillActivated {
        name: String,
        source: String,
        digest: String,
        order: usize,
    },
    SkillDeactivated {
        name: String,
    },
    TitleChanged {
        title: String,
    },
    ModelChanged {
        model: String,
        model_id: String,
    },
    TurnStarted(UserMessageRecord),
    AssistantCompleted(AssistantRecord),
    ToolStarted(ToolCallRecord),
    ToolFinished(ToolResultRecord),
    TurnCompleted,
    TurnFailed(TurnFailure),
    TurnCancelled {
        reason: String,
    },
    TurnInterrupted {
        reason: String,
        partial_output: Option<String>,
    },
    ContextReset {
        new_epoch: u64,
    },
    ContextCompacted(CompactionRecord),
    SessionForked {
        parent_session_id: SessionId,
        through_seq: u64,
    },
    SessionArchived,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionCreated {
    pub title: String,
    pub workspace: PathBuf,
    pub model: String,
    pub model_id: String,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    pub parent_session_id: Option<SessionId>,
}
