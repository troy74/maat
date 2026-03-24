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

use std::collections::{HashMap, VecDeque};
use std::io::stdout;
use std::path::{Path, PathBuf};

use crossterm::{
    event::{Event, EventStream, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures::StreamExt;
use crate::input::parse_input;
use maat_core::commands::{
    command_specs, common_completion_prefix, completion_suffix, suggest_commands,
    CommandCompletionContext, CommandSuggestion,
};
use maat_core::{
    BackendRequest, ChannelId, ChatMessage, ChatReply, HeraldAttachment, HeraldEvent,
    HeraldPayload, ParsedCommand, Role, SessionState, StatusEvent, StatusKind, StepState,
    TokenUsage, WorkflowState,
};
use ratatui::{
    layout::{Constraint, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
    DefaultTerminal, Frame,
};
use tokio::sync::mpsc;

#[derive(Debug, Clone, PartialEq, Eq)]
struct RecentArtifact {
    handle: String,
    mime_type: String,
    display_name: String,
}

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
    draft_attachments: Vec<HeraldAttachment>,
    draft_artifact_handles: Vec<String>,
    file_picker_active: bool,
    file_picker_query: String,
    file_picker_matches: Vec<PathBuf>,
    selected_file_match: usize,
    recent_artifacts: Vec<RecentArtifact>,
    selected_recent_artifact: usize,
    suggestions: Vec<CommandSuggestion>,
    selected_suggestion: usize,
    known_sessions: Vec<String>,
    known_models: Vec<String>,
    known_prompts: Vec<String>,
    known_automations: Vec<String>,
    known_runs: Vec<String>,
    focused_automation: Option<String>,
    verbose_status: bool,
    status_feed: VecDeque<String>,
    session_states: HashMap<String, String>,
    workflow_states: HashMap<String, String>,
    step_states: HashMap<String, String>,
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
            draft_attachments: Vec::new(),
            draft_artifact_handles: Vec::new(),
            file_picker_active: false,
            file_picker_query: String::new(),
            file_picker_matches: Vec::new(),
            selected_file_match: 0,
            recent_artifacts: Vec::new(),
            selected_recent_artifact: 0,
            suggestions: Vec::new(),
            selected_suggestion: 0,
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
            known_automations: Vec::new(),
            known_runs: Vec::new(),
            focused_automation: None,
            verbose_status: false,
            status_feed: VecDeque::new(),
            session_states: HashMap::new(),
            workflow_states: HashMap::new(),
            step_states: HashMap::new(),
        }
    }

    fn on_reply(&mut self, reply: ChatReply) {
        self.remember_recent_artifacts(&reply.content);
        self.remember_background_runs(&reply.content);
        self.messages.push(ChatMessage::assistant(&reply.content));
        self.last_usage = Some(reply.usage);
        self.last_latency_ms = Some(reply.latency_ms);
        self.status = "Ready".into();
        self.auto_scroll = true; // snap back to bottom on new message
    }

    fn on_status(&mut self, event: StatusEvent) {
        let line = format_status_event(&event);
        self.status_feed.push_front(line);
        while self.status_feed.len() > 20 {
            self.status_feed.pop_back();
        }

        match event.kind {
            StatusKind::SessionState { session_id, state } => {
                update_status_map(
                    &mut self.session_states,
                    short_id(&session_id.0.to_string()),
                    format_session_state(&state),
                    is_active_session_state(&state),
                );
            }
            StatusKind::WorkflowState { workflow_id, state } => {
                update_status_map(
                    &mut self.workflow_states,
                    short_id(&workflow_id.0.to_string()),
                    format_workflow_state(&state),
                    is_active_workflow_state(&state),
                );
            }
            StatusKind::StepState { workflow_id, step_id, state } => {
                update_status_map(
                    &mut self.step_states,
                    format!("{}:{}", short_id(&workflow_id.0.to_string()), short_id(&step_id.0.to_string())),
                    format_step_state(&state),
                    is_active_step_state(&state),
                );
            }
            StatusKind::RunCompleted { .. } => {}
            StatusKind::HeartBeat { .. } => {}
        }
    }

    fn active_status_count(&self) -> usize {
        self.session_states.len() + self.workflow_states.len() + self.step_states.len()
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
        self.suggestions.clear();
        self.selected_suggestion = 0;
    }

    fn completion_context(&self) -> CommandCompletionContext {
        CommandCompletionContext {
            sessions: self.known_sessions.clone(),
            model_ids: self.known_models.clone(),
            prompt_names: self.known_prompts.clone(),
            artifact_handles: self
                .recent_artifacts
                .iter()
                .map(|artifact| artifact.handle.clone())
                .collect(),
            automations: self.known_automations.clone(),
            runs: self.known_runs.clone(),
        }
    }

    fn refresh_suggestions(&mut self) {
        if !self.input.starts_with('/') {
            self.reset_completion();
            return;
        }

        let next = suggest_commands(&self.input, &self.completion_context());
        if next != self.suggestions {
            self.suggestions = next;
            self.selected_suggestion = 0;
        } else if self.selected_suggestion >= self.suggestions.len() {
            self.selected_suggestion = 0;
        }
    }

    fn current_suggestion(&self) -> Option<&CommandSuggestion> {
        self.suggestions.get(self.selected_suggestion)
    }

    fn open_file_picker(&mut self) {
        self.file_picker_active = true;
        self.file_picker_query.clear();
        self.refresh_file_picker();
        self.status = "File picker: type to filter, Enter to attach, Esc to close".into();
    }

    fn close_file_picker(&mut self) {
        self.file_picker_active = false;
        self.file_picker_query.clear();
        self.file_picker_matches.clear();
        self.selected_file_match = 0;
        self.status = "Ready".into();
    }

    fn refresh_file_picker(&mut self) {
        self.file_picker_matches = find_file_matches(&self.file_picker_query);
        if self.selected_file_match >= self.file_picker_matches.len() {
            self.selected_file_match = 0;
        }
    }

    fn selected_file_path(&self) -> Option<&PathBuf> {
        self.file_picker_matches.get(self.selected_file_match)
    }

    fn remember_recent_artifacts(&mut self, content: &str) {
        for artifact in parse_recent_artifacts(content) {
            self.recent_artifacts.retain(|existing| existing.handle != artifact.handle);
            self.recent_artifacts.insert(0, artifact);
        }
        if self.recent_artifacts.len() > 8 {
            self.recent_artifacts.truncate(8);
        }
        if self.selected_recent_artifact >= self.recent_artifacts.len() {
            self.selected_recent_artifact = 0;
        }
    }

    fn remember_background_runs(&mut self, content: &str) {
        for handle in parse_background_run_handles(content) {
            remember_value(&mut self.known_runs, &handle);
            remember_value(&mut self.known_sessions, &handle);
        }
    }

    fn current_recent_artifact(&self) -> Option<&RecentArtifact> {
        self.recent_artifacts.get(self.selected_recent_artifact)
    }
}

