use std::{
    collections::HashMap,
    io,
    path::PathBuf,
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use crossterm::event::{
    Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind,
};
use futures_util::StreamExt;
use nono::undo::ChangeType;
use ratatui::{
    DefaultTerminal, Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph, Wrap},
};
use ratatui_textarea::{CursorMove, TextArea};
use tokio::{
    sync::mpsc,
    task::JoinHandle,
    time::{self, MissedTickBehavior},
};
use tui_markdown::{Options as MarkdownOptions, StyleSheet, from_str_with_options};

use crate::{
    config::{
        load_model, load_reasoning, load_theme, save_default_provider, save_model, save_reasoning,
        save_theme,
    },
    providers::{Model, Provider, ProviderEvent, ProviderKind, ReasoningOption},
    undo::Undo,
};

struct Palette {
    background: Color,
    surface: Color,
    foreground: Color,
    muted: Color,
    border: Color,
    accent: Color,
    added: Color,
    removed: Color,
}

// One accent; everything else greyscale except diffs and failures.
// Text colours meet 4.5:1 against their background in both themes.
const DARK: Palette = Palette {
    background: Color::Rgb(0, 0, 0),
    surface: Color::Rgb(14, 14, 14),
    foreground: Color::Rgb(237, 237, 237),
    muted: Color::Rgb(163, 163, 163),
    border: Color::Rgb(51, 51, 51),
    accent: Color::Rgb(232, 115, 74),
    added: Color::Rgb(62, 245, 140),
    removed: Color::Rgb(255, 69, 69),
};

const LIGHT: Palette = Palette {
    background: Color::Rgb(255, 255, 255),
    surface: Color::Rgb(242, 242, 242),
    foreground: Color::Rgb(26, 26, 26),
    muted: Color::Rgb(64, 64, 64),
    border: Color::Rgb(208, 208, 208),
    // The dark-theme orange is 3.0:1 on white; this one is 4.7:1.
    accent: Color::Rgb(196, 80, 27),
    added: Color::Rgb(0, 135, 58),
    removed: Color::Rgb(217, 0, 27),
};

// Chosen at startup from NONAUT_THEME or the saved setting, then by /theme.
static LIGHT_THEME: AtomicBool = AtomicBool::new(false);

fn palette() -> &'static Palette {
    if LIGHT_THEME.load(Ordering::Relaxed) {
        &LIGHT
    } else {
        &DARK
    }
}

/// NONAUT_THEME wins over the saved /theme choice; dark is the default.
pub(crate) fn init_theme() {
    let light = match std::env::var("NONAUT_THEME") {
        Ok(theme) => theme.eq_ignore_ascii_case("light"),
        Err(_) => load_theme().ok().flatten().as_deref() == Some("light"),
    };
    LIGHT_THEME.store(light, Ordering::Relaxed);
}
// Driven by the event loop, not the terminal, so a frozen app stops blinking.
const CURSOR_BLINK: Duration = Duration::from_millis(530);
const BRAND_MARK: &str = "⎔";
// The prompt starts two lines tall and grows with newlines up to this many.
const PROMPT_MIN_LINES: usize = 2;
const PROMPT_MAX_LINES: usize = 10;
const PROMPT_HISTORY_LIMIT: usize = 500;

#[derive(Clone, Copy)]
struct SlashCommand {
    name: &'static str,
    description: &'static str,
    visible: bool,
}

