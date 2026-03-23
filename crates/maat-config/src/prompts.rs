use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Debug, Clone)]
pub struct PromptLibrary {
    dir: PathBuf,
    pub primary_system: String,
    pub named_session: String,
    pub compaction: String,
    pub capability_nudge: String,
    bouquet: PromptBouquet,
}

#[derive(Debug, Clone)]
struct PromptBouquet {
    identity: ManagedPromptFile,
    persona: ManagedPromptFile,
    rules: ManagedPromptFile,
    workflow: ManagedPromptFile,
    memory: ManagedPromptFile,
    mistakes: ManagedPromptFile,
    users_default: ManagedPromptFile,
    user_overrides: HashMap<String, ManagedPromptFile>,
}

#[derive(Debug, Clone)]
struct ManagedPromptFile {
    path: PathBuf,
    content: String,
    policy: UpdatePolicy,
}

#[derive(Debug, Clone)]
pub struct PromptAssetInfo {
    pub name: String,
    pub path: PathBuf,
    pub policy: Option<UpdatePolicy>,
    pub content: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdatePolicy {
    HumanOnly,
    AppendOnly,
    RuleGated,
}

#[derive(Debug, Deserialize, Default)]
struct PromptFileFrontmatter {
    #[serde(default)]
    update_policy: Option<String>,
}

impl PromptLibrary {
    pub fn load(dir: impl AsRef<Path>) -> Self {
        let dir = dir.as_ref();
        let bouquet_dir = dir.join("bouquet");
        Self {
            dir: dir.to_path_buf(),
            primary_system: read_or_default(dir.join("primary_system.md"), DEFAULT_PRIMARY_SYSTEM),
            named_session: read_or_default(dir.join("named_session.md"), DEFAULT_NAMED_SESSION),
            compaction: read_or_default(dir.join("compaction.md"), DEFAULT_COMPACTION),
            capability_nudge: read_or_default(
                dir.join("capability_nudge.md"),
                DEFAULT_CAPABILITY_NUDGE,
            ),
            bouquet: PromptBouquet {
                identity: load_managed_file(
                    bouquet_dir.join("identity.md"),
                    DEFAULT_IDENTITY,
                    UpdatePolicy::HumanOnly,
                ),
                persona: load_managed_file(
                    bouquet_dir.join("persona.md"),
                    DEFAULT_PERSONA,
                    UpdatePolicy::RuleGated,
                ),
                rules: load_managed_file(
                    bouquet_dir.join("rules.md"),
                    DEFAULT_RULES,
                    UpdatePolicy::HumanOnly,
                ),
                workflow: load_managed_file(
                    bouquet_dir.join("workflow.md"),
                    DEFAULT_WORKFLOW,
                    UpdatePolicy::HumanOnly,
                ),
                memory: load_managed_file(
                    bouquet_dir.join("memory.md"),
                    DEFAULT_MEMORY,
                    UpdatePolicy::AppendOnly,
                ),
                mistakes: load_managed_file(
                    bouquet_dir.join("mistakes.md"),
                    DEFAULT_MISTAKES,
                    UpdatePolicy::AppendOnly,
                ),
                users_default: load_managed_file(
                    bouquet_dir.join("users").join("default.md"),
                    DEFAULT_USER,
                    UpdatePolicy::AppendOnly,
                ),
                user_overrides: HashMap::new(),
            },
        }
    }

    pub fn render_primary_system(&self, user_id: &str, tool_lines: &str) -> String {
        self.primary_system
            .replace("{{BOUQUET}}", &self.render_bouquet(user_id))
            .replace("{{TOOLS}}", tool_lines)
    }

    pub fn render_named_session(
        &self,
        user_id: &str,
        session_context: &str,
        tool_lines: &str,
    ) -> String {
        self.named_session
            .replace("{{BOUQUET}}", &self.render_bouquet(user_id))
            .replace("{{SESSION_CONTEXT}}", session_context)
            .replace("{{TOOLS}}", tool_lines)
    }

    pub fn append_memory(&mut self, entry: &str) -> Result<(), String> {
        append_entry(&mut self.bouquet.memory, "Memory", entry)
    }

    pub fn append_mistake(&mut self, entry: &str) -> Result<(), String> {
        append_entry(&mut self.bouquet.mistakes, "Mistake", entry)
    }

    pub fn append_user_note(&mut self, user_id: &str, entry: &str) -> Result<(), String> {
        let key = sanitize_user_id(user_id);
        if !self.bouquet.user_overrides.contains_key(&key) {
            let path = self.dir.join("bouquet").join("users").join(format!("{key}.md"));
            let managed = if path.exists() {
                load_managed_file(path, DEFAULT_USER, UpdatePolicy::AppendOnly)
            } else {
                create_managed_file(path, DEFAULT_USER, UpdatePolicy::AppendOnly)?
            };
            self.bouquet.user_overrides.insert(key.clone(), managed);
        }
        let managed = self
            .bouquet
            .user_overrides
            .get_mut(&key)
            .ok_or_else(|| "failed to load user prompt file".to_string())?;
        append_entry(managed, "User Note", entry)
    }

