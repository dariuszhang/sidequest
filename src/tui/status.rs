use std::collections::BTreeMap;

use anyhow::Result;
use chrono::{Duration, Local};
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use crate::config::{SideQuestConfig, WorkMode};
use crate::runtime::{RuntimeSnapshot, RuntimeStatus};
use crate::state::{HarvestEntry, HarvestEntryStatus};

use super::{ACCENT, DIM, enter_tui, key_hints, leave_tui};

pub fn run_status(
    config: &SideQuestConfig,
    snapshot: Option<&RuntimeSnapshot>,
    pending_entries: &[HarvestEntry],
) -> Result<()> {
    let mut terminal = enter_tui()?;

    loop {
        terminal.draw(|frame| {
            draw_status(frame, config, snapshot, pending_entries);
        })?;

        if let Event::Key(key) = event::read()? {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc | KeyCode::Enter => break,
                _ => {}
            }
        }
    }

    leave_tui(&mut terminal)?;
    Ok(())
}

fn draw_status(
    frame: &mut ratatui::Frame<'_>,
    config: &SideQuestConfig,
    snapshot: Option<&RuntimeSnapshot>,
    pending_entries: &[HarvestEntry],
) {
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(1)])
        .split(frame.area());

    let block = Block::default()
        .title(" SideQuest ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ACCENT));
    let inner = block.inner(outer[0]);
    frame.render_widget(block, outer[0]);

    // Split inner into columns
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(inner);

    // Left column: daemon + budget
    let left_sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(8), Constraint::Min(4)])
        .split(columns[0]);

    frame.render_widget(daemon_panel(snapshot), left_sections[0]);
    frame.render_widget(budget_panel(snapshot), left_sections[1]);

    // Right column: grind + quests + loot
    let right_sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(8),
            Constraint::Length(8),
            Constraint::Min(3),
        ])
        .split(columns[1]);

    frame.render_widget(grind_panel(config, pending_entries), right_sections[0]);
    frame.render_widget(quest_panel(config, pending_entries), right_sections[1]);
    frame.render_widget(loot_panel(pending_entries), right_sections[2]);

    frame.render_widget(key_hints(&[("q", "quit"), ("enter", "dismiss")]), outer[1]);
}

fn daemon_panel(snapshot: Option<&RuntimeSnapshot>) -> Paragraph<'static> {
    let mut lines = Vec::new();

    match snapshot {
        Some(snap) => {
            let status_str = match snap.status {
                RuntimeStatus::Running => "running",
                RuntimeStatus::Idle => "idle",
                RuntimeStatus::Starting => "starting",
                RuntimeStatus::Backoff => "backoff",
                RuntimeStatus::Stopped => "stopped",
            };
            let status_color = match snap.status {
                RuntimeStatus::Running => ratatui::style::Color::Green,
                RuntimeStatus::Idle => ACCENT,
                _ => ratatui::style::Color::Yellow,
            };

            lines.push(Line::from(vec![
                Span::styled("  Status   ", DIM),
                Span::styled(
                    status_str,
                    Style::default()
                        .fg(status_color)
                        .add_modifier(Modifier::BOLD),
                ),
            ]));

            let heartbeat = snap
                .daemon_heartbeat_at
                .map(|at| at.with_timezone(&Local).format("%H:%M").to_string())
                .unwrap_or_else(|| "—".to_string());
            lines.push(Line::from(vec![
                Span::styled("  Pulse    ", DIM),
                Span::raw(heartbeat),
            ]));

            if let Some(last_run) = &snap.last_run {
                lines.push(Line::from(vec![
                    Span::styled("  Last     ", DIM),
                    Span::raw(last_run.title.clone()),
                ]));
            }

            if let Some(decision) = &snap.scheduler_decision {
                let next = decision
                    .cutoff_time
                    .with_timezone(&Local)
                    .format("%H:%M")
                    .to_string();
                lines.push(Line::from(vec![
                    Span::styled("  Next     ", DIM),
                    Span::raw(format!("{} — {}", next, decision.reason)),
                ]));
            }
        }
        None => {
            lines.push(Line::from(vec![
                Span::styled("  Status   ", DIM),
                Span::styled("no runtime data", DIM),
            ]));
            lines.push(Line::from(Span::styled(
                "  Run `sidequest daemon` or `sidequest install` to start.",
                DIM,
            )));
        }
    }

    Paragraph::new(Text::from(lines))
        .block(
            Block::default()
                .title(" Daemon ")
                .borders(Borders::ALL)
                .border_style(DIM),
        )
        .wrap(Wrap { trim: false })
}

fn budget_panel(snapshot: Option<&RuntimeSnapshot>) -> Paragraph<'static> {
    let mut lines = Vec::new();

    if let Some(snap) = snapshot
        && let Some(oracle) = &snap.oracle_snapshot
        && !oracle.budgets.is_empty()
    {
        for (provider, budget) in &oracle.budgets {
            let pct = (budget.session_utilization * 100.0) as u16;
            let reset = relative_reset_time(budget.session_resets_at.with_timezone(&Local));
            let bar = usage_bar(pct, 20);
            lines.push(Line::from(vec![
                Span::styled(
                    format!("  {provider:<8}"),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::raw(format!("{bar} {pct}%")),
            ]));
            lines.push(Line::from(vec![
                Span::styled("           ", DIM),
                Span::styled(format!("resets {reset}"), DIM),
            ]));
        }
        for failure in &oracle.failures {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("  {:<8}", failure.provider.to_string()),
                    Style::default().fg(ratatui::style::Color::Red),
                ),
                Span::raw(failure.message.clone()),
            ]));
        }
    } else {
        lines.push(Line::from(Span::styled("  No budget data yet.", DIM)));
    }

    Paragraph::new(Text::from(lines))
        .block(
            Block::default()
                .title(" Agent Budget ")
                .borders(Borders::ALL)
                .border_style(DIM),
        )
        .wrap(Wrap { trim: false })
}

