use maat_core::{HeraldAttachment, HeraldPayload, ParsedCommand, SessionName};

/// Parse raw user input into a herald-agnostic payload, recognising `@name:`
/// and slash-command syntax.
pub fn parse_input(
    text: String,
    active_session: Option<&str>,
    attachments: Vec<HeraldAttachment>,
    artifact_handles: Vec<String>,
    focused_automation: Option<String>,
) -> HeraldPayload {
    let t = text.trim();

    if let Some(rest) = t.strip_prefix('@') {
        if let Some(colon) = rest.find(':') {
            let name = rest[..colon].trim();
            let msg = rest[colon + 1..].trim();
            if !name.is_empty() && !msg.is_empty() {
                if attachments.is_empty() && artifact_handles.is_empty() {
                    return HeraldPayload::Command(ParsedCommand::RouteToSession {
                        name: SessionName(name.to_string()),
                        message: msg.to_string(),
                    });
                }
                return HeraldPayload::Message {
                    text: msg.to_string(),
                    attachments,
                    artifact_handles,
                    session: Some(SessionName(name.to_string())),
                };
            }
        }
    }

    if let Some(rest) = t.strip_prefix("/session new ") {
        if let Some(colon) = rest.find(':') {
            let name = rest[..colon].trim();
            let desc = rest[colon + 1..].trim();
            if !name.is_empty() {
                return HeraldPayload::Command(ParsedCommand::SessionNew {
                    name: SessionName(name.to_string()),
                    description: desc.to_string(),
                });
            }
        }
    }

    if t == "/session list" || t == "/sessions" {
        return HeraldPayload::Command(ParsedCommand::SessionList);
    }

    if let Some(rest) = t.strip_prefix("/session end ") {
        let name = rest.trim();
        if !name.is_empty() {
            return HeraldPayload::Command(ParsedCommand::SessionEnd {
                name: SessionName(name.to_string()),
            });
        }
    }

    if t == "/status" {
        return HeraldPayload::Command(ParsedCommand::StatusAll);
    }
    if let Some(rest) = t.strip_prefix("/status ") {
        let name = rest.trim();
        if !name.is_empty() {
            return HeraldPayload::Command(ParsedCommand::StatusSession {
                name: SessionName(name.to_string()),
            });
        }
    }

    if t == "/models" || t == "/model list" {
        return HeraldPayload::Command(ParsedCommand::ModelList);
    }
    if let Some(rest) = t.strip_prefix("/model set ") {
        let parts: Vec<&str> = rest.split_whitespace().collect();
        match parts.as_slice() {
            [model_id] => {
                return HeraldPayload::Command(ParsedCommand::ModelSwap {
                    session: None,
                    model_id: (*model_id).to_string(),
                });
            }
            [session, model_id] => {
                return HeraldPayload::Command(ParsedCommand::ModelSwap {
                    session: Some(SessionName((*session).trim_start_matches('@').to_string())),
                    model_id: (*model_id).to_string(),
                });
            }
            _ => {}
        }
    }

    if let Some(rest) = t.strip_prefix("/purge ") {
        let session = rest.trim();
        if !session.is_empty() {
            return HeraldPayload::Command(ParsedCommand::Purge {
                session: SessionName(session.to_string()),
            });
        }
    }

    if t == "/tools" || t == "/talents" {
        return HeraldPayload::Command(ParsedCommand::ToolsList);
    }

    if t == "/skills" {
        return HeraldPayload::Command(ParsedCommand::SkillsList);
    }
    if t == "/skills reload" {
        return HeraldPayload::Command(ParsedCommand::SkillsReload);
    }
    if let Some(rest) = t.strip_prefix("/skills search ") {
        let query = rest.trim();
        if !query.is_empty() {
            return HeraldPayload::Command(ParsedCommand::SkillSearch {
                query: query.to_string(),
            });
        }
    }
    if let Some(rest) = t.strip_prefix("/skills install ") {
        let source = rest.trim();
        if !source.is_empty() {
            return HeraldPayload::Command(ParsedCommand::SkillInstall {
                source: source.to_string(),
            });
        }
    }

    if t == "/automations" || t == "/automation list" {
        return HeraldPayload::Command(ParsedCommand::AutomationsList);
    }
    if let Some(rest) = t.strip_prefix("/automation show ") {
        let name = rest.trim();
        if !name.is_empty() {
            return HeraldPayload::Command(ParsedCommand::AutomationShow { name: name.to_string() });
        }
    }
    if let Some(rest) = t.strip_prefix("/automation run ") {
        let name = rest.trim();
        if !name.is_empty() {
            return HeraldPayload::Command(ParsedCommand::AutomationRun { name: name.to_string() });
        }
    }
    if t == "/automation run" {
        if let Some(name) = focused_automation {
            return HeraldPayload::Command(ParsedCommand::AutomationRun { name });
        }
    }
    if let Some(rest) = t.strip_prefix("/automation pause ") {
        let name = rest.trim();
        if !name.is_empty() {
            return HeraldPayload::Command(ParsedCommand::AutomationPause { name: name.to_string() });
        }
    }
    if let Some(rest) = t.strip_prefix("/automation resume ") {
        let name = rest.trim();
        if !name.is_empty() {
            return HeraldPayload::Command(ParsedCommand::AutomationResume { name: name.to_string() });
        }
    }
    if let Some(rest) = t.strip_prefix("/automation delete ") {
        let name = rest.trim();
        if !name.is_empty() {
            return HeraldPayload::Command(ParsedCommand::AutomationDelete { name: name.to_string() });
        }
    }
    if t == "/runs" || t == "/run list" {
        return HeraldPayload::Command(ParsedCommand::RunsList);
    }
    if let Some(rest) = t.strip_prefix("/run show ") {
        let handle = rest.trim();
        if !handle.is_empty() {
            return HeraldPayload::Command(ParsedCommand::RunShow { handle: handle.to_string() });
        }
    }
    if let Some(rest) = t.strip_prefix("/run open ") {
        let handle = rest.trim();
        if !handle.is_empty() {
            return HeraldPayload::Command(ParsedCommand::RunOpen { handle: handle.to_string() });
        }
    }
    if let Some(rest) = t.strip_prefix("/run cancel ") {
        let handle = rest.trim();
        if !handle.is_empty() {
            return HeraldPayload::Command(ParsedCommand::RunCancel { handle: handle.to_string() });
        }
    }
    if let Some(rest) = t.strip_prefix("/run start ") {
        if let Some((title, prompt)) = rest.split_once(':') {
            let title = title.trim();
            let prompt = prompt.trim();
            if !title.is_empty() && !prompt.is_empty() {
                return HeraldPayload::Command(ParsedCommand::RunStart {
                    title: title.to_string(),
                    prompt: prompt.to_string(),
                });
            }
        }
    }
    if let Some(rest) = t.strip_prefix("/automation create ") {
        if let Some((name, rhs)) = rest.split_once('|') {
            if let Some((schedule, prompt)) = rhs.split_once('|') {
                return HeraldPayload::Command(ParsedCommand::AutomationCreate {
                    name: name.trim().to_string(),
                    schedule: schedule.trim().to_string(),
                    prompt: prompt.trim().to_string(),
                });
            }
        }
    }
    if let Some(rest) = t.strip_prefix("/automation edit ") {
        if let Some((name, rhs)) = rest.split_once('|') {
            if let Some((schedule, prompt)) = rhs.split_once('|') {
                return HeraldPayload::Command(ParsedCommand::AutomationEdit {
                    name: name.trim().to_string(),
                    schedule: schedule.trim().to_string(),
                    prompt: prompt.trim().to_string(),
                });
            }
        }
    }

    if t == "/artifacts" {
        return HeraldPayload::Command(ParsedCommand::ArtifactsList);
    }
    if let Some(rest) = t.strip_prefix("/artifacts import ") {
        let path = rest.trim();
        if !path.is_empty() {
            return HeraldPayload::Command(ParsedCommand::ArtifactImport {
                path: path.to_string(),
            });
        }
    }
    if let Some(rest) = t.strip_prefix("/artifacts show ") {
        let handle = rest.trim();
        if !handle.is_empty() {
            return HeraldPayload::Command(ParsedCommand::ArtifactShow {
                handle: handle.to_string(),
            });
        }
    }

    if let Some(rest) = t.strip_prefix("/memory add ") {
        let text = rest.trim();
        if !text.is_empty() {
            return HeraldPayload::Command(ParsedCommand::MemoryAdd {
                text: text.to_string(),
            });
        }
    }
    if let Some(rest) = t.strip_prefix("/mistake add ") {
        let text = rest.trim();
        if !text.is_empty() {
            return HeraldPayload::Command(ParsedCommand::MistakeAdd {
                text: text.to_string(),
            });
        }
    }
    if let Some(rest) = t.strip_prefix("/user note add ") {
        let text = rest.trim();
        if !text.is_empty() {
            return HeraldPayload::Command(ParsedCommand::UserNoteAdd {
                user: None,
                text: text.to_string(),
            });
        }
    }
    if let Some(rest) = t.strip_prefix("/persona append ") {
        let text = rest.trim();
        if !text.is_empty() {
            return HeraldPayload::Command(ParsedCommand::PersonaAppend {
                text: text.to_string(),
            });
        }
    }

    if t == "/prompts" {
        return HeraldPayload::Command(ParsedCommand::PromptsList);
    }
    if let Some(rest) = t.strip_prefix("/prompts show ") {
        let name = rest.trim();
        if !name.is_empty() {
            return HeraldPayload::Command(ParsedCommand::PromptShow {
                name: name.to_string(),
            });
        }
    }

    if t == "/config" {
        return HeraldPayload::Command(ParsedCommand::ConfigShow);
    }
    if let Some(rest) = t.strip_prefix("/config set ") {
        if let Some((key, val)) = rest.split_once(' ') {
            return HeraldPayload::Command(ParsedCommand::ConfigSet {
                key: key.trim().to_string(),
                value: val.trim().to_string(),
            });
        }
    }

    if t == "/secret list" {
        return HeraldPayload::Command(ParsedCommand::SecretList);
    }
    if let Some(rest) = t.strip_prefix("/secret set ") {
        if let Some((key, val)) = rest.split_once(' ') {
            return HeraldPayload::Command(ParsedCommand::SecretSet {
                key: key.trim().to_string(),
                value: val.trim().to_string(),
            });
        }
    }
    if let Some(key) = t.strip_prefix("/secret delete ") {
        return HeraldPayload::Command(ParsedCommand::SecretDelete {
            key: key.trim().to_string(),
        });
    }

    if t == "/auth google" {
        return HeraldPayload::Command(ParsedCommand::AuthGoogle);
    }

    if let Some(session) = active_session {
        if !t.starts_with('/') {
            return HeraldPayload::Message {
                text,
                attachments,
                artifact_handles,
                session: Some(SessionName(session.to_string())),
            };
        }
    }

    if attachments.is_empty() && artifact_handles.is_empty() {
        HeraldPayload::Text(text)
    } else {
        let text = if t.is_empty() {
            "Please inspect the attached item(s) and help with them.".to_string()
        } else {
            text
        };
        HeraldPayload::Message {
            text,
            attachments,
            artifact_handles,
            session: None,
        }
    }
}
