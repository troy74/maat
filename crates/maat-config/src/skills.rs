use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use async_trait::async_trait;
use maat_core::{
    CapabilityCard, CapabilityId, CapabilityKind, CapabilityProvenance, CapabilityRoutingHints,
    CapabilityTrust, CostProfile, LlmToolDef, MaatError, ModelSelectionPolicy, Permission,
    PluginMode, Tool, ToolRegistry,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Debug, Clone)]
pub struct InstalledSkill {
    pub id: String,
    pub name: String,
    pub description: String,
    pub short_description: Option<String>,
    pub path: PathBuf,
    pub source: SkillSource,
    pub trust: CapabilityTrust,
    pub permissions: Vec<Permission>,
    pub reference: Option<String>,
    pub semantic_description: String,
    pub execution: SkillExecution,
    pub instructions: String,
}

impl InstalledSkill {
    pub fn capability_card(&self) -> CapabilityCard {
        let source_tag = self.source.tag();
        let mut semantic_terms = vec![
            self.id.clone(),
            sanitize_tag(&self.name),
            sanitize_tag(&self.description),
        ];
        if let Some(short) = &self.short_description {
            semantic_terms.push(sanitize_tag(short));
        }

        CapabilityCard {
            id: CapabilityId(self.id.clone()),
            name: self.name.clone(),
            semantic_description: self.semantic_description.clone(),
            kind: CapabilityKind::Skill(PluginMode::Stdio {
                command: format!("skill://{}", self.id),
            }),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "request": { "type": "string" },
                    "skill_path": { "type": "string", "default": self.path.display().to_string() }
                }
            }),
            output_schema: json!({ "type": "object" }),
            cost_profile: CostProfile { avg_latency_ms: 0, estimated_tokens: 1200 },
            tags: vec![
                "skill".into(),
                sanitize_tag(&self.name),
                source_tag.into(),
                format!("trust_{}", sanitize_tag(&format!("{:?}", self.trust))),
            ],
            semantic_terms,
            trust: self.trust.clone(),
            provenance: CapabilityProvenance {
                source: source_tag.into(),
                path: Some(self.path.display().to_string()),
                reference: self.reference.clone(),
            },
            permissions: self.permissions.clone(),
            routing_hints: Some(CapabilityRoutingHints {
                preferred_tags: vec!["skill".into(), source_tag.into()],
                avoids_tags: if self.trust == CapabilityTrust::Untrusted {
                    vec!["untrusted".into()]
                } else {
                    Vec::new()
                },
                model_policy: Some(ModelSelectionPolicy::default()),
            }),
        }
    }

    pub fn tool(&self) -> Arc<dyn Tool> {
        Arc::new(InstalledSkillTool { skill: self.clone() })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillSource {
    Workspace,
    CodexHome,
    GitHub,
    External,
    ClawHub,
}

impl SkillSource {
    pub fn tag(&self) -> &'static str {
        match self {
            SkillSource::Workspace => "workspace_skill",
            SkillSource::CodexHome => "codex_skill",
            SkillSource::GitHub => "github_skill",
            SkillSource::External => "external_skill",
            SkillSource::ClawHub => "clawhub_skill",
        }
    }

    fn as_manifest_str(&self) -> &'static str {
        match self {
            SkillSource::Workspace => "workspace",
            SkillSource::CodexHome => "codex_home",
            SkillSource::GitHub => "github",
            SkillSource::External => "external",
            SkillSource::ClawHub => "clawhub",
        }
    }

    fn from_manifest_str(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "workspace" => Some(SkillSource::Workspace),
            "codex_home" | "codex" => Some(SkillSource::CodexHome),
            "github" => Some(SkillSource::GitHub),
            "external" => Some(SkillSource::External),
            "clawhub" => Some(SkillSource::ClawHub),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallSource {
    LocalPath(PathBuf),
    ClawHubSlug(String),
    GitHub { repo: String, path: Option<String> },
}

impl InstallSource {
    pub fn parse(input: &str) -> Self {
        let trimmed = input.trim();
        if let Some(slug) = trimmed.strip_prefix("clawhub:") {
            return Self::ClawHubSlug(slug.trim().to_string());
        }
        if let Some(rest) = trimmed.strip_prefix("github:") {
            let (repo, path) = rest
                .split_once(':')
                .map(|(repo, path)| (repo.trim().to_string(), Some(path.trim().to_string())))
                .unwrap_or_else(|| (rest.trim().to_string(), None));
            return Self::GitHub { repo, path };
        }
        Self::LocalPath(PathBuf::from(trimmed))
    }
}

#[derive(Debug, Default, Clone)]
pub struct SkillRegistry {
    skills: Vec<InstalledSkill>,
}

impl SkillRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, skill: InstalledSkill) {
        self.skills.push(skill);
    }

    pub fn all(&self) -> &[InstalledSkill] {
        &self.skills
    }

    pub fn capability_cards(&self) -> Vec<CapabilityCard> {
        self.skills.iter().map(InstalledSkill::capability_card).collect()
    }

    pub fn register_tools(&self, registry: &mut ToolRegistry) {
        for skill in &self.skills {
            registry.register(skill.tool());
        }
    }
}

