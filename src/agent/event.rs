use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::session::TurnId;
use super::diagnostics::TurnDiagnostics;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum AgentEvent {
    TurnStarted {
        turn_id: TurnId,
    },
    TextDelta {
        turn_id: TurnId,
        message_id: Uuid,
        chunk: String,
        is_final: bool,
    },
    ToolStarted {
        turn_id: TurnId,
        call_id: String,
        name: String,
        arguments: Value,
    },
    ToolFinished {
        turn_id: TurnId,
        call_id: String,
        name: String,
        result: Value,
        success: bool,
    },
    DiagnosticsUpdated {
        turn_id: TurnId,
        diagnostics: TurnDiagnostics,
    },
    ApprovalRequired {
        turn_id: TurnId,
        request_id: String,
        call_id: String,
        tool_name: String,
        arguments: Value,
    },
    TurnCompleted {
        turn_id: TurnId,
    },
    Error {
        turn_id: Option<TurnId>,
        message: String,
        recoverable: bool,
    },
}
