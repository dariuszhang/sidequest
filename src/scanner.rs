use std::collections::BTreeSet;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

use crate::config::SideQuestConfig;

const RECENT_REPO_DAYS: &str = "14 days ago";

#[derive(Debug, Clone)]
pub struct ScannedRepo {
    pub path: PathBuf,
    pub name: String,
    pub commit_count: usize,
    pub already_in_grind: bool,
}

impl ScannedRepo {
    pub fn display_path(&self) -> String {
        if let Some(home) = dirs::home_dir()
            && let Ok(stripped) = self.path.strip_prefix(&home)
        {
            return format!("~/{}", stripped.display());
        }
        self.path.display().to_string()
    }
}

pub fn scan_recent_repositories(config: &SideQuestConfig) -> Result<Vec<ScannedRepo>> {
    let existing = config
        .grind
        .iter()
        .filter_map(|repo| repo.expanded_path().ok())
        .collect::<BTreeSet<_>>();
    let mut paths = BTreeSet::new();
    for root in scan_roots()? {
        if !root.exists() {
            continue;
        }
        if is_git_repo(&root) {
            paths.insert(root.clone());
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() && is_git_repo(&path) {
                paths.insert(path);
            }
        }
    }

    let mut repos = Vec::new();
    for path in paths {
        let commit_count = recent_commit_count(&path)?;
        if commit_count == 0 {
            continue;
        }
        repos.push(ScannedRepo {
            name: path
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_else(|| path.display().to_string()),
            already_in_grind: existing.contains(&path),
            path,
            commit_count,
        });
    }

    repos.sort_by(|left, right| {
        right
            .commit_count
            .cmp(&left.commit_count)
            .then_with(|| left.name.cmp(&right.name))
    });
    Ok(repos)
}

pub fn prompt_for_repo_selection(config: &SideQuestConfig) -> Result<Vec<PathBuf>> {
    let repos = scan_recent_repositories(config)?;
    if repos.is_empty() {
        println!("No recent git repositories were found.");
        return Ok(Vec::new());
    }

    let mut selected = repos
        .iter()
        .map(|repo| repo.already_in_grind)
        .collect::<Vec<_>>();
    loop {
        println!("Scanning for git repos with recent activity...\n");
        for (index, repo) in repos.iter().enumerate() {
            let mark = if selected[index] { "*" } else { " " };
            let detail = if repo.already_in_grind && selected[index] {
                "already in grind".to_string()
            } else {
                format!(
                    "{} commit{}",
                    repo.commit_count,
                    if repo.commit_count == 1 { "" } else { "s" }
                )
            };
            println!(
                "  {}. [{}] {:<36} ({})",
                index + 1,
                mark,
                repo.display_path(),
                detail
            );
        }

        print!("\nToggle numbers, [a]ll, [n]one, [enter] confirm: ");
        io::stdout().flush()?;
        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        let trimmed = input.trim();
        if trimmed.is_empty() {
            break;
        }
        match trimmed {
            "a" | "A" => selected.fill(true),
            "n" | "N" => selected.fill(false),
            _ => {
                for token in trimmed.split_whitespace() {
                    if let Ok(number) = token.parse::<usize>()
                        && let Some(choice) = selected.get_mut(number.saturating_sub(1))
                    {
                        *choice = !*choice;
                    }
                }
            }
        }
        println!();
    }

    Ok(repos
        .into_iter()
        .zip(selected)
        .filter_map(|(repo, is_selected)| is_selected.then_some(repo.path))
        .collect())
}

fn scan_roots() -> Result<Vec<PathBuf>> {
    let mut roots = Vec::new();
    roots.push(std::env::current_dir().context("failed to read current directory")?);
    if let Some(home) = dirs::home_dir() {
        roots.extend(
            ["code", "projects", "src", "work"]
                .iter()
                .map(|segment| home.join(segment)),
        );
        roots.push(home.join("Desktop/Coding"));
    }
    Ok(roots)
}

fn is_git_repo(path: &Path) -> bool {
    path.join(".git").exists()
}

fn recent_commit_count(repo: &Path) -> Result<usize> {
    let output = Command::new("git")
        .args(["rev-list", "--count", "--since", RECENT_REPO_DAYS, "HEAD"])
        .current_dir(repo)
        .output()
        .with_context(|| format!("failed to inspect git history in {}", repo.display()))?;
    if !output.status.success() {
        return Ok(0);
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<usize>()
        .unwrap_or(0))
}