#[derive(Debug, Deserialize)]
struct SkillFrontmatter {
    name: Option<String>,
    description: Option<String>,
    metadata: Option<SkillMetadata>,
}

#[derive(Debug, Deserialize)]
struct SkillMetadata {
    #[serde(rename = "short-description")]
    short_description: Option<String>,
    trust: Option<String>,
    #[serde(default)]
    permissions: Vec<String>,
    source: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct InstalledSkillManifest {
    pub id: String,
    pub name: String,
    pub trust: String,
    #[serde(default)]
    pub permissions: Vec<String>,
    pub source_kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
    #[serde(default)]
    pub execution: SkillExecutionManifest,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SkillExecution {
    Instructions,
    Command { command: String, args: Vec<String> },
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SkillExecutionManifest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
}

pub fn load_installed_skills(dirs: &[PathBuf]) -> SkillRegistry {
    let mut registry = SkillRegistry::new();

    for dir in dirs {
        if !dir.exists() || !dir.is_dir() {
            continue;
        }

        if let Some(skill) = load_skill_dir(dir) {
            registry.register(skill);
            continue;
        }

        let Ok(entries) = fs::read_dir(dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if let Some(skill) = load_skill_dir(&path) {
                registry.register(skill);
            }
        }
    }

    registry
}

pub fn default_skill_dirs(config_dirs: &[String]) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = config_dirs.iter().map(PathBuf::from).collect();
    if let Ok(codex_home) = std::env::var("CODEX_HOME") {
        dirs.push(Path::new(&codex_home).join("skills"));
    }
    dirs.sort();
    dirs.dedup();
    dirs
}

pub fn install_skill_from_dir(source_dir: &Path, dest_root: &Path) -> Result<InstalledSkill, String> {
    let source_dir = source_dir
        .canonicalize()
        .map_err(|e| format!("source canonicalize failed: {e}"))?;
    let mut skill = load_skill_dir(&source_dir)
        .ok_or_else(|| format!("No SKILL.md found at {}", source_dir.display()))?;

    fs::create_dir_all(dest_root)
        .map_err(|e| format!("failed to create destination root {}: {e}", dest_root.display()))?;
    let dest_dir = dest_root.join(&skill.id);
    if dest_dir.exists() {
        return Err(format!("destination already exists: {}", dest_dir.display()));
    }

    copy_dir_recursive(&source_dir, &dest_dir)?;

    skill.path = dest_dir.clone();
    skill.source = SkillSource::Workspace;
    skill.trust = match skill.trust {
        CapabilityTrust::Core => CapabilityTrust::Trusted,
        other => other,
    };
    skill.reference = Some(source_dir.display().to_string());

    write_manifest(&dest_dir, &skill)?;
    Ok(skill)
}

pub fn install_skill(source: InstallSource, dest_root: &Path) -> Result<InstalledSkill, String> {
    match source {
        InstallSource::LocalPath(path) => install_skill_from_dir(&path, dest_root),
        InstallSource::ClawHubSlug(slug) => install_skill_from_clawhub(&slug, dest_root),
        InstallSource::GitHub { repo, path } => install_skill_from_github(&repo, path.as_deref(), dest_root),
    }
}

pub fn search_clawhub(query: &str) -> Result<String, String> {
    let output = Command::new("clawhub")
        .arg("search")
        .arg(query)
        .output()
        .map_err(|e| {
            format!(
                "failed to launch clawhub CLI: {e}. Install it first or use a local path."
            )
        })?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        Err(command_error("clawhub search", &output.stderr))
    }
}

