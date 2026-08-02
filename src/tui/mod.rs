//! Inline terminal host for Noya.

mod app;
mod command;
mod event;
mod markdown;
mod ui;

pub use app::AppInfo;
use app::{App, TuiAction};

use crate::{
    Agent, AgentEvent, ApprovalPrompt, TurnControl,
    session::{CreateSession, SessionFilter, SessionManager, SessionSummary, Transcript},
};
use anyhow::{Context, Result};
use crossterm::terminal;
use ratatui::{Terminal, TerminalOptions, Viewport, backend::CrosstermBackend};
use std::{
    collections::{HashMap, HashSet},
    io,
    ops::Range,
    time::Duration,
};
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

type NoyaTerminal = Terminal<CrosstermBackend<io::Stdout>>;

#[derive(Default)]
struct UiRuntime {
    rendered_message_ids: HashSet<Uuid>,
    streaming_messages: HashMap<Uuid, StreamingMessageState>,
}

#[derive(Debug, Clone, Copy)]
struct StreamingMessageState {
    committed_lines: usize,
    render_width: usize,
}

impl UiRuntime {
    fn clear(&mut self) {
        self.rendered_message_ids.clear();
        self.streaming_messages.clear();
    }

    fn stream_view(&self, app: &App) -> Option<ui::StreamView> {
        let message = app.current_streaming_message()?;
        let state = self.streaming_messages.get(&message.id)?;
        Some(ui::StreamView {
            committed_lines: state.committed_lines,
            render_width: state.render_width,
        })
    }
}

enum AgentCommand {
    Submit(String),
    Reset,
    NewSession,
    ListSessions,
    ResumeSession(String),
    RenameSession(String),
    Retry,
    Compact,
    Cancel,
    Shutdown,
}

enum HostEvent {
    Agent(AgentEvent),
    SessionChanged {
        summary: SessionSummary,
        transcript: Transcript,
        log_path: Option<std::path::PathBuf>,
        context_tokens: usize,
    },
    SessionUpdated {
        summary: SessionSummary,
        log_path: Option<std::path::PathBuf>,
        context_tokens: usize,
    },
    SessionList(Vec<SessionSummary>),
    RetrySubmitted(String),
    Notice(String),
}

struct AgentHost {
    command_tx: mpsc::UnboundedSender<AgentCommand>,
    event_rx: mpsc::UnboundedReceiver<HostEvent>,
    approval_rx: mpsc::UnboundedReceiver<ApprovalPrompt>,
}

pub async fn run(agent: Agent, info: AppInfo) -> Result<()> {
    let mut terminal = init_terminal()?;
    let result = run_loop(&mut terminal, agent, info).await;
    let restore = restore_terminal();
    match (result, restore) {
        (Err(error), _) => Err(error),
        (Ok(()), Err(error)) => Err(error),
        (Ok(()), Ok(())) => Ok(()),
    }
}

fn init_terminal() -> Result<NoyaTerminal> {
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        let _ = restore_terminal();
        previous_hook(panic_info);
    }));
    terminal::enable_raw_mode().context("enable terminal raw mode")?;
    let initialized = (|| {
        let options = TerminalOptions {
            viewport: Viewport::Inline(ui::VIEWPORT_HEIGHT),
        };
        let mut terminal = Terminal::with_options(CrosstermBackend::new(io::stdout()), options)
            .context("initialize terminal")?;
        terminal.clear().context("clear terminal")?;
        Ok(terminal)
    })();
    if initialized.is_err() {
        let _ = restore_terminal();
    }
    initialized
}

pub fn restore_terminal() -> Result<()> {
    if terminal::is_raw_mode_enabled().unwrap_or(false) {
        terminal::disable_raw_mode().context("disable terminal raw mode")?;
    }
    Ok(())
}

