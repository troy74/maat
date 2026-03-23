//! LLM client abstraction.
//!
//! `LlmClient` — single trait all backends implement.
//! `OpenAiCompatClient` — default backend (OpenRouter, Groq, Ollama, …).
//!
//! Tool calling: pass `&[LlmToolDef]` to `complete()`.
//! When the model requests tool calls, `stop_reason == ToolUse` and
//! `response.tool_calls` is populated.  The caller (MINION) injects
//! tool results and loops.

use async_openai::{
    config::OpenAIConfig,
    types::{
        ChatCompletionMessageToolCall, ChatCompletionRequestAssistantMessageArgs,
        ChatCompletionRequestMessage, ChatCompletionRequestSystemMessageArgs,
        ChatCompletionRequestToolMessageArgs, ChatCompletionRequestUserMessageArgs,
        ChatCompletionTool, ChatCompletionToolType, CreateChatCompletionRequestArgs,
        FinishReason, FunctionObject,
    },
    Client,
};
use async_trait::async_trait;
use maat_core::{
    ChatMessage, GeneratedArtifact, LlmToolDef, MaatError, ModelSpec, PendingToolCall, Role,
    StopReason, TokenUsage,
};
use serde_json::json;
use tracing::debug;

// ─────────────────────────────────────────────
// Response
// ─────────────────────────────────────────────

pub struct CompletionResponse {
    pub content: String,
    pub stop_reason: StopReason,
    pub usage: TokenUsage,
    pub latency_ms: u64,
    /// Populated when stop_reason == ToolUse.
    pub tool_calls: Vec<PendingToolCall>,
    pub generated_artifacts: Vec<GeneratedArtifact>,
}

// ─────────────────────────────────────────────
// Trait
// ─────────────────────────────────────────────

#[async_trait]
pub trait LlmClient: Send + Sync {
    async fn complete(
        &self,
        messages: Vec<ChatMessage>,
        tools: &[LlmToolDef],
    ) -> Result<CompletionResponse, MaatError>;
}

// ─────────────────────────────────────────────
// OpenAI-compatible backend
// ─────────────────────────────────────────────

pub struct OpenAiCompatClient {
    client: Client<OpenAIConfig>,
    http: reqwest::Client,
    api_base: String,
    api_key: String,
    model_id: String,
    profile_id: Option<String>,
    max_tokens: u32,
    temperature: f32,
}

impl OpenAiCompatClient {
    pub fn from_spec(spec: &ModelSpec) -> Result<Self, MaatError> {
        let api_key = std::env::var(&spec.api_key_env).map_err(|_| {
            MaatError::Config(format!(
                "env var `{}` not set — did you export your API key?",
                spec.api_key_env
            ))
        })?;

        let config = OpenAIConfig::new()
            .with_api_base(&spec.base_url)
            .with_api_key(&api_key);

        Ok(Self {
            client: Client::with_config(config),
            http: reqwest::Client::new(),
            api_base: spec.base_url.clone(),
            api_key,
            model_id: spec.model_id.clone(),
            profile_id: spec.profile_id.clone(),
            max_tokens: spec.max_tokens,
            temperature: spec.temperature,
        })
    }
}

// ─────────────────────────────────────────────
// Message conversion
// ─────────────────────────────────────────────

fn to_api_message(m: ChatMessage) -> Result<ChatCompletionRequestMessage, MaatError> {
    let msg = match m.role {
        Role::System => ChatCompletionRequestSystemMessageArgs::default()
            .content(m.content)
            .build()
            .map_err(|e| MaatError::Llm(e.to_string()))?
            .into(),

        Role::User => ChatCompletionRequestUserMessageArgs::default()
            .content(m.content)
            .build()
            .map_err(|e| MaatError::Llm(e.to_string()))?
            .into(),

        Role::Assistant => {
            let mut b = ChatCompletionRequestAssistantMessageArgs::default();
            b.content(m.content.clone());

            // If this assistant turn made tool calls, re-attach them.
            if let Some(json) = m.tool_calls_json {
                if let Ok(calls) = serde_json::from_str::<Vec<PendingToolCall>>(&json) {
                    let api_calls: Vec<ChatCompletionMessageToolCall> = calls
                        .into_iter()
                        .map(|tc| ChatCompletionMessageToolCall {
                            id: tc.id,
                            r#type: async_openai::types::ChatCompletionToolType::Function,
                            function: async_openai::types::FunctionCall {
                                name: tc.name,
                                arguments: tc.input.to_string(),
                            },
                        })
                        .collect();
                    b.tool_calls(api_calls);
                }
            }

            b.build()
                .map_err(|e| MaatError::Llm(e.to_string()))?
                .into()
        }

        Role::Tool => ChatCompletionRequestToolMessageArgs::default()
            .tool_call_id(m.tool_call_id.unwrap_or_default())
            .content(m.content)
            .build()
            .map_err(|e| MaatError::Llm(e.to_string()))?
            .into(),
    };
    Ok(msg)
}

