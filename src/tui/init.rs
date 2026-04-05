use std::path::PathBuf;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};

use crate::config::SideQuestConfig;
use crate::harvester::slugify;
use crate::oracle::ProviderKind;
use crate::scanner::ScannedRepo;

use super::{ACCENT, DIM, HIGHLIGHT, TextInput, enter_tui, key_hints, leave_tui};

pub struct InitInput {
    pub detected_providers: Vec<ProviderKind>,
    pub scanned_repos: Vec<ScannedRepo>,
    pub current_config: SideQuestConfig,
}

pub enum InitOutcome {
    Completed(InitResult),
    Cancelled,
}

pub struct InitResult {
    pub provider_preference: Option<Vec<ProviderKind>>,
    pub enabled_providers: Vec<ProviderKind>,
    pub selected_repos: Vec<PathBuf>,
    pub quest: Option<QuestInput>,
    pub sleep_window: Option<(String, String)>,
}

pub struct QuestInput {
    pub goal: String,
    pub name: String,
    pub directory: String,
}

pub fn run_repo_selector(repos: Vec<ScannedRepo>) -> Result<Option<Vec<PathBuf>>> {
    let mut terminal = enter_tui()?;
    let mut state = RepoSelectorState::new(&repos);

    let result = loop {
        terminal.draw(|frame| {
            let (block_area, hint_area) =
                super::wizard_layout(frame.area(), "SideQuest", 1, 1);
            let block = Block::default()
                .title(" Select Repositories ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(ACCENT));
            let inner = block.inner(block_area);
            frame.render_widget(block, block_area);
            render_repo_list(frame, inner, &repos, &state.selected, &mut state.list_state);
            frame.render_widget(
                key_hints(&[
                    ("space", "toggle"),
                    ("a", "all"),
                    ("n", "none"),
                    ("enter", "confirm"),
                    ("esc", "cancel"),
                ]),
                hint_area,
            );
        })?;

        if let Event::Key(key) = event::read()? {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => break None,
                KeyCode::Enter => {
                    let paths = collect_selected(&repos, &state.selected);
                    break Some(paths);
                }
                KeyCode::Char(' ') => state.toggle_current(),
                KeyCode::Char('a') => state.selected.fill(true),
                KeyCode::Char('n') => state.selected.fill(false),
                KeyCode::Up | KeyCode::Char('k') => state.move_up(),
                KeyCode::Down | KeyCode::Char('j') => state.move_down(),
                _ => {}
            }
        }
    };

    leave_tui(&mut terminal)?;
    Ok(result)
}

