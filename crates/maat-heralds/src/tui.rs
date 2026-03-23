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

    fn scroll_down(&mut self, n: usize, total_lines: usize, visible_h: usize) {
        let max = total_lines.saturating_sub(visible_h);
        self.scroll_offset = (self.scroll_offset + n).min(max);
        if self.scroll_offset >= max {
            self.auto_scroll = true;
        }
    }

    fn snap_to_bottom(&mut self, total_lines: usize, visible_h: usize) {
        self.scroll_offset = total_lines.saturating_sub(visible_h);
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

                            if text.trim() == "/help" {
                                app.messages.push(ChatMessage {
                                    role: Role::System,
                                    tool_call_id: None,
                                    tool_calls_json: None,
                                    content: concat!(
                                        "Commands\n",
                                        "  /help                          — this message\n",
                                        "  /tools  or  /talents           — list loaded tools/talents\n",
                                        "  /config                        — show current config\n",
                                        "  /config set <key> <value>      — update a config value\n",
                                        "  /secret list                   — list known secret keys\n",
                                        "  /secret set <key> <value>      — store a secret\n",
                                        "  /secret delete <key>           — remove a secret\n",
                                        "  /auth google                   — start Google OAuth flow\n",
                                        "  /session new <name>: <desc>    — create named session\n",
                                        "  /session list                  — list sessions + summaries\n",
                                        "  /session end <name>            — end a named session\n",
                                        "  /status                        — show PHAROH status\n",
                                        "  @<name>: <message>             — route to named session\n",
                                        "  PageUp/Down  ↑/↓              — scroll\n",
                                        "  Ctrl+C                         — quit",
                                    ).into(),
                                });
                            } else {
                                let payload = parse_input(text.clone());
                                app.messages.push(ChatMessage::user(&text));
                                app.status = "Thinking…".into();
                                let _ = tx.send(payload).await;
                            }
                        }

                        // Backspace
                        KeyCode::Backspace => { app.input.pop(); }

                        // Scroll
                        KeyCode::PageUp   => app.scroll_up(10),
                        KeyCode::PageDown => {
                            // we need total_lines; compute lazily with a sentinel
                            app.scroll_down(10, usize::MAX, 0);
                        }
                        KeyCode::Up   => app.scroll_up(1),
                        KeyCode::Down => {
                            app.scroll_down(1, usize::MAX, 0);
                        }

                        // Typing
                        KeyCode::Char(c) => app.input.push(c),

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
    let display_input = if app.input.len() > inner_w_input {
        &app.input[app.input.len() - inner_w_input..]
    } else {
        app.input.as_str()
    };

    let input_widget = Paragraph::new(display_input).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Message  (Enter: send) "),
    );
    f.render_widget(input_widget, input_area);

    let cursor_x = input_area.x + 1 + display_input.len() as u16;
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
    let status_line = format!("{:<width$} │{}", app.status, stats, width = 20);
    let status = Paragraph::new(status_line)
        .style(Style::default().fg(Color::DarkGray));
    f.render_widget(status, status_area);
}

// ─────────────────────────────────────────────
// Message → styled lines
// ─────────────────────────────────────────────

fn message_to_lines(msg: &ChatMessage, _width: usize) -> Vec<Line<'static>> {
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
    lines.extend(content_lines);

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

/// Parse raw user input into a HeraldPayload, recognising `@name:` and `/session` syntax.
fn parse_input(text: String) -> HeraldPayload {
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
    if t == "/session list" {
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

    // /tools  or  /talents
    if t == "/tools" || t == "/talents" {
        return HeraldPayload::Command(ParsedCommand::ToolsList);
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

    HeraldPayload::Text(text)
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