// ─────────────────────────────────────────────
// Entry point
// ─────────────────────────────────────────────

pub async fn run_tui(
    tx: mpsc::Sender<BackendRequest>,
    reply_tx: mpsc::Sender<HeraldEvent>,
    mut rx: mpsc::Receiver<HeraldEvent>,
    model_id: String,
) -> anyhow::Result<()> {
    enable_raw_mode()?;
    execute!(stdout(), EnterAlternateScreen)?;
    let mut terminal = ratatui::init();

    let result = event_loop(&mut terminal, &tx, &reply_tx, &mut rx, model_id).await;

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
    tx: &mpsc::Sender<BackendRequest>,
    reply_tx: &mpsc::Sender<HeraldEvent>,
    rx: &mut mpsc::Receiver<HeraldEvent>,
    model_id: String,
) -> anyhow::Result<()> {
    let mut app = App::new(model_id);
    let mut events = EventStream::new();

    while !app.quit {
        terminal.draw(|f| render(f, &mut app))?;

        tokio::select! {
            Some(Ok(event)) = events.next() => {
                if let Event::Key(key) = event {
                    if app.file_picker_active {
                        match key.code {
                            KeyCode::Esc => {
                                app.close_file_picker();
                            }
                            KeyCode::Enter => {
                                if let Some(path) = app.selected_file_path().cloned() {
                                    match make_attachment(&path.to_string_lossy()) {
                                        Ok(attachment) => {
                                            let label = attachment_label(&attachment);
                                            let size = format_bytes(attachment.size_bytes);
                                            app.draft_attachments.push(attachment);
                                            app.close_file_picker();
                                            app.status = format!("Attached `{label}` ({size}).");
                                        }
                                        Err(error) => {
                                            app.status = format!("Attach failed: {error}");
                                        }
                                    }
                                } else {
                                    app.status = "No file selected".into();
                                }
                            }
                            KeyCode::Backspace => {
                                app.file_picker_query.pop();
                                app.refresh_file_picker();
                            }
                            KeyCode::Up => {
                                if !app.file_picker_matches.is_empty() {
                                    app.selected_file_match = app.selected_file_match.saturating_sub(1);
                                }
                            }
                            KeyCode::Down => {
                                if !app.file_picker_matches.is_empty() {
                                    app.selected_file_match =
                                        (app.selected_file_match + 1).min(app.file_picker_matches.len().saturating_sub(1));
                                }
                            }
                            KeyCode::Tab | KeyCode::Right => {
                                if let Some(path) = app.selected_file_path() {
                                    app.file_picker_query = display_picker_path(path);
                                    app.refresh_file_picker();
                                }
                            }
                            KeyCode::Char('o') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                                app.close_file_picker();
                            }
                            KeyCode::Char(c) => {
                                app.file_picker_query.push(c);
                                app.refresh_file_picker();
                            }
                            _ => {}
                        }
                        continue;
                    }

                    match key.code {
                        // Quit
                        KeyCode::Char('c')
                            if key.modifiers.contains(KeyModifiers::CONTROL) =>
                        {
                            app.quit = true;
                        }

                        KeyCode::Char('o')
                            if key.modifiers.contains(KeyModifiers::CONTROL) =>
                        {
                            app.open_file_picker();
                        }

                        KeyCode::Char('r')
                            if key.modifiers.contains(KeyModifiers::CONTROL) =>
                        {
                            if !app.recent_artifacts.is_empty() {
                                app.selected_recent_artifact =
                                    (app.selected_recent_artifact + 1) % app.recent_artifacts.len();
                                if let Some(artifact) = app.current_recent_artifact() {
                                    app.status = format!("Recent artifact: {} — {}", artifact.handle, artifact.display_name);
                                }
                            }
                        }

                        KeyCode::Char('y')
                            if key.modifiers.contains(KeyModifiers::CONTROL) =>
                        {
                            if let Some((handle, display_name)) = app
                                .current_recent_artifact()
                                .map(|artifact| (artifact.handle.clone(), artifact.display_name.clone()))
                            {
                                if !app.input.is_empty() && !app.input.ends_with(' ') {
                                    app.input.push(' ');
                                }
                                app.input.push_str(&handle);
                                app.refresh_suggestions();
                                app.status = format!("Inserted artifact handle `{handle}` ({display_name})");
                            }
                        }

                        KeyCode::Char('t')
                            if key.modifiers.contains(KeyModifiers::CONTROL) =>
                        {
                            if let Some(handle) = app.current_recent_artifact().map(|artifact| artifact.handle.clone()) {
                                if !app.draft_artifact_handles.iter().any(|existing| existing == &handle) {
                                    app.draft_artifact_handles.push(handle.clone());
                                }
                                app.status = format!("Attached recent artifact `{handle}` to the draft");
                            }
                        }

                        // Send message
                        KeyCode::Enter if !app.input.is_empty() || !app.draft_attachments.is_empty() || !app.draft_artifact_handles.is_empty() => {
                            let text = std::mem::take(&mut app.input);
                            app.reset_completion();

                            if text.trim() == "/help" {
                                app.messages.push(ChatMessage {
                                    role: Role::System,
                                    image_inputs: vec![],
                                    tool_call_id: None,
                                    tool_calls_json: None,
                                    content: render_command_help().into(),
                                });
                            } else {
                                match handle_local_input(&mut app, text.clone()) {
                                    LocalInput::Handled(message) => {
                                        app.messages.push(ChatMessage {
                                            role: Role::System,
                                            image_inputs: vec![],
                                            tool_call_id: None,
                                            tool_calls_json: None,
                                            content: message,
                                        });
                                    }
                                    LocalInput::Send(payload) => {
                                        remember_command_side_effects(&mut app, &payload);
                                        let display_text = render_draft_message_preview(
                                            &text,
                                            &app.draft_attachments,
                                            &app.draft_artifact_handles,
                                        );
                                        app.messages.push(ChatMessage::user(&display_text));
                                        app.status = "Thinking…".into();
                                        app.draft_attachments.clear();
                                        app.draft_artifact_handles.clear();
                                        let _ = tx.send(BackendRequest {
                                            channel: ChannelId("tui".into()),
                                            payload,
                                            reply_tx: reply_tx.clone(),
                                        }).await;
                                    }
                                }
                            }
                        }

                        // Backspace
                        KeyCode::Backspace => {
                            app.input.pop();
                            app.refresh_suggestions();
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
                            accept_completion(&mut app);
                        }

                        KeyCode::Char('n')
                            if key.modifiers.contains(KeyModifiers::CONTROL) =>
                        {
                            cycle_suggestion(&mut app, 1);
                        }

                        KeyCode::Char('p')
                            if key.modifiers.contains(KeyModifiers::CONTROL) =>
                        {
                            cycle_suggestion(&mut app, -1);
                        }

                        KeyCode::Right => {
                            accept_completion(&mut app);
                        }

                        KeyCode::Char(c) => {
                            app.input.push(c);
                            app.refresh_suggestions();
                        }

                        _ => {}
                    }
                }
            }

            Some(event) = rx.recv() => {
                match event {
                    HeraldEvent::AssistantMessage(reply) => app.on_reply(reply),
                    HeraldEvent::Status(event) => app.on_status(event),
                    HeraldEvent::Error(e) => app.status = format!("Error: {e}"),
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
    let status_detail_height = if app.verbose_status { 5 } else { 1 };
    let [msg_area, runtime_area, picker_area, recent_area, attachments_area, suggestion_area, input_area, status_area] = Layout::vertical([
        Constraint::Min(3),
        Constraint::Length(status_detail_height),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
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

    let runtime_lines: Vec<Line<'static>> = if app.verbose_status {
        if app.status_feed.is_empty() {
            vec![Line::from(Span::styled(
                "No live runtime status yet.",
                Style::default().fg(Color::DarkGray),
            ))]
        } else {
            app.status_feed
                .iter()
                .take(4)
                .map(|line| Line::from(Span::styled(line.clone(), Style::default().fg(Color::LightBlue))))
                .collect()
        }
    } else {
        let summary = if app.active_status_count() == 0 {
            "Runtime idle. /verbose to expand status details.".to_string()
        } else {
            format!(
                "{} active runtime items. /verbose to expand status details.",
                app.active_status_count()
            )
        };
        vec![Line::from(Span::styled(summary, Style::default().fg(Color::DarkGray)))]
    };
    let runtime_widget = Paragraph::new(runtime_lines)
        .block(Block::default().borders(Borders::ALL).title(" Runtime "));
    f.render_widget(runtime_widget, runtime_area);

    // ── input ──────────────────────────────────────────────────────
    let inner_w_input = input_area.width.saturating_sub(2) as usize;
    let input_text = if app.file_picker_active {
        app.file_picker_query.clone()
    } else {
        app.input.clone()
    };
    let input_title = if app.file_picker_active {
        " File Picker  (Enter: attach, Esc: close) "
    } else {
        " Message  (Enter: send) "
    };
    let input_len = input_text.chars().count();
    let display_input = if input_len > inner_w_input {
        input_text
            .chars()
            .skip(input_len.saturating_sub(inner_w_input))
            .collect::<String>()
    } else {
        input_text.clone()
    };

    let inline_suffix = if app.file_picker_active {
        app.selected_file_path()
            .map(|path| display_picker_path(path))
            .and_then(|path| completion_suffix(&app.file_picker_query, &path))
            .unwrap_or_default()
    } else {
        app.current_suggestion()
            .and_then(|item| completion_suffix(&app.input, &item.replacement))
            .unwrap_or_default()
    };
    let visible_suffix = if input_len <= inner_w_input {
        let room = inner_w_input.saturating_sub(display_input.chars().count());
        inline_suffix.chars().take(room).collect::<String>()
    } else {
        String::new()
    };

    let input_widget = Paragraph::new(Line::from(vec![
        Span::raw(display_input.clone()),
        Span::styled(visible_suffix, Style::default().fg(Color::DarkGray)),
    ]))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(input_title),
    );
    f.render_widget(input_widget, input_area);

    let cursor_x = input_area.x + 1 + display_input.chars().count() as u16;
    let cursor_y = input_area.y + 1;
    f.set_cursor_position((cursor_x, cursor_y));

    // ── file picker strip ──────────────────────────────────────────
    let picker_line = if !app.file_picker_active {
        Line::from(Span::styled(
            "Ctrl+O opens the file picker.",
            Style::default().fg(Color::DarkGray),
        ))
    } else if app.file_picker_matches.is_empty() {
        Line::from(Span::styled(
            "No matching files in the workspace.",
            Style::default().fg(Color::DarkGray),
        ))
    } else {
        let spans = app
            .file_picker_matches
            .iter()
            .take(3)
            .enumerate()
            .flat_map(|(idx, path)| {
                let mut spans = Vec::new();
                if idx > 0 {
                    spans.push(Span::raw("  "));
                }
                let style = if idx == app.selected_file_match {
                    Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::Gray)
                };
                spans.push(Span::styled(display_picker_path(path), style));
                spans
            })
            .collect::<Vec<_>>();
        Line::from(spans)
    };
    f.render_widget(Paragraph::new(picker_line), picker_area);

    // ── recent artifacts strip ────────────────────────────────────
    let recent_line = if app.recent_artifacts.is_empty() {
        Line::from(Span::styled(
            "No recent artifact handles yet.",
            Style::default().fg(Color::DarkGray),
        ))
    } else {
        let spans = app
            .recent_artifacts
            .iter()
            .take(3)
            .enumerate()
            .flat_map(|(idx, artifact)| {
                let mut spans = Vec::new();
                if idx > 0 {
                    spans.push(Span::raw("  "));
                }
                let style = if idx == app.selected_recent_artifact {
                    Style::default().fg(Color::LightGreen).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::Green)
                };
                spans.push(Span::styled(artifact.handle.clone(), style));
                spans.push(Span::styled(
                    format!(" — {}", artifact.display_name),
                    Style::default().fg(Color::DarkGray),
                ));
                spans
            })
            .collect::<Vec<_>>();
        Line::from(spans)
    };
    f.render_widget(Paragraph::new(recent_line), recent_area);

    // ── attachment strip ───────────────────────────────────────────
    let attachment_line = if app.draft_attachments.is_empty() && app.draft_artifact_handles.is_empty() {
        Line::from(Span::styled(
            "No draft attachments. Add one with /attach <path>.",
            Style::default().fg(Color::DarkGray),
        ))
    } else {
        let mut items = app
            .draft_attachments
            .iter()
            .map(|attachment| {
                vec![
                    Span::styled(
                        attachment_label(attachment),
                        Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!(" ({})", format_bytes(attachment.size_bytes)),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]
            })
            .collect::<Vec<_>>();
        items.extend(app.draft_artifact_handles.iter().map(|handle| {
            vec![
                Span::styled(
                    format!("artifact:{handle}"),
                    Style::default().fg(Color::LightGreen).add_modifier(Modifier::BOLD),
                ),
            ]
        }));
        let spans = items
            .into_iter()
            .take(3)
            .enumerate()
            .flat_map(|(idx, spans)| {
                let mut item = Vec::new();
                if idx > 0 {
                    item.push(Span::raw("  "));
                }
                item.extend(spans);
                item
            })
            .collect::<Vec<_>>();
        Line::from(spans)
    };
    f.render_widget(Paragraph::new(attachment_line), attachments_area);

    // ── suggestion strip ───────────────────────────────────────────
    let suggestion_line = if app.suggestions.is_empty() {
        Line::from(Span::styled(
            "Type / for commands. Tab or → accepts the current suggestion.",
            Style::default().fg(Color::DarkGray),
        ))
    } else {
        let spans = app
            .suggestions
            .iter()
            .take(3)
            .enumerate()
            .flat_map(|(idx, item)| {
                let mut spans = Vec::new();
                if idx > 0 {
                    spans.push(Span::raw("  "));
                }
                let style = if idx == app.selected_suggestion {
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::Gray)
                };
                spans.push(Span::styled(item.replacement.clone(), style));
                if !item.detail.is_empty() {
                    spans.push(Span::styled(
                        format!(" — {}", item.detail),
                        Style::default().fg(Color::DarkGray),
                    ));
                }
                spans
            })
            .collect::<Vec<_>>();
        Line::from(spans)
    };
    f.render_widget(Paragraph::new(suggestion_line), suggestion_area);

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
    let completion = if app.suggestions.is_empty() {
        String::new()
    } else {
        " │ Tab/→ accept  Ctrl+N/P cycle".to_string()
    };
    let picker = if app.file_picker_active {
        " │ picker: Enter attach Esc close".to_string()
    } else {
        " │ Ctrl+O picker".to_string()
    };
    let recent = if app.recent_artifacts.is_empty() {
        String::new()
    } else {
        " │ Ctrl+R cycle artifact  Ctrl+Y insert  Ctrl+T attach".to_string()
    };
    let runtime = if app.active_status_count() == 0 {
        " │ runtime:idle".to_string()
    } else {
        format!(" │ runtime:{}", app.active_status_count())
    };
    let verbose = format!(
        " │ verbose:{}",
        if app.verbose_status { "on" } else { "off" }
    );
    let attachment_count = app.draft_attachments.len() + app.draft_artifact_handles.len();
    let attachments = if attachment_count == 0 {
        String::new()
    } else {
        format!(" │ attachments:{attachment_count}")
    };
    let status_line = format!(
        "{:<width$} │{}{}{}{}{}{}{}{}",
        app.status,
        stats,
        route,
        picker,
        recent,
        runtime,
        verbose,
        attachments,
        completion,
        width = 20
    );
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

    if trimmed == "/attach clear" {
        let cleared = app.draft_attachments.len() + app.draft_artifact_handles.len();
        app.draft_attachments.clear();
        app.draft_artifact_handles.clear();
        return LocalInput::Handled(format!("Cleared {cleared} draft attachment(s)."));
    }

    if let Some(rest) = trimmed.strip_prefix("/detach ") {
        let target = rest.trim();
        if target.is_empty() {
            return LocalInput::Handled("Usage: /detach <index|name>".into());
        }
        if let Some(attachment) = remove_draft_attachment(&mut app.draft_attachments, target) {
            return LocalInput::Handled(format!(
                "Removed `{}` from the draft.",
                attachment_label(&attachment),
            ));
        }
        if let Some(handle) = remove_draft_artifact_handle(&mut app.draft_artifact_handles, target) {
            return LocalInput::Handled(format!(
                "Removed artifact handle `{handle}` from the draft.",
            ));
        }
        return LocalInput::Handled(format!(
            "No draft attachment matched `{target}`.",
        ));
    }

    if let Some(rest) = trimmed.strip_prefix("/attach ") {
        let path = rest.trim();
        if path.is_empty() {
            return LocalInput::Handled("Usage: /attach <path>".into());
        }
        match make_attachment(path) {
            Ok(attachment) => {
                app.draft_attachments.push(attachment.clone());
                return LocalInput::Handled(format!(
                    "Attached `{}` ({}).",
                    attachment_label(&attachment),
                    format_bytes(attachment.size_bytes),
                ));
            }
            Err(error) => {
                return LocalInput::Handled(format!("Attach failed: {error}"));
            }
        }
    }

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

    if trimmed == "/verbose" || trimmed == "/verbose toggle" {
        app.verbose_status = !app.verbose_status;
        return LocalInput::Handled(format!(
            "Verbose runtime status {}.",
            if app.verbose_status { "enabled" } else { "disabled" }
        ));
    }
    if trimmed == "/verbose on" {
        app.verbose_status = true;
        return LocalInput::Handled("Verbose runtime status enabled.".into());
    }
    if trimmed == "/verbose off" {
        app.verbose_status = false;
        return LocalInput::Handled("Verbose runtime status disabled.".into());
    }

    if let Some(rest) = trimmed.strip_prefix("/run open ") {
        let handle = rest.trim();
        if !handle.is_empty() {
            app.active_session = Some(handle.to_string());
            remember_value(&mut app.known_sessions, handle);
            remember_value(&mut app.known_runs, handle);
            return LocalInput::Handled(format!("Active session set to @{} from background run.", handle));
        }
    }

    if let Some(name) = app
        .focused_automation
        .clone()
        .filter(|_| looks_like_run_focused_automation(trimmed))
    {
        return LocalInput::Send(HeraldPayload::Command(ParsedCommand::AutomationRun { name }));
    }

    LocalInput::Send(parse_input(
        text,
        app.active_session.as_deref(),
        app.draft_attachments.clone(),
        app.draft_artifact_handles.clone(),
        app.focused_automation.clone(),
    ))
}

fn accept_completion(app: &mut App) {
    if !app.input.starts_with('/') {
        return;
    }

    app.refresh_suggestions();
    if app.suggestions.is_empty() {
        app.status = "No command suggestions".into();
        return;
    }

    let replacements = app
        .suggestions
        .iter()
        .map(|item| item.replacement.clone())
        .collect::<Vec<_>>();

    let common = common_completion_prefix(&replacements);
    if common.len() > app.input.len() {
        app.input = common;
    } else if let Some(suggestion) = app.current_suggestion() {
        app.input = suggestion.replacement.clone();
    }

    app.refresh_suggestions();
    app.status = format!("Command: {}", app.input);
}

fn cycle_suggestion(app: &mut App, direction: isize) {
    app.refresh_suggestions();
    if app.suggestions.len() <= 1 {
        return;
    }

    let len = app.suggestions.len() as isize;
    let next = (app.selected_suggestion as isize + direction).rem_euclid(len) as usize;
    app.selected_suggestion = next;

    if let Some(suggestion) = app.current_suggestion() {
        app.status = format!("Suggestion: {} — {}", suggestion.replacement, suggestion.detail);
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
            ParsedCommand::AutomationShow { name }
            | ParsedCommand::AutomationRun { name }
            | ParsedCommand::AutomationPause { name }
            | ParsedCommand::AutomationResume { name }
            | ParsedCommand::AutomationDelete { name } => {
                remember_value(&mut app.known_automations, name);
                app.focused_automation = Some(name.clone());
            }
            ParsedCommand::AutomationCreate { name, .. }
            | ParsedCommand::AutomationEdit { name, .. } => {
                remember_value(&mut app.known_automations, name);
                app.focused_automation = Some(name.clone());
            }
            ParsedCommand::RunShow { handle } | ParsedCommand::RunOpen { handle } => {
                remember_value(&mut app.known_runs, handle);
                remember_value(&mut app.known_sessions, handle);
            }
            ParsedCommand::RunCancel { handle } => {
                remember_value(&mut app.known_runs, handle);
                remember_value(&mut app.known_sessions, handle);
            }
            ParsedCommand::RunStart { title, .. } => {
                remember_value(&mut app.known_runs, title);
            }
            _ => {}
        }
    }
}

