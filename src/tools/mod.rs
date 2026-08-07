//! Coding tool interface and registry.

mod command;
mod filesystem;
mod git;
mod lsp;
mod patch;

use anyhow::Result;
use async_trait::async_trait;
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use command::RunCommand;
use filesystem::{ListDir, ReadFile, SearchText, WriteFile};
use git::{GitDiff, GitStatus};
use lsp::CodeNavigation;
use patch::ApplyPatch;
use serde_json::Value;
use std::{collections::BTreeSet, path::PathBuf, sync::Arc};

use crate::llm::{FunctionDefinition, ToolDefinition};

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters(&self) -> Value;
    fn requires_approval(&self) -> bool {
        false
    }
    fn risk(&self) -> ToolRisk {
        ToolRisk::ReadOnly
    }
    async fn execute(&self, args: Value) -> Result<Value>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolRisk {
    ReadOnly,
    Mutating,
    Dangerous,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum ToolApprovalMode {
    Never,
    #[default]
    Mutating,
    Always,
}

impl ToolApprovalMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Never => "never",
            Self::Mutating => "mutating",
            Self::Always => "always",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolPolicy {
    pub approval_mode: ToolApprovalMode,
    pub blocked_tools: BTreeSet<String>,
}

impl ToolPolicy {
    pub fn with_blocked_tools<I, S>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.blocked_tools = names
            .into_iter()
            .map(Into::into)
            .map(|name| name.trim().to_string())
            .filter(|name| !name.is_empty())
            .collect();
        self
    }

    pub fn is_blocked(&self, name: &str) -> bool {
        self.blocked_tools.contains(name)
    }

    pub fn requires_approval(&self, risk: ToolRisk) -> bool {
        match self.approval_mode {
            ToolApprovalMode::Never => false,
            ToolApprovalMode::Mutating => !matches!(risk, ToolRisk::ReadOnly),
            ToolApprovalMode::Always => true,
        }
    }
}

#[derive(Clone)]
pub struct ToolRegistry {
    tools: Vec<Arc<dyn Tool>>,
    policy: ToolPolicy,
}

impl ToolRegistry {
    pub fn coding_defaults(workspace: impl Into<PathBuf>) -> Self {
        let workspace = workspace.into();
        Self {
            tools: vec![
                Arc::new(ReadFile {
                    workspace: workspace.clone(),
                }),
                Arc::new(ListDir {
                    workspace: workspace.clone(),
                }),
                Arc::new(SearchText {
                    workspace: workspace.clone(),
                }),
                Arc::new(CodeNavigation::new(workspace.clone())),
                Arc::new(ApplyPatch {
                    workspace: workspace.clone(),
                }),
                Arc::new(WriteFile {
                    workspace: workspace.clone(),
                }),
                Arc::new(GitStatus {
                    workspace: workspace.clone(),
                }),
                Arc::new(GitDiff {
                    workspace: workspace.clone(),
                }),
                Arc::new(RunCommand { workspace }),
            ],
            policy: ToolPolicy::default(),
        }
    }

    pub fn with_policy(mut self, policy: ToolPolicy) -> Self {
        self.policy = policy;
        self
    }

    pub fn policy(&self) -> &ToolPolicy {
        &self.policy
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
            .map(|tool| tool.requires_approval() || self.policy.requires_approval(tool.risk()))
    }

    pub fn is_blocked(&self, name: &str) -> bool {
        self.policy.is_blocked(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_policy_requires_approval_for_mutating_tools() {
        let registry = ToolRegistry::coding_defaults(PathBuf::from("."));

        assert_eq!(registry.requires_approval("read_file"), Some(false));
        assert_eq!(registry.requires_approval("list_dir"), Some(false));
        assert_eq!(registry.requires_approval("search_text"), Some(false));
        assert_eq!(registry.requires_approval("code_navigation"), Some(false));
        assert_eq!(registry.requires_approval("apply_patch"), Some(true));
        assert_eq!(registry.requires_approval("git_status"), Some(false));
        assert_eq!(registry.requires_approval("git_diff"), Some(false));
        assert_eq!(registry.requires_approval("write_file"), Some(true));
        assert_eq!(registry.requires_approval("run_command"), Some(true));
    }

    #[test]
    fn policy_can_disable_approval_and_block_tools() {
        let policy = ToolPolicy {
            approval_mode: ToolApprovalMode::Never,
            blocked_tools: ["run_command".to_string()].into_iter().collect(),
        };
        let registry = ToolRegistry::coding_defaults(PathBuf::from(".")).with_policy(policy);

        assert_eq!(registry.requires_approval("write_file"), Some(false));
        assert!(registry.is_blocked("run_command"));
    }

    #[test]
    fn coding_defaults_publish_the_complete_tool_catalog() {
        let registry = ToolRegistry::coding_defaults(PathBuf::from("."));
        let names = registry
            .definitions()
            .into_iter()
            .map(|definition| definition.function.name)
            .collect::<Vec<_>>();

        assert_eq!(
            names,
            [
                "read_file",
                "list_dir",
                "search_text",
                "code_navigation",
                "apply_patch",
                "write_file",
                "git_status",
                "git_diff",
                "run_command",
            ]
        );
    }
}
