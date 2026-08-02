//! Coding tool interface and registry.

mod command;
mod filesystem;

use anyhow::Result;
use async_trait::async_trait;
use command::RunCommand;
use filesystem::{ListDir, ReadFile, SearchText, WriteFile};
use serde_json::Value;
use std::{path::PathBuf, sync::Arc};

use crate::llm::{FunctionDefinition, ToolDefinition};

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters(&self) -> Value;
    fn requires_approval(&self) -> bool {
        false
    }
    async fn execute(&self, args: Value) -> Result<Value>;
}

#[derive(Clone)]
pub struct ToolRegistry {
    tools: Vec<Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn coding_defaults(workspace: impl Into<PathBuf>) -> Self {
        let workspace = workspace.into();
        Self {
            tools: vec![
                Arc::new(ReadFile {
                    workspace: workspace.clone(),
                }),
                Arc::new(WriteFile {
                    workspace: workspace.clone(),
                }),
                Arc::new(ListDir {
                    workspace: workspace.clone(),
                }),
                Arc::new(SearchText {
                    workspace: workspace.clone(),
                }),
                Arc::new(RunCommand { workspace }),
            ],
        }
    }
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools
            .iter()
            .map(|tool| ToolDefinition {
                r#type: "function".into(),
                function: FunctionDefinition {
                    name: tool.name().into(),
                    description: tool.description().into(),
                    parameters: tool.parameters(),
                },
            })
            .collect()
    }
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.iter().find(|tool| tool.name() == name).cloned()
    }
    pub fn requires_approval(&self, name: &str) -> Option<bool> {
        self.tools
            .iter()
            .find(|tool| tool.name() == name)
            .map(|tool| tool.requires_approval())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_tools_do_not_require_approval() {
        let registry = ToolRegistry::coding_defaults(PathBuf::from("."));

        assert_eq!(registry.requires_approval("read_file"), Some(false));
        assert_eq!(registry.requires_approval("list_dir"), Some(false));
        assert_eq!(registry.requires_approval("search_text"), Some(false));
        assert_eq!(registry.requires_approval("write_file"), Some(false));
        assert_eq!(registry.requires_approval("run_command"), Some(false));
    }
}
