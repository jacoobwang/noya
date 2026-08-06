use anyhow::Result;
use crate::skills::SkillInfo;
use std::path::Path;

const BASE: &str = r#"You are a coding agent operating inside the user's repository.

Rules:
- Inspect before editing. Keep changes focused and explain what changed.
- Use the available tools for file inspection, search, and edits.
- Prefer apply_patch for changes to existing files; use write_file for new files or intentional full replacements.
- Use read_file ranges for large files, and inspect git_status/git_diff before reporting completion when the workspace is a Git repository.
- Never claim a command or test ran unless you ran it.
- Do not modify files outside the configured workspace.
- Do not run destructive commands or make broad changes unless the user explicitly requested them.
- Prefer small, reversible edits and run focused validation after edits.
"#;

pub fn build(workspace: &Path, skills: &[(&SkillInfo, &str)]) -> Result<String> {
    let mut prompt = format!("{}\nWorkspace: {}\n", BASE, workspace.display());
    for name in ["AGENTS.md", "README.md"] {
        let path = workspace.join(name);
        if path.is_file() {
            let content = std::fs::read_to_string(&path)?;
            prompt.push_str(&format!("\n## {}\n{}\n", name, truncate(&content, 8_000)));
        }
    }
    for (info, body) in skills {
        prompt.push_str(&format!(
            "\n<skill name=\"{}\" source=\"{}\" digest=\"{}\">\n{}\n</skill>\n",
            info.name, info.source, info.digest, body
        ));
    }
    Ok(prompt)
}

fn truncate(value: &str, max: usize) -> String {
    value.chars().take(max).collect()
}
