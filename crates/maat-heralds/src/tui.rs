//! TUI herald — ratatui + crossterm.
//!
//! Layout:
//!   ┌─────────────────────┐
//!   │  messages (scroll)  │
//!   ├─────────────────────┤
//!   │  input bar          │
//!   ├─────────────────────┤
//!   │  status line        │
//!   └─────────────────────┘
//!
//! Communicates with the backend via plain tokio mpsc channels —
//! no dependency on maat-pharoh.

use std::io::stdout;

use crossterm::{
    event::{Event, EventStream, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures::StreamExt;
use maat_core::commands::{command_specs, complete_command, CommandCompletionContext};
use maat_core::{
    ChatMessage, ChatReply, HeraldPayload, ParsedCommand, Role, SessionName, TokenUsage,
    TuiEvent,
};
use ratatui::{
    layout::{Constraint, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
    DefaultTerminal, Frame,
};
use tokio::sync::mpsc;

// ─────────────────────────────────────────────
// Application state
// ─────────────────────────────────────────────

struct App {
    messages: Vec<ChatMessage>,
    input: String,
    status: String,
    model_id: String,
    quit: bool,
    // Scroll state: offset in rendered lines from the top.
    scroll_offset: usize,
    // When true, jump to bottom on each new message.
    auto_scroll: bool,
    // Stats from the last completed turn.
    last_usage: Option<TokenUsage>,
    last_latency_ms: Option<u64>,
    last_total_lines: usize,
    last_visible_h: usize,
    active_session: Option<String>,
    completion_matches: Vec<String>,
    completion_index: usize,
    known_sessions: Vec<String>,
    known_models: Vec<String>,
    known_prompts: Vec<String>,
}

impl App {
    fn new(model_id: String) -> Self {
        Self {
            messages: Vec::new(),
            input: String::new(),
            status: "Ready".into(),
            model_id,
            quit: false,
            scroll_offset: 0,
            auto_scroll: true,
            last_usage: None,
            last_latency_ms: None,
            last_total_lines: 0,
            last_visible_h: 0,
            active_session: None,
            completion_matches: Vec::new(),
            completion_index: 0,
            known_sessions: Vec::new(),
            known_models: vec!["default".into()],
            known_prompts: vec![
                "primary_system".into(),
                "named_session".into(),
                "compaction".into(),
                "capability_nudge".into(),
                "identity".into(),
                "persona".into(),
                "rules".into(),
                "memory".into(),
                "mistakes".into(),
                "users/default".into(),
            ],
        }
    }

    fn on_reply(&mut self, reply: ChatReply) {
        self.messages.push(ChatMessage::assistant(&reply.content));
        self.last_usage = Some(reply.usage);
        self.last_latency_ms = Some(reply.latency_ms);
        self.status = "Ready".into();
        self.auto_scroll = true; // snap back to bottom on new message
    }

    fn scroll_up(&mut self, n: usize) {
        self.scroll_offset = self.scroll_offset.saturating_sub(n);
        self.auto_scroll = false;
    }

    fn scroll_down(&mut self, n: usize) {
        let max = self.last_total_lines.saturating_sub(self.last_visible_h);
        self.scroll_offset = (self.scroll_offset + n).min(max);
        if self.scroll_offset >= max {
            self.auto_scroll = true;
        }
    }

    fn snap_to_bottom(&mut self, total_lines: usize, visible_h: usize) {
        self.scroll_offset = total_lines.saturating_sub(visible_h);
    }

    fn reset_completion(&mut self) {
        self.completion_matches.clear();
        self.completion_index = 0;
    }
}

// ─────────────────────────────────────────────
// Entry point
// ─────────────────────────────────────────────

pub async fn run_tui(
    tx: mpsc::Sender<HeraldPayload>,
    mut rx: mpsc::Receiver<TuiEvent>,
    model_id: String,
) -> anyhow::Result<()> {
    enable_raw_mode()?;
    execute!(stdout(), EnterAlternateScreen)?;
    let mut terminal = ratatui::init();

    let result = event_loop(&mut terminal, &tx, &mut rx, model_id).await;

    ratatui::restore();
    disable_raw_mode()?;
    execute!(stdout(), LeaveAlternateScreen)?;

    result
}

// ─────────────────────────────────────────────
// Event loop
// ─────────────────────────────────────────────

async fn event_loop(
    terminal: &mut DefaultTerminal,
    tx: &mpsc::Sender<HeraldPayload>,
    rx: &mut mpsc::Receiver<TuiEvent>,
    model_id: String,
) -> anyhow::Result<()> {
    let mut app = App::new(model_id);
    let mut events = EventStream::new();

    while !app.quit {
        terminal.draw(|f| render(f, &mut app))?;

        tokio::select! {
            Some(Ok(event)) = events.next() => {
                if let Event::Key(key) = event {
                    match key.code {
                        // Quit
                        KeyCode::Char('c')
                            if key.modifiers.contains(KeyModifiers::CONTROL) =>
                        {
                            app.quit = true;
                        }

                        // Send message
                        KeyCode::Enter if !app.input.is_empty() => {
                            let text = std::mem::take(&mut app.input);
                            app.reset_completion();

                            if text.trim() == "/help" {
                                app.messages.push(ChatMessage {
                                    role: Role::System,
                                    tool_call_id: None,
                                    tool_calls_json: None,
                                    content: render_command_help().into(),
                                });
                            } else {
                                match handle_local_input(&mut app, text.clone()) {
                                    LocalInput::Handled(message) => {
                                        app.messages.push(ChatMessage {
                                            role: Role::System,
                                            tool_call_id: None,
                                            tool_calls_json: None,
                                            content: message,
                                        });
                                    }
                                    LocalInput::Send(payload) => {
                                        remember_command_side_effects(&mut app, &payload);
                                        app.messages.push(ChatMessage::user(&text));
                                        app.status = "Thinking…".into();
                                        let _ = tx.send(payload).await;
                                    }
                                }
                            }
                        }

                        // Backspace
                        KeyCode::Backspace => {
                            app.input.pop();
                            app.reset_completion();
                        }

                        // Scroll
                        KeyCode::PageUp   => app.scroll_up(10),
                        KeyCode::PageDown => {
                            app.scroll_down(10);
                        }
                        KeyCode::Up   => app.scroll_up(1),
                        KeyCode::Down => {
                            app.scroll_down(1);
                        }

                        // Typing
                        KeyCode::Tab => {
                            autocomplete_input(&mut app);
                        }

                        KeyCode::Char(c) => {
                            app.input.push(c);
                            app.reset_completion();
                        }

                        _ => {}
                    }
                }
            }

            Some(event) = rx.recv() => {
                match event {
                    TuiEvent::AssistantMessage(reply) => app.on_reply(reply),
                    TuiEvent::Error(e) => app.status = format!("Error: {e}"),
                }
            }
        }
    }

    Ok(())
}

// ─────────────────────────────────────────────
// Rendering
// ─────────────────────────────────────────────

fn render(f: &mut Frame, app: &mut App) {
    let [msg_area, input_area, status_area] = Layout::vertical([
        Constraint::Min(3),
        Constraint::Length(3),
        Constraint::Length(1),
    ])
    .areas(f.area());

    // ── render all messages to lines ───────────────────────────────
    let inner_w = msg_area.width.saturating_sub(2) as usize;
    let all_lines: Vec<Line<'static>> = app
        .messages
        .iter()
        .flat_map(|m| message_to_lines(m, inner_w))
        .collect();

    let total_lines = all_lines.len();
    let inner_h = msg_area.height.saturating_sub(2) as usize;
    app.last_total_lines = total_lines;
    app.last_visible_h = inner_h;

    // Apply auto-scroll before computing the visible slice.
    if app.auto_scroll {
        app.snap_to_bottom(total_lines, inner_h);
    }
    // Clamp scroll in case window was resized.
    app.scroll_offset = app.scroll_offset.min(total_lines.saturating_sub(inner_h));

    let visible: Vec<Line<'static>> = all_lines
        .into_iter()
        .skip(app.scroll_offset)
        .take(inner_h)
        .collect();

    let at_bottom = app.scroll_offset + inner_h >= total_lines;
    let title = if at_bottom {
        " MAAT ".to_string()
    } else {
        format!(" MAAT  ↑ {} lines above ", app.scroll_offset)
    };

    let msgs = Paragraph::new(visible)
        .block(Block::default().borders(Borders::ALL).title(title))
        .wrap(Wrap { trim: false });
    f.render_widget(msgs, msg_area);

    // ── input ──────────────────────────────────────────────────────
    let inner_w_input = input_area.width.saturating_sub(2) as usize;
    let input_len = app.input.chars().count();
    let display_input = if input_len > inner_w_input {
        app.input
            .chars()
            .skip(input_len.saturating_sub(inner_w_input))
            .collect::<String>()
    } else {
        app.input.clone()
    };

    let input_widget = Paragraph::new(display_input.as_str()).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Message  (Enter: send) "),
    );
    f.render_widget(input_widget, input_area);

    let cursor_x = input_area.x + 1 + display_input.chars().count() as u16;
    let cursor_y = input_area.y + 1;
    f.set_cursor_position((cursor_x, cursor_y));

    // ── status bar ─────────────────────────────────────────────────
    let stats = match (&app.last_usage, app.last_latency_ms) {
        (Some(u), Some(ms)) => format!(
            " in:{} out:{} {}ms │ model: {}",
            u.input_tokens, u.output_tokens, ms, app.model_id
        ),
        _ => format!(" model: {}", app.model_id),
    };
    let route = match &app.active_session {
        Some(session) => format!(" │ route:@{}", session),
        None => String::new(),
    };
    let status_line = format!("{:<width$} │{}{}", app.status, stats, route, width = 20);
    let status = Paragraph::new(status_line)
        .style(Style::default().fg(Color::DarkGray));
    f.render_widget(status, status_area);
}

// ─────────────────────────────────────────────
// Message → styled lines
// ─────────────────────────────────────────────

fn message_to_lines(msg: &ChatMessage, width: usize) -> Vec<Line<'static>> {
    let (label, color) = match msg.role {
        Role::User      => ("You ", Color::Cyan),
        Role::Assistant => ("MAAT", Color::Green),
        Role::System    => ("sys ", Color::DarkGray),
        Role::Tool      => ("tool", Color::Yellow),
    };

    let prefix = Line::from(Span::styled(
        format!("[{label}]"),
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    ));

    let mut lines = vec![prefix];

    // Render content through the markdown parser.
    let content_lines = render_markdown(&msg.content);
    for line in content_lines {
        lines.extend(wrap_line(line, width));
    }

    // Blank separator between messages.
    lines.push(Line::from(""));

    lines
}

// ─────────────────────────────────────────────
// Markdown renderer
// ─────────────────────────────────────────────

fn render_markdown(text: &str) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut in_code_block = false;

    for raw in text.lines() {
        if raw.starts_with("```") {
            in_code_block = !in_code_block;
            if in_code_block {
                // Show language hint if present (e.g. ```rust)
                let lang = raw.trim_start_matches('`').trim();
                if !lang.is_empty() {
                    lines.push(Line::from(Span::styled(
                        format!("┌─ {lang} "),
                        Style::default().fg(Color::DarkGray),
                    )));
                } else {
                    lines.push(Line::from(Span::styled(
                        "┌──────",
                        Style::default().fg(Color::DarkGray),
                    )));
                }
            } else {
                lines.push(Line::from(Span::styled(
                    "└──────",
                    Style::default().fg(Color::DarkGray),
                )));
            }
            continue;
        }

        if in_code_block {
            lines.push(Line::from(vec![
                Span::styled("│ ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    raw.to_owned(),
                    Style::default().fg(Color::Yellow),
                ),
            ]));
            continue;
        }

        // Headings
        if let Some(rest) = raw.strip_prefix("### ") {
            lines.push(Line::from(Span::styled(
                rest.to_owned(),
                Style::default().add_modifier(Modifier::BOLD),
            )));
            continue;
        }
        if let Some(rest) = raw.strip_prefix("## ") {
            lines.push(Line::from(Span::styled(
                rest.to_owned(),
                Style::default().add_modifier(Modifier::BOLD),
            )));
            continue;
        }
        if let Some(rest) = raw.strip_prefix("# ") {
            lines.push(Line::from(Span::styled(
                rest.to_owned(),
                Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
            )));
            continue;
        }

        // Inline formatting
        lines.push(parse_inline(raw));
    }

    lines
}

