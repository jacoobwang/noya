use super::Tool;
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::path::{Component, Path, PathBuf};
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
        "Read a UTF-8 file or a zero-based line range in the workspace"
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "offset": {"type": "integer", "minimum": 0, "description": "Zero-based line offset"},
                "limit": {"type": "integer", "minimum": 1, "description": "Maximum lines to return"}
            },
            "required": ["path"],
            "additionalProperties": false
        })
    }
    async fn execute(&self, args: Value) -> Result<Value> {
        let path = workspace_path(
            &self.workspace,
            args["path"].as_str().context("path must be a string")?,
        )?;
        let offset = optional_usize(&args, "offset")?.unwrap_or(0);
        let limit = optional_usize(&args, "limit")?;
        if limit == Some(0) {
            bail!("limit must be greater than zero");
        }
        let content = tokio::fs::read_to_string(&path).await?;
        let lines = content.split_inclusive('\n').collect::<Vec<_>>();
        let total_lines = lines.len();
        let start = offset.min(total_lines);
        let end = limit
            .map(|limit| start.saturating_add(limit).min(total_lines))
            .unwrap_or(total_lines);
        let content = lines[start..end].concat();
        Ok(json!({
            "path": path.strip_prefix(&self.workspace).unwrap_or(&path),
            "content": content,
            "offset": start,
            "returned_lines": end.saturating_sub(start),
            "total_lines": total_lines,
            "truncated": start > 0 || end < total_lines,
            "next_offset": (end < total_lines).then_some(end),
        }))
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
            .await
            .context(
                "run ripgrep (rg); install it with `brew install ripgrep` on macOS or your Linux package manager",
            )?;
        Ok(
            json!({"matches": String::from_utf8_lossy(&output.stdout), "exit_code": output.status.code()}),
        )
    }
}

pub(super) fn workspace_path(workspace: &Path, raw: &str) -> Result<PathBuf> {
    let raw_path = Path::new(raw);
    if raw_path.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        bail!("path is outside workspace: {}", raw);
    }
    let candidate = workspace.join(raw_path);
    let workspace = workspace.canonicalize().context("canonicalize workspace")?;
    let mut existing_ancestor = candidate.as_path();
    while !existing_ancestor.exists() {
        existing_ancestor = existing_ancestor
            .parent()
            .context("path has no existing ancestor")?;
    }
    let resolved = existing_ancestor
        .canonicalize()
        .context("canonicalize workspace path")?;
    if resolved != workspace && !resolved.starts_with(&workspace) {
        bail!("path is outside workspace: {}", raw);
    }
    Ok(candidate)
}

fn optional_usize(args: &Value, name: &str) -> Result<Option<usize>> {
    args.get(name)
        .map(|value| {
            let value = value
                .as_u64()
                .with_context(|| format!("{name} must be a non-negative integer"))?;
            usize::try_from(value).with_context(|| format!("{name} is too large"))
        })
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn read_file_returns_a_line_range_with_pagination_metadata() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("sample.txt"), "one\ntwo\nthree\nfour").unwrap();
        let tool = ReadFile {
            workspace: workspace.path().to_path_buf(),
        };

        let result = tool
            .execute(json!({"path": "sample.txt", "offset": 1, "limit": 2}))
            .await
            .unwrap();

        assert_eq!(result["content"], "two\nthree\n");
        assert_eq!(result["returned_lines"], 2);
        assert_eq!(result["total_lines"], 4);
        assert_eq!(result["next_offset"], 3);
        assert_eq!(result["truncated"], true);
    }

    #[cfg(unix)]
    #[test]
    fn workspace_path_rejects_symlinks_that_escape_the_workspace() {
        use std::os::unix::fs::symlink;

        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), workspace.path().join("outside")).unwrap();

        let error = workspace_path(workspace.path(), "outside/file.txt")
            .unwrap_err()
            .to_string();

        assert!(error.contains("outside workspace"));
    }

    #[test]
    fn workspace_path_rejects_parent_directory_traversal() {
        let workspace = tempfile::tempdir().unwrap();

        let error = workspace_path(workspace.path(), "../outside.txt")
            .unwrap_err()
            .to_string();

        assert!(error.contains("outside workspace"));
    }
}