fn load_skill_dir(dir: &Path) -> Option<InstalledSkill> {
    let skill_path = dir.join("SKILL.md");
    if !skill_path.exists() {
        return None;
    }

    let raw = fs::read_to_string(&skill_path).ok()?;
    let (frontmatter, body) = split_frontmatter(&raw);
    let fm: SkillFrontmatter = serde_yaml::from_str(frontmatter).ok()?;
    let manifest = read_manifest(dir).ok().flatten();

    let name = fm
        .name
        .clone()
        .unwrap_or_else(|| dir.file_name().unwrap_or_default().to_string_lossy().to_string());
    let description = fm.description.unwrap_or_else(|| "Third-party skill".into());
    let metadata = fm.metadata;
    let short_description = metadata.as_ref().and_then(|m| m.short_description.clone());
    let excerpt = body.lines().take(24).collect::<Vec<_>>().join(" ");
    let semantic_description = match short_description.as_deref() {
        Some(short) if !short.is_empty() => format!("{description} {short} {excerpt}").trim().to_string(),
        _ => format!("{description} {excerpt}").trim().to_string(),
    };
    let inferred_source = infer_source(dir);
    let source = manifest
        .as_ref()
        .and_then(|m| SkillSource::from_manifest_str(&m.source_kind))
        .unwrap_or(inferred_source);
    let trust = manifest
        .as_ref()
        .map(|m| parse_trust(&m.trust))
        .or_else(|| metadata.as_ref().and_then(|m| m.trust.as_deref()).map(parse_trust))
        .unwrap_or_else(|| default_trust_for_source(source));
    let permissions = manifest
        .as_ref()
        .map(|m| m.permissions.iter().filter_map(|p| parse_permission(p)).collect::<Vec<_>>())
        .or_else(|| {
            metadata.as_ref().map(|m| {
                m.permissions
                    .iter()
                    .filter_map(|permission| parse_permission(permission))
                    .collect::<Vec<_>>()
            })
        })
        .unwrap_or_default();
    let reference = manifest
        .as_ref()
        .and_then(|m| m.reference.clone())
        .or_else(|| metadata.and_then(|m| m.source));
    let execution = manifest
        .as_ref()
        .map(|m| parse_execution(&m.execution))
        .unwrap_or(SkillExecution::Instructions);

    Some(InstalledSkill {
        id: manifest
            .as_ref()
            .map(|m| m.id.clone())
            .unwrap_or_else(|| sanitize_id(&name)),
        name: manifest
            .as_ref()
            .map(|m| m.name.clone())
            .unwrap_or(name),
        description,
        short_description,
        path: dir.to_path_buf(),
        source,
        trust,
        permissions,
        reference,
        semantic_description,
        execution,
        instructions: body.trim().to_string(),
    })
}

