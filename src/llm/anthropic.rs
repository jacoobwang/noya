use super::protocol::{
    AssistantMessage, CalledFunction, ChatMessage, ChatResponse, ChatStreamResponse, Choice,
    LlmEvent, ToolCall, ToolDefinition,
};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;

#[derive(Debug, Serialize)]
pub(super) struct Request {
    pub model: String,
    pub max_tokens: u32,
    pub messages: Vec<Message>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<Tool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    pub stream: bool,
}

#[derive(Debug, Serialize)]
pub(super) struct Message {
    pub role: String,
    pub content: Vec<Value>,
}

#[derive(Debug, Serialize)]
pub(super) struct Tool {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

pub(super) fn request(
    model: &str,
    messages: &[ChatMessage],
    tools: &[ToolDefinition],
    temperature: f32,
    send_temperature: bool,
    stream: bool,
) -> Request {
    let mut system_parts = Vec::new();
    let mut native_messages = Vec::new();

    for message in messages {
        match message.role.as_str() {
            "system" => {
                if !message.content.is_empty() {
                    system_parts.push(message.content.clone());
                }
            }
            "assistant" => {
                let mut content = Vec::new();
                if !message.content.is_empty() {
                    content.push(json!({"type": "text", "text": message.content}));
                }
                for call in message.tool_calls.as_deref().unwrap_or_default() {
                    let input = serde_json::from_str(&call.function.arguments)
                        .unwrap_or_else(|_| json!({"raw_arguments": call.function.arguments}));
                    content.push(json!({
                        "type": "tool_use",
                        "id": call.id,
                        "name": call.function.name,
                        "input": input,
                    }));
                }
                if !content.is_empty() {
                    native_messages.push(Message {
                        role: "assistant".to_string(),
                        content,
                    });
                }
            }
            "tool" => {
                native_messages.push(Message {
                    role: "user".to_string(),
                    content: vec![json!({
                        "type": "tool_result",
                        "tool_use_id": message.tool_call_id.clone().unwrap_or_default(),
                        "content": message.content,
                    })],
                });
            }
            _ => native_messages.push(Message {
                role: "user".to_string(),
                content: vec![json!({"type": "text", "text": message.content})],
            }),
        }
    }

    Request {
        model: model.to_string(),
        max_tokens: 8192,
        messages: native_messages,
        system: (!system_parts.is_empty()).then_some(system_parts.join("\n\n")),
        tools: tools.iter().map(tool).collect(),
        temperature: send_temperature.then_some(temperature),
        stream,
    }
}

fn tool(definition: &ToolDefinition) -> Tool {
    Tool {
        name: definition.function.name.clone(),
        description: definition.function.description.clone(),
        input_schema: definition.function.parameters.clone(),
    }
}

#[derive(Debug, Deserialize)]
struct Response {
    content: Vec<ContentBlock>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum ContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "tool_use")]
    ToolUse { id: String, name: String, input: Value },
    #[serde(other)]
    Other,
}

pub(super) fn response(body: &str) -> Result<ChatResponse> {
    let response: Response = serde_json::from_str(body).context("decode Anthropic response")?;
    let mut content = String::new();
    let mut tool_calls = Vec::new();
    for block in response.content {
        match block {
            ContentBlock::Text { text } => content.push_str(&text),
            ContentBlock::ToolUse { id, name, input } => tool_calls.push(ToolCall {
                id,
                r#type: "function".to_string(),
                function: CalledFunction {
                    name,
                    arguments: serde_json::to_string(&input)?,
                },
            }),
            ContentBlock::Other => {}
        }
    }
    Ok(ChatResponse {
        choices: vec![Choice {
            message: AssistantMessage {
                content: Some(content),
                reasoning_content: None,
                tool_calls,
            },
        }],
    })
}

#[derive(Debug, Default)]
struct ToolCallBuilder {
    id: String,
    name: String,
    arguments: String,
}

#[derive(Debug, Default)]
pub(super) struct StreamAccumulator {
    content: String,
    reasoning_content: String,
    tool_calls: BTreeMap<usize, ToolCallBuilder>,
}

