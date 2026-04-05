use std::collections::BTreeMap;
use std::io::{self, Write};
use std::process::Command;

use anyhow::{Context, Result, bail};
use chrono::Utc;

use crate::config::SideQuestPaths;
use crate::config::WorkMode;
use crate::harvester::GitRepoSession;
use crate::runtime::{ControlRequest, ControlRequestKind, append_control_request, next_request_id};
use crate::state::{
    HarvestEntry, HarvestEntryStatus, HarvestLedger, read_harvest_ledger, write_harvest_ledger,
};
use crate::tui::loot::{LootDecision, LootInput, LootOutcome, run_loot_review};

pub fn run(paths: &SideQuestPaths) -> Result<()> {
    let mut ledger = read_harvest_ledger(paths)?;
    let pending = pending_entries(&ledger);
    if pending.is_empty() {
        println!("No loot waiting.");
        return Ok(());
    }

    let input = LootInput {
        entries: ledger
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| e.status == HarvestEntryStatus::Pending)
            .map(|(i, e)| (i, e.clone()))
            .collect(),
    };
    let outcome = run_loot_review(input, peek_entries)?;

    match outcome {
        LootOutcome::Cancelled => return Ok(()),
        LootOutcome::Decisions(decisions) => {
            for decision in &decisions {
                match decision {
                    LootDecision::Accept(idx) => {
                        if let Err(error) = accept_grind_entry(&mut ledger, *idx) {
                            println!(
                                "Warning: could not accept `{}`: {}",
                                ledger.entries[*idx].title, error
                            );
                            match resolve_grind_accept_error(paths, &mut ledger, *idx)? {
                                AcceptResolution::Dismissed | AcceptResolution::Skipped => {}
                                AcceptResolution::Quit => break,
                            }
                        }
                    }
                    LootDecision::Dismiss(idx) => {
                        mark_entry(&mut ledger, *idx, HarvestEntryStatus::Dismissed);
                    }
                    LootDecision::Acknowledge(idx) => {
                        mark_entry(&mut ledger, *idx, HarvestEntryStatus::Acknowledged);
                    }
                }
            }
            cleanup_resolved_branches(&ledger)?;
        }
    }

    write_harvest_ledger(paths, &ledger)?;
    if pending_entries(&ledger).is_empty() {
        append_control_request(
            paths,
            &ControlRequest {
                id: next_request_id(),
                created_at: Utc::now(),
                kind: ControlRequestKind::HarvestCompleted,
            },
        )?;
    }
    Ok(())
}

fn pending_entries(ledger: &HarvestLedger) -> Vec<HarvestEntry> {
    ledger
        .entries
        .iter()
        .filter(|entry| entry.status == HarvestEntryStatus::Pending)
        .cloned()
        .collect()
}

fn accept_grind_entry(ledger: &mut HarvestLedger, index: usize) -> Result<()> {
    let entry = ledger.entries[index].clone();
    if entry.commit.trim().is_empty() {
        bail!("cannot accept loot without a commit hash");
    }

    let session = GitRepoSession::open(&entry.repo_path)?;
    let target_branch = session.preferred_target_branch()?;
    session.cherry_pick_to_branch(&entry.commit, &target_branch)?;
    mark_entry(ledger, index, HarvestEntryStatus::Accepted);
    Ok(())
}

fn resolve_grind_accept_error(
    paths: &SideQuestPaths,
    ledger: &mut HarvestLedger,
    index: usize,
) -> Result<AcceptResolution> {
    loop {
        print!("[d]ismiss  [s]kip  [q]uit & save\n> ");
        io::stdout().flush()?;
        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        match input.trim() {
            "d" | "D" => {
                mark_entry(ledger, index, HarvestEntryStatus::Dismissed);
                return Ok(AcceptResolution::Dismissed);
            }
            "s" | "S" => return Ok(AcceptResolution::Skipped),
            "q" | "Q" => {
                write_harvest_ledger(paths, ledger)?;
                return Ok(AcceptResolution::Quit);
            }
            _ => {}
        }
    }
}

fn mark_entry(ledger: &mut HarvestLedger, index: usize, status: HarvestEntryStatus) {
    if let Some(entry) = ledger.entries.get_mut(index) {
        entry.status = status;
        entry.looted_at = Some(Utc::now());
    }
}

fn peek_entries(entries: &[HarvestEntry]) -> Result<()> {
    if entries.is_empty() {
        return Ok(());
    }

    let session = GitRepoSession::open(&entries[0].repo_path)?;
    let mut patch = String::new();
    for (offset, entry) in entries.iter().enumerate() {
        if offset > 0 {
            patch.push_str("\n\n");
        }
        patch.push_str(&session.show_commit_patch(&entry.commit)?);
    }
    show_in_pager(&patch)
}

fn show_in_pager(contents: &str) -> Result<()> {
    let pager = std::env::var("PAGER").unwrap_or_else(|_| "less -R".to_string());
    let status = Command::new("sh")
        .arg("-lc")
        .arg(&pager)
        .stdin(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            if let Some(mut stdin) = child.stdin.take() {
                use std::io::Write as _;
                stdin.write_all(contents.as_bytes())?;
            }
            child.wait()
        })
        .context("failed to open pager")?;
    if !status.success() {
        println!("{}", contents);
    }
    Ok(())
}

fn cleanup_resolved_branches(ledger: &HarvestLedger) -> Result<()> {
    let mut by_branch: BTreeMap<(std::path::PathBuf, String), Vec<&HarvestEntry>> = BTreeMap::new();
    for entry in ledger
        .entries
        .iter()
        .filter(|entry| entry.mode == WorkMode::Grind)
    {
        by_branch
            .entry((entry.repo_path.clone(), entry.branch.clone()))
            .or_default()
            .push(entry);
    }

    for ((repo_path, branch), entries) in by_branch {
        if entries
            .iter()
            .any(|entry| entry.status == HarvestEntryStatus::Pending)
        {
            continue;
        }
        if let Ok(session) = GitRepoSession::open(&repo_path) {
            let _ = session.delete_branch(&branch);
        }
    }

    Ok(())
}

enum AcceptResolution {
    Dismissed,
    Skipped,
    Quit,
}
