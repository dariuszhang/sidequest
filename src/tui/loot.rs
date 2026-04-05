use std::collections::BTreeMap;

use anyhow::Result;
use chrono::NaiveDate;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use crate::config::WorkMode;
use crate::state::HarvestEntry;

use super::{ACCENT, DIM, enter_tui, key_hints, leave_tui};

pub struct LootInput {
    pub entries: Vec<(usize, HarvestEntry)>,
}

#[derive(Debug, Clone)]
pub enum LootDecision {
    Accept(usize),
    Dismiss(usize),
    Acknowledge(usize),
}

pub enum LootOutcome {
    Decisions(Vec<LootDecision>),
    Cancelled,
}

pub fn run_loot_review(
    input: LootInput,
    peek_fn: impl Fn(&[HarvestEntry]) -> Result<()>,
) -> Result<LootOutcome> {
    let mut terminal = enter_tui()?;
    let mut state = LootState::new(&input);

    let result = loop {
        terminal.draw(|frame| state.draw(frame, &input))?;

        if let Event::Key(key) = event::read()? {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match state.handle_key(key.code, &input) {
                LootAction::Continue => {}
                LootAction::Peek(entries) => {
                    leave_tui(&mut terminal)?;
                    let _ = peek_fn(&entries);
                    terminal = enter_tui()?;
                }
                LootAction::Finish => {
                    break LootOutcome::Decisions(state.decisions);
                }
                LootAction::Cancel => {
                    break LootOutcome::Cancelled;
                }
            }
        }
    };

    leave_tui(&mut terminal)?;
    Ok(result)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Screen {
    Summary,
    GrindReview,
    QuestReview,
}

enum LootAction {
    Continue,
    Peek(Vec<HarvestEntry>),
    Finish,
    Cancel,
}

struct LootState {
    screen: Screen,
    decisions: Vec<LootDecision>,
    grind_indices: Vec<usize>,
    quest_groups: Vec<QuestGroup>,
    current_grind: usize,
    current_quest_group: usize,
}

#[derive(Debug, Clone)]
struct QuestGroup {
    name: String,
    entry_positions: Vec<usize>,
}

impl LootState {
    fn new(input: &LootInput) -> Self {
        let grind_indices: Vec<usize> = input
            .entries
            .iter()
            .enumerate()
            .filter(|(_, (_, e))| e.mode == WorkMode::Grind)
            .map(|(i, _)| i)
            .collect();

        let mut quest_map: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for (i, (_, entry)) in input.entries.iter().enumerate() {
            if entry.mode == WorkMode::Quest {
                let name = entry
                    .quest
                    .clone()
                    .unwrap_or_else(|| entry.repo_name.clone());
                quest_map.entry(name).or_default().push(i);
            }
        }
        let quest_groups: Vec<QuestGroup> = quest_map
            .into_iter()
            .map(|(name, entry_positions)| QuestGroup {
                name,
                entry_positions,
            })
            .collect();

        Self {
            screen: Screen::Summary,
            decisions: Vec::new(),
            grind_indices,
            quest_groups,
            current_grind: 0,
            current_quest_group: 0,
        }
    }

    fn draw(&mut self, frame: &mut ratatui::Frame<'_>, input: &LootInput) {
        let outer = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(3), Constraint::Length(1)])
            .split(frame.area());

        let block = Block::default()
            .title(" Overnight Loot ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(ACCENT));
        let inner = block.inner(outer[0]);
        frame.render_widget(block, outer[0]);

        match self.screen {
            Screen::Summary => self.draw_summary(frame, inner, input),
            Screen::GrindReview => self.draw_grind_review(frame, inner, input),
            Screen::QuestReview => self.draw_quest_review(frame, inner, input),
        }

        let hints = self.current_hints();
        frame.render_widget(key_hints(&hints), outer[1]);
    }

    fn draw_summary(&mut self, frame: &mut ratatui::Frame<'_>, area: Rect, input: &LootInput) {
        let mut lines: Vec<Line> = Vec::new();
        lines.push(Line::from(""));

        let mut grind_by_repo: BTreeMap<&str, Vec<&HarvestEntry>> = BTreeMap::new();
        let mut quest_by_name: BTreeMap<String, Vec<&HarvestEntry>> = BTreeMap::new();

        for (_, entry) in &input.entries {
            match entry.mode {
                WorkMode::Grind => {
                    grind_by_repo
                        .entry(&entry.repo_name)
                        .or_default()
                        .push(entry);
                }
                WorkMode::Quest => {
                    let name = entry
                        .quest
                        .clone()
                        .unwrap_or_else(|| entry.repo_name.clone());
                    quest_by_name.entry(name).or_default().push(entry);
                }
            }
        }

        for (repo, entries) in &grind_by_repo {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("Grind: {repo}"),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(
                        " ({} improvement{})",
                        entries.len(),
                        if entries.len() == 1 { "" } else { "s" }
                    ),
                    DIM,
                ),
            ]));

            let mut by_night: BTreeMap<Option<NaiveDate>, Vec<&&HarvestEntry>> = BTreeMap::new();
            for entry in entries {
                by_night.entry(entry.night_of).or_default().push(entry);
            }
            for (night, items) in by_night {
                lines.push(Line::from(Span::styled(
                    format!("  {}", format_night(night)),
                    DIM,
                )));
                for entry in items {
                    let stat = entry
                        .short_stat
                        .as_deref()
                        .unwrap_or("diff stat unavailable");
                    lines.push(Line::from(format!("    {} ({})", entry.title, stat)));
                }
            }
            lines.push(Line::from(""));
        }

        for (quest, entries) in &quest_by_name {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("Quest: {quest}"),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(
                        " ({} commit{})",
                        entries.len(),
                        if entries.len() == 1 { "" } else { "s" }
                    ),
                    DIM,
                ),
            ]));
            for entry in entries {
                lines.push(Line::from(format!("    {}", entry.title)));
            }
            lines.push(Line::from(""));
        }

        frame.render_widget(
            Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }),
            area,
        );
    }

    fn draw_grind_review(&self, frame: &mut ratatui::Frame<'_>, area: Rect, input: &LootInput) {
        let Some(&pos) = self.grind_indices.get(self.current_grind) else {
            return;
        };
        let (_, entry) = &input.entries[pos];
        let total = self.grind_indices.len();
        let current = self.current_grind + 1;

        let mut lines = vec![
            Line::from(""),
            Line::from(vec![
                Span::styled(format!("[{current}/{total}] "), Style::default().fg(ACCENT)),
                Span::styled(
                    format!("{} — ", entry.repo_name),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::raw(&entry.title),
            ]),
            Line::from(""),
        ];

        if let Some(stat) = &entry.short_stat {
            lines.push(Line::from(Span::styled(stat.clone(), DIM)));
        }

        if let Some(note) = &entry.note {
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::styled("note: ", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(note.clone()),
            ]));
        }

        frame.render_widget(
            Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }),
            area,
        );
    }

    fn draw_quest_review(&self, frame: &mut ratatui::Frame<'_>, area: Rect, input: &LootInput) {
        let Some(group) = self.quest_groups.get(self.current_quest_group) else {
            return;
        };

        let entries: Vec<&HarvestEntry> = group
            .entry_positions
            .iter()
            .map(|&i| &input.entries[i].1)
            .collect();

        let mut lines = vec![
            Line::from(""),
            Line::from(vec![
                Span::styled(
                    format!("Quest: {}", group.name),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(
                        " ({} new commit{})",
                        entries.len(),
                        if entries.len() == 1 { "" } else { "s" }
                    ),
                    DIM,
                ),
            ]),
            Line::from(""),
        ];

        for entry in &entries {
            lines.push(Line::from(format!("  • {}", entry.title)));
            if let Some(stat) = &entry.short_stat {
                lines.push(Line::from(Span::styled(format!("    {stat}"), DIM)));
            }
        }

        frame.render_widget(
            Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }),
            area,
        );
    }

    fn current_hints(&self) -> Vec<(&'static str, &'static str)> {
        match self.screen {
            Screen::Summary => {
                let has_grind = !self.grind_indices.is_empty();
                if has_grind {
                    vec![
                        ("a", "accept all grind"),
                        ("r", "review each"),
                        ("q", "quit"),
                    ]
                } else {
                    vec![("r", "review each"), ("q", "quit")]
                }
            }
            Screen::GrindReview => vec![
                ("a", "accept"),
                ("d", "dismiss"),
                ("p", "peek diff"),
                ("s", "skip"),
                ("q", "quit & save"),
            ],
            Screen::QuestReview => vec![
                ("p", "peek diff"),
                ("a", "acknowledge"),
                ("q", "quit & save"),
            ],
        }
    }

    fn handle_key(&mut self, code: KeyCode, input: &LootInput) -> LootAction {
        match self.screen {
            Screen::Summary => self.handle_summary_key(code, input),
            Screen::GrindReview => self.handle_grind_key(code, input),
            Screen::QuestReview => self.handle_quest_key(code, input),
        }
    }

    fn handle_summary_key(&mut self, code: KeyCode, input: &LootInput) -> LootAction {
        match code {
            KeyCode::Char('a') | KeyCode::Char('A') => {
                for &pos in &self.grind_indices {
                    let (ledger_idx, _) = &input.entries[pos];
                    self.decisions.push(LootDecision::Accept(*ledger_idx));
                }
                if self.quest_groups.is_empty() {
                    LootAction::Finish
                } else {
                    self.current_quest_group = 0;
                    self.screen = Screen::QuestReview;
                    LootAction::Continue
                }
            }
            KeyCode::Char('r') | KeyCode::Char('R') => {
                if !self.grind_indices.is_empty() {
                    self.current_grind = 0;
                    self.screen = Screen::GrindReview;
                } else if !self.quest_groups.is_empty() {
                    self.current_quest_group = 0;
                    self.screen = Screen::QuestReview;
                }
                LootAction::Continue
            }
            KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc => LootAction::Cancel,
            _ => LootAction::Continue,
        }
    }

    fn handle_grind_key(&mut self, code: KeyCode, input: &LootInput) -> LootAction {
        let Some(&pos) = self.grind_indices.get(self.current_grind) else {
            return self.advance_past_grind();
        };

        match code {
            KeyCode::Char('a') | KeyCode::Char('A') => {
                let (ledger_idx, _) = &input.entries[pos];
                self.decisions.push(LootDecision::Accept(*ledger_idx));
                self.current_grind += 1;
                if self.current_grind >= self.grind_indices.len() {
                    return self.advance_past_grind();
                }
                LootAction::Continue
            }
            KeyCode::Char('d') | KeyCode::Char('D') => {
                let (ledger_idx, _) = &input.entries[pos];
                self.decisions.push(LootDecision::Dismiss(*ledger_idx));
                self.current_grind += 1;
                if self.current_grind >= self.grind_indices.len() {
                    return self.advance_past_grind();
                }
                LootAction::Continue
            }
            KeyCode::Char('p') | KeyCode::Char('P') => {
                let (_, entry) = &input.entries[pos];
                LootAction::Peek(vec![entry.clone()])
            }
            KeyCode::Char('s') | KeyCode::Char('S') => {
                self.current_grind += 1;
                if self.current_grind >= self.grind_indices.len() {
                    return self.advance_past_grind();
                }
                LootAction::Continue
            }
            KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc => LootAction::Finish,
            _ => LootAction::Continue,
        }
    }

    fn handle_quest_key(&mut self, code: KeyCode, input: &LootInput) -> LootAction {
        let Some(group) = self.quest_groups.get(self.current_quest_group) else {
            return LootAction::Finish;
        };

        match code {
            KeyCode::Char('a') | KeyCode::Char('A') => {
                for &pos in &group.entry_positions {
                    let (ledger_idx, _) = &input.entries[pos];
                    self.decisions.push(LootDecision::Acknowledge(*ledger_idx));
                }
                self.current_quest_group += 1;
                if self.current_quest_group >= self.quest_groups.len() {
                    LootAction::Finish
                } else {
                    LootAction::Continue
                }
            }
            KeyCode::Char('p') | KeyCode::Char('P') => {
                let entries: Vec<HarvestEntry> = group
                    .entry_positions
                    .iter()
                    .map(|&i| input.entries[i].1.clone())
                    .collect();
                LootAction::Peek(entries)
            }
            KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc => LootAction::Finish,
            _ => LootAction::Continue,
        }
    }

    fn advance_past_grind(&mut self) -> LootAction {
        if self.quest_groups.is_empty() {
            LootAction::Finish
        } else {
            self.current_quest_group = 0;
            self.screen = Screen::QuestReview;
            LootAction::Continue
        }
    }
}

fn format_night(night: Option<NaiveDate>) -> String {
    match night {
        Some(date) => format!("{}", date.format("%b %-d")),
        None => "Unknown night".to_string(),
    }
}
