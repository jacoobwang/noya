use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use noya::{
    Agent, AgentConfig, LlmClient,
    model::{CredentialStore, Model, ModelOverrides, ModelStatus, RuntimeModelConfig},
    tui,
};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "noya", about = "A coding agent for repository tasks")]
pub struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    #[arg(long, default_value = ".")]
    workspace: PathBuf,

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
}

#[derive(Subcommand)]
enum Command {
    /// Sign in to a supported model.
    Login {
        /// Model to configure.
        #[arg(default_value = "openai")]
        model: Model,
    },
    /// Remove the selected model credential.
    Logout {
        /// Model to remove; defaults to the active model.
        model: Option<Model>,
    },
    /// List supported models and login status.
    Models,
}

pub async fn run(cli: Cli) -> Result<()> {
    let store = CredentialStore::discover()?;
    match cli.command {
        Some(Command::Login { model }) => login(model, &store),
        Some(Command::Logout { model }) => logout(model, &store),
        Some(Command::Models) => models(&store),
        None => run_agent(cli, &store).await,
    }
}

fn login(model: Model, store: &CredentialStore) -> Result<()> {
    let api_key = rpassword::prompt_password(format!("{}: ", model.api_key_label()))
        .context("read API key")?;
    store.login(model, &api_key)?;
    println!(
        "Logged in to {model}. Credential saved to {}.",
        store.path().display()
    );
    Ok(())
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

async fn run_agent(cli: Cli, store: &CredentialStore) -> Result<()> {
    let model = RuntimeModelConfig::resolve(
        ModelOverrides {
            model: cli.model,
            api_key: cli.api_key,
            base_url: cli.base_url,
            model_id: cli.model_id,
        },
        store,
    )?;
    let workspace = cli.workspace.canonicalize()?;
    let agent = Agent::new(
        AgentConfig {
            workspace: workspace.clone(),
            max_tool_loops: cli.max_tool_loops,
            temperature: 0.2,
        },
        LlmClient::new(model.base_url, model.api_key, model.model_id.clone())
            .with_custom_temperature(model.model.supports_custom_temperature()),
    )?;
    tui::run(
        agent,
        tui::AppInfo {
            workspace,
            model: model.model.to_string(),
            model_id: model.model_id,
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use noya::model::Model;

    #[test]
    fn parses_model_login_logout_and_models_commands() {
        let login = Cli::try_parse_from(["noya", "login", "deepseek"]).unwrap();
        assert!(matches!(
            login.command,
            Some(Command::Login {
                model: Model::DeepSeek
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
            Some(Command::Login { model: Model::Qwen })
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
    fn login_defaults_to_openai_and_runtime_accepts_model_overrides() {
        let login = Cli::try_parse_from(["noya", "login"]).unwrap();
        assert!(matches!(
            login.command,
            Some(Command::Login {
                model: Model::OpenAi
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
        assert!(run.command.is_none());
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