pub fn run_init_wizard(input: InitInput) -> Result<InitOutcome> {
    let mut terminal = enter_tui()?;
    let mut wizard = InitWizard::new(&input);

    let result = loop {
        terminal.draw(|frame| wizard.draw(frame, &input))?;

        if let Event::Key(key) = event::read()? {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match wizard.handle_key(key.code, &input) {
                Action::Continue => {}
                Action::Finish => {
                    break InitOutcome::Completed(wizard.into_result(&input));
                }
                Action::Cancel => {
                    break InitOutcome::Cancelled;
                }
            }
        }
    };

    leave_tui(&mut terminal)?;
    Ok(result)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InitStep {
    Welcome,
    ProviderPreference,
    RepoSelection,
    QuestGoal,
    QuestName,
    QuestDirectory,
    SleepWindow,
    Done,
}

impl InitStep {
    fn number(self) -> usize {
        match self {
            Self::Welcome => 1,
            Self::ProviderPreference => 2,
            Self::RepoSelection => 3,
            Self::QuestGoal | Self::QuestName | Self::QuestDirectory => 4,
            Self::SleepWindow => 5,
            Self::Done => 5,
        }
    }
}

const TOTAL_STEPS: usize = 5;

enum Action {
    Continue,
    Finish,
    Cancel,
}

struct InitWizard {
    step: InitStep,
    provider_list_state: ListState,
    provider_order: Vec<ProviderKind>,
    provider_enabled: Vec<bool>,
    repo_state: RepoSelectorState,
    quest_goal: TextInput,
    quest_name: TextInput,
    quest_directory: TextInput,
    sleep_start: TimeInput,
    sleep_end: TimeInput,
    sleep_error: Option<String>,
    sleep_focus: SleepFocus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SleepFocus {
    Start,
    End,
}

impl InitWizard {
    fn new(input: &InitInput) -> Self {
        let provider_order = if input.current_config.provider_preference.is_empty() {
            input.detected_providers.clone()
        } else {
            input.current_config.provider_preference.clone()
        };
        let provider_enabled = vec![true; provider_order.len()];

        Self {
            step: InitStep::Welcome,
            provider_list_state: ListState::default().with_selected(Some(0)),
            provider_order,
            provider_enabled,
            repo_state: RepoSelectorState::new(&input.scanned_repos),
            quest_goal: TextInput::new("describe a side project, or path to .md/.txt"),
            quest_name: TextInput::new(""),
            quest_directory: TextInput::new(""),
            sleep_start: TimeInput::from_hhmm(&input.current_config.sleep_window.start),
            sleep_end: TimeInput::from_hhmm(&input.current_config.sleep_window.end),
            sleep_error: None,
            sleep_focus: SleepFocus::Start,
        }
    }

    fn active_providers(&self) -> Vec<ProviderKind> {
        self.provider_order
            .iter()
            .zip(self.provider_enabled.iter())
            .filter(|&(_, &enabled)| enabled)
            .map(|(p, _)| *p)
            .collect()
    }

    fn draw(&mut self, frame: &mut ratatui::Frame<'_>, input: &InitInput) {
        let (block_area, hint_area) =
            super::wizard_layout(frame.area(), "SideQuest", self.step.number(), TOTAL_STEPS);
        let block = Block::default()
            .title(format!(
                " SideQuest — First Time Setup ─── {}/{TOTAL_STEPS} ",
                self.step.number()
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(ACCENT));
        let inner = block.inner(block_area);
        frame.render_widget(block, block_area);

        match self.step {
            InitStep::Welcome => self.draw_welcome(frame, inner, input),
            InitStep::ProviderPreference => self.draw_provider_pref(frame, inner),
            InitStep::RepoSelection => {
                self.draw_repo_selection(frame, inner, &input.scanned_repos);
            }
            InitStep::QuestGoal => self.draw_quest_goal(frame, inner),
            InitStep::QuestName => self.draw_quest_name(frame, inner),
            InitStep::QuestDirectory => self.draw_quest_directory(frame, inner),
            InitStep::SleepWindow => self.draw_sleep_window(frame, inner),
            InitStep::Done => self.draw_done(frame, inner),
        }

        let hints = self.current_hints();
        frame.render_widget(key_hints(&hints), hint_area);
    }

    fn draw_welcome(
        &self,
        frame: &mut ratatui::Frame<'_>,
        area: Rect,
        input: &InitInput,
    ) {
        let mut lines = vec![
            Line::from(""),
            Line::from(Span::styled(
                "Welcome to SideQuest!",
                Style::default()
                    .fg(ACCENT)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from("Your AI coding agents have subscription tokens that expire if unused."),
            Line::from("SideQuest puts them to work while you sleep — so you wake up to"),
            Line::from("freshly written code and a brand new session ready to go."),
            Line::from(""),
            Line::from(Span::styled(
                "Here's what SideQuest can do:",
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(vec![
                Span::styled("  Grind  ", Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
                Span::raw("Review, refactor, and improve your existing repositories"),
            ]),
            Line::from(vec![
                Span::styled("  Quest  ", Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
                Span::raw("Build entirely new side projects from a goal you describe"),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::raw("  SideQuest only runs during your sleep window, and always "),
            ]),
            Line::from(vec![
                Span::raw("  finishes early enough to guarantee you a "),
                Span::styled("fresh session", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(" in the morning."),
            ]),
            Line::from(""),
        ];

        lines.push(Line::from(Span::styled(
            "Detected agents:",
            Style::default().add_modifier(Modifier::BOLD),
        )));

        if input.detected_providers.is_empty() {
            lines.push(Line::from(Span::styled(
                "  (none found — you can still configure SideQuest and add agents later)",
                DIM,
            )));
        } else {
            for provider in &input.detected_providers {
                lines.push(Line::from(vec![
                    Span::styled("  ✓ ", Style::default().fg(ACCENT)),
                    Span::raw(provider.to_string()),
                ]));
            }
        }

        frame.render_widget(
            Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }),
            area,
        );
    }

    fn draw_provider_pref(&mut self, frame: &mut ratatui::Frame<'_>, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(6), Constraint::Min(1)])
            .split(area);

        frame.render_widget(
            Paragraph::new(vec![
                Line::from(""),
                Line::from(Span::styled(
                    "Configure your AI agents",
                    Style::default().add_modifier(Modifier::BOLD),
                )),
                Line::from(""),
                Line::from("SideQuest will try the first enabled agent, and continue work with the next"),
                Line::from("if the first one's budget is exhausted. Disable agents you don't want to use."),
            ]),
            chunks[0],
        );

        let items: Vec<ListItem> = self
            .provider_order
            .iter()
            .enumerate()
            .map(|(idx, p)| {
                let enabled = self.provider_enabled[idx];
                let rank = if !enabled {
                    "   ".to_string()
                } else {
                    let rank_num = self
                        .provider_order
                        .iter()
                        .zip(self.provider_enabled.iter())
                        .take(idx + 1)
                        .filter(|&(_, &e)| e)
                        .count();
                    format!(" {rank_num}.")
                };
                let checkbox = if enabled { "[*]" } else { "[ ]" };
                let style = if enabled {
                    Style::default()
                } else {
                    DIM
                };
                ListItem::new(Line::from(vec![
                    Span::raw(format!(" {checkbox} ")),
                    Span::styled(format!("{rank} {p}"), style),
                ]))
            })
            .collect();

        let list = List::new(items)
            .highlight_style(HIGHLIGHT)
            .highlight_symbol(">> ");

        frame.render_stateful_widget(list, chunks[1], &mut self.provider_list_state);
    }

    fn draw_repo_selection(
        &mut self,
        frame: &mut ratatui::Frame<'_>,
        area: Rect,
        repos: &[ScannedRepo],
    ) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(7), Constraint::Min(1)])
            .split(area);

        frame.render_widget(
            Paragraph::new(vec![
                Line::from(""),
                Line::from(Span::styled(
                    "Grind — Select repositories",
                    Style::default().add_modifier(Modifier::BOLD),
                )),
                Line::from(""),
                Line::from("These repositories had recent activity and can be reviewed and improved"),
                Line::from("overnight. Sidequest automatically finds opportunities to improve or build"),
                Line::from("new features on them. You can re-scan anytime later with `sidequest grind scan`."),
                Line::from(""),
            ])
            .wrap(Wrap { trim: false }),
            chunks[0],
        );

        if repos.is_empty() {
            frame.render_widget(
                Paragraph::new(Span::styled(
                    "  No repositories with recent commits were found.",
                    DIM,
                )),
                chunks[1],
            );
        } else {
            render_repo_list(
                frame,
                chunks[1],
                repos,
                &self.repo_state.selected,
                &mut self.repo_state.list_state,
            );
        }
    }

    fn draw_quest_goal(&self, frame: &mut ratatui::Frame<'_>, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(14), Constraint::Length(1)])
            .split(area);

        frame.render_widget(
            Paragraph::new(vec![
                Line::from(""),
                Line::from(Span::styled(
                    "Quest — Side projects built overnight",
                    Style::default().add_modifier(Modifier::BOLD),
                )),
                Line::from(""),
                Line::from("A quest is a brand new project that SideQuest builds for you from scratch."),
                Line::from("Describe what you want built — SideQuest will automatically work over many "),
                Line::from("nights with your spare tokens, so be as specific or as open-ended as you like."),
                Line::from(""),
                Line::from("You can also point to a .md or .txt file with a detailed spec."),
                Line::from(""),
                Line::from(Span::styled("Managing quests later:", Style::default().add_modifier(Modifier::BOLD))),
                Line::from(vec![
                    Span::styled("  sidequest quest", Style::default().fg(ACCENT)),
                    Span::raw("          create a new quest"),
                ]),
                Line::from(vec![
                    Span::styled("  sidequest quest list", Style::default().fg(ACCENT)),
                    Span::raw("     list all quests"),
                ]),
                Line::from(vec![
                    Span::styled("  sidequest quest pause <name>", Style::default().fg(ACCENT)),
                    Span::raw("  pause/resume a quest"),
                ]),
            ]),
            chunks[0],
        );

        frame.render_widget(self.quest_goal.render("Quest goal"), chunks[1]);
    }

    fn draw_quest_name(&self, frame: &mut ratatui::Frame<'_>, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(4), Constraint::Length(1)])
            .split(area);

        frame.render_widget(
            Paragraph::new(vec![
                Line::from(""),
                Line::from("Give this quest a short name. This is used to identify it in commands"),
                Line::from(vec![
                    Span::raw("like "),
                    Span::styled("sidequest quest log <name>", Style::default().fg(ACCENT)),
                    Span::raw(" and "),
                    Span::styled("sidequest quest pause <name>", Style::default().fg(ACCENT)),
                    Span::raw("."),
                ]),
            ]),
            chunks[0],
        );

        frame.render_widget(self.quest_name.render("Name"), chunks[1]);
    }

    fn draw_quest_directory(&self, frame: &mut ratatui::Frame<'_>, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(3), Constraint::Length(1)])
            .split(area);

        frame.render_widget(
            Paragraph::new(vec![
                Line::from(""),
                Line::from("Where should the quest project code live on disk?"),
            ]),
            chunks[0],
        );

        frame.render_widget(self.quest_directory.render("Directory"), chunks[1]);
    }

    fn draw_sleep_window(&self, frame: &mut ratatui::Frame<'_>, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(9),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Min(0),
            ])
            .split(area);

        frame.render_widget(
            Paragraph::new(vec![
                Line::from(""),
                Line::from(Span::styled(
                    "Sleep window",
                    Style::default().add_modifier(Modifier::BOLD),
                )),
                Line::from(""),
                Line::from("SideQuest only runs agents during this window — your sleeping hours."),
                Line::from("It will always stop early enough to let your token budget reset, so"),
                Line::from("you wake up with a fresh 5 hour session for your own work."),
                Line::from(""),
                Line::from("Enter times in 24-hour format."),
            ]),
            chunks[0],
        );

        let start_style = if self.sleep_focus == SleepFocus::Start {
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        let end_style = if self.sleep_focus == SleepFocus::End {
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };

        let start_display = self.sleep_start.display_with_placeholder();
        let end_display = self.sleep_end.display_with_placeholder();

        let cursor_start = if self.sleep_focus == SleepFocus::Start && !self.sleep_start.is_complete() {
            "_"
        } else {
            ""
        };
        let cursor_end = if self.sleep_focus == SleepFocus::End && !self.sleep_end.is_complete() {
            "_"
        } else {
            ""
        };

        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("  Sleep: ", start_style),
                start_display,
                Span::raw(cursor_start),
                Span::raw("    →    "),
                Span::styled("Wake: ", end_style),
                end_display,
                Span::raw(cursor_end),
            ])),
            chunks[1],
        );

        if let Some(err) = &self.sleep_error {
            frame.render_widget(
                Paragraph::new(Span::styled(
                    format!("  {err}"),
                    Style::default().fg(ratatui::style::Color::Red),
                )),
                chunks[2],
            );
        }
    }

    fn draw_done(&self, frame: &mut ratatui::Frame<'_>, area: Rect) {
        let selected_count = self.repo_state.selected.iter().filter(|s| **s).count();
        let active_providers = self.active_providers();

        let mut lines = vec![
            Line::from(""),
            Line::from(Span::styled(
                "You're all set! Your overnight adventure begins tonight.",
                Style::default()
                    .fg(ACCENT)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from("While you sleep, your AI agents will be hard at work on your code."),
            Line::from("Check the results in the morning with `sidequest loot`."),
            Line::from(""),
            Line::from(Span::styled(
                "Your setup:",
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
        ];

        if !active_providers.is_empty() {
            lines.push(Line::from(vec![
                Span::styled("  Agents     ", DIM),
                Span::raw(
                    active_providers
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(" → "),
                ),
            ]));
        }

        lines.push(Line::from(vec![
            Span::styled("  Grind      ", DIM),
            Span::raw(format!(
                "{selected_count} repo{}",
                if selected_count == 1 { "" } else { "s" }
            )),
        ]));

        if !self.quest_goal.value.is_empty() {
            lines.push(Line::from(vec![
                Span::styled("  Quest      ", DIM),
                Span::raw(self.quest_name.effective_value().to_string()),
            ]));
        } else {
            lines.push(Line::from(vec![
                Span::styled("  Quest      ", DIM),
                Span::styled("none (add one later with `sidequest quest`)", DIM),
            ]));
        }

        lines.push(Line::from(vec![
            Span::styled("  Sleep      ", DIM),
            Span::raw(format!(
                "{} → {}",
                self.sleep_start.to_hhmm(),
                self.sleep_end.to_hhmm(),
            )),
        ]));

        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  Happy questing!",
            Style::default().fg(ACCENT).add_modifier(Modifier::ITALIC),
        )));

        frame.render_widget(
            Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }),
            area,
        );
    }

    fn current_hints(&self) -> Vec<(&'static str, &'static str)> {
        match self.step {
            InitStep::Welcome => vec![("enter", "begin setup"), ("q", "quit")],
            InitStep::ProviderPreference => {
                vec![
                    ("↑↓", "navigate"),
                    ("space", "enable/disable"),
                    ("s", "swap order"),
                    ("enter", "next"),
                    ("esc", "back"),
                ]
            }
            InitStep::RepoSelection => vec![
                ("space", "toggle"),
                ("a", "all"),
                ("n", "none"),
                ("↑↓", "navigate"),
                ("enter", "next"),
                ("esc", "back"),
            ],
            InitStep::QuestGoal => {
                vec![("enter", "next (empty to skip)"), ("esc", "back")]
            }
            InitStep::QuestName | InitStep::QuestDirectory => {
                vec![("enter", "next"), ("esc", "back")]
            }
            InitStep::SleepWindow => {
                vec![("tab", "switch field"), ("enter", "next"), ("esc", "back")]
            }
            InitStep::Done => vec![("enter", "finish"), ("esc", "back")],
        }
    }

    fn handle_key(&mut self, code: KeyCode, input: &InitInput) -> Action {
        match self.step {
            InitStep::Welcome => match code {
                KeyCode::Enter => {
                    if input.detected_providers.len() < 2 {
                        self.step = InitStep::RepoSelection;
                    } else {
                        self.step = InitStep::ProviderPreference;
                    }
                    Action::Continue
                }
                KeyCode::Char('q') | KeyCode::Esc => Action::Cancel,
                _ => Action::Continue,
            },

            InitStep::ProviderPreference => match code {
                KeyCode::Enter => {
                    if self.active_providers().is_empty() {
                        // Must have at least one enabled
                        return Action::Continue;
                    }
                    self.step = InitStep::RepoSelection;
                    Action::Continue
                }
                KeyCode::Esc => {
                    self.step = InitStep::Welcome;
                    Action::Continue
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    if let Some(current) = self.provider_list_state.selected() {
                        let next = if current > 0 {
                            current - 1
                        } else {
                            self.provider_order.len().saturating_sub(1)
                        };
                        self.provider_list_state.select(Some(next));
                    }
                    Action::Continue
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    if let Some(current) = self.provider_list_state.selected() {
                        let next = if current + 1 < self.provider_order.len() {
                            current + 1
                        } else {
                            0
                        };
                        self.provider_list_state.select(Some(next));
                    }
                    Action::Continue
                }
                KeyCode::Char(' ') => {
                    // Toggle enable/disable
                    if let Some(idx) = self.provider_list_state.selected()
                        && let Some(val) = self.provider_enabled.get_mut(idx)
                    {
                        *val = !*val;
                    }
                    Action::Continue
                }
                KeyCode::Char('s') => {
                    // Swap order (promote selected to top)
                    if let Some(idx) = self.provider_list_state.selected() {
                        if idx > 0 {
                            self.provider_order.swap(idx, idx - 1);
                            self.provider_enabled.swap(idx, idx - 1);
                            self.provider_list_state.select(Some(idx - 1));
                        } else if self.provider_order.len() > 1 {
                            let last = self.provider_order.len() - 1;
                            self.provider_order.swap(0, last);
                            self.provider_enabled.swap(0, last);
                            self.provider_list_state.select(Some(last));
                        }
                    }
                    Action::Continue
                }
                _ => Action::Continue,
            },

            InitStep::RepoSelection => match code {
                KeyCode::Enter => {
                    self.step = InitStep::QuestGoal;
                    Action::Continue
                }
                KeyCode::Esc => {
                    if input.detected_providers.len() < 2 {
                        self.step = InitStep::Welcome;
                    } else {
                        self.step = InitStep::ProviderPreference;
                    }
                    Action::Continue
                }
                KeyCode::Char(' ') => {
                    self.repo_state.toggle_current();
                    Action::Continue
                }
                KeyCode::Char('a') => {
                    self.repo_state.selected.fill(true);
                    Action::Continue
                }
                KeyCode::Char('n') => {
                    self.repo_state.selected.fill(false);
                    Action::Continue
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.repo_state.move_up();
                    Action::Continue
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.repo_state.move_down();
                    Action::Continue
                }
                _ => Action::Continue,
            },

            InitStep::QuestGoal => match code {
                KeyCode::Enter => {
                    if self.quest_goal.value.is_empty() {
                        self.step = InitStep::SleepWindow;
                    } else {
                        let default_name = slugify(&self.quest_goal.value);
                        self.quest_name.placeholder = default_name;
                        self.step = InitStep::QuestName;
                    }
                    Action::Continue
                }
                KeyCode::Esc => {
                    self.step = InitStep::RepoSelection;
                    Action::Continue
                }
                _ => {
                    self.quest_goal.handle_key(crossterm::event::KeyEvent::new(
                        code,
                        crossterm::event::KeyModifiers::NONE,
                    ));
                    Action::Continue
                }
            },

            InitStep::QuestName => match code {
                KeyCode::Enter => {
                    let name = self.quest_name.effective_value().to_string();
                    let default_dir = input
                        .current_config
                        .default_quest_projects_directory()
                        .map(|d| d.join(&name).display().to_string())
                        .unwrap_or_default();
                    self.quest_directory.placeholder = default_dir;
                    self.step = InitStep::QuestDirectory;
                    Action::Continue
                }
                KeyCode::Esc => {
                    self.step = InitStep::QuestGoal;
                    Action::Continue
                }
                _ => {
                    self.quest_name.handle_key(crossterm::event::KeyEvent::new(
                        code,
                        crossterm::event::KeyModifiers::NONE,
                    ));
                    Action::Continue
                }
            },

            InitStep::QuestDirectory => match code {
                KeyCode::Enter => {
                    self.step = InitStep::SleepWindow;
                    Action::Continue
                }
                KeyCode::Esc => {
                    self.step = InitStep::QuestName;
                    Action::Continue
                }
                _ => {
                    self.quest_directory
                        .handle_key(crossterm::event::KeyEvent::new(
                            code,
                            crossterm::event::KeyModifiers::NONE,
                        ));
                    Action::Continue
                }
            },

            InitStep::SleepWindow => match code {
                KeyCode::Enter => {
                    let start = self.sleep_start.to_hhmm();
                    let end = self.sleep_end.to_hhmm();
                    if !is_valid_time(&start) || !is_valid_time(&end) {
                        self.sleep_error = Some("Enter 4 digits for each time (e.g. 2300 for 11 PM)".to_string());
                        return Action::Continue;
                    }
                    self.sleep_error = None;
                    self.step = InitStep::Done;
                    Action::Continue
                }
                KeyCode::Esc => {
                    if self.quest_goal.value.is_empty() {
                        self.step = InitStep::QuestGoal;
                    } else {
                        self.step = InitStep::QuestDirectory;
                    }
                    Action::Continue
                }
                KeyCode::Tab | KeyCode::BackTab => {
                    self.sleep_focus = match self.sleep_focus {
                        SleepFocus::Start => SleepFocus::End,
                        SleepFocus::End => SleepFocus::Start,
                    };
                    Action::Continue
                }
                KeyCode::Char(c) if c.is_ascii_digit() => {
                    let complete_before = match self.sleep_focus {
                        SleepFocus::Start => self.sleep_start.is_complete(),
                        SleepFocus::End => self.sleep_end.is_complete(),
                    };
                    match self.sleep_focus {
                        SleepFocus::Start => self.sleep_start.push_digit(c),
                        SleepFocus::End => self.sleep_end.push_digit(c),
                    }
                    // Auto-advance to wake field when sleep time is complete
                    if self.sleep_focus == SleepFocus::Start
                        && !complete_before
                        && self.sleep_start.is_complete()
                    {
                        self.sleep_focus = SleepFocus::End;
                    }
                    self.sleep_error = None;
                    Action::Continue
                }
                KeyCode::Backspace => {
                    match self.sleep_focus {
                        SleepFocus::Start => self.sleep_start.pop_digit(),
                        SleepFocus::End => self.sleep_end.pop_digit(),
                    }
                    self.sleep_error = None;
                    Action::Continue
                }
                _ => Action::Continue,
            },

            InitStep::Done => match code {
                KeyCode::Enter | KeyCode::Char('q') => Action::Finish,
                KeyCode::Esc => {
                    self.step = InitStep::SleepWindow;
                    Action::Continue
                }
                _ => Action::Continue,
            },
        }
    }

    fn into_result(self, input: &InitInput) -> InitResult {
        let active = self.active_providers();
        let provider_preference = if active != input.detected_providers {
            Some(active.clone())
        } else {
            None
        };

        let selected_repos = collect_selected(&input.scanned_repos, &self.repo_state.selected);

        let quest = if self.quest_goal.value.is_empty() {
            None
        } else {
            Some(QuestInput {
                goal: self.quest_goal.value,
                name: self.quest_name.effective_value().to_string(),
                directory: self.quest_directory.effective_value().to_string(),
            })
        };

        let start = self.sleep_start.to_hhmm();
        let end = self.sleep_end.to_hhmm();
        let changed = start != input.current_config.sleep_window.start
            || end != input.current_config.sleep_window.end;
        let sleep_window = if changed { Some((start, end)) } else { None };

        InitResult {
            provider_preference,
            enabled_providers: active,
            selected_repos,
            quest,
            sleep_window,
        }
    }
}

