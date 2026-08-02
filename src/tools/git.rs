use super::{Tool, filesystem::workspace_path};
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use tokio::process::Command;

pub(super) struct GitStatus {
    pub(super) workspace: PathBuf,
}

#[async_trait]
impl Tool for GitStatus {
    fn name(&self) -> &str {
        "git_status"
    }

    fn description(&self) -> &str {
        "Show the concise Git branch and working-tree status for the workspace"
    }

    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {}, "additionalProperties": false})
    }

    async fn execute(&self, _args: Value) -> Result<Value> {
        let output = git_output(&self.workspace, &["status", "--short", "--branch"]).await?;
        Ok(json!({"status": output}))
    }
}

pub(super) struct GitDiff {
    pub(super) workspace: PathBuf,
}

#[async_trait]
impl Tool for GitDiff {
    fn name(&self) -> &str {
        "git_diff"
    }

    fn description(&self) -> &str {
        "Show the unstaged or staged Git diff for the workspace, optionally limited to one path"
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "staged": {"type": "boolean", "default": false},
                "path": {"type": "string"}
            },
            "additionalProperties": false
        })
    }

    async fn execute(&self, args: Value) -> Result<Value> {
        let staged = args
            .get("staged")
            .map(|value| value.as_bool().context("staged must be a boolean"))
            .transpose()?
            .unwrap_or(false);
        let mut arguments = vec!["diff", "--no-ext-diff"];
        if staged {
            arguments.push("--cached");
        }
        arguments.push("--");
        if let Some(raw_path) = args.get("path") {
            let raw_path = raw_path.as_str().context("path must be a string")?;
            workspace_path(&self.workspace, raw_path)?;
            arguments.push(raw_path);
        }
        let diff = git_output(&self.workspace, &arguments).await?;
        Ok(json!({"staged": staged, "diff": diff}))
    }
}

async fn git_output(workspace: &Path, arguments: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(arguments)
        .current_dir(workspace)
        .kill_on_drop(true)
        .output()
        .await
        .context("run git")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git command failed: {}", stderr.trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn initialize_repository(path: &Path) {
        let run = |arguments: &[&str]| {
            let status = std::process::Command::new("git")
                .args(arguments)
                .current_dir(path)
                .status()
                .unwrap();
            assert!(status.success());
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.name", "Noya Test"]);
        run(&["config", "user.email", "noya@example.com"]);
        std::fs::write(path.join("sample.txt"), "before\n").unwrap();
        run(&["add", "sample.txt"]);
        run(&["commit", "-q", "-m", "initial"]);
    }

    #[tokio::test]
    async fn reports_status_and_diff_from_the_workspace_repository() {
        let workspace = tempfile::tempdir().unwrap();
        initialize_repository(workspace.path());
        std::fs::write(workspace.path().join("sample.txt"), "after\n").unwrap();

        let status = GitStatus {
            workspace: workspace.path().to_path_buf(),
        }
        .execute(json!({}))
        .await
        .unwrap();
        let diff = GitDiff {
            workspace: workspace.path().to_path_buf(),
        }
        .execute(json!({"path": "sample.txt"}))
        .await
        .unwrap();

        assert!(status["status"].as_str().unwrap().contains("M sample.txt"));
        assert!(diff["diff"].as_str().unwrap().contains("+after"));
        assert!(diff["diff"].as_str().unwrap().contains("-before"));
    }
}
