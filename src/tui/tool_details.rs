use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind};
use ratatui::{
    Frame,
    layout::Rect,
    style::Style,
    text::{Line, Text},
    widgets::{Block, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState},
};

use super::{theme::palette, tool_output::ToolOutput};

pub(super) enum Action {
    None,
    Close,
    Copy,
    Scroll,
    Redraw,
}

pub(super) struct ToolDetails {
    text: Text<'static>,
    area: Rect,
    scroll: (u16, u16),
    max_scroll: (u16, u16),
    width: u16,
}

impl ToolDetails {
    pub fn new(output: Option<&ToolOutput>) -> Self {
        let text = Text::raw(output.map_or_else(
            || "No tool output is available yet.".to_owned(),
            ToolOutput::details,
        ));
        let width = u16::try_from(text.width()).unwrap_or(u16::MAX);
        Self {
            text,
            area: Rect::default(),
            scroll: (0, 0),
            max_scroll: (0, 0),
            width,
        }
    }

    pub fn content(&self) -> String {
        self.text.to_string()
    }

    pub fn area(&self) -> Rect {
        self.area
    }

    pub fn render(&mut self, frame: &mut Frame) {
        let screen = frame.area();
        let width = if screen.width > 8 {
            screen.width - 4
        } else {
            screen.width
        }
        .min(140);
        let height = if screen.height > 6 {
            screen.height - 2
        } else {
            screen.height
        };
        let popup = Rect::new(
            screen.x + (screen.width - width) / 2,
            screen.y + (screen.height - height) / 2,
            width,
            height,
        );
        let block = Block::new()
            .borders(Borders::ALL)
            .style(Style::new().fg(palette().foreground).bg(palette().surface))
            .border_style(palette().accent)
            .title(Line::styled(
                " Tool details ",
                Style::new().fg(palette().accent).bold(),
            ))
            .title_bottom(Line::styled(
                " q/Esc close · c copy · ↑↓/PgUp/PgDn scroll · ←→ pan ",
                Style::new().fg(palette().muted),
            ));
        self.area = block.inner(popup);
        self.max_scroll = (
            u16::try_from(self.text.height())
                .unwrap_or(u16::MAX)
                .saturating_sub(self.area.height),
            self.width.saturating_sub(self.area.width),
        );
        self.scroll.0 = self.scroll.0.min(self.max_scroll.0);
        self.scroll.1 = self.scroll.1.min(self.max_scroll.1);
        let first = usize::from(self.scroll.0)
            .saturating_add(1)
            .min(self.text.height());
        let last =
            (usize::from(self.scroll.0) + usize::from(self.area.height)).min(self.text.height());
        let block = block.title_top(
            Line::styled(
                format!(" {first}–{last}/{} ", self.text.height()),
                Style::new().fg(palette().muted),
            )
            .right_aligned(),
        );
        frame.render_widget(Clear, popup);
        frame.render_widget(block, popup);
        frame.render_widget(
            Paragraph::new(self.text.clone())
                .style(Style::new().fg(palette().foreground).bg(palette().surface))
                .scroll(self.scroll),
            self.area,
        );
        if self.max_scroll.0 > 0 && !self.area.is_empty() {
            let mut state = ScrollbarState::new(self.text.height())
                .position(usize::from(self.scroll.0))
                .viewport_content_length(usize::from(self.area.height));
            frame.render_stateful_widget(
                Scrollbar::new(ScrollbarOrientation::VerticalRight)
                    .begin_symbol(None)
                    .end_symbol(None)
                    .thumb_style(Style::new().fg(palette().accent))
                    .track_style(Style::new().fg(palette().border)),
                popup,
                &mut state,
            );
        }
    }

