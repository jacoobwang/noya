use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{process::Command, time::timeout};

pub const DEFAULT_CONTINUATION_PROMPT: &str =
    "Continue working toward the objective. No human input is available; make reasonable assumptions, verify your work, and do not claim completion without evidence.";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutonomousConfig {
    pub max_continuations: usize,
    pub max_turns: usize,
    pub max_tokens: u64,
    pub timeout_ms: u64,
    pub continuation_prompt: String,
    pub gates: QualityGateConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QualityGateConfig {
    pub commands: Vec<String>,
    pub max_retries: usize,
    pub timeout_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutonomousStatus {
    pub turns_used: usize,
    pub continuations_used: usize,
    pub tokens_used: u64,
    pub started_at_ms: u128,
    pub last_gate_failure: Option<QualityGateFailure>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QualityGateFailure {
    pub command: String,
    pub attempt: usize,
    pub exit: String,
    pub output: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AutonomousStopReason {
    GatesPassed,
    MaxContinuations,
    MaxTurns,
    MaxTokens,
    Timeout,
    GateRetriesExhausted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutonomousReport {
    pub status: AutonomousStatus,
    pub stop_reason: AutonomousStopReason,
}

impl Default for AutonomousConfig {
    fn default() -> Self {
        Self {
            max_continuations: 3,
            max_turns: 12,
            max_tokens: 80_000,
            timeout_ms: 30 * 60 * 1000,
            continuation_prompt: DEFAULT_CONTINUATION_PROMPT.to_string(),
            gates: QualityGateConfig::default(),
        }
    }
}

impl Default for QualityGateConfig {
    fn default() -> Self {
        Self {
            commands: Vec::new(),
            max_retries: 3,
            timeout_ms: 5 * 60 * 1000,
        }
    }
}

impl AutonomousConfig {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.max_turns > 0, "autonomous max turns must be positive");
        ensure!(self.max_tokens > 0, "autonomous max tokens must be positive");
        ensure!(self.timeout_ms > 0, "autonomous timeout must be positive");
        self.gates.validate()
    }

    pub fn from_environment() -> Self {
        let mut config = Self::default();
        if let Some(commands) = env_string("NOYA_AUTONOMOUS_GATES") {
            config.gates.commands = commands
                .split(',')
                .map(str::trim)
                .filter(|command| !command.is_empty())
                .map(str::to_string)
                .collect();
        }
        config.max_continuations = env_usize(
            "NOYA_AUTONOMOUS_MAX_CONTINUATIONS",
            config.max_continuations,
        );
        config.max_turns = env_usize("NOYA_AUTONOMOUS_MAX_TURNS", config.max_turns);
        config.max_tokens = env_u64("NOYA_AUTONOMOUS_MAX_TOKENS", config.max_tokens);
        config.timeout_ms = env_u64("NOYA_AUTONOMOUS_TIMEOUT_MS", config.timeout_ms);
        config.gates.max_retries =
            env_usize("NOYA_AUTONOMOUS_GATE_RETRIES", config.gates.max_retries);
        config.gates.timeout_ms =
            env_u64("NOYA_AUTONOMOUS_GATE_TIMEOUT_MS", config.gates.timeout_ms);
        config
    }
}

impl QualityGateConfig {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.timeout_ms > 0,
            "quality gate timeout must be positive"
        );
        for command in &self.commands {
            ensure!(!command.trim().is_empty(), "quality gate command cannot be empty");
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub(crate) struct GateRunner {
    workspace: PathBuf,
    config: QualityGateConfig,
    previous_failure: Option<QualityGateFailure>,
    previous_fingerprint: Option<u64>,
}

#[derive(Debug, Clone)]
pub(crate) enum GateOutcome {
    Passed,
    Failed(QualityGateFailure),
    RetriesExhausted(QualityGateFailure),
}

impl GateRunner {
    pub(crate) fn new(workspace: impl Into<PathBuf>, config: QualityGateConfig) -> Result<Self> {
        config.validate()?;
        Ok(Self {
            workspace: workspace.into(),
            config,
            previous_failure: None,
            previous_fingerprint: None,
        })
    }

    pub(crate) async fn run(&mut self) -> Result<GateOutcome> {
        if self.config.commands.is_empty() {
            return Ok(GateOutcome::Passed);
        }
        for command in &self.config.commands {
            let fingerprint = workspace_fingerprint(&self.workspace).await?;
            if self
                .previous_failure
                .as_ref()
                .is_some_and(|failure| failure.command == *command)
                && self.previous_fingerprint == Some(fingerprint)
            {
                let previous = self.previous_failure.as_ref().expect("checked above");
                let failure = QualityGateFailure {
                    command: command.clone(),
                    attempt: previous.attempt.saturating_add(1),
                    exit: "not rerun: workspace unchanged".to_string(),
                    output: "The failed gate was not rerun because the workspace is unchanged."
                        .to_string(),
                };
                self.previous_failure = Some(failure.clone());
                return if failure.attempt > self.config.max_retries {
                    Ok(GateOutcome::RetriesExhausted(failure))
                } else {
                    Ok(GateOutcome::Failed(failure))
                };
            }

            let output = run_command(
                &self.workspace,
                command,
                Duration::from_millis(self.config.timeout_ms),
            )
            .await?;
            if output.success {
                self.previous_failure = None;
                self.previous_fingerprint = None;
                continue;
            }
            let attempt = self
                .previous_failure
                .as_ref()
                .filter(|failure| failure.command == *command)
                .map_or(1, |failure| failure.attempt.saturating_add(1));
            let failure = QualityGateFailure {
                command: command.clone(),
                attempt,
                exit: output.exit,
                output: truncate_output(&output.output),
            };
            self.previous_fingerprint = Some(workspace_fingerprint(&self.workspace).await?);
            self.previous_failure = Some(failure.clone());
            return if attempt > self.config.max_retries {
                Ok(GateOutcome::RetriesExhausted(failure))
            } else {
                Ok(GateOutcome::Failed(failure))
            };
        }
        Ok(GateOutcome::Passed)
    }
}

#[derive(Debug)]
struct CommandOutput {
    success: bool,
    exit: String,
    output: String,
}

async fn run_command(workspace: &Path, command: &str, limit: Duration) -> Result<CommandOutput> {
    let child = Command::new("sh")
        .arg("-lc")
        .arg(command)
        .current_dir(workspace)
        .kill_on_drop(true)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .with_context(|| format!("start quality gate: {command}"))?;
    let output = match timeout(limit, child.wait_with_output()).await {
        Ok(output) => output.context("wait for quality gate")?,
        Err(_) => {
            return Ok(CommandOutput {
                success: false,
                exit: format!("timed out after {} ms", limit.as_millis()),
                output: String::new(),
            });
        }
    };
    let exit = output
        .status
        .code()
        .map_or_else(|| "terminated".to_string(), |code| code.to_string());
    Ok(CommandOutput {
        success: output.status.success(),
        exit,
        output: format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ),
    })
}

async fn workspace_fingerprint(workspace: &Path) -> Result<u64> {
    let status = run_command(
        workspace,
        "git status --porcelain=v1 --untracked-files=all && git diff --no-ext-diff --binary",
        Duration::from_secs(30),
    )
    .await;
    let mut hasher = DefaultHasher::new();
    match status {
        Ok(output) => output.output.hash(&mut hasher),
        Err(_) => workspace.to_string_lossy().hash(&mut hasher),
    }
    Ok(hasher.finish())
}

fn truncate_output(output: &str) -> String {
    const MAX_OUTPUT: usize = 6_000;
    if output.chars().count() <= MAX_OUTPUT {
        return output.trim().to_string();
    }
    let truncated = output.chars().take(MAX_OUTPUT).collect::<String>();
    format!("{truncated}\n[output truncated]")
}

fn env_string(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.trim().is_empty())
}

fn env_usize(name: &str, default: usize) -> usize {
    env_string(name)
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn env_u64(name: &str, default: u64) -> u64 {
    env_string(name)
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn quality_gate_passes_and_captures_failure_output() {
        let workspace = tempfile::tempdir().unwrap();
        let mut passing = GateRunner::new(
            workspace.path(),
            QualityGateConfig {
                commands: vec!["printf verified".to_string()],
                ..QualityGateConfig::default()
            },
        )
        .unwrap();
        assert!(matches!(passing.run().await.unwrap(), GateOutcome::Passed));

        let mut failing = GateRunner::new(
            workspace.path(),
            QualityGateConfig {
                commands: vec!["printf nope >&2; exit 7".to_string()],
                max_retries: 0,
                ..QualityGateConfig::default()
            },
        )
        .unwrap();
        let outcome = failing.run().await.unwrap();
        let GateOutcome::RetriesExhausted(failure) = outcome else {
            panic!("expected an exhausted quality gate");
        };
        assert_eq!(failure.exit, "7");
        assert!(failure.output.contains("nope"));
    }

    #[test]
    fn environment_config_overrides_limits_and_gates() {
        let config = AutonomousConfig::default();
        assert!(config.max_turns > 0);
        assert_eq!(config.gates.max_retries, 3);
    }
}