fn looks_like_run_focused_automation(text: &str) -> bool {
    let lower = text.trim().to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "run it"
            | "run it."
            | "ok run it"
            | "ok run it."
            | "run that"
            | "run that."
            | "run the automation"
            | "run the automation."
            | "run it, lets see"
            | "run it, let's see"
            | "okay run it"
            | "okay run it."
            | "test it"
            | "test it."
    )
}

fn remember_value(values: &mut Vec<String>, value: &str) {
    if !values.iter().any(|existing| existing == value) {
        values.push(value.to_string());
        values.sort();
    }
}

fn make_attachment(path: &str) -> anyhow::Result<HeraldAttachment> {
    let input = PathBuf::from(path);
    let canonical = std::fs::canonicalize(&input)?;
    let meta = std::fs::metadata(&canonical)?;
    if !meta.is_file() {
        anyhow::bail!("not a file: {}", canonical.display());
    }

    Ok(HeraldAttachment {
        mime_type: detect_content_type(&canonical).to_string(),
        size_bytes: meta.len(),
        pointer: canonical.to_string_lossy().to_string(),
    })
}

fn display_picker_path(path: &Path) -> String {
    std::env::current_dir()
        .ok()
        .and_then(|cwd| path.strip_prefix(&cwd).ok().map(|relative| relative.display().to_string()))
        .unwrap_or_else(|| path.display().to_string())
}

