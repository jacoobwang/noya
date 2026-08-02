//! Agent runtime: turn orchestration behind a small event-driven interface.

mod control;
mod event;
mod prompt;

pub use control::{ApprovalDecision, ApprovalPrompt, ApprovalRequest, TurnControl};
pub use event::AgentEvent;

use anyhow::{Context, Result};
use serde_json::Value;
use std::path::PathBuf;
use uuid::Uuid;

use crate::{
    llm::{ChatMessage, LlmClient, LlmEvent},
    tools::ToolRegistry,
};

#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub workspace: PathBuf,
    pub max_tool_loops: usize,
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
            let response = tokio::select! {
                response = self.llm.complete_stream(
                    self.messages.clone(),
                    self.tools.definitions(),
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
            if tool_loops >= self.config.max_tool_loops {
                anyhow::bail!("maximum tool loops reached");
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
                let (result, success) = match self.tools.get(&call.function.name) {
                    Some(tool) => match tokio::select! {
                        result = tool.execute(args) => result,
                        _ = control.cancellation.cancelled() => anyhow::bail!("turn cancelled"),
                    } {
                        Ok(result) => (result, true),
                        Err(error) => (serde_json::json!({"error": error.to_string()}), false),
                    },
                    None => (
                        serde_json::json!({"error": format!("unknown tool: {}", call.function.name)}),
                        false,
                    ),
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
    async fn turn_does_not_execute_tools_beyond_the_tool_loop_limit() {
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
                temperature: 0.2,
            },
            llm,
        )
        .unwrap();
        let mut events = Vec::new();

        let error = agent
            .turn("keep inspecting", |event| events.push(event))
            .await
            .unwrap_err()
            .to_string();

        assert_eq!(error, "maximum tool loops reached");
        assert_eq!(counter.load(Ordering::SeqCst), 3);
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, AgentEvent::ToolStarted { .. }))
                .count(),
            2
        );
    }
}