// ─────────────────────────────────────────────
// Command parsing
// ─────────────────────────────────────────────

enum LocalInput {
    Handled(String),
    Send(HeraldPayload),
}

/// Parse raw user input into a local action or a HeraldPayload.
fn handle_local_input(app: &mut App, text: String) -> LocalInput {
    let trimmed = text.trim();

    if let Some(rest) = trimmed.strip_prefix("/session use ") {
        let name = rest.trim();
        if !name.is_empty() {
            app.active_session = Some(name.to_string());
            return LocalInput::Handled(format!("Active session set to @{}.", name));
        }
    }

    if trimmed == "/session leave" {
        app.active_session = None;
        return LocalInput::Handled("Cleared active session target.".into());
    }

    LocalInput::Send(parse_input(text, app.active_session.as_deref()))
}

/// Parse raw user input into a HeraldPayload, recognising `@name:` and `/session` syntax.
fn parse_input(text: String, active_session: Option<&str>) -> HeraldPayload {
    let t = text.trim();

    // @name: message  →  route to named session
    if let Some(rest) = t.strip_prefix('@') {
        if let Some(colon) = rest.find(':') {
            let name = rest[..colon].trim();
            let msg  = rest[colon + 1..].trim();
            if !name.is_empty() && !msg.is_empty() {
                return HeraldPayload::Command(ParsedCommand::RouteToSession {
                    name: SessionName(name.to_string()),
                    message: msg.to_string(),
                });
            }
        }
    }

    // /session new <name>: <description>
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

    // /session list
    if t == "/session list" || t == "/sessions" {
        return HeraldPayload::Command(ParsedCommand::SessionList);
    }

    // /session end <name>
    if let Some(rest) = t.strip_prefix("/session end ") {
        let name = rest.trim();
        if !name.is_empty() {
            return HeraldPayload::Command(ParsedCommand::SessionEnd {
                name: SessionName(name.to_string()),
            });
        }
    }

    // /status
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

    // /tools  or  /talents
    if t == "/tools" || t == "/talents" {
        return HeraldPayload::Command(ParsedCommand::ToolsList);
    }

    // /skills
    if t == "/skills" {
        return HeraldPayload::Command(ParsedCommand::SkillsList);
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

    // /config
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

    // /secret
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

    // /auth google
    if t == "/auth google" {
        return HeraldPayload::Command(ParsedCommand::AuthGoogle);
    }

    if let Some(session) = active_session {
        if !t.starts_with('/') {
            return HeraldPayload::Command(ParsedCommand::RouteToSession {
                name: SessionName(session.to_string()),
                message: text,
            });
        }
    }

    HeraldPayload::Text(text)
}

fn autocomplete_input(app: &mut App) {
    if !app.input.starts_with('/') {
        return;
    }

    let prefix = app.input.clone();
    let ctx = CommandCompletionContext {
        sessions: app.known_sessions.clone(),
        model_ids: app.known_models.clone(),
        prompt_names: app.known_prompts.clone(),
    };
    let matches = complete_command(&prefix, &ctx);

    if matches.is_empty() {
        app.status = "No command matches".into();
        return;
    }

    if app.completion_matches != matches {
        app.completion_matches = matches;
        app.completion_index = 0;
    } else if app.completion_matches.len() > 1 {
        app.completion_index = (app.completion_index + 1) % app.completion_matches.len();
    }

    let common = common_prefix(&app.completion_matches);
    if common.len() > app.input.len() {
        app.input = common;
    } else {
        app.input = app.completion_matches[app.completion_index].to_string();
    }

    if app.completion_matches.len() > 1 {
        app.status = format!("Commands: {}", app.completion_matches.join("  "));
    } else {
        app.status = format!("Command: {}", app.input);
    }
}

fn remember_command_side_effects(app: &mut App, payload: &HeraldPayload) {
    if let HeraldPayload::Command(cmd) = payload {
        match cmd {
            ParsedCommand::SessionNew { name, .. }
            | ParsedCommand::SessionEnd { name }
            | ParsedCommand::StatusSession { name }
            | ParsedCommand::Purge { session: name } => {
                remember_value(&mut app.known_sessions, &name.0);
            }
            ParsedCommand::RouteToSession { name, .. } => {
                remember_value(&mut app.known_sessions, &name.0);
            }
            ParsedCommand::ModelSwap { model_id, .. } => {
                remember_value(&mut app.known_models, model_id);
            }
            ParsedCommand::PromptShow { name } => {
                remember_value(&mut app.known_prompts, name);
            }
            _ => {}
        }
    }
}

fn remember_value(values: &mut Vec<String>, value: &str) {
    if !values.iter().any(|existing| existing == value) {
        values.push(value.to_string());
        values.sort();
    }
}

fn render_command_help() -> String {
    let mut lines = vec!["Commands".to_string()];
    for spec in command_specs() {
        lines.push(format!("  {:<30} — {}", spec.template, spec.description));
    }
    lines.push("  @<name>: <message>             — route to named session".into());
    lines.push("  Tab                            — autocomplete slash commands".into());
    lines.push("  PageUp/Down  ↑/↓              — scroll".into());
    lines.push("  Ctrl+C                         — quit".into());
    lines.join("\n")
}

fn common_prefix(items: &[String]) -> String {
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

/// Parse a single line for inline `**bold**` and `` `code` `` markers.
fn parse_inline(text: &str) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut chars = text.chars().peekable();
    let mut buf = String::new();

    while let Some(c) = chars.next() {
        match c {
            // Inline code: `...`
            '`' => {
                if !buf.is_empty() {
                    spans.push(Span::raw(buf.clone()));
                    buf.clear();
                }
                let mut code = String::new();
                for nc in chars.by_ref() {
                    if nc == '`' { break; }
                    code.push(nc);
                }
                spans.push(Span::styled(code, Style::default().fg(Color::Yellow)));
            }

            // Bold: **...**
            '*' if chars.peek() == Some(&'*') => {
                chars.next(); // consume second *
                if !buf.is_empty() {
                    spans.push(Span::raw(buf.clone()));
                    buf.clear();
                }
                let mut bold = String::new();
                loop {
                    match chars.next() {
                        Some('*') if chars.peek() == Some(&'*') => {
                            chars.next();
                            break;
                        }
                        Some(nc) => bold.push(nc),
                        None => break,
                    }
                }
                spans.push(Span::styled(
                    bold,
                    Style::default().add_modifier(Modifier::BOLD),
                ));
            }

            _ => buf.push(c),
        }
    }

    if !buf.is_empty() {
        spans.push(Span::raw(buf));
    }

    Line::from(spans)
}

fn wrap_line(line: Line<'static>, width: usize) -> Vec<Line<'static>> {
    if width == 0 {
        return vec![line];
    }

    let mut wrapped = Vec::new();
    let mut current: Vec<Span<'static>> = Vec::new();
    let mut current_width = 0usize;

    for span in line.spans {
        let content = span.content.to_string();
        let chars: Vec<char> = content.chars().collect();
        let mut idx = 0usize;

        while idx < chars.len() {
            if current_width == width {
                wrapped.push(Line::from(std::mem::take(&mut current)));
                current_width = 0;
            }

            let remaining = width.saturating_sub(current_width);
            let take = remaining.min(chars.len() - idx);
            let chunk: String = chars[idx..idx + take].iter().collect();
            current.push(Span::styled(chunk, span.style));
            current_width += take;
            idx += take;

            if current_width == width {
                wrapped.push(Line::from(std::mem::take(&mut current)));
                current_width = 0;
            }
        }
    }

    if current.is_empty() {
        wrapped.push(Line::from(""));
    } else {
        wrapped.push(Line::from(current));
    }

    wrapped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_line_splits_long_content_into_visual_lines() {
        let wrapped = wrap_line(Line::from("abcdefghij"), 4);
        let rendered: Vec<String> = wrapped
            .into_iter()
            .map(|line| line.spans.iter().map(|span| span.content.to_string()).collect())
            .collect();

        assert_eq!(rendered, vec!["abcd", "efgh", "ij"]);
    }

    #[test]
    fn auto_scroll_uses_wrapped_visual_line_count() {
        let mut app = App::new("test-model".into());
        app.messages.push(ChatMessage::assistant("abcdefghij"));
        app.auto_scroll = true;

        let all_lines: Vec<Line<'static>> = app
            .messages
            .iter()
            .flat_map(|m| message_to_lines(m, 4))
            .collect();

        let total_lines = all_lines.len();
        app.snap_to_bottom(total_lines, 3);

        assert!(app.scroll_offset > 0, "scroll should move to show wrapped tail");
    }

    #[test]
    fn active_session_routes_plain_text_to_named_session() {
        let payload = parse_input("hello".into(), Some("coding"));
        match payload {
            HeraldPayload::Command(ParsedCommand::RouteToSession { name, message }) => {
                assert_eq!(name.0, "coding");
                assert_eq!(message, "hello");
            }
            other => panic!("unexpected payload: {other:?}"),
        }
    }

    #[test]
    fn autocomplete_extends_common_command_prefix() {
        let mut app = App::new("test-model".into());
        app.input = "/sk".into();
        autocomplete_input(&mut app);
        assert_eq!(app.input, "/skills");
    }
}
