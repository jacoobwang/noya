use super::{RunId, SessionId, TurnId, event::SCHEMA_VERSION};
use serde::{Deserialize, Serialize};
use std::{fs, path::Path};
use time::OffsetDateTime;
use uuid::Uuid;

pub(super) const DRAFT_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);
pub(super) const DRAFT_BYTE_INTERVAL: usize = 4 * 1024;

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct ActiveDraft {
    pub schema_version: u32,
    pub session_id: SessionId,
    pub run_id: RunId,
    pub turn_id: TurnId,
    pub message_id: Uuid,
    pub content: String,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

impl ActiveDraft {
    pub(super) fn new(
        session_id: SessionId,
        run_id: RunId,
        turn_id: TurnId,
        message_id: Uuid,
        content: String,
    ) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            session_id,
            run_id,
            turn_id,
            message_id,
            content,
            updated_at: OffsetDateTime::now_utc(),
        }
    }
}

pub(super) fn partial_output(
    directory: &Path,
    session_id: SessionId,
    turn_id: TurnId,
) -> Option<String> {
    fs::read_to_string(directory.join("active.json"))
        .ok()
        .and_then(|content| serde_json::from_str::<ActiveDraft>(&content).ok())
        .filter(|draft| {
            draft.schema_version == SCHEMA_VERSION
                && draft.session_id == session_id
                && draft.turn_id == turn_id
        })
        .map(|draft| draft.content)
}
