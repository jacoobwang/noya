//! Agent runtime: turn orchestration behind a small event-driven interface.

mod control;
mod event;
mod prompt;

pub use control::{ApprovalDecision, ApprovalPrompt, ApprovalRequest, TurnControl};
pub use event::AgentEvent;

use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};
use uuid::Uuid;

use crate::{
    llm::{ChatMessage, LlmClient, LlmEvent},
    model::Model,
    session::{
        ActiveSkillRecord, AssistantRecord, CompactionRecord, RunId, RuntimeSnapshot, Session,
        SessionSummary, ToolCallRecord, ToolResultRecord, Transcript, TurnFailure, TurnId,
    },
    skills::{SkillInfo, SkillRegistry},
    tools::ToolRegistry,
};

const FINAL_RESPONSE_INSTRUCTION: &str = "The tool-call limit has been reached. Do not call any more tools. Use the information already available in the conversation to give the user the best possible final answer, and clearly state anything that could not be verified.";
const TOOL_LIMIT_FALLBACK: &str = "The tool-call limit was reached, and the model did not provide a final answer from the available results.";

#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub workspace: PathBuf,
    pub max_tool_loops: usize,
    pub tool_timeout: Duration,
    pub max_tool_output_bytes: usize,
    pub temperature: f32,
}

pub struct Agent {
    config: AgentConfig,
    llm: LlmClient,
    tools: ToolRegistry,
    skills: SkillRegistry,
    session: Session,
    system_prompt: String,
    run_id: RunId,
}

impl Agent {
    pub fn new(config: AgentConfig, llm: LlmClient) -> Result<Self> {
        let session = Session::ephemeral(
            config.workspace.clone(),
            "custom".to_string(),
            llm.model_id().to_string(),
        )?;
        Self::with_session(config, llm, session)
    }

    pub fn with_session(config: AgentConfig, llm: LlmClient, session: Session) -> Result<Self> {
        let model = session.summary().model;
        Self::with_session_for_model(config, llm, session, model)
    }

    pub fn with_session_for_model(
        mut config: AgentConfig,
        llm: LlmClient,
        mut session: Session,
        model: impl Into<String>,
    ) -> Result<Self> {
        ensure!(
            !config.tool_timeout.is_zero(),
            "tool timeout must be greater than zero"
        );
        ensure!(
            config.max_tool_output_bytes >= 256,
            "max tool output must be at least 256 bytes"
        );
        config.workspace = config
            .workspace
            .canonicalize()
            .with_context(|| format!("canonicalize workspace {}", config.workspace.display()))?;
        ensure!(
            session.summary().workspace == config.workspace,
            "session workspace does not match agent workspace"
        );
        let model = model.into();
        session.change_model(model.clone(), llm.model_id().to_string())?;
        let skills = SkillRegistry::discover(&config.workspace)?;
        for warning in skills.warnings() {
            tracing::warn!("{warning}");
        }
        let active = active_skill_prompts(&skills, &session.active_skills())?;
        let system = prompt::build(&config.workspace, &active)?;
        let tools = ToolRegistry::coding_defaults(config.workspace.clone());
        let active_records = session.active_skills();
        let run_id = start_runtime(
            &mut session,
            &config,
            &tools,
            &system,
            &model,
            llm.model_id(),
            &active_records,
        )?;
        Ok(Self {
            tools,
            skills,
            config,
            llm,
            session,
            system_prompt: system,
            run_id,
        })
    }

    pub fn session_summary(&self) -> SessionSummary {
        self.session.summary()
    }

    pub fn transcript(&self) -> Transcript {
        self.session.transcript()
    }

    pub fn context_token_estimate(&self) -> usize {
        self.session.context().estimated_tokens
    }

    pub fn session_log_path(&self) -> Option<PathBuf> {
        self.session.log_path()
    }

    pub fn list_skills(&self) -> (Vec<SkillInfo>, Vec<String>) {
        (self.skills.list(), self.skills.warnings().to_vec())
    }

    pub fn active_skills(&self) -> Vec<ActiveSkillRecord> {
        self.session.active_skills()
    }

    pub fn activate_skill(&mut self, name: &str) -> Result<SkillInfo> {
        let info = self
            .skills
            .get(name)
            .cloned()
            .with_context(|| format!("Skill '{name}' was not found"))?;
        let mut active = self.session.active_skills();
        if active.iter().any(|active| active.name == name) {
            return Ok(info);
        }
        let record = ActiveSkillRecord {
            name: info.name.clone(),
            source: info.source.to_string(),
            digest: info.digest.clone(),
            order: active.len(),
        };
        active.push(record.clone());
        let prompts = active_skill_prompts(&self.skills, &active)?;
        let system = prompt::build(&self.config.workspace, &prompts)?;
        self.session.activate_skill(record)?;
        self.system_prompt = system;
        self.restart_runtime()?;
        Ok(info)
    }

