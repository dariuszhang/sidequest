pub mod init;
pub mod loot;
pub mod status;

use std::io::{self, Stdout};

use anyhow::{Context, Result};
use crossterm::event::{KeyCode, KeyEvent};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

pub type Term = Terminal<CrosstermBackend<Stdout>>;

pub const ACCENT: Color = Color::Rgb(255, 165, 0); // orange
pub const DIM: Style = Style::new().fg(Color::DarkGray);
pub const HIGHLIGHT: Style = Style::new().fg(Color::Black).bg(Color::Rgb(255, 165, 0));

pub fn enter_tui() -> Result<Term> {
    enable_raw_mode().context("failed to enable raw mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).context("failed to enter alternate screen")?;
    let backend = CrosstermBackend::new(stdout);
    Terminal::new(backend).context("failed to create terminal")
}

pub fn leave_tui(terminal: &mut Term) -> Result<()> {
    disable_raw_mode().ok();
    execute!(terminal.backend_mut(), LeaveAlternateScreen).ok();
    terminal.show_cursor().ok();
    Ok(())
}

pub fn key_hints(hints: &[(&str, &str)]) -> Line<'static> {
    let mut spans = Vec::new();
    for (index, (key, label)) in hints.iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(
            format!("[{key}]"),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::raw(format!(" {label}")));
    }
    Line::from(spans)
}

pub fn wizard_layout(area: Rect, title: &str, step: usize, total_steps: usize) -> (Rect, Rect) {
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(1)])
        .split(area);

    // Caller renders its own block over outer[0]; we just compute the layout.
    let _ = (title, step, total_steps); // used by caller for block title
    (outer[0], outer[1])
}

#[derive(Debug, Clone)]
pub struct TextInput {
    pub value: String,
    pub placeholder: String,
}

impl TextInput {
    pub fn new(placeholder: impl Into<String>) -> Self {
        Self {
            value: String::new(),
            placeholder: placeholder.into(),
        }
    }

    pub fn with_value(mut self, value: impl Into<String>) -> Self {
        self.value = value.into();
        self
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char(c) => self.value.push(c),
            KeyCode::Backspace => {
                self.value.pop();
            }
            _ => {}
        }
    }

    pub fn render(&self, label: &str) -> Paragraph<'static> {
        let display = if self.value.is_empty() {
            Span::styled(self.placeholder.clone(), DIM)
        } else {
            Span::raw(self.value.clone())
        };
        let cursor = Span::styled("_", Style::default().add_modifier(Modifier::SLOW_BLINK));
        Paragraph::new(Line::from(vec![
            Span::styled(
                format!("{label}: "),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            display,
            cursor,
        ]))
    }

    pub fn effective_value(&self) -> &str {
        if self.value.is_empty() {
            &self.placeholder
        } else {
            &self.value
        }
    }
}
