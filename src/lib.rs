//! Noya standalone coding-agent engine.
//!
//! The public seam is [`Agent`]. It owns the turn loop while LLM models and
//! tools remain adapters, so the CLI can eventually be replaced by an HTTP or
//! desktop host without moving coding-agent policy into the transport.

pub mod agent;
pub mod llm;
pub mod model;
pub mod tools;
pub mod tui;

pub use agent::{
    Agent, AgentConfig, AgentEvent, ApprovalDecision, ApprovalPrompt, ApprovalRequest, TurnControl,
};
pub use llm::{ChatMessage, ChatRequest, ChatResponse, ChatStreamResponse, LlmClient, LlmEvent};
pub use tools::{Tool, ToolRegistry};