    pub fn deactivate_skill(&mut self, name: &str) -> Result<()> {
        ensure!(
            self.session.active_skills().iter().any(|active| active.name == name),
            "Skill '{name}' is not active"
        );
        self.session.deactivate_skill(name)?;
        self.system_prompt = self.build_system_prompt()?;
        self.restart_runtime()?;
        Ok(())
    }

    pub fn skill_info(&self, name: &str) -> Result<SkillInfo> {
        self.skills
            .get(name)
            .cloned()
            .with_context(|| format!("Skill '{name}' was not found"))
    }

    pub fn rename_session(&mut self, title: impl Into<String>) -> Result<()> {
        self.session.rename(title)
    }

    pub fn replace_session(&mut self, mut session: Session) -> Result<()> {
        ensure!(
            session.summary().workspace == self.config.workspace,
            "session workspace does not match agent workspace"
        );
        let model = self.session.summary().model;
        session.change_model(model.clone(), self.llm.model_id().to_string())?;
        let active = active_skill_prompts(&self.skills, &session.active_skills())?;
        let system = prompt::build(&self.config.workspace, &active)?;
        let active_records = session.active_skills();
        let run_id = start_runtime(
            &mut session,
            &self.config,
            &self.tools,
            &system,
            &model,
            self.llm.model_id(),
            &active_records,
        )?;
        self.session = session;
        self.system_prompt = system;
        self.run_id = run_id;
        Ok(())
    }

    pub fn switch_model(&mut self, model: impl Into<String>, llm: LlmClient) -> Result<()> {
        let model = model.into();
        let model_id = llm.model_id().to_string();
        self.session.change_model(model.clone(), model_id.clone())?;
        let active_records = self.session.active_skills();
        let run_id = start_runtime(
            &mut self.session,
            &self.config,
            &self.tools,
            &self.system_prompt,
            &model,
            &model_id,
            &active_records,
        )?;
        self.llm = llm;
        self.run_id = run_id;
        Ok(())
    }

    pub fn retry_input(&self) -> Option<String> {
        self.session.retry_input()
    }

    pub async fn compact(&mut self) -> Result<bool> {
        let Some(plan) = self.session.compaction_plan() else {
            return Ok(false);
        };
        let response = self
            .llm
            .complete(
                vec![
                    ChatMessage {
                        role: "system".to_string(),
                        content: "Summarize earlier coding-agent context for future continuation. Preserve decisions, changed files, commands, results, unresolved work, constraints, and exact identifiers. Do not invent facts.".to_string(),
                        reasoning_content: None,
                        tool_call_id: None,
                        tool_calls: None,
                    },
                    ChatMessage {
                        role: "user".to_string(),
                        content: plan.source,
                        reasoning_content: None,
                        tool_call_id: None,
                        tool_calls: None,
                    },
                ],
                Vec::new(),
                self.config.temperature,
            )
            .await
            .context("generate session compaction summary")?;
        let summary = response
            .choices
            .into_iter()
            .next()
            .and_then(|choice| choice.message.content)
            .filter(|content| !content.trim().is_empty())
            .context("compaction model returned no summary")?;
        self.session.apply_compaction(CompactionRecord {
            summary,
            through_seq: plan.through_seq,
            through_turn_id: plan.through_turn_id,
            keep_from_turn_id: plan.keep_from_turn_id,
            source_token_estimate: plan.source_token_estimate,
            summary_model: self.llm.model_id().to_string(),
        })?;
        Ok(true)
    }

    pub async fn auto_compact_if_needed(&mut self) -> Result<bool> {
        let enabled = std::env::var("NOYA_AUTO_COMPACT")
            .map(|value| {
                !matches!(
                    value.trim().to_ascii_lowercase().as_str(),
                    "0" | "false" | "off"
                )
            })
            .unwrap_or(true);
        if !enabled {
            return Ok(false);
        }
        let Some(context_window) = self
            .session
            .summary()
            .model
            .parse::<Model>()
            .ok()
            .and_then(Model::context_window)
        else {
            return Ok(false);
        };
        if !self.session.should_auto_compact(context_window) {
            return Ok(false);
        }
        self.compact().await
    }

    pub async fn turn<F>(&mut self, input: impl Into<String>, mut emit: F) -> Result<()>
    where
        F: FnMut(AgentEvent),
    {
        self.turn_with_control(input, &mut emit, TurnControl::non_interactive())
            .await
    }