fn find_file_matches(query: &str) -> Vec<PathBuf> {
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(_) => return Vec::new(),
    };

    let query = query.trim();
    if query.is_empty() {
        let mut seed = Vec::new();
        collect_files(&cwd, &mut seed, 50);
        return seed;
    }

    let lower = query.to_ascii_lowercase();
    let mut matches = Vec::new();
    collect_matching_files(&cwd, &lower, &mut matches, 50);
    matches
}

fn collect_files(root: &Path, out: &mut Vec<PathBuf>, limit: usize) {
    if out.len() >= limit {
        return;
    }
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        if out.len() >= limit {
            break;
        }
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if should_skip_path(&name) {
            continue;
        }
        if path.is_file() {
            out.push(path);
        } else if path.is_dir() {
            collect_files(&path, out, limit);
        }
    }
}

fn collect_matching_files(root: &Path, query: &str, out: &mut Vec<PathBuf>, limit: usize) {
    if out.len() >= limit {
        return;
    }
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        if out.len() >= limit {
            break;
        }
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if should_skip_path(&name) {
            continue;
        }
        if path.is_file() {
            let display = display_picker_path(&path).to_ascii_lowercase();
            if display.contains(query) {
                out.push(path);
            }
        } else if path.is_dir() {
            collect_matching_files(&path, query, out, limit);
        }
    }
}

