use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use noya::{
    Agent, AgentConfig, LlmClient,
    model::{
        AuthenticationMode, CredentialStore, Model, ModelOverrides, ModelStatus,
        ProviderProtocol, RuntimeModelConfig,
    },
    session::{CreateSession, ExportFormat, SessionFilter, SessionManager, SessionSummary},
    tui,
};
use std::{
    env, fs,
    io::{self, Write},
    path::{Path, PathBuf},
    process::Command as ProcessCommand,
};

#[derive(Parser)]
#[command(
    name = "noya",
    version = env!("CARGO_PKG_VERSION"),
    about = "A coding agent for repository tasks"
)]
pub struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    #[arg(long)]
    workspace: Option<PathBuf>,

    #[arg(long)]
    pub(super) model: Option<Model>,

    #[arg(long, env = "OPENAI_COMPAT_BASE_URL")]
    base_url: Option<String>,

    #[arg(long, env = "OPENAI_COMPAT_API_KEY")]
    api_key: Option<String>,

    #[arg(long, env = "OPENAI_COMPAT_MODEL")]
    model_id: Option<String>,

    /// Maximum tool-call rounds; one final model response is still allowed.
    #[arg(long, default_value_t = 50)]
    max_tool_loops: usize,

    /// Maximum execution time for one tool call.
    #[arg(long, default_value_t = 120)]
    tool_timeout_seconds: u64,

    /// Maximum serialized tool result retained in the model context.
    #[arg(long, default_value_t = 32_768)]
    max_tool_output_bytes: usize,

    /// Maximum number of project Workers kept alive in this process.
    #[arg(long, env = "NOYA_MAX_WORKERS", default_value_t = 4)]
    max_workers: usize,
}

#[derive(Subcommand)]
enum Command {
    /// Upgrade the installed Noya binary to the latest release.
    Upgrade {
        /// Install a specific release tag instead of the latest release.
        #[arg(long)]
        version: Option<String>,
    },
    /// Uninstall the current Noya binary without removing user data.
    Uninstall {
        /// Skip the confirmation prompt.
        #[arg(long)]
        yes: bool,
    },
    /// Sign in to a supported model.
    Login {
        /// Model to configure.
        #[arg(default_value = "openai")]
        model: Model,
        /// Provider protocol: openai-compatible or anthropic-messages.
        #[arg(long)]
        protocol: Option<String>,
        /// Authentication mode: bearer or x-api-key.
        #[arg(long)]
        auth_mode: Option<String>,
    },
    /// Remove the selected model credential.
    Logout {
        /// Model to remove; defaults to the active model.
        model: Option<Model>,
    },
    /// List supported models and login status.
    Models,
    /// Resume the latest workspace session or one matching an ID prefix.
    Resume { session_id: Option<String> },
    /// List local sessions.
    Sessions {
        /// List sessions across all workspaces.
        #[arg(long)]
        all: bool,
        /// List archived sessions instead of active sessions.
        #[arg(long)]
        archived: bool,
        #[arg(long)]
        json: bool,
    },
    /// Inspect, export, fork, or archive one session.
    Session {
        #[command(subcommand)]
        command: SessionCommand,
    },
}