async fn run_loop(terminal: &mut NoyaTerminal, agent: Agent, info: AppInfo) -> Result<()> {
    let mut app = App::new(info);
    app.load_session(
        agent.session_summary(),
        agent.transcript(),
        agent.session_log_path(),
        agent.context_token_estimate(),
    );
    let mut ui_runtime = UiRuntime::default();
    flush_unrendered_messages(terminal, &app, &mut ui_runtime)?;
    render_welcome(terminal, &app.info)?;
    let mut input_events = event::EventHandler::new(Duration::from_millis(80));
    let mut host = spawn_agent_host(agent, SessionManager::discover()?);
    let mut approval_response: Option<(String, oneshot::Sender<crate::ApprovalDecision>)> = None;

    loop {
        flush_unrendered_messages(terminal, &app, &mut ui_runtime)?;
        let stream_view = ui_runtime.stream_view(&app);
        terminal.draw(|frame| ui::draw(frame, &app, stream_view))?;

        tokio::select! {
            Some(input_event) = input_events.next() => {
                let action = match input_event {
                    event::Event::Key(key) => event::handle_key_event(key, &mut app),
                    event::Event::Quit => TuiAction::Quit,
                    event::Event::Resize(width, height) => {
                        tracing::debug!(width, height, "terminal resized");
                        TuiAction::None
                    }
                    event::Event::Tick => TuiAction::None,
                };
                apply_action(
                    action,
                    &mut app,
                    &host.command_tx,
                    &mut approval_response,
                    &mut ui_runtime,
                );
            }
            Some(host_event) = host.event_rx.recv() => {
                match host_event {
                    HostEvent::Agent(agent_event) => {
                        if matches!(agent_event, AgentEvent::TurnCompleted { .. }) {
                            approval_response = None;
                        }
                        app.handle_agent_event(agent_event);
                    }
                    HostEvent::SessionChanged { summary, transcript, log_path, context_tokens } => {
                        ui_runtime.clear();
                        app.load_session(summary, transcript, log_path, context_tokens);
                        app.add_message(app::Message::new(app::MessageKind::System, "Session ready."));
                    }
                    HostEvent::SessionUpdated { summary, log_path, context_tokens } => {
                        app.update_session(summary, log_path, context_tokens);
                    }
                    HostEvent::SessionList(summaries) => {
                        app.add_message(app::Message::new(
                            app::MessageKind::System,
                            render_session_list(&summaries),
                        ));
                    }
                    HostEvent::RetrySubmitted(input) => {
                        app.add_message(app::Message::new(app::MessageKind::User, input));
                        app.agent_state = app::AgentState::Thinking;
                    }
                    HostEvent::Notice(message) => {
                        app.add_message(app::Message::new(app::MessageKind::System, message));
                        app.agent_state = app::AgentState::Idle;
                    }
                }
            }
            Some(prompt) = host.approval_rx.recv() => {
                approval_response = Some((prompt.request.request_id.clone(), prompt.respond));
            }
            else => break,
        }

        if app.should_quit {
            let _ = host.command_tx.send(AgentCommand::Shutdown);
            break;
        }
    }
    Ok(())
}

fn apply_action(
    action: TuiAction,
    app: &mut App,
    command_tx: &mpsc::UnboundedSender<AgentCommand>,
    approval_response: &mut Option<(String, oneshot::Sender<crate::ApprovalDecision>)>,
    ui_runtime: &mut UiRuntime,
) {
    match action {
        TuiAction::None => {}
        TuiAction::Clear => ui_runtime.clear(),
        TuiAction::Submit(input) => {
            if command_tx.send(AgentCommand::Submit(input)).is_err() {
                app.handle_agent_event(AgentEvent::Error {
                    turn_id: None,
                    message: "Agent host is unavailable".to_string(),
                    recoverable: false,
                });
            }
        }
        TuiAction::Reset => {
            let _ = command_tx.send(AgentCommand::Reset);
        }
        TuiAction::NewSession => {
            let _ = command_tx.send(AgentCommand::NewSession);
        }
        TuiAction::ListSessions => {
            let _ = command_tx.send(AgentCommand::ListSessions);
        }
        TuiAction::ResumeSession(prefix) => {
            let _ = command_tx.send(AgentCommand::ResumeSession(prefix));
        }
        TuiAction::RenameSession(title) => {
            let _ = command_tx.send(AgentCommand::RenameSession(title));
        }
        TuiAction::Retry => {
            let _ = command_tx.send(AgentCommand::Retry);
        }
        TuiAction::Compact => {
            let _ = command_tx.send(AgentCommand::Compact);
        }
        TuiAction::Cancel => {
            app.status_message = Some("Cancelling current turn…".to_string());
            let _ = command_tx.send(AgentCommand::Cancel);
        }
        TuiAction::Approval(decision) => {
            let Some((request_id, _)) = approval_response.as_ref() else {
                app.status_message = Some("Approval request is not ready yet.".to_string());
                return;
            };
            if app
                .pending_approval
                .as_ref()
                .is_some_and(|pending| pending.request_id != *request_id)
            {
                app.status_message = Some("Approval request changed; please retry.".to_string());
                return;
            }
            let (_, respond) = approval_response.take().expect("approval response exists");
            if respond.send(decision).is_err() {
                app.status_message = Some("Approval request expired.".to_string());
            } else {
                app.mode = app::AppMode::Normal;
                app.pending_approval = None;
                app.agent_state = app::AgentState::Thinking;
            }
        }
        TuiAction::Quit => app.should_quit = true,
    }
}