fn should_skip_path(name: &str) -> bool {
    matches!(name, ".git" | "target" | "node_modules")
}

fn detect_content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|ext| ext.to_str()).unwrap_or_default().to_ascii_lowercase().as_str() {
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "md" => "text/markdown",
        "txt" => "text/plain",
        "json" => "application/json",
        _ => "application/octet-stream",
    }
}

fn attachment_label(attachment: &HeraldAttachment) -> String {
    Path::new(&attachment.pointer)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(&attachment.pointer)
        .to_string()
}

fn render_draft_message_preview(
    text: &str,
    attachments: &[HeraldAttachment],
    artifact_handles: &[String],
) -> String {
    if attachments.is_empty() && artifact_handles.is_empty() {
        return text.to_string();
    }

    let mut labels = attachments
        .iter()
        .map(attachment_label)
        .collect::<Vec<_>>();
    labels.extend(artifact_handles.iter().map(|handle| format!("artifact:{handle}")));
    let labels = labels.join(", ");

    if text.trim().is_empty() {
        format!("[Attached items]\nAttachments: {labels}")
    } else {
        format!("{text}\n\nAttachments: {labels}")
    }
}

fn parse_recent_artifacts(content: &str) -> Vec<RecentArtifact> {
    content
        .lines()
        .filter_map(|line| parse_artifact_line(line.trim_start()))
        .collect()
}