    pub async fn turn_with_control<F>(
        &mut self,
        input: impl Into<String>,
        mut emit: F,
        control: TurnControl,
    ) -> Result<()>
    where
        F: FnMut(AgentEvent),
    {
        let turn_id = self.session.begin_turn(input.into())?;
        emit(AgentEvent::TurnStarted { turn_id });
        let result = self.run_turn(turn_id, &mut emit, &control).await;
        match result {
            Ok(()) => {
                self.session.finish_turn(&turn_id)?;
                emit(AgentEvent::TurnCompleted { turn_id });
                Ok(())
            }
            Err(error) => {
                let message = error.to_string();
                if control.is_cancelled() {
                    self.session
                        .cancel_turn(&turn_id, message.clone())
                        .context("persist cancelled turn")?;
                } else {
                    self.session
                        .fail_turn(
                            &turn_id,
                            TurnFailure {
                                message: message.clone(),
                                recoverable: true,
                            },
                        )
                        .context("persist failed turn")?;
                }
                Err(error)
            }
        }
    }

    async fn run_turn<F>(
        &mut self,
        turn_id: TurnId,
        emit: &mut F,
        control: &TurnControl,
    ) -> Result<()>
    where
        F: FnMut(AgentEvent),
    {
        let mut tool_loops = 0;
        loop {
            let force_final_response = tool_loops >= self.config.max_tool_loops;
            let mut request_messages = vec![ChatMessage {
                role: "system".into(),
                content: self.system_prompt.clone(),
                reasoning_content: None,
                tool_call_id: None,
                tool_calls: None,
            }];
            request_messages.extend(self.session.context().messages);
            let tool_definitions = if force_final_response {
                request_messages[0].content.push_str("\n\n");
                request_messages[0]
                    .content
                    .push_str(FINAL_RESPONSE_INSTRUCTION);
                Vec::new()
            } else {
                self.tools.definitions()
            };
            let message_id = Uuid::new_v4();
            let mut streamed_content = String::new();
            let mut checkpoint_warning_emitted = false;
            let run_id = self.run_id;
            let session = &mut self.session;
            let response = tokio::select! {
                response = self.llm.complete_stream(
                    request_messages,
                    tool_definitions,
                    self.config.temperature,
                    |event| match event {
                        LlmEvent::TextDelta(chunk) => {
                            streamed_content.push_str(&chunk);
                            emit(AgentEvent::TextDelta {
                                turn_id,
                                message_id,
                                chunk,
                                is_final: false,
                            });
                            if let Err(error) = session.checkpoint_draft(
                                run_id,
                                turn_id,
                                message_id,
                                &streamed_content,
                                false,
                            ) && !checkpoint_warning_emitted {
                                checkpoint_warning_emitted = true;
                                emit(AgentEvent::Error {
                                    turn_id: Some(turn_id),
                                    message: format!("crash recovery checkpoint unavailable: {error}"),
                                    recoverable: true,
                                });
                            }
                        }
                    },
                ) => response?,
                _ = control.cancellation.cancelled() => anyhow::bail!("turn cancelled"),
            };
            if !streamed_content.is_empty()
                && let Err(error) =
                    session.checkpoint_draft(run_id, turn_id, message_id, &streamed_content, true)
                && !checkpoint_warning_emitted
            {
                emit(AgentEvent::Error {
                    turn_id: Some(turn_id),
                    message: format!("crash recovery checkpoint unavailable: {error}"),
                    recoverable: true,
                });
            }
            if force_final_response && !response.tool_calls.is_empty() {
                let content = if response.content.trim().is_empty() {
                    emit(AgentEvent::TextDelta {
                        turn_id,
                        message_id,
                        chunk: TOOL_LIMIT_FALLBACK.into(),
                        is_final: false,
                    });
                    TOOL_LIMIT_FALLBACK.to_string()
                } else {
                    response.content
                };
                self.session.record_assistant(AssistantRecord {
                    message_id,
                    content,
                    reasoning_content: response.reasoning_content,
                    tool_calls: Vec::new(),
                })?;
                emit(AgentEvent::TextDelta {
                    turn_id,
                    message_id,
                    chunk: String::new(),
                    is_final: true,
                });
                return Ok(());
            }
            let has_tool_calls = !response.tool_calls.is_empty();
            self.session.record_assistant(AssistantRecord {
                message_id,
                content: response.content.clone(),
                reasoning_content: response.reasoning_content,
                tool_calls: response.tool_calls.clone(),
            })?;
            emit(AgentEvent::TextDelta {
                turn_id,
                message_id,
                chunk: String::new(),
                is_final: true,
            });
            if !has_tool_calls {
                return Ok(());
            }
            tool_loops += 1;
            for call in response.tool_calls {
                let mut args: Value = serde_json::from_str(&call.function.arguments)
                    .context("decode tool arguments")?;
                if self
                    .tools
                    .requires_approval(&call.function.name)
                    .unwrap_or(false)
                {
                    let request = ApprovalRequest {
                        request_id: Uuid::new_v4().to_string(),
                        call_id: call.id.clone(),
                        tool_name: call.function.name.clone(),
                        arguments: args.clone(),
                    };
                    emit(AgentEvent::ApprovalRequired {
                        turn_id,
                        request_id: request.request_id.clone(),
                        call_id: request.call_id.clone(),
                        tool_name: request.tool_name.clone(),
                        arguments: request.arguments.clone(),
                    });
                    match control.request_approval(request).await {
                        ApprovalDecision::Approve => {}
                        ApprovalDecision::Modify(modified) => args = modified,
                        ApprovalDecision::Reject => {
                            let result =
                                serde_json::json!({"error": "tool execution rejected by user"});
                            self.session.record_tool_started(ToolCallRecord {
                                call_id: call.id.clone(),
                                name: call.function.name.clone(),
                                arguments: args,
                            })?;
                            self.session.record_tool_finished(ToolResultRecord {
                                call_id: call.id.clone(),
                                name: call.function.name.clone(),
                                result: result.clone(),
                                success: false,
                                duration_ms: 0,
                            })?;
                            emit(AgentEvent::ToolFinished {
                                turn_id,
                                call_id: call.id.clone(),
                                name: call.function.name.clone(),
                                result: result.clone(),
                                success: false,
                            });
                            continue;
                        }
                    }
                }
                self.session.record_tool_started(ToolCallRecord {
                    call_id: call.id.clone(),
                    name: call.function.name.clone(),
                    arguments: args.clone(),
                })?;
                emit(AgentEvent::ToolStarted {
                    turn_id,
                    call_id: call.id.clone(),
                    name: call.function.name.clone(),
                    arguments: args.clone(),
                });
                let started = Instant::now();
                let (result, success) = tokio::select! {
                    result = execute_tool(
                        &self.tools,
                        &call.function.name,
                        args,
                        self.config.tool_timeout,
                        self.config.max_tool_output_bytes,
                    ) => result,
                        _ = control.cancellation.cancelled() => anyhow::bail!("turn cancelled"),
                };
                self.session.record_tool_finished(ToolResultRecord {
                    call_id: call.id.clone(),
                    name: call.function.name.clone(),
                    result: result.clone(),
                    success,
                    duration_ms: started.elapsed().as_millis().min(u64::MAX as u128) as u64,
                })?;
                emit(AgentEvent::ToolFinished {
                    turn_id,
                    call_id: call.id.clone(),
                    name: call.function.name.clone(),
                    result: result.clone(),
                    success,
                });
            }
        }
    }