fn read_manifest(dir: &Path) -> Result<Option<InstalledSkillManifest>, String> {
    let path = dir.join("maat-skill.toml");
    if !path.exists() {
        return Ok(None);
    }
    let text = fs::read_to_string(&path)
        .map_err(|e| format!("failed to read {}: {e}", path.display()))?;
    let manifest = toml::from_str(&text)
        .map_err(|e| format!("failed to parse {}: {e}", path.display()))?;
    Ok(Some(manifest))
}

fn write_manifest(dir: &Path, skill: &InstalledSkill) -> Result<(), String> {
    let manifest = InstalledSkillManifest {
        id: skill.id.clone(),
        name: skill.name.clone(),
        trust: format!("{:?}", skill.trust).to_ascii_lowercase(),
        permissions: skill
            .permissions
            .iter()
            .map(|p| format!("{p:?}").to_ascii_lowercase())
            .collect(),
        source_kind: skill.source.as_manifest_str().into(),
        reference: skill.reference.clone(),
        execution: execution_manifest(&skill.execution),
    };
    let text = toml::to_string_pretty(&manifest)
        .map_err(|e| format!("failed to encode manifest: {e}"))?;
    fs::write(dir.join("maat-skill.toml"), text)
        .map_err(|e| format!("failed to write manifest: {e}"))
}

fn copy_dir_recursive(source_dir: &Path, dest_dir: &Path) -> Result<(), String> {
    fs::create_dir_all(dest_dir)
        .map_err(|e| format!("failed to create {}: {e}", dest_dir.display()))?;
    for entry in fs::read_dir(source_dir)
        .map_err(|e| format!("failed to read {}: {e}", source_dir.display()))?
    {
        let entry = entry.map_err(|e| format!("failed to iterate directory: {e}"))?;
        let file_type = entry
            .file_type()
            .map_err(|e| format!("failed to inspect {}: {e}", entry.path().display()))?;
        let dest_path = dest_dir.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_recursive(&entry.path(), &dest_path)?;
        } else {
            fs::copy(entry.path(), &dest_path)
                .map_err(|e| format!("failed to copy to {}: {e}", dest_path.display()))?;
        }
    }
    Ok(())
}

fn infer_source(dir: &Path) -> SkillSource {
    let path = dir.to_string_lossy();
    if path.contains("/.codex/skills/") {
        SkillSource::CodexHome
    } else if path.contains("github.com") || path.contains("/github/") {
        SkillSource::GitHub
    } else if path.contains("/clawhub/") {
        SkillSource::ClawHub
    } else if path.contains("/skills/") || path.ends_with("/skills") {
        SkillSource::Workspace
    } else {
        SkillSource::External
    }
}

fn split_frontmatter(raw: &str) -> (&str, &str) {
    if let Some(rest) = raw.strip_prefix("---\n") {
        if let Some(idx) = rest.find("\n---\n") {
            let frontmatter = &rest[..idx];
            let body = &rest[idx + 5..];
            return (frontmatter, body);
        }
    }
    ("", raw)
}

