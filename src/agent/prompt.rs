use anyhow::Result;
use std::path::Path;

const BASE: &str = r#"You are a coding agent operating inside the user's repository.

Rules:
- Inspect before editing. Keep changes focused and explain what changed.
- Use the available tools for file inspection, search, and edits.
- Never claim a command or test ran unless you ran it.
- Do not modify files outside the configured workspace.
- Ask for confirmation before destructive commands or broad changes.
- Prefer small, reversible edits and run focused validation after edits.
"#;

pub fn build(workspace: &Path) -> Result<String> {
    let mut prompt = format!("{}\nWorkspace: {}\n", BASE, workspace.display());
    for name in ["AGENTS.md", "README.md"] {
        let path = workspace.join(name);
        if path.is_file() {
            let content = std::fs::read_to_string(&path)?;
            prompt.push_str(&format!("\n## {}\n{}\n", name, truncate(&content, 8_000)));
        }
    }
    Ok(prompt)
}

fn truncate(value: &str, max: usize) -> String {
    value.chars().take(max).collect()
}
