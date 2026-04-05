use std::collections::BTreeMap;

use chrono::{Duration, Local};

use crate::config::SideQuestConfig;
use crate::runtime::{RuntimeSnapshot, RuntimeStatus};
use crate::state::{HarvestEntry, HarvestEntryStatus};

pub fn render_status(
    config: &SideQuestConfig,
    snapshot: Option<&RuntimeSnapshot>,
    pending_entries: &[HarvestEntry],
) -> String {
    let mut lines = Vec::new();
    lines.push("SideQuest".to_string());
    lines.push(String::new());

    match snapshot {
        Some(snapshot) => {
            let daemon = match snapshot.status {
                RuntimeStatus::Running => "running",
                RuntimeStatus::Idle => "healthy",
                RuntimeStatus::Starting => "starting",
                RuntimeStatus::Backoff => "backoff",
                RuntimeStatus::Stopped => "stopped",
            };
            let heartbeat = snapshot
                .daemon_heartbeat_at
                .map(|at| {
                    let local = at.with_timezone(&Local);
                    format!("heartbeat {}", local.format("%Y-%m-%d %H:%M"))
                })
                .unwrap_or_else(|| "no heartbeat yet".to_string());
            lines.push(format!("Daemon: {} ({})", daemon, heartbeat));
            if let Some(last_run) = &snapshot.last_run {
                lines.push(format!(
                    "Last run: {} [{}]",
                    last_run.title,
                    format!("{:?}", last_run.status).to_lowercase()
                ));
            }
            if let Some(decision) = &snapshot.scheduler_decision {
                lines.push(format!(
                    "Next window: {} → {}",
                    decision
                        .cutoff_time
                        .with_timezone(&Local)
                        .format("%Y-%m-%d %H:%M"),
                    decision
                        .wake_time
                        .with_timezone(&Local)
                        .format("%Y-%m-%d %H:%M")
                ));
                lines.push(format!("Scheduler: {}", decision.reason));
            }
            if let Some(oracle) = &snapshot.oracle_snapshot
                && !oracle.budgets.is_empty()
            {
                lines.push(String::new());
                lines.push("Budget:".to_string());
                for (provider, budget) in &oracle.budgets {
                    lines.push(format!(
                        "  {}: {:.0}% used (session resets {})",
                        provider,
                        budget.session_utilization * 100.0,
                        relative_reset_time(budget.session_resets_at.with_timezone(&Local))
                    ));
                }
            }
        }
        None => lines.push("Daemon: no runtime snapshot yet".to_string()),
    }

    let grind_total = config.grind.len();
    lines.push(String::new());
    lines.push(format!("Grind repos: {}", grind_total));

    let mut by_repo: BTreeMap<String, usize> = BTreeMap::new();
    let mut pending_quests: BTreeMap<String, usize> = BTreeMap::new();
    for entry in pending_entries
        .iter()
        .filter(|entry| entry.status == HarvestEntryStatus::Pending)
    {
        match entry.mode {
            crate::config::WorkMode::Grind => {
                *by_repo.entry(entry.repo_name.clone()).or_default() += 1;
            }
            crate::config::WorkMode::Quest => {
                let name = entry
                    .quest
                    .clone()
                    .unwrap_or_else(|| entry.repo_name.clone());
                *pending_quests.entry(name).or_default() += 1;
            }
        }
    }

    if by_repo.is_empty() {
        lines.push("  No grind loot waiting.".to_string());
    } else {
        for (repo, count) in by_repo {
            lines.push(format!(
                "  {}: {} improvement{} ready",
                repo,
                count,
                if count == 1 { "" } else { "s" }
            ));
        }
    }

    lines.push(String::new());
    lines.push("Quests:".to_string());
    if config.quests.is_empty() {
        lines.push("  No quests configured.".to_string());
    } else {
        for quest in &config.quests {
            let suffix = pending_quests
                .get(&quest.name)
                .map(|count| {
                    format!(
                        " · {} pending update{}",
                        count,
                        if *count == 1 { "" } else { "s" }
                    )
                })
                .unwrap_or_default();
            lines.push(format!("  {}: {:?}{}", quest.name, quest.status, suffix));
        }
    }

    if pending_entries.is_empty() {
        lines.push(String::new());
        lines.push("No loot waiting.".to_string());
    } else {
        lines.push(String::new());
        lines.push(format!(
            "{} item{} ready for review. Run `sidequest loot` to collect.",
            pending_entries.len(),
            if pending_entries.len() == 1 { "" } else { "s" }
        ));
    }

    lines.join("\n")
}

fn relative_reset_time(at: chrono::DateTime<Local>) -> String {
    let delta = at - Local::now();
    if delta <= Duration::zero() {
        return "now".to_string();
    }

    let hours = delta.num_hours();
    let minutes = (delta - Duration::hours(hours)).num_minutes();
    match (hours, minutes) {
        (0, minutes) => format!("in {}m", minutes),
        (hours, 0) => format!("in {}h", hours),
        (hours, minutes) => format!("in {}h {}m", hours, minutes),
    }
}