fn to_api_tool(t: &LlmToolDef) -> ChatCompletionTool {
    ChatCompletionTool {
        r#type: ChatCompletionToolType::Function,
        function: FunctionObject {
            name: t.name.clone(),
            description: Some(t.description.clone()),
            parameters: Some(t.parameters.clone()),
            strict: None,
        },
    }
}

fn to_json_message(m: ChatMessage) -> serde_json::Value {
    match m.role {
        Role::System => json!({
            "role": "system",
            "content": m.content,
        }),
        Role::User => json!({
            "role": "user",
            "content": m.content,
        }),
        Role::Assistant => {
            let tool_calls = m
                .tool_calls_json
                .as_deref()
                .and_then(|raw| serde_json::from_str::<Vec<PendingToolCall>>(raw).ok())
                .map(|calls| {
                    calls.into_iter().map(|tc| {
                        json!({
                            "id": tc.id,
                            "type": "function",
                            "function": {
                                "name": tc.name,
                                "arguments": tc.input.to_string(),
                            }
                        })
                    }).collect::<Vec<_>>()
                });

            let mut value = json!({
                "role": "assistant",
                "content": m.content,
            });
            if let Some(tool_calls) = tool_calls {
                value["tool_calls"] = serde_json::Value::Array(tool_calls);
            }
            value
        }
        Role::Tool => json!({
            "role": "tool",
            "tool_call_id": m.tool_call_id.unwrap_or_default(),
            "content": m.content,
        }),
    }
}

// ─────────────────────────────────────────────
// Implementation
// ─────────────────────────────────────────────

#[async_trait]
impl LlmClient for OpenAiCompatClient {
    async fn complete(
        &self,
        messages: Vec<ChatMessage>,
        tools: &[LlmToolDef],
    ) -> Result<CompletionResponse, MaatError> {
        if self.should_request_image_output() {
            return self.complete_with_image_output(messages, tools).await;
        }

        let api_messages: Vec<ChatCompletionRequestMessage> = messages
            .into_iter()
            .map(to_api_message)
            .collect::<Result<_, _>>()?;

        let mut req = CreateChatCompletionRequestArgs::default();
        req.model(&self.model_id)
            .messages(api_messages)
            .max_tokens(self.max_tokens as u16)
            .temperature(self.temperature);

        if !tools.is_empty() {
            req.tools(tools.iter().map(to_api_tool).collect::<Vec<_>>());
        }

        let request = req.build().map_err(|e| MaatError::Llm(e.to_string()))?;

        tracing::debug!(model = %self.model_id, tools = tools.len(), "→ LLM");

        let t0 = std::time::Instant::now();
        let response = self
            .client
            .chat()
            .create(request)
            .await
            .map_err(|e| MaatError::Llm(e.to_string()))?;
        let latency_ms = t0.elapsed().as_millis() as u64;

        let choice = response
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| MaatError::Llm("no choices returned".into()))?;

        let content = choice.message.content.unwrap_or_default();

        let stop_reason = match choice.finish_reason {
            Some(FinishReason::Length)    => StopReason::MaxTokens,
            Some(FinishReason::ToolCalls) => StopReason::ToolUse,
            _                             => StopReason::EndTurn,
        };

        let usage = response
            .usage
            .map(|u| TokenUsage { input_tokens: u.prompt_tokens, output_tokens: u.completion_tokens })
            .unwrap_or_default();

        // Parse tool calls when the model requests them.
        let tool_calls: Vec<PendingToolCall> = choice
            .message
            .tool_calls
            .unwrap_or_default()
            .into_iter()
            .map(|tc| PendingToolCall {
                id: tc.id,
                name: tc.function.name,
                input: serde_json::from_str(&tc.function.arguments).unwrap_or_default(),
            })
            .collect();

