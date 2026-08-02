use super::Tool;
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use tokio::process::Command;

pub(super) struct ReadFile {
    pub(super) workspace: PathBuf,
}

#[async_trait]
impl Tool for ReadFile {
    fn name(&self) -> &str {
        "read_file"
    }
    fn description(&self) -> &str {
        "Read a UTF-8 file in the workspace"
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"],"additionalProperties":false})
    }
    async fn execute(&self, args: Value) -> Result<Value> {
        let path = workspace_path(
            &self.workspace,
            args["path"].as_str().context("path must be a string")?,
        )?;
        Ok(
            json!({"path": path.strip_prefix(&self.workspace).unwrap_or(&path), "content": tokio::fs::read_to_string(path).await?}),
        )
    }
}

pub(super) struct WriteFile {
    pub(super) workspace: PathBuf,
}

#[async_trait]
impl Tool for WriteFile {
    fn name(&self) -> &str {
        "write_file"
    }
    fn description(&self) -> &str {
        "Write or replace a UTF-8 file in the workspace"
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{"path":{"type":"string"},"content":{"type":"string"}},"required":["path","content"],"additionalProperties":false})
    }
    async fn execute(&self, args: Value) -> Result<Value> {
        let path = workspace_path(
            &self.workspace,
            args["path"].as_str().context("path must be a string")?,
        )?;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(
            &path,
            args["content"]
                .as_str()
                .context("content must be a string")?,
        )
        .await?;
        Ok(json!({"written": path.strip_prefix(&self.workspace).unwrap_or(&path)}))
    }
}

pub(super) struct ListDir {
    pub(super) workspace: PathBuf,
}

#[async_trait]
impl Tool for ListDir {
    fn name(&self) -> &str {
        "list_dir"
    }
    fn description(&self) -> &str {
        "List entries in a workspace directory"
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{"path":{"type":"string"}},"required":[],"additionalProperties":false})
    }
    async fn execute(&self, args: Value) -> Result<Value> {
        let raw = args.get("path").and_then(Value::as_str).unwrap_or(".");
        let path = workspace_path(&self.workspace, raw)?;
        let mut entries = tokio::fs::read_dir(path).await?;
        let mut result = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            result.push(entry.file_name().to_string_lossy().to_string());
        }
        result.sort();
        Ok(json!({"entries": result}))
    }
}

pub(super) struct SearchText {
    pub(super) workspace: PathBuf,
}

#[async_trait]
impl Tool for SearchText {
    fn name(&self) -> &str {
        "search_text"
    }
    fn description(&self) -> &str {
        "Search text recursively using ripgrep when available"
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{"query":{"type":"string"},"path":{"type":"string"}},"required":["query"],"additionalProperties":false})
    }
    async fn execute(&self, args: Value) -> Result<Value> {
        let query = args["query"].as_str().context("query must be a string")?;
        let raw = args.get("path").and_then(Value::as_str).unwrap_or(".");
        let path = workspace_path(&self.workspace, raw)?;
        let output = Command::new("rg")
            .args([
                "-n",
                "--hidden",
                "--glob",
                "!.git",
                query,
                path.to_str().unwrap_or("."),
            ])
            .output()
            .await?;
        Ok(
            json!({"matches": String::from_utf8_lossy(&output.stdout), "exit_code": output.status.code()}),
        )
    }
}

fn workspace_path(workspace: &Path, raw: &str) -> Result<PathBuf> {
    let candidate = workspace.join(raw);
    let workspace = workspace.canonicalize().context("canonicalize workspace")?;
    let parent = candidate
        .parent()
        .unwrap_or(&candidate)
        .canonicalize()
        .unwrap_or_else(|_| workspace.clone());
    if parent != workspace && !parent.starts_with(&workspace) {
        bail!("path is outside workspace: {}", raw);
    }
    Ok(candidate)
}
