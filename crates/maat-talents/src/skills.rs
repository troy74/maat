use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use maat_config::{fetch_github_asset, install_skill_from_dir};
use maat_core::{
    CapabilityCard, CapabilityId, CapabilityKind, CapabilityProvenance, CapabilityRoutingHints,
    CapabilityTrust, CostProfile, LlmToolDef, MaatError, ModelSelectionPolicy, ModelTrait,
    Permission, Tool, ToolRegistry,
};
use serde_json::{json, Value};

pub struct SkillTalent {
    skills_root: PathBuf,
}

impl SkillTalent {
    pub fn new(skills_root: PathBuf) -> Self {
        Self { skills_root }
    }

    pub fn register_all(&self, registry: &mut ToolRegistry) {
        registry.register(Arc::new(SkillManage {
            skills_root: self.skills_root.clone(),
        }));
    }
}

struct SkillManage {
    skills_root: PathBuf,
}

#[async_trait]
impl Tool for SkillManage {
    fn llm_definition(&self) -> LlmToolDef {
        LlmToolDef {
            name: "skill_manage".into(),
            description: "Create, scaffold, fetch assets for, or install local MAAT skills. Use this when the user wants MAAT to build or extend its own capabilities, especially command-mode skills around local CLIs or models.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "description": "One of: create_command_skill, fetch_github_asset, install_local"
                    },
                    "name": { "type": "string", "description": "Human name for the skill" },
                    "skill_id": { "type": "string", "description": "Optional explicit skill id; defaults to a slug from name" },
                    "description": { "type": "string", "description": "What the skill does and when to use it" },
                    "short_description": { "type": "string", "description": "Optional concise summary" },
                    "skill_dir": { "type": "string", "description": "Optional skill directory. Defaults to the configured local skills root plus skill_id." },
                    "instructions": { "type": "string", "description": "Body text for SKILL.md. Keep it concise and procedural." },
                    "command": { "type": "string", "description": "Command to run for command-mode skills, such as 'bash' or './bin/tool'" },
                    "args": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Default command args for the skill execution"
                    },
                    "permissions": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Permissions like file_read, file_write, process_spawn, network"
                    },
                    "trust": { "type": "string", "description": "trusted, review, or untrusted" },
                    "files": {
                        "type": "array",
                        "description": "Optional additional files to write into the skill directory",
                        "items": {
                            "type": "object",
                            "properties": {
                                "path": { "type": "string" },
                                "content": { "type": "string" },
                                "executable": { "type": "boolean" }
                            },
                            "required": ["path", "content"]
                        }
                    },
                    "repo": { "type": "string", "description": "GitHub repo owner/name for fetch_github_asset" },
                    "repo_path": { "type": "string", "description": "Path inside the GitHub repo" },
                    "ref": { "type": "string", "description": "Optional branch, tag, or commit-ish for GitHub fetches" },
                    "dest_path": { "type": "string", "description": "Destination file path for fetched GitHub assets" },
                    "source_dir": { "type": "string", "description": "Existing local skill directory to install into the configured skills root" }
                },
                "required": ["action"]
            }),
        }
    }

    fn capability_card(&self) -> Option<CapabilityCard> {
        let def = self.llm_definition();
        Some(CapabilityCard {
            id: CapabilityId(def.name.clone()),
            name: "Skill Manage".into(),
            semantic_description: def.description.clone(),
            kind: CapabilityKind::Talent,
            input_schema: def.parameters,
            output_schema: json!({ "type": "object" }),
            cost_profile: CostProfile { avg_latency_ms: 80, estimated_tokens: 350 },
            tags: vec![
                "skill".into(),
                "install".into(),
                "scaffold".into(),
                "self_extension".into(),
            ],
            semantic_terms: vec![
                "skill".into(),
                "skills".into(),
                "install skill".into(),
                "create skill".into(),
                "scaffold skill".into(),
                "command skill".into(),
                "github asset".into(),
                "self extension".into(),
                "capability".into(),
            ],
            trust: CapabilityTrust::Core,
            provenance: CapabilityProvenance {
                source: "compiled_talent".into(),
                path: None,
                reference: None,
            },
            permissions: vec![
                Permission::FileRead,
                Permission::FileWrite,
                Permission::ProcessSpawn,
                Permission::Network,
            ],
            routing_hints: Some(CapabilityRoutingHints {
                preferred_tags: vec!["skill".into(), "install".into(), "scaffold".into()],
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
        let action = require_str(&input, "action")?;
        match action.as_str() {
            "create_command_skill" => self.create_command_skill(&input),
            "fetch_github_asset" => self.fetch_github_asset(&input),
            "install_local" => self.install_local(&input),
            other => Err(MaatError::Tool(format!("unsupported action '{other}'"))),
        }
    }
}

impl SkillManage {
    fn create_command_skill(&self, input: &Value) -> Result<Value, MaatError> {
        let name = require_str(input, "name")?;
        let description = require_str(input, "description")?;
        let skill_id = input["skill_id"]
            .as_str()
            .map(|value| slugify_skill_id(value))
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| slugify_skill_id(&name));
        let short_description = input["short_description"].as_str().map(|value| value.trim().to_string());
        let instructions = input["instructions"].as_str().map(|value| value.trim().to_string()).filter(|value| !value.is_empty()).unwrap_or_else(|| {
            format!(
                "Use this skill when the user wants {description}. Execute the configured local command deterministically and return the produced output path or artifact details."
            )
        });
        let command = require_str(input, "command")?;
        let args = optional_string_array(input.get("args"));
        let permissions = normalize_permissions(optional_string_array(input.get("permissions")));
        let trust = input["trust"].as_str().unwrap_or("trusted").trim().to_ascii_lowercase();

        let skill_dir = input["skill_dir"]
            .as_str()
            .map(PathBuf::from)
            .unwrap_or_else(|| self.skills_root.join(&skill_id));
        if skill_dir.exists() {
            return Err(MaatError::Tool(format!(
                "destination already exists: {}",
                skill_dir.display()
            )));
        }
        fs::create_dir_all(&skill_dir)
            .map_err(|e| MaatError::Tool(format!("failed to create {}: {e}", skill_dir.display())))?;

        let skill_md = render_skill_md(&name, &description, short_description.as_deref(), &instructions);
        fs::write(skill_dir.join("SKILL.md"), skill_md)
            .map_err(|e| MaatError::Tool(format!("failed to write SKILL.md: {e}")))?;

        write_extra_files(&skill_dir, input.get("files"))?;

        let manifest = render_manifest(
            &skill_id,
            &name,
            &trust,
            &permissions,
            &command,
            &args,
        );
        fs::write(skill_dir.join("maat-skill.toml"), manifest)
            .map_err(|e| MaatError::Tool(format!("failed to write maat-skill.toml: {e}")))?;

        Ok(json!({
            "status": "created",
            "skill_id": skill_id,
            "path": skill_dir.display().to_string(),
            "restart_required": true,
            "skills_root": self.skills_root.display().to_string(),
        }))
    }

    fn fetch_github_asset(&self, input: &Value) -> Result<Value, MaatError> {
        let repo = require_str(input, "repo")?;
        let repo_path = require_str(input, "repo_path")?;
        let dest_path = PathBuf::from(require_str(input, "dest_path")?);
        let reference = input["ref"].as_str();
        fetch_github_asset(&repo, &repo_path, reference, &dest_path)
            .map_err(MaatError::Tool)?;
        Ok(json!({
            "status": "fetched",
            "repo": repo,
            "repo_path": repo_path,
            "dest_path": dest_path.display().to_string(),
            "ref": reference,
        }))
    }

    fn install_local(&self, input: &Value) -> Result<Value, MaatError> {
        let source_dir = PathBuf::from(require_str(input, "source_dir")?);
        let installed = install_skill_from_dir(&source_dir, &self.skills_root)
            .map_err(MaatError::Tool)?;
        Ok(json!({
            "status": "installed",
            "skill_id": installed.id,
            "name": installed.name,
            "path": installed.path.display().to_string(),
            "restart_required": true,
        }))
    }
}

fn require_str(input: &Value, key: &str) -> Result<String, MaatError> {
    input[key]
        .as_str()
        .map(|value| value.to_string())
        .ok_or_else(|| MaatError::Tool(format!("missing '{key}'")))
}

fn optional_string_array(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(|value| value.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(|text| text.to_string()))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

fn normalize_permissions(values: Vec<String>) -> Vec<String> {
    let mut normalized = values
        .into_iter()
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    if !normalized.iter().any(|value| value == "process_spawn") {
        normalized.push("process_spawn".into());
    }
    normalized.sort();
    normalized.dedup();
    normalized
}

fn render_skill_md(
    name: &str,
    description: &str,
    short_description: Option<&str>,
    instructions: &str,
) -> String {
    let mut output = format!("---\nname: {name}\ndescription: {description}\n");
    if let Some(short) = short_description.filter(|value| !value.is_empty()) {
        output.push_str("metadata:\n");
        output.push_str(&format!("  short-description: {short}\n"));
    }
    output.push_str("---\n\n");
    output.push_str(instructions);
    output.push('\n');
    output
}

fn render_manifest(
    skill_id: &str,
    name: &str,
    trust: &str,
    permissions: &[String],
    command: &str,
    args: &[String],
) -> String {
    let permission_lines = if permissions.is_empty() {
        "permissions = []".to_string()
    } else {
        format!(
            "permissions = [{}]",
            permissions
                .iter()
                .map(|value| format!("\"{value}\""))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    let arg_lines = if args.is_empty() {
        "args = []".to_string()
    } else {
        format!(
            "args = [{}]",
            args.iter()
                .map(|value| format!("\"{}\"", value.replace('"', "\\\"")))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    format!(
        "id = \"{skill_id}\"\nname = \"{}\"\ntrust = \"{}\"\n{permission_lines}\nsource_kind = \"workspace\"\n\n[execution]\nmode = \"command\"\ncommand = \"{}\"\n{arg_lines}\n",
        name.replace('"', "\\\""),
        trust,
        command.replace('"', "\\\""),
    )
}

fn write_extra_files(skill_dir: &Path, files: Option<&Value>) -> Result<(), MaatError> {
    let Some(files) = files.and_then(|value| value.as_array()) else {
        return Ok(());
    };
    for file in files {
        let path = file["path"]
            .as_str()
            .ok_or_else(|| MaatError::Tool("skill file entry missing 'path'".into()))?;
        let content = file["content"]
            .as_str()
            .ok_or_else(|| MaatError::Tool("skill file entry missing 'content'".into()))?;
        let executable = file["executable"].as_bool().unwrap_or(false);
        let target = skill_dir.join(path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| MaatError::Tool(format!("failed to create {}: {e}", parent.display())))?;
        }
        fs::write(&target, content)
            .map_err(|e| MaatError::Tool(format!("failed to write {}: {e}", target.display())))?;
        #[cfg(unix)]
        if executable {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&target)
                .map_err(|e| MaatError::Tool(format!("failed to stat {}: {e}", target.display())))?
                .permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&target, perms)
                .map_err(|e| MaatError::Tool(format!("failed to chmod {}: {e}", target.display())))?;
        }
    }
    Ok(())
}

fn slugify_skill_id(value: &str) -> String {
    value
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch.to_ascii_lowercase() } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_command_skill_writes_standard_files() {
        let root = std::env::temp_dir().join(format!("maat-skill-talent-{}", maat_core::now_ms()));
        let tool = SkillManage { skills_root: root.clone() };
        let result = tool.create_command_skill(&json!({
            "name": "Image Rectify",
            "description": "Rectify scanned images with a local model",
            "command": "bash",
            "args": ["scripts/run.sh"],
            "files": [
                { "path": "scripts/run.sh", "content": "#!/usr/bin/env bash\necho hi\n", "executable": true }
            ]
        })).expect("create skill");

        let path = PathBuf::from(result["path"].as_str().unwrap());
        assert!(path.join("SKILL.md").exists());
        assert!(path.join("maat-skill.toml").exists());
        assert!(path.join("scripts/run.sh").exists());
        let manifest = fs::read_to_string(path.join("maat-skill.toml")).unwrap();
        assert!(manifest.contains("process_spawn"));
        let _ = fs::remove_dir_all(root);
    }
}
