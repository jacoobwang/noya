use crate::tui::command::{self, SlashCommand};
use crate::{
    AgentEvent, ApprovalDecision, ApprovalRequest,
    session::{SessionSummary, Transcript, TranscriptKind},
};
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
    Clear,
    Reset,
    NewSession,
    ListSessions,
    ResumeSession(String),
    RenameSession(String),
    Retry,
    Compact,
    Cancel,
    Approval(ApprovalDecision),
    Quit,
}

pub struct App {
    pub info: AppInfo,
    pub messages: VecDeque<Message>,
    pub input: String,
    pub cursor_position: usize,
    pub command_selection: usize,
    pub streaming_message_id: Option<Uuid>,
    pub agent_state: AgentState,
    pub mode: AppMode,
    pub pending_approval: Option<ApprovalRequest>,
    pub status_message: Option<String>,
    pub should_quit: bool,
    pub session: Option<SessionSummary>,
    pub session_log_path: Option<PathBuf>,
    pub context_tokens: usize,
    command_menu_dismissed: bool,
}

impl App {
    pub fn new(info: AppInfo) -> Self {
        Self {
            info,
            messages: VecDeque::new(),
            input: String::new(),
            cursor_position: 0,
            command_selection: 0,
            streaming_message_id: None,
            agent_state: AgentState::Idle,
            mode: AppMode::Normal,
            pending_approval: None,
            status_message: None,
            should_quit: false,
            session: None,
            session_log_path: None,
            context_tokens: 0,
            command_menu_dismissed: false,
        }
    }

    pub fn load_session(
        &mut self,
        summary: SessionSummary,
        transcript: Transcript,
        log_path: Option<PathBuf>,
        context_tokens: usize,
    ) {
        self.messages.clear();
        self.streaming_message_id = None;
        self.session = Some(summary);
        self.session_log_path = log_path;
        self.context_tokens = context_tokens;
        self.agent_state = AgentState::Idle;
        self.mode = AppMode::Normal;
        let skipped = transcript.items.len().saturating_sub(200);
        if skipped > 0 {
            self.add_message(Message::new(
                MessageKind::System,
                format!("{skipped} earlier transcript items are available via session export."),
            ));
        }
        for item in transcript.items.into_iter().skip(skipped) {
            let kind = match item.kind {
                TranscriptKind::User => MessageKind::User,
                TranscriptKind::Agent => MessageKind::Agent,
                TranscriptKind::Tool => MessageKind::Tool,
                TranscriptKind::System => MessageKind::System,
                TranscriptKind::Error => MessageKind::Error,
            };
            let mut message = Message::new(kind, item.content);
            message.id = item.id;
            message.tool_call_id = item.tool_call_id;
            self.add_message(message);
        }
    }

    pub fn update_session(
        &mut self,
        summary: SessionSummary,
        log_path: Option<PathBuf>,
        context_tokens: usize,
    ) {
        self.session = Some(summary);
        self.session_log_path = log_path;
        self.context_tokens = context_tokens;
    }