        tracing::debug!(
            in_tok = usage.input_tokens,
            out_tok = usage.output_tokens,
            latency_ms,
            tool_calls = ?tool_calls.len(),
            "← LLM"
        );

        Ok(CompletionResponse {
            content,
            stop_reason,
            usage,
            latency_ms,
            tool_calls,
            generated_artifacts: vec![],
        })
    }
}

impl OpenAiCompatClient {
    fn should_request_image_output(&self) -> bool {
        self.profile_id.as_deref() == Some("image_preview")
            || self.model_id.contains("image-preview")
    }

    async fn complete_with_image_output(
        &self,
        messages: Vec<ChatMessage>,
        tools: &[LlmToolDef],
    ) -> Result<CompletionResponse, MaatError> {
        let api_messages = messages.into_iter().map(to_json_message).collect::<Vec<_>>();
        let mut body = json!({
            "model": self.model_id,
            "messages": api_messages,
            "max_tokens": self.max_tokens,
            "temperature": self.temperature,
            "modalities": ["image", "text"],
        });
        if !tools.is_empty() {
            body["tools"] = serde_json::Value::Array(
                tools.iter().map(|tool| {
                    json!({
                        "type": "function",
                        "function": {
                            "name": tool.name,
                            "description": tool.description,
                            "parameters": tool.parameters,
                        }
                    })
                }).collect()
            );
        }

        let url = format!("{}/chat/completions", self.api_base.trim_end_matches('/'));
        let t0 = std::time::Instant::now();
        let response = self.http
            .post(url)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| MaatError::Llm(e.to_string()))?;
        let latency_ms = t0.elapsed().as_millis() as u64;
        let status = response.status();
        let raw_json = response
            .text()
            .await
            .map_err(|e| MaatError::Llm(e.to_string()))?;
        if !status.is_success() {
            return Err(MaatError::Llm(format!("{}: {}", status, raw_json)));
        }
        debug!(
            model = %self.model_id,
            raw_preview = %truncate_for_log(&raw_json, 1200),
            "image-output raw response"
        );
        let payload: serde_json::Value = serde_json::from_str(&raw_json)
            .map_err(|e| MaatError::Llm(format!("invalid chat completion json: {e}")))?;
        let choice = payload
            .get("choices")
            .and_then(|choices| choices.as_array())
            .and_then(|choices| choices.first())
            .ok_or_else(|| MaatError::Llm("no choices returned".into()))?;
        let message = choice
            .get("message")
            .cloned()
            .unwrap_or_else(|| json!({}));

