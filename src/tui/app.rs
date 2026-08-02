use crate::{AgentEvent, ApprovalDecision, ApprovalRequest};
use serde_json::Value;
use std::{collections::VecDeque, path::PathBuf};
use uuid::Uuid;

const MAX_MESSAGE_HISTORY: usize = 1000;

#[derive(Debug, Clone)]
pub struct AppInfo {
    pub workspace: PathBuf,
    pub model: String,
    pub model_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageKind {
    User,
    Agent,
    Tool,
    System,
    Error,
}

#[derive(Debug, Clone)]
pub struct Message {
    pub id: Uuid,
    pub kind: MessageKind,
    pub content: String,
    pub is_streaming: bool,
    pub tool_call_id: Option<String>,
}

impl Message {
    pub fn new(kind: MessageKind, content: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            kind,
            content: content.into(),
            is_streaming: false,
            tool_call_id: None,
        }
    }

    fn streaming(kind: MessageKind) -> Self {
        let mut message = Self::new(kind, "");
        message.is_streaming = true;
        message
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentState {
    Idle,
    Thinking,
    Generating,
    RunningTool,
    WaitingApproval,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppMode {
    Normal,
    Confirming,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TuiAction {
    None,
    Submit(String),
    Reset,
    Cancel,
    Approval(ApprovalDecision),
    Quit,
}

pub struct App {
    pub info: AppInfo,
    pub messages: VecDeque<Message>,
    pub input: String,
    pub cursor_position: usize,
    pub streaming_message_id: Option<Uuid>,
    pub agent_state: AgentState,
    pub mode: AppMode,
    pub pending_approval: Option<ApprovalRequest>,
    pub status_message: Option<String>,
    pub should_quit: bool,
}

impl App {
    pub fn new(info: AppInfo) -> Self {
        Self {
            info,
            messages: VecDeque::new(),
            input: String::new(),
            cursor_position: 0,
            streaming_message_id: None,
            agent_state: AgentState::Idle,
            mode: AppMode::Normal,
            pending_approval: None,
            status_message: None,
            should_quit: false,
        }
    }

    pub fn handle_agent_event(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::TurnStarted => {
                self.agent_state = AgentState::Thinking;
                self.status_message = None;
            }
            AgentEvent::TextDelta { chunk, is_final } => {
                if self.streaming_message_id.is_none() && !chunk.is_empty() {
                    let message = Message::streaming(MessageKind::Agent);
                    self.streaming_message_id = Some(message.id);
                    self.add_message(message);
                }
                if let Some(id) = self.streaming_message_id
                    && let Some(message) = self.messages.iter_mut().find(|message| message.id == id)
                {
                    message.content.push_str(&chunk);
                    if is_final {
                        message.is_streaming = false;
                        self.streaming_message_id = None;
                    } else {
                        self.agent_state = AgentState::Generating;
                    }
                }
            }
            AgentEvent::ToolStarted {
                call_id,
                name,
                arguments,
            } => {
                let mut message = Message::streaming(MessageKind::Tool);
                message.tool_call_id = Some(call_id);
                message.content = format!("{name} {}", compact_json(&arguments));
                self.add_message(message);
                self.agent_state = AgentState::RunningTool;
            }
            AgentEvent::ToolFinished {
                call_id,
                name,
                result,
                success,
            } => {
                let marker = if success { "completed" } else { "failed" };
                let content = format!("{name} {marker}: {}", compact_json(&result));
                if let Some(message) = self
                    .messages
                    .iter_mut()
                    .rev()
                    .find(|message| message.tool_call_id.as_deref() == Some(&call_id))
                {
                    message.content = content;
                    message.is_streaming = false;
                } else {
                    let mut message = Message::new(MessageKind::Tool, content);
                    message.tool_call_id = Some(call_id);
                    self.add_message(message);
                }
                self.agent_state = AgentState::Thinking;
            }
            AgentEvent::ApprovalRequired {
                request_id,
                call_id,
                tool_name,
                arguments,
            } => {
                self.pending_approval = Some(ApprovalRequest {
                    request_id,
                    call_id,
                    tool_name: tool_name.clone(),
                    arguments: arguments.clone(),
                });
                self.mode = AppMode::Confirming;
                self.agent_state = AgentState::WaitingApproval;
                self.add_message(Message::new(
                    MessageKind::System,
                    format!(
                        "Approval required for {tool_name}: {}\nUse /approve, /reject, or /modify <json>.",
                        compact_json(&arguments)
                    ),
                ));
            }
            AgentEvent::TurnCompleted => {
                self.finish_streaming_message();
                self.agent_state = AgentState::Idle;
                self.mode = AppMode::Normal;
                self.pending_approval = None;
            }
            AgentEvent::Error {
                message,
                recoverable,
            } => {
                self.finish_streaming_message();
                let label = if recoverable {
                    "Recoverable error"
                } else {
                    "Error"
                };
                self.add_message(Message::new(
                    MessageKind::Error,
                    format!("{label}: {message}"),
                ));
                self.agent_state = AgentState::Error;
                self.mode = AppMode::Normal;
                self.pending_approval = None;
            }
        }
    }

    pub fn submit_input(&mut self) -> TuiAction {
        let input = self.input.trim().to_string();
        if input.is_empty() {
            return TuiAction::None;
        }
        self.input.clear();
        self.cursor_position = 0;
        self.handle_submission(input)
    }

    pub fn handle_submission(&mut self, input: String) -> TuiAction {
        match input.as_str() {
            "/help" => {
                self.add_message(Message::new(
                    MessageKind::System,
                    "Commands: /help /clear /reset /status /cancel /quit",
                ));
                TuiAction::None
            }
            "/clear" => {
                self.messages.clear();
                self.streaming_message_id = None;
                TuiAction::None
            }
            "/reset" if self.agent_state == AgentState::Idle => {
                self.messages.clear();
                self.streaming_message_id = None;
                self.agent_state = AgentState::Idle;
                self.add_message(Message::new(MessageKind::System, "Session reset."));
                TuiAction::Reset
            }
            "/reset" => {
                self.status_message = Some("Agent is busy; cancel before resetting.".to_string());
                TuiAction::None
            }
            "/status" => {
                self.add_message(Message::new(
                    MessageKind::System,
                    format!(
                        "Workspace: {}\nModel: {}\nModel ID: {}\nState: {:?}",
                        self.info.workspace.display(),
                        self.info.model,
                        self.info.model_id,
                        self.agent_state
                    ),
                ));
                TuiAction::None
            }
            "/cancel" => TuiAction::Cancel,
            "/approve" | "/confirm" if self.mode == AppMode::Confirming => {
                TuiAction::Approval(ApprovalDecision::Approve)
            }
            "/reject" if self.mode == AppMode::Confirming => {
                TuiAction::Approval(ApprovalDecision::Reject)
            }
            "/quit" | "/exit" => {
                self.should_quit = true;
                TuiAction::Quit
            }
            _ if input.starts_with("/modify ") && self.mode == AppMode::Confirming => {
                match serde_json::from_str::<Value>(input[8..].trim()) {
                    Ok(value) => TuiAction::Approval(ApprovalDecision::Modify(value)),
                    Err(error) => {
                        self.status_message = Some(format!("Invalid JSON: {error}"));
                        TuiAction::None
                    }
                }
            }
            _ if input.starts_with('/') => {
                self.status_message = Some(format!("Unknown command: {input}"));
                TuiAction::None
            }
            _ if self.agent_state != AgentState::Idle => {
                self.status_message = Some("Agent is busy; wait or use /cancel.".to_string());
                TuiAction::None
            }
            _ => {
                self.add_message(Message::new(MessageKind::User, input.clone()));
                self.agent_state = AgentState::Thinking;
                TuiAction::Submit(input)
            }
        }
    }

    pub fn current_streaming_message(&self) -> Option<&Message> {
        self.streaming_message_id
            .and_then(|id| self.messages.iter().find(|message| message.id == id))
            .or_else(|| {
                self.messages
                    .iter()
                    .rev()
                    .find(|message| message.is_streaming)
            })
    }

    pub fn add_message(&mut self, message: Message) {
        self.messages.push_back(message);
        while self.messages.len() > MAX_MESSAGE_HISTORY {
            self.messages.pop_front();
        }
    }

    fn finish_streaming_message(&mut self) {
        if let Some(id) = self.streaming_message_id.take()
            && let Some(message) = self.messages.iter_mut().find(|message| message.id == id)
        {
            message.is_streaming = false;
        }
        for message in &mut self.messages {
            message.is_streaming = false;
        }
    }
}

fn compact_json(value: &Value) -> String {
    let rendered = serde_json::to_string(value).unwrap_or_else(|_| value.to_string());
    const LIMIT: usize = 500;
    if rendered.chars().count() <= LIMIT {
        rendered
    } else {
        format!("{}…", rendered.chars().take(LIMIT).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AgentEvent;
    use std::path::PathBuf;

    #[test]
    fn text_deltas_update_one_streaming_message() {
        let mut app = App::new(AppInfo {
            workspace: PathBuf::from("/repo"),
            model: "test".to_string(),
            model_id: "test-model".to_string(),
        });

        app.handle_agent_event(AgentEvent::TurnStarted);
        app.handle_agent_event(AgentEvent::TextDelta {
            chunk: "Hel".to_string(),
            is_final: false,
        });
        app.handle_agent_event(AgentEvent::TextDelta {
            chunk: "lo".to_string(),
            is_final: false,
        });

        assert_eq!(app.messages.len(), 1);
        assert_eq!(app.messages[0].content, "Hello");
        assert!(app.messages[0].is_streaming);
        assert_eq!(app.agent_state, AgentState::Generating);

        app.handle_agent_event(AgentEvent::TextDelta {
            chunk: String::new(),
            is_final: true,
        });

        assert!(!app.messages[0].is_streaming);
        assert_eq!(app.streaming_message_id, None);
    }

    fn app() -> App {
        App::new(AppInfo {
            workspace: PathBuf::from("/repo"),
            model: "test".to_string(),
            model_id: "test-model".to_string(),
        })
    }

    #[test]
    fn tool_completion_updates_matching_call_id() {
        let mut app = app();
        app.handle_agent_event(AgentEvent::ToolStarted {
            call_id: "first".to_string(),
            name: "read_file".to_string(),
            arguments: serde_json::json!({"path":"a"}),
        });
        app.handle_agent_event(AgentEvent::ToolStarted {
            call_id: "second".to_string(),
            name: "read_file".to_string(),
            arguments: serde_json::json!({"path":"b"}),
        });

        app.handle_agent_event(AgentEvent::ToolFinished {
            call_id: "first".to_string(),
            name: "read_file".to_string(),
            result: serde_json::json!({"content":"A"}),
            success: true,
        });

        assert!(!app.messages[0].is_streaming);
        assert!(app.messages[1].is_streaming);
    }

    #[test]
    fn busy_agent_rejects_another_submission() {
        let mut app = app();
        app.agent_state = AgentState::Generating;

        let action = app.handle_submission("another task".to_string());

        assert_eq!(action, TuiAction::None);
        assert!(app.status_message.as_deref().unwrap().contains("busy"));
    }

    #[test]
    fn busy_agent_rejects_reset() {
        let mut app = app();
        app.agent_state = AgentState::RunningTool;

        let action = app.handle_submission("/reset".to_string());

        assert_eq!(action, TuiAction::None);
        assert!(app.status_message.as_deref().unwrap().contains("busy"));
    }

    #[test]
    fn status_uses_model_terminology() {
        let mut app = app();

        assert_eq!(
            app.handle_submission("/status".to_string()),
            TuiAction::None
        );

        let status = &app.messages.back().unwrap().content;
        assert!(status.contains("Model: test"));
        assert!(status.contains("Model ID: test-model"));
        assert!(!status.contains("Provider:"));
    }

    #[test]
    fn approval_commands_return_structured_decisions() {
        let mut app = app();
        app.mode = AppMode::Confirming;

        assert_eq!(
            app.handle_submission("/approve".to_string()),
            TuiAction::Approval(ApprovalDecision::Approve)
        );
        assert_eq!(
            app.handle_submission("/modify {\"command\":\"cargo test\"}".to_string()),
            TuiAction::Approval(ApprovalDecision::Modify(
                serde_json::json!({"command":"cargo test"})
            ))
        );
    }

    #[test]
    fn interrupted_stream_keeps_partial_text_and_returns_to_idle() {
        let mut app = app();
        app.handle_agent_event(AgentEvent::TurnStarted);
        app.handle_agent_event(AgentEvent::TextDelta {
            chunk: "partial".to_string(),
            is_final: false,
        });

        app.handle_agent_event(AgentEvent::Error {
            message: "connection closed".to_string(),
            recoverable: true,
        });
        app.handle_agent_event(AgentEvent::TurnCompleted);

        assert_eq!(app.messages[0].content, "partial");
        assert!(!app.messages[0].is_streaming);
        assert_eq!(app.agent_state, AgentState::Idle);
        assert_eq!(app.messages[1].kind, MessageKind::Error);
    }
}