fn parse_artifact_line(line: &str) -> Option<RecentArtifact> {
    let rest = line.strip_prefix("- ")?;
    let mut parts = rest.split_whitespace();
    let handle = parts.next()?.to_string();
    let mime_type = parts.next()?.to_string();
    let display_name = parts.collect::<Vec<_>>().join(" ");
    if display_name.is_empty() {
        return None;
    }
    Some(RecentArtifact {
        handle,
        mime_type,
        display_name,
    })
}

fn parse_background_run_handles(content: &str) -> Vec<String> {
    content
        .lines()
        .filter_map(|line| {
            let start = line.find("background run `")?;
            let rest = &line[start + "background run `".len()..];
            let end = rest.find('`')?;
            Some(rest[..end].to_string())
        })
        .collect()
}

fn format_status_event(event: &StatusEvent) -> String {
    match &event.kind {
        StatusKind::SessionState { session_id, state } => {
            format!("session {} {}", short_id(&session_id.0.to_string()), format_session_state(state))
        }
        StatusKind::WorkflowState { workflow_id, state } => {
            format!("workflow {} {}", short_id(&workflow_id.0.to_string()), format_workflow_state(state))
        }
        StatusKind::StepState { workflow_id, step_id, state } => {
            format!(
                "step {}:{} {}",
                short_id(&workflow_id.0.to_string()),
                short_id(&step_id.0.to_string()),
                format_step_state(state)
            )
        }
        StatusKind::RunCompleted { handle, status, summary, .. } => {
            format!("run {} {:?} {}", handle, status, truncate_inline(summary, 60))
        }
        StatusKind::HeartBeat { session_id } => {
            format!("heartbeat {}", short_id(&session_id.0.to_string()))
        }
    }
}

fn short_id(value: &str) -> String {
    value.chars().take(6).collect()
}