fn flush_unrendered_messages(
    terminal: &mut NoyaTerminal,
    app: &App,
    runtime: &mut UiRuntime,
) -> Result<()> {
    let terminal_width = usize::from(terminal.size()?.width.max(1));
    runtime
        .streaming_messages
        .retain(|id, _| app.messages.iter().any(|message| message.id == *id));

    for message in &app.messages {
        if runtime.rendered_message_ids.contains(&message.id) {
            continue;
        }

        let (render_width, committed_lines) = if message.is_streaming {
            let state =
                runtime
                    .streaming_messages
                    .entry(message.id)
                    .or_insert(StreamingMessageState {
                        committed_lines: 0,
                        render_width: terminal_width,
                    });
            (state.render_width, state.committed_lines)
        } else if let Some(state) = runtime.streaming_messages.get(&message.id) {
            (state.render_width, state.committed_lines)
        } else {
            (terminal_width, 0)
        };

        let lines = ui::message_lines(message, render_width);
        let range = transcript_line_range(message.is_streaming, committed_lines, lines.len());
        if range.is_empty() {
            if !message.is_streaming {
                runtime.streaming_messages.remove(&message.id);
                runtime.rendered_message_ids.insert(message.id);
            }
            continue;
        }
        let committed_end = range.end;
        let lines = lines[range].to_vec();
        let height = u16::try_from(lines.len()).unwrap_or(u16::MAX);
        terminal.insert_before(height, move |buffer| {
            ui::render_transcript_buffer(lines, buffer);
        })?;

        if message.is_streaming {
            if let Some(state) = runtime.streaming_messages.get_mut(&message.id) {
                state.committed_lines = state.committed_lines.max(committed_end);
            }
        } else {
            runtime.streaming_messages.remove(&message.id);
            runtime.rendered_message_ids.insert(message.id);
        }
    }
    Ok(())
}

fn render_welcome(terminal: &mut NoyaTerminal, info: &AppInfo) -> Result<()> {
    let terminal_width = usize::from(terminal.size()?.width.max(1));
    let lines = ui::welcome_lines(info, terminal_width);
    let height = u16::try_from(lines.len()).unwrap_or(u16::MAX);
    terminal.insert_before(height, move |buffer| {
        ui::render_transcript_buffer(lines, buffer);
    })?;
    Ok(())
}

fn transcript_line_range(
    is_streaming: bool,
    committed_lines: usize,
    total_lines: usize,
) -> Range<usize> {
    let start = committed_lines.min(total_lines);
    // `message_lines` ends with the active content line and a final separator. While a
    // message is streaming, both remain in the inline viewport; every earlier line is stable
    // enough to move into native terminal scrollback immediately.
    let ready_lines = if is_streaming {
        total_lines.saturating_sub(2)
    } else {
        total_lines
    };
    start..ready_lines.max(start).min(total_lines)
}

