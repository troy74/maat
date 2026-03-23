//! Web search talent — Tavily search API.
//!
//! Tavily docs: https://docs.tavily.com/docs/tavily-api/rest_api
//!
//! Store your key with:
//!   /secret set maat/tavily/api_key tvly-xxxxxxxx
//! or add TAVILY_API_KEY to your .env file.

use std::sync::Arc;

use async_trait::async_trait;
use maat_core::{
    CapabilityCard, CapabilityId, CapabilityKind, CapabilityProvenance, CapabilityRoutingHints,
    CapabilityTrust, CostProfile, LlmToolDef, MaatError, ModelSelectionPolicy, ModelTrait,
    Permission, Tool, ToolRegistry,
};
use serde_json::{json, Value};
use tracing::debug;

// ─────────────────────────────────────────────
// SearchTalent
// ─────────────────────────────────────────────

pub struct SearchTalent {
    api_key: String,
}

impl SearchTalent {
    pub fn new(api_key: String) -> Self {
        Self { api_key }
    }

    pub fn register_all(&self, registry: &mut ToolRegistry) {
        registry.register(Arc::new(WebSearch { api_key: self.api_key.clone() }));
    }
}

// ─────────────────────────────────────────────
// WebSearch tool
// ─────────────────────────────────────────────

pub struct WebSearch {
    api_key: String,
}

#[async_trait]
impl Tool for WebSearch {
    fn llm_definition(&self) -> LlmToolDef {
        LlmToolDef {
            name: "web_search".into(),
            description: "Search the web for current information. Use when the user asks about recent events, facts you may not know, prices, news, documentation, or anything that benefits from a live search.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "The search query"
                    },
                    "max_results": {
                        "type": "integer",
                        "description": "Number of results to return (default 5, max 10)",
                        "default": 5
                    },
                    "search_depth": {
                        "type": "string",
                        "enum": ["basic", "advanced"],
                        "description": "Search depth — 'basic' is fast, 'advanced' is thorough (default 'basic')",
                        "default": "basic"
                    }
                },
                "required": ["query"]
            }),
        }
    }

    fn capability_card(&self) -> Option<CapabilityCard> {
        let def = self.llm_definition();
        Some(CapabilityCard {
            id: CapabilityId(def.name.clone()),
            name: "Web Search".into(),
            semantic_description: def.description.clone(),
            kind: CapabilityKind::Talent,
            input_schema: def.parameters,
            output_schema: json!({ "type": "object" }),
            cost_profile: CostProfile { avg_latency_ms: 1800, estimated_tokens: 800 },
            tags: vec!["search".into(), "web".into(), "current-events".into()],
            semantic_terms: Vec::new(),
            trust: CapabilityTrust::Core,
            provenance: CapabilityProvenance {
                source: "compiled_talent".into(),
                path: None,
                reference: None,
            },
            permissions: vec![Permission::Network],
            routing_hints: Some(CapabilityRoutingHints {
                preferred_tags: vec!["search".into()],
                avoids_tags: vec!["premium".into()],
                model_policy: Some(ModelSelectionPolicy {
                    preferred_profiles: vec![],
                    allow_profiles: vec![],
                    deny_profiles: vec![],
                    required_traits: vec![ModelTrait::ToolCalling, ModelTrait::FastResponse],
                    max_cost_tier: None,
                    max_latency_tier: None,
                    min_reasoning_tier: None,
                    require_tool_calling: Some(true),
                }),
            }),
        })
    }

    async fn call(&self, input: Value) -> Result<Value, MaatError> {
        let query = input["query"]
            .as_str()
            .ok_or_else(|| MaatError::Tool("missing 'query'".into()))?
            .to_string();
        let max_results = input["max_results"].as_u64().unwrap_or(5).clamp(1, 10);
        let search_depth = input["search_depth"].as_str().unwrap_or("basic");

        debug!(query = %query, depth = %search_depth, "web_search");

        let body = json!({
            "api_key": self.api_key,
            "query": query,
            "search_depth": search_depth,
            "max_results": max_results,
            "include_answer": true,
            "include_raw_content": false
        });

        let resp: Value = reqwest::Client::new()
            .post("https://api.tavily.com/search")
            .json(&body)
            .send()
            .await
            .map_err(|e| MaatError::Tool(format!("Tavily request failed: {e}")))?
            .json()
            .await
            .map_err(|e| MaatError::Tool(format!("Tavily response parse: {e}")))?;

        // Tavily returns { "detail": "..." } on error
        if let Some(detail) = resp["detail"].as_str() {
            return Err(MaatError::Tool(format!("Tavily error: {detail}")));
        }

        let answer = resp["answer"].as_str().unwrap_or("").to_string();

        let results: Vec<Value> = resp["results"]
            .as_array()
            .unwrap_or(&vec![])
            .iter()
            .map(|r| {
                json!({
                    "title":   r["title"].as_str().unwrap_or(""),
                    "url":     r["url"].as_str().unwrap_or(""),
                    "content": r["content"].as_str().unwrap_or(""),
                    "score":   r["score"].as_f64().unwrap_or(0.0)
                })
            })
            .collect();

        Ok(json!({
            "query": query,
            "answer": answer,
            "results": results
        }))
    }
}