#[derive(Subcommand)]
enum SessionCommand {
    /// Show a session transcript.
    Show {
        session_id: String,
        #[arg(long)]
        json: bool,
    },
    /// Export a session to stdout.
    Export {
        session_id: String,
        #[arg(long, value_enum, default_value_t = SessionExportFormat::Markdown)]
        format: SessionExportFormat,
    },
    /// Archive a session without deleting its history.
    Archive { session_id: String },
    /// Fork a session at its latest or a selected completed turn sequence.
    Fork {
        session_id: String,
        #[arg(long)]
        through_seq: Option<u64>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum SessionExportFormat {
    Markdown,
    Jsonl,
}

pub async fn run(mut cli: Cli) -> Result<()> {
    match cli.command.take() {
        Some(Command::Upgrade { version }) => upgrade(version),
        Some(Command::Uninstall { yes }) => uninstall(yes),
        command => {
            let store = CredentialStore::discover()?;
            run_with_store(cli, command, &store).await
        }
    }
}

async fn run_with_store(cli: Cli, command: Option<Command>, store: &CredentialStore) -> Result<()> {
    match command {
        Some(Command::Login {
            model,
            protocol,
            auth_mode,
        }) => login(model, protocol, auth_mode, store),
        Some(Command::Logout { model }) => logout(model, store),
        Some(Command::Models) => models(store),
        Some(Command::Resume { session_id }) => resume_agent(cli, store, session_id).await,
        Some(Command::Sessions {
            all,
            archived,
            json,
        }) => sessions(&cli, all, archived, json),
        Some(Command::Session { command }) => session_command(command),
        None => run_new_agent(cli, store).await,
        Some(Command::Upgrade { .. }) | Some(Command::Uninstall { .. }) => {
            unreachable!("installation commands are handled before loading credentials")
        }
    }
}

fn upgrade(version: Option<String>) -> Result<()> {
    let executable = installed_executable()?;
    let install_dir = executable
        .parent()
        .context("installed Noya executable has no parent directory")?;
    let repository = env::var("NOYA_REPOSITORY").unwrap_or_else(|_| "jacoobwang/noya".to_string());
    let script_url =
        format!("https://raw.githubusercontent.com/{repository}/main/scripts/install.sh");
    println!("Upgrading Noya in {}...", install_dir.display());

    let mut installer = ProcessCommand::new("sh");
    installer
        .arg("-c")
        .arg(
            "if command -v curl >/dev/null 2>&1; then \
                curl --fail --silent --show-error --location \"$NOYA_INSTALL_SCRIPT\"; \
             elif command -v wget >/dev/null 2>&1; then \
                wget --quiet --output-document=- \"$NOYA_INSTALL_SCRIPT\"; \
             else \
                echo 'noya: curl or wget is required to upgrade' >&2; exit 1; \
             fi | sh",
        )
        .env("NOYA_INSTALL_SCRIPT", script_url)
        .env("NOYA_INSTALL_DIR", install_dir);
    if let Some(version) = version {
        installer.env("NOYA_VERSION", version);
    }
    let status = installer.status().context("run Noya installer")?;
    if !status.success() {
        bail!("Noya upgrade failed with status {status}");
    }
    Ok(())
}

fn uninstall(yes: bool) -> Result<()> {
    let executable = installed_executable()?;
    if !yes {
        print!(
            "Remove {}? User data in ~/.noya will be kept. [y/N] ",
            executable.display()
        );
        io::stdout().flush().context("flush uninstall prompt")?;
        let mut input = String::new();
        io::stdin()
            .read_line(&mut input)
            .context("read uninstall confirmation")?;
        if !matches!(input.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            println!("Uninstall cancelled.");
            return Ok(());
        }
    }
    fs::remove_file(&executable)
        .with_context(|| format!("remove Noya executable {}", executable.display()))?;
    println!(
        "Removed {}. User data in ~/.noya was kept.",
        executable.display()
    );
    Ok(())
}

fn installed_executable() -> Result<PathBuf> {
    let executable = env::current_exe().context("find the running Noya executable")?;
    if is_development_binary(&executable) {
        bail!(
            "{} is a development binary; run upgrade/uninstall on an installed Noya binary",
            executable.display()
        );
    }
    if executable.file_name().and_then(|name| name.to_str()) != Some("noya") {
        bail!(
            "current executable is not a Noya binary: {}",
            executable.display()
        );
    }
    Ok(executable)
}

fn is_development_binary(path: &Path) -> bool {
    path.components()
        .any(|component| component.as_os_str() == "target")
}

fn login(
    model: Model,
    protocol: Option<String>,
    auth_mode: Option<String>,
    store: &CredentialStore,
) -> Result<()> {
    let protocol = protocol
        .map(|value| value.parse::<ProviderProtocol>().map_err(anyhow::Error::msg))
        .transpose()?
        .unwrap_or_else(|| model.default_protocol());
    let authentication = auth_mode
        .map(|value| value.parse::<AuthenticationMode>().map_err(anyhow::Error::msg))
        .transpose()?
        .unwrap_or_else(|| model.default_authentication());
    let default_base_url = store
        .base_url(model)?
        .unwrap_or_else(|| model.base_url().to_string());
    let base_url = prompt_base_url(&default_base_url)?;
    let api_key = rpassword::prompt_password(format!("{}: ", model.api_key_label()))
        .context("read API key")?;
    store.login_with_config(
        model,
        &api_key,
        Some(&base_url),
        protocol,
        authentication,
    )?;
    println!(
        "Logged in to {model} using {protocol} and {authentication}. Credential saved to {}.",
        store.path().display()
    );
    Ok(())
}

fn prompt_base_url(default_base_url: &str) -> Result<String> {
    print!("Base URL [{default_base_url}]: ");
    io::stdout().flush().context("flush base URL prompt")?;
    let mut input = String::new();
    io::stdin().read_line(&mut input).context("read base URL")?;
    let base_url = input.trim();
    Ok(if base_url.is_empty() {
        default_base_url.to_string()
    } else {
        base_url.to_string()
    })
}

fn logout(model: Option<Model>, store: &CredentialStore) -> Result<()> {
    let model = match model.or(store.active_model()?) {
        Some(model) => model,
        None => bail!(
            "no model selected; run `noya logout <model>` (supported: {})",
            Model::supported().join(", ")
        ),
    };
    if store.logout(model)? {
        println!("Logged out from {model}.");
    } else {
        println!("No stored credential for {model}.");
    }
    Ok(())
}

fn models(store: &CredentialStore) -> Result<()> {
    println!("{}", render_models(&store.model_statuses()?));
    Ok(())
}

fn render_models(statuses: &[ModelStatus]) -> String {
    let name_width = statuses
        .iter()
        .map(|status| status.model.id().len())
        .max()
        .unwrap_or(5)
        .max("MODEL".len());
    let id_width = statuses
        .iter()
        .map(|status| status.model.default_model_id().len())
        .max()
        .unwrap_or(8)
        .max("MODEL ID".len());
    let mut lines = vec![format!(
        "{:<name_width$}  {:<id_width$}  STATUS",
        "MODEL", "MODEL ID"
    )];
    for status in statuses {
        let state = if status.active {
            "active"
        } else if status.logged_in {
            "logged in"
        } else {
            "not logged in"
        };
        lines.push(format!(
            "{:<name_width$}  {:<id_width$}  {state}",
            status.model.id(),
            status.model.default_model_id()
        ));
    }
    lines.join("\n")
}

async fn run_new_agent(cli: Cli, store: &CredentialStore) -> Result<()> {
    let model = RuntimeModelConfig::resolve(
        ModelOverrides {
            model: cli.model,
            api_key: cli.api_key.clone(),
            base_url: cli.base_url.clone(),
            model_id: cli.model_id.clone(),
            ..ModelOverrides::default()
        },
        store,
    )?;
    let workspace = resolve_workspace(cli.workspace.as_deref())?;
    let manager = SessionManager::discover()?;
    let session = manager.create(CreateSession {
        workspace: workspace.clone(),
        model: model.model.to_string(),
        model_id: model.model_id.clone(),
    })?;
    run_tui(cli, model, session, workspace).await
}

async fn resume_agent(
    cli: Cli,
    store: &CredentialStore,
    session_prefix: Option<String>,
) -> Result<()> {
    let manager = SessionManager::discover()?;
    let session_id = match session_prefix {
        Some(prefix) => manager.resolve_prefix(&prefix, false)?,
        None => {
            let workspace = resolve_workspace(cli.workspace.as_deref())?;
            manager
                .latest(&workspace)?
                .context("no resumable session found for this workspace")?
                .session_id
        }
    };
    let session = manager.open(session_id)?;
    let summary = session.summary();
    if let Some(workspace) = cli.workspace.as_deref() {
        let workspace = workspace.canonicalize()?;
        if workspace != summary.workspace {
            bail!(
                "session workspace is {}; explicit workspace is {}",
                summary.workspace.display(),
                workspace.display()
            );
        }
    }
    let session_model = summary
        .model
        .parse::<Model>()
        .map_err(anyhow::Error::msg)
        .context("session references an unsupported model")?;
    let selected_model = cli.model.unwrap_or(session_model);
    let resumed_model_id = match (&cli.model_id, cli.model) {
        (Some(model_id), _) => Some(model_id.clone()),
        (None, Some(explicit_model)) if explicit_model != session_model => None,
        (None, _) => Some(summary.model_id),
    };
    let model = RuntimeModelConfig::resolve(
        ModelOverrides {
            model: Some(selected_model),
            api_key: cli.api_key.clone(),
            base_url: cli.base_url.clone(),
            model_id: resumed_model_id,
            ..ModelOverrides::default()
        },
        store,
    )?;
    let workspace = summary.workspace;
    run_tui(cli, model, session, workspace).await
}

async fn run_tui(
    cli: Cli,
    model: RuntimeModelConfig,
    session: noya::session::Session,
    workspace: PathBuf,
) -> Result<()> {
    let agent = Agent::with_session_for_model(
        AgentConfig {
            workspace: workspace.clone(),
            max_tool_loops: cli.max_tool_loops,
            tool_timeout: std::time::Duration::from_secs(cli.tool_timeout_seconds),
            max_tool_output_bytes: cli.max_tool_output_bytes,
            temperature: 0.2,
        },
        LlmClient::with_settings(
            reqwest::Client::new(),
            model.base_url,
            model.api_key,
            model.model_id.clone(),
            model.protocol,
            model.authentication,
        )
            .with_custom_temperature(model.model.supports_custom_temperature()),
        session,
        model.model.to_string(),
    )?;
    tui::run(
        agent,
        tui::AppInfo {
            workspace,
            model: model.model.to_string(),
            model_id: model.model_id,
        },
        cli.max_workers,
    )
    .await
}

fn sessions(cli: &Cli, all: bool, archived: bool, json: bool) -> Result<()> {
    let manager = SessionManager::discover()?;
    let workspace = if all {
        None
    } else {
        Some(resolve_workspace(cli.workspace.as_deref())?)
    };
    let mut summaries = manager.list(SessionFilter {
        workspace,
        include_archived: archived,
    })?;
    summaries.retain(|summary| summary.archived == archived);
    if json {
        println!("{}", serde_json::to_string_pretty(&summaries)?);
    } else {
        println!("{}", render_sessions(&summaries));
    }
    Ok(())
}

fn session_command(command: SessionCommand) -> Result<()> {
    let manager = SessionManager::discover()?;
    match command {
        SessionCommand::Show { session_id, json } => {
            let id = manager.resolve_prefix(&session_id, true)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&manager.show(id)?)?);
            } else {
                print!("{}", manager.export(id, ExportFormat::Markdown)?);
            }
        }
        SessionCommand::Export { session_id, format } => {
            let id = manager.resolve_prefix(&session_id, true)?;
            let format = match format {
                SessionExportFormat::Markdown => ExportFormat::Markdown,
                SessionExportFormat::Jsonl => ExportFormat::Jsonl,
            };
            print!("{}", manager.export(id, format)?);
        }
        SessionCommand::Archive { session_id } => {
            let id = manager.resolve_prefix(&session_id, false)?;
            manager.archive(id)?;
            println!("Archived session {id}.");
        }
        SessionCommand::Fork {
            session_id,
            through_seq,
        } => {
            let id = manager.resolve_prefix(&session_id, true)?;
            let child = manager.fork(id, through_seq)?;
            println!("Forked session {} from {id}.", child.id());
        }
    }
    Ok(())
}