fn grind_panel(config: &SideQuestConfig, pending: &[HarvestEntry]) -> Paragraph<'static> {
    let mut lines = Vec::new();

    if config.grind.is_empty() {
        lines.push(Line::from(Span::styled(
            "  No grind repos configured.",
            DIM,
        )));
        lines.push(Line::from(Span::styled(
            "  Run `sidequest grind <path>` to add one.",
            DIM,
        )));
    } else {
        let mut by_repo: BTreeMap<String, usize> = BTreeMap::new();
        for entry in pending
            .iter()
            .filter(|e| e.status == HarvestEntryStatus::Pending && e.mode == WorkMode::Grind)
        {
            *by_repo.entry(entry.repo_name.clone()).or_default() += 1;
        }

        for repo in &config.grind {
            let name = repo.resolved_name().unwrap_or_else(|_| repo.path.clone());
            let pending_count = by_repo.get(&name).copied().unwrap_or(0);
            if pending_count > 0 {
                lines.push(Line::from(vec![
                    Span::styled("  ✓ ", Style::default().fg(ACCENT)),
                    Span::raw(name.to_string()),
                    Span::styled(
                        format!("  {pending_count} pending"),
                        Style::default().fg(ACCENT),
                    ),
                ]));
            } else {
                lines.push(Line::from(vec![
                    Span::styled("  · ", DIM),
                    Span::raw(name.to_string()),
                ]));
            }
        }
    }

    Paragraph::new(Text::from(lines))
        .block(
            Block::default()
                .title(format!(" Grind ({} repos) ", config.grind.len()))
                .borders(Borders::ALL)
                .border_style(DIM),
        )
        .wrap(Wrap { trim: false })
}

fn quest_panel(config: &SideQuestConfig, pending: &[HarvestEntry]) -> Paragraph<'static> {
    let mut lines = Vec::new();

    if config.quests.is_empty() {
        lines.push(Line::from(Span::styled("  No quests configured.", DIM)));
        lines.push(Line::from(Span::styled(
            "  Run `sidequest quest` to start one.",
            DIM,
        )));
    } else {
        let mut pending_quests: BTreeMap<String, usize> = BTreeMap::new();
        for entry in pending
            .iter()
            .filter(|e| e.status == HarvestEntryStatus::Pending && e.mode == WorkMode::Quest)
        {
            let name = entry
                .quest
                .clone()
                .unwrap_or_else(|| entry.repo_name.clone());
            *pending_quests.entry(name).or_default() += 1;
        }

        for quest in &config.quests {
            let status = format!("{:?}", quest.status).to_lowercase();
            let pending_count = pending_quests.get(&quest.name).copied().unwrap_or(0);
            let icon = match quest.status {
                crate::config::QuestStatus::Active => {
                    Span::styled("  ▶ ", Style::default().fg(ACCENT))
                }
                crate::config::QuestStatus::Paused => Span::styled("  ⏸ ", DIM),
                crate::config::QuestStatus::Completed => {
                    Span::styled("  ✓ ", Style::default().fg(ratatui::style::Color::Green))
                }
            };
            let mut spans = vec![
                icon,
                Span::raw(format!("{:<14}", quest.name)),
                Span::styled(status.to_string(), DIM),
            ];
            if pending_count > 0 {
                spans.push(Span::styled(
                    format!("  · {pending_count} new"),
                    Style::default().fg(ACCENT),
                ));
            }
            lines.push(Line::from(spans));
        }
    }

    Paragraph::new(Text::from(lines))
        .block(
            Block::default()
                .title(" Quests ")
                .borders(Borders::ALL)
                .border_style(DIM),
        )
        .wrap(Wrap { trim: false })
}

fn loot_panel(pending_entries: &[HarvestEntry]) -> Paragraph<'static> {
    let pending: Vec<_> = pending_entries
        .iter()
        .filter(|e| e.status == HarvestEntryStatus::Pending)
        .collect();

    let lines = if pending.is_empty() {
        vec![Line::from(Span::styled("  No loot waiting.", DIM))]
    } else {
        vec![
            Line::from(vec![
                Span::styled("  ", Style::default()),
                Span::styled(
                    format!("{}", pending.len()),
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                ),
                Span::raw(format!(
                    " item{} ready for review",
                    if pending.len() == 1 { "" } else { "s" }
                )),
            ]),
            Line::from(Span::styled("  Run `sidequest loot` to collect.", DIM)),
        ]
    };

    Paragraph::new(Text::from(lines))
        .block(
            Block::default()
                .title(" Loot ")
                .borders(Borders::ALL)
                .border_style(DIM),
        )
        .wrap(Wrap { trim: false })
}

fn usage_bar(pct: u16, width: usize) -> String {
    let filled = (pct as usize * width / 100).min(width);
    let empty = width - filled;
    format!("[{}{}]", "█".repeat(filled), "░".repeat(empty))
}

fn relative_reset_time(at: chrono::DateTime<Local>) -> String {
    let delta = at - Local::now();
    if delta <= Duration::zero() {
        return "now".to_string();
    }

    let hours = delta.num_hours();
    let minutes = (delta - Duration::hours(hours)).num_minutes();
    match (hours, minutes) {
        (0, m) => format!("in {m}m"),
        (h, 0) => format!("in {h}h"),
        (h, m) => format!("in {h}h {m}m"),
    }
}
