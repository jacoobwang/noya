use super::Tool;
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::path::PathBuf;
use tokio::process::Command;

pub(super) struct RunCommand {
    pub(super) workspace: PathBuf,
}

#[async_trait]
impl Tool for RunCommand {
    fn name(&self) -> &str {
        "run_command"
    }
    fn description(&self) -> &str {
        "Run a non-interactive command in the workspace"
    }
    fn risk(&self) -> super::ToolRisk {
        super::ToolRisk::Dangerous
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{"command":{"type":"string"}},"required":["command"],"additionalProperties":false})
    }
    async fn execute(&self, args: Value) -> Result<Value> {
        let command = args["command"]
            .as_str()
            .context("command must be a string")?;
        let output = Command::new("sh")
            .args(["-lc", command])
            .current_dir(&self.workspace)
            .kill_on_drop(true)
            .output()
            .await?;
        Ok(
            json!({"status": output.status.code(), "stdout": String::from_utf8_lossy(&output.stdout), "stderr": String::from_utf8_lossy(&output.stderr)}),
        )
    }
}