    pub fn reset(&mut self) -> Result<()> {
        self.session.reset_context()
    }

    fn build_system_prompt(&self) -> Result<String> {
        let active = self.session.active_skills();
        let prompts = active_skill_prompts(&self.skills, &active)?;
        prompt::build(&self.config.workspace, &prompts)
    }

    fn restart_runtime(&mut self) -> Result<()> {
        let active_records = self.session.active_skills();
        let model = self.session.summary().model;
        self.run_id = start_runtime(
            &mut self.session,
            &self.config,
            &self.tools,
            &self.system_prompt,
            &model,
            self.llm.model_id(),
            &active_records,
        )?;
        Ok(())
    }
}

fn start_runtime(
    session: &mut Session,
    config: &AgentConfig,
    tools: &ToolRegistry,
    system_prompt: &str,
    model: &str,
    model_id: &str,
    active_skills: &[ActiveSkillRecord],
) -> Result<RunId> {
    session.start_runtime(RuntimeSnapshot {
        noya_version: env!("CARGO_PKG_VERSION").to_string(),
        workspace: config.workspace.clone(),
        model: model.to_string(),
        model_id: model_id.to_string(),
        system_prompt: system_prompt.to_string(),
        tool_names: tools
            .definitions()
            .into_iter()
            .map(|definition| definition.function.name)
            .collect(),
        max_tool_loops: config.max_tool_loops,
        tool_timeout_ms: config.tool_timeout.as_millis().min(u64::MAX as u128) as u64,
        max_tool_output_bytes: config.max_tool_output_bytes,
        temperature: Some(config.temperature),
        active_skills: active_skills.to_vec(),
    })
}

fn active_skill_prompts<'a>(
    registry: &'a SkillRegistry,
    active: &[ActiveSkillRecord],
) -> Result<Vec<(&'a SkillInfo, &'a str)>> {
    let mut ordered = active.to_vec();
    ordered.sort_by_key(|skill| skill.order);
    ordered
        .iter()
        .map(|record| {
            let info = registry
                .get(&record.name)
                .with_context(|| format!("active Skill '{}' is no longer available", record.name))?;
            ensure!(
                info.digest == record.digest,
                "active Skill '{}' changed on disk (expected {}, found {})",
                record.name,
                record.digest,
                info.digest
            );
            let body = registry
                .body(&record.name)
                .with_context(|| format!("active Skill '{}' has no body", record.name))?;
            Ok((info, body))
        })
        .collect()
}

