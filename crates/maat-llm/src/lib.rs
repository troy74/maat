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
    ChatMessage, LlmToolDef, MaatError, ModelSpec, PendingToolCall, Role, StopReason,
    TokenUsage,
};

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
    model_id: String,
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
            .with_api_key(api_key);

        Ok(Self {
            client: Client::with_config(config),
            model_id: spec.model_id.clone(),
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

        Ok(CompletionResponse { content, stop_reason, usage, latency_ms, tool_calls })
    }
}
