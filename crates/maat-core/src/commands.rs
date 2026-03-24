#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandSpec {
    pub template: &'static str,
    pub description: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuggestionKind {
    Command,
    Session,
    Model,
    Prompt,
    Artifact,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSuggestion {
    pub replacement: String,
    pub label: String,
    pub detail: String,
    pub kind: SuggestionKind,
}

#[derive(Debug, Default, Clone)]
pub struct CommandCompletionContext {
    pub sessions: Vec<String>,
    pub model_ids: Vec<String>,
    pub prompt_names: Vec<String>,
    pub artifact_handles: Vec<String>,
    pub automations: Vec<String>,
    pub runs: Vec<String>,
}

pub const COMMAND_SPECS: &[CommandSpec] = &[
    CommandSpec { template: "/help", description: "show command help" },
    CommandSpec { template: "/tools", description: "list loaded tools" },
    CommandSpec { template: "/skills", description: "list installed skills" },
    CommandSpec { template: "/skills search ", description: "search ClawHub" },
    CommandSpec { template: "/skills install ", description: "install a skill" },
    CommandSpec { template: "/skills reload", description: "reload installed skills without restart" },
    CommandSpec { template: "/automations", description: "list configured automations" },
    CommandSpec { template: "/automation show ", description: "show one automation" },
    CommandSpec { template: "/automation run ", description: "run an automation now" },
    CommandSpec { template: "/automation pause ", description: "pause an automation" },
    CommandSpec { template: "/automation resume ", description: "resume an automation" },
    CommandSpec { template: "/automation create ", description: "create an automation" },
    CommandSpec { template: "/automation edit ", description: "edit an automation" },
    CommandSpec { template: "/automation delete ", description: "delete an automation" },
    CommandSpec { template: "/runs", description: "list background runs" },
    CommandSpec { template: "/run show ", description: "show one background run" },
    CommandSpec { template: "/run open ", description: "focus a run session" },
    CommandSpec { template: "/run cancel ", description: "request cancellation of a background run" },
    CommandSpec { template: "/run start ", description: "start a background run" },
    CommandSpec { template: "/artifacts", description: "list stored artifacts" },
    CommandSpec { template: "/artifacts import ", description: "import a local file as an artifact" },
    CommandSpec { template: "/artifacts show ", description: "show one artifact by handle" },
    CommandSpec { template: "/attach ", description: "attach a local file to the draft message" },
    CommandSpec { template: "/attach clear", description: "clear draft attachments" },
    CommandSpec { template: "/detach ", description: "remove one draft attachment by index or name" },
    CommandSpec { template: "/memory add ", description: "append durable memory" },
    CommandSpec { template: "/mistake add ", description: "append a mistake/correction" },
    CommandSpec { template: "/user note add ", description: "append a user note" },
    CommandSpec { template: "/persona append ", description: "append a persona change if allowed" },
    CommandSpec { template: "/prompts", description: "list prompt assets" },
    CommandSpec { template: "/prompts show ", description: "show one prompt asset" },
    CommandSpec { template: "/config", description: "show config" },
    CommandSpec { template: "/config set ", description: "set config value" },
    CommandSpec { template: "/secret list", description: "list secret keys" },
    CommandSpec { template: "/secret set ", description: "store a secret" },
    CommandSpec { template: "/secret delete ", description: "delete a secret" },
    CommandSpec { template: "/auth google", description: "start Google auth" },
    CommandSpec { template: "/session list", description: "list sessions" },
    CommandSpec { template: "/session new ", description: "create a session" },
    CommandSpec { template: "/session use ", description: "set local active session" },
    CommandSpec { template: "/session leave", description: "clear local active session" },
    CommandSpec { template: "/session end ", description: "end a named session" },
    CommandSpec { template: "/status", description: "show PHAROH status" },
    CommandSpec { template: "/status ", description: "show one session status" },
    CommandSpec { template: "/models", description: "list model profiles" },
    CommandSpec { template: "/model set ", description: "set model/profile" },
    CommandSpec { template: "/purge ", description: "purge a session" },
];

pub fn command_specs() -> &'static [CommandSpec] {
    COMMAND_SPECS
}

