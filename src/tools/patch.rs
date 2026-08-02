use super::{Tool, filesystem::workspace_path};
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::path::PathBuf;

pub(super) struct ApplyPatch {
    pub(super) workspace: PathBuf,
}

#[async_trait]
impl Tool for ApplyPatch {
    fn name(&self) -> &str {
        "apply_patch"
    }

    fn description(&self) -> &str {
        "Validate and apply one or more exact, unambiguous text replacements to an existing UTF-8 file in one write"
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "edits": {
                    "type": "array",
                    "minItems": 1,
                    "items": {
                        "type": "object",
                        "properties": {
                            "old_text": {"type": "string", "minLength": 1},
                            "new_text": {"type": "string"}
                        },
                        "required": ["old_text", "new_text"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["path", "edits"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, args: Value) -> Result<Value> {
        let raw_path = args["path"].as_str().context("path must be a string")?;
        let path = workspace_path(&self.workspace, raw_path)?;
        let edits = args["edits"].as_array().context("edits must be an array")?;
        if edits.is_empty() {
            bail!("edits must not be empty");
        }

        let original = tokio::fs::read_to_string(&path)
            .await
            .with_context(|| format!("read file for patch: {raw_path}"))?;
        let mut updated = original.clone();
        for (index, edit) in edits.iter().enumerate() {
            let old_text = edit["old_text"]
                .as_str()
                .with_context(|| format!("edits[{index}].old_text must be a string"))?;
            let new_text = edit["new_text"]
                .as_str()
                .with_context(|| format!("edits[{index}].new_text must be a string"))?;
            if old_text.is_empty() {
                bail!("edits[{index}].old_text must not be empty");
            }
            if old_text == new_text {
                bail!("edits[{index}] does not change the file");
            }
            let matches = updated.match_indices(old_text).count();
            match matches {
                1 => updated = updated.replacen(old_text, new_text, 1),
                0 => bail!("edits[{index}] context was not found; file was not changed"),
                count => bail!(
                    "edits[{index}] context matched {count} times; provide more context; file was not changed"
                ),
            }
        }

        tokio::fs::write(&path, updated.as_bytes())
            .await
            .with_context(|| format!("write patched file: {raw_path}"))?;
        Ok(json!({
            "path": path.strip_prefix(&self.workspace).unwrap_or(&path),
            "edits_applied": edits.len(),
            "bytes_before": original.len(),
            "bytes_after": updated.len(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn applies_multiple_exact_edits_atomically() {
        let workspace = tempfile::tempdir().unwrap();
        let path = workspace.path().join("sample.txt");
        std::fs::write(&path, "alpha\nbeta\ngamma\n").unwrap();
        let tool = ApplyPatch {
            workspace: workspace.path().to_path_buf(),
        };

        let result = tool
            .execute(json!({
                "path": "sample.txt",
                "edits": [
                    {"old_text": "alpha", "new_text": "first"},
                    {"old_text": "gamma", "new_text": "third"}
                ]
            }))
            .await
            .unwrap();

        assert_eq!(result["edits_applied"], 2);
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            "first\nbeta\nthird\n"
        );
    }

    #[tokio::test]
    async fn ambiguous_or_missing_context_leaves_the_file_unchanged() {
        let workspace = tempfile::tempdir().unwrap();
        let path = workspace.path().join("sample.txt");
        let original = "same\nsame\nlast\n";
        std::fs::write(&path, original).unwrap();
        let tool = ApplyPatch {
            workspace: workspace.path().to_path_buf(),
        };

        let error = tool
            .execute(json!({
                "path": "sample.txt",
                "edits": [
                    {"old_text": "last", "new_text": "changed"},
                    {"old_text": "same", "new_text": "duplicate"}
                ]
            }))
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("matched 2 times"));
        assert_eq!(std::fs::read_to_string(path).unwrap(), original);
    }
}