const SLASH_COMMANDS: [SlashCommand; 13] = [
    SlashCommand {
        name: "/provider",
        description: "choose or reconnect a provider",
        visible: true,
    },
    SlashCommand {
        name: "/model",
        description: "choose a provider model",
        visible: true,
    },
    SlashCommand {
        name: "/reasoning",
        description: "choose model reasoning effort",
        visible: true,
    },
    SlashCommand {
        name: "/new",
        description: "start a fresh session",
        visible: true,
    },
    SlashCommand {
        name: "/exec",
        description: "run a command directly in nono",
        visible: true,
    },
    SlashCommand {
        name: "/clear",
        description: "clear the screen",
        visible: true,
    },
    SlashCommand {
        name: "/undo",
        description: "restore files to before the last turn",
        visible: true,
    },
    SlashCommand {
        name: "/theme",
        description: "switch between dark and light",
        visible: true,
    },
    SlashCommand {
        name: "/help",
        description: "show available commands",
        visible: true,
    },
    SlashCommand {
        name: "/detach",
        description: "keep this session running and detach",
        visible: true,
    },
    SlashCommand {
        name: "/q",
        description: "leave nonaut",
        visible: true,
    },
    SlashCommand {
        name: "/quit",
        description: "leave nonaut",
        visible: false,
    },
    SlashCommand {
        name: "/exit",
        description: "leave nonaut",
        visible: false,
    },
];

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
}