    pub fn handle_agent_event(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::TurnStarted { .. } => {
                self.agent_state = AgentState::Thinking;
                self.status_message = None;
            }
            AgentEvent::TextDelta {
                message_id,
                chunk,
                is_final,
                ..
            } => {
                if self.streaming_message_id != Some(message_id) && !chunk.is_empty() {
                    self.finish_streaming_message();
                    let mut message = Message::streaming(MessageKind::Agent);
                    message.id = message_id;
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
                ..
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
                ..
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
                ..
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
            AgentEvent::TurnCompleted { .. } => {
                self.finish_streaming_message();
                self.agent_state = AgentState::Idle;
                self.mode = AppMode::Normal;
                self.pending_approval = None;
            }
            AgentEvent::Error {
                turn_id,
                message,
                recoverable,
            } => {
                let label = if recoverable {
                    "Recoverable error"
                } else {
                    "Error"
                };
                self.add_message(Message::new(
                    MessageKind::Error,
                    format!("{label}: {message}"),
                ));
                if turn_id.is_none() {
                    self.finish_streaming_message();
                    self.agent_state = AgentState::Error;
                    self.mode = AppMode::Normal;
                    self.pending_approval = None;
                }
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
        self.command_selection = 0;
        self.command_menu_dismissed = false;
        self.handle_submission(input)
    }

    pub fn input_changed(&mut self) {
        self.command_selection = 0;
        self.command_menu_dismissed = false;
    }

    pub fn command_suggestions(&self) -> Vec<&'static SlashCommand> {
        if self.command_menu_dismissed {
            Vec::new()
        } else {
            command::suggestions(&self.input, self.mode)
        }
    }

    pub fn select_next_command(&mut self) -> bool {
        let count = self.command_suggestions().len();
        if count == 0 {
            return false;
        }
        self.command_selection = (self.command_selection + 1) % count;
        true
    }

    pub fn select_previous_command(&mut self) -> bool {
        let count = self.command_suggestions().len();
        if count == 0 {
            return false;
        }
        self.command_selection = (self.command_selection + count - 1) % count;
        true
    }

    pub fn accept_selected_command(&mut self) -> Option<TuiAction> {
        let command = self.selected_command()?;
        let takes_argument = command.argument.is_some();
        self.complete_command(command, takes_argument);
        if takes_argument {
            Some(TuiAction::None)
        } else {
            Some(self.submit_input())
        }
    }

    pub fn complete_selected_command(&mut self) -> bool {
        let Some(command) = self.selected_command() else {
            return false;
        };
        self.complete_command(command, command.argument.is_some());
        self.command_menu_dismissed = true;
        true
    }

    pub fn dismiss_command_menu(&mut self) -> bool {
        if self.command_suggestions().is_empty() {
            false
        } else {
            self.command_menu_dismissed = true;
            true
        }
    }

    fn selected_command(&self) -> Option<&'static SlashCommand> {
        let suggestions = self.command_suggestions();
        suggestions
            .get(
                self.command_selection
                    .min(suggestions.len().saturating_sub(1)),
            )
            .copied()
    }

    fn complete_command(&mut self, command: &SlashCommand, append_space: bool) {
        self.input = command.input();
        if append_space {
            self.input.push(' ');
        }
        self.cursor_position = self.input.len();
        self.command_selection = 0;
        self.command_menu_dismissed = append_space;
    }

    pub fn handle_submission(&mut self, input: String) -> TuiAction {
        match input.as_str() {
            "/help" => {
                self.add_message(Message::new(
                    MessageKind::System,
                    "Commands: /help /new /sessions /resume <id> /rename <title> /retry /compact /clear /reset /status /cancel /quit",
                ));
                TuiAction::None
            }
            "/clear" => {
                self.messages.clear();
                self.streaming_message_id = None;
                TuiAction::Clear
            }
            "/reset" if self.agent_state == AgentState::Idle => TuiAction::Reset,
            "/reset" => {
                self.status_message = Some("Agent is busy; cancel before resetting.".to_string());
                TuiAction::None
            }
            "/new" if self.agent_state == AgentState::Idle => {
                self.agent_state = AgentState::Thinking;
                TuiAction::NewSession
            }
            "/new" => {
                self.status_message =
                    Some("Agent is busy; cancel before switching sessions.".to_string());
                TuiAction::None
            }
            "/sessions" => TuiAction::ListSessions,
            "/retry" if self.agent_state == AgentState::Idle => {
                self.agent_state = AgentState::Thinking;
                TuiAction::Retry
            }
            "/retry" => {
                self.status_message = Some("Agent is busy; wait or use /cancel.".to_string());
                TuiAction::None
            }
            "/compact" if self.agent_state == AgentState::Idle => {
                self.agent_state = AgentState::Thinking;
                TuiAction::Compact
            }
            "/compact" => {
                self.status_message = Some("Agent is busy; cancel before compacting.".to_string());
                TuiAction::None
            }
            "/status" => {
                let session = self.session.as_ref();
                self.add_message(Message::new(
                    MessageKind::System,
                    format!(
                        "Session: {}\nTitle: {}\nLog: {}\nWorkspace: {}\nModel: {}\nModel ID: {}\nCompleted turns: {}\nContext epoch: {}\nEstimated context tokens: {}\nCompaction cutoff: {}\nState: {:?}",
                        session.map(|value| value.session_id.to_string()).unwrap_or_else(|| "ephemeral".to_string()),
                        session.map(|value| value.title.as_str()).unwrap_or("New session"),
                        self.session_log_path.as_ref().map_or_else(|| "ephemeral".to_string(), |path| path.display().to_string()),
                        self.info.workspace.display(),
                        self.info.model,
                        self.info.model_id,
                        session.map_or(0, |value| value.completed_turns),
                        session.map_or(0, |value| value.context_epoch),
                        self.context_tokens,
                        session.and_then(|value| value.compaction_through_seq).map_or_else(|| "none".to_string(), |seq| seq.to_string()),
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
            _ if input.starts_with("/resume ") && self.agent_state == AgentState::Idle => {
                let prefix = input[8..].trim();
                if prefix.is_empty() {
                    self.status_message = Some("Usage: /resume <session-id-prefix>".to_string());
                    TuiAction::None
                } else {
                    self.agent_state = AgentState::Thinking;
                    TuiAction::ResumeSession(prefix.to_string())
                }
            }
            _ if input.starts_with("/rename ") && self.agent_state == AgentState::Idle => {
                TuiAction::RenameSession(input[8..].trim().to_string())
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
    use crate::{AgentEvent, session::TurnId};
    use std::path::PathBuf;

    fn text_delta(
        turn_id: TurnId,
        message_id: Uuid,
        chunk: impl Into<String>,
        is_final: bool,
    ) -> AgentEvent {
        AgentEvent::TextDelta {
            turn_id,
            message_id,
            chunk: chunk.into(),
            is_final,
        }
    }

    #[test]
    fn text_deltas_update_one_streaming_message() {
        let mut app = App::new(AppInfo {
            workspace: PathBuf::from("/repo"),
            model: "test".to_string(),
            model_id: "test-model".to_string(),
        });

        let turn_id = TurnId::new();
        let message_id = Uuid::new_v4();
        app.handle_agent_event(AgentEvent::TurnStarted { turn_id });
        app.handle_agent_event(text_delta(turn_id, message_id, "Hel", false));
        app.handle_agent_event(text_delta(turn_id, message_id, "lo", false));

        assert_eq!(app.messages.len(), 1);
        assert_eq!(app.messages[0].content, "Hello");
        assert!(app.messages[0].is_streaming);
        assert_eq!(app.agent_state, AgentState::Generating);

        app.handle_agent_event(text_delta(turn_id, message_id, "", true));

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
        let turn_id = TurnId::new();
        app.handle_agent_event(AgentEvent::ToolStarted {
            turn_id,
            call_id: "first".to_string(),
            name: "read_file".to_string(),
            arguments: serde_json::json!({"path":"a"}),
        });
        app.handle_agent_event(AgentEvent::ToolStarted {
            turn_id,
            call_id: "second".to_string(),
            name: "read_file".to_string(),
            arguments: serde_json::json!({"path":"b"}),
        });

        app.handle_agent_event(AgentEvent::ToolFinished {
            turn_id,
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
        let turn_id = TurnId::new();
        let message_id = Uuid::new_v4();
        app.handle_agent_event(AgentEvent::TurnStarted { turn_id });
        app.handle_agent_event(text_delta(turn_id, message_id, "partial", false));

        app.handle_agent_event(AgentEvent::Error {
            turn_id: None,
            message: "connection closed".to_string(),
            recoverable: true,
        });
        app.handle_agent_event(AgentEvent::TurnCompleted { turn_id });

        assert_eq!(app.messages[0].content, "partial");
        assert!(!app.messages[0].is_streaming);
        assert_eq!(app.agent_state, AgentState::Idle);
        assert_eq!(app.messages[1].kind, MessageKind::Error);
    }

    #[test]
    fn session_commands_map_to_lifecycle_actions() {
        let mut app = app();

        assert_eq!(
            app.handle_submission("/new".to_string()),
            TuiAction::NewSession
        );
        app.agent_state = AgentState::Idle;
        assert_eq!(
            app.handle_submission("/sessions".to_string()),
            TuiAction::ListSessions
        );
        assert_eq!(
            app.handle_submission("/resume 019fbd63".to_string()),
            TuiAction::ResumeSession("019fbd63".to_string())
        );
        app.agent_state = AgentState::Idle;
        assert_eq!(
            app.handle_submission("/rename Durable work".to_string()),
            TuiAction::RenameSession("Durable work".to_string())
        );
        assert_eq!(
            app.handle_submission("/retry".to_string()),
            TuiAction::Retry
        );
        app.agent_state = AgentState::Idle;
        assert_eq!(
            app.handle_submission("/compact".to_string()),
            TuiAction::Compact
        );
        app.agent_state = AgentState::Idle;
        assert_eq!(
            app.handle_submission("/clear".to_string()),
            TuiAction::Clear
        );
    }
}