fn spawn_agent_host(mut agent: Agent, manager: SessionManager) -> AgentHost {
    let (command_tx, mut command_rx) = mpsc::unbounded_channel();
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (approval_tx, approval_rx) = mpsc::unbounded_channel();

    tokio::spawn(async move {
        while let Some(command) = command_rx.recv().await {
            let command = match command {
                AgentCommand::Retry => match agent.retry_input() {
                    Some(input) => {
                        let _ = event_tx.send(HostEvent::RetrySubmitted(input.clone()));
                        AgentCommand::Submit(input)
                    }
                    None => {
                        send_host_error(
                            &event_tx,
                            "No failed, cancelled, or interrupted turn to retry.",
                        );
                        continue;
                    }
                },
                command => command,
            };
            match command {
                AgentCommand::Submit(input) => {
                    let active_workspace = agent.session_summary().workspace;
                    let control = TurnControl::interactive(approval_tx.clone());
                    let active_control = control.clone();
                    let turn_events = event_tx.clone();
                    let mut task = tokio::spawn(async move {
                        let mut completion_event = None;
                        let result = agent
                            .turn_with_control(
                                input,
                                |event| {
                                    if matches!(event, AgentEvent::TurnCompleted { .. }) {
                                        completion_event = Some(event);
                                    } else {
                                        let _ = turn_events.send(HostEvent::Agent(event));
                                    }
                                },
                                control,
                            )
                            .await;
                        let auto_compacted = if result.is_ok() {
                            agent.auto_compact_if_needed().await
                        } else {
                            Ok(false)
                        };
                        if let Some(event) = completion_event {
                            let _ = turn_events.send(HostEvent::Agent(event));
                        }
                        (agent, result, auto_compacted)
                    });

                    loop {
                        tokio::select! {
                            joined = &mut task => {
                                match joined {
                                    Ok((returned_agent, result, auto_compacted)) => {
                                        agent = returned_agent;
                                        if let Err(error) = result {
                                            let _ = event_tx.send(HostEvent::Agent(AgentEvent::Error {
                                                turn_id: None,
                                                message: error.to_string(),
                                                recoverable: true,
                                            }));
                                        }
                                        match auto_compacted {
                                            Ok(true) => {
                                                let _ = event_tx.send(HostEvent::Notice(
                                                    "Session context was compacted automatically."
                                                        .to_string(),
                                                ));
                                            }
                                            Ok(false) => {}
                                            Err(error) => send_host_error(
                                                &event_tx,
                                                &format!("Automatic compaction failed: {error}"),
                                            ),
                                        }
                                        let _ = event_tx.send(session_updated(&agent));
                                    }
                                    Err(error) => {
                                        let _ = event_tx.send(HostEvent::Agent(AgentEvent::Error {
                                            turn_id: None,
                                            message: format!("Agent task failed: {error}"),
                                            recoverable: false,
                                        }));
                                        return;
                                    }
                                }
                                break;
                            }
                            next = command_rx.recv() => match next {
                                Some(AgentCommand::Cancel) => active_control.cancel(),
                                Some(AgentCommand::Shutdown) | None => {
                                    active_control.cancel();
                                    let _ = task.await;
                                    return;
                                }
                                Some(AgentCommand::Submit(_))
                                | Some(AgentCommand::Reset)
                                | Some(AgentCommand::NewSession)
                                | Some(AgentCommand::ResumeSession(_))
                                | Some(AgentCommand::RenameSession(_))
                                | Some(AgentCommand::Retry)
                                | Some(AgentCommand::Compact) => {
                                    send_host_error(&event_tx, "Agent is busy");
                                }
                                Some(AgentCommand::ListSessions) => send_session_list_for_workspace(
                                    &event_tx,
                                    &manager,
                                    active_workspace.clone(),
                                ),
                            }
                        }
                    }
                }
                AgentCommand::Reset => {
                    if let Err(error) = agent.reset() {
                        send_host_error(&event_tx, &format!("Failed to reset context: {error}"));
                    } else {
                        let _ = event_tx.send(HostEvent::Notice("Context reset.".to_string()));
                        let _ = event_tx.send(session_updated(&agent));
                    }
                }
                AgentCommand::NewSession => {
                    let summary = agent.session_summary();
                    match manager
                        .create(CreateSession {
                            workspace: summary.workspace,
                            model: summary.model,
                            model_id: summary.model_id,
                        })
                        .and_then(|session| agent.replace_session(session))
                    {
                        Ok(()) => {
                            let _ = event_tx.send(session_changed(&agent));
                        }
                        Err(error) => send_host_error(
                            &event_tx,
                            &format!("Failed to create session: {error}"),
                        ),
                    }
                }
                AgentCommand::ListSessions => send_session_list(&event_tx, &manager, &agent),
                AgentCommand::ResumeSession(prefix) => {
                    let result = manager
                        .resolve_prefix(&prefix, false)
                        .and_then(|id| manager.open(id))
                        .and_then(|session| agent.replace_session(session));
                    match result {
                        Ok(()) => {
                            let _ = event_tx.send(session_changed(&agent));
                        }
                        Err(error) => send_host_error(
                            &event_tx,
                            &format!("Failed to resume session: {error}"),
                        ),
                    }
                }
                AgentCommand::RenameSession(title) => match agent.rename_session(title) {
                    Ok(()) => {
                        let _ = event_tx.send(session_updated(&agent));
                        let _ = event_tx.send(HostEvent::Notice("Session renamed.".to_string()));
                    }
                    Err(error) => {
                        send_host_error(&event_tx, &format!("Failed to rename session: {error}"))
                    }
                },
                AgentCommand::Compact => match agent.compact().await {
                    Ok(true) => {
                        let _ = event_tx.send(HostEvent::Notice(
                            "Session context compacted; full transcript remains available."
                                .to_string(),
                        ));
                        let _ = event_tx.send(session_updated(&agent));
                    }
                    Ok(false) => {
                        let _ = event_tx.send(HostEvent::Notice(
                                "Nothing to compact; at least four recent completed turns are always retained."
                                    .to_string(),
                            ));
                    }
                    Err(error) => {
                        send_host_error(&event_tx, &format!("Failed to compact session: {error}"))
                    }
                },
                AgentCommand::Retry => unreachable!("retry is normalized before dispatch"),
                AgentCommand::Cancel => {}
                AgentCommand::Shutdown => break,
            }
        }
    });

    AgentHost {
        command_tx,
        event_rx,
        approval_rx,
    }
}

