use crate::llm::{ChatMessage, ToolCall};
use serde::{Deserialize, Serialize};
use std::{fmt, path::PathBuf, str::FromStr};
use time::OffsetDateTime;
use uuid::Uuid;

macro_rules! uuid_id {
    ($name:ident) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub Uuid);

        impl $name {
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }

        impl FromStr for $name {
            type Err = uuid::Error;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Uuid::parse_str(value).map(Self)
            }
        }
    };
}

uuid_id!(SessionId);
uuid_id!(RunId);
uuid_id!(TurnId);

#[derive(Debug, Clone)]
pub struct CreateSession {
    pub workspace: PathBuf,
    pub model: String,
    pub model_id: String,
}

#[derive(Debug, Clone, Default)]
pub struct SessionFilter {
    pub workspace: Option<PathBuf>,
    pub include_archived: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Idle,
    Running,
    Interrupted,
    Corrupt,
    Archived,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSummary {
    pub schema_version: u32,
    pub session_id: SessionId,
    pub title: String,
    pub workspace: PathBuf,
    pub model: String,
    pub model_id: String,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
    pub status: SessionStatus,
    pub last_seq: u64,
    pub completed_turns: usize,
    pub context_epoch: u64,
    #[serde(default)]
    pub compaction_through_seq: Option<u64>,
    pub parent_session_id: Option<SessionId>,
    pub archived: bool,
    #[serde(default)]
    pub active_skills: Vec<ActiveSkillRecord>,
}

#[derive(Debug, Clone)]
pub struct ModelContext {
    pub messages: Vec<ChatMessage>,
    pub estimated_tokens: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeSnapshot {
    pub noya_version: String,
    pub workspace: PathBuf,
    pub model: String,
    pub model_id: String,
    pub system_prompt: String,
    pub tool_names: Vec<String>,
    pub max_tool_loops: usize,
    pub tool_timeout_ms: u64,
    pub max_tool_output_bytes: usize,
    pub temperature: Option<f32>,
    #[serde(default)]
    pub tool_approval_mode: String,
    #[serde(default)]
    pub blocked_tools: Vec<String>,
    #[serde(default)]
    pub active_skills: Vec<ActiveSkillRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveSkillRecord {
    pub name: String,
    pub source: String,
    pub digest: String,
    pub order: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserMessageRecord {
    pub message_id: Uuid,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistantRecord {
    pub message_id: Uuid,
    pub content: String,
    pub reasoning_content: Option<String>,
    pub tool_calls: Vec<ToolCall>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallRecord {
    pub call_id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResultRecord {
    pub call_id: String,
    pub name: String,
    pub result: serde_json::Value,
    pub success: bool,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnFailure {
    pub message: String,
    pub recoverable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionRecord {
    pub summary: String,
    pub through_seq: u64,
    pub through_turn_id: TurnId,
    pub keep_from_turn_id: Option<TurnId>,
    pub source_token_estimate: usize,
    pub summary_model: String,
}

#[derive(Debug, Clone)]
pub(crate) struct CompactionPlan {
    pub source: String,
    pub through_seq: u64,
    pub through_turn_id: TurnId,
    pub keep_from_turn_id: Option<TurnId>,
    pub source_token_estimate: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    Markdown,
    Jsonl,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TranscriptKind {
    User,
    Agent,
    Tool,
    System,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptItem {
    pub id: Uuid,
    pub kind: TranscriptKind,
    pub content: String,
    pub turn_id: Option<TurnId>,
    pub tool_call_id: Option<String>,
    pub interrupted: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Transcript {
    pub items: Vec<TranscriptItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub summary: SessionSummary,
    pub transcript: Transcript,
}

impl Transcript {
    pub fn to_markdown(&self, summary: &SessionSummary) -> String {
        let mut output = format!(
            "# {}\n\n- Session: `{}`\n- Workspace: `{}`\n- Model: `{}` / `{}`\n\n",
            summary.title,
            summary.session_id,
            summary.workspace.display(),
            summary.model,
            summary.model_id
        );
        for item in &self.items {
            let label = match item.kind {
                TranscriptKind::User => "User",
                TranscriptKind::Agent => "Noya",
                TranscriptKind::Tool => "Tool",
                TranscriptKind::System => "System",
                TranscriptKind::Error => "Error",
            };
            output.push_str(&format!("## {label}\n\n{}\n\n", item.content));
        }
        output
    }
}
