use std::sync::Arc;

use async_trait::async_trait;
use maat_config::{
    delete_automation, describe_schedule, find_automation, load_automations, parse_schedule_expr,
    set_automation_status, slugify_automation_id, upsert_automation, AutomationSpec,
    AutomationStatus,
};
use maat_core::{
    CapabilityCard, CapabilityId, CapabilityKind, CapabilityProvenance, CapabilityRoutingHints,
    CapabilityTrust, CostProfile, LlmToolDef, MaatError, ModelSelectionPolicy, ModelTrait,
    Permission, Tool, ToolRegistry,
};
use serde_json::{json, Value};

pub struct AutomationTalent {
    dir: String,
}

impl AutomationTalent {
    pub fn new(dir: String) -> Self {
        Self { dir }
    }

    pub fn register_all(&self, registry: &mut ToolRegistry) {
        registry.register(Arc::new(AutomationManage {
            dir: self.dir.clone(),
        }));
    }
}

struct AutomationManage {
    dir: String,
}

#[async_trait]
impl Tool for AutomationManage {
    fn llm_definition(&self) -> LlmToolDef {
        LlmToolDef {
            name: "automation_manage".into(),
            description: "List, create, update, pause, resume, or delete MAAT automations. Use this when the user wants to manage scheduled jobs or recurring runs.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "description": "One of: list, create, update, pause, resume, delete, show" },
                    "name": { "type": "string", "description": "Automation name or id" },
                    "schedule": { "type": "string", "description": "Schedule expression like 'every 60m', 'daily 09:30', or 'weekly mon 09:30'" },
                    "prompt": { "type": "string", "description": "Automation task prompt" }
                },
                "required": ["action"]
            }),
        }
    }

    fn capability_card(&self) -> Option<CapabilityCard> {
        let def = self.llm_definition();
        Some(CapabilityCard {
            id: CapabilityId(def.name.clone()),
            name: "Automation Manage".into(),
            semantic_description: def.description.clone(),
            kind: CapabilityKind::Talent,
            input_schema: def.parameters,
            output_schema: json!({ "type": "object" }),
            cost_profile: CostProfile { avg_latency_ms: 40, estimated_tokens: 250 },
            tags: vec!["automation".into(), "schedule".into(), "config".into()],
            semantic_terms: Vec::new(),
            trust: CapabilityTrust::Core,
            provenance: CapabilityProvenance {
                source: "compiled_talent".into(),
                path: None,
                reference: None,
            },
            permissions: vec![Permission::FileRead, Permission::FileWrite],
            routing_hints: Some(CapabilityRoutingHints {
                preferred_tags: vec!["automation".into(), "schedule".into()],
                avoids_tags: vec![],
                model_policy: Some(ModelSelectionPolicy {
                    preferred_profiles: vec![],
                    allow_profiles: vec![],
                    deny_profiles: vec![],
                    required_traits: vec![ModelTrait::StructuredOutput],
                    max_cost_tier: None,
                    max_latency_tier: None,
                    min_reasoning_tier: None,
                    require_tool_calling: Some(true),
                }),
            }),
        })
    }

    async fn call(&self, input: Value) -> Result<Value, MaatError> {
        let action = input["action"]
            .as_str()
            .ok_or_else(|| MaatError::Tool("missing 'action'".into()))?;
        match action {
            "list" => {
                let specs = load_automations(&self.dir)
                    .map_err(|e| MaatError::Tool(e.to_string()))?;
                Ok(json!({
                    "automations": specs.into_iter().map(|spec| json!({
                        "id": spec.id,
                        "name": spec.name,
                        "status": format!("{:?}", spec.status),
                        "schedule": describe_schedule(&spec.schedule),
                        "session": spec.session,
                        "delivery": spec.delivery,
                    })).collect::<Vec<_>>()
                }))
            }
            "show" => {
                let name = require_str(&input, "name")?;
                let spec = find_automation(&self.dir, &name)
                    .map_err(|e| MaatError::Tool(e.to_string()))?
                    .ok_or_else(|| MaatError::Tool(format!("no automation '{name}' found")))?;
                Ok(json!({
                    "id": spec.id,
                    "name": spec.name,
                    "status": format!("{:?}", spec.status),
                    "schedule": describe_schedule(&spec.schedule),
                    "prompt": spec.prompt,
                    "session": spec.session,
                    "delivery": spec.delivery,
                }))
            }
            "create" | "update" => {
                let name = require_str(&input, "name")?;
                let schedule_expr = require_str(&input, "schedule")?;
                let prompt = require_str(&input, "prompt")?;
                let schedule = parse_schedule_expr(&schedule_expr)
                    .map_err(MaatError::Tool)?;
                let existing = find_automation(&self.dir, &name)
                    .map_err(|e| MaatError::Tool(e.to_string()))?;
                let existing_session = existing.as_ref().and_then(|spec| spec.session.clone());
                let existing_delivery = existing.as_ref().and_then(|spec| spec.delivery.clone());
                let spec = AutomationSpec {
                    id: existing
                        .as_ref()
                        .map(|spec| spec.id.clone())
                        .unwrap_or_else(|| slugify_automation_id(&name)),
                    name: name.clone(),
                    prompt,
                    status: existing.as_ref().map(|spec| spec.status.clone()).unwrap_or_default(),
                    schedule,
                    session: existing_session.or(Some("automation".into())),
                    delivery: existing_delivery,
                };
                let _ = upsert_automation(&self.dir, &spec)
                    .map_err(|e| MaatError::Tool(e.to_string()))?;
                Ok(json!({
                    "status": if action == "create" { "created" } else { "updated" },
                    "name": spec.name,
                    "schedule": describe_schedule(&spec.schedule),
                }))
            }
            "pause" => mutate_status(&self.dir, &input, AutomationStatus::Paused),
            "resume" => mutate_status(&self.dir, &input, AutomationStatus::Active),
            "delete" => {
                let name = require_str(&input, "name")?;
                match delete_automation(&self.dir, &name)
                    .map_err(|e| MaatError::Tool(e.to_string()))? {
                    Some(spec) => Ok(json!({ "status": "deleted", "name": spec.name })),
                    None => Err(MaatError::Tool(format!("no automation '{name}' found"))),
                }
            }
            other => Err(MaatError::Tool(format!("unsupported action '{other}'"))),
        }
    }
}

fn require_str(input: &Value, key: &str) -> Result<String, MaatError> {
    input[key]
        .as_str()
        .map(|value| value.to_string())
        .ok_or_else(|| MaatError::Tool(format!("missing '{key}'")))
}

fn mutate_status(dir: &str, input: &Value, status: AutomationStatus) -> Result<Value, MaatError> {
    let name = require_str(input, "name")?;
    match set_automation_status(dir, &name, status)
        .map_err(|e| MaatError::Tool(e.to_string()))? {
        Some(spec) => Ok(json!({
            "status": format!("{:?}", spec.status),
            "name": spec.name,
        })),
        None => Err(MaatError::Tool(format!("no automation '{name}' found"))),
    }
}