fn format_session_state(state: &SessionState) -> String {
    match state {
        SessionState::Idle => "idle".into(),
        SessionState::Running { .. } => "running".into(),
        SessionState::Blocked { .. } => "blocked".into(),
        SessionState::AwaitingUser => "awaiting-user".into(),
        SessionState::Completed => "completed".into(),
        SessionState::Failed { error } => format!("failed: {}", truncate_inline(error, 40)),
        SessionState::Cancelled => "cancelled".into(),
    }
}

fn format_workflow_state(state: &WorkflowState) -> String {
    match state {
        WorkflowState::Pending => "pending".into(),
        WorkflowState::Running { completed, total } => format!("running {completed}/{total}"),
        WorkflowState::Paused => "paused".into(),
        WorkflowState::Completed => "completed".into(),
        WorkflowState::Failed { error } => format!("failed: {}", truncate_inline(error, 40)),
        WorkflowState::Cancelled => "cancelled".into(),
    }
}

fn format_step_state(state: &StepState) -> String {
    match state {
        StepState::Pending => "pending".into(),
        StepState::Running => "running".into(),
        StepState::Completed => "completed".into(),
        StepState::Failed { error, .. } => format!("failed: {}", truncate_inline(error, 40)),
        StepState::Retrying { attempt } => format!("retrying attempt {attempt}"),
        StepState::Cancelled => "cancelled".into(),
    }
}

fn truncate_inline(text: &str, max: usize) -> String {
    let value: String = text.chars().take(max).collect();
    if text.chars().count() > max {
        format!("{value}...")
    } else {
        value
    }
}

fn is_active_session_state(state: &SessionState) -> bool {
    matches!(state, SessionState::Running { .. } | SessionState::Blocked { .. } | SessionState::AwaitingUser)
}

fn is_active_workflow_state(state: &WorkflowState) -> bool {
    matches!(state, WorkflowState::Pending | WorkflowState::Running { .. } | WorkflowState::Paused)
}

fn is_active_step_state(state: &StepState) -> bool {
    matches!(state, StepState::Pending | StepState::Running | StepState::Retrying { .. })
}

fn update_status_map(
    map: &mut HashMap<String, String>,
    key: String,
    value: String,
    active: bool,
) {
    if active {
        map.insert(key, value);
    } else {
        map.remove(&key);
    }
}

fn remove_draft_attachment(
    attachments: &mut Vec<HeraldAttachment>,
    target: &str,
) -> Option<HeraldAttachment> {
    if let Ok(index) = target.parse::<usize>() {
        if index == 0 {
            return None;
        }
        let idx = index - 1;
        if idx < attachments.len() {
            return Some(attachments.remove(idx));
        }
    }

    let target_lower = target.to_ascii_lowercase();
    let idx = attachments.iter().position(|attachment| {
        attachment_label(attachment)
            .to_ascii_lowercase()
            .contains(&target_lower)
    })?;
    Some(attachments.remove(idx))
}

fn remove_draft_artifact_handle(
    handles: &mut Vec<String>,
    target: &str,
) -> Option<String> {
    if let Ok(index) = target.parse::<usize>() {
        if index == 0 {
            return None;
        }
        let idx = index - 1;
        if idx < handles.len() {
            return Some(handles.remove(idx));
        }
    }

    let target_lower = target.to_ascii_lowercase();
    let idx = handles
        .iter()
        .position(|handle| handle.to_ascii_lowercase().contains(&target_lower))?;
    Some(handles.remove(idx))
}

fn format_bytes(size_bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * 1024;
    if size_bytes >= MB {
        format!("{:.1}MB", size_bytes as f64 / MB as f64)
    } else if size_bytes >= KB {
        format!("{:.1}KB", size_bytes as f64 / KB as f64)
    } else {
        format!("{size_bytes}B")
    }
}