    pub fn append_persona(&mut self, entry: &str) -> Result<(), String> {
        if self.bouquet.persona.policy == UpdatePolicy::HumanOnly {
            return Err("persona updates are human-only".into());
        }
        if self.bouquet.persona.policy == UpdatePolicy::RuleGated
            && !self
                .bouquet
                .rules
                .content
                .contains("ALLOW_PERSONA_SELF_UPDATE")
        {
            return Err("RULES.md does not allow persona self-update".into());
        }
        append_entry(&mut self.bouquet.persona, "Persona Update", entry)
    }

    pub fn assets(&self, user_id: &str) -> Vec<PromptAssetInfo> {
        let mut assets = vec![
            PromptAssetInfo {
                name: "primary_system".into(),
                path: self.dir.join("primary_system.md"),
                policy: None,
                content: self.primary_system.clone(),
            },
            PromptAssetInfo {
                name: "named_session".into(),
                path: self.dir.join("named_session.md"),
                policy: None,
                content: self.named_session.clone(),
            },
            PromptAssetInfo {
                name: "compaction".into(),
                path: self.dir.join("compaction.md"),
                policy: None,
                content: self.compaction.clone(),
            },
            PromptAssetInfo {
                name: "capability_nudge".into(),
                path: self.dir.join("capability_nudge.md"),
                policy: None,
                content: self.capability_nudge.clone(),
            },
            prompt_asset("identity", &self.bouquet.identity),
            prompt_asset("persona", &self.bouquet.persona),
            prompt_asset("rules", &self.bouquet.rules),
            prompt_asset("workflow", &self.bouquet.workflow),
            prompt_asset("memory", &self.bouquet.memory),
            prompt_asset("mistakes", &self.bouquet.mistakes),
            prompt_asset("users/default", &self.bouquet.users_default),
        ];
        let key = sanitize_user_id(user_id);
        if let Some(user_file) = self.bouquet.user_overrides.get(&key) {
            assets.push(prompt_asset(&format!("users/{key}"), user_file));
        } else {
            let path = self.dir.join("bouquet").join("users").join(format!("{key}.md"));
            if path.exists() {
                let loaded = load_managed_file(path, DEFAULT_USER, UpdatePolicy::AppendOnly);
                assets.push(prompt_asset(&format!("users/{key}"), &loaded));
            }
        }
        assets
    }

    pub fn asset_names(&self, user_id: &str) -> Vec<String> {
        self.assets(user_id).into_iter().map(|asset| asset.name).collect()
    }

    pub fn show_asset(&self, user_id: &str, name: &str) -> Option<PromptAssetInfo> {
        self.assets(user_id)
            .into_iter()
            .find(|asset| asset.name == name)
    }

    fn render_bouquet(&self, user_id: &str) -> String {
        let user_section = self.user_prompt_content(user_id);

        [
            self.bouquet.identity.content.as_str(),
            self.bouquet.persona.content.as_str(),
            self.bouquet.rules.content.as_str(),
            self.bouquet.workflow.content.as_str(),
            self.bouquet.memory.content.as_str(),
            self.bouquet.mistakes.content.as_str(),
            user_section.as_str(),
        ]
        .join("\n\n")
    }

    fn user_prompt_content(&self, user_id: &str) -> String {
        let key = sanitize_user_id(user_id);
        if let Some(file) = self.bouquet.user_overrides.get(&key) {
            return file.content.clone();
        }
        let path = self.dir.join("bouquet").join("users").join(format!("{key}.md"));
        if path.exists() {
            load_managed_file(path, DEFAULT_USER, UpdatePolicy::AppendOnly).content
        } else {
            self.bouquet.users_default.content.clone()
        }
    }
}

fn load_managed_file(path: PathBuf, default: &str, default_policy: UpdatePolicy) -> ManagedPromptFile {
    let raw = read_or_default(path.clone(), default);
    let (frontmatter, content) = split_frontmatter(&raw);
    let parsed: PromptFileFrontmatter = serde_yaml::from_str(frontmatter).unwrap_or_default();
    let policy = parsed
        .update_policy
        .as_deref()
        .map(parse_policy)
        .unwrap_or(default_policy);
    ManagedPromptFile {
        path,
        content: content.trim().to_string(),
        policy,
    }
}

fn prompt_asset(name: &str, file: &ManagedPromptFile) -> PromptAssetInfo {
    PromptAssetInfo {
        name: name.to_string(),
        path: file.path.clone(),
        policy: Some(file.policy),
        content: file.content.clone(),
    }
}

fn create_managed_file(
    path: PathBuf,
    default: &str,
    default_policy: UpdatePolicy,
) -> Result<ManagedPromptFile, String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
    }
    std::fs::write(&path, default)
        .map_err(|e| format!("failed to write {}: {e}", path.display()))?;
    Ok(load_managed_file(path, default, default_policy))
}

