//! Inline terminal host for Noya.

mod app;
mod command;
mod event;
mod markdown;
mod theme;
mod ui;

pub use app::AppInfo;
use app::{App, ModelChoice, TuiAction};

use crate::{
    Agent, AgentConfig, AgentEvent, ApprovalPrompt, LlmClient, TurnControl,
    model::{
        AuthenticationMode, CredentialStore, Model, ModelCatalogStore, ModelOverrides, ModelStatus,
        ProviderProtocol, RuntimeModelConfig,
    },
    session::{CreateSession, SessionFilter, SessionManager, SessionSummary, Transcript},
};
use anyhow::{Context, Result, bail, ensure};
use crossterm::terminal;
use ratatui::{Terminal, TerminalOptions, Viewport, backend::CrosstermBackend};
use std::{
    collections::{HashMap, HashSet},
    io,
    ops::Range,
    path::PathBuf,
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
    ListModels,
    ListSkills,
    ActivateSkill(String),
    DeactivateSkill(String),
    ShowSkill(String),
    SwitchModel(String),
    SwitchModelTo {
        model: String,
        model_id: String,
    },
    FetchModelChoices {
        model: String,
        base_url: String,
        api_key: String,
        protocol: ProviderProtocol,
        authentication: AuthenticationMode,
    },
    CatalogDiscovered {
        provider: Model,
        base_url: String,
        model_ids: Vec<String>,
    },
    ConfigureModel {
        model: String,
        base_url: String,
        api_key: String,
        model_id: String,
        protocol: ProviderProtocol,
        authentication: AuthenticationMode,
    },
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
    ModelChoices(Vec<ModelChoice>),
    ModelSetupRequired {
        model: Model,
        base_url: String,
    },
    ModelSetupChoices {
        model: String,
        base_url: String,
        api_key: String,
        model_ids: Vec<String>,
        protocol: ProviderProtocol,
        authentication: AuthenticationMode,
    },
    RetrySubmitted(String),
    Notice(String),
    CommandFailed(String),
}

struct AgentHost {
    command_tx: mpsc::UnboundedSender<AgentCommand>,
    event_rx: mpsc::UnboundedReceiver<HostEvent>,
    approval_rx: mpsc::UnboundedReceiver<ApprovalPrompt>,
    config: AgentConfig,
    llm: LlmClient,
}