// -- Time input that auto-inserts ":"  -------------------------------------------

#[derive(Debug, Clone)]
struct TimeInput {
    digits: String, // up to 4 raw digits, e.g. "2300"
    placeholder_hhmm: String,
}

impl TimeInput {
    fn from_hhmm(hhmm: &str) -> Self {
        let raw_digits: String = hhmm.chars().filter(|c| c.is_ascii_digit()).collect();
        Self {
            digits: String::new(),
            placeholder_hhmm: if raw_digits.len() == 4 {
                format!("{}:{}", &raw_digits[..2], &raw_digits[2..])
            } else {
                hhmm.to_string()
            },
        }
    }

    fn push_digit(&mut self, c: char) {
        if self.digits.len() < 4 {
            self.digits.push(c);
        }
    }

    fn pop_digit(&mut self) {
        self.digits.pop();
    }

    fn is_complete(&self) -> bool {
        self.digits.len() == 4
    }

    fn to_hhmm(&self) -> String {
        if self.digits.len() == 4 {
            format!("{}:{}", &self.digits[..2], &self.digits[2..])
        } else if self.digits.is_empty() {
            self.placeholder_hhmm.clone()
        } else {
            // Partial — pad with placeholder
            self.digits.clone()
        }
    }

    fn display_with_placeholder(&self) -> Span<'static> {
        if self.digits.is_empty() {
            Span::styled(self.placeholder_hhmm.clone(), DIM)
        } else if self.digits.len() <= 2 {
            let entered = &self.digits;
            Span::raw(format!("{entered}:"))
        } else {
            let hh = &self.digits[..2];
            let mm = &self.digits[2..];
            Span::raw(format!("{hh}:{mm}"))
        }
    }
}

