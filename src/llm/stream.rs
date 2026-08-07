use super::protocol::{CalledFunction, ChatStreamResponse, LlmEvent, ToolCall, Usage};
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::collections::BTreeMap;

#[derive(Debug, Deserialize)]
struct StreamChunk {
    #[serde(default)]
    choices: Vec<StreamChoice>,
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Debug, Deserialize)]
struct StreamChoice {
    delta: StreamDelta,
}

#[derive(Debug, Default, Deserialize)]
struct StreamDelta {
    content: Option<String>,
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<StreamToolCall>,
}

#[derive(Debug, Deserialize)]
struct StreamToolCall {
    index: usize,
    id: Option<String>,
    function: Option<StreamFunction>,
}

#[derive(Debug, Default, Deserialize)]
struct StreamFunction {
    name: Option<String>,
    arguments: Option<String>,
}

#[derive(Debug, Default)]
struct ToolCallBuilder {
    id: String,
    name: String,
    arguments: String,
}

#[derive(Default)]
pub(super) struct StreamAccumulator {
    content: String,
    reasoning_content: String,
    tool_calls: BTreeMap<usize, ToolCallBuilder>,
    usage: Option<Usage>,
}

impl StreamAccumulator {
    pub(super) fn apply<F>(&mut self, data: &str, emit: &mut F) -> Result<()>
    where
        F: FnMut(LlmEvent),
    {
        let chunk: StreamChunk =
            serde_json::from_str(data).context("decode SSE completion event")?;
        if let Some(usage) = chunk.usage {
            self.usage = Some(usage.normalized());
        }
        for choice in chunk.choices {
            if let Some(text) = choice.delta.content
                && !text.is_empty()
            {
                self.content.push_str(&text);
                emit(LlmEvent::TextDelta(text));
            }
            if let Some(reasoning) = choice.delta.reasoning_content {
                self.reasoning_content.push_str(&reasoning);
            }
            for delta in choice.delta.tool_calls {
                let builder = self.tool_calls.entry(delta.index).or_default();
                if let Some(id) = delta.id
                    && builder.id.is_empty()
                {
                    builder.id = id;
                }
                if let Some(function) = delta.function {
                    if let Some(name) = function.name {
                        builder.name.push_str(&name);
                    }
                    if let Some(arguments) = function.arguments {
                        builder.arguments.push_str(&arguments);
                    }
                }
            }
        }
        Ok(())
    }

    pub(super) fn finish(self) -> Result<ChatStreamResponse> {
        let tool_calls = self
            .tool_calls
            .into_values()
            .map(|builder| {
                if builder.id.is_empty() {
                    bail!("streamed tool call is missing id");
                }
                if builder.name.is_empty() {
                    bail!("streamed tool call is missing function name");
                }
                Ok(ToolCall {
                    id: builder.id,
                    r#type: "function".to_string(),
                    function: CalledFunction {
                        name: builder.name,
                        arguments: builder.arguments,
                    },
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(ChatStreamResponse {
            content: self.content,
            reasoning_content: (!self.reasoning_content.is_empty())
                .then_some(self.reasoning_content),
            tool_calls,
            usage: self.usage,
        })
    }
}

pub(super) fn find_event_boundary(buffer: &[u8]) -> Option<(usize, usize)> {
    let crlf = buffer
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|position| (position, 4));
    let lf = buffer
        .windows(2)
        .position(|window| window == b"\n\n")
        .map(|position| (position, 2));
    match (crlf, lf) {
        (Some(left), Some(right)) => Some(if left.0 <= right.0 { left } else { right }),
        (Some(boundary), None) | (None, Some(boundary)) => Some(boundary),
        (None, None) => None,
    }
}

pub(super) fn decode_sse_data(event: &[u8]) -> Result<Option<String>> {
    let event = std::str::from_utf8(event).context("decode SSE event as UTF-8")?;
    let data = event
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(str::trim_start)
        .collect::<Vec<_>>();
    if data.is_empty() {
        Ok(None)
    } else {
        Ok(Some(data.join("\n")))
    }
}