impl StreamAccumulator {
    pub(super) fn apply<F>(&mut self, data: &str, emit: &mut F) -> Result<bool>
    where
        F: FnMut(LlmEvent),
    {
        let event: Value = serde_json::from_str(data).context("decode Anthropic SSE event")?;
        match event.get("type").and_then(Value::as_str) {
            Some("content_block_start") => {
                let index = event
                    .get("index")
                    .and_then(Value::as_u64)
                    .context("Anthropic tool block is missing index")?
                    as usize;
                let block = event
                    .get("content_block")
                    .context("Anthropic block start is missing content_block")?;
                if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                    let builder = self.tool_calls.entry(index).or_default();
                    builder.id = block
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    builder.name = block
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                }
            }
            Some("content_block_delta") => {
                let index = event
                    .get("index")
                    .and_then(Value::as_u64)
                    .unwrap_or_default() as usize;
                let delta = event.get("delta").context("Anthropic event is missing delta")?;
                match delta.get("type").and_then(Value::as_str) {
                    Some("text_delta") => {
                        if let Some(text) = delta.get("text").and_then(Value::as_str) {
                            self.content.push_str(text);
                            emit(LlmEvent::TextDelta(text.to_string()));
                        }
                    }
                    Some("thinking_delta") => {
                        if let Some(thinking) = delta.get("thinking").and_then(Value::as_str) {
                            self.reasoning_content.push_str(thinking);
                        }
                    }
                    Some("input_json_delta") => {
                        if let Some(partial) = delta.get("partial_json").and_then(Value::as_str) {
                            self.tool_calls.entry(index).or_default().arguments.push_str(partial);
                        }
                    }
                    _ => {}
                }
            }
            Some("message_stop") => return Ok(true),
            _ => {}
        }
        Ok(false)
    }

    pub(super) fn finish(self) -> Result<ChatStreamResponse> {
        let tool_calls = self
            .tool_calls
            .into_values()
            .map(|builder| {
                if builder.id.is_empty() || builder.name.is_empty() {
                    bail!("Anthropic tool call is missing id or name");
                }
                Ok(ToolCall {
                    id: builder.id,
                    r#type: "function".to_string(),
                    function: CalledFunction {
                        name: builder.name,
                        arguments: if builder.arguments.is_empty() {
                            "{}".to_string()
                        } else {
                            builder.arguments
                        },
                    },
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(ChatStreamResponse {
            content: self.content,
            reasoning_content: (!self.reasoning_content.is_empty()).then_some(self.reasoning_content),
            tool_calls,
        })
    }
}

pub(super) fn stream_response(response: ChatResponse, emit: &mut impl FnMut(LlmEvent)) -> Result<ChatStreamResponse> {
    let choice = response
        .choices
        .into_iter()
        .next()
        .context("Anthropic response returned no choices")?;
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

    #[test]
    fn request_moves_system_and_tool_messages_into_anthropic_blocks() {
        let request = request(
            "claude-sonnet-5",
            &[
                ChatMessage {
                    role: "system".to_string(),
                    content: "Be concise".to_string(),
                    reasoning_content: None,
                    tool_call_id: None,
                    tool_calls: None,
                },
                ChatMessage {
                    role: "assistant".to_string(),
                    content: "".to_string(),
                    reasoning_content: None,
                    tool_call_id: None,
                    tool_calls: Some(vec![ToolCall {
                        id: "call-1".to_string(),
                        r#type: "function".to_string(),
                        function: CalledFunction {
                            name: "read_file".to_string(),
                            arguments: r#"{"path":"README.md"}"#.to_string(),
                        },
                    }]),
                },
                ChatMessage {
                    role: "tool".to_string(),
                    content: "contents".to_string(),
                    reasoning_content: None,
                    tool_call_id: Some("call-1".to_string()),
                    tool_calls: None,
                },
            ],
            &[],
            0.2,
            true,
            true,
        );
        let encoded = serde_json::to_value(request).unwrap();
        assert_eq!(encoded["system"], "Be concise");
        assert_eq!(encoded["messages"][0]["role"], "assistant");
        assert_eq!(encoded["messages"][0]["content"][0]["type"], "tool_use");
        assert_eq!(encoded["messages"][1]["content"][0]["type"], "tool_result");
    }

    #[test]
    fn response_maps_text_and_tool_use() {
        let response = response(
            r#"{
                "content": [
                    {"type":"text","text":"I will read it."},
                    {"type":"tool_use","id":"call-1","name":"read_file","input":{"path":"README.md"}}
                ]
            }"#,
        )
        .unwrap();
        let message = &response.choices[0].message;
        assert_eq!(message.content.as_deref(), Some("I will read it."));
        assert_eq!(message.tool_calls[0].function.name, "read_file");
        assert_eq!(message.tool_calls[0].function.arguments, r#"{"path":"README.md"}"#);
    }

    #[test]
    fn stream_maps_text_and_tool_input_deltas() {
        let mut accumulator = StreamAccumulator::default();
        let mut events = Vec::new();
        assert!(!accumulator
            .apply(
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hi"}}"#,
                &mut |event| events.push(event),
            )
            .unwrap());
        accumulator
            .apply(
                r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"call-1","name":"read_file","input":{}}}"#,
                &mut |_| {},
            )
            .unwrap();
        accumulator
            .apply(
                r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"README.md\"}"}}"#,
                &mut |_| {},
            )
            .unwrap();
        assert!(accumulator
            .apply(r#"{"type":"message_stop"}"#, &mut |_| {})
            .unwrap());
        let response = accumulator.finish().unwrap();
        assert_eq!(events, vec![LlmEvent::TextDelta("Hi".to_string())]);
        assert_eq!(response.tool_calls[0].function.name, "read_file");
        assert_eq!(response.tool_calls[0].function.arguments, r#"{"path":"README.md"}"#);
    }
}