    pub fn handle_event(&mut self, event: Event) -> Action {
        let previous = self.scroll;
        let page = self.area.height.saturating_sub(1).max(1);
        match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => match (key.code, key.modifiers)
            {
                (KeyCode::Esc, _)
                | (KeyCode::Char('q'), KeyModifiers::NONE)
                | (KeyCode::Char('c'), KeyModifiers::CONTROL) => return Action::Close,
                (KeyCode::Char('c'), KeyModifiers::NONE) => return Action::Copy,
                (KeyCode::Char('l'), KeyModifiers::CONTROL) => return Action::Redraw,
                (KeyCode::Up, _) | (KeyCode::Char('k'), KeyModifiers::NONE) => {
                    self.scroll.0 = self.scroll.0.saturating_sub(1)
                }
                (KeyCode::Down, _) | (KeyCode::Char('j'), KeyModifiers::NONE) => {
                    self.scroll.0 = self.scroll.0.saturating_add(1)
                }
                (KeyCode::PageUp, _) => self.scroll.0 = self.scroll.0.saturating_sub(page),
                (KeyCode::PageDown, _) => self.scroll.0 = self.scroll.0.saturating_add(page),
                (KeyCode::Home, _) => self.scroll = (0, 0),
                (KeyCode::End, _) => self.scroll.0 = self.max_scroll.0,
                (KeyCode::Left, _) => self.scroll.1 = self.scroll.1.saturating_sub(4),
                (KeyCode::Right, _) => self.scroll.1 = self.scroll.1.saturating_add(4),
                _ => {}
            },
            Event::Mouse(mouse) => match mouse.kind {
                MouseEventKind::ScrollUp => self.scroll.0 = self.scroll.0.saturating_sub(3),
                MouseEventKind::ScrollDown => self.scroll.0 = self.scroll.0.saturating_add(3),
                MouseEventKind::ScrollLeft => self.scroll.1 = self.scroll.1.saturating_sub(4),
                MouseEventKind::ScrollRight => self.scroll.1 = self.scroll.1.saturating_add(4),
                _ => {}
            },
            _ => {}
        }
        self.scroll.0 = self.scroll.0.min(self.max_scroll.0);
        self.scroll.1 = self.scroll.1.min(self.max_scroll.1);
        if self.scroll == previous {
            Action::None
        } else {
            Action::Scroll
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ToolContext;
    use crossterm::event::KeyEvent;
    use ratatui::{Terminal, backend::TestBackend};

    fn output(text: String) -> ToolDetails {
        ToolDetails::new(Some(&ToolOutput {
            context: ToolContext::minimal("details-test", "run_command"),
            elapsed: None,
            success: true,
            output: text,
            failure: None,
        }))
    }

    fn draw(details: &mut ToolDetails, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| details.render(frame)).unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    fn key(details: &mut ToolDetails, code: KeyCode) {
        details.handle_event(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)));
    }

    #[test]
    fn scrolls_pages_and_long_lines_and_clamps_after_resize() {
        let mut details = output(
            (0..80)
                .map(|index| format!("diagnostic {index}\n"))
                .collect::<String>()
                + &"工具".repeat(20)
                + " horizontal-tail",
        );
        assert!(draw(&mut details, 40, 16).contains("Tool: run_command"));
        key(&mut details, KeyCode::PageDown);
        let page = draw(&mut details, 40, 16);
        assert!(!page.contains("Tool: run_command"));
        assert!(page.contains("diagnostic"));
        key(&mut details, KeyCode::PageUp);
        assert!(draw(&mut details, 40, 16).contains("Tool: run_command"));
        key(&mut details, KeyCode::End);
        assert!(!draw(&mut details, 40, 16).contains("horizontal-tail"));
        for _ in 0..40 {
            key(&mut details, KeyCode::Right);
        }
        assert!(draw(&mut details, 40, 16).contains("horizontal-tail"));
        assert!(draw(&mut details, 140, 30).contains("horizontal-tail"));
        assert_eq!(details.scroll.1, 0);
        key(&mut details, KeyCode::Home);
        assert!(draw(&mut details, 140, 30).contains("Tool: run_command"));
    }

    #[test]
    fn fits_small_terminals_without_losing_close_controls() {
        for (width, height) in [(1, 1), (4, 3), (8, 6), (20, 8)] {
            let mut details = ToolDetails::new(None);
            draw(&mut details, width, height);
            assert!(matches!(
                details.handle_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))),
                Action::Close
            ));
        }
    }
}