struct WorkerHost {
    host: AgentHost,
    status: WorkerStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkerStatus {
    Idle,
    Running,
    WaitingApproval,
    Error,
}

struct PendingWorkerApproval {
    request: crate::ApprovalRequest,
    respond: oneshot::Sender<crate::ApprovalDecision>,
}

const DEFAULT_MAX_WORKERS: usize = 4;
const MAX_PROJECT_HISTORY: usize = 10;

pub async fn run(agent: Agent, info: AppInfo, max_workers: usize) -> Result<()> {
    let mut terminal = init_terminal()?;
    let result = run_loop(
        &mut terminal,
        agent,
        info,
        if max_workers == 0 {
            DEFAULT_MAX_WORKERS
        } else {
            max_workers
        },
    )
    .await;
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

async fn run_loop(
    terminal: &mut NoyaTerminal,
    agent: Agent,
    info: AppInfo,
    max_workers: usize,
) -> Result<()> {
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
    let manager = SessionManager::discover()?;
    let model_store = CredentialStore::discover()?;
    let catalog_store = ModelCatalogStore::discover()?;
    let initial_workspace = app.info.workspace.clone();
    let mut workers = HashMap::from([(
        initial_workspace.clone(),
        WorkerHost {
            host: spawn_agent_host(
                agent,
                manager.clone(),
                model_store.clone(),
                catalog_store.clone(),
            ),
            status: WorkerStatus::Idle,
        },
    )]);
    let mut active_workspace = initial_workspace;
    let mut pending_approvals: HashMap<PathBuf, PendingWorkerApproval> = HashMap::new();
    let mut approval_response: Option<(String, oneshot::Sender<crate::ApprovalDecision>)> = None;

    loop {
        drain_worker_events(
            &mut workers,
            &mut app,
            &active_workspace,
            &mut pending_approvals,
            &mut approval_response,
        );
        flush_unrendered_messages(terminal, &app, &mut ui_runtime)?;
        let stream_view = ui_runtime.stream_view(&app);
        terminal.draw(|frame| ui::draw(frame, &app, stream_view))?;

        let Some(input_event) = input_events.next().await else {
            break;
        };
        let action = match input_event {
            event::Event::Key(key) => event::handle_key_event(key, &mut app),
            event::Event::Quit => TuiAction::Quit,
            event::Event::Resize(width, height) => {
                tracing::debug!(width, height, "terminal resized");
                TuiAction::None
            }
            event::Event::Tick => TuiAction::None,
        };
        match action {
            TuiAction::ListProjects => {
                let message = render_project_list(
                    &manager,
                    &workers,
                    &active_workspace,
                );
                app.add_message(app::Message::new(app::MessageKind::System, message));
            }
            TuiAction::SwitchProject(target) => {
                if let Err(error) = switch_project(
                    &target,
                    &manager,
                    &model_store,
                    &catalog_store,
                    &mut workers,
                    &mut active_workspace,
                    &mut app,
                    &mut pending_approvals,
                    &mut approval_response,
                    &mut ui_runtime,
                    max_workers,
                ) {
                    app.add_message(app::Message::new(
                        app::MessageKind::Error,
                        format!("Failed to switch project: {error}"),
                    ));
                    app.agent_state = app::AgentState::Idle;
                }
            }
            other => {
                let command_tx = workers
                    .get(&active_workspace)
                    .map(|worker| &worker.host.command_tx);
                apply_action(
                    other,
                    &mut app,
                    command_tx,
                    &mut approval_response,
                    &mut ui_runtime,
                );
            }
        }

        if app.should_quit {
            for worker in workers.values() {
                let _ = worker.host.command_tx.send(AgentCommand::Shutdown);
            }
            break;
        }
    }
    Ok(())
}

fn apply_action(
    action: TuiAction,
    app: &mut App,
    command_tx: Option<&mpsc::UnboundedSender<AgentCommand>>,
    approval_response: &mut Option<(String, oneshot::Sender<crate::ApprovalDecision>)>,
    ui_runtime: &mut UiRuntime,
) {
    let send = |command| command_tx.is_some_and(|sender| sender.send(command).is_ok());
    match action {
        TuiAction::None => {}
        TuiAction::Clear => ui_runtime.clear(),
        TuiAction::Submit(input) => {
            if !send(AgentCommand::Submit(input)) {
                app.handle_agent_event(AgentEvent::Error {
                    turn_id: None,
                    message: "Agent host is unavailable".to_string(),
                    recoverable: false,
                });
            }
        }
        TuiAction::Reset => {
            let _ = send(AgentCommand::Reset);
        }
        TuiAction::NewSession => {
            let _ = send(AgentCommand::NewSession);
        }
        TuiAction::ListModels => {
            let _ = send(AgentCommand::ListModels);
        }
        TuiAction::ListSkills => {
            let _ = send(AgentCommand::ListSkills);
        }
        TuiAction::ActivateSkill(name) => {
            let _ = send(AgentCommand::ActivateSkill(name));
        }
        TuiAction::DeactivateSkill(name) => {
            let _ = send(AgentCommand::DeactivateSkill(name));
        }
        TuiAction::ShowSkill(name) => {
            let _ = send(AgentCommand::ShowSkill(name));
        }
        TuiAction::SwitchModel(model) => {
            let _ = send(AgentCommand::SwitchModel(model));
        }
        TuiAction::SwitchModelTo { model, model_id } => {
            let _ = send(AgentCommand::SwitchModelTo { model, model_id });
        }
        TuiAction::FetchModelChoices {
            model,
            base_url,
            api_key,
            protocol,
            authentication,
        } => {
            let _ = send(AgentCommand::FetchModelChoices {
                model,
                base_url,
                api_key,
                protocol,
                authentication,
            });
        }
        TuiAction::ConfigureModel {
            model,
            base_url,
            api_key,
            model_id,
            protocol,
            authentication,
        } => {
            let _ = send(AgentCommand::ConfigureModel {
                model,
                base_url,
                api_key,
                model_id,
                protocol,
                authentication,
            });
        }
        TuiAction::ListSessions => {
            let _ = send(AgentCommand::ListSessions);
        }
        TuiAction::ResumeSession(prefix) => {
            let _ = send(AgentCommand::ResumeSession(prefix));
        }
        TuiAction::RenameSession(title) => {
            let _ = send(AgentCommand::RenameSession(title));
        }
        TuiAction::Retry => {
            let _ = send(AgentCommand::Retry);
        }
        TuiAction::Compact => {
            let _ = send(AgentCommand::Compact);
        }
        TuiAction::Cancel => {
            app.status_message = Some("Cancelling current turn…".to_string());
            let _ = send(AgentCommand::Cancel);
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
        TuiAction::ListProjects | TuiAction::SwitchProject(_) => unreachable!("handled by run loop"),
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

fn spawn_agent_host(
    mut agent: Agent,
    manager: SessionManager,
    model_store: CredentialStore,
    catalog_store: ModelCatalogStore,
) -> AgentHost {
    let config = agent.config();
    let llm = agent.llm_client();
    let (command_tx, mut command_rx) = mpsc::unbounded_channel();
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (approval_tx, approval_rx) = mpsc::unbounded_channel();

    let discovery_tx = command_tx.clone();
    let deferred_tx = command_tx.clone();
    let discovery_store = model_store.clone();
    tokio::spawn(async move {
        for provider in Model::all().iter().copied() {
            let Ok(config) = RuntimeModelConfig::resolve(
                ModelOverrides {
                    model: Some(provider),
                    ..ModelOverrides::default()
                },
                &discovery_store,
            ) else {
                continue;
            };
            if config.api_key.trim().is_empty() || config.base_url.trim().is_empty() {
                continue;
            }
            let client = LlmClient::with_settings(
                reqwest::Client::new(),
                config.base_url.clone(),
                config.api_key,
                config.model_id,
                config.protocol,
                config.authentication,
            );
            match client.list_models().await {
                Ok(model_ids) => {
                    let _ = discovery_tx.send(AgentCommand::CatalogDiscovered {
                        provider,
                        base_url: config.base_url,
                        model_ids,
                    });
                }
                Err(error) => {
                    tracing::debug!(
                        ?provider,
                        ?error,
                        "model discovery failed; using cached catalog"
                    );
                }
            }
        }
    });

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
                    let active_summary = agent.session_summary();
                    let active_workspace = active_summary.workspace.clone();
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
                                | Some(AgentCommand::SwitchModel(_))
                                | Some(AgentCommand::SwitchModelTo { .. })
                                | Some(AgentCommand::ActivateSkill(_))
                                | Some(AgentCommand::DeactivateSkill(_))
                                | Some(AgentCommand::ShowSkill(_))
                                | Some(AgentCommand::ListSkills)
                                | Some(AgentCommand::FetchModelChoices { .. })
                                | Some(AgentCommand::ConfigureModel { .. })
                                | Some(AgentCommand::ResumeSession(_))
                                | Some(AgentCommand::RenameSession(_))
                                | Some(AgentCommand::Retry)
                                | Some(AgentCommand::Compact) => {
                                    send_host_error(&event_tx, "Agent is busy");
                                }
                                Some(AgentCommand::CatalogDiscovered {
                                    provider,
                                    base_url,
                                    model_ids,
                                }) => {
                                    let _ = deferred_tx.send(AgentCommand::CatalogDiscovered {
                                        provider,
                                        base_url,
                                        model_ids,
                                    });
                                }
                                Some(AgentCommand::ListSessions) => send_session_list_for_workspace(
                                    &event_tx,
                                    &manager,
                                    active_workspace.clone(),
                                ),
                                Some(AgentCommand::ListModels) => {
                                    send_model_choices(&event_tx, &model_store, &catalog_store, &active_summary)
                                }
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
                AgentCommand::ListModels => {
                    send_model_choices(
                        &event_tx,
                        &model_store,
                        &catalog_store,
                        &agent.session_summary(),
                    );
                }
                AgentCommand::ListSkills => send_skill_list(&event_tx, &agent),
                AgentCommand::ActivateSkill(name) => match agent.activate_skill(&name) {
                    Ok(info) => {
                        let _ = event_tx.send(HostEvent::Notice(format!(
                            "Activated Skill '{}' ({}, {}).",
                            info.name, info.source, info.digest
                        )));
                        let _ = event_tx.send(session_updated(&agent));
                    }
                    Err(error) => send_host_error(
                        &event_tx,
                        &format!("Failed to activate Skill '{name}': {error}"),
                    ),
                },
                AgentCommand::DeactivateSkill(name) => match agent.deactivate_skill(&name) {
                    Ok(()) => {
                        let _ = event_tx.send(HostEvent::Notice(format!(
                            "Deactivated Skill '{name}'."
                        )));
                        let _ = event_tx.send(session_updated(&agent));
                    }
                    Err(error) => send_host_error(
                        &event_tx,
                        &format!("Failed to deactivate Skill '{name}': {error}"),
                    ),
                },
                AgentCommand::ShowSkill(name) => match agent.skill_info(&name) {
                    Ok(info) => {
                        let active = agent
                            .active_skills()
                            .iter()
                            .any(|skill| skill.name == info.name);
                        let _ = event_tx.send(HostEvent::Notice(format!(
                            "Skill: {}\nDescription: {}\nSource: {}\nPath: {}\nDigest: {}\nModel invocation disabled: {}\nActive: {}",
                            info.name,
                            info.description,
                            info.source,
                            info.path.display(),
                            info.digest,
                            info.disable_model_invocation,
                            active
                        )));
                    }
                    Err(error) => send_host_error(
                        &event_tx,
                        &format!("Failed to show Skill '{name}': {error}"),
                    ),
                },
                AgentCommand::FetchModelChoices {
                    model,
                    base_url,
                    api_key,
                    protocol,
                    authentication,
                } => {
                    let result = async {
                        let provider = model.parse::<Model>().map_err(anyhow::Error::msg)?;
                        let client = LlmClient::with_settings(
                            reqwest::Client::new(),
                            base_url.clone(),
                            api_key.clone(),
                            "",
                            protocol,
                            authentication,
                        );
                        let model_ids = client.list_models().await?;
                        catalog_store.save(provider, &base_url, model_ids.clone())?;
                        Ok::<_, anyhow::Error>((provider, model_ids))
                    }
                    .await;
                    match result {
                        Ok((provider, model_ids)) => {
                            let _ = event_tx.send(HostEvent::ModelSetupChoices {
                                model: provider.id().to_string(),
                                base_url,
                                api_key,
                                model_ids,
                                protocol,
                                authentication,
                            });
                        }
                        Err(error) => {
                            let _ = event_tx.send(HostEvent::CommandFailed(format!(
                                "Failed to discover models: {error}"
                            )));
                        }
                    }
                }
                AgentCommand::CatalogDiscovered {
                    provider,
                    base_url,
                    model_ids,
                } => match apply_catalog_discovery(
                    &mut agent,
                    &model_store,
                    &catalog_store,
                    provider,
                    &base_url,
                    model_ids,
                ) {
                    Ok(true) => {
                        let _ = event_tx.send(session_updated(&agent));
                    }
                    Ok(false) => {}
                    Err(error) => tracing::warn!(
                        ?provider,
                        ?error,
                        "failed to apply discovered model catalog"
                    ),
                },
                AgentCommand::SwitchModel(name) => {
                    let requested = match name.parse::<Model>() {
                        Ok(model) => model,
                        Err(error) => {
                            let _ = event_tx.send(HostEvent::CommandFailed(format!(
                                "Failed to switch model: {error}"
                            )));
                            continue;
                        }
                    };
                    match RuntimeModelConfig::resolve(
                        ModelOverrides {
                            model: Some(requested),
                            ..ModelOverrides::default()
                        },
                        &model_store,
                    ) {
                        Ok(config) if config.api_key.is_empty() => {
                            let _ = event_tx.send(HostEvent::ModelSetupRequired {
                                model: requested,
                                base_url: config.base_url,
                            });
                            continue;
                        }
                        Err(error) => {
                            let _ = event_tx.send(HostEvent::CommandFailed(format!(
                                "Failed to switch model: {error}"
                            )));
                            continue;
                        }
                        Ok(_) => {}
                    }
                    match switch_model(&mut agent, &model_store, &name) {
                        Ok(message) => {
                            let _ = event_tx.send(session_updated(&agent));
                            let _ = event_tx.send(HostEvent::Notice(message));
                        }
                        Err(error) => {
                            let _ = event_tx.send(HostEvent::CommandFailed(format!(
                                "Failed to switch model: {error}"
                            )));
                        }
                    }
                }
                AgentCommand::SwitchModelTo { model, model_id } => {
                    let result = switch_model_to(&mut agent, &model_store, &model, &model_id);
                    match result {
                        Ok(message) => {
                            let _ = event_tx.send(session_updated(&agent));
                            let _ = event_tx.send(HostEvent::Notice(message));
                        }
                        Err(error) => {
                            let _ = event_tx.send(HostEvent::CommandFailed(format!(
                                "Failed to switch model: {error}"
                            )));
                        }
                    }
                }
                AgentCommand::ConfigureModel {
                    model,
                    base_url,
                    api_key,
                    model_id,
                    protocol,
                    authentication,
                } => {
                    let result = (|| -> Result<String> {
                        let model = model.parse::<Model>().map_err(anyhow::Error::msg)?;
                        model_store.login_with_config(
                            model,
                            &api_key,
                            Some(&base_url),
                            protocol,
                            authentication,
                        )?;
                        model_store.set_model_id(model, &model_id)?;
                        let config = RuntimeModelConfig::resolve(
                            ModelOverrides {
                                model: Some(model),
                                model_id: Some(model_id),
                                ..ModelOverrides::default()
                            },
                            &model_store,
                        )?;
                        let llm = LlmClient::with_settings(
                            reqwest::Client::new(),
                            config.base_url.clone(),
                            config.api_key.clone(),
                            config.model_id.clone(),
                            config.protocol,
                            config.authentication,
                        )
                        .with_custom_temperature(config.model.supports_custom_temperature());
                        agent.switch_model(config.model.to_string(), llm)?;
                        Ok(format!(
                            "Configured and switched to {} ({}).",
                            config.model, config.model_id
                        ))
                    })();
                    match result {
                        Ok(message) => {
                            let _ = event_tx.send(session_updated(&agent));
                            let _ = event_tx.send(HostEvent::Notice(message));
                        }
                        Err(error) => {
                            let _ = event_tx.send(HostEvent::CommandFailed(format!(
                                "Failed to configure model: {error}"
                            )));
                        }
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
        config,
        llm,
    }
}

fn drain_worker_events(
    workers: &mut HashMap<PathBuf, WorkerHost>,
    app: &mut App,
    active_workspace: &PathBuf,
    pending_approvals: &mut HashMap<PathBuf, PendingWorkerApproval>,
    approval_response: &mut Option<(String, oneshot::Sender<crate::ApprovalDecision>)>,
) {
    let workspaces = workers.keys().cloned().collect::<Vec<_>>();
    let mut events = Vec::new();
    let mut approvals = Vec::new();
    for workspace in workspaces {
        let Some(worker) = workers.get_mut(&workspace) else {
            continue;
        };
        while let Ok(event) = worker.host.event_rx.try_recv() {
            events.push((workspace.clone(), event));
        }
        while let Ok(prompt) = worker.host.approval_rx.try_recv() {
            approvals.push((workspace.clone(), prompt));
        }
    }

    for (workspace, event) in events {
        let is_active = &workspace == active_workspace;
        if let Some(worker) = workers.get_mut(&workspace) {
            match &event {
                HostEvent::Agent(AgentEvent::TurnStarted { .. }) => {
                    worker.status = WorkerStatus::Running;
                }
                HostEvent::Agent(AgentEvent::ApprovalRequired { .. }) => {
                    worker.status = WorkerStatus::WaitingApproval;
                }
                HostEvent::Agent(AgentEvent::TurnCompleted { .. }) => {
                    worker.status = WorkerStatus::Idle;
                }
                HostEvent::Agent(AgentEvent::Error { recoverable, .. }) => {
                    worker.status = if *recoverable {
                        WorkerStatus::Idle
                    } else {
                        WorkerStatus::Error
                    };
                }
                HostEvent::SessionChanged { .. }
                | HostEvent::SessionUpdated { .. }
                | HostEvent::SessionList(_)
                | HostEvent::ModelChoices(_)
                | HostEvent::ModelSetupRequired { .. }
                | HostEvent::ModelSetupChoices { .. }
                | HostEvent::RetrySubmitted(_)
                | HostEvent::Notice(_)
                | HostEvent::CommandFailed(_)
                | HostEvent::Agent(AgentEvent::TextDelta { .. })
                | HostEvent::Agent(AgentEvent::ToolStarted { .. })
                | HostEvent::Agent(AgentEvent::ToolFinished { .. })
                | HostEvent::Agent(AgentEvent::DiagnosticsUpdated { .. }) => {}
            }
        }

        match event {
            HostEvent::Agent(agent_event) if is_active => {
                if matches!(agent_event, AgentEvent::TurnCompleted { .. }) {
                    *approval_response = None;
                }
                app.handle_agent_event(agent_event);
            }
            HostEvent::Agent(AgentEvent::Error { .. }) => {
                if !is_active {
                    app.notify_background_worker();
                }
            }
            HostEvent::Agent(AgentEvent::TurnCompleted { .. }) => {
                if !is_active {
                    app.notify_background_worker();
                }
            }
            HostEvent::SessionChanged {
                summary,
                transcript,
                log_path,
                context_tokens,
            } if is_active => {
                app.load_session(summary, transcript, log_path, context_tokens);
                app.add_message(app::Message::new(
                    app::MessageKind::System,
                    "Session ready.",
                ));
            }
            HostEvent::SessionUpdated {
                summary,
                log_path,
                context_tokens,
            } if is_active => app.update_session(summary, log_path, context_tokens),
            HostEvent::SessionList(summaries) if is_active => app.add_message(app::Message::new(
                app::MessageKind::System,
                render_session_list(&summaries),
            )),
            HostEvent::ModelChoices(choices) if is_active => app.open_model_menu(choices),
            HostEvent::ModelSetupRequired { model, base_url } if is_active => {
                app.begin_model_setup(model.id().to_string(), base_url);
            }
            HostEvent::ModelSetupChoices {
                model,
                base_url,
                api_key,
                model_ids,
                protocol,
                authentication,
            } if is_active => app.begin_model_selection(
                model,
                base_url,
                api_key,
                model_ids,
                protocol,
                authentication,
            ),
            HostEvent::RetrySubmitted(input) if is_active => {
                app.add_message(app::Message::new(app::MessageKind::User, input));
                app.agent_state = app::AgentState::Thinking;
            }
            HostEvent::Notice(message) if is_active => {
                app.add_message(app::Message::new(app::MessageKind::System, message));
                app.agent_state = app::AgentState::Idle;
            }
            HostEvent::CommandFailed(message) if is_active => {
                if app.mode == app::AppMode::ConfiguringModel {
                    app.cancel_model_setup();
                }
                app.add_message(app::Message::new(app::MessageKind::Error, message));
                app.agent_state = app::AgentState::Idle;
            }
            HostEvent::Notice(_) | HostEvent::CommandFailed(_) if !is_active => {
                app.notify_background_worker();
            }
            _ => {}
        }
    }

    for (workspace, prompt) in approvals {
        let request_id = prompt.request.request_id.clone();
        if &workspace == active_workspace {
            *approval_response = Some((request_id, prompt.respond));
        } else {
            pending_approvals.insert(
                workspace,
                PendingWorkerApproval {
                    request: prompt.request,
                    respond: prompt.respond,
                },
            );
            app.notify_background_worker();
        }
    }
}

fn switch_project(
    target: &str,
    manager: &SessionManager,
    model_store: &CredentialStore,
    catalog_store: &ModelCatalogStore,
    workers: &mut HashMap<PathBuf, WorkerHost>,
    active_workspace: &mut PathBuf,
    app: &mut App,
    pending_approvals: &mut HashMap<PathBuf, PendingWorkerApproval>,
    approval_response: &mut Option<(String, oneshot::Sender<crate::ApprovalDecision>)>,
    ui_runtime: &mut UiRuntime,
    max_workers: usize,
) -> Result<()> {
    let workspace = resolve_project_target(manager, target)?;
    if workspace == *active_workspace {
        app.agent_state = agent_state_for_worker(
            workers.get(&workspace).map(|worker| worker.status),
        );
        return Ok(());
    }

    if approval_response.is_some() {
        if let Some(request) = app.pending_approval.take() {
            if let Some((_, respond)) = approval_response.take() {
                pending_approvals.insert(
                    active_workspace.clone(),
                    PendingWorkerApproval {
                        request,
                        respond,
                    },
                );
            }
        }
    }

    if !workers.contains_key(&workspace) {
        ensure_worker_capacity(workers.len(), max_workers)?;
        let base_config = workers
            .get(active_workspace)
            .context("active Worker is unavailable")?
            .host
            .config
            .clone();
        let seed_llm = workers
            .get(active_workspace)
            .context("active Worker is unavailable")?
            .host
            .llm
            .clone();
        let worker_agent = build_worker_agent(
            manager,
            model_store,
            base_config,
            workspace.clone(),
            app.info.model.clone(),
            app.info.model_id.clone(),
            seed_llm,
        )?;
        let host = spawn_agent_host(
            worker_agent,
            manager.clone(),
            model_store.clone(),
            catalog_store.clone(),
        );
        workers.insert(
            workspace.clone(),
            WorkerHost {
                host,
                status: WorkerStatus::Idle,
            },
        );
    }

    let summary = manager
        .latest(&workspace)?
        .context("project has no resumable session")?;
    let snapshot = manager.show(summary.session_id)?;
    let log_path = manager.log_path(summary.session_id).ok();
    *active_workspace = workspace.clone();
    ui_runtime.clear();
    app.clear_background_notifications();
    app.load_session(snapshot.summary, snapshot.transcript, log_path, 0);
    app.add_message(app::Message::new(
        app::MessageKind::System,
        format!("Switched to project: {}", workspace.display()),
    ));
    if let Some(pending) = pending_approvals.remove(&workspace) {
        let request = pending.request.clone();
        *approval_response = Some((request.request_id.clone(), pending.respond));
        app.handle_agent_event(AgentEvent::ApprovalRequired {
            turn_id: crate::session::TurnId::new(),
            request_id: request.request_id,
            call_id: request.call_id,
            tool_name: request.tool_name,
            arguments: request.arguments,
        });
    }
    app.agent_state = agent_state_for_worker(
        workers.get(&workspace).map(|worker| worker.status),
    );
    Ok(())
}

fn resolve_project_target(manager: &SessionManager, target: &str) -> Result<PathBuf> {
    if let Ok(index) = target.parse::<usize>() {
        ensure!(index > 0, "project index starts at 1");
        let projects = project_summaries(manager)?;
        return projects
            .get(index - 1)
            .map(|summary| summary.workspace.clone())
            .with_context(|| format!("project index {index} is not available"));
    }
    let path = if let Some(rest) = target.strip_prefix("~/") {
        dirs::home_dir()
            .context("cannot determine the user home directory")?
            .join(rest)
    } else {
        PathBuf::from(target)
    };
    ensure!(path.is_dir(), "project path is not an accessible directory: {}", path.display());
    path.canonicalize()
        .with_context(|| format!("canonicalize project path {}", path.display()))
}

fn project_summaries(manager: &SessionManager) -> Result<Vec<SessionSummary>> {
    let mut by_workspace = HashMap::<PathBuf, SessionSummary>::new();
    for summary in manager.list(SessionFilter {
        workspace: None,
        include_archived: false,
    })? {
        by_workspace
            .entry(summary.workspace.clone())
            .and_modify(|current| {
                if summary.updated_at > current.updated_at {
                    *current = summary.clone();
                }
            })
            .or_insert(summary);
    }
    let mut summaries = by_workspace.into_values().collect::<Vec<_>>();
    summaries.sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
    summaries.truncate(MAX_PROJECT_HISTORY);
    Ok(summaries)
}

fn render_project_list(
    manager: &SessionManager,
    workers: &HashMap<PathBuf, WorkerHost>,
    active_workspace: &PathBuf,
) -> String {
    let summaries = match project_summaries(manager) {
        Ok(summaries) => summaries,
        Err(error) => return format!("Failed to list projects: {error}"),
    };
    if summaries.is_empty() {
        return "No projects found.".to_string();
    }
    let mut lines = vec!["Projects:".to_string()];
    for (index, summary) in summaries.iter().enumerate() {
        let marker = if &summary.workspace == active_workspace {
            "*"
        } else {
            " "
        };
        let status = workers
            .get(&summary.workspace)
            .map(|worker| worker_status_label(worker.status))
            .unwrap_or("stopped");
        lines.push(format!(
            "{marker} {:>2}. [{status:<16}] {}",
            index + 1,
            summary.workspace.display()
        ));
    }
    lines.push("Use /project <index|path> to switch.".to_string());
    lines.join("\n")
}

fn build_worker_agent(
    manager: &SessionManager,
    model_store: &CredentialStore,
    mut config: AgentConfig,
    workspace: PathBuf,
    seed_model: String,
    seed_model_id: String,
    seed_llm: LlmClient,
) -> Result<Agent> {
    let workspace = workspace.canonicalize()?;
    let session = match manager.latest(&workspace)? {
        Some(summary) => manager.open(summary.session_id)?,
        None => manager.create(CreateSession {
            workspace: workspace.clone(),
            model: seed_model.clone(),
            model_id: seed_model_id,
        })?,
    };
    let summary = session.summary();
    let model = summary
        .model
        .parse::<Model>()
        .map_err(anyhow::Error::msg)
        .context("project session references an unsupported model")?;
    config.workspace = workspace;
    let llm = if summary.model == seed_model {
        seed_llm
    } else {
        let runtime = RuntimeModelConfig::resolve(
            ModelOverrides {
                model: Some(model),
                model_id: Some(summary.model_id.clone()),
                ..ModelOverrides::default()
            },
            model_store,
        )?;
        LlmClient::with_settings(
            reqwest::Client::new(),
            runtime.base_url,
            runtime.api_key,
            runtime.model_id,
            runtime.protocol,
            runtime.authentication,
        )
        .with_custom_temperature(runtime.model.supports_custom_temperature())
    };
    Agent::with_session_for_model(config, llm, session, summary.model)
}

fn ensure_worker_capacity(current: usize, max_workers: usize) -> Result<()> {
    ensure!(
        current < max_workers.max(1),
        "Worker limit reached ({})",
        max_workers.max(1)
    );
    Ok(())
}

fn agent_state_for_worker(status: Option<WorkerStatus>) -> app::AgentState {
    match status.unwrap_or(WorkerStatus::Error) {
        WorkerStatus::Idle => app::AgentState::Idle,
        WorkerStatus::Running => app::AgentState::Thinking,
        WorkerStatus::WaitingApproval => app::AgentState::WaitingApproval,
        WorkerStatus::Error => app::AgentState::Error,
    }
}

fn worker_status_label(status: WorkerStatus) -> &'static str {
    match status {
        WorkerStatus::Idle => "idle",
        WorkerStatus::Running => "running",
        WorkerStatus::WaitingApproval => "waiting approval",
        WorkerStatus::Error => "error",
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

fn send_skill_list(sender: &mpsc::UnboundedSender<HostEvent>, agent: &Agent) {
    let (skills, warnings) = agent.list_skills();
    let active = agent.active_skills();
    let mut output = if skills.is_empty() {
        "No valid Skills discovered.".to_string()
    } else {
        skills
            .into_iter()
            .map(|skill| {
                let marker = if active.iter().any(|item| item.name == skill.name) {
                    "*"
                } else {
                    " "
                };
                format!(
                    "{marker} {:<24} [{}] {}",
                    skill.name, skill.source, skill.description
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    if !warnings.is_empty() {
        output.push_str("\n\nWarnings:\n");
        output.push_str(&warnings.join("\n"));
    }
    let _ = sender.send(HostEvent::Notice(output));
}

fn send_session_list(
    sender: &mpsc::UnboundedSender<HostEvent>,
    manager: &SessionManager,
    agent: &Agent,
) {
    send_session_list_for_workspace(sender, manager, agent.session_summary().workspace);
}

fn send_model_choices(
    sender: &mpsc::UnboundedSender<HostEvent>,
    store: &CredentialStore,
    catalog_store: &ModelCatalogStore,
    current: &SessionSummary,
) {
    match store.model_statuses() {
        Ok(statuses) => {
            let choices = model_choices_with_catalog(
                &statuses,
                store,
                catalog_store,
                &current.model,
                &current.model_id,
            );
            if choices.is_empty() {
                let _ = sender.send(HostEvent::CommandFailed(
                    "No logged-in models. Run `noya login <model>` first.".to_string(),
                ));
            } else {
                let _ = sender.send(HostEvent::ModelChoices(choices));
            }
        }
        Err(error) => {
            let _ = sender.send(HostEvent::CommandFailed(format!(
                "Failed to list models: {error}"
            )));
        }
    }
}

fn switch_model(agent: &mut Agent, store: &CredentialStore, name: &str) -> Result<String> {
    let requested = name.parse::<Model>().map_err(anyhow::Error::msg)?;
    let current = agent.session_summary();
    if current.model == requested.id() {
        return Ok(format!(
            "Already using {} ({}).",
            requested.id(),
            current.model_id
        ));
    }
    let model = RuntimeModelConfig::resolve(
        ModelOverrides {
            model: Some(requested),
            ..ModelOverrides::default()
        },
        store,
    )?;
    let llm = LlmClient::with_settings(
        reqwest::Client::new(),
        model.base_url.clone(),
        model.api_key.clone(),
        model.model_id.clone(),
        model.protocol,
        model.authentication,
    )
    .with_custom_temperature(model.model.supports_custom_temperature());
    agent.switch_model(model.model.to_string(), llm)?;
    Ok(format!(
        "Switched this session to {} ({}).",
        model.model, model.model_id
    ))
}

fn switch_model_to(
    agent: &mut Agent,
    store: &CredentialStore,
    name: &str,
    model_id: &str,
) -> Result<String> {
    let requested = name.parse::<Model>().map_err(anyhow::Error::msg)?;
    let config = RuntimeModelConfig::resolve(
        ModelOverrides {
            model: Some(requested),
            model_id: Some(model_id.to_string()),
            ..ModelOverrides::default()
        },
        store,
    )?;
    if config.api_key.is_empty() {
        bail!("no API credential configured for {}", requested.id());
    }
    let llm = LlmClient::with_settings(
        reqwest::Client::new(),
        config.base_url.clone(),
        config.api_key.clone(),
        config.model_id.clone(),
        config.protocol,
        config.authentication,
    )
    .with_custom_temperature(config.model.supports_custom_temperature());
    store.set_model_id(requested, model_id)?;
    agent.switch_model(config.model.to_string(), llm)?;
    Ok(format!(
        "Switched this session to {} ({}).",
        config.model, model_id
    ))
}

fn apply_catalog_discovery(
    agent: &mut Agent,
    credential_store: &CredentialStore,
    catalog_store: &ModelCatalogStore,
    provider: Model,
    base_url: &str,
    model_ids: Vec<String>,
) -> Result<bool> {
    catalog_store.save(provider, base_url, model_ids.clone())?;
    let current = agent.session_summary();
    if current.model != provider.id() || model_ids.iter().any(|id| id == &current.model_id) {
        return Ok(false);
    }
    let fallback = model_ids
        .iter()
        .find(|id| *id == provider.default_model_id())
        .cloned()
        .or_else(|| model_ids.first().cloned())
        .context("discovered model catalog is empty")?;
    credential_store.set_model_id(provider, &fallback)?;
    let config = RuntimeModelConfig::resolve(
        ModelOverrides {
            model: Some(provider),
            model_id: Some(fallback),
            ..ModelOverrides::default()
        },
        credential_store,
    )?;
    let llm = LlmClient::with_settings(
        reqwest::Client::new(),
        config.base_url,
        config.api_key,
        config.model_id,
        config.protocol,
        config.authentication,
    );
    agent.switch_model(config.model.to_string(), llm)?;
    Ok(true)
}

fn model_choices_with_catalog(
    statuses: &[ModelStatus],
    store: &CredentialStore,
    catalog_store: &ModelCatalogStore,
    current_model: &str,
    current_model_id: &str,
) -> Vec<ModelChoice> {
    let mut choices = Vec::new();
    for status in statuses {
        let model_ids = if status.logged_in {
            store
                .base_url(status.model)
                .ok()
                .flatten()
                .and_then(|base_url| catalog_store.get(status.model, &base_url).ok().flatten())
                .map(|catalog| catalog.models)
                .filter(|models| !models.is_empty())
                .unwrap_or_else(|| vec![status.model.default_model_id().to_string()])
        } else {
            vec![status.model.default_model_id().to_string()]
        };
        for model_id in model_ids {
            choices.push(ModelChoice {
                model: status.model.id().to_string(),
                current: current_model == status.model.id() && current_model_id == model_id,
                model_id,
            });
        }
    }
    choices
}

#[cfg(test)]
fn model_choices(
    statuses: &[ModelStatus],
    current_model: &str,
    current_model_id: &str,
) -> Vec<ModelChoice> {
    statuses
        .iter()
        .map(|status| ModelChoice {
            model: status.model.id().to_string(),
            model_id: if current_model == status.model.id() {
                current_model_id.to_string()
            } else {
                status.model.default_model_id().to_string()
            },
            current: current_model == status.model.id(),
        })
        .collect()
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

    #[test]
    fn model_choices_include_all_models_and_mark_current() {
        let choices = model_choices(
            &[
                ModelStatus {
                    model: Model::DeepSeek,
                    logged_in: true,
                    active: true,
                },
                ModelStatus {
                    model: Model::Qwen,
                    logged_in: true,
                    active: false,
                },
                ModelStatus {
                    model: Model::Kimi,
                    logged_in: false,
                    active: false,
                },
            ],
            "qwen",
            "qwen-custom",
        );

        assert_eq!(choices.len(), 3);
        assert_eq!(choices[0].model, "deepseek");
        assert!(!choices[0].current);
        assert_eq!(choices[1].model, "qwen");
        assert_eq!(choices[1].model_id, "qwen-custom");
        assert!(choices[1].current);
        assert_eq!(choices[2].model, "kimi");
    }

    #[test]
    fn model_choices_include_the_current_model_without_a_saved_credential() {
        let choices = model_choices(
            &[
                ModelStatus {
                    model: Model::OpenAi,
                    logged_in: false,
                    active: false,
                },
                ModelStatus {
                    model: Model::DeepSeek,
                    logged_in: false,
                    active: false,
                },
            ],
            "openai",
            "gpt-4o",
        );

        assert_eq!(choices.len(), 2);
        assert_eq!(choices[0].model, "openai");
        assert_eq!(choices[0].model_id, "gpt-4o");
        assert!(choices[0].current);
        assert_eq!(choices[1].model, "deepseek");
    }

    #[test]
    fn project_history_deduplicates_workspaces() {
        let data = tempfile::tempdir().unwrap();
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let manager = SessionManager::at(data.path());

        let first_session = manager
            .create(CreateSession {
                workspace: first.path().to_path_buf(),
                model: "qwen".to_string(),
                model_id: "qwen3-coder-plus".to_string(),
            })
            .unwrap();
        drop(first_session);
        let second_session = manager
            .create(CreateSession {
                workspace: second.path().to_path_buf(),
                model: "qwen".to_string(),
                model_id: "qwen3-coder-plus".to_string(),
            })
            .unwrap();
        drop(second_session);

        let projects = project_summaries(&manager).unwrap();
        assert_eq!(projects.len(), 2);
        assert_ne!(projects[0].workspace, projects[1].workspace);
    }
}