pub fn suggest_commands(
    input: &str,
    ctx: &CommandCompletionContext,
) -> Vec<CommandSuggestion> {
    if !input.starts_with('/') {
        return Vec::new();
    }

    if let Some(prefix) = input.strip_prefix("/session use ") {
        return dynamic_suggestions(prefix, &ctx.sessions, "/session use ", SuggestionKind::Session, "use session");
    }
    if let Some(prefix) = input.strip_prefix("/session end ") {
        return dynamic_suggestions(prefix, &ctx.sessions, "/session end ", SuggestionKind::Session, "end session");
    }
    if let Some(prefix) = input.strip_prefix("/status ") {
        let mut sessions = ctx.sessions.clone();
        sessions.push("primary".into());
        return dynamic_suggestions(prefix, &sessions, "/status ", SuggestionKind::Session, "show status");
    }
    if let Some(prefix) = input.strip_prefix("/purge ") {
        let mut sessions = ctx.sessions.clone();
        sessions.push("primary".into());
        return dynamic_suggestions(prefix, &sessions, "/purge ", SuggestionKind::Session, "purge session");
    }
    if let Some(prefix) = input.strip_prefix("/prompts show ") {
        return dynamic_suggestions(prefix, &ctx.prompt_names, "/prompts show ", SuggestionKind::Prompt, "show prompt");
    }
    if let Some(prefix) = input.strip_prefix("/artifacts show ") {
        return dynamic_suggestions(prefix, &ctx.artifact_handles, "/artifacts show ", SuggestionKind::Artifact, "show artifact");
    }
    if let Some(prefix) = input.strip_prefix("/automation show ") {
        return dynamic_suggestions(prefix, &ctx.automations, "/automation show ", SuggestionKind::Command, "show automation");
    }
    if let Some(prefix) = input.strip_prefix("/automation run ") {
        return dynamic_suggestions(prefix, &ctx.automations, "/automation run ", SuggestionKind::Command, "run automation");
    }
    if let Some(prefix) = input.strip_prefix("/automation pause ") {
        return dynamic_suggestions(prefix, &ctx.automations, "/automation pause ", SuggestionKind::Command, "pause automation");
    }
    if let Some(prefix) = input.strip_prefix("/automation resume ") {
        return dynamic_suggestions(prefix, &ctx.automations, "/automation resume ", SuggestionKind::Command, "resume automation");
    }
    if let Some(prefix) = input.strip_prefix("/automation edit ") {
        return dynamic_suggestions(prefix, &ctx.automations, "/automation edit ", SuggestionKind::Command, "edit automation");
    }
    if let Some(prefix) = input.strip_prefix("/automation delete ") {
        return dynamic_suggestions(prefix, &ctx.automations, "/automation delete ", SuggestionKind::Command, "delete automation");
    }
    if let Some(prefix) = input.strip_prefix("/run show ") {
        return dynamic_suggestions(prefix, &ctx.runs, "/run show ", SuggestionKind::Command, "show background run");
    }
    if let Some(prefix) = input.strip_prefix("/run open ") {
        return dynamic_suggestions(prefix, &ctx.runs, "/run open ", SuggestionKind::Command, "open background run");
    }
    if let Some(prefix) = input.strip_prefix("/run cancel ") {
        return dynamic_suggestions(prefix, &ctx.runs, "/run cancel ", SuggestionKind::Command, "cancel background run");
    }
    if let Some(prefix) = input.strip_prefix("/model set ") {
        let parts: Vec<&str> = prefix.split_whitespace().collect();
        return match parts.as_slice() {
            [] => ctx
                .model_ids
                .iter()
                .map(|id| suggestion(format!("/model set {id}"), id.clone(), "set primary model".into(), SuggestionKind::Model))
                .collect(),
            [partial] => {
                let mut suggestions = dynamic_suggestions(partial, &ctx.sessions, "/model set ", SuggestionKind::Session, "target session");
                suggestions.extend(
                    ctx.model_ids
                        .iter()
                        .filter(|id| id.starts_with(partial))
                        .map(|id| suggestion(format!("/model set {id}"), id.clone(), "set primary model".into(), SuggestionKind::Model)),
                );
                suggestions
            }
            [session, partial] => ctx
                .model_ids
                .iter()
                .filter(|id| id.starts_with(partial))
                .map(|id| suggestion(
                    format!("/model set {session} {id}"),
                    id.clone(),
                    format!("set model for {session}"),
                    SuggestionKind::Model,
                ))
                .collect(),
            _ => Vec::new(),
        };
    }

    COMMAND_SPECS
        .iter()
        .filter(|spec| spec.template.starts_with(input) || input.starts_with(spec.template))
        .map(|spec| {
            suggestion(
                spec.template.to_string(),
                spec.template.trim().to_string(),
                spec.description.to_string(),
                SuggestionKind::Command,
            )
        })
        .collect()
}

