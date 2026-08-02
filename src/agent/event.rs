use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub enum AgentEvent {
    TurnStarted,
    TextDelta {
        chunk: String,
        is_final: bool,
    },
    ToolStarted {
        call_id: String,
        name: String,
        arguments: Value,
    },
    ToolFinished {
        call_id: String,
        name: String,
        result: Value,
        success: bool,
    },
    ApprovalRequired {
        request_id: String,
        call_id: String,
        tool_name: String,
        arguments: Value,
    },
    TurnCompleted,
    Error {
        message: String,
        recoverable: bool,
    },
}
