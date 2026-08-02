//! Agent runtime: turn orchestration behind a small event-driven interface.

mod control;
mod event;
mod prompt;

pub use control::{ApprovalDecision, ApprovalPrompt, ApprovalRequest, TurnControl};
pub use event::AgentEvent;

use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::{path::PathBuf, time::Duration};
use uuid::Uuid;

use crate::{
    llm::{ChatMessage, LlmClient, LlmEvent},
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
    messages: Vec<ChatMessage>,
}

impl Agent {
    pub fn new(config: AgentConfig, llm: LlmClient) -> Result<Self> {
        ensure!(
            !config.tool_timeout.is_zero(),
            "tool timeout must be greater than zero"
        );
        ensure!(
            config.max_tool_output_bytes >= 256,
            "max tool output must be at least 256 bytes"
        );
        let system = prompt::build(&config.workspace)?;
        Ok(Self {
            tools: ToolRegistry::coding_defaults(config.workspace.clone()),
            messages: vec![ChatMessage {
                role: "system".into(),
                content: system,
                reasoning_content: None,
                tool_call_id: None,
                tool_calls: None,
            }],
            config,
            llm,
        })
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
        emit(AgentEvent::TurnStarted);
        self.messages.push(ChatMessage {
            role: "user".into(),
            content: input.into(),
            reasoning_content: None,
            tool_call_id: None,
            tool_calls: None,
        });
        let mut tool_loops = 0;
        loop {
            let force_final_response = tool_loops >= self.config.max_tool_loops;
            let mut request_messages = self.messages.clone();
            let tool_definitions = if force_final_response {
                if let Some(system) = request_messages
                    .iter_mut()
                    .find(|message| message.role == "system")
                {
                    system.content.push_str("\n\n");
                    system.content.push_str(FINAL_RESPONSE_INSTRUCTION);
                } else {
                    request_messages.insert(
                        0,
                        ChatMessage {
                            role: "system".into(),
                            content: FINAL_RESPONSE_INSTRUCTION.into(),
                            reasoning_content: None,
                            tool_call_id: None,
                            tool_calls: None,
                        },
                    );
                }
                Vec::new()
            } else {
                self.tools.definitions()
            };
            let response = tokio::select! {
                response = self.llm.complete_stream(
                    request_messages,
                    tool_definitions,
                    self.config.temperature,
                    |event| match event {
                        LlmEvent::TextDelta(chunk) => emit(AgentEvent::TextDelta {
                            chunk,
                            is_final: false,
                        }),
                    },
                ) => response?,
                _ = control.cancellation.cancelled() => anyhow::bail!("turn cancelled"),
            };
            if force_final_response && !response.tool_calls.is_empty() {
                let content = if response.content.trim().is_empty() {
                    emit(AgentEvent::TextDelta {
                        chunk: TOOL_LIMIT_FALLBACK.into(),
                        is_final: false,
                    });
                    TOOL_LIMIT_FALLBACK.to_string()
                } else {
                    response.content
                };
                emit(AgentEvent::TextDelta {
                    chunk: String::new(),
                    is_final: true,
                });
                self.messages.push(ChatMessage {
                    role: "assistant".into(),
                    content,
                    reasoning_content: response.reasoning_content,
                    tool_call_id: None,
                    tool_calls: None,
                });
                emit(AgentEvent::TurnCompleted);
                return Ok(());
            }
            emit(AgentEvent::TextDelta {
                chunk: String::new(),
                is_final: true,
            });
            if response.tool_calls.is_empty() {
                self.messages.push(ChatMessage {
                    role: "assistant".into(),
                    content: response.content,
                    reasoning_content: response.reasoning_content,
                    tool_call_id: None,
                    tool_calls: None,
                });
                emit(AgentEvent::TurnCompleted);
                return Ok(());
            }
            tool_loops += 1;
            self.messages.push(ChatMessage {
                role: "assistant".into(),
                content: response.content,
                reasoning_content: response.reasoning_content,
                tool_call_id: None,
                tool_calls: Some(response.tool_calls.clone()),
            });
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
                            emit(AgentEvent::ToolFinished {
                                call_id: call.id.clone(),
                                name: call.function.name.clone(),
                                result: result.clone(),
                                success: false,
                            });
                            self.messages.push(ChatMessage {
                                role: "tool".into(),
                                content: serde_json::to_string(&result)?,
                                reasoning_content: None,
                                tool_call_id: Some(call.id),
                                tool_calls: None,
                            });
                            continue;
                        }
                    }
                }
                emit(AgentEvent::ToolStarted {
                    call_id: call.id.clone(),
                    name: call.function.name.clone(),
                    arguments: args.clone(),
                });
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
                emit(AgentEvent::ToolFinished {
                    call_id: call.id.clone(),
                    name: call.function.name.clone(),
                    result: result.clone(),
                    success,
                });
                self.messages.push(ChatMessage {
                    role: "tool".into(),
                    content: serde_json::to_string(&result)?,
                    reasoning_content: None,
                    tool_call_id: Some(call.id),
                    tool_calls: None,
                });
            }
        }
    }

    pub fn reset(&mut self) -> Result<()> {
        self.messages = vec![ChatMessage {
            role: "system".into(),
            content: prompt::build(&self.config.workspace)?,
            reasoning_content: None,
            tool_call_id: None,
            tool_calls: None,
        }];
        Ok(())
    }
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

        assert_eq!(
            events,
            vec![
                AgentEvent::TurnStarted,
                AgentEvent::TextDelta {
                    chunk: "Hel".to_string(),
                    is_final: false,
                },
                AgentEvent::TextDelta {
                    chunk: "lo".to_string(),
                    is_final: false,
                },
                AgentEvent::TextDelta {
                    chunk: String::new(),
                    is_final: true,
                },
                AgentEvent::TurnCompleted,
            ]
        );
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
        assert!(matches!(events.last(), Some(AgentEvent::TurnCompleted)));
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
        assert!(matches!(events.last(), Some(AgentEvent::TurnCompleted)));
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
        assert!(matches!(events.last(), Some(AgentEvent::TurnCompleted)));
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