pub fn complete_command(
    input: &str,
    ctx: &CommandCompletionContext,
) -> Vec<String> {
    suggest_commands(input, ctx)
        .into_iter()
        .map(|suggestion| suggestion.replacement)
        .collect()
}

pub fn common_completion_prefix(items: &[String]) -> String {
    if items.is_empty() {
        return String::new();
    }
    let mut prefix = items[0].to_string();
    for item in items.iter().skip(1) {
        while !item.starts_with(&prefix) && !prefix.is_empty() {
            prefix.pop();
        }
    }
    prefix
}

pub fn completion_suffix(input: &str, suggestion: &str) -> Option<String> {
    suggestion
        .strip_prefix(input)
        .filter(|suffix| !suffix.is_empty())
        .map(|suffix| suffix.to_string())
}

fn dynamic_suggestions(
    prefix: &str,
    items: &[String],
    command_prefix: &str,
    kind: SuggestionKind,
    detail: &str,
) -> Vec<CommandSuggestion> {
    items.iter()
        .filter(|item| item.starts_with(prefix))
        .map(|item| suggestion(
            format!("{command_prefix}{item}"),
            item.clone(),
            detail.to_string(),
            kind,
        ))
        .collect()
}

fn suggestion(
    replacement: String,
    label: String,
    detail: String,
    kind: SuggestionKind,
) -> CommandSuggestion {
    CommandSuggestion { replacement, label, detail, kind }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_ctx() -> CommandCompletionContext {
        CommandCompletionContext {
            sessions: vec!["coding".into(), "ops".into()],
            model_ids: vec!["default".into(), "gemini_flash".into()],
            prompt_names: vec!["workflow".into(), "persona".into()],
            artifact_handles: vec!["bright-canvas-a1b2".into()],
            automations: vec!["daily-summary".into()],
            runs: vec!["quiet-thread-a1b2".into()],
        }
    }

    #[test]
    fn suggests_dynamic_session_targets() {
        let suggestions = suggest_commands("/session use c", &test_ctx());
        assert_eq!(suggestions[0].replacement, "/session use coding");
        assert_eq!(suggestions[0].kind, SuggestionKind::Session);
    }

    #[test]
    fn suggests_model_targets_for_model_set() {
        let suggestions = suggest_commands("/model set g", &test_ctx());
        assert!(suggestions.iter().any(|item| item.replacement == "/model set gemini_flash"));
    }

    #[test]
    fn computes_completion_suffix() {
        let suffix = completion_suffix("/art", "/artifacts").unwrap();
        assert_eq!(suffix, "ifacts");
    }
}