struct ToolSummary {
    message_index: usize,
    started: usize,
    succeeded: usize,
    failed: usize,
    last_error: String,
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
    events_tx: mpsc::UnboundedSender<(u64, ProviderEvent)>,
    events_rx: mpsc::UnboundedReceiver<(u64, ProviderEvent)>,
    connection_tx: mpsc::UnboundedSender<ConnectionEvent>,
    connection_rx: mpsc::UnboundedReceiver<ConnectionEvent>,
    connection: Option<JoinHandle<()>>,
    connection_generation: u64,
    request: Option<JoinHandle<()>>,
    request_uses_provider: bool,
    undo: Option<Arc<Mutex<Undo>>>,
    prompt_history: Vec<String>,
    /// Position while browsing history with Up/Down; None when not browsing.
    history_index: Option<usize>,
    history_draft: String,
    request_generation: u64,
    queued_request: Option<String>,
    streaming_message: Option<usize>,
    tool_summaries: HashMap<String, ToolSummary>,
    session_tokens: u64,
    conversation_scroll: u16,
    conversation_max_scroll: u16,
    follow_output: bool,
    command_selection: usize,
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
            events_tx,
            events_rx,
            connection_tx,
            connection_rx,
            connection: None,
            connection_generation: 0,
            request: None,
            request_uses_provider: false,
            undo: None,
            prompt_history: Vec::new(),
            history_index: None,
            history_draft: String::new(),
            request_generation: 0,
            queued_request: None,
            streaming_message: None,
            tool_summaries: HashMap::new(),
            session_tokens: 0,
            conversation_scroll: 0,
            conversation_max_scroll: 0,
            follow_output: true,
            command_selection: 0,
            force_redraw: false,
            cursor_visible: true,
            should_quit: false,
        }
    }

    pub async fn run(&mut self, terminal: &mut DefaultTerminal) -> io::Result<()> {
        let mut terminal_events = EventStream::new();
        let mut blink = time::interval(CURSOR_BLINK);
        blink.set_missed_tick_behavior(MissedTickBehavior::Delay);
        while !self.should_quit {
            if self.force_redraw {
                terminal.clear()?;
                self.force_redraw = false;
            }
            terminal.draw(|frame| self.render(frame))?;
            tokio::select! {
                _ = blink.tick() => self.cursor_visible = !self.cursor_visible,
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
        input.set_block(Self::prompt_block(0));
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
    fn prompt_block(tokens: u64) -> Block<'static> {
        let key = Style::new().fg(palette().foreground);
        let label = Style::new().fg(palette().muted);
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
            .title_bottom(Line::from(vec![
                Span::styled(" Enter", key),
                Span::styled(" send · ", label),
                Span::styled("Shift+Enter", key),
                Span::styled(" newline · ", label),
                Span::styled("/q", key),
                Span::styled(" quit ", label),
            ]))
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
        let [header, conversation, prompt] = Layout::vertical([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(u16::try_from(prompt_lines + 2).unwrap_or(u16::MAX)),
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
        let [brand, status] =
            Layout::horizontal([Constraint::Length(46), Constraint::Min(1)]).areas(header_content);
        frame.render_widget(
            Paragraph::new(Line::from(vec![Span::styled(
                format!("  {BRAND_MARK}  nonaut"),
                Style::new().fg(palette().accent).bold(),
            )]))
            .style(Style::new().bg(palette().background)),
            brand,
        );
        frame.render_widget(
            Paragraph::new(format!(
                "{}   {}   reasoning {}   sandboxed  ",
                self.provider_name, self.model_name, self.reasoning_effort
            ))
            .alignment(Alignment::Right)
            .style(Style::new().fg(palette().muted).bg(palette().background)),
            status,
        );

        let mut lines = Vec::new();
        for message in &self.messages {
            match message.role {
                Role::You => {
                    for (index, line) in message.content.lines().enumerate() {
                        let marker = if index == 0 { "›  " } else { "   " };
                        lines.push(Line::from(vec![
                            Span::styled(marker, Style::new().fg(palette().accent).bold()),
                            Span::styled(line, Style::new().fg(palette().foreground).bold()),
                        ]));
                    }
                    lines.push(Line::default());
                }
                Role::Assistant => {
                    let options = MarkdownOptions::new(NonautMarkdown)
                        .table_width(conversation.width.saturating_sub(4));
                    let rendered = from_str_with_options(&message.content, &options);
                    lines.extend(rendered.lines);
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
                        lines.push(Line::styled(line, style));
                    }
                    lines.push(Line::styled(
                        "─".repeat(usize::from(conversation.width.saturating_sub(4))),
                        Style::new().fg(border),
                    ));
                    lines.push(Line::default());
                }
            }
        }
        let visible_height = conversation.height;
        let content_width = usize::from(conversation.width.saturating_sub(4).max(1));
        let rendered_lines = lines
            .iter()
            .map(|line| line.width().max(1).div_ceil(content_width))
            .sum::<usize>();
        self.conversation_max_scroll = u16::try_from(rendered_lines)
            .unwrap_or(u16::MAX)
            .saturating_sub(visible_height);
        if self.follow_output {
            self.conversation_scroll = self.conversation_max_scroll;
        } else {
            self.conversation_scroll = self.conversation_scroll.min(self.conversation_max_scroll);
        }
        frame.render_widget(
            Paragraph::new(lines)
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
                .wrap(Wrap { trim: false })
                .scroll((self.conversation_scroll, 0)),
            conversation,
        );
        self.input.set_cursor_style(if self.cursor_visible {
            Style::new().bg(palette().accent).fg(palette().background)
        } else {
            Style::new()
                .fg(palette().foreground)
                .bg(palette().background)
        });
        self.input
            .set_block(Self::prompt_block(self.session_tokens));
        frame.render_widget(&self.input, prompt);
        if self.model_picker.is_none()
            && self.reasoning_picker.is_none()
            && self.provider_picker.is_none()
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
            match mouse.kind {
                MouseEventKind::ScrollUp => self.scroll_conversation_up(3),
                MouseEventKind::ScrollDown => self.scroll_conversation_down(3),
                _ => {}
            }
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
                    match save_default_provider(provider) {
                        Ok(()) => {
                            self.provider_picker = None;
                            self.begin_connect(provider);
                        }
                        Err(error) => self.push(
                            Role::System,
                            format!("Could not save the default provider: {error:#}"),
                        ),
                    }
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
        self.conversation_scroll = self.conversation_scroll.saturating_sub(lines);
    }

    fn scroll_conversation_down(&mut self, lines: u16) {
        self.conversation_scroll = self
            .conversation_scroll
            .saturating_add(lines)
            .min(self.conversation_max_scroll);
        self.follow_output = self.conversation_scroll == self.conversation_max_scroll;
    }

    fn follow_latest_output(&mut self) {
        self.follow_output = true;
    }

    fn submit(&mut self) {
        let content = self.input.lines().join("\n").trim().to_owned();
        if content.is_empty() {
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
                self.conversation_scroll = 0;
                self.conversation_max_scroll = 0;
            }
            "/provider" => self.open_provider_picker(),
            "/model" => self.open_model_picker(),
            "/reasoning" => self.open_reasoning_picker(),
            "/new" => self.begin_new_session(),
            "/exec" => self.push(Role::System, "Usage: /exec <command>"),
            "/detach" => match crate::sessions::request_detach() {
                Ok(()) => self.push(Role::System, "Session detached."),
                Err(error) => self.push(Role::Error, format!("Could not detach: {error:#}")),
            },
            "/theme" => self.toggle_theme(),
            "/undo" => self.undo_last(),
            "/help" => self.push(Role::System, Self::command_help()),
            _ if content.starts_with("/exec ") => {
                let command = content[6..].trim().to_owned();
                if command.is_empty() {
                    self.push(Role::System, "Usage: /exec <command>");
                } else {
                    self.begin_exec(command);
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
            }
            Err(error) => self.push(Role::Error, format!("Undo failed\n{error:#}")),
        }
    }

    fn toggle_theme(&mut self) {
        let light = !LIGHT_THEME.load(Ordering::Relaxed);
        LIGHT_THEME.store(light, Ordering::Relaxed);
        // The prompt caches its styles, so rebuild it with the new palette.
        self.input = Self::new_input();
        let name = if light { "light" } else { "dark" };
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

    fn begin_exec(&mut self, command: String) {
        if self.request.is_some() {
            self.push(
                Role::System,
                "A response or command is already running. Press Esc to interrupt it first.",
            );
            return;
        }
        self.push(Role::You, format!("/exec {command}"));
        self.streaming_message = None;
        self.tool_summaries.clear();
        self.request_generation = self.request_generation.wrapping_add(1);
        let request_id = self.request_generation;
        let sender = self.events_tx.clone();
        let workspace = self.cwd.clone();
        let undo = self.undo.clone();
        self.request_uses_provider = false;
        self.request = Some(tokio::spawn(async move {
            checkpoint(undo).await;
            let _ = sender.send((request_id, ProviderEvent::ToolStarted("exec".to_owned())));
            let result = crate::tools::execute_shell(&workspace, command).await;
            let (success, output) = match result {
                Ok(output) => (true, output),
                Err(error) => (false, format!("{error:#}")),
            };
            let _ = sender.send((
                request_id,
                ProviderEvent::ToolFinished {
                    tool: "exec".to_owned(),
                    success,
                    output,
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
        self.request_generation = self.request_generation.wrapping_add(1);
        let request_id = self.request_generation;
        let sender = self.events_tx.clone();
        let undo = self.undo.clone();
        self.request = Some(tokio::spawn(async move {
            checkpoint(undo).await;
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
        self.queued_request = None;
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
                    message.content.push_str(&delta);
                }
            }
            ProviderEvent::Usage(tokens) => self.session_tokens = tokens,
            ProviderEvent::ToolStarted(tool) => {
                self.streaming_message = None;
                if Self::compact_tool(&tool) {
                    self.start_compact_tool(&tool);
                } else {
                    if tool == "exec" {
                        self.push(Role::Running, "Running command in nono…");
                    } else {
                        self.push(Role::Running, format!("Running {tool} in nono…"));
                    }
                }
            }
            ProviderEvent::ToolFinished {
                tool,
                success,
                output,
            } => {
                self.streaming_message = None;
                if Self::compact_tool(&tool) && self.finish_compact_tool(&tool, success, &output) {
                    return;
                }
                if let Some(message) = self
                    .messages
                    .iter_mut()
                    .rev()
                    .find(|message| message.role == Role::Running)
                {
                    message.role = Role::System;
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
            }
            ProviderEvent::Completed => {
                self.streaming_message = None;
                self.request = None;
                self.request_uses_provider = false;
                self.start_queued_request();
            }
            ProviderEvent::Interrupted => {
                let provider_request = self.request_uses_provider;
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
                self.session_tokens = 0;
                self.request = None;
                self.request_uses_provider = false;
                self.push(Role::System, "New session started.");
            }
            ProviderEvent::Failed(error) => {
                if let Some(index) = self.streaming_message.take() {
                    if let Some(message) = self.messages.get_mut(index) {
                        message.content = format!("Request failed: {error}");
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

    fn compact_tool(tool: &str) -> bool {
        matches!(tool, "read_file" | "list_directory")
    }

    fn start_compact_tool(&mut self, tool: &str) {
        if let Some(summary) = self.tool_summaries.get_mut(tool) {
            summary.started += 1;
            if let Some(message) = self.messages.get_mut(summary.message_index) {
                message.role = Role::Running;
                message.content = Self::running_summary(tool, summary.started);
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
            message.role = role;
            message.content = content;
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
        self.messages.push(Message {
            role,
            content: content.into(),
        });
    }
}

/// Snapshots the workspace before a turn can change it. Hashing runs off the
/// event loop so the interface stays responsive on large workspaces.
async fn checkpoint(undo: Option<Arc<Mutex<Undo>>>) {
    if let Some(undo) = undo {
        let _ = tokio::task::spawn_blocking(move || {
            undo.lock()
                .unwrap_or_else(PoisonError::into_inner)
                .checkpoint();
        })
        .await;
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
        LIGHT_THEME.store(true, Ordering::Relaxed);
        assert_eq!(palette().background, Color::Rgb(255, 255, 255));
        LIGHT_THEME.store(false, Ordering::Relaxed);
        assert_eq!(palette().background, Color::Rgb(0, 0, 0));
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
        for prompt in ["first prompt", "/theme", "third\nprompt"] {
            app.input.insert_str(prompt);
            app.submit();
        }
        app.input.insert_str("unsent draft");
        press(&mut app, KeyCode::Up);
        assert_eq!(app.input.lines(), ["third", "prompt"]);
        press(&mut app, KeyCode::Up);
        assert_eq!(app.input.lines(), ["/theme"]);
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
                tool: "read_file".to_owned(),
                success: true,
                output: "contents".to_owned(),
            },
        );
        assert_eq!(app.messages.last().unwrap().role, Role::Success);

        app.handle_provider_event(
            0,
            ProviderEvent::ToolFinished {
                tool: "read_file".to_owned(),
                success: false,
                output: "denied".to_owned(),
            },
        );
        assert_eq!(app.messages.last().unwrap().role, Role::Error);
    }

    #[test]
    fn running_tool_stops_animating_when_it_finishes() {
        let mut app = app_with_input("");
        app.handle_provider_event(0, ProviderEvent::ToolStarted("read_file".to_owned()));
        assert_eq!(app.messages.last().unwrap().role, Role::Running);

        app.handle_provider_event(
            0,
            ProviderEvent::ToolFinished {
                tool: "read_file".to_owned(),
                success: true,
                output: String::new(),
            },
        );

        assert_eq!(app.messages.len(), 1);
        assert_eq!(app.messages[0].role, Role::Success);
        assert_eq!(app.messages[0].content, "Read 1 file");
    }

    #[test]
    fn repeated_reads_share_one_counted_status() {
        let mut app = app_with_input("");
        for _ in 0..3 {
            app.handle_provider_event(0, ProviderEvent::ToolStarted("read_file".to_owned()));
            app.handle_provider_event(
                0,
                ProviderEvent::ToolFinished {
                    tool: "read_file".to_owned(),
                    success: true,
                    output: String::new(),
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
            "Usage: /exec <command>"
        );
    }

    #[tokio::test]
    async fn exec_does_not_require_a_provider() {
        let mut app = app_with_input("/exec printf hello");
        app.submit();
        assert!(app.request.is_some());
        assert!(!app.request_uses_provider);
        assert_eq!(app.messages[0].content, "/exec printf hello");
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