// -- Repo selector state ---------------------------------------------------------

struct RepoSelectorState {
    selected: Vec<bool>,
    list_state: ListState,
}

impl RepoSelectorState {
    fn new(repos: &[ScannedRepo]) -> Self {
        let selected = repos.iter().map(|r| r.already_in_grind).collect();
        let list_state = if repos.is_empty() {
            ListState::default()
        } else {
            ListState::default().with_selected(Some(0))
        };
        Self {
            selected,
            list_state,
        }
    }

    fn toggle_current(&mut self) {
        if let Some(idx) = self.list_state.selected()
            && let Some(val) = self.selected.get_mut(idx)
        {
            *val = !*val;
        }
    }

    fn move_up(&mut self) {
        if let Some(current) = self.list_state.selected() {
            let next = if current > 0 {
                current - 1
            } else {
                self.selected.len().saturating_sub(1)
            };
            self.list_state.select(Some(next));
        }
    }

    fn move_down(&mut self) {
        if let Some(current) = self.list_state.selected() {
            let next = if current + 1 < self.selected.len() {
                current + 1
            } else {
                0
            };
            self.list_state.select(Some(next));
        }
    }
}

fn render_repo_list(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    repos: &[ScannedRepo],
    selected: &[bool],
    list_state: &mut ListState,
) {
    let items: Vec<ListItem> = repos
        .iter()
        .zip(selected.iter())
        .map(|(repo, &is_selected)| {
            let checkbox = if is_selected { "[*]" } else { "[ ]" };
            let detail = if repo.already_in_grind && is_selected {
                "already in grind".to_string()
            } else {
                format!(
                    "{} commit{}",
                    repo.commit_count,
                    if repo.commit_count == 1 { "" } else { "s" }
                )
            };
            ListItem::new(Line::from(vec![
                Span::raw(format!(" {checkbox} ")),
                Span::raw(format!("{:<36}", repo.display_path())),
                Span::styled(format!("({detail})"), DIM),
            ]))
        })
        .collect();

    let list = List::new(items)
        .highlight_style(HIGHLIGHT)
        .highlight_symbol(">> ");

    frame.render_stateful_widget(list, area, list_state);
}

fn collect_selected(repos: &[ScannedRepo], selected: &[bool]) -> Vec<PathBuf> {
    repos
        .iter()
        .zip(selected.iter())
        .filter(|&(_, &is_selected)| is_selected)
        .map(|(repo, _)| repo.path.clone())
        .collect()
}

fn is_valid_time(value: &str) -> bool {
    let parts: Vec<&str> = value.split(':').collect();
    if parts.len() != 2 {
        return false;
    }
    let Ok(hours) = parts[0].parse::<u32>() else {
        return false;
    };
    let Ok(minutes) = parts[1].parse::<u32>() else {
        return false;
    };
    hours < 24 && minutes < 60
}