fn sanitize_id(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

fn sanitize_tag(name: &str) -> String {
    sanitize_id(name).replace('-', "_")
}

fn parse_trust(value: &str) -> CapabilityTrust {
    match value.trim().to_ascii_lowercase().as_str() {
        "core" => CapabilityTrust::Core,
        "trusted" => CapabilityTrust::Trusted,
        "review" | "needs_review" => CapabilityTrust::Review,
        _ => CapabilityTrust::Untrusted,
    }
}

fn default_trust_for_source(source: SkillSource) -> CapabilityTrust {
    match source {
        SkillSource::Workspace => CapabilityTrust::Trusted,
        SkillSource::CodexHome => CapabilityTrust::Trusted,
        SkillSource::GitHub => CapabilityTrust::Review,
        SkillSource::ClawHub => CapabilityTrust::Review,
        SkillSource::External => CapabilityTrust::Review,
    }
}

fn parse_permission(value: &str) -> Option<Permission> {
    match value.trim().to_ascii_lowercase().as_str() {
        "network" => Some(Permission::Network),
        "file_read" | "fileread" | "read" => Some(Permission::FileRead),
        "file_write" | "filewrite" | "write" => Some(Permission::FileWrite),
        "process_spawn" | "process" | "spawn" => Some(Permission::ProcessSpawn),
        "email" => Some(Permission::Email),
        "calendar" => Some(Permission::Calendar),
        _ => None,
    }
}

fn parse_execution(manifest: &SkillExecutionManifest) -> SkillExecution {
    match manifest
        .mode
        .as_deref()
        .map(|mode| mode.trim().to_ascii_lowercase())
        .as_deref()
    {
        Some("command") => match &manifest.command {
            Some(command) => SkillExecution::Command {
                command: command.clone(),
                args: manifest.args.clone(),
            },
            None => SkillExecution::Instructions,
        },
        _ => SkillExecution::Instructions,
    }
}

fn execution_manifest(execution: &SkillExecution) -> SkillExecutionManifest {
    match execution {
        SkillExecution::Instructions => SkillExecutionManifest {
            mode: Some("instructions".into()),
            command: None,
            args: Vec::new(),
        },
        SkillExecution::Command { command, args } => SkillExecutionManifest {
            mode: Some("command".into()),
            command: Some(command.clone()),
            args: args.clone(),
        },
    }
}

fn install_skill_from_clawhub(slug: &str, dest_root: &Path) -> Result<InstalledSkill, String> {
    fs::create_dir_all(dest_root)
        .map_err(|e| format!("failed to create destination root {}: {e}", dest_root.display()))?;

    let before = snapshot_skill_dirs(dest_root)?;
    let status = Command::new("clawhub")
        .arg("install")
        .arg(slug)
        .current_dir(dest_root)
        .status()
        .map_err(|e| {
            format!("failed to launch clawhub CLI: {e}. Install it first or use a local path.")
        })?;
    if !status.success() {
        return Err(format!("clawhub install failed for slug '{slug}'."));
    }

    let after = snapshot_skill_dirs(dest_root)?;
    let created = after
        .into_iter()
        .find(|entry| !before.contains(entry))
        .unwrap_or_else(|| dest_root.join(slug));
    let mut skill = load_skill_dir(&created)
        .ok_or_else(|| format!("clawhub installed '{slug}', but no SKILL.md was found in {}", created.display()))?;
    skill.path = created.clone();
    skill.source = SkillSource::ClawHub;
    skill.trust = match skill.trust {
        CapabilityTrust::Core => CapabilityTrust::Review,
        CapabilityTrust::Trusted => CapabilityTrust::Review,
        other => other,
    };
    skill.reference = Some(format!("clawhub:{slug}"));
    write_manifest(&created, &skill)?;
    Ok(skill)
}

fn install_skill_from_github(
    repo: &str,
    path: Option<&str>,
    dest_root: &Path,
) -> Result<InstalledSkill, String> {
    fs::create_dir_all(dest_root)
        .map_err(|e| format!("failed to create destination root {}: {e}", dest_root.display()))?;

    let checkout_root = std::env::temp_dir().join(format!(
        "maat-github-skill-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let repo_url = format!("https://github.com/{repo}.git");
    let clone_status = Command::new("git")
        .arg("clone")
        .arg("--depth")
        .arg("1")
        .arg("--filter=blob:none")
        .arg("--sparse")
        .arg(&repo_url)
        .arg(&checkout_root)
        .status()
        .map_err(|e| format!("failed to launch git clone: {e}"))?;
    if !clone_status.success() {
        return Err(format!("git clone failed for repository {repo}."));
    }

    if let Some(path) = path {
        let sparse_status = Command::new("git")
            .arg("-C")
            .arg(&checkout_root)
            .arg("sparse-checkout")
            .arg("set")
            .arg(path)
            .status()
            .map_err(|e| format!("failed to configure sparse checkout: {e}"))?;
        if !sparse_status.success() {
            let _ = fs::remove_dir_all(&checkout_root);
            return Err(format!("git sparse-checkout failed for {repo}:{path}."));
        }
    }

    let source_dir = match path {
        Some(path) => checkout_root.join(path),
        None => checkout_root.clone(),
    };
    if !source_dir.join("SKILL.md").exists() {
        let _ = fs::remove_dir_all(&checkout_root);
        return Err(format!(
            "GitHub source {} does not contain SKILL.md{}",
            repo,
            path.map(|p| format!(" at {p}")).unwrap_or_default()
        ));
    }

    let mut skill = install_skill_from_dir(&source_dir, dest_root)?;
    skill.source = SkillSource::GitHub;
    skill.trust = match skill.trust {
        CapabilityTrust::Core => CapabilityTrust::Review,
        CapabilityTrust::Trusted => CapabilityTrust::Review,
        other => other,
    };
    skill.reference = Some(match path {
        Some(path) => format!("github:{repo}:{path}"),
        None => format!("github:{repo}"),
    });
    write_manifest(&skill.path, &skill)?;
    let _ = fs::remove_dir_all(&checkout_root);
    Ok(skill)
}

fn snapshot_skill_dirs(root: &Path) -> Result<Vec<PathBuf>, String> {
    let mut dirs = Vec::new();
    if !root.exists() {
        return Ok(dirs);
    }
    for entry in fs::read_dir(root).map_err(|e| format!("failed to read {}: {e}", root.display()))? {
        let entry = entry.map_err(|e| format!("failed to iterate directory: {e}"))?;
        if entry
            .file_type()
            .map_err(|e| format!("failed to inspect {}: {e}", entry.path().display()))?
            .is_dir()
        {
            dirs.push(entry.path());
        }
    }
    dirs.sort();
    Ok(dirs)
}

fn command_error(cmd: &str, stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr).trim().to_string();
    if text.is_empty() {
        format!("{cmd} failed")
    } else {
        format!("{cmd} failed: {text}")
    }
}

#[derive(Debug, Clone)]
struct InstalledSkillTool {
    skill: InstalledSkill,
}

#[async_trait]
impl Tool for InstalledSkillTool {
    fn llm_definition(&self) -> LlmToolDef {
        LlmToolDef {
            name: self.skill.id.clone(),
            description: self.skill.description.clone(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "request": {
                        "type": "string",
                        "description": "What you want this skill to help with."
                    },
                    "title": {
                        "type": "string",
                        "description": "Optional artifact title or short label for the skill to use."
                    },
                    "content": {
                        "type": "string",
                        "description": "Optional main text content for skills that create or transform artifacts."
                    },
                    "output_path": {
                        "type": "string",
                        "description": "Optional relative output path for a generated artifact, for example 'output/pdf/report.pdf'."
                    }
                },
                "required": ["request"]
            }),
        }
    }

    fn capability_card(&self) -> Option<CapabilityCard> {
        Some(self.skill.capability_card())
    }

    async fn call(&self, input: serde_json::Value) -> Result<serde_json::Value, MaatError> {
        let request = input
            .get("request")
            .and_then(|value| value.as_str())
            .unwrap_or("")
            .trim()
            .to_string();

        match &self.skill.execution {
            SkillExecution::Instructions => Ok(json!({
                "mode": "instructions",
                "completed": false,
                "skill": self.skill.name,
                "request": request,
                "path": self.skill.path.display().to_string(),
                "instructions": self.skill.instructions,
                "guidance": "This skill call is guidance only. It does not by itself create files, send messages, or finish the task. Follow the workflow and use the concrete talents or tools needed for each step.",
                "next_action": "Use this workflow as scaffolding, then call the required file, email, search, or other tools and verify their results before claiming completion."
            })),
            SkillExecution::Command { command, args } => {
                if self.skill.trust == CapabilityTrust::Untrusted
                    || !self.skill.permissions.contains(&Permission::ProcessSpawn)
                {
                    return Err(MaatError::Tool(format!(
                        "skill '{}' is not allowed to execute commands",
                        self.skill.name
                    )));
                }

                let command = command.clone();
                let args = args.clone();
                let skill_dir = self.skill.path.clone();
                let child_request = request.clone();
                let child_input = input.to_string();
                let workspace_dir = std::env::current_dir().unwrap_or_else(|_| skill_dir.clone());

                let output = tokio::task::spawn_blocking(move || {
                    let mut cmd = Command::new(&command);
                    cmd.args(&args)
                        .current_dir(&skill_dir)
                        .env("MAAT_SKILL_REQUEST", &child_request)
                        .env("MAAT_SKILL_INPUT", &child_input)
                        .env("MAAT_WORKSPACE_DIR", workspace_dir);
                    cmd.output()
                })
                .await
                .map_err(|e| MaatError::Tool(format!("skill task join error: {e}")))?
                .map_err(|e| MaatError::Tool(format!("skill command failed to launch: {e}")))?;

                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
                    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
                    return Err(MaatError::Tool(format!(
                        "skill command exited unsuccessfully: {}",
                        command_failure_detail(&stderr, &stdout)
                    )));
                }

                Ok(json!({
                    "mode": "command",
                    "skill": self.skill.name,
                    "request": request,
                    "stdout": String::from_utf8_lossy(&output.stdout).trim().to_string()
                }))
            }
        }
    }
}

