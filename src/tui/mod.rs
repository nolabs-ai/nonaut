use std::{
    collections::HashMap,
    io,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    sync::{Arc, Mutex, PoisonError},
    time::{Duration, Instant},
};

use crossterm::event::{
    Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use crossterm::{Command, clipboard::CopyToClipboard};
use futures_util::StreamExt;
use nono::undo::ChangeType;
use ratatui::{
    DefaultTerminal, Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph, Widget, Wrap},
};
use ratatui_textarea::{CursorMove, TextArea};
use tokio::{
    sync::mpsc,
    task::JoinHandle,
    time::{self, MissedTickBehavior},
};
use tui_markdown::{Options as MarkdownOptions, StyleSheet, from_str_with_options};

mod changes;
mod commands;
mod theme;

use commands::{SLASH_COMMANDS, SlashCommand, parse_exec};
use theme::{Theme, palette, select as select_theme, selected as theme};

use crate::{
    config::{
        load_model, load_reasoning, save_default_provider, save_model, save_reasoning, save_theme,
    },
    providers::{Model, Provider, ProviderEvent, ProviderKind, ReasoningOption},
    tools::ToolContext,
    undo::Undo,
};

pub(crate) use theme::init as init_theme;
// Driven by the event loop, not the terminal, so a frozen app stops blinking.
const CURSOR_BLINK: Duration = Duration::from_millis(530);
const BRAND_MARK: &str = "⎔";
// The prompt starts two lines tall and grows with newlines up to this many.
const PROMPT_MIN_LINES: usize = 2;
const PROMPT_MAX_LINES: usize = 10;
const PROMPT_HISTORY_LIMIT: usize = 500;
const CHANGES_REFRESH: Duration = Duration::from_secs(2);
static NEXT_MARKDOWN_JOB: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WorkspaceView {
    Agent,
    Changes,
}

#[derive(Clone, Copy)]
struct NonautMarkdown;

impl StyleSheet for NonautMarkdown {
    fn heading(&self, _level: u8) -> Style {
        Style::new().fg(palette().accent).bold()
    }

    fn code(&self) -> Style {
        Style::new().fg(palette().foreground).bg(palette().surface)
    }

    fn link(&self) -> Style {
        Style::new().fg(palette().accent).underlined()
    }

    fn blockquote(&self) -> Style {
        Style::new().fg(palette().muted).italic()
    }

    fn heading_meta(&self) -> Style {
        Style::new().fg(palette().muted)
    }

    fn metadata_block(&self) -> Style {
        Style::new().fg(palette().muted)
    }

    fn code_block_fence(&self) -> &str {
        ""
    }

    fn html(&self) -> Style {
        Style::new().fg(palette().muted)
    }

    fn table_header(&self) -> Style {
        Style::new().fg(palette().accent).bold()
    }

    fn table_cell(&self) -> Style {
        Style::new().fg(palette().foreground)
    }

    fn table_border(&self) -> Style {
        Style::new().fg(palette().border)
    }

