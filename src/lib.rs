//! Noya standalone coding-agent engine.
//!
//! The public seam is [`Agent`]. It owns the turn loop while LLM models and
//! tools remain adapters, so the CLI can eventually be replaced by an HTTP or
//! desktop host without moving coding-agent policy into the transport.

pub mod agent;
pub mod llm;
pub mod model;
pub mod session;
pub mod skills;
pub mod tools;
pub mod tui;

pub use agent::{
    Agent, AgentConfig, AgentDiagnostics, AgentEvent, ApprovalDecision, ApprovalPrompt,
    ApprovalRequest, AutonomousConfig, AutonomousReport, AutonomousStatus, AutonomousStopReason,
    QualityGateConfig, QualityGateFailure, TurnControl, TurnDiagnostics,
};
pub use llm::{
    ChatMessage, ChatRequest, ChatResponse, ChatStreamResponse, CostRates, LlmClient, LlmEvent,
    Usage,
};
pub use tools::{Tool, ToolApprovalMode, ToolPolicy, ToolRegistry, ToolRisk};
pub use skills::{SkillInfo, SkillRegistry, SkillSource};
