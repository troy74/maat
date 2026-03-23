//! Google Calendar tools — list and create events via the Calendar REST API.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{Duration, Utc};
use maat_config::{MaatConfig, SecretResolver};
use maat_core::{
    CapabilityCard, CapabilityId, CapabilityKind, CapabilityProvenance, CapabilityRoutingHints,
    CapabilityTrust, CostProfile, LlmToolDef, MaatError, ModelSelectionPolicy, ModelTrait,
    Permission, Tool,
};
use serde_json::{json, Value};
use tracing::debug;

use super::auth::{refresh_access_token, TokenSet};

// ─────────────────────────────────────────────
// Shared token helper
// ─────────────────────────────────────────────

async fn valid_access_token(
    client_id: &str,
    client_secret: &str,
    resolver: &SecretResolver,
    config: &MaatConfig,
) -> Result<String, MaatError> {
    let token_key = config
        .google
        .as_ref()
        .map(|g| g.token_key().to_string())
        .unwrap_or_else(|| "maat/google/oauth_token".into());

    let raw = resolver.get(&token_key).ok_or_else(|| {
        MaatError::Config("Google not authenticated. Run /auth google first.".into())
    })?;

    let token = TokenSet::from_json(&raw)
        .ok_or_else(|| MaatError::Config("Stored Google token is invalid.".into()))?;

    if !token.is_expired() {
        return Ok(token.access_token);
    }

    debug!("Google access token expired — refreshing");
    let rt = token.refresh_token.clone().ok_or_else(|| {
        MaatError::Config(
            "Google token expired and no refresh token. Run /auth google again.".into(),
        )
    })?;

    let refreshed = refresh_access_token(client_id, client_secret, &rt, token)
        .await
        .map_err(|e| MaatError::Tool(format!("token refresh: {e}")))?;

    let _ = resolver.set(&token_key, &refreshed.to_json());
    Ok(refreshed.access_token)
}

// ─────────────────────────────────────────────
// CalendarList
// ─────────────────────────────────────────────

pub struct CalendarList {
    pub client_id: String,
    pub client_secret: String,
    pub resolver: Arc<SecretResolver>,
    pub config: Arc<MaatConfig>,
}

#[async_trait]
impl Tool for CalendarList {
    fn llm_definition(&self) -> LlmToolDef {
        LlmToolDef {
            name: "calendar_list".into(),
            description: "List upcoming Google Calendar events. Use when the user asks what's on their calendar, upcoming meetings, schedule, or agenda.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "days_ahead": {
                        "type": "integer",
                        "description": "How many days ahead to look (default 7, max 90)",
                        "default": 7
                    },
                    "max_results": {
                        "type": "integer",
                        "description": "Maximum number of events to return (default 10)",
                        "default": 10
                    },
                    "calendar_id": {
                        "type": "string",
                        "description": "Calendar ID to query (default 'primary')",
                        "default": "primary"
                    }
                },
                "required": []
            }),
        }
    }

    fn capability_card(&self) -> Option<CapabilityCard> {
        let def = self.llm_definition();
        Some(calendar_capability_card(
            &def,
            "Calendar List",
            vec!["calendar".into(), "read".into()],
            1300,
            700,
        ))
    }

    async fn call(&self, input: Value) -> Result<Value, MaatError> {
        let days_ahead = input["days_ahead"].as_i64().unwrap_or(7).clamp(1, 90);
        let max_results = input["max_results"].as_i64().unwrap_or(10).clamp(1, 100);
        let calendar_id = input["calendar_id"].as_str().unwrap_or("primary");

        let token = valid_access_token(
            &self.client_id,
            &self.client_secret,
            &self.resolver,
            &self.config,
        )
        .await?;

        let now = Utc::now();
        let time_min = now.to_rfc3339();
        let time_max = (now + Duration::days(days_ahead)).to_rfc3339();

        let url = format!(
            "https://www.googleapis.com/calendar/v3/calendars/{}/events",
            urlencoded(calendar_id)
        );

        let resp: Value = reqwest::Client::new()
            .get(&url)
            .bearer_auth(&token)
            .query(&[
                ("timeMin", time_min.as_str()),
                ("timeMax", time_max.as_str()),
                ("maxResults", &max_results.to_string()),
                ("orderBy", "startTime"),
                ("singleEvents", "true"),
            ])
            .send()
            .await
            .map_err(|e| MaatError::Tool(format!("Calendar API request: {e}")))?
            .json()
            .await
            .map_err(|e| MaatError::Tool(format!("Calendar API parse: {e}")))?;

        if let Some(err) = resp["error"]["message"].as_str() {
            return Err(MaatError::Tool(format!("Calendar API error: {err}")));
        }

        let events: Vec<Value> = resp["items"]
            .as_array()
            .unwrap_or(&vec![])
            .iter()
            .map(|e| {
                let summary = e["summary"].as_str().unwrap_or("(no title)");
                let start = e["start"]["dateTime"]
                    .as_str()
                    .or_else(|| e["start"]["date"].as_str())
                    .unwrap_or("?");
                let end = e["end"]["dateTime"]
                    .as_str()
                    .or_else(|| e["end"]["date"].as_str())
                    .unwrap_or("?");
                let location = e["location"].as_str().unwrap_or("");
                let description = e["description"].as_str().unwrap_or("");
                let link = e["htmlLink"].as_str().unwrap_or("");
                let id = e["id"].as_str().unwrap_or("");
                json!({
                    "id": id,
                    "summary": summary,
                    "start": start,
                    "end": end,
                    "location": location,
                    "description": description,
                    "link": link
                })
            })
            .collect();

        Ok(json!({
            "count": events.len(),
            "range_days": days_ahead,
            "events": events
        }))
    }
}