    fn list_marker(&self) -> Style {
        Style::new().fg(palette().accent)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Role {
    You,
    Assistant,
    System,
    Running,
    Success,
    Error,
}

struct Message {
    role: Role,
    content: String,
    rendered_markdown: Option<(u16, Vec<Line<'static>>)>,
    markdown_job: Option<(u16, u64)>,
}

impl Message {
    fn new(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            rendered_markdown: None,
            markdown_job: None,
        }
    }

    fn replace(&mut self, role: Role, content: impl Into<String>) {
        self.role = role;
        self.content = content.into();
        self.rendered_markdown = None;
        self.markdown_job = None;
    }

    fn append(&mut self, content: &str) {
        self.content.push_str(content);
        self.rendered_markdown = None;
        self.markdown_job = None;
    }

    fn set_role(&mut self, role: Role) {
        self.role = role;
        self.rendered_markdown = None;
        self.markdown_job = None;
    }
}

struct MarkdownResult {
    message_index: usize,
    width: u16,
    job: u64,
    lines: Vec<Line<'static>>,
}

#[derive(Clone, Copy)]
struct ConversationSelection {
    /// Columns are relative to the conversation text area; rows are in the
    /// fully wrapped conversation document.
    anchor: (u16, u16),
    cursor: (u16, u16),
    dragging: bool,
    copy_pending: bool,
}

struct ToolSummary {
    message_index: usize,
    started: usize,
    succeeded: usize,
    failed: usize,
    last_error: String,
}

struct RunningTool {
    message_index: usize,
    title: String,
    started_at: Instant,
    timeout_seconds: Option<u64>,
    latest_line: String,
    line_count: usize,
}

struct ModelPicker {
    models: Vec<Model>,
    selected: usize,
}

struct ReasoningPicker {
    options: Vec<ReasoningOption>,
    selected: usize,
}

struct ProviderPicker {
    providers: Vec<ProviderKind>,
    selected: usize,
}

struct ThemePicker {
    selected: usize,
}

enum ConnectionEvent {
    Connected(u64, Provider, Option<String>),
    Failed(u64, String),
}

pub struct App {
    input: TextArea<'static>,
    messages: Vec<Message>,
    cwd: PathBuf,
    provider: Option<Provider>,
    active_provider: Option<ProviderKind>,
    provider_name: String,
    models: Vec<Model>,
    model_id: String,
    model_name: String,
    reasoning_effort: String,
    reasoning_options: Vec<ReasoningOption>,
    model_picker: Option<ModelPicker>,
    reasoning_picker: Option<ReasoningPicker>,
    provider_picker: Option<ProviderPicker>,
    theme_picker: Option<ThemePicker>,
    events_tx: mpsc::UnboundedSender<(u64, ProviderEvent)>,
    events_rx: mpsc::UnboundedReceiver<(u64, ProviderEvent)>,
    connection_tx: mpsc::UnboundedSender<ConnectionEvent>,
    connection_rx: mpsc::UnboundedReceiver<ConnectionEvent>,
    connection: Option<JoinHandle<()>>,
    connection_generation: u64,
    request: Option<JoinHandle<()>>,
    request_uses_provider: bool,
    audit_session: Option<crate::control_plane::AuditSession>,
    undo: Option<Arc<Mutex<Undo>>>,
    prompt_history: Vec<String>,
    /// Position while browsing history with Up/Down; None when not browsing.
    history_index: Option<usize>,
    history_draft: String,
    request_generation: u64,
    queued_request: Option<String>,
    streaming_message: Option<usize>,
    tool_summaries: HashMap<String, ToolSummary>,
    running_tools: HashMap<String, RunningTool>,
    last_tool_output: Option<(String, String)>,
    session_tokens: u64,
    conversation_scroll: u16,
    conversation_max_scroll: u16,
    follow_output: bool,
    conversation_area: Rect,
    conversation_selection: Option<ConversationSelection>,
    command_selection: usize,
    workspace_view: WorkspaceView,
    changes_diff: String,
    changes_file_count: usize,
    changes_error: Option<String>,
    changes_scroll: u16,
    changes_horizontal_scroll: u16,
    changes_max_scroll: u16,
    changes_query: String,
    changes_searching: bool,
    changes_match: Option<usize>,
    changes_refreshing: bool,
    changes_tx: mpsc::UnboundedSender<changes::Snapshot>,
    changes_rx: mpsc::UnboundedReceiver<changes::Snapshot>,
    markdown_tx: mpsc::UnboundedSender<MarkdownResult>,
    markdown_rx: mpsc::UnboundedReceiver<MarkdownResult>,
    force_redraw: bool,
    cursor_visible: bool,
    should_quit: bool,
}

impl App {
    pub fn new(
        cwd: PathBuf,
        default_provider: Option<ProviderKind>,
        startup_warning: Option<String>,
    ) -> Self {
        let mut app = Self::new_shell(cwd);
        if let Some(warning) = startup_warning {
            app.push(Role::System, warning);
        }
        if let Some(provider) = default_provider {
            app.begin_connect(provider);
        } else {
            app.push(Role::System, "Choose a default provider to get started.");
            app.open_provider_picker();
        }
        app
    }

    pub(crate) fn new_shell(cwd: PathBuf) -> Self {
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let (connection_tx, connection_rx) = mpsc::unbounded_channel();
        let (changes_tx, changes_rx) = mpsc::unbounded_channel();
        let (markdown_tx, markdown_rx) = mpsc::unbounded_channel();
        Self {
            input: Self::new_input(),
            messages: Vec::new(),
            cwd,
            provider: None,
            active_provider: None,
            provider_name: "no provider".to_owned(),
            models: Vec::new(),
            model_id: "none".to_owned(),
            model_name: "none".to_owned(),
            reasoning_effort: "none".to_owned(),
            reasoning_options: Vec::new(),
            model_picker: None,
            reasoning_picker: None,
            provider_picker: None,
            theme_picker: None,
            events_tx,
            events_rx,
            connection_tx,
            connection_rx,
            connection: None,
            connection_generation: 0,
            request: None,
            request_uses_provider: false,
            audit_session: None,
            undo: None,
            prompt_history: Vec::new(),
            history_index: None,
            history_draft: String::new(),
            request_generation: 0,
            queued_request: None,
            streaming_message: None,
            tool_summaries: HashMap::new(),
            running_tools: HashMap::new(),
            last_tool_output: None,
            session_tokens: 0,
            conversation_scroll: 0,
            conversation_max_scroll: 0,
            follow_output: true,
            conversation_area: Rect::default(),
            conversation_selection: None,
            command_selection: 0,
            workspace_view: WorkspaceView::Agent,
            changes_diff: String::new(),
            changes_file_count: 0,
            changes_error: None,
            changes_scroll: 0,
            changes_horizontal_scroll: 0,
            changes_max_scroll: 0,
            changes_query: String::new(),
            changes_searching: false,
            changes_match: None,
            changes_refreshing: false,
            changes_tx,
            changes_rx,
            markdown_tx,
            markdown_rx,
            force_redraw: false,
            cursor_visible: true,
            should_quit: false,
        }
    }

    pub async fn run(&mut self, terminal: &mut DefaultTerminal) -> io::Result<()> {
        let mut terminal_events = EventStream::new();
        let mut blink = time::interval(CURSOR_BLINK);
        blink.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut changes_refresh = time::interval(CHANGES_REFRESH);
        changes_refresh.set_missed_tick_behavior(MissedTickBehavior::Skip);
        self.begin_changes_summary();
        while !self.should_quit {
            if self.force_redraw {
                terminal.clear()?;
                self.force_redraw = false;
            }
            terminal.draw(|frame| self.render(frame))?;
            tokio::select! {
                _ = blink.tick() => {
                    self.cursor_visible = !self.cursor_visible;
                    self.refresh_running_tools();
                },
                _ = changes_refresh.tick() => {
                    if self.workspace_view == WorkspaceView::Changes {
                        self.begin_changes_refresh();
                    }
                },
                terminal_event = terminal_events.next() => match terminal_event {
                    Some(Ok(event)) => {
                        // Keep the cursor solid while the user is typing.
                        self.cursor_visible = true;
                        blink.reset();
                        self.handle_terminal_event(event);
                    }
                    Some(Err(error)) => return Err(error),
                    None => self.should_quit = true,
                },
                provider_event = self.events_rx.recv() => {
                    if let Some((request_id, event)) = provider_event {
                        self.handle_provider_event(request_id, event);
                    }
                },
                connection_event = self.connection_rx.recv() => {
                    if let Some(event) = connection_event { self.handle_connection_event(event); }
                }
                snapshot = self.changes_rx.recv() => {
                    if let Some(snapshot) = snapshot { self.apply_changes_snapshot(snapshot); }
                }
                rendered = self.markdown_rx.recv() => {
                    if let Some(rendered) = rendered { self.apply_markdown(rendered); }
                }
            }
        }
        if let Some(request) = self.request.take() {
            if let Some(provider) = self.provider.clone() {
                let _ = provider.interrupt().await;
            }
            request.abort();
        }
        if let Some(connection) = self.connection.take() {
            connection.abort();
        }
        Ok(())
    }

    fn new_input() -> TextArea<'static> {
        let mut input = TextArea::default();
        input.set_block(Self::prompt_block(0, false));
        input.set_style(
            Style::new()
                .fg(palette().foreground)
                .bg(palette().background),
        );
        input.set_cursor_line_style(Style::new().bg(palette().background));
        input.set_cursor_style(Style::new().bg(palette().accent).fg(palette().background));
        input.set_placeholder_text("Ask anything…");
        input.set_placeholder_style(Style::new().fg(palette().muted));
        input
    }

    /// Rounded prompt with the token count in the top border and the key
    /// hints in the bottom border, so no separate footer row is needed.
    fn prompt_block(tokens: u64, has_changes: bool) -> Block<'static> {
        let key = Style::new().fg(palette().foreground);
        let label = Style::new().fg(palette().muted);
        let mut hints = vec![
            Span::styled(" Enter", key),
            Span::styled(" send · ", label),
            Span::styled("Shift+Enter", key),
            Span::styled(" newline · ", label),
        ];
        if has_changes {
            hints.extend([
                Span::styled("Alt+2", key),
                Span::styled(" review · ", label),
            ]);
        }
        hints.extend([Span::styled("/q", key), Span::styled(" quit ", label)]);
        Block::new()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(palette().accent))
            .style(Style::new().bg(palette().background))
            .title_top(Line::styled(
                " › ",
                Style::new().fg(palette().accent).bold(),
            ))
            .title_top(Line::styled(format!(" {tokens} tokens "), label).right_aligned())
            .title_bottom(Line::from(hints))
            // Border plus padding puts typed text in the same column as message text.
            .padding(Padding::new(4, 1, 0, 0))
    }

    fn render(&mut self, frame: &mut Frame) {
        frame.render_widget(
            Block::new().style(Style::new().bg(palette().background)),
            frame.area(),
        );
        let prompt_lines = self
            .input
            .lines()
            .len()
            .clamp(PROMPT_MIN_LINES, PROMPT_MAX_LINES);
        let prompt_height = if self.workspace_view == WorkspaceView::Agent {
            u16::try_from(prompt_lines + 2).unwrap_or(u16::MAX)
        } else {
            2
        };
        let [header, content, footer] = Layout::vertical([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(prompt_height),
        ])
        .areas(frame.area());
        frame.render_widget(
            Block::new()
                .borders(Borders::BOTTOM)
                .border_style(palette().border)
                .style(Style::new().bg(palette().background)),
            header,
        );
        let [_, header_content, _] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(header);
        let [brand, tabs, status] = Layout::horizontal([
            Constraint::Length(18),
            Constraint::Length(40),
            Constraint::Min(1),
        ])
        .areas(header_content);
        frame.render_widget(
            Paragraph::new(Line::from(vec![Span::styled(
                format!("  {BRAND_MARK}  nonaut"),
                Style::new().fg(palette().accent).bold(),
            )]))
            .style(Style::new().bg(palette().background)),
            brand,
        );
        self.render_workspace_tabs(frame, tabs);
        frame.render_widget(
            Paragraph::new(format!(
                "{}   {}   reasoning {}   sandboxed  ",
                self.provider_name, self.model_name, self.reasoning_effort
            ))
            .alignment(Alignment::Right)
            .style(Style::new().fg(palette().muted).bg(palette().background)),
            status,
        );

        if self.workspace_view == WorkspaceView::Changes {
            self.render_changes(frame, content, footer);
            return;
        }

        let conversation = content;
        let prompt = footer;
        self.conversation_area = Rect::new(
            conversation.x.saturating_add(2),
            conversation.y,
            conversation.width.saturating_sub(4),
            conversation.height,
        );

        let mut lines = Vec::new();
        let markdown_tx = self.markdown_tx.clone();
        for (message_index, message) in self.messages.iter_mut().enumerate() {
            match message.role {
                Role::You => {
                    for (index, line) in message.content.lines().enumerate() {
                        let marker = if index == 0 { "›  " } else { "   " };
                        lines.push(Line::from(vec![
                            Span::styled(marker, Style::new().fg(palette().accent).bold()),
                            Span::styled(
                                line.to_owned(),
                                Style::new().fg(palette().foreground).bold(),
                            ),
                        ]));
                    }
                    lines.push(Line::default());
                }
                Role::Assistant => {
                    let width = conversation.width.saturating_sub(4);
                    if self.streaming_message == Some(message_index) {
                        lines.extend(message.content.lines().map(|line| {
                            Line::styled(line.to_owned(), Style::new().fg(palette().foreground))
                        }));
                    } else {
                        let cached = message
                            .rendered_markdown
                            .as_ref()
                            .filter(|(cached_width, _)| *cached_width == width)
                            .map(|(_, lines)| lines);
                        if let Some(cached) = cached {
                            lines.extend(cached.iter().cloned());
                        } else {
                            if message
                                .markdown_job
                                .is_none_or(|(job_width, _)| job_width != width)
                            {
                                let job = NEXT_MARKDOWN_JOB.fetch_add(1, Ordering::Relaxed);
                                message.markdown_job = Some((width, job));
                                if let Some(rendered) = render_markdown(
                                    markdown_tx.clone(),
                                    message_index,
                                    width,
                                    job,
                                    message.content.clone(),
                                ) {
                                    lines.extend(rendered.iter().cloned());
                                    message.rendered_markdown = Some((width, rendered));
                                    message.markdown_job = None;
                                }
                            }
                            if message.rendered_markdown.is_none() {
                                lines.extend(message.content.lines().map(|line| {
                                    Line::styled(
                                        line.to_owned(),
                                        Style::new().fg(palette().foreground),
                                    )
                                }));
                            }
                        }
                    }
                    lines.push(Line::default());
                    lines.push(Line::default());
                }
                Role::System | Role::Running | Role::Success | Role::Error => {
                    let border = match message.role {
                        Role::Running => palette().accent,
                        Role::Success => palette().added,
                        Role::Error => palette().removed,
                        _ => palette().border,
                    };
                    let rows = message.content.lines().enumerate().map(|(index, line)| {
                        let style = if message.role == Role::Error && index == 0 {
                            Style::new().fg(palette().removed)
                        } else if (message.role == Role::Success && index == 0)
                            || (line.starts_with('+') && !line.starts_with("+++"))
                        {
                            Style::new().fg(palette().added)
                        } else if line.starts_with('-') && !line.starts_with("---") {
                            Style::new().fg(palette().removed)
                        } else {
                            Style::new().fg(palette().muted)
                        };
                        (line, style)
                    });
                    for (line, style) in rows {
                        lines.push(Line::styled(line.to_owned(), style));
                    }
                    lines.push(Line::styled(
                        "─".repeat(usize::from(conversation.width.saturating_sub(4))),
                        Style::new().fg(border),
                    ));
                    lines.push(Line::default());
                }
            }
        }
        let content_width = conversation.width.saturating_sub(4).max(1);
        self.copy_pending_selection(&lines, content_width);
        let rendered_lines = lines
            .iter()
            .map(|line| line.width().max(1).div_ceil(usize::from(content_width)))
            .sum::<usize>();
        let paragraph = Paragraph::new(lines)
            .style(
                Style::new()
                    .fg(palette().foreground)
                    .bg(palette().background),
            )
            .block(
                Block::new()
                    .style(Style::new().bg(palette().background))
                    .padding(Padding::horizontal(2)),
            )
            .wrap(Wrap { trim: false });
        let visible_height = conversation.height;
        self.conversation_max_scroll = u16::try_from(rendered_lines)
            .unwrap_or(u16::MAX)
            .saturating_sub(visible_height);
        if self.follow_output {
            self.conversation_scroll = self.conversation_max_scroll;
        } else {
            self.conversation_scroll = self.conversation_scroll.min(self.conversation_max_scroll);
        }
        frame.render_widget(
            paragraph.scroll((self.conversation_scroll, 0)),
            conversation,
        );
        self.render_conversation_selection(frame);
        self.input.set_cursor_style(if self.cursor_visible {
            Style::new().bg(palette().accent).fg(palette().background)
        } else {
            Style::new()
                .fg(palette().foreground)
                .bg(palette().background)
        });
        self.input.set_block(Self::prompt_block(
            self.session_tokens,
            self.changes_file_count > 0,
        ));
        frame.render_widget(&self.input, prompt);
        if self.model_picker.is_none()
            && self.reasoning_picker.is_none()
            && self.provider_picker.is_none()
            && self.theme_picker.is_none()
        {
            self.render_command_menu(frame, prompt);
        }
        if let Some(picker) = &self.model_picker {
            self.render_model_picker(frame, picker);
        }
        if let Some(picker) = &self.reasoning_picker {
            self.render_reasoning_picker(frame, picker);
        }
        if let Some(picker) = &self.provider_picker {
            self.render_provider_picker(frame, picker);
        }
        if let Some(picker) = &self.theme_picker {
            self.render_theme_picker(frame, picker);
        }
    }

    fn render_workspace_tabs(&self, frame: &mut Frame, area: Rect) {
        let active = Style::new().fg(palette().accent).bold();
        let inactive = Style::new().fg(palette().muted);
        let agent_style = if self.workspace_view == WorkspaceView::Agent {
            active
        } else {
            inactive
        };
        let changes_style = if self.workspace_view == WorkspaceView::Changes {
            active
        } else {
            inactive
        };
        let mut tabs = vec![Span::styled("1. Agent", agent_style)];
        if self.changes_file_count > 0 {
            tabs.extend([
                Span::raw("    "),
                Span::styled(
                    format!("2. Review ({})", self.changes_file_count),
                    changes_style,
                ),
            ]);
        }
        frame.render_widget(
            Paragraph::new(Line::from(tabs)).style(Style::new().bg(palette().background)),
            area,
        );
    }

    fn render_changes(&mut self, frame: &mut Frame, content: Rect, footer: Rect) {
        let body = if let Some(error) = &self.changes_error {
            vec![
                Line::styled(
                    "Git diff is unavailable",
                    Style::new().fg(palette().removed).bold(),
                ),
                Line::default(),
                Line::styled(error, Style::new().fg(palette().muted)),
            ]
        } else if self.changes_diff.is_empty() {
            vec![
                Line::styled(
                    "No working tree changes",
                    Style::new().fg(palette().foreground),
                ),
                Line::default(),
                Line::styled(
                    "This view refreshes automatically when files change.",
                    Style::new().fg(palette().muted),
                ),
            ]
        } else {
            let query = self.changes_query.to_lowercase();
            let mut rendered = Vec::new();
            for (index, line) in self.changes_diff.lines().enumerate() {
                let selected = self.changes_match == Some(index);
                if line.starts_with("diff --git ") {
                    rendered.push(Line::default());
                    let path = line
                        .rsplit_once(" b/")
                        .map_or(line, |(_, path)| path.trim_matches('"'));
                    let style = if selected {
                        Style::new()
                            .fg(palette().background)
                            .bg(palette().accent)
                            .bold()
                    } else {
                        Style::new()
                            .fg(palette().foreground)
                            .bg(palette().surface)
                            .bold()
                    };
                    rendered.push(Line::styled(
                        format!("{} FILE  {path} ", if selected { "›" } else { " " }),
                        style,
                    ));
                    continue;
                }
                let mut style = if line.starts_with('+') && !line.starts_with("+++") {
                    Style::new().fg(palette().added)
                } else if line.starts_with('-') && !line.starts_with("---") {
                    Style::new().fg(palette().removed)
                } else if line.starts_with("@@") {
                    Style::new().fg(palette().accent)
                } else {
                    Style::new().fg(palette().muted)
                };
                if !query.is_empty() && line.to_lowercase().contains(&query) {
                    style = style.bg(palette().surface);
                }
                if selected {
                    style = Style::new()
                        .fg(palette().background)
                        .bg(palette().accent)
                        .bold();
                }
                let marker = if selected { "› " } else { "  " };
                rendered.push(Line::styled(format!("{marker}{line}"), style));
            }
            rendered
        };

        self.changes_max_scroll = u16::try_from(body.len())
            .unwrap_or(u16::MAX)
            .saturating_sub(content.height.saturating_sub(2));
        self.changes_scroll = self.changes_scroll.min(self.changes_max_scroll);
        frame.render_widget(
            Paragraph::new(body)
                .style(Style::new().bg(palette().background))
                .block(
                    Block::new()
                        .borders(Borders::ALL)
                        .border_style(Style::new().fg(palette().border))
                        .title(" changes vs HEAD ")
                        .padding(Padding::horizontal(1)),
                )
                .scroll((self.changes_scroll, self.changes_horizontal_scroll)),
            content,
        );

        let search_status = self.change_match_status();
        let footer_line = if self.changes_searching {
            Line::from(vec![
                Span::styled(" /", Style::new().fg(palette().accent).bold()),
                Span::styled(&self.changes_query, Style::new().fg(palette().foreground)),
                Span::styled("█", Style::new().fg(palette().accent)),
                Span::styled(
                    format!("  {search_status} · Enter select · Esc cancel"),
                    Style::new().fg(palette().muted),
                ),
            ])
        } else {
            Line::from(vec![
                Span::styled(" Alt+1", Style::new().fg(palette().foreground)),
                Span::styled(" agent · ", Style::new().fg(palette().muted)),
                Span::styled("↑/↓ PgUp/PgDn", Style::new().fg(palette().foreground)),
                Span::styled(" scroll · ", Style::new().fg(palette().muted)),
                Span::styled("←/→", Style::new().fg(palette().foreground)),
                Span::styled(" pan · ", Style::new().fg(palette().muted)),
                Span::styled("Tab/Shift+Tab", Style::new().fg(palette().foreground)),
                Span::styled(" files · ", Style::new().fg(palette().muted)),
                Span::styled("/", Style::new().fg(palette().foreground)),
                Span::styled(" search · ", Style::new().fg(palette().muted)),
                Span::styled("n/N", Style::new().fg(palette().foreground)),
                Span::styled(" next/previous ", Style::new().fg(palette().muted)),
                Span::styled(search_status, Style::new().fg(palette().accent).bold()),
            ])
        };
        frame.render_widget(
            Paragraph::new(footer_line)
                .style(Style::new().bg(palette().background))
                .block(
                    Block::new()
                        .borders(Borders::TOP)
                        .border_style(palette().border),
                ),
            footer,
        );
    }

    fn begin_changes_refresh(&mut self) {
        self.begin_changes_load(self.workspace_view == WorkspaceView::Changes);
    }

    fn begin_changes_summary(&mut self) {
        self.begin_changes_load(false);
    }

    fn begin_changes_load(&mut self, full: bool) {
        if self.changes_refreshing {
            return;
        }
        self.changes_refreshing = true;
        let workspace = self.cwd.clone();
        let sender = self.changes_tx.clone();
        tokio::spawn(async move {
            let snapshot = if full {
                changes::load(&workspace).await
            } else {
                changes::load_summary(&workspace).await
            };
            let _ = sender.send(snapshot);
        });
    }

    fn apply_changes_snapshot(&mut self, snapshot: changes::Snapshot) {
        self.changes_refreshing = false;
        if snapshot.full {
            self.changes_diff = snapshot.diff;
        }
        self.changes_file_count = snapshot.file_count;
        self.changes_error = snapshot.error;
        if self.changes_file_count == 0 && self.workspace_view == WorkspaceView::Changes {
            self.workspace_view = WorkspaceView::Agent;
            self.changes_searching = false;
            self.changes_match = None;
        }
        // The user may have opened Review while its summary was still
        // loading. Follow it with the full content now rather than leaving a
        // stale or empty diff visible.
        if !snapshot.full && self.workspace_view == WorkspaceView::Changes {
            self.begin_changes_refresh();
        }
    }

    fn change_match_status(&self) -> String {
        if self.changes_query.is_empty() {
            return String::new();
        }
        let query = self.changes_query.to_lowercase();
        let matches = self
            .changes_diff
            .lines()
            .enumerate()
            .filter_map(|(index, line)| line.to_lowercase().contains(&query).then_some(index))
            .collect::<Vec<_>>();
        if matches.is_empty() {
            return "no matches".to_owned();
        }
        if let Some(selected) = self.changes_match
            && let Some(position) = matches.iter().position(|index| *index == selected)
        {
            return format!("match {}/{}", position + 1, matches.len());
        }
        format!("{} matches", matches.len())
    }

    fn rendered_change_index(&self, raw_index: usize) -> usize {
        raw_index
            + self
                .changes_diff
                .lines()
                .take(raw_index + 1)
                .filter(|line| line.starts_with("diff --git "))
                .count()
    }

    fn render_theme_picker(&self, frame: &mut Frame, picker: &ThemePicker) {
        let area = frame.area();
        let width = area.width.saturating_sub(4).min(52);
        let height = area.height.saturating_sub(4).min(
            u16::try_from(Theme::ALL.len())
                .unwrap_or(u16::MAX)
                .saturating_mul(2)
                .saturating_add(2),
        );
        let popup = Rect::new(
            area.x + area.width.saturating_sub(width) / 2,
            area.y + area.height.saturating_sub(height) / 2,
            width,
            height,
        );
        frame.render_widget(Clear, popup);

        let current_theme = theme();
        let lines = Theme::ALL
            .iter()
            .enumerate()
            .flat_map(|(index, option)| {
                let current = if *option == current_theme { "●" } else { " " };
                let marker = if index == picker.selected { "›" } else { " " };
                let selected_style = if index == picker.selected {
                    Style::new()
                        .fg(palette().background)
                        .bg(palette().accent)
                        .bold()
                } else {
                    Style::new().fg(palette().foreground).bg(palette().surface)
                };
                [
                    Line::styled(
                        format!(" {marker} {current} {}", option.name()),
                        selected_style,
                    ),
                    Line::styled(
                        format!("       {}", option.description()),
                        Style::new().fg(palette().muted).bg(palette().surface),
                    ),
                ]
            })
            .collect::<Vec<_>>();
        frame.render_widget(
            Paragraph::new(lines)
                .style(Style::new().bg(palette().surface))
                .block(
                    Block::new()
                        .borders(Borders::ALL)
                        .border_style(Style::new().fg(palette().accent))
                        .title(" theme · ↑/↓ select · Enter apply · Esc close "),
                ),
            popup,
        );
    }

    fn render_command_menu(&self, frame: &mut Frame, prompt: Rect) {
        let commands = self.matching_commands();
        if commands.is_empty() {
            return;
        }
        let height = u16::try_from(commands.len())
            .unwrap_or(u16::MAX)
            .saturating_add(2)
            .min(prompt.y);
        if height < 3 {
            return;
        }
        let width = frame.area().width.saturating_sub(4).min(72);
        let popup = Rect::new(prompt.x.saturating_add(2), prompt.y - height, width, height);
        let selected = self.command_selection.min(commands.len().saturating_sub(1));
        let lines = commands
            .iter()
            .enumerate()
            .map(|(index, command)| {
                let marker = if index == selected { "›" } else { " " };
                let style = if index == selected {
                    Style::new()
                        .fg(palette().background)
                        .bg(palette().accent)
                        .bold()
                } else {
                    Style::new().fg(palette().foreground).bg(palette().surface)
                };
                Line::from(vec![
                    Span::styled(format!(" {marker} {:<12}", command.name), style),
                    Span::styled(
                        format!(" {}", command.description),
                        if index == selected {
                            style
                        } else {
                            Style::new().fg(palette().muted).bg(palette().surface)
                        },
                    ),
                ])
            })
            .collect::<Vec<_>>();
        frame.render_widget(Clear, popup);
        frame.render_widget(
            Paragraph::new(lines)
                .style(Style::new().bg(palette().surface))
                .block(
                    Block::new()
                        .borders(Borders::ALL)
                        .border_style(Style::new().fg(palette().accent))
                        .title(" commands · ↑/↓ or Tab select · Enter run "),
                ),
            popup,
        );
    }

    fn render_model_picker(&self, frame: &mut Frame, picker: &ModelPicker) {
        let area = frame.area();
        let width = area.width.saturating_sub(4).min(68);
        let height = area.height.saturating_sub(4).min(
            u16::try_from(picker.models.len())
                .unwrap_or(u16::MAX)
                .saturating_add(2),
        );
        let popup = Rect::new(
            area.x + area.width.saturating_sub(width) / 2,
            area.y + area.height.saturating_sub(height) / 2,
            width,
            height,
        );
        frame.render_widget(Clear, popup);

        let visible = usize::from(height.saturating_sub(2));
        let start = picker
            .selected
            .saturating_sub(visible.saturating_sub(1))
            .min(picker.models.len().saturating_sub(visible));
        let lines = picker
            .models
            .iter()
            .enumerate()
            .skip(start)
            .take(visible)
            .map(|(index, model)| {
                let current = if model.id == self.model_id {
                    "●"
                } else {
                    " "
                };
                let marker = if index == picker.selected { "›" } else { " " };
                let style = if index == picker.selected {
                    Style::new()
                        .fg(palette().background)
                        .bg(palette().accent)
                        .bold()
                } else {
                    Style::new().fg(palette().foreground).bg(palette().surface)
                };
                Line::styled(
                    format!(" {marker} {current} {}  {}", model.display_name, model.id),
                    style,
                )
            })
            .collect::<Vec<_>>();
        frame.render_widget(
            Paragraph::new(lines)
                .style(Style::new().bg(palette().surface))
                .block(
                    Block::new()
                        .borders(Borders::ALL)
                        .border_style(Style::new().fg(palette().accent))
                        .title(" model · ↑/↓ select · Enter apply · Esc close "),
                ),
            popup,
        );
    }

    fn render_reasoning_picker(&self, frame: &mut Frame, picker: &ReasoningPicker) {
        let area = frame.area();
        let width = area.width.saturating_sub(4).min(72);
        let height = area.height.saturating_sub(4).min(
            u16::try_from(picker.options.len())
                .unwrap_or(u16::MAX)
                .saturating_mul(2)
                .saturating_add(2),
        );
        let popup = Rect::new(
            area.x + area.width.saturating_sub(width) / 2,
            area.y + area.height.saturating_sub(height) / 2,
            width,
            height,
        );
        frame.render_widget(Clear, popup);

        let lines = picker
            .options
            .iter()
            .enumerate()
            .flat_map(|(index, option)| {
                let current = if option.id == self.reasoning_effort {
                    "●"
                } else {
                    " "
                };
                let marker = if index == picker.selected { "›" } else { " " };
                let style = if index == picker.selected {
                    Style::new()
                        .fg(palette().background)
                        .bg(palette().accent)
                        .bold()
                } else {
                    Style::new().fg(palette().foreground).bg(palette().surface)
                };
                [
                    Line::styled(format!(" {marker} {current} {}", option.id), style),
                    Line::styled(
                        format!("       {}", option.description),
                        Style::new().fg(palette().muted).bg(palette().surface),
                    ),
                ]
            })
            .collect::<Vec<_>>();
        frame.render_widget(
            Paragraph::new(lines)
                .style(Style::new().bg(palette().surface))
                .block(
                    Block::new()
                        .borders(Borders::ALL)
                        .border_style(Style::new().fg(palette().accent))
                        .title(" reasoning · ↑/↓ select · Enter apply · Esc close "),
                ),
            popup,
        );
    }

    fn render_provider_picker(&self, frame: &mut Frame, picker: &ProviderPicker) {
        let area = frame.area();
        let width = area.width.saturating_sub(4).min(68);
        let height = area.height.saturating_sub(4).min(
            u16::try_from(picker.providers.len())
                .unwrap_or(u16::MAX)
                .saturating_mul(2)
                .saturating_add(2),
        );
        let popup = Rect::new(
            area.x + area.width.saturating_sub(width) / 2,
            area.y + area.height.saturating_sub(height) / 2,
            width,
            height,
        );
        frame.render_widget(Clear, popup);

        let lines = picker
            .providers
            .iter()
            .enumerate()
            .flat_map(|(index, provider)| {
                let current = if self.active_provider == Some(*provider) {
                    "●"
                } else {
                    " "
                };
                let marker = if index == picker.selected { "›" } else { " " };
                let style = if index == picker.selected {
                    Style::new()
                        .fg(palette().background)
                        .bg(palette().accent)
                        .bold()
                } else {
                    Style::new().fg(palette().foreground).bg(palette().surface)
                };
                [
                    Line::styled(
                        format!(" {marker} {current} {}", provider.display_name()),
                        style,
                    ),
                    Line::styled(
                        format!("       {}", provider.description()),
                        Style::new().fg(palette().muted).bg(palette().surface),
                    ),
                ]
            })
            .collect::<Vec<_>>();
        frame.render_widget(
            Paragraph::new(lines)
                .style(Style::new().bg(palette().surface))
                .block(
                    Block::new()
                        .borders(Borders::ALL)
                        .border_style(Style::new().fg(palette().accent))
                        .title(" provider · ↑/↓ select · Enter connect · Esc close "),
                ),
            popup,
        );
    }

    fn handle_terminal_event(&mut self, event: Event) {
        if let Event::Key(key) = event
            && key.kind == KeyEventKind::Press
            && self.handle_workspace_shortcut(key)
        {
            return;
        }
        if self.workspace_view == WorkspaceView::Changes {
            self.handle_changes_event(event);
            return;
        }
        if self.theme_picker.is_some() {
            if let Event::Key(key) = event
                && key.kind == KeyEventKind::Press
            {
                self.handle_theme_picker_key(key);
            }
            return;
        }
        if self.provider_picker.is_some() {
            if let Event::Key(key) = event
                && key.kind == KeyEventKind::Press
            {
                self.handle_provider_picker_key(key);
            }
            return;
        }
        if self.model_picker.is_some() {
            if let Event::Key(key) = event
                && key.kind == KeyEventKind::Press
            {
                self.handle_model_picker_key(key);
            }
            return;
        }
        if self.reasoning_picker.is_some() {
            if let Event::Key(key) = event
                && key.kind == KeyEventKind::Press
            {
                self.handle_reasoning_picker_key(key);
            }
            return;
        }
        if let Event::Paste(text) = &event {
            // Terminals send CR or CRLF line ends; the prompt stores LF.
            let text = text.replace("\r\n", "\n").replace('\r', "\n");
            self.input.insert_str(text);
            self.command_selection = 0;
            self.history_index = None;
            return;
        }
        if let Event::Mouse(mouse) = event {
            self.handle_conversation_mouse(mouse);
            return;
        }
        if let Event::Key(key) = event
            && key.kind == KeyEventKind::Press
            && self.handle_key(key)
        {
            return;
        }
        self.input.input(event);
        self.command_selection = 0;
        self.history_index = None;
    }

    fn handle_workspace_shortcut(&mut self, key: KeyEvent) -> bool {
        match (key.code, key.modifiers) {
            (KeyCode::Char('1'), KeyModifiers::ALT) => {
                self.workspace_view = WorkspaceView::Agent;
                self.changes_searching = false;
                true
            }
            (KeyCode::Char('2'), KeyModifiers::ALT) => {
                if self.changes_file_count == 0 {
                    return false;
                }
                self.workspace_view = WorkspaceView::Changes;
                self.model_picker = None;
                self.reasoning_picker = None;
                self.provider_picker = None;
                self.theme_picker = None;
                self.begin_changes_refresh();
                true
            }
            _ => false,
        }
    }

    fn handle_changes_event(&mut self, event: Event) {
        if let Event::Mouse(mouse) = event {
            match mouse.kind {
                MouseEventKind::ScrollUp => {
                    self.changes_scroll = self.changes_scroll.saturating_sub(3);
                }
                MouseEventKind::ScrollDown => {
                    self.changes_scroll = self
                        .changes_scroll
                        .saturating_add(3)
                        .min(self.changes_max_scroll);
                }
                _ => {}
            }
            return;
        }
        let Event::Key(key) = event else {
            return;
        };
        if key.kind != KeyEventKind::Press {
            return;
        }
        if self.changes_searching {
            match key.code {
                KeyCode::Enter => {
                    self.changes_searching = false;
                    self.find_change_match(true);
                }
                KeyCode::Esc => self.changes_searching = false,
                KeyCode::Backspace => {
                    self.changes_query.pop();
                    self.changes_match = None;
                }
                KeyCode::Char(character)
                    if key.modifiers == KeyModifiers::NONE
                        || key.modifiers == KeyModifiers::SHIFT =>
                {
                    self.changes_query.push(character);
                    self.changes_match = None;
                }
                _ => {}
            }
            return;
        }
        match (key.code, key.modifiers) {
            (KeyCode::Char('c'), KeyModifiers::CONTROL) => self.should_quit = true,
            (KeyCode::Esc, _) | (KeyCode::Char('q'), KeyModifiers::NONE) => {
                self.workspace_view = WorkspaceView::Agent;
            }
            (KeyCode::Up, _) | (KeyCode::Char('k'), KeyModifiers::NONE) => {
                self.changes_scroll = self.changes_scroll.saturating_sub(1);
            }
            (KeyCode::Down, _) | (KeyCode::Char('j'), KeyModifiers::NONE) => {
                self.changes_scroll = self
                    .changes_scroll
                    .saturating_add(1)
                    .min(self.changes_max_scroll);
            }
            (KeyCode::PageUp, _) => {
                self.changes_scroll = self.changes_scroll.saturating_sub(10);
            }
            (KeyCode::PageDown, _) => {
                self.changes_scroll = self
                    .changes_scroll
                    .saturating_add(10)
                    .min(self.changes_max_scroll);
            }
            (KeyCode::Home, _) => self.changes_scroll = 0,
            (KeyCode::End, _) => self.changes_scroll = self.changes_max_scroll,
            (KeyCode::Tab, KeyModifiers::NONE) => self.jump_change_file(true),
            (KeyCode::BackTab, _) | (KeyCode::Tab, KeyModifiers::SHIFT) => {
                self.jump_change_file(false);
            }
            (KeyCode::Left, _) | (KeyCode::Char('h'), KeyModifiers::NONE) => {
                self.changes_horizontal_scroll = self.changes_horizontal_scroll.saturating_sub(4);
            }
            (KeyCode::Right, _) | (KeyCode::Char('l'), KeyModifiers::NONE) => {
                self.changes_horizontal_scroll = self.changes_horizontal_scroll.saturating_add(4);
            }
            (KeyCode::Char('/'), KeyModifiers::NONE) => {
                self.changes_searching = true;
                self.changes_query.clear();
                self.changes_match = None;
            }
            (KeyCode::Char('n'), KeyModifiers::NONE) => self.find_change_match(true),
            (KeyCode::Char('N'), KeyModifiers::SHIFT) => self.find_change_match(false),
            _ => {}
        }
    }

    fn find_change_match(&mut self, forward: bool) {
        if self.changes_query.is_empty() {
            return;
        }
        let query = self.changes_query.to_lowercase();
        let lines = self.changes_diff.lines().collect::<Vec<_>>();
        if lines.is_empty() {
            return;
        }
        let origin = self
            .changes_match
            .unwrap_or_else(|| usize::from(self.changes_scroll));
        let found = if forward {
            (0..lines.len())
                .map(|offset| {
                    (origin + usize::from(self.changes_match.is_some()) + offset) % lines.len()
                })
                .find(|index| lines[*index].to_lowercase().contains(&query))
        } else {
            (0..lines.len())
                .map(|offset| (origin + lines.len() - 1 - offset) % lines.len())
                .find(|index| lines[*index].to_lowercase().contains(&query))
        };
        if let Some(index) = found {
            self.changes_match = Some(index);
            self.changes_scroll =
                u16::try_from(self.rendered_change_index(index)).unwrap_or(u16::MAX);
        }
    }

    fn jump_change_file(&mut self, forward: bool) {
        let headers = self
            .changes_diff
            .lines()
            .enumerate()
            .filter_map(|(index, line)| line.starts_with("diff --git ").then_some(index))
            .collect::<Vec<_>>();
        if headers.is_empty() {
            return;
        }
        let current_scroll = usize::from(self.changes_scroll).saturating_add(1);
        let current = headers
            .iter()
            .rposition(|index| self.rendered_change_index(*index) <= current_scroll)
            .unwrap_or(0);
        let next = if forward {
            (current + 1) % headers.len()
        } else {
            (current + headers.len() - 1) % headers.len()
        };
        self.changes_scroll =
            u16::try_from(self.rendered_change_index(headers[next])).unwrap_or(u16::MAX);
    }

    fn handle_provider_picker_key(&mut self, key: KeyEvent) {
        match (key.code, key.modifiers) {
            (KeyCode::Esc, _) | (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                self.provider_picker = None;
            }
            (KeyCode::Up, _) | (KeyCode::Char('k'), KeyModifiers::NONE) => {
                if let Some(picker) = &mut self.provider_picker {
                    picker.selected = picker.selected.saturating_sub(1);
                }
            }
            (KeyCode::Down, _) | (KeyCode::Char('j'), KeyModifiers::NONE) => {
                if let Some(picker) = &mut self.provider_picker {
                    picker.selected =
                        (picker.selected + 1).min(picker.providers.len().saturating_sub(1));
                }
            }
            (KeyCode::Enter, _) => {
                let provider = self
                    .provider_picker
                    .as_ref()
                    .and_then(|picker| picker.providers.get(picker.selected))
                    .copied();
                if let Some(provider) = provider {
                    self.provider_picker = None;
                    self.begin_connect(provider);
                }
            }
            _ => {}
        }
    }

    fn handle_theme_picker_key(&mut self, key: KeyEvent) {
        match (key.code, key.modifiers) {
            (KeyCode::Esc, _) | (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                self.theme_picker = None;
            }
            (KeyCode::Up, _) | (KeyCode::Char('k'), KeyModifiers::NONE) => {
                if let Some(picker) = &mut self.theme_picker {
                    picker.selected = picker.selected.saturating_sub(1);
                }
            }
            (KeyCode::Down, _) | (KeyCode::Char('j'), KeyModifiers::NONE) => {
                if let Some(picker) = &mut self.theme_picker {
                    picker.selected = (picker.selected + 1).min(Theme::ALL.len().saturating_sub(1));
                }
            }
            (KeyCode::Enter, _) => {
                let selected = self
                    .theme_picker
                    .as_ref()
                    .and_then(|picker| Theme::ALL.get(picker.selected))
                    .copied();
                self.theme_picker = None;
                if let Some(selected) = selected {
                    self.apply_theme(selected);
                }
            }
            _ => {}
        }
    }

    fn handle_model_picker_key(&mut self, key: KeyEvent) {
        match (key.code, key.modifiers) {
            (KeyCode::Esc, _) | (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                self.model_picker = None;
            }
            (KeyCode::Up, _) | (KeyCode::Char('k'), KeyModifiers::NONE) => {
                if let Some(picker) = &mut self.model_picker {
                    picker.selected = picker.selected.saturating_sub(1);
                }
            }
            (KeyCode::Down, _) | (KeyCode::Char('j'), KeyModifiers::NONE) => {
                if let Some(picker) = &mut self.model_picker {
                    picker.selected = (picker.selected + 1).min(picker.models.len() - 1);
                }
            }
            (KeyCode::Enter, _) => {
                let model = self
                    .model_picker
                    .as_ref()
                    .and_then(|picker| picker.models.get(picker.selected))
                    .cloned();
                self.model_picker = None;
                if let Some(model) = model {
                    let result = self.provider.as_ref().map_or_else(
                        || Ok(model.clone()),
                        |provider| provider.select_model(&model.id),
                    );
                    match result {
                        Ok(model) => {
                            self.model_id = model.id;
                            self.model_name = model.display_name.clone();
                            self.reasoning_options = model.reasoning_efforts.clone();
                            self.reasoning_effort = model.default_reasoning_effort.clone();
                            self.restore_reasoning_for_model();
                            if let Some(provider) = self.active_provider
                                && let Err(error) = save_model(provider, &self.model_id)
                            {
                                self.push(
                                    Role::System,
                                    format!("Could not remember the model: {error:#}"),
                                );
                            }
                            self.push(
                                Role::System,
                                format!("Model switched to {}.", model.display_name),
                            );
                        }
                        Err(error) => {
                            self.push(Role::System, format!("Could not switch model: {error:#}"))
                        }
                    }
                }
            }
            _ => {}
        }
    }

    fn handle_reasoning_picker_key(&mut self, key: KeyEvent) {
        match (key.code, key.modifiers) {
            (KeyCode::Esc, _) | (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                self.reasoning_picker = None;
            }
            (KeyCode::Up, _) | (KeyCode::Char('k'), KeyModifiers::NONE) => {
                if let Some(picker) = &mut self.reasoning_picker {
                    picker.selected = picker.selected.saturating_sub(1);
                }
            }
            (KeyCode::Down, _) | (KeyCode::Char('j'), KeyModifiers::NONE) => {
                if let Some(picker) = &mut self.reasoning_picker {
                    picker.selected =
                        (picker.selected + 1).min(picker.options.len().saturating_sub(1));
                }
            }
            (KeyCode::Enter, _) => {
                let option = self
                    .reasoning_picker
                    .as_ref()
                    .and_then(|picker| picker.options.get(picker.selected))
                    .cloned();
                self.reasoning_picker = None;
                if let Some(option) = option {
                    let result = self.provider.as_ref().map_or_else(
                        || Ok(option.id.clone()),
                        |provider| provider.select_reasoning_effort(&option.id),
                    );
                    match result {
                        Ok(effort) => {
                            self.reasoning_effort = effort;
                            if let Some(provider) = self.active_provider
                                && let Err(error) =
                                    save_reasoning(provider, &self.model_id, &self.reasoning_effort)
                            {
                                self.push(
                                    Role::System,
                                    format!("Could not remember reasoning effort: {error:#}"),
                                );
                            }
                            self.push(
                                Role::System,
                                format!("Reasoning set to {}.", self.reasoning_effort),
                            );
                        }
                        Err(error) => self.push(
                            Role::System,
                            format!("Could not change reasoning effort: {error:#}"),
                        ),
                    }
                }
            }
            _ => {}
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> bool {
        match (key.code, key.modifiers) {
            (KeyCode::Char('l'), KeyModifiers::CONTROL) => {
                self.force_redraw = true;
                true
            }
            (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                if self.request.is_some() {
                    self.interrupt_active();
                } else {
                    self.should_quit = true;
                }
                true
            }
            (KeyCode::Esc, _) if self.request.is_some() => {
                self.queued_request = None;
                self.interrupt_active();
                true
            }
            (KeyCode::Enter, modifiers) if modifiers.contains(KeyModifiers::SHIFT) => {
                self.input.insert_newline();
                true
            }
            // While browsing history Up/Down keep browsing, even if the
            // recalled prompt is a slash command that would open the menu.
            (KeyCode::Up | KeyCode::Down, KeyModifiers::NONE)
                if self.history_index.is_some() && self.recall_history(key.code) =>
            {
                true
            }
            _ if self.handle_command_key(key) => true,
            (KeyCode::Up | KeyCode::Down, KeyModifiers::NONE) if self.recall_history(key.code) => {
                true
            }
            (KeyCode::Enter, modifiers) if !modifiers.contains(KeyModifiers::SHIFT) => {
                self.submit();
                true
            }
            (KeyCode::PageUp, _) => {
                self.scroll_conversation_up(10);
                true
            }
            (KeyCode::PageDown, _) => {
                self.scroll_conversation_down(10);
                true
            }
            _ => false,
        }
    }

    fn handle_command_key(&mut self, key: KeyEvent) -> bool {
        let command_count = self.matching_commands().len();
        if command_count == 0 {
            return false;
        }
        match key.code {
            KeyCode::Up => {
                self.command_selection = self.command_selection.saturating_sub(1);
                true
            }
            KeyCode::Down => {
                self.command_selection = (self.command_selection + 1).min(command_count - 1);
                true
            }
            KeyCode::Tab => {
                self.command_selection = (self.command_selection + 1) % command_count;
                true
            }
            KeyCode::Enter => {
                let command = self.matching_commands()
                    [self.command_selection.min(command_count.saturating_sub(1))]
                .name;
                self.input = Self::new_input();
                self.input.insert_str(command);
                self.command_selection = 0;
                self.submit();
                true
            }
            _ => false,
        }
    }

    /// Up on the first line recalls older prompts; Down on the last line
    /// moves forward and finally restores the unsent draft. Returns false
    /// when the key should move the cursor inside the prompt instead.
    fn recall_history(&mut self, key: KeyCode) -> bool {
        let row = self.input.cursor().0;
        let last_row = self.input.lines().len().saturating_sub(1);
        let next = match (key, self.history_index) {
            (KeyCode::Up, _) if row != 0 || self.prompt_history.is_empty() => return false,
            (KeyCode::Up, None) => {
                self.history_draft = self.input.lines().join("\n");
                self.prompt_history.len() - 1
            }
            (KeyCode::Up, Some(index)) => index.saturating_sub(1),
            (KeyCode::Down, Some(index)) if row == last_row => index + 1,
            _ => return false,
        };
        let text = if next < self.prompt_history.len() {
            self.history_index = Some(next);
            self.prompt_history[next].clone()
        } else {
            self.history_index = None;
            std::mem::take(&mut self.history_draft)
        };
        self.input = Self::new_input();
        self.input.insert_str(text);
        if key == KeyCode::Up {
            // Keep the next Up browsing instead of walking up a multi-line recall.
            self.input.move_cursor(CursorMove::Top);
        }
        self.command_selection = 0;
        true
    }

    fn matching_commands(&self) -> Vec<&'static SlashCommand> {
        let lines = self.input.lines();
        let Some(input) = lines.first().map(String::as_str) else {
            return Vec::new();
        };
        if lines.len() != 1 || !input.starts_with('/') || input.chars().any(char::is_whitespace) {
            return Vec::new();
        }
        SLASH_COMMANDS
            .iter()
            .filter(|command| command.visible && command.name.starts_with(input))
            .collect()
    }

    fn scroll_conversation_up(&mut self, lines: u16) {
        if self.conversation_scroll == 0 {
            return;
        }
        self.follow_output = false;
        let previous = self.conversation_scroll;
        self.conversation_scroll = self.conversation_scroll.saturating_sub(lines);
        self.adjust_drag_selection(previous);
    }

    fn scroll_conversation_down(&mut self, lines: u16) {
        let previous = self.conversation_scroll;
        self.conversation_scroll = self
            .conversation_scroll
            .saturating_add(lines)
            .min(self.conversation_max_scroll);
        self.adjust_drag_selection(previous);
        self.follow_output = self.conversation_scroll == self.conversation_max_scroll;
    }

    fn adjust_drag_selection(&mut self, previous_scroll: u16) {
        let Some(selection) = &mut self.conversation_selection else {
            return;
        };
        if !selection.dragging {
            return;
        }
        selection.cursor.1 = if self.conversation_scroll >= previous_scroll {
            selection
                .cursor
                .1
                .saturating_add(self.conversation_scroll - previous_scroll)
        } else {
            selection
                .cursor
                .1
                .saturating_sub(previous_scroll - self.conversation_scroll)
        };
    }

    fn handle_conversation_mouse(&mut self, mouse: MouseEvent) {
        match mouse.kind {
            MouseEventKind::ScrollUp => self.scroll_conversation_up(3),
            MouseEventKind::ScrollDown => self.scroll_conversation_down(3),
            MouseEventKind::Down(MouseButton::Left)
                if self
                    .conversation_area
                    .contains((mouse.column, mouse.row).into()) =>
            {
                self.follow_output = false;
                self.conversation_selection = Some(ConversationSelection {
                    anchor: screen_to_document(
                        self.conversation_area,
                        self.conversation_scroll,
                        mouse.column,
                        mouse.row,
                    ),
                    cursor: screen_to_document(
                        self.conversation_area,
                        self.conversation_scroll,
                        mouse.column,
                        mouse.row,
                    ),
                    dragging: true,
                    copy_pending: false,
                });
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                let area = self.conversation_area;
                if let Some(selection) = &mut self.conversation_selection {
                    selection.cursor =
                        screen_to_document(area, self.conversation_scroll, mouse.column, mouse.row);
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let area = self.conversation_area;
                if let Some(selection) = &mut self.conversation_selection {
                    selection.cursor =
                        screen_to_document(area, self.conversation_scroll, mouse.column, mouse.row);
                    selection.dragging = false;
                    if selection.anchor == selection.cursor {
                        self.conversation_selection = None;
                    } else {
                        selection.copy_pending = true;
                    }
                }
            }
            _ => {}
        }
    }

    fn render_conversation_selection(&mut self, frame: &mut Frame) {
        let Some(selection) = &mut self.conversation_selection else {
            return;
        };
        let area = self.conversation_area.intersection(frame.area());
        if area.is_empty() {
            return;
        }
        let (start, end) = ordered_selection(selection.anchor, selection.cursor);
        if let Some((start, end)) = project_selection(area, self.conversation_scroll, start, end) {
            highlight_selection(frame.buffer_mut(), area, start, end);
        }
    }

    fn copy_pending_selection(&mut self, lines: &[Line<'_>], width: u16) {
        let Some(selection) = &mut self.conversation_selection else {
            return;
        };
        if !selection.copy_pending {
            return;
        }
        let (start, end) = ordered_selection(selection.anchor, selection.cursor);
        let text = selected_document_text(lines, width, start, end);
        if !text.is_empty() {
            let _ = copy_to_clipboard(&text);
        }
        selection.copy_pending = false;
    }

    fn follow_latest_output(&mut self) {
        self.follow_output = true;
    }

    fn submit(&mut self) {
        let content = self.input.lines().join("\n").trim().to_owned();
        if content.is_empty() {
            return;
        }
        if self.connection.is_some() && !is_local_command(&content) {
            if self.messages.last().is_none_or(|message| {
                message.content != "Provider is still connecting. Your prompt was not sent."
            }) {
                self.push(
                    Role::System,
                    "Provider is still connecting. Your prompt was not sent.",
                );
            }
            return;
        }
        self.follow_latest_output();
        self.input = Self::new_input();
        if self.prompt_history.last() != Some(&content) {
            self.prompt_history.push(content.clone());
            if self.prompt_history.len() > PROMPT_HISTORY_LIMIT {
                self.prompt_history.remove(0);
            }
        }
        self.history_index = None;
        self.history_draft.clear();
        match content.as_str() {
            "/q" | "/quit" | "/exit" => self.should_quit = true,
            "/clear" => {
                self.messages.clear();
                self.streaming_message = None;
                self.tool_summaries.clear();
                self.running_tools.clear();
                self.conversation_scroll = 0;
                self.conversation_max_scroll = 0;
            }
            "/details" => match self.last_tool_output.clone() {
                Some((title, output)) if !output.trim().is_empty() => {
                    self.push(
                        Role::System,
                        format!("Full output: {title}\n{}", output.trim_end()),
                    );
                }
                Some((title, _)) => {
                    self.push(Role::System, format!("{title} produced no output."));
                }
                None => self.push(Role::System, "No tool output is available yet."),
            },
            "/provider" => self.open_provider_picker(),
            "/model" => self.open_model_picker(),
            "/reasoning" => self.open_reasoning_picker(),
            "/new" => self.begin_new_session(),
            "/exec" => self.push(Role::System, "Usage: /exec [--cwd DIR] <command>"),
            "/detach" => match crate::sessions::request_detach() {
                Ok(()) => self.push(Role::System, "Session detached."),
                Err(error) => self.push(Role::Error, format!("Could not detach: {error:#}")),
            },
            "/theme" => self.open_theme_picker(),
            "/undo" => self.undo_last(),
            "/help" => self.push(Role::System, Self::command_help()),
            _ if content.starts_with("/exec ") => {
                match parse_exec(&self.cwd, content[6..].trim()) {
                    Ok((cwd, command)) => self.begin_exec(cwd, command),
                    Err(error) => {
                        self.push(
                            Role::System,
                            format!("Usage: /exec [--cwd DIR] <command>\n{error}"),
                        );
                    }
                }
            }
            _ if self.request.is_some() => {
                self.queued_request = Some(content);
                self.interrupt_active();
            }
            _ => self.begin_request(content),
        }
    }

    pub(crate) fn enable_undo(&mut self) {
        match Undo::open(&self.cwd) {
            Ok(undo) => self.undo = Some(Arc::new(Mutex::new(undo))),
            Err(error) => self.push(
                Role::System,
                format!("/undo is unavailable this session: {error:#}"),
            ),
        }
    }

    fn undo_last(&mut self) {
        if self.request.is_some() {
            self.push(
                Role::System,
                "A response or command is running. Press Esc to stop it before /undo.",
            );
            return;
        }
        let Some(undo) = self.undo.clone() else {
            self.push(Role::System, "/undo is unavailable this session.");
            return;
        };
        let result = undo.lock().unwrap_or_else(PoisonError::into_inner).undo();
        match result {
            Ok(None) => self.push(Role::System, "Nothing to undo yet."),
            Ok(Some(changes)) if changes.is_empty() => {
                self.push(Role::System, "The last turn changed no tracked files.");
            }
            Ok(Some(changes)) => {
                let mut lines = vec![format!(
                    "Undid the last turn: {} file(s). The model is not told; mention it if you continue.",
                    changes.len()
                )];
                for change in changes.iter().take(20) {
                    let action = match change.change_type {
                        ChangeType::Modified => "restored",
                        ChangeType::Created => "recreated",
                        ChangeType::Deleted => "removed",
                        ChangeType::PermissionsChanged => "permissions",
                    };
                    let path = change.path.strip_prefix(&self.cwd).unwrap_or(&change.path);
                    lines.push(format!("{action:<10} {}", path.display()));
                }
                if changes.len() > 20 {
                    lines.push(format!("…and {} more", changes.len() - 20));
                }
                self.push(Role::Success, lines.join("\n"));
                self.begin_changes_refresh();
            }
            Err(error) => self.push(Role::Error, format!("Undo failed\n{error:#}")),
        }
    }

    fn open_theme_picker(&mut self) {
        let selected = Theme::ALL
            .iter()
            .position(|option| *option == theme())
            .unwrap_or(0);
        self.provider_picker = None;
        self.model_picker = None;
        self.reasoning_picker = None;
        self.theme_picker = Some(ThemePicker { selected });
    }

    fn apply_theme(&mut self, selected: Theme) {
        select_theme(selected);
        for message in &mut self.messages {
            message.rendered_markdown = None;
            message.markdown_job = None;
        }
        // The prompt caches its styles, so rebuild it with the new palette.
        self.input = Self::new_input();
        let name = selected.name();
        match save_theme(name) {
            Ok(()) => self.push(Role::System, format!("Theme: {name}.")),
            Err(error) => self.push(
                Role::Error,
                format!("Theme: {name} (could not save it: {error:#})"),
            ),
        }
    }

    fn command_help() -> String {
        let commands = SLASH_COMMANDS
            .iter()
            .filter(|command| command.visible)
            .map(|command| format!("{:<12} {}", command.name, command.description))
            .collect::<Vec<_>>()
            .join("\n");
        format!("## Commands\n\n{commands}")
    }

    fn interrupt_active(&self) {
        if !self.request_uses_provider {
            if let Some(request) = &self.request {
                request.abort();
                let _ = self
                    .events_tx
                    .send((self.request_generation, ProviderEvent::Interrupted));
            }
            return;
        }
        if let Some(provider) = self.provider.clone() {
            let request_id = self.request_generation;
            let sender = self.events_tx.clone();
            tokio::spawn(async move {
                let event = match provider.interrupt().await {
                    Ok(()) => ProviderEvent::Interrupted,
                    Err(error) => ProviderEvent::Failed(format!("{error:#}")),
                };
                let _ = sender.send((request_id, event));
            });
        }
    }

    fn open_provider_picker(&mut self) {
        let providers = ProviderKind::ALL.to_vec();
        let selected = self
            .active_provider
            .and_then(|active| providers.iter().position(|provider| *provider == active))
            .unwrap_or(0);
        self.model_picker = None;
        self.reasoning_picker = None;
        self.theme_picker = None;
        self.provider_picker = Some(ProviderPicker {
            providers,
            selected,
        });
    }

    fn open_model_picker(&mut self) {
        let models = self.models.clone();
        if models.is_empty() {
            self.push(Role::System, "The provider returned no selectable models.");
            return;
        }
        let selected = models
            .iter()
            .position(|model| model.id == self.model_id)
            .unwrap_or(0);
        self.provider_picker = None;
        self.reasoning_picker = None;
        self.theme_picker = None;
        self.model_picker = Some(ModelPicker { models, selected });
    }

    fn open_reasoning_picker(&mut self) {
        let options = self.reasoning_options.clone();
        if options.is_empty() {
            self.push(
                Role::System,
                "The selected model returned no reasoning options.",
            );
            return;
        }
        let selected = options
            .iter()
            .position(|option| option.id == self.reasoning_effort)
            .unwrap_or(0);
        self.provider_picker = None;
        self.model_picker = None;
        self.theme_picker = None;
        self.reasoning_picker = Some(ReasoningPicker { options, selected });
    }

    fn restore_reasoning_for_model(&mut self) {
        let Some(provider_kind) = self.active_provider else {
            return;
        };
        let saved = match load_reasoning(provider_kind, &self.model_id) {
            Ok(saved) => saved,
            Err(error) => {
                self.push(
                    Role::System,
                    format!("Could not load the saved reasoning effort: {error:#}"),
                );
                return;
            }
        };
        let Some(saved) = saved else {
            return;
        };
        let Some(provider) = &self.provider else {
            return;
        };
        match provider.select_reasoning_effort(&saved) {
            Ok(effort) => self.reasoning_effort = effort,
            Err(_) => self.push(
                Role::System,
                format!(
                    "Saved reasoning effort `{saved}` is no longer available for {}.",
                    self.model_name
                ),
            ),
        }
    }

    fn begin_new_session(&mut self) {
        let Some(provider) = self.provider.clone() else {
            self.push(Role::System, "No provider is connected.");
            return;
        };
        if let Some(request) = self.request.take() {
            request.abort();
        }
        self.queued_request = None;
        self.streaming_message = None;
        self.request_generation = self.request_generation.wrapping_add(1);
        let request_id = self.request_generation;
        let sender = self.events_tx.clone();
        self.request = Some(tokio::spawn(async move {
            let event = match provider.new_session().await {
                Ok(()) => ProviderEvent::SessionStarted,
                Err(error) => {
                    ProviderEvent::Failed(format!("could not start a new session: {error:#}"))
                }
            };
            let _ = sender.send((request_id, event));
        }));
        self.request_uses_provider = true;
    }

    fn begin_exec(&mut self, cwd: PathBuf, command: String) {
        if self.request.is_some() {
            self.push(
                Role::System,
                "A response or command is already running. Press Esc to interrupt it first.",
            );
            return;
        }
        let shown_cwd = cwd.strip_prefix(&self.cwd).map_or_else(
            |_| cwd.display().to_string(),
            |path| {
                if path.as_os_str().is_empty() {
                    ".".to_owned()
                } else {
                    path.display().to_string()
                }
            },
        );
        self.push(Role::You, format!("/exec --cwd {shown_cwd} {command}"));
        self.streaming_message = None;
        self.tool_summaries.clear();
        self.begin_audit_session();
        self.request_generation = self.request_generation.wrapping_add(1);
        let request_id = self.request_generation;
        let sender = self.events_tx.clone();
        let workspace = self.cwd.clone();
        let call_id = format!("exec-{request_id}");
        let mut audit = crate::tools::audit_context(
            &call_id,
            &workspace,
            "run_command",
            &serde_json::json!({
                "program": "sh",
                "args": ["-c", command],
                "cwd": cwd,
            }),
        );
        audit.tool = "exec".to_owned();
        let undo = self.undo.clone();
        crate::undo::arm(&workspace, undo);
        self.request_uses_provider = false;
        self.request = Some(tokio::spawn(async move {
            let _ = sender.send((request_id, ProviderEvent::ToolStarted(audit)));
            let progress = crate::providers::tool_progress_reporter(
                request_id,
                call_id.clone(),
                sender.clone(),
            );
            let result =
                crate::tools::execute_shell(&workspace, &cwd, command, Some(progress)).await;
            let (success, output, failure) = match result {
                Ok(output) => (true, output, None),
                Err(error) => {
                    let failure = crate::tools::audit_failure(&error);
                    (false, format!("{error:#}"), Some(failure))
                }
            };
            let _ = sender.send((
                request_id,
                ProviderEvent::ToolFinished {
                    call_id,
                    tool: "exec".to_owned(),
                    success,
                    output,
                    failure,
                },
            ));
            let _ = sender.send((request_id, ProviderEvent::Completed));
        }));
    }

    fn begin_request(&mut self, content: String) {
        self.push(Role::You, content.clone());
        let Some(provider) = self.provider.clone() else {
            self.push(Role::System, "No provider is connected.");
            return;
        };
        self.streaming_message = None;
        self.tool_summaries.clear();
        self.begin_audit_session();
        self.request_generation = self.request_generation.wrapping_add(1);
        let request_id = self.request_generation;
        let sender = self.events_tx.clone();
        crate::undo::arm(&self.cwd, self.undo.clone());
        self.request = Some(tokio::spawn(async move {
            if let Err(error) = provider.stream(content, request_id, sender.clone()).await {
                let _ = sender.send((request_id, ProviderEvent::Failed(format!("{error:#}"))));
            }
        }));
        self.request_uses_provider = true;
    }

    fn begin_connect(&mut self, provider: ProviderKind) {
        if let Some(connection) = self.connection.take() {
            connection.abort();
        }
        if let Some(request) = self.request.take() {
            request.abort();
        }
        self.streaming_message = None;
        self.session_tokens = 0;
        self.provider = None;
        self.active_provider = Some(provider);
        self.provider_name = provider.display_name().to_owned();
        self.models.clear();
        self.model_id = "none".to_owned();
        self.model_name = "connecting…".to_owned();
        self.reasoning_effort = "none".to_owned();
        self.reasoning_options.clear();
        self.connection_generation = self.connection_generation.wrapping_add(1);
        let generation = self.connection_generation;
        let cwd = self.cwd.clone();
        let sender = self.connection_tx.clone();
        let saved_model = match load_model(provider) {
            Ok(model) => model,
            Err(error) => {
                self.push(
                    Role::System,
                    format!("Could not load the saved model: {error:#}"),
                );
                None
            }
        };
        self.push(
            Role::System,
            format!(
                "Connecting to {}… {}",
                provider.display_name(),
                provider.connect_hint()
            ),
        );
        self.connection = Some(tokio::spawn(async move {
            let event = match Provider::connect(provider, &cwd).await {
                Ok(provider) => {
                    let warning = saved_model.and_then(|model| {
                        provider.select_model(&model).err().map(|_| {
                            format!(
                                "Saved model `{model}` is no longer available; using the provider default."
                            )
                        })
                    });
                    ConnectionEvent::Connected(generation, provider, warning)
                }
                Err(error) => ConnectionEvent::Failed(generation, format!("{error:#}")),
            };
            let _ = sender.send(event);
        }));
    }

    fn handle_connection_event(&mut self, event: ConnectionEvent) {
        let generation = match &event {
            ConnectionEvent::Connected(generation, _, _)
            | ConnectionEvent::Failed(generation, _) => *generation,
        };
        if generation != self.connection_generation {
            return;
        }
        self.connection = None;
        match event {
            ConnectionEvent::Connected(_, provider, warning) => {
                let save_error = self
                    .active_provider
                    .and_then(|kind| save_default_provider(kind).err());
                let model = provider.model();
                self.models = provider.models().to_vec();
                self.model_id = model.id;
                self.model_name = model.display_name;
                self.reasoning_effort = model.default_reasoning_effort;
                self.reasoning_options = model.reasoning_efforts;
                self.provider = Some(provider);
                self.restore_reasoning_for_model();
                self.push(
                    Role::Success,
                    "Provider connected. Type /help for commands.",
                );
                if let Some(warning) = warning {
                    self.push(Role::System, warning);
                }
                if let Some(error) = save_error {
                    self.push(
                        Role::System,
                        format!("Could not save the default provider: {error:#}"),
                    );
                }
            }
            ConnectionEvent::Failed(_, error) => {
                self.provider = None;
                self.model_id = "none".to_owned();
                self.model_name = "connection failed".to_owned();
                self.reasoning_effort = "none".to_owned();
                self.reasoning_options.clear();
                self.push(Role::System, format!("Provider connection failed: {error}"));
                self.open_provider_picker();
            }
        }
    }

    fn handle_provider_event(&mut self, request_id: u64, event: ProviderEvent) {
        if request_id != self.request_generation {
            return;
        }
        match event {
            ProviderEvent::Delta(delta) => {
                if self.streaming_message.is_none() {
                    self.push(Role::Assistant, String::new());
                    self.streaming_message = Some(self.messages.len() - 1);
                }
                if let Some(index) = self.streaming_message
                    && let Some(message) = self.messages.get_mut(index)
                {
                    message.append(&delta);
                }
            }
            ProviderEvent::Usage(tokens) => {
                self.session_tokens = tokens;
                crate::control_plane::record_usage(&mut self.audit_session, tokens);
            }
            ProviderEvent::ToolStarted(context) => {
                self.audit_tool_started(&context);
                let tool = context.tool.clone();
                self.streaming_message = None;
                if Self::compact_tool(&tool) {
                    self.start_compact_tool(&tool);
                } else {
                    self.start_running_tool(context);
                }
            }
            ProviderEvent::ToolProgress {
                call_id,
                latest_line,
                line_count,
            } => {
                if let Some(running) = self.running_tools.get_mut(&call_id) {
                    running.latest_line = latest_line;
                    running.line_count = line_count;
                }
                self.refresh_running_tool(&call_id);
            }
            ProviderEvent::ToolFinished {
                call_id,
                tool,
                success,
                output,
                failure,
            } => {
                self.audit_tool_finished(&call_id, &tool, success, &output, failure.as_ref());
                self.streaming_message = None;
                if Self::compact_tool(&tool) && self.finish_compact_tool(&tool, success, &output) {
                    return;
                }
                let running = self.running_tools.remove(&call_id);
                let title = running
                    .as_ref()
                    .map(|running| running.title.clone())
                    .unwrap_or_else(|| Self::tool_title(&tool, &[]));
                let elapsed = running.as_ref().map(|running| running.started_at.elapsed());
                self.last_tool_output = Some((title.clone(), output.clone()));
                if let Some(running) = running {
                    let role = if success { Role::Success } else { Role::Error };
                    let status = if success { "Completed" } else { "Failed" };
                    let content = Self::completed_tool_content(
                        status,
                        &title,
                        elapsed.unwrap_or_default(),
                        &output,
                        failure.as_ref(),
                    );
                    if let Some(message) = self.messages.get_mut(running.message_index) {
                        message.replace(role, content);
                    }
                } else if let Some(message) = self
                    .messages
                    .iter_mut()
                    .rev()
                    .find(|message| message.role == Role::Running)
                {
                    message.set_role(Role::System);
                }
                if tool == "run_command" || tool == "exec" {
                    if success {
                        self.begin_changes_refresh();
                    }
                    return;
                }
                if success {
                    if matches!(
                        tool.as_str(),
                        "write_file" | "edit_file" | "delete_file" | "exec"
                    ) && !output.trim().is_empty()
                    {
                        let label = if tool == "exec" {
                            "Command completed".to_owned()
                        } else {
                            format!("Completed {tool}")
                        };
                        self.push(Role::Success, format!("{label}\n{}", output.trim_end()));
                    } else {
                        self.push(Role::Success, format!("Completed {tool}"));
                    }
                } else {
                    let label = if tool == "exec" {
                        "Command failed".to_owned()
                    } else {
                        format!("Failed {tool}")
                    };
                    self.push(Role::Error, format!("{label}\n{}", output.trim_end()));
                }
                if success
                    && matches!(
                        tool.as_str(),
                        "write_file" | "edit_file" | "delete_file" | "exec"
                    )
                {
                    self.begin_changes_refresh();
                }
            }
            ProviderEvent::Completed => {
                self.finish_audit_session(true);
                self.streaming_message = None;
                self.request = None;
                self.request_uses_provider = false;
                self.begin_changes_refresh();
                self.start_queued_request();
            }
            ProviderEvent::Interrupted => {
                self.finish_audit_session(false);
                let provider_request = self.request_uses_provider;
                self.finish_running_tools("Interrupted");
                self.streaming_message = None;
                if let Some(request) = self.request.take() {
                    request.abort();
                }
                self.request_generation = self.request_generation.wrapping_add(1);
                self.request_uses_provider = false;
                self.push(
                    Role::System,
                    if provider_request {
                        "Response interrupted."
                    } else {
                        "Command interrupted."
                    },
                );
                self.start_queued_request();
            }
            ProviderEvent::SessionStarted => {
                self.messages.clear();
                self.streaming_message = None;
                self.tool_summaries.clear();
                self.running_tools.clear();
                self.last_tool_output = None;
                self.session_tokens = 0;
                self.request = None;
                self.request_uses_provider = false;
                self.push(Role::System, "New session started.");
            }
            ProviderEvent::Failed(error) => {
                self.finish_audit_session(false);
                self.finish_running_tools("Stopped");
                if let Some(index) = self.streaming_message.take() {
                    if let Some(message) = self.messages.get_mut(index) {
                        message.replace(Role::Error, format!("Request failed: {error}"));
                    }
                } else {
                    self.push(Role::System, format!("Request failed: {error}"));
                }
                self.request = None;
                self.request_uses_provider = false;
                self.start_queued_request();
            }
        }
    }

    fn start_queued_request(&mut self) {
        if let Some(content) = self.queued_request.take() {
            self.begin_request(content);
        }
    }

    fn begin_audit_session(&mut self) {
        let provider = self.active_provider.map(|provider| provider.id());
        let model = (self.model_id != "none").then_some(self.model_id.as_str());
        let reasoning = (self.reasoning_effort != "none").then_some(self.reasoning_effort.as_str());
        match crate::control_plane::start_session(&self.cwd, provider, model, reasoning) {
            Ok(session) => self.audit_session = session,
            Err(error) => self.push(
                Role::System,
                format!("Could not start the local audit session: {error:#}"),
            ),
        }
    }

    fn finish_audit_session(&mut self, success: bool) {
        if let Some(session) = self.audit_session.take()
            && let Err(error) = crate::control_plane::finish_session(session, success)
        {
            self.push(
                Role::System,
                format!("Could not finalize the local audit session: {error:#}"),
            );
        }
    }

    fn audit_tool_started(&mut self, context: &ToolContext) {
        let result = crate::control_plane::tool_started(&mut self.audit_session, context);
        self.handle_audit_result(result);
    }

    fn audit_tool_finished(
        &mut self,
        call_id: &str,
        tool: &str,
        success: bool,
        output: &str,
        failure: Option<&crate::tools::ToolFailure>,
    ) {
        let result = crate::control_plane::tool_finished(
            &mut self.audit_session,
            call_id,
            tool,
            success,
            output,
            failure,
        );
        self.handle_audit_result(result);
    }

    fn handle_audit_result(&mut self, result: Option<anyhow::Result<()>>) {
        if let Some(Err(error)) = result {
            self.audit_session = None;
            self.push(
                Role::System,
                format!("Could not record the local audit event: {error:#}"),
            );
        }
    }

    fn compact_tool(tool: &str) -> bool {
        matches!(tool, "read_file" | "list_directory")
    }

    fn start_running_tool(&mut self, context: ToolContext) {
        let title = Self::tool_title(&context.tool, &context.argv_display);
        let message_index = self.messages.len();
        let running = RunningTool {
            message_index,
            title,
            started_at: Instant::now(),
            timeout_seconds: context.timeout_seconds,
            latest_line: String::new(),
            line_count: 0,
        };
        let content = Self::running_tool_content(&running, self.conversation_area.width);
        self.push(Role::Running, content);
        self.running_tools.insert(context.call_id, running);
    }

    fn refresh_running_tools(&mut self) {
        let call_ids = self.running_tools.keys().cloned().collect::<Vec<_>>();
        for call_id in call_ids {
            self.refresh_running_tool(&call_id);
        }
    }

    fn finish_running_tools(&mut self, status: &str) {
        for (_, running) in self.running_tools.drain() {
            let content = format!(
                "{status} {} · {}",
                running.title,
                Self::format_duration(running.started_at.elapsed())
            );
            if let Some(message) = self.messages.get_mut(running.message_index) {
                message.replace(Role::Error, content);
            }
        }
    }

    fn refresh_running_tool(&mut self, call_id: &str) {
        let Some(running) = self.running_tools.get(call_id) else {
            return;
        };
        let content = Self::running_tool_content(running, self.conversation_area.width);
        if let Some(message) = self.messages.get_mut(running.message_index) {
            message.replace(Role::Running, content);
        }
    }

    fn tool_title(tool: &str, argv: &[String]) -> String {
        if !argv.is_empty() {
            return argv.join(" ");
        }
        if tool == "exec" || tool == "run_command" {
            "command".to_owned()
        } else {
            tool.to_owned()
        }
    }

    fn running_tool_content(running: &RunningTool, width: u16) -> String {
        let elapsed = running.started_at.elapsed();
        let timing = running.timeout_seconds.map_or_else(
            || Self::format_duration(elapsed),
            |timeout| {
                format!(
                    "{} / {}",
                    Self::format_duration(elapsed),
                    Self::format_duration(Duration::from_secs(timeout))
                )
            },
        );
        let title = format!("Running {} in nono…", running.title);
        let mut content = Self::right_aligned_status(&title, &timing, width);
        if !running.latest_line.is_empty() {
            content.push_str("\n  ");
            content.push_str(&running.latest_line);
        }
        if running.line_count > 0 {
            content.push_str(&format!(
                "\n  {} output lines · Esc cancel",
                running.line_count
            ));
        }
        content
    }

    fn completed_tool_content(
        status: &str,
        title: &str,
        elapsed: Duration,
        output: &str,
        failure: Option<&crate::tools::ToolFailure>,
    ) -> String {
        let mut content = format!("{status} {title} · {}", Self::format_duration(elapsed));
        if let Some(summary) = failure.and_then(Self::failure_summary) {
            content.push_str("\n  ");
            content.push_str(summary);
        }
        let (detail, hidden) = Self::compact_tool_output(output, 10);
        if !detail.is_empty() {
            content.push('\n');
            content.push_str(&detail);
        }
        if hidden > 0 {
            content.push_str(&format!(
                "\n… {hidden} lines hidden · /details for full output"
            ));
        }
        content
    }

    fn failure_summary(failure: &crate::tools::ToolFailure) -> Option<&'static str> {
        match failure.reason {
            "sandbox_denied" | "workspace_denied" => {
                Some("The configured sandbox policy denied this operation.")
            }
            "timeout" => Some("The command exceeded its configured deadline."),
            "tls_error" => Some("TLS setup or certificate verification failed."),
            "network_error" => Some("A network connection or download failed."),
            "command_not_found" => Some("The executable was not found in the sandbox PATH."),
            "command_not_executable" => Some("The executable could not be started."),
            _ => None,
        }
    }

    fn compact_tool_output(output: &str, visible: usize) -> (String, usize) {
        let lines = output.lines().collect::<Vec<_>>();
        let hidden = lines.len().saturating_sub(visible);
        let detail = lines
            .iter()
            .skip(hidden)
            .map(|line| line.chars().take(240).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        (detail, hidden)
    }

    fn format_duration(duration: Duration) -> String {
        let seconds = duration.as_secs();
        format!("{:02}:{:02}", seconds / 60, seconds % 60)
    }

    fn right_aligned_status(title: &str, timing: &str, width: u16) -> String {
        let width = usize::from(width);
        let padding = width.saturating_sub(title.chars().count() + timing.chars().count());
        if padding < 2 {
            format!("{title} · {timing}")
        } else {
            format!("{title}{}{timing}", " ".repeat(padding))
        }
    }

    fn start_compact_tool(&mut self, tool: &str) {
        if let Some(summary) = self.tool_summaries.get_mut(tool) {
            summary.started += 1;
            if let Some(message) = self.messages.get_mut(summary.message_index) {
                message.replace(Role::Running, Self::running_summary(tool, summary.started));
            }
            return;
        }

        let message_index = self.messages.len();
        self.push(Role::Running, Self::running_summary(tool, 1));
        self.tool_summaries.insert(
            tool.to_owned(),
            ToolSummary {
                message_index,
                started: 1,
                succeeded: 0,
                failed: 0,
                last_error: String::new(),
            },
        );
    }

    fn finish_compact_tool(&mut self, tool: &str, success: bool, output: &str) -> bool {
        let Some(summary) = self.tool_summaries.get_mut(tool) else {
            return false;
        };
        if success {
            summary.succeeded += 1;
        } else {
            summary.failed += 1;
            summary.last_error = output.trim_end().to_owned();
        }

        let role = if summary.failed == 0 {
            Role::Success
        } else {
            Role::Error
        };
        let content = Self::completed_summary(tool, summary);
        if let Some(message) = self.messages.get_mut(summary.message_index) {
            message.replace(role, content);
        }
        true
    }

    fn running_summary(tool: &str, count: usize) -> String {
        let action = match tool {
            "read_file" => "Reading files in nono…",
            "list_directory" => "Listing directories in nono…",
            _ => return format!("Running {tool} in nono…"),
        };
        if count == 1 {
            action.to_owned()
        } else {
            format!("{action} ({count})")
        }
    }

    fn completed_summary(tool: &str, summary: &ToolSummary) -> String {
        let noun = match tool {
            "read_file" => "file",
            "list_directory" => "director",
            _ => "call",
        };
        let succeeded = if tool == "list_directory" {
            format!(
                "Listed {} director{}",
                summary.succeeded,
                if summary.succeeded == 1 { "y" } else { "ies" }
            )
        } else {
            format!(
                "Read {} {noun}{}",
                summary.succeeded,
                if summary.succeeded == 1 { "" } else { "s" }
            )
        };
        if summary.failed == 0 {
            succeeded
        } else {
            format!(
                "{succeeded} · {} failed\n{}",
                summary.failed, summary.last_error
            )
        }
    }

    fn push(&mut self, role: Role, content: impl Into<String>) {
        self.messages.push(Message::new(role, content));
    }

    fn apply_markdown(&mut self, rendered: MarkdownResult) {
        let Some(message) = self.messages.get_mut(rendered.message_index) else {
            return;
        };
        if message.markdown_job != Some((rendered.width, rendered.job)) {
            return;
        }
        message.rendered_markdown = Some((rendered.width, rendered.lines));
        message.markdown_job = None;
    }
}

fn render_markdown(
    sender: mpsc::UnboundedSender<MarkdownResult>,
    message_index: usize,
    width: u16,
    job: u64,
    content: String,
) -> Option<Vec<Line<'static>>> {
    if tokio::runtime::Handle::try_current().is_err() {
        return Some(markdown_lines(&content, width));
    }
    tokio::task::spawn_blocking(move || {
        let lines = markdown_lines(&content, width);
        let _ = sender.send(MarkdownResult {
            message_index,
            width,
            job,
            lines,
        });
    });
    None
}

fn is_local_command(content: &str) -> bool {
    SLASH_COMMANDS.iter().any(|command| {
        content == command.name || (command.name == "/exec" && content.starts_with("/exec "))
    })
}

fn screen_to_document(area: Rect, scroll: u16, column: u16, row: u16) -> (u16, u16) {
    let column = column
        .clamp(area.x, area.right().saturating_sub(1))
        .saturating_sub(area.x);
    let row = row
        .clamp(area.y, area.bottom().saturating_sub(1))
        .saturating_sub(area.y)
        .saturating_add(scroll);
    (column, row)
}

fn ordered_selection(first: (u16, u16), second: (u16, u16)) -> ((u16, u16), (u16, u16)) {
    if (first.1, first.0) <= (second.1, second.0) {
        (first, second)
    } else {
        (second, first)
    }
}

fn selection_columns(area: Rect, start: (u16, u16), end: (u16, u16), row: u16) -> (u16, u16) {
    let first = if row == start.1 { start.0 } else { area.x };
    let last = if row == end.1 {
        end.0
    } else {
        area.right().saturating_sub(1)
    };
    (first, last)
}

fn project_selection(
    area: Rect,
    scroll: u16,
    start: (u16, u16),
    end: (u16, u16),
) -> Option<((u16, u16), (u16, u16))> {
    let visible_end = scroll.saturating_add(area.height.saturating_sub(1));
    if end.1 < scroll || start.1 > visible_end {
        return None;
    }
    let first_row = start.1.max(scroll);
    let last_row = end.1.min(visible_end);
    let right = area.width.saturating_sub(1);
    let first_column = if first_row == start.1 {
        start.0.min(right)
    } else {
        0
    };
    let last_column = if last_row == end.1 {
        end.0.min(right)
    } else {
        right
    };
    Some((
        (
            area.x.saturating_add(first_column),
            area.y.saturating_add(first_row - scroll),
        ),
        (
            area.x.saturating_add(last_column),
            area.y.saturating_add(last_row - scroll),
        ),
    ))
}

fn highlight_selection(
    buffer: &mut ratatui::buffer::Buffer,
    area: Rect,
    start: (u16, u16),
    end: (u16, u16),
) {
    for row in start.1..=end.1 {
        let (first, last) = selection_columns(area, start, end, row);
        for column in first..=last {
            let cell = &mut buffer[(column, row)];
            cell.set_style(cell.style().add_modifier(Modifier::REVERSED));
        }
    }
}

fn selected_text(
    buffer: &ratatui::buffer::Buffer,
    area: Rect,
    start: (u16, u16),
    end: (u16, u16),
) -> String {
    let mut output = String::new();
    for row in start.1..=end.1 {
        let (first, last) = selection_columns(area, start, end, row);
        let line_start = output.len();
        for column in first..=last {
            output.push_str(buffer[(column, row)].symbol());
        }
        output.truncate(line_start + output[line_start..].trim_end().len());
        if row != end.1 {
            output.push('\n');
        }
    }
    output.trim_end().to_owned()
}

fn selected_document_text(
    lines: &[Line<'_>],
    width: u16,
    start: (u16, u16),
    end: (u16, u16),
) -> String {
    if width == 0 {
        return String::new();
    }
    let height = end.1.saturating_sub(start.1).saturating_add(1);
    let area = Rect::new(0, 0, width, height);
    let mut buffer = ratatui::buffer::Buffer::empty(area);
    Paragraph::new(lines.to_vec())
        .wrap(Wrap { trim: false })
        .scroll((start.1, 0))
        .render(area, &mut buffer);
    selected_text(
        &buffer,
        area,
        (start.0.min(width - 1), 0),
        (end.0.min(width - 1), height - 1),
    )
}

fn copy_to_clipboard(text: &str) -> Result<(), String> {
    use std::io::Write as _;

    let mut sequence = String::new();
    CopyToClipboard::to_clipboard_from(text)
        .write_ansi(&mut sequence)
        .map_err(|error| error.to_string())?;
    let mut stdout = std::io::stdout().lock();
    stdout
        .write_all(sequence.as_bytes())
        .and_then(|()| stdout.flush())
        .map_err(|error| error.to_string())
}

fn markdown_lines(content: &str, width: u16) -> Vec<Line<'static>> {
    let options = MarkdownOptions::new(NonautMarkdown).table_width(width);
    from_str_with_options(content, &options)
        .lines
        .into_iter()
        .map(own_line)
        .collect()
}

fn own_line(line: Line<'_>) -> Line<'static> {
    Line {
        style: line.style,
        alignment: line.alignment,
        spans: line
            .spans
            .into_iter()
            .map(|span| Span::styled(span.content.into_owned(), span.style))
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn app_with_input(value: &str) -> App {
        let mut app = App::new_shell(PathBuf::from("."));
        app.input.insert_str(value);
        app
    }
    #[test]
    fn prompt_grows_with_newlines_between_min_and_max() {
        fn prompt_rows(value: &str) -> u16 {
            let mut app = app_with_input(value);
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 40)).unwrap();
            terminal.draw(|frame| app.render(frame)).unwrap();
            let buffer = terminal.backend().buffer();
            let top = (0..40).find(|&y| buffer[(0, y)].symbol() == "╭").unwrap();
            40 - top
        }
        assert_eq!(prompt_rows("one line"), 4);
        assert_eq!(prompt_rows("a\nb\nc\nd"), 6);
        assert_eq!(prompt_rows(&"line\n".repeat(20)), 12);
    }

    #[test]
    fn theme_flag_selects_the_palette() {
        select_theme(Theme::Light);
        assert_eq!(
            palette().background,
            ratatui::style::Color::Rgb(255, 255, 255)
        );
        select_theme(Theme::Dark);
        assert_eq!(palette().background, ratatui::style::Color::Rgb(0, 0, 0));
    }

    #[test]
    fn theme_command_opens_picker_on_the_current_selection() {
        let mut app = app_with_input("/theme");

        app.submit();
        let picker = app.theme_picker.as_ref().unwrap();
        assert_eq!(Theme::ALL[picker.selected], theme());

        app.handle_theme_picker_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

        assert!(app.theme_picker.is_none());
    }

    #[test]
    fn pasted_newlines_stay_in_the_prompt_instead_of_submitting() {
        let mut app = app_with_input("");
        app.handle_terminal_event(Event::Paste("first\r\nsecond\rthird".to_owned()));
        assert!(app.messages.is_empty());
        assert!(app.request.is_none());
        assert_eq!(app.input.lines(), ["first", "second", "third"]);
    }

    fn press(app: &mut App, code: KeyCode) {
        app.handle_terminal_event(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)));
    }

    #[test]
    fn up_and_down_browse_prompt_history_and_restore_the_draft() {
        let mut app = app_with_input("");
        for prompt in ["first prompt", "/help", "third\nprompt"] {
            app.input.insert_str(prompt);
            app.submit();
        }
        app.input.insert_str("unsent draft");
        press(&mut app, KeyCode::Up);
        assert_eq!(app.input.lines(), ["third", "prompt"]);
        press(&mut app, KeyCode::Up);
        assert_eq!(app.input.lines(), ["/help"]);
        // A recalled slash command must not swallow the next Up.
        press(&mut app, KeyCode::Up);
        assert_eq!(app.input.lines(), ["first prompt"]);
        press(&mut app, KeyCode::Up);
        assert_eq!(app.input.lines(), ["first prompt"]);
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Down);
        assert_eq!(app.input.lines(), ["third", "prompt"]);
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Down);
        assert_eq!(app.input.lines(), ["unsent draft"]);
    }

    #[test]
    fn up_moves_inside_a_multi_line_prompt_before_recalling() {
        let mut app = app_with_input("");
        app.input.insert_str("old prompt");
        app.submit();
        app.input.insert_str("line one\nline two");
        press(&mut app, KeyCode::Up);
        assert_eq!(app.input.lines(), ["line one", "line two"]);
        press(&mut app, KeyCode::Up);
        assert_eq!(app.input.lines(), ["old prompt"]);
    }

    #[test]
    fn submitting_text_adds_local_shell_messages() {
        let mut app = app_with_input("hello");
        app.submit();
        assert_eq!(app.messages.len(), 2);
        assert_eq!(app.messages[0].content, "hello");
        assert!(app.input.is_empty());
    }
    #[test]
    fn quit_command_exits() {
        let mut app = app_with_input("/quit");
        app.submit();
        assert!(app.should_quit);
    }

    #[test]
    fn short_quit_command_exits() {
        let mut app = app_with_input("/q");
        app.submit();
        assert!(app.should_quit);
    }

    #[test]
    fn tab_cycles_commands_and_wraps() {
        let mut app = app_with_input("/");
        let command_count = app.matching_commands().len();
        assert!(app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)));
        assert_eq!(app.command_selection, 1);
        assert_eq!(app.input.lines(), ["/"]);

        for _ in 1..command_count {
            app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        }
        assert_eq!(app.command_selection, 0);
    }

    #[tokio::test]
    async fn alt_number_switches_between_agent_and_changes_views() {
        let mut app = app_with_input("draft");
        app.changes_file_count = 1;

        app.handle_terminal_event(Event::Key(KeyEvent::new(
            KeyCode::Char('2'),
            KeyModifiers::ALT,
        )));
        assert_eq!(app.workspace_view, WorkspaceView::Changes);
        app.handle_terminal_event(Event::Key(KeyEvent::new(
            KeyCode::Char('x'),
            KeyModifiers::NONE,
        )));
        assert_eq!(app.input.lines(), ["draft"]);

        app.handle_terminal_event(Event::Key(KeyEvent::new(
            KeyCode::Char('1'),
            KeyModifiers::ALT,
        )));
        assert_eq!(app.workspace_view, WorkspaceView::Agent);
        assert_eq!(app.input.lines(), ["draft"]);
    }

    #[test]
    fn changes_search_moves_to_matching_diff_lines() {
        let mut app = app_with_input("");
        app.changes_diff = "first\nneedle one\nmiddle\nneedle two\n".to_owned();
        app.changes_query = "needle".to_owned();

        app.find_change_match(true);
        assert_eq!(app.changes_match, Some(1));
        assert_eq!(app.changes_scroll, 1);
        app.find_change_match(true);
        assert_eq!(app.changes_match, Some(3));
        app.find_change_match(false);
        assert_eq!(app.changes_match, Some(1));
    }

    #[test]
    fn header_shows_both_workspaces_and_change_count() {
        let mut app = app_with_input("");
        app.changes_file_count = 4;
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();

        terminal.draw(|frame| app.render(frame)).unwrap();

        let visible = terminal.backend().buffer().content().iter().fold(
            String::new(),
            |mut rendered, cell| {
                rendered.push_str(cell.symbol());
                rendered
            },
        );
        assert!(visible.contains("1. Agent"));
        assert!(visible.contains("2. Review (4)"));
    }

    #[test]
    fn clean_workspace_hides_changes_tab() {
        let mut app = app_with_input("");
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();

        terminal.draw(|frame| app.render(frame)).unwrap();

        let visible = terminal.backend().buffer().content().iter().fold(
            String::new(),
            |mut rendered, cell| {
                rendered.push_str(cell.symbol());
                rendered
            },
        );
        assert!(visible.contains("1. Agent"));
        assert!(!visible.contains("2. Review"));
    }

    #[test]
    fn tab_and_backtab_jump_between_changed_files() {
        let mut app = app_with_input("");
        app.changes_diff = concat!(
            "diff --git a/one.rs b/one.rs\n",
            "--- a/one.rs\n",
            "+++ b/one.rs\n",
            "@@ -1 +1 @@\n",
            "-old\n",
            "+new\n",
            "diff --git a/two.rs b/two.rs\n",
            "--- a/two.rs\n",
            "+++ b/two.rs\n",
            "@@ -1 +1 @@\n",
            "-before\n",
            "+after\n",
        )
        .to_owned();

        app.jump_change_file(true);
        assert_eq!(app.changes_scroll, 8);
        app.jump_change_file(true);
        assert_eq!(app.changes_scroll, 1);
        app.jump_change_file(false);
        assert_eq!(app.changes_scroll, 8);
    }

    #[test]
    fn enter_runs_the_selected_slash_command() {
        let mut app = app_with_input("/pro");
        assert!(app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
        assert!(app.provider_picker.is_some());
        assert!(app.input.is_empty());
    }

    #[test]
    fn slash_command_menu_hides_exit_aliases() {
        let app = app_with_input("/qui");
        assert!(app.matching_commands().is_empty());
    }

    #[test]
    fn exit_alias_still_exits() {
        let mut app = app_with_input("/exit");
        app.submit();
        assert!(app.should_quit);
    }

    #[test]
    fn help_is_generated_from_the_command_registry() {
        let help = App::command_help();
        assert!(help.contains("/provider"));
        assert!(help.contains("/reasoning"));
        assert!(help.contains("/exec"));
        assert!(help.contains("/q"));
        assert!(help.contains("/detach"));
        assert!(!help.lines().any(|line| line.starts_with("/quit")));
    }

    #[test]
    fn shift_enter_inserts_newline() {
        let mut app = app_with_input("first line");
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT));
        app.input.insert_str("second line");
        assert_eq!(app.input.lines(), ["first line", "second line"]);
    }

    #[test]
    fn usage_event_updates_session_tokens() {
        let mut app = app_with_input("");
        app.handle_provider_event(0, ProviderEvent::Usage(12_345));
        assert_eq!(app.session_tokens, 12_345);
    }

    #[test]
    fn tool_results_use_success_and_error_roles() {
        let mut app = app_with_input("");
        app.handle_provider_event(
            0,
            ProviderEvent::ToolFinished {
                call_id: "read-1".to_owned(),
                tool: "read_file".to_owned(),
                success: true,
                output: "contents".to_owned(),
                failure: None,
            },
        );
        assert_eq!(app.messages.last().unwrap().role, Role::Success);

        app.handle_provider_event(
            0,
            ProviderEvent::ToolFinished {
                call_id: "read-2".to_owned(),
                tool: "read_file".to_owned(),
                success: false,
                output: "denied".to_owned(),
                failure: Some(crate::tools::ToolFailure {
                    reason: "sandbox_denied",
                    exit_code: Some(1),
                }),
            },
        );
        assert_eq!(app.messages.last().unwrap().role, Role::Error);
    }

    #[test]
    fn running_tool_stops_animating_when_it_finishes() {
        let mut app = app_with_input("");
        app.handle_provider_event(
            0,
            ProviderEvent::ToolStarted(ToolContext::minimal("read-1", "read_file")),
        );
        assert_eq!(app.messages.last().unwrap().role, Role::Running);

        app.handle_provider_event(
            0,
            ProviderEvent::ToolFinished {
                call_id: "read-1".to_owned(),
                tool: "read_file".to_owned(),
                success: true,
                output: String::new(),
                failure: None,
            },
        );

        assert_eq!(app.messages.len(), 1);
        assert_eq!(app.messages[0].role, Role::Success);
        assert_eq!(app.messages[0].content, "Read 1 file");
    }

    #[test]
    fn command_cards_show_identity_progress_timing_and_fold_output() {
        let mut app = app_with_input("");
        app.conversation_area.width = 80;
        let mut context = ToolContext::minimal("command-1", "run_command");
        context.argv_display = vec!["make".to_owned(), "check".to_owned()];
        context.timeout_seconds = Some(120);
        app.handle_provider_event(0, ProviderEvent::ToolStarted(context));
        assert!(
            app.messages[0]
                .content
                .contains("Running make check in nono…")
        );
        assert!(app.messages[0].content.contains("00:00 / 02:00"));

        app.handle_provider_event(
            0,
            ProviderEvent::ToolProgress {
                call_id: "command-1".to_owned(),
                latest_line: "Checking nonaut v0.0.3".to_owned(),
                line_count: 42,
            },
        );
        assert!(app.messages[0].content.contains("Checking nonaut v0.0.3"));
        assert!(app.messages[0].content.contains("42 output lines"));

        let output = (1..=20)
            .map(|line| format!("diagnostic {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        app.handle_provider_event(
            0,
            ProviderEvent::ToolFinished {
                call_id: "command-1".to_owned(),
                tool: "run_command".to_owned(),
                success: false,
                output: output.clone(),
                failure: Some(crate::tools::ToolFailure {
                    reason: "process_exit",
                    exit_code: Some(1),
                }),
            },
        );
        assert_eq!(app.messages[0].role, Role::Error);
        assert!(
            app.messages[0]
                .content
                .contains("Failed make check · 00:00")
        );
        assert!(app.messages[0].content.contains("10 lines hidden"));
        assert!(!app.messages[0].content.contains("diagnostic 1\n"));

        app.input.insert_str("/details");
        app.submit();
        assert!(app.messages.last().unwrap().content.contains(&output));
    }

    #[test]
    fn repeated_reads_share_one_counted_status() {
        let mut app = app_with_input("");
        for index in 0..3 {
            let call_id = format!("read-{index}");
            app.handle_provider_event(
                0,
                ProviderEvent::ToolStarted(ToolContext::minimal(&call_id, "read_file")),
            );
            app.handle_provider_event(
                0,
                ProviderEvent::ToolFinished {
                    call_id,
                    tool: "read_file".to_owned(),
                    success: true,
                    output: String::new(),
                    failure: None,
                },
            );
        }

        assert_eq!(app.messages.len(), 1);
        assert_eq!(app.messages[0].role, Role::Success);
        assert_eq!(app.messages[0].content, "Read 3 files");
    }

    #[test]
    fn stale_provider_events_are_ignored() {
        let mut app = app_with_input("");
        app.request_generation = 2;
        app.handle_provider_event(1, ProviderEvent::Usage(12_345));
        assert_eq!(app.session_tokens, 0);
    }

    #[test]
    fn interrupted_response_adds_status_message() {
        let mut app = app_with_input("");
        app.request_uses_provider = true;
        app.handle_provider_event(0, ProviderEvent::Interrupted);
        assert_eq!(
            app.messages.last().unwrap().content,
            "Response interrupted."
        );
    }

    #[tokio::test]
    async fn input_remains_editable_while_streaming() {
        let mut app = app_with_input("");
        app.request = Some(tokio::spawn(std::future::pending()));
        app.handle_terminal_event(Event::Key(KeyEvent::new(
            KeyCode::Char('x'),
            KeyModifiers::NONE,
        )));
        assert_eq!(app.input.lines(), ["x"]);
        app.request.take().unwrap().abort();
    }

    #[tokio::test]
    async fn prompts_wait_for_the_provider_to_connect() {
        let mut app = app_with_input("start immediately");
        app.connection = Some(tokio::spawn(std::future::pending()));

        app.submit();

        assert_eq!(app.input.lines(), ["start immediately"]);
        assert!(app.queued_request.is_none());
        assert!(app.request.is_none());
        assert!(app.messages.last().unwrap().content.contains("not sent"));
        app.connection.take().unwrap().abort();
    }

    #[tokio::test]
    async fn local_commands_work_while_the_provider_connects() {
        let mut app = app_with_input("/theme");
        app.connection = Some(tokio::spawn(std::future::pending()));

        app.submit();

        assert!(app.theme_picker.is_some());
        assert!(app.input.is_empty());
        app.connection.take().unwrap().abort();
    }

    #[test]
    fn conversation_scroll_stops_and_resumes_following_output() {
        let mut app = App::new_shell(PathBuf::from("."));
        app.conversation_scroll = 20;
        app.conversation_max_scroll = 20;

        app.scroll_conversation_up(3);
        assert_eq!(app.conversation_scroll, 17);
        assert!(!app.follow_output);

        app.scroll_conversation_down(3);
        assert_eq!(app.conversation_scroll, 20);
        assert!(app.follow_output);
    }

    #[test]
    fn mouse_wheel_scrolls_conversation_history() {
        let mut app = App::new_shell(PathBuf::from("."));
        app.conversation_area = Rect::new(0, 0, 80, 20);
        app.conversation_scroll = 20;
        app.conversation_max_scroll = 20;

        app.handle_conversation_mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 10,
            row: 10,
            modifiers: KeyModifiers::NONE,
        });

        assert_eq!(app.conversation_scroll, 17);
        assert!(!app.follow_output);
    }

    #[test]
    fn conversation_selection_extracts_and_highlights_visible_text() {
        let area = Rect::new(0, 0, 10, 2);
        let mut buffer = ratatui::buffer::Buffer::empty(area);
        buffer.set_string(0, 0, "alpha", Style::default());
        buffer.set_string(0, 1, "beta", Style::default());
        let start = (1, 0);
        let end = (2, 1);

        assert_eq!(selected_text(&buffer, area, start, end), "lpha\nbet");
        highlight_selection(&mut buffer, area, start, end);
        assert_eq!(
            buffer[(1, 0)].style().add_modifier(Modifier::REVERSED),
            buffer[(1, 0)].style()
        );
        assert_ne!(
            buffer[(0, 0)].style().add_modifier(Modifier::REVERSED),
            buffer[(0, 0)].style()
        );
    }

    #[test]
    fn conversation_selection_survives_scrolling_and_projects_into_view() {
        let mut app = App::new_shell(PathBuf::from("."));
        app.conversation_scroll = 20;
        app.conversation_max_scroll = 40;
        app.conversation_selection = Some(ConversationSelection {
            anchor: (2, 18),
            cursor: (4, 24),
            dragging: false,
            copy_pending: false,
        });

        app.scroll_conversation_up(3);

        let selection = app.conversation_selection.unwrap();
        assert_eq!(selection.anchor, (2, 18));
        assert_eq!(selection.cursor, (4, 24));
        assert_eq!(
            project_selection(
                Rect::new(10, 5, 20, 4),
                17,
                selection.anchor,
                selection.cursor
            ),
            Some(((12, 6), (29, 8)))
        );
    }

    #[test]
    fn dragging_selection_extends_with_scrolling_and_copies_offscreen_rows() {
        let mut app = App::new_shell(PathBuf::from("."));
        app.conversation_scroll = 10;
        app.conversation_max_scroll = 30;
        app.conversation_selection = Some(ConversationSelection {
            anchor: (1, 8),
            cursor: (2, 12),
            dragging: true,
            copy_pending: false,
        });

        app.scroll_conversation_down(3);

        assert_eq!(app.conversation_selection.unwrap().cursor, (2, 15));
        let lines = [Line::from("alpha"), Line::from("beta"), Line::from("gamma")];
        assert_eq!(
            selected_document_text(&lines, 10, (1, 0), (2, 2)),
            "lpha\nbeta\ngam"
        );
    }

    #[test]
    fn scrolling_short_history_keeps_following_output() {
        let mut app = App::new_shell(PathBuf::from("."));
        app.scroll_conversation_up(3);
        assert!(app.follow_output);
    }

    #[test]
    fn clear_command_removes_messages_from_screen() {
        let mut app = app_with_input("hello");
        app.submit();
        app.input.insert_str("/clear");
        app.submit();
        assert!(app.messages.is_empty());
    }

    #[test]
    fn markdown_code_blocks_hide_literal_fences() {
        let options = MarkdownOptions::new(NonautMarkdown);
        let rendered = from_str_with_options("```text\npermission denied\n```", &options);
        let visible = rendered.to_string();
        assert!(visible.contains("permission denied"));
        assert!(!visible.contains("```"));
    }

    #[tokio::test]
    async fn settled_markdown_renders_off_the_ui_thread_and_is_cached() {
        let mut app = App::new_shell(PathBuf::from("."));
        app.push(Role::Assistant, "**cached** response");
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 30)).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();
        assert!(app.messages[0].markdown_job.is_some());
        assert!(app.messages[0].rendered_markdown.is_none());

        let rendered = tokio::time::timeout(Duration::from_secs(2), app.markdown_rx.recv())
            .await
            .unwrap()
            .unwrap();
        app.apply_markdown(rendered);
        assert!(app.messages[0].rendered_markdown.is_some());
        terminal.draw(|frame| app.render(frame)).unwrap();
        assert!(app.messages[0].markdown_job.is_none());
    }

    #[test]
    fn provider_command_opens_picker() {
        let mut app = app_with_input("/provider");
        app.submit();
        assert!(app.provider_picker.is_some());
    }

    #[test]
    fn model_command_opens_picker_and_applies_selection() {
        let mut app = app_with_input("/model");
        app.models = vec![
            Model {
                id: "model-a".to_owned(),
                display_name: "Model A".to_owned(),
                default_reasoning_effort: "medium".to_owned(),
                reasoning_efforts: vec![ReasoningOption {
                    id: "medium".to_owned(),
                    description: "Balanced".to_owned(),
                }],
            },
            Model {
                id: "model-b".to_owned(),
                display_name: "Model B".to_owned(),
                default_reasoning_effort: "low".to_owned(),
                reasoning_efforts: vec![ReasoningOption {
                    id: "low".to_owned(),
                    description: "Fast".to_owned(),
                }],
            },
        ];
        app.model_id = "model-a".to_owned();
        app.model_name = "Model A".to_owned();

        app.submit();
        assert!(app.model_picker.is_some());
        app.handle_model_picker_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        app.handle_model_picker_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert!(app.model_picker.is_none());
        assert_eq!(app.model_id, "model-b");
        assert_eq!(app.model_name, "Model B");
        assert_eq!(app.reasoning_effort, "low");
    }

    #[test]
    fn reasoning_command_opens_picker() {
        let mut app = app_with_input("/reasoning");
        app.reasoning_effort = "medium".to_owned();
        app.reasoning_options = vec![ReasoningOption {
            id: "medium".to_owned(),
            description: "Balanced".to_owned(),
        }];

        app.submit();

        assert!(app.reasoning_picker.is_some());
    }

    #[test]
    fn new_requires_a_connected_provider() {
        let mut app = app_with_input("/new");

        app.submit();

        assert_eq!(
            app.messages.last().unwrap().content,
            "No provider is connected."
        );
    }

    #[test]
    fn exec_without_a_command_shows_usage() {
        let mut app = app_with_input("/exec");
        app.submit();
        assert_eq!(
            app.messages.last().unwrap().content,
            "Usage: /exec [--cwd DIR] <command>"
        );
    }

    #[tokio::test]
    async fn exec_does_not_require_a_provider() {
        let mut app = app_with_input("/exec printf hello");
        app.submit();
        assert!(app.request.is_some());
        assert!(!app.request_uses_provider);
        assert!(app.messages[0].content.ends_with("printf hello"));
        app.request.take().unwrap().abort();
    }

    #[tokio::test]
    async fn exec_can_be_interrupted_without_a_provider() {
        let mut app = app_with_input("/exec sleep 30");
        app.submit();
        app.interrupt_active();
        while app.request.is_some() {
            let (request_id, event) = app.events_rx.recv().await.unwrap();
            app.handle_provider_event(request_id, event);
        }
        assert_eq!(app.messages.last().unwrap().content, "Command interrupted.");
    }
}