fn session_changed(agent: &Agent) -> HostEvent {
    HostEvent::SessionChanged {
        summary: agent.session_summary(),
        transcript: agent.transcript(),
        log_path: agent.session_log_path(),
        context_tokens: agent.context_token_estimate(),
    }
}

fn session_updated(agent: &Agent) -> HostEvent {
    HostEvent::SessionUpdated {
        summary: agent.session_summary(),
        log_path: agent.session_log_path(),
        context_tokens: agent.context_token_estimate(),
    }
}

fn send_host_error(sender: &mpsc::UnboundedSender<HostEvent>, message: &str) {
    let _ = sender.send(HostEvent::Agent(AgentEvent::Error {
        turn_id: None,
        message: message.to_string(),
        recoverable: true,
    }));
}

fn send_session_list(
    sender: &mpsc::UnboundedSender<HostEvent>,
    manager: &SessionManager,
    agent: &Agent,
) {
    send_session_list_for_workspace(sender, manager, agent.session_summary().workspace);
}

fn send_session_list_for_workspace(
    sender: &mpsc::UnboundedSender<HostEvent>,
    manager: &SessionManager,
    workspace: std::path::PathBuf,
) {
    match manager.list(SessionFilter {
        workspace: Some(workspace),
        include_archived: false,
    }) {
        Ok(summaries) => {
            let _ = sender.send(HostEvent::SessionList(summaries));
        }
        Err(error) => send_host_error(sender, &format!("Failed to list sessions: {error}")),
    }
}

fn render_session_list(summaries: &[SessionSummary]) -> String {
    if summaries.is_empty() {
        return "No sessions found for this workspace.".to_string();
    }
    let mut lines = vec!["Sessions:".to_string()];
    for summary in summaries {
        let id = summary.session_id.to_string();
        lines.push(format!(
            "{}  {:>3} turns  {}",
            &id[..12],
            summary.completed_turns,
            summary.title
        ));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use app::{Message, MessageKind};

    #[test]
    fn streaming_output_commits_completed_lines_before_the_message_finishes() {
        let mut message = Message::new(
            MessageKind::Agent,
            "first line\n\nsecond line\n\nactive line",
        );
        message.is_streaming = true;
        let lines = ui::message_lines(&message, 40);

        let range = transcript_line_range(true, 0, lines.len());

        assert_eq!(range, 0..lines.len().saturating_sub(2));
        assert!(!range.is_empty());
    }

    #[test]
    fn streaming_output_only_appends_new_lines_and_flushes_the_tail_at_completion() {
        assert_eq!(transcript_line_range(true, 0, 7), 0..5);
        assert_eq!(transcript_line_range(true, 5, 9), 5..7);
        assert_eq!(transcript_line_range(false, 7, 10), 7..10);
    }
}