fn command_failure_detail(stderr: &str, stdout: &str) -> String {
    if !stderr.is_empty() {
        return stderr.to_string();
    }

    if let Ok(value) = serde_json::from_str::<serde_json::Value>(stdout) {
        if let Some(message) = value.get("message").and_then(|item| item.as_str()) {
            return message.to_string();
        }
    }

    if !stdout.is_empty() {
        return stdout.to_string();
    }

    "command failed without diagnostic output".into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use maat_core::ToolRegistry;

    fn temp_skill_root() -> PathBuf {
        std::env::temp_dir().join(format!(
            "maat-skill-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ))
    }

    #[test]
    fn parses_skill_frontmatter_and_body_into_registry() {
        let root = temp_skill_root();
        let skill_dir = root.join("sample-skill");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            r#"---
name: "sample skill"
description: "Does sample work"
metadata:
  short-description: "Short form"
  trust: "trusted"
  permissions: ["network", "file_read"]
  source: "github.com/example/sample-skill"
---

# Sample Skill

Use this for sample tasks.
"#,
        )
        .unwrap();

        let registry = load_installed_skills(&[root.clone()]);
        let skill = registry.all().first().expect("skill should load");

        assert_eq!(skill.id, "sample-skill");
        assert_eq!(skill.name, "sample skill");
        assert_eq!(skill.description, "Does sample work");
        assert_eq!(skill.short_description.as_deref(), Some("Short form"));
        assert_eq!(skill.trust, CapabilityTrust::Trusted);
        assert_eq!(skill.permissions, vec![Permission::Network, Permission::FileRead]);
        assert_eq!(skill.reference.as_deref(), Some("github.com/example/sample-skill"));
        assert!(skill.semantic_description.contains("Use this for sample tasks."));

        let card = skill.capability_card();
        assert_eq!(card.name, "sample skill");
        assert!(card.tags.iter().any(|tag| tag == "skill"));
        assert_eq!(card.trust, CapabilityTrust::Trusted);
        assert_eq!(card.provenance.reference.as_deref(), Some("github.com/example/sample-skill"));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn workspace_path_is_classified_as_workspace_skill() {
        let dir = PathBuf::from("/tmp/example/skills/pdf");
        assert_eq!(infer_source(&dir), SkillSource::Workspace);
    }

    #[test]
    fn install_writes_manifest() {
        let root = temp_skill_root();
        let source = root.join("source-skill");
        let dest_root = root.join("installed");
        fs::create_dir_all(&source).unwrap();
        fs::write(
            source.join("SKILL.md"),
            r#"---
name: "sample skill"
description: "Does sample work"
---

# Sample Skill
"#,
        )
        .unwrap();

        let installed = install_skill_from_dir(&source, &dest_root).unwrap();
        let manifest_path = dest_root.join(&installed.id).join("maat-skill.toml");
        assert!(manifest_path.exists());

        let reloaded = load_installed_skills(&[dest_root]).all()[0].clone();
        assert_eq!(reloaded.reference.as_deref(), Some(source.canonicalize().unwrap().display().to_string().as_str()));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn parses_install_sources() {
        assert_eq!(
            InstallSource::parse("clawhub:pdf"),
            InstallSource::ClawHubSlug("pdf".into())
        );
        assert_eq!(
            InstallSource::parse("github:openai/skills:skills/.curated/pdf"),
            InstallSource::GitHub {
                repo: "openai/skills".into(),
                path: Some("skills/.curated/pdf".into()),
            }
        );
        assert_eq!(
            InstallSource::parse("./skills/pdf"),
            InstallSource::LocalPath(PathBuf::from("./skills/pdf"))
        );
    }

    #[tokio::test]
    async fn instruction_skill_tool_returns_guidance() {
        let skill = InstalledSkill {
            id: "sample-skill".into(),
            name: "sample skill".into(),
            description: "Does sample work".into(),
            short_description: None,
            path: PathBuf::from("/tmp/sample-skill"),
            source: SkillSource::Workspace,
            trust: CapabilityTrust::Trusted,
            permissions: vec![],
            reference: None,
            semantic_description: "sample semantic".into(),
            execution: SkillExecution::Instructions,
            instructions: "Step 1: do the thing".into(),
        };

        let result = skill
            .tool()
            .call(json!({"request": "help me"}))
            .await
            .unwrap();

        assert_eq!(result["mode"], "instructions");
        assert!(result["instructions"]
            .as_str()
            .unwrap()
            .contains("Step 1"));
    }

    #[tokio::test]
    async fn command_skill_requires_process_spawn_permission() {
        let skill = InstalledSkill {
            id: "sample-skill".into(),
            name: "sample skill".into(),
            description: "Does sample work".into(),
            short_description: None,
            path: PathBuf::from("/tmp/sample-skill"),
            source: SkillSource::Workspace,
            trust: CapabilityTrust::Trusted,
            permissions: vec![],
            reference: None,
            semantic_description: "sample semantic".into(),
            execution: SkillExecution::Command {
                command: "echo".into(),
                args: vec!["hello".into()],
            },
            instructions: String::new(),
        };

        let error = skill
            .tool()
            .call(json!({"request": "help me"}))
            .await
            .unwrap_err();

        assert!(error.to_string().contains("not allowed"));
    }

    #[test]
    fn command_failure_uses_structured_stdout_message_when_stderr_is_empty() {
        let detail = command_failure_detail(
            "",
            r#"{"status":"blocked","message":"reportlab is not installed"}"#,
        );
        assert_eq!(detail, "reportlab is not installed");
    }

    #[test]
    fn skill_registry_registers_installed_skill_tools() {
        let mut skills = SkillRegistry::new();
        skills.register(InstalledSkill {
            id: "sample-skill".into(),
            name: "sample skill".into(),
            description: "Does sample work".into(),
            short_description: None,
            path: PathBuf::from("/tmp/sample-skill"),
            source: SkillSource::Workspace,
            trust: CapabilityTrust::Trusted,
            permissions: vec![],
            reference: None,
            semantic_description: "sample semantic".into(),
            execution: SkillExecution::Instructions,
            instructions: "Step 1".into(),
        });

        let mut registry = ToolRegistry::new();
        skills.register_tools(&mut registry);
        assert!(registry.get_by_name("sample-skill").is_some());
    }
}