fn render_command_help() -> String {
    let mut lines = vec!["Commands".to_string()];
    for spec in command_specs() {
        lines.push(format!("  {:<30} — {}", spec.template, spec.description));
    }
    lines.push("  /attach <path>                 — add a file to the draft message".into());
    lines.push("  /attach clear                  — clear draft attachments".into());
    lines.push("  /detach <index|name>           — remove one draft attachment".into());
    lines.push("  /automations                   — list automations".into());
    lines.push("  /automation show <name>        — show one automation".into());
    lines.push("  /automation run <name>         — run an automation now".into());
    lines.push("  /automation pause <name>       — pause an automation".into());
    lines.push("  /automation resume <name>      — resume an automation".into());
    lines.push("  /automation create <name> | <schedule> | <prompt>".into());
    lines.push("  /automation edit <name> | <schedule> | <prompt>".into());
    lines.push("  /automation delete <name>      — delete an automation".into());
    lines.push("  /runs                          — list background runs".into());
    lines.push("  /run show <handle>             — inspect a background run".into());
    lines.push("  /run open <handle>             — focus the run session locally".into());
    lines.push("  /run cancel <handle>           — request cancellation of a run".into());
    lines.push("  /run start <title>: <prompt>   — start a background run".into());
    lines.push("  /verbose [on|off]              — toggle expanded runtime status lane".into());
    lines.push("  @<name>: <message>             — route to named session".into());
    lines.push("  Tab / Right                    — accept current completion".into());
    lines.push("  Ctrl+N / Ctrl+P                — cycle completion suggestions".into());
    lines.push("  Ctrl+R / Ctrl+Y                — cycle or insert recent artifact handle".into());
    lines.push("  Ctrl+T                         — attach current recent artifact to draft".into());
    lines.push("  PageUp/Down  ↑/↓              — scroll".into());
    lines.push("  Ctrl+C                         — quit".into());
    lines.join("\n")
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
        let payload = parse_input("hello".into(), Some("coding"), Vec::new(), Vec::new(), None);
        match payload {
            HeraldPayload::Message { text, session, attachments, artifact_handles } => {
                assert_eq!(text, "hello");
                assert_eq!(session.unwrap().0, "coding");
                assert!(attachments.is_empty());
                assert!(artifact_handles.is_empty());
            }
            other => panic!("unexpected payload: {other:?}"),
        }
    }

    #[test]
    fn active_session_routes_plain_text_to_named_session_without_attachments_command_removed() {
        let payload = parse_input("@coding: hello".into(), None, Vec::new(), Vec::new(), None);
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
        accept_completion(&mut app);
        assert_eq!(app.input, "/skills");
    }

    #[test]
    fn refreshes_live_suggestions_for_session_targets() {
        let mut app = App::new("test-model".into());
        app.known_sessions = vec!["coding".into()];
        app.input = "/session use c".into();
        app.refresh_suggestions();
        assert_eq!(app.current_suggestion().unwrap().replacement, "/session use coding");
    }

    #[test]
    fn attachments_promote_text_turn_to_message_payload() {
        let payload = parse_input(
            "please edit this".into(),
            None,
            vec![HeraldAttachment {
                mime_type: "image/png".into(),
                size_bytes: 12,
                pointer: "/tmp/test.png".into(),
            }],
            Vec::new(),
            None,
        );
        match payload {
            HeraldPayload::Message { text, attachments, session, artifact_handles } => {
                assert_eq!(text, "please edit this");
                assert!(session.is_none());
                assert_eq!(attachments.len(), 1);
                assert!(artifact_handles.is_empty());
            }
            other => panic!("unexpected payload: {other:?}"),
        }
    }

    #[test]
    fn display_picker_path_prefers_relative_workspace_paths() {
        let cwd = std::env::current_dir().unwrap();
        let path = cwd.join("Cargo.toml");
        let shown = display_picker_path(&path);
        assert_eq!(shown, "Cargo.toml");
    }

    #[test]
    fn find_file_matches_returns_workspace_hits() {
        let matches = find_file_matches("cargo.toml");
        assert!(matches.iter().any(|path| display_picker_path(path).ends_with("Cargo.toml")));
    }

    #[test]
    fn remove_draft_attachment_supports_one_based_index() {
        let mut attachments = vec![
            HeraldAttachment {
                mime_type: "image/png".into(),
                size_bytes: 12,
                pointer: "/tmp/one.png".into(),
            },
            HeraldAttachment {
                mime_type: "application/pdf".into(),
                size_bytes: 20,
                pointer: "/tmp/two.pdf".into(),
            },
        ];

        let removed = remove_draft_attachment(&mut attachments, "2").unwrap();
        assert_eq!(attachment_label(&removed), "two.pdf");
        assert_eq!(attachments.len(), 1);
    }

    #[test]
    fn render_draft_message_preview_includes_attachment_labels() {
        let preview = render_draft_message_preview(
            "please review this",
            &[HeraldAttachment {
                mime_type: "image/png".into(),
                size_bytes: 12,
                pointer: "/tmp/test-image.png".into(),
            }],
            &[],
        );
        assert!(preview.contains("please review this"));
        assert!(preview.contains("Attachments: test-image.png"));
    }

    #[test]
    fn parse_input_promotes_artifact_handles_into_message_payload() {
        let payload = parse_input(
            "".into(),
            None,
            Vec::new(),
            vec!["bright-canvas-a1b2".into()],
            None,
        );
        match payload {
            HeraldPayload::Message { text, attachments, artifact_handles, session } => {
                assert_eq!(text, "Please inspect the attached item(s) and help with them.");
                assert!(attachments.is_empty());
                assert_eq!(artifact_handles, vec!["bright-canvas-a1b2"]);
                assert!(session.is_none());
            }
            other => panic!("unexpected payload: {other:?}"),
        }
    }

    #[test]
    fn parses_recent_artifacts_from_assistant_reply() {
        let artifacts = parse_recent_artifacts(
            "Attached artifacts:\n  - bright-canvas-a1b2  image/png  clown-poster.png\n\nDone.",
        );
        assert_eq!(artifacts.len(), 1);
        assert_eq!(artifacts[0].handle, "bright-canvas-a1b2");
        assert_eq!(artifacts[0].display_name, "clown-poster.png");
    }

    #[test]
    fn app_reply_updates_recent_artifacts() {
        let mut app = App::new("test-model".into());
        app.on_reply(ChatReply {
            content: "Generated artifacts:\n  - joy-poster-z9x8  image/png  joy.png".into(),
            usage: TokenUsage::default(),
            latency_ms: 0,
        });
        assert_eq!(app.recent_artifacts.len(), 1);
        assert_eq!(app.recent_artifacts[0].handle, "joy-poster-z9x8");
    }

    #[test]
    fn automation_run_without_name_uses_focused_automation() {
        let payload = parse_input(
            "/automation run".into(),
            None,
            Vec::new(),
            Vec::new(),
            Some("daily-summary".into()),
        );
        match payload {
            HeraldPayload::Command(ParsedCommand::AutomationRun { name }) => {
                assert_eq!(name, "daily-summary");
            }
            other => panic!("unexpected payload: {other:?}"),
        }
    }

    #[test]
    fn local_input_run_it_uses_focused_automation() {
        let mut app = App::new("test-model".into());
        app.focused_automation = Some("daily-summary".into());
        match handle_local_input(&mut app, "OK run it".into()) {
            LocalInput::Send(HeraldPayload::Command(ParsedCommand::AutomationRun { name })) => {
                assert_eq!(name, "daily-summary");
            }
            _ => panic!("unexpected local input result"),
        }
    }
}