async fn execute_tool(
    tools: &ToolRegistry,
    name: &str,
    args: Value,
    timeout: Duration,
    max_output_bytes: usize,
) -> (Value, bool) {
    let Some(tool) = tools.get(name) else {
        return (
            serde_json::json!({"error": format!("unknown tool: {name}")}),
            false,
        );
    };
    let (result, success) = match tokio::time::timeout(timeout, tool.execute(args)).await {
        Ok(Ok(result)) => (result, true),
        Ok(Err(error)) => (serde_json::json!({"error": error.to_string()}), false),
        Err(_) => (
            serde_json::json!({
                "error": format!("tool timed out after {} ms", timeout.as_millis()),
                "timeout_ms": timeout.as_millis(),
            }),
            false,
        ),
    };
    (limit_tool_result(result, max_output_bytes), success)
}

fn limit_tool_result(result: Value, max_bytes: usize) -> Value {
    let rendered = serde_json::to_string(&result).unwrap_or_else(|_| result.to_string());
    if rendered.len() <= max_bytes {
        return result;
    }

    let mut preview_bytes = rendered.len().min(max_bytes.saturating_sub(128));
    loop {
        while !rendered.is_char_boundary(preview_bytes) {
            preview_bytes = preview_bytes.saturating_sub(1);
        }
        let limited = serde_json::json!({
            "truncated": true,
            "original_bytes": rendered.len(),
            "preview": &rendered[..preview_bytes],
        });
        if serde_json::to_vec(&limited).map_or(true, |value| value.len() <= max_bytes)
            || preview_bytes == 0
        {
            return limited;
        }
        preview_bytes = preview_bytes.saturating_mul(3) / 4;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{CreateSession, SessionManager};
    use axum::{
        Router,
        body::Body,
        http::{Response, header::CONTENT_TYPE},
        routing::post,
    };
    use reqwest::Client;

    #[test]
    fn agent_config_is_workspace_scoped() {
        let c = AgentConfig {
            workspace: PathBuf::from("repo"),
            max_tool_loops: 8,
            tool_timeout: Duration::from_secs(120),
            max_tool_output_bytes: 32_768,
            temperature: 0.2,
        };
        assert_eq!(c.workspace, PathBuf::from("repo"));
    }

    #[test]
    fn model_switch_replaces_the_client_and_persists_session_metadata() {
        let data = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let manager = SessionManager::at(data.path());
        let session = manager
            .create(CreateSession {
                workspace: workspace.path().to_path_buf(),
                model: "deepseek".to_string(),
                model_id: "deepseek-v4-flash".to_string(),
            })
            .unwrap();
        let session_id = *session.id();
        let mut agent = Agent::with_session_for_model(
            AgentConfig {
                workspace: workspace.path().to_path_buf(),
                max_tool_loops: 4,
                tool_timeout: Duration::from_secs(120),
                max_tool_output_bytes: 32_768,
                temperature: 0.2,
            },
            LlmClient::new("https://deepseek.example", "secret", "deepseek-v4-flash"),
            session,
            "deepseek",
        )
        .unwrap();

        agent
            .switch_model(
                "qwen",
                LlmClient::new("https://qwen.example", "other-secret", "qwen3-coder-plus"),
            )
            .unwrap();

        let summary = agent.session_summary();
        assert_eq!(summary.model, "qwen");
        assert_eq!(summary.model_id, "qwen3-coder-plus");
        drop(agent);

        let session_directory = data.path().join("sessions").join(session_id.to_string());
        for entry in std::fs::read_dir(&session_directory).unwrap() {
            let path = entry.unwrap().path();
            if path.is_file() {
                let content = std::fs::read(&path).unwrap();
                assert!(!String::from_utf8_lossy(&content).contains("other-secret"));
            }
        }

        let reopened = manager.open(session_id).unwrap();
        assert_eq!(reopened.summary().model, "qwen");
        assert_eq!(reopened.summary().model_id, "qwen3-coder-plus");
    }

    async fn stream_response() -> Response<Body> {
        Response::builder()
            .header(CONTENT_TYPE, "text/event-stream")
            .body(Body::from(concat!(
                "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n\n",
                "data: [DONE]\n\n"
            )))
            .unwrap()
    }

    async fn write_file_stream(
        axum::extract::State(counter): axum::extract::State<
            std::sync::Arc<std::sync::atomic::AtomicUsize>,
        >,
    ) -> Response<Body> {
        use std::sync::atomic::Ordering;
        let call = counter.fetch_add(1, Ordering::SeqCst);
        let body = if call == 0 {
            concat!(
                "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"write-1\",\"function\":{\"name\":\"write_file\",\"arguments\":\"{\\\"path\\\":\\\"note.txt\\\",\\\"content\\\":\\\"hello\\\"}\"}}]}}]}\n\n",
                "data: [DONE]\n\n"
            )
        } else {
            concat!(
                "data: {\"choices\":[{\"delta\":{\"content\":\"Written.\"}}]}\n\n",
                "data: [DONE]\n\n"
            )
        };
        Response::builder()
            .header(CONTENT_TYPE, "text/event-stream")
            .body(Body::from(body))
            .unwrap()
    }

    async fn slow_response() -> Response<Body> {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        stream_response().await
    }

    fn tool_call_stream(call: usize) -> String {
        format!(
            "data: {{\"choices\":[{{\"delta\":{{\"tool_calls\":[{{\"index\":0,\"id\":\"list-{call}\",\"function\":{{\"name\":\"list_dir\",\"arguments\":\"{{}}\"}}}}]}}}}]}}\n\ndata: [DONE]\n\n"
        )
    }

    async fn final_response_after_two_tools(
        axum::extract::State(counter): axum::extract::State<
            std::sync::Arc<std::sync::atomic::AtomicUsize>,
        >,
    ) -> Response<Body> {
        use std::sync::atomic::Ordering;
        let call = counter.fetch_add(1, Ordering::SeqCst);
        let body = if call < 2 {
            tool_call_stream(call)
        } else {
            concat!(
                "data: {\"choices\":[{\"delta\":{\"content\":\"Finished.\"}}]}\n\n",
                "data: [DONE]\n\n"
            )
            .to_string()
        };
        Response::builder()
            .header(CONTENT_TYPE, "text/event-stream")
            .body(Body::from(body))
            .unwrap()
    }

    async fn endless_tool_responses(
        axum::extract::State(counter): axum::extract::State<
            std::sync::Arc<std::sync::atomic::AtomicUsize>,
        >,
        axum::Json(request): axum::Json<Value>,
    ) -> Response<Body> {
        use std::sync::atomic::Ordering;
        let call = counter.fetch_add(1, Ordering::SeqCst);
        let tools_available = request["tools"]
            .as_array()
            .is_some_and(|tools| !tools.is_empty());
        let body = if tools_available {
            tool_call_stream(call)
        } else {
            concat!(
                "data: {\"choices\":[{\"delta\":{\"content\":\"Summarized without more tools.\"}}]}\n\n",
                "data: [DONE]\n\n"
            )
            .to_string()
        };
        Response::builder()
            .header(CONTENT_TYPE, "text/event-stream")
            .body(Body::from(body))
            .unwrap()
    }

    async fn tool_response_even_without_definitions(
        axum::extract::State(counter): axum::extract::State<
            std::sync::Arc<std::sync::atomic::AtomicUsize>,
        >,
    ) -> Response<Body> {
        use std::sync::atomic::Ordering;
        let call = counter.fetch_add(1, Ordering::SeqCst);
        Response::builder()
            .header(CONTENT_TYPE, "text/event-stream")
            .body(Body::from(tool_call_stream(call)))
            .unwrap()
    }

    #[tokio::test]
    async fn turn_emits_streaming_text_in_order() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route("/chat/completions", post(stream_response)),
            )
            .await
            .unwrap();
        });
        let workspace = tempfile::tempdir().unwrap();
        let llm = LlmClient::with_client(
            Client::builder().no_proxy().build().unwrap(),
            format!("http://{address}"),
            "test-key",
            "test-model",
        );
        let mut agent = Agent::new(
            AgentConfig {
                workspace: workspace.path().to_path_buf(),
                max_tool_loops: 4,
                tool_timeout: Duration::from_secs(120),
                max_tool_output_bytes: 32_768,
                temperature: 0.2,
            },
            llm,
        )
        .unwrap();
        let mut events = Vec::new();

        agent
            .turn("hello", |event| events.push(event))
            .await
            .unwrap();

        assert_eq!(events.len(), 5);
        let turn_id = match events[0] {
            AgentEvent::TurnStarted { turn_id } => turn_id,
            ref event => panic!("unexpected event: {event:?}"),
        };
        let message_id = match &events[1] {
            AgentEvent::TextDelta {
                turn_id: delta_turn,
                message_id,
                chunk,
                is_final: false,
            } => {
                assert_eq!(*delta_turn, turn_id);
                assert_eq!(chunk, "Hel");
                *message_id
            }
            event => panic!("unexpected event: {event:?}"),
        };
        assert!(matches!(
            &events[2],
            AgentEvent::TextDelta {
                turn_id: delta_turn,
                message_id: delta_message,
                chunk,
                is_final: false,
            } if *delta_turn == turn_id && *delta_message == message_id && chunk == "lo"
        ));
        assert!(matches!(
            &events[3],
            AgentEvent::TextDelta {
                turn_id: delta_turn,
                message_id: delta_message,
                chunk,
                is_final: true,
            } if *delta_turn == turn_id && *delta_message == message_id && chunk.is_empty()
        ));
        assert!(matches!(
            events[4],
            AgentEvent::TurnCompleted { turn_id: completed } if completed == turn_id
        ));
    }

    #[tokio::test]
    async fn turn_executes_mutating_tool_without_approval() {
        let counter = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new()
                    .route("/chat/completions", post(write_file_stream))
                    .with_state(counter),
            )
            .await
            .unwrap();
        });
        let workspace = tempfile::tempdir().unwrap();
        let llm = LlmClient::with_client(
            Client::builder().no_proxy().build().unwrap(),
            format!("http://{address}"),
            "test-key",
            "test-model",
        );
        let mut agent = Agent::new(
            AgentConfig {
                workspace: workspace.path().to_path_buf(),
                max_tool_loops: 4,
                tool_timeout: Duration::from_secs(120),
                max_tool_output_bytes: 32_768,
                temperature: 0.2,
            },
            llm,
        )
        .unwrap();
        let mut events = Vec::new();
        agent
            .turn("write a note", |event| events.push(event))
            .await
            .unwrap();

        assert!(
            !events
                .iter()
                .any(|event| matches!(event, AgentEvent::ApprovalRequired { .. }))
        );
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::ToolFinished { call_id, success: true, .. } if call_id == "write-1"
        )));
        assert_eq!(
            std::fs::read_to_string(workspace.path().join("note.txt")).unwrap(),
            "hello"
        );
    }

    #[tokio::test]
    async fn completed_agent_turn_is_durable_and_reopens_with_tool_context() {
        let counter = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new()
                    .route("/chat/completions", post(write_file_stream))
                    .with_state(counter),
            )
            .await
            .unwrap();
        });
        let data = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let manager = SessionManager::at(data.path());
        let session = manager
            .create(CreateSession {
                workspace: workspace.path().to_path_buf(),
                model: "test".to_string(),
                model_id: "test-model".to_string(),
            })
            .unwrap();
        let session_id = *session.id();
        let llm = LlmClient::with_client(
            Client::builder().no_proxy().build().unwrap(),
            format!("http://{address}"),
            "test-key",
            "test-model",
        );
        let mut agent = Agent::with_session(
            AgentConfig {
                workspace: workspace.path().to_path_buf(),
                max_tool_loops: 4,
                tool_timeout: Duration::from_secs(120),
                max_tool_output_bytes: 32_768,
                temperature: 0.2,
            },
            llm,
            session,
        )
        .unwrap();

        agent.turn("write a note", |_| {}).await.unwrap();
        drop(agent);

        let session_directory = data.path().join("sessions").join(session_id.to_string());
        for entry in std::fs::read_dir(&session_directory).unwrap() {
            let path = entry.unwrap().path();
            if path.is_file() {
                let content = std::fs::read(&path).unwrap();
                assert!(!String::from_utf8_lossy(&content).contains("test-key"));
            }
        }

        let reopened = manager.open(session_id).unwrap();
        let context = reopened.context().messages;
        assert_eq!(context.len(), 4);
        assert_eq!(context[0].role, "user");
        assert_eq!(context[1].tool_calls.as_ref().unwrap()[0].id, "write-1");
        assert_eq!(context[2].tool_call_id.as_deref(), Some("write-1"));
        assert_eq!(context[3].content, "Written.");
    }

    #[tokio::test]
    async fn turn_can_be_cancelled_while_waiting_for_llm() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route("/chat/completions", post(slow_response)),
            )
            .await
            .unwrap();
        });
        let workspace = tempfile::tempdir().unwrap();
        let llm = LlmClient::with_client(
            Client::builder().no_proxy().build().unwrap(),
            format!("http://{address}"),
            "key",
            "model",
        );
        let mut agent = Agent::new(
            AgentConfig {
                workspace: workspace.path().to_path_buf(),
                max_tool_loops: 4,
                tool_timeout: Duration::from_secs(120),
                max_tool_output_bytes: 32_768,
                temperature: 0.2,
            },
            llm,
        )
        .unwrap();
        let control = TurnControl::non_interactive();
        let cancellation = control.clone();

        let turn = tokio::spawn(async move {
            agent
                .turn_with_control("wait", |_| {}, control)
                .await
                .unwrap_err()
                .to_string()
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        cancellation.cancel();
        let error = tokio::time::timeout(std::time::Duration::from_secs(1), turn)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(error, "turn cancelled");
    }

    #[tokio::test]
    async fn turn_allows_a_final_response_after_reaching_the_tool_loop_limit() {
        use std::sync::atomic::Ordering;

        let counter = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server_counter = counter.clone();
        tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new()
                    .route("/chat/completions", post(final_response_after_two_tools))
                    .with_state(server_counter),
            )
            .await
            .unwrap();
        });
        let workspace = tempfile::tempdir().unwrap();
        let llm = LlmClient::with_client(
            Client::builder().no_proxy().build().unwrap(),
            format!("http://{address}"),
            "test-key",
            "test-model",
        );
        let mut agent = Agent::new(
            AgentConfig {
                workspace: workspace.path().to_path_buf(),
                max_tool_loops: 2,
                tool_timeout: Duration::from_secs(120),
                max_tool_output_bytes: 32_768,
                temperature: 0.2,
            },
            llm,
        )
        .unwrap();
        let mut events = Vec::new();

        agent
            .turn("inspect the workspace", |event| events.push(event))
            .await
            .unwrap();

        assert_eq!(counter.load(Ordering::SeqCst), 3);
        assert!(matches!(
            events.last(),
            Some(AgentEvent::TurnCompleted { .. })
        ));
    }

    #[tokio::test]
    async fn turn_forces_a_final_response_without_tools_at_the_tool_loop_limit() {
        use std::sync::atomic::Ordering;

        let counter = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server_counter = counter.clone();
        tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new()
                    .route("/chat/completions", post(endless_tool_responses))
                    .with_state(server_counter),
            )
            .await
            .unwrap();
        });
        let workspace = tempfile::tempdir().unwrap();
        let llm = LlmClient::with_client(
            Client::builder().no_proxy().build().unwrap(),
            format!("http://{address}"),
            "test-key",
            "test-model",
        );
        let mut agent = Agent::new(
            AgentConfig {
                workspace: workspace.path().to_path_buf(),
                max_tool_loops: 2,
                tool_timeout: Duration::from_secs(120),
                max_tool_output_bytes: 32_768,
                temperature: 0.2,
            },
            llm,
        )
        .unwrap();
        let mut events = Vec::new();

        agent
            .turn("keep inspecting", |event| events.push(event))
            .await
            .unwrap();

        assert_eq!(counter.load(Ordering::SeqCst), 3);
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, AgentEvent::ToolStarted { .. }))
                .count(),
            2
        );
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::TextDelta { chunk, .. }
                if chunk.contains("Summarized without more tools")
        )));
        assert!(matches!(
            events.last(),
            Some(AgentEvent::TurnCompleted { .. })
        ));
    }

    #[tokio::test]
    async fn tool_execution_times_out_and_returns_a_model_visible_error() {
        let workspace = tempfile::tempdir().unwrap();
        let tools = ToolRegistry::coding_defaults(workspace.path());

        let (result, success) = execute_tool(
            &tools,
            "run_command",
            serde_json::json!({"command": "sleep 5"}),
            Duration::from_millis(10),
            1_024,
        )
        .await;

        assert!(!success);
        assert_eq!(result["timeout_ms"], 10);
        assert!(result["error"].as_str().unwrap().contains("timed out"));
    }

    #[tokio::test]
    async fn provider_tool_calls_after_the_limit_are_not_executed_or_returned_as_errors() {
        use std::sync::atomic::Ordering;

        let counter = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server_counter = counter.clone();
        tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new()
                    .route(
                        "/chat/completions",
                        post(tool_response_even_without_definitions),
                    )
                    .with_state(server_counter),
            )
            .await
            .unwrap();
        });
        let workspace = tempfile::tempdir().unwrap();
        let llm = LlmClient::with_client(
            Client::builder().no_proxy().build().unwrap(),
            format!("http://{address}"),
            "test-key",
            "test-model",
        );
        let mut agent = Agent::new(
            AgentConfig {
                workspace: workspace.path().to_path_buf(),
                max_tool_loops: 1,
                tool_timeout: Duration::from_secs(120),
                max_tool_output_bytes: 32_768,
                temperature: 0.2,
            },
            llm,
        )
        .unwrap();
        let mut events = Vec::new();

        agent
            .turn("keep inspecting", |event| events.push(event))
            .await
            .unwrap();

        assert_eq!(counter.load(Ordering::SeqCst), 2);
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, AgentEvent::ToolStarted { .. }))
                .count(),
            1
        );
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::TextDelta { chunk, .. } if chunk == TOOL_LIMIT_FALLBACK
        )));
        assert!(matches!(
            events.last(),
            Some(AgentEvent::TurnCompleted { .. })
        ));
    }

    #[test]
    fn oversized_tool_results_are_replaced_with_a_bounded_preview() {
        let result = limit_tool_result(serde_json::json!({"stdout": "x".repeat(2_000)}), 256);
        let serialized = serde_json::to_vec(&result).unwrap();

        assert!(serialized.len() <= 256);
        assert_eq!(result["truncated"], true);
        assert!(result["original_bytes"].as_u64().unwrap() > 2_000);
        assert!(result["preview"].as_str().unwrap().contains("stdout"));
    }
}