// ─────────────────────────────────────────────
// CalendarCreate
// ─────────────────────────────────────────────

pub struct CalendarCreate {
    pub client_id: String,
    pub client_secret: String,
    pub resolver: Arc<SecretResolver>,
    pub config: Arc<MaatConfig>,
}

#[async_trait]
impl Tool for CalendarCreate {
    fn llm_definition(&self) -> LlmToolDef {
        LlmToolDef {
            name: "calendar_create".into(),
            description: "Create a new Google Calendar event. Use when the user wants to schedule, book, or add an event/meeting to their calendar.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "summary": {
                        "type": "string",
                        "description": "Event title"
                    },
                    "start_datetime": {
                        "type": "string",
                        "description": "Start date/time in RFC3339 format, e.g. '2026-03-24T10:00:00+00:00'. Use the user's local timezone offset."
                    },
                    "end_datetime": {
                        "type": "string",
                        "description": "End date/time in RFC3339 format"
                    },
                    "description": {
                        "type": "string",
                        "description": "Optional event description or notes"
                    },
                    "location": {
                        "type": "string",
                        "description": "Optional event location (address, room, or video link)"
                    },
                    "calendar_id": {
                        "type": "string",
                        "description": "Calendar to add the event to (default 'primary')"
                    },
                    "attendees": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional list of attendee email addresses"
                    }
                },
                "required": ["summary", "start_datetime", "end_datetime"]
            }),
        }
    }

    fn capability_card(&self) -> Option<CapabilityCard> {
        let def = self.llm_definition();
        Some(calendar_capability_card(
            &def,
            "Calendar Create",
            vec!["calendar".into(), "write".into()],
            1600,
            900,
        ))
    }

    async fn call(&self, input: Value) -> Result<Value, MaatError> {
        let summary = input["summary"]
            .as_str()
            .ok_or_else(|| MaatError::Tool("missing 'summary'".into()))?;
        let start = input["start_datetime"]
            .as_str()
            .ok_or_else(|| MaatError::Tool("missing 'start_datetime'".into()))?;
        let end = input["end_datetime"]
            .as_str()
            .ok_or_else(|| MaatError::Tool("missing 'end_datetime'".into()))?;
        let calendar_id = input["calendar_id"].as_str().unwrap_or("primary");

        let token = valid_access_token(
            &self.client_id,
            &self.client_secret,
            &self.resolver,
            &self.config,
        )
        .await?;

        let mut body = json!({
            "summary": summary,
            "start": { "dateTime": start },
            "end":   { "dateTime": end }
        });

        if let Some(desc) = input["description"].as_str() {
            body["description"] = json!(desc);
        }
        if let Some(loc) = input["location"].as_str() {
            body["location"] = json!(loc);
        }
        if let Some(att) = input["attendees"].as_array() {
            let emails: Vec<Value> = att
                .iter()
                .filter_map(|v| v.as_str())
                .map(|e| json!({ "email": e }))
                .collect();
            body["attendees"] = json!(emails);
        }

        let url = format!(
            "https://www.googleapis.com/calendar/v3/calendars/{}/events",
            urlencoded(calendar_id)
        );

        let resp: Value = reqwest::Client::new()
            .post(&url)
            .bearer_auth(&token)
            .json(&body)
            .send()
            .await
            .map_err(|e| MaatError::Tool(format!("Calendar API request: {e}")))?
            .json()
            .await
            .map_err(|e| MaatError::Tool(format!("Calendar API parse: {e}")))?;

        if let Some(err) = resp["error"]["message"].as_str() {
            return Err(MaatError::Tool(format!("Calendar API error: {err}")));
        }

        Ok(json!({
            "status": "created",
            "id": resp["id"],
            "summary": resp["summary"],
            "start": resp["start"]["dateTime"],
            "end": resp["end"]["dateTime"],
            "link": resp["htmlLink"]
        }))
    }
}

// ─────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────

fn urlencoded(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
            _ => format!("%{:02X}", c as u32),
        })
        .collect()
}

fn calendar_capability_card(
    def: &LlmToolDef,
    display_name: &str,
    tags: Vec<String>,
    avg_latency_ms: u64,
    estimated_tokens: u32,
) -> CapabilityCard {
    CapabilityCard {
        id: CapabilityId(def.name.clone()),
        name: display_name.into(),
        semantic_description: def.description.clone(),
        kind: CapabilityKind::Talent,
        input_schema: def.parameters.clone(),
        output_schema: json!({ "type": "object" }),
        cost_profile: CostProfile { avg_latency_ms, estimated_tokens },
        tags,
        semantic_terms: Vec::new(),
        trust: CapabilityTrust::Core,
        provenance: CapabilityProvenance {
            source: "compiled_talent".into(),
            path: None,
            reference: None,
        },
        permissions: vec![Permission::Calendar, Permission::Network],
        routing_hints: Some(CapabilityRoutingHints {
            preferred_tags: vec!["calendar".into()],
            avoids_tags: vec![],
            model_policy: Some(ModelSelectionPolicy {
                preferred_profiles: vec![],
                allow_profiles: vec![],
                deny_profiles: vec![],
                required_traits: vec![ModelTrait::ToolCalling, ModelTrait::StructuredOutput],
                max_cost_tier: None,
                max_latency_tier: None,
                min_reasoning_tier: None,
                require_tool_calling: Some(true),
            }),
        }),
    }
}