fn append_entry(file: &mut ManagedPromptFile, heading: &str, entry: &str) -> Result<(), String> {
    if file.policy == UpdatePolicy::HumanOnly {
        return Err(format!("{} is human-only", file.path.display()));
    }
    let mut new_content = file.content.clone();
    if !new_content.ends_with('\n') {
        new_content.push('\n');
    }
    new_content.push_str(&format!("\n## {heading}\n- {entry}\n"));
    let updated = rebuild_with_policy_header(&new_content, file.policy);
    std::fs::write(&file.path, &updated)
        .map_err(|e| format!("failed to write {}: {e}", file.path.display()))?;
    file.content = new_content.trim().to_string();
    Ok(())
}

fn rebuild_with_policy_header(content: &str, policy: UpdatePolicy) -> String {
    format!(
        "---\nupdate_policy: {}\n---\n\n{}\n",
        policy_name(policy),
        content.trim()
    )
}

fn read_or_default(path: PathBuf, default: &str) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|_| default.to_string())
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

fn parse_policy(value: &str) -> UpdatePolicy {
    match value.trim().to_ascii_lowercase().as_str() {
        "append_only" => UpdatePolicy::AppendOnly,
        "rule_gated" => UpdatePolicy::RuleGated,
        _ => UpdatePolicy::HumanOnly,
    }
}

fn policy_name(policy: UpdatePolicy) -> &'static str {
    match policy {
        UpdatePolicy::HumanOnly => "human_only",
        UpdatePolicy::AppendOnly => "append_only",
        UpdatePolicy::RuleGated => "rule_gated",
    }
}

fn sanitize_user_id(user_id: &str) -> String {
    user_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

const DEFAULT_PRIMARY_SYSTEM: &str = "You are MAAT.\n\n{{BOUQUET}}\n\nYou have access to the following tools and MUST use them when relevant instead of saying you cannot perform a task:\n{{TOOLS}}\n";

const DEFAULT_NAMED_SESSION: &str = "You are MAAT.\n\n{{BOUQUET}}\n\nSession context:\n{{SESSION_CONTEXT}}\n\nYou have access to the following tools and MUST use them when relevant instead of saying you cannot perform a task:\n{{TOOLS}}\n";

const DEFAULT_COMPACTION: &str = "You are a concise summariser. Summarise the conversation below, preserving key facts, decisions, user preferences, and important context. Be brief and avoid editorializing.";

const DEFAULT_CAPABILITY_NUDGE: &str = "You are a capability router. Given a user request and a shortlist of candidate capabilities, pick the best-fit capabilities and explain the fit briefly. Prefer explicit tag/schema matches, but use semantic clues when metadata is incomplete. Never override hard policy constraints.";

const DEFAULT_IDENTITY: &str = "---\nupdate_policy: human_only\n---\n\n# Identity\nMAAT is a thoughtful, capable, tool-using assistant.";

const DEFAULT_PERSONA: &str = "---\nupdate_policy: rule_gated\n---\n\n# Persona\nBe warm, concise, practical, and calm.";

const DEFAULT_RULES: &str = "---\nupdate_policy: human_only\n---\n\n# Rules\n- Use tools when they are the right way to complete the task.\n- Prefer visible, editable prompt files over compiled prompt strings.\n- Do not silently rewrite your own governing files unless policy allows it.\n- Do not claim a file was saved, an email was sent, or an attachment was added unless the relevant tool result confirms it.\n- ALLOW_PERSONA_SELF_UPDATE may be added here by a human if persona self-edits should be allowed.";

const DEFAULT_WORKFLOW: &str = "---\nupdate_policy: human_only\n---\n\n# Workflow\n- Treat instruction-style skills as guidance, not completion. After reading the guidance, call the concrete talents needed to do the work.\n- For multi-step artifact tasks, work in order: create or update the artifact, verify it exists and looks right, then send or publish it.\n- Use stable WIP locations for intermediate files. For PDF work in this repo, use `tmp/pdfs/` for drafts and `output/pdf/` for final artifacts unless the user asks for something else.\n- If a required step is not actually possible with the available tools, say exactly which step is blocked and stop short of claiming success.";

const DEFAULT_MEMORY: &str = "---\nupdate_policy: append_only\n---\n\n# Hard Memory\nDurable facts and preferences can be appended here.";

const DEFAULT_MISTAKES: &str = "---\nupdate_policy: append_only\n---\n\n# Mistakes\nRepeat failures and corrections can be appended here.";

const DEFAULT_USER: &str = "---\nupdate_policy: append_only\n---\n\n# User Notes\nPer-user preferences and working notes can be appended here.";
