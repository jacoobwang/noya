//! OpenAI-compatible LLM adapter. Protocol DTOs and SSE assembly stay internal.

mod protocol;
mod stream;

pub use protocol::{
    AssistantMessage, CalledFunction, ChatMessage, ChatRequest, ChatResponse, ChatStreamResponse,
    Choice, FunctionDefinition, LlmEvent, ToolCall, ToolDefinition,
};

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use reqwest::Client;
use serde::Deserialize;
use stream::{StreamAccumulator, decode_sse_data, find_event_boundary};

#[derive(Clone)]
pub struct LlmClient {
    http: Client,
    base_url: String,
    api_key: String,
    model: String,
    send_temperature: bool,
}

impl LlmClient {
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self::with_client(Client::new(), base_url, api_key, model)
    }

    pub fn with_client(
        http: Client,
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            http,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key: api_key.into(),
            model: model.into(),
            send_temperature: true,
        }
    }

    pub fn with_custom_temperature(mut self, enabled: bool) -> Self {
        self.send_temperature = enabled;
        self
    }

    pub fn model_id(&self) -> &str {
        &self.model
    }

    pub async fn list_models(&self) -> Result<Vec<String>> {
        self.ensure_api_key()?;
        let response = self
            .http
            .get(format!("{}/models", self.base_url))
            .bearer_auth(&self.api_key)
            .send()
            .await
            .context("send model discovery request")?;
        let status = response.status();
        let body = response
            .text()
            .await
            .context("read model discovery response")?;
        if !status.is_success() {
            bail!("LLM model discovery failed ({}): {}", status, body);
        }
        let payload: ModelListResponse =
            serde_json::from_str(&body).context("decode model discovery response")?;
        let mut models = payload
            .data
            .into_iter()
            .map(|model| model.id.trim().to_string())
            .filter(|id| !id.is_empty())
            .collect::<Vec<_>>();
        models.sort();
        models.dedup();
        if models.is_empty() {
            bail!("LLM model discovery returned no model IDs");
        }
        Ok(models)
    }

    pub async fn complete(
        &self,
        messages: Vec<ChatMessage>,
        tools: Vec<ToolDefinition>,
        temperature: f32,
    ) -> Result<ChatResponse> {
        self.ensure_api_key()?;
        let response = self
            .http
            .post(format!("{}/chat/completions", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&ChatRequest {
                model: self.model.clone(),
                messages,
                tools,
                temperature: self.send_temperature.then_some(temperature),
                stream: false,
            })
            .send()
            .await
            .context("send chat completion request")?;
        let status = response.status();
        let body = response
            .text()
            .await
            .context("read chat completion response")?;
        if !status.is_success() {
            bail!("LLM request failed ({}): {}", status, body);
        }
        serde_json::from_str(&body).context("decode chat completion response")
    }

    pub async fn complete_stream<F>(
        &self,
        messages: Vec<ChatMessage>,
        tools: Vec<ToolDefinition>,
        temperature: f32,
        mut emit: F,
    ) -> Result<ChatStreamResponse>
    where
        F: FnMut(LlmEvent),
    {
        self.ensure_api_key()?;
        let response = self
            .http
            .post(format!("{}/chat/completions", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&ChatRequest {
                model: self.model.clone(),
                messages: messages.clone(),
                tools: tools.clone(),
                temperature: self.send_temperature.then_some(temperature),
                stream: true,
            })
            .send()
            .await
            .context("send streaming chat completion request")?;

        let status = response.status();
        if !status.is_success() {
            let body = response
                .text()
                .await
                .context("read streaming chat completion error")?;
            if matches!(status.as_u16(), 400 | 404 | 405 | 415 | 422 | 501) {
                return self
                    .complete_non_stream_fallback(messages, tools, temperature, emit)
                    .await;
            }
            bail!("LLM streaming request failed ({}): {}", status, body);
        }

        let is_event_stream = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("text/event-stream"));
        if !is_event_stream {
            let body = response
                .text()
                .await
                .context("read non-streaming completion response")?;
            let decoded: ChatResponse =
                serde_json::from_str(&body).context("decode non-streaming completion response")?;
            return response_from_complete(decoded, &mut emit);
        }

        let mut bytes = response.bytes_stream();
        let mut buffer = Vec::new();
        let mut stream = StreamAccumulator::default();
        let mut done = false;

        while let Some(chunk) = bytes.next().await {
            buffer.extend_from_slice(&chunk.context("read streaming completion chunk")?);
            while let Some((event_end, delimiter_len)) = find_event_boundary(&buffer) {
                let event = buffer.drain(..event_end).collect::<Vec<_>>();
                buffer.drain(..delimiter_len);
                let Some(data) = decode_sse_data(&event)? else {
                    continue;
                };
                if data == "[DONE]" {
                    done = true;
                    break;
                }
                stream.apply(&data, &mut emit)?;
            }
            if done {
                break;
            }
        }

        if !done {
            bail!("LLM stream ended before [DONE]");
        }

        stream.finish()
    }

    async fn complete_non_stream_fallback<F>(
        &self,
        messages: Vec<ChatMessage>,
        tools: Vec<ToolDefinition>,
        temperature: f32,
        mut emit: F,
    ) -> Result<ChatStreamResponse>
    where
        F: FnMut(LlmEvent),
    {
        let response = self.complete(messages, tools, temperature).await?;
        response_from_complete(response, &mut emit)
    }

    fn ensure_api_key(&self) -> Result<()> {
        if self.api_key.trim().is_empty() {
            bail!(
                "no API credential configured; run `noya login <model>` or set the provider API key environment variable"
            );
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
struct ModelListResponse {
    data: Vec<ModelListItem>,
}

#[derive(Debug, Deserialize)]
struct ModelListItem {
    id: String,
}

fn response_from_complete<F>(response: ChatResponse, emit: &mut F) -> Result<ChatStreamResponse>
where
    F: FnMut(LlmEvent),
{
    let choice = response
        .choices
        .into_iter()
        .next()
        .context("LLM returned no choices")?;
    let content = choice.message.content.unwrap_or_default();
    if !content.is_empty() {
        emit(LlmEvent::TextDelta(content.clone()));
    }
    Ok(ChatStreamResponse {
        content,
        reasoning_content: choice.message.reasoning_content,
        tool_calls: choice.message.tool_calls,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Router,
        body::Body,
        http::{Response, header::CONTENT_TYPE},
        routing::{get, post},
    };

    async fn stream_response() -> Response<Body> {
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"considering\"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"lo\"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call-1\",\"function\":{\"name\":\"read_file\",\"arguments\":\"{\\\"pa\"}}]},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"th\\\":\\\"README.md\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: [DONE]\n\n"
        );
        Response::builder()
            .header(CONTENT_TYPE, "text/event-stream")
            .body(Body::from(body))
            .unwrap()
    }

    async fn fallback_response(
        axum::Json(request): axum::Json<serde_json::Value>,
    ) -> Response<Body> {
        if request["stream"].as_bool() == Some(true) {
            return Response::builder()
                .status(400)
                .body(Body::from("stream unsupported"))
                .unwrap();
        }
        Response::builder()
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from(
                r#"{"choices":[{"message":{"content":"fallback","tool_calls":[]}}]}"#,
            ))
            .unwrap()
    }

    async fn interrupted_response() -> Response<Body> {
        Response::builder()
            .header(CONTENT_TYPE, "text/event-stream")
            .body(Body::from(
                "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n",
            ))
            .unwrap()
    }

    async fn models_response() -> Response<Body> {
        Response::builder()
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from(
                r#"{"data":[{"id":"z-model"},{"id":"a-model"},{"id":"z-model"}]}"#,
            ))
            .unwrap()
    }

    async fn mock_server() -> String {
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
        format!("http://{address}")
    }

    #[tokio::test]
    async fn complete_stream_emits_text_and_assembles_tool_calls() {
        let client = LlmClient::with_client(
            Client::builder().no_proxy().build().unwrap(),
            mock_server().await,
            "test-key",
            "test-model",
        );
        let mut events = Vec::new();

        let response = client
            .complete_stream(Vec::new(), Vec::new(), 0.2, |event| events.push(event))
            .await
            .unwrap();

        assert_eq!(
            events,
            vec![
                LlmEvent::TextDelta("Hel".to_string()),
                LlmEvent::TextDelta("lo".to_string()),
            ]
        );
        assert_eq!(response.content, "Hello");
        assert_eq!(response.reasoning_content.as_deref(), Some("considering"));
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].id, "call-1");
        assert_eq!(response.tool_calls[0].r#type, "function");
        assert_eq!(response.tool_calls[0].function.name, "read_file");
        assert_eq!(
            response.tool_calls[0].function.arguments,
            "{\"path\":\"README.md\"}"
        );
    }

    #[tokio::test]
    async fn list_models_reads_and_normalizes_openai_compatible_catalog() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route("/models", get(models_response)),
            )
            .await
            .unwrap();
        });
        let client = LlmClient::with_client(
            Client::builder().no_proxy().build().unwrap(),
            format!("http://{address}/"),
            "test-key",
            "",
        );

        assert_eq!(client.list_models().await.unwrap(), ["a-model", "z-model"]);
    }

    #[tokio::test]
    async fn missing_api_key_fails_at_request_time_without_contacting_the_endpoint() {
        let client = LlmClient::with_client(
            Client::builder().no_proxy().build().unwrap(),
            "http://127.0.0.1:1",
            "",
            "test-model",
        );

        let error = client
            .complete_stream(Vec::new(), Vec::new(), 0.2, |_| {})
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("no API credential configured"));
    }

    #[test]
    fn chat_request_can_omit_custom_temperature() {
        let request = ChatRequest {
            model: "kimi-k3".to_string(),
            messages: Vec::new(),
            tools: Vec::new(),
            temperature: None,
            stream: true,
        };

        let encoded = serde_json::to_value(request).unwrap();
        assert!(encoded.get("temperature").is_none());
        assert!(encoded.get("tools").is_none());
    }

    #[test]
    fn assistant_tool_message_preserves_reasoning_and_function_type() {
        let message = ChatMessage {
            role: "assistant".to_string(),
            content: String::new(),
            reasoning_content: Some("private reasoning state".to_string()),
            tool_call_id: None,
            tool_calls: Some(vec![ToolCall {
                id: "call-1".to_string(),
                r#type: "function".to_string(),
                function: CalledFunction {
                    name: "read_file".to_string(),
                    arguments: "{}".to_string(),
                },
            }]),
        };

        let encoded = serde_json::to_value(message).unwrap();
        assert_eq!(encoded["reasoning_content"], "private reasoning state");
        assert_eq!(encoded["tool_calls"][0]["type"], "function");
    }

    #[tokio::test]
    async fn complete_stream_falls_back_when_model_service_rejects_streaming() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route("/chat/completions", post(fallback_response)),
            )
            .await
            .unwrap();
        });
        let client = LlmClient::with_client(
            Client::builder().no_proxy().build().unwrap(),
            format!("http://{address}"),
            "key",
            "model",
        );
        let mut events = Vec::new();

        let response = client
            .complete_stream(Vec::new(), Vec::new(), 0.2, |event| events.push(event))
            .await
            .unwrap();

        assert_eq!(response.content, "fallback");
        assert_eq!(events, vec![LlmEvent::TextDelta("fallback".to_string())]);
    }

    #[tokio::test]
    async fn complete_stream_reports_an_interrupted_stream() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route("/chat/completions", post(interrupted_response)),
            )
            .await
            .unwrap();
        });
        let client = LlmClient::with_client(
            Client::builder().no_proxy().build().unwrap(),
            format!("http://{address}"),
            "key",
            "model",
        );
        let mut events = Vec::new();

        let error = client
            .complete_stream(Vec::new(), Vec::new(), 0.2, |event| events.push(event))
            .await
            .unwrap_err();

        assert_eq!(events, vec![LlmEvent::TextDelta("partial".to_string())]);
        assert!(error.to_string().contains("before [DONE]"));
    }
}
