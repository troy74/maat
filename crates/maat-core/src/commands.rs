#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandSpec {
    pub template: &'static str,
    pub description: &'static str,
}

#[derive(Debug, Default, Clone)]
pub struct CommandCompletionContext {
    pub sessions: Vec<String>,
    pub model_ids: Vec<String>,
    pub prompt_names: Vec<String>,
}

pub const COMMAND_SPECS: &[CommandSpec] = &[
    CommandSpec { template: "/help", description: "show command help" },
    CommandSpec { template: "/tools", description: "list loaded tools" },
    CommandSpec { template: "/skills", description: "list installed skills" },
    CommandSpec { template: "/skills search ", description: "search ClawHub" },
    CommandSpec { template: "/skills install ", description: "install a skill" },
    CommandSpec { template: "/artifacts", description: "list stored artifacts" },
    CommandSpec { template: "/artifacts import ", description: "import a local file as an artifact" },
    CommandSpec { template: "/artifacts show ", description: "show one artifact by handle" },
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

pub fn complete_command(
    input: &str,
    ctx: &CommandCompletionContext,
) -> Vec<String> {
    if !input.starts_with('/') {
        return Vec::new();
    }

    if let Some(prefix) = input.strip_prefix("/session use ") {
        return complete_dynamic(prefix, &ctx.sessions, "/session use ");
    }
    if let Some(prefix) = input.strip_prefix("/session end ") {
        return complete_dynamic(prefix, &ctx.sessions, "/session end ");
    }
    if let Some(prefix) = input.strip_prefix("/status ") {
        let mut sessions = ctx.sessions.clone();
        sessions.push("primary".into());
        return complete_dynamic(prefix, &sessions, "/status ");
    }
    if let Some(prefix) = input.strip_prefix("/purge ") {
        let mut sessions = ctx.sessions.clone();
        sessions.push("primary".into());
        return complete_dynamic(prefix, &sessions, "/purge ");
    }
    if let Some(prefix) = input.strip_prefix("/prompts show ") {
        return complete_dynamic(prefix, &ctx.prompt_names, "/prompts show ");
    }
    if let Some(prefix) = input.strip_prefix("/model set ") {
        let parts: Vec<&str> = prefix.split_whitespace().collect();
        return match parts.as_slice() {
            [] => ctx.model_ids.iter().map(|id| format!("/model set {id}")).collect(),
            [partial] => {
                let mut suggestions = complete_dynamic(partial, &ctx.sessions, "/model set ");
                suggestions.extend(ctx.model_ids.iter().filter(|id| id.starts_with(partial)).map(|id| format!("/model set {id}")));
                suggestions
            }
            [session, partial] => ctx
                .model_ids
                .iter()
                .filter(|id| id.starts_with(partial))
                .map(|id| format!("/model set {session} {id}"))
                .collect(),
            _ => Vec::new(),
        };
    }

    COMMAND_SPECS
        .iter()
        .filter(|spec| spec.template.starts_with(input) || input.starts_with(spec.template))
        .map(|spec| spec.template.to_string())
        .collect()
}

fn complete_dynamic(prefix: &str, items: &[String], command_prefix: &str) -> Vec<String> {
    items.iter()
        .filter(|item| item.starts_with(prefix))
        .map(|item| format!("{command_prefix}{item}"))
        .collect()
}