        let content = message
            .get("content")
            .and_then(json_content_to_string)
            .unwrap_or_default();
        let tool_calls = message
            .get("tool_calls")
            .and_then(|calls| calls.as_array())
            .map(|calls| {
                calls.iter().map(|tc| PendingToolCall {
                    id: tc.get("id").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
                    name: tc.get("function").and_then(|f| f.get("name")).and_then(|v| v.as_str()).unwrap_or_default().to_string(),
                    input: tc.get("function")
                        .and_then(|f| f.get("arguments"))
                        .and_then(|v| v.as_str())
                        .and_then(|raw| serde_json::from_str(raw).ok())
                        .unwrap_or_default(),
                }).collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let generated_artifacts = message
            .get("images")
            .and_then(|images| images.as_array())
            .map(|images| parse_generated_images(images))
            .filter(|artifacts| !artifacts.is_empty())
            .or_else(|| {
                message
                    .get("content")
                    .and_then(|content| parse_generated_images_from_content(content))
            })
            .unwrap_or_default();

        let finish_reason = choice
            .get("finish_reason")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let stop_reason = match finish_reason {
            "tool_calls" => StopReason::ToolUse,
            "length" => StopReason::MaxTokens,
            _ => StopReason::EndTurn,
        };
        let usage = TokenUsage {
            input_tokens: payload.get("usage").and_then(|u| u.get("prompt_tokens")).and_then(|v| v.as_u64()).unwrap_or(0) as u32,
            output_tokens: payload.get("usage").and_then(|u| u.get("completion_tokens")).and_then(|v| v.as_u64()).unwrap_or(0) as u32,
        };

        let content = if content.trim().is_empty() && generated_artifacts.is_empty() {
            "Image generation returned no parseable image payload.".to_string()
        } else {
            content
        };

        Ok(CompletionResponse {
            content,
            stop_reason,
            usage,
            latency_ms,
            tool_calls,
            generated_artifacts,
        })
    }
}

fn json_content_to_string(value: &serde_json::Value) -> Option<String> {
    if let Some(text) = value.as_str() {
        return Some(text.to_string());
    }
    value.as_array().map(|parts| {
        parts.iter()
            .filter_map(|part| {
                part.get("text")
                    .and_then(|v| v.as_str())
                    .map(|text| text.to_string())
                    .or_else(|| {
                        part.get("type")
                            .and_then(|v| v.as_str())
                            .filter(|kind| *kind == "output_text")
                            .and_then(|_| part.get("text").and_then(|v| v.as_str()).map(|s| s.to_string()))
                    })
                    .or_else(|| {
                        part.get("type")
                            .and_then(|v| v.as_str())
                            .filter(|kind| *kind == "text")
                            .and_then(|_| part.get("content").and_then(|v| v.as_str()).map(|s| s.to_string()))
                    })
            })
            .collect::<Vec<_>>()
            .join("\n")
    })
}

fn parse_generated_images(images: &[serde_json::Value]) -> Vec<GeneratedArtifact> {
    images
        .iter()
        .enumerate()
        .filter_map(|(index, image)| {
            let image_url = extract_image_url(image)?;
            let (mime_type, data_base64) = parse_image_payload(&image_url)?;
            let suggested_name =
                format!("generated-image-{}.{}", index + 1, image_extension(&mime_type));
            Some(GeneratedArtifact {
                kind: "image".into(),
                mime_type,
                suggested_name,
                summary: "Generated image output".into(),
                data_base64,
            })
        })
        .collect()
}

fn parse_data_url(value: &str) -> Option<(&str, &str)> {
    let rest = value.strip_prefix("data:")?;
    let (meta, data) = rest.split_once(',')?;
    let mime_type = meta.split(';').next()?;
    Some((mime_type, data))
}

fn parse_generated_images_from_content(content: &serde_json::Value) -> Option<Vec<GeneratedArtifact>> {
    let parts = content.as_array()?;
    let images = parts
        .iter()
        .enumerate()
        .filter_map(|(index, part)| {
            let image_url = extract_image_url(part)?;
            let (mime_type, data_base64) = parse_image_payload(&image_url)?;
            let suggested_name =
                format!("generated-image-{}.{}", index + 1, image_extension(&mime_type));
            Some(GeneratedArtifact {
                kind: "image".into(),
                mime_type,
                suggested_name,
                summary: "Generated image output".into(),
                data_base64,
            })
        })
        .collect::<Vec<_>>();
    if images.is_empty() {
        None
    } else {
        Some(images)
    }
}

fn extract_image_url(value: &serde_json::Value) -> Option<String> {
    value
        .get("image_url")
        .and_then(|inner| inner.as_str().map(ToString::to_string).or_else(|| {
            inner.get("url").and_then(|url| url.as_str()).map(ToString::to_string)
        }))
        .or_else(|| {
            value
                .get("imageUrl")
                .and_then(|inner| inner.as_str().map(ToString::to_string).or_else(|| {
                    inner.get("url").and_then(|url| url.as_str()).map(ToString::to_string)
                }))
        })
        .or_else(|| value.get("url").and_then(|url| url.as_str()).map(ToString::to_string))
}

fn parse_image_payload(value: &str) -> Option<(String, String)> {
    if let Some((mime_type, data_base64)) = parse_data_url(value) {
        return Some((mime_type.to_string(), data_base64.to_string()));
    }
    None
}

fn image_extension(mime_type: &str) -> &'static str {
    match mime_type {
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        _ => "png",
    }
}

fn truncate_for_log(value: &str, max_len: usize) -> String {
    if value.len() <= max_len {
        value.to_string()
    } else {
        format!("{}...", &value[..max_len])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_generated_images_from_data_urls() {
        let images = vec![json!({
            "image_url": "data:image/png;base64,QUJDRA=="
        })];
        let parsed = parse_generated_images(&images);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].mime_type, "image/png");
        assert_eq!(parsed[0].data_base64, "QUJDRA==");
    }
}