fn resolve_workspace(workspace: Option<&std::path::Path>) -> Result<PathBuf> {
    workspace
        .unwrap_or_else(|| std::path::Path::new("."))
        .canonicalize()
        .context("resolve workspace")
}

fn render_sessions(summaries: &[SessionSummary]) -> String {
    let mut lines = vec![format!(
        "{:<12}  {:<20}  {:<10}  {:>5}  TITLE",
        "SESSION", "UPDATED", "MODEL", "TURNS"
    )];
    for summary in summaries {
        let id = summary.session_id.to_string();
        let updated = summary
            .updated_at
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_else(|_| summary.updated_at.to_string());
        lines.push(format!(
            "{:<12}  {:<20}  {:<10}  {:>5}  {}",
            &id[..12],
            updated.chars().take(20).collect::<String>(),
            summary.model,
            summary.completed_turns,
            summary.title
        ));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use noya::model::Model;

    #[test]
    fn parses_version_option() {
        let error = match Cli::try_parse_from(["noya", "--version"]) {
            Ok(_) => panic!("--version should exit during argument parsing"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), clap::error::ErrorKind::DisplayVersion);
        assert!(error.to_string().contains(env!("CARGO_PKG_VERSION")));
    }

    #[test]
    fn parses_model_login_logout_and_models_commands() {
        let login = Cli::try_parse_from(["noya", "login", "deepseek"]).unwrap();
        assert!(matches!(
            login.command,
            Some(Command::Login {
                model: Model::DeepSeek,
                ..
            })
        ));

        let logout = Cli::try_parse_from(["noya", "logout", "deepseek"]).unwrap();
        assert!(matches!(
            logout.command,
            Some(Command::Logout {
                model: Some(Model::DeepSeek)
            })
        ));

        let qwen = Cli::try_parse_from(["noya", "login", "qwen"]).unwrap();
        assert!(matches!(
            qwen.command,
            Some(Command::Login {
                model: Model::Qwen,
                ..
            })
        ));

        let kimi = Cli::try_parse_from(["noya", "logout", "kimi"]).unwrap();
        assert!(matches!(
            kimi.command,
            Some(Command::Logout {
                model: Some(Model::Kimi)
            })
        ));

        let models = Cli::try_parse_from(["noya", "models"]).unwrap();
        assert!(matches!(models.command, Some(Command::Models)));
    }

    #[test]
    fn parses_session_lifecycle_commands_and_optional_workspace() {
        let fresh = Cli::try_parse_from(["noya"]).unwrap();
        assert!(fresh.workspace.is_none());

        let resume = Cli::try_parse_from(["noya", "resume", "019fbd63"]).unwrap();
        assert!(matches!(
            resume.command,
            Some(Command::Resume {
                session_id: Some(ref value)
            }) if value == "019fbd63"
        ));

        let sessions = Cli::try_parse_from(["noya", "sessions", "--all", "--json"]).unwrap();
        assert!(matches!(
            sessions.command,
            Some(Command::Sessions {
                all: true,
                archived: false,
                json: true,
            })
        ));

        let export =
            Cli::try_parse_from(["noya", "session", "export", "019fbd63", "--format", "jsonl"])
                .unwrap();
        assert!(matches!(
            export.command,
            Some(Command::Session {
                command: SessionCommand::Export {
                    ref session_id,
                    format: SessionExportFormat::Jsonl,
                }
            }) if session_id == "019fbd63"
        ));
    }

    #[test]
    fn login_defaults_to_openai_and_runtime_accepts_model_overrides() {
        let login = Cli::try_parse_from(["noya", "login"]).unwrap();
        assert!(matches!(
            login.command,
            Some(Command::Login {
                model: Model::OpenAi,
                ..
            })
        ));

        let run = Cli::try_parse_from([
            "noya",
            "--model",
            "deepseek",
            "--model-id",
            "deepseek-custom",
        ])
        .unwrap();
        assert_eq!(run.model, Some(Model::DeepSeek));
        assert_eq!(run.model_id.as_deref(), Some("deepseek-custom"));
        assert_eq!(run.max_tool_loops, 50);
        assert_eq!(run.tool_timeout_seconds, 120);
        assert_eq!(run.max_tool_output_bytes, 32_768);
        assert!(run.command.is_none());
    }

    #[test]
    fn parses_upgrade_and_uninstall_commands() {
        let upgrade = Cli::try_parse_from(["noya", "upgrade", "--version", "v0.3.0"]).unwrap();
        assert!(matches!(
            upgrade.command,
            Some(Command::Upgrade {
                version: Some(version)
            }) if version == "v0.3.0"
        ));

        let uninstall = Cli::try_parse_from(["noya", "uninstall", "--yes"]).unwrap();
        assert!(matches!(
            uninstall.command,
            Some(Command::Uninstall { yes: true })
        ));
    }

    #[test]
    fn model_list_shows_default_ids_and_status() {
        let rendered = render_models(&[
            ModelStatus {
                model: Model::OpenAi,
                logged_in: false,
                active: false,
            },
            ModelStatus {
                model: Model::Qwen,
                logged_in: true,
                active: true,
            },
        ]);

        assert!(rendered.contains("MODEL ID"));
        assert!(rendered.contains("openai  gpt-4o"));
        assert!(rendered.contains("qwen    qwen3-coder-plus  active"));
    }
}
