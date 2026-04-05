use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Local, NaiveDate};

use crate::config::{SideQuestPaths, WorkMode};
use crate::oracle::ProviderKind;
use crate::state::{
    HarvestEntry, HarvestEntryStatus, HarvestLedger, clear_session_report,
    mark_pending_entries_stale, update_pending_entry_commits,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HarvestTaskStatus {
    Completed,
    Failed,
}

#[derive(Debug, Clone)]
pub struct HarvestTask {
    pub status: HarvestTaskStatus,
    pub mode: WorkMode,
    pub title: String,
    pub repository: PathBuf,
    pub provider: ProviderKind,
    pub branch: String,
    pub summary: String,
    pub commit: Option<String>,
    pub short_stat: Option<String>,
    pub tests_added: Option<usize>,
    pub tests_passing: Option<bool>,
    pub next_step: Option<String>,
}

#[derive(Debug, Clone)]
pub struct HarvestRecord {
    pub finished_at: DateTime<Local>,
    pub providers: Vec<ProviderKind>,
    pub tasks: Vec<HarvestTask>,
    pub spend_used_fraction: Option<f64>,
}

impl HarvestRecord {
    pub fn banner(&self) -> String {
        let completed = self
            .tasks
            .iter()
            .filter(|task| task.status == HarvestTaskStatus::Completed)
            .count();
        format!(
            "🗡️ SideQuest: {} task{} completed overnight. Run `sidequest loot` to review.",
            completed,
            if completed == 1 { "" } else { "s" }
        )
    }

    pub fn to_markdown(&self) -> String {
        let mut markdown = String::new();
        markdown.push_str(&format!(
            "# SideQuest loot journal — {}\n\n",
            self.finished_at.format("%Y-%m-%d %H:%M")
        ));
        markdown.push_str(&format!(
            "- Providers: `{}`\n",
            self.providers
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ));
        if let Some(used) = self.spend_used_fraction {
            markdown.push_str(&format!(
                "- Spend used: {:.0}% of safe budget\n",
                used * 100.0
            ));
        }
        markdown.push('\n');

        for task in &self.tasks {
            let status = match task.status {
                HarvestTaskStatus::Completed => "completed",
                HarvestTaskStatus::Failed => "failed",
            };
            markdown.push_str(&format!(
                "## [{} / {}] {}\n\n",
                task.mode, status, task.title
            ));
            markdown.push_str(&format!("- Repository: `{}`\n", task.repository.display()));
            markdown.push_str(&format!("- Provider: `{}`\n", task.provider));
            markdown.push_str(&format!("- Branch: `{}`\n", task.branch));
            if let Some(commit) = &task.commit {
                markdown.push_str(&format!("- Commit: `{}`\n", commit));
            }
            if let Some(short_stat) = &task.short_stat {
                markdown.push_str(&format!("- Diff: {}\n", short_stat));
            }
            if let Some(tests_added) = task.tests_added {
                markdown.push_str(&format!("- Tests added: `{tests_added}`\n"));
            }
            if let Some(tests_passing) = task.tests_passing {
                markdown.push_str(&format!(
                    "- Tests passing: {}\n",
                    if tests_passing { "yes" } else { "no" }
                ));
            }
            markdown.push_str(&format!("- Summary: {}\n", task.summary));
            if let Some(next_step) = &task.next_step {
                markdown.push_str(&format!("- Next step: {}\n", next_step));
            }
            markdown.push('\n');
        }

        markdown
    }
}

pub fn format_pending_harvest_banner(entries: &[HarvestEntry]) -> String {
    if entries.is_empty() {
        return "🗡️ SideQuest has no pending loot.\n".to_string();
    }

    let mut by_repo: BTreeMap<String, (usize, BTreeSet<String>)> = BTreeMap::new();
    for entry in entries {
        let label = entry
            .repo_path
            .file_name()
            .map(|name| format!("{}/", name.to_string_lossy()))
            .unwrap_or_else(|| entry.repo_path.display().to_string());
        let bucket = by_repo.entry(label).or_default();
        bucket.0 += 1;
        bucket.1.insert(entry.mode.to_string());
    }

    let mut output = String::new();
    output.push_str(&format!(
        "🗡️ SideQuest completed {} task{} overnight across {} repo{}:\n\n",
        entries.len(),
        if entries.len() == 1 { "" } else { "s" },
        by_repo.len(),
        if by_repo.len() == 1 { "" } else { "s" }
    ));
    for (repo, (count, modes)) in by_repo {
        output.push_str(&format!(
            "  {repo:<16} {count} task{} ({})\n",
            if count == 1 { " " } else { "s" },
            modes.into_iter().collect::<Vec<_>>().join(", ")
        ));
    }
    output.push_str("\n  Run `sidequest loot` to review and collect it.\n");
    output
}

pub fn short_pending_harvest_banner(entries: &[HarvestEntry]) -> String {
    if entries.is_empty() {
        return "No pending loot.".to_string();
    }
    let repos = entries
        .iter()
        .map(|entry| entry.repo_path.clone())
        .collect::<BTreeSet<_>>()
        .len();
    format!(
        "{} task{} completed across {} repo{}. Run `sidequest loot` to review.",
        entries.len(),
        if entries.len() == 1 { "" } else { "s" },
        repos,
        if repos == 1 { "" } else { "s" }
    )
}

#[derive(Debug, Clone)]
pub struct Harvester {
    paths: SideQuestPaths,
}

impl Harvester {
    pub fn new(paths: SideQuestPaths) -> Self {
        Self { paths }
    }

    pub fn write_harvest(&self, record: &HarvestRecord) -> Result<PathBuf> {
        self.paths.ensure()?;
        let path = self
            .paths
            .harvests_dir
            .join(format!("{}.md", record.finished_at.date_naive()));

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("failed to open {}", path.display()))?;
        if file.metadata()?.len() > 0 {
            file.write_all(b"\n---\n\n")?;
        }
        file.write_all(record.to_markdown().as_bytes())?;

        fs::write(
            &self.paths.latest_harvest_file,
            format!("{}\n", record.banner()),
        )
        .with_context(|| {
            format!(
                "failed to write {}",
                self.paths.latest_harvest_file.display()
            )
        })?;

        Ok(path)
    }

    pub fn read_latest_text(&self) -> Result<Option<String>> {
        if !self.paths.harvests_dir.exists() {
            return Ok(None);
        }
        let mut files: Vec<_> = fs::read_dir(&self.paths.harvests_dir)?
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension() == Some(OsStr::new("md")))
            .collect();
        files.sort();
        if let Some(path) = files.pop() {
            return Ok(Some(
                fs::read_to_string(&path)
                    .with_context(|| format!("failed to read {}", path.display()))?,
            ));
        }
        Ok(None)
    }

    pub fn read_by_date(&self, date: NaiveDate) -> Result<Option<String>> {
        let path = self.paths.harvests_dir.join(format!("{date}.md"));
        if !path.exists() {
            return Ok(None);
        }
        Ok(Some(fs::read_to_string(&path).with_context(|| {
            format!("failed to read {}", path.display())
        })?))
    }
}

#[derive(Debug, Clone)]
pub struct GitRepoSession {
    repo: PathBuf,
    original_checkout: OriginalCheckout,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BranchAction {
    Created,
    CheckedOut,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrindBranchState {
    Fresh,
    Continuing,
    Rebased { old_commit_count: usize },
    FreshAfterStale { stale_count: usize },
}

#[derive(Debug, Clone)]
enum OriginalCheckout {
    Branch(String),
    Detached(String),
}

impl GitRepoSession {
    pub fn open(repo: impl AsRef<Path>) -> Result<Self> {
        let repo = repo.as_ref().to_path_buf();
        assert_git_repo(&repo)?;
        let original_checkout = detect_original_checkout(&repo)?;
        Ok(Self {
            repo,
            original_checkout,
        })
    }

    pub fn initialize(path: impl AsRef<Path>, name: &str) -> Result<()> {
        let repo = path.as_ref();
        fs::create_dir_all(repo).with_context(|| format!("failed to create {}", repo.display()))?;
        if repo.join(".git").exists() {
            return Ok(());
        }
        run_git(repo, ["init"])?;
        run_git(repo, ["config", "user.name", "SideQuest"])?;
        run_git(repo, ["config", "user.email", "sidequest@local"])?;
        let readme = repo.join("README.md");
        fs::write(
            &readme,
            format!("# {name}\n\nThis repository is managed by SideQuest.\n"),
        )
        .with_context(|| format!("failed to write {}", readme.display()))?;
        run_git(repo, ["add", "README.md"])?;
        run_git(repo, ["commit", "-m", "Initialize quest repository"])?;
        Ok(())
    }

    pub fn create_branch(&self, branch: &str) -> Result<()> {
        run_git(&self.repo, ["checkout", "-b", branch]).map(|_| ())
    }

    pub fn checkout_or_create_sidequest_branch(&self, branch: &str) -> Result<BranchAction> {
        if self.branch_exists(branch)? {
            run_git(&self.repo, ["checkout", branch])?;
            return Ok(BranchAction::CheckedOut);
        }

        run_git(&self.repo, ["checkout", "-b", branch])?;
        Ok(BranchAction::Created)
    }

    pub fn has_changes(&self) -> Result<bool> {
        let output = Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(&self.repo)
            .output()
            .context("failed to check git status")?;
        if !output.status.success() {
            bail!("git status failed");
        }
        Ok(!String::from_utf8_lossy(&output.stdout).trim().is_empty())
    }

    pub fn commit_all(&self, message: &str) -> Result<Option<String>> {
        run_git(&self.repo, ["add", "-A"])?;
        run_git(&self.repo, ["reset", "--", ".sidequest"])?;
        let staged = Command::new("git")
            .args(["diff", "--cached", "--quiet"])
            .current_dir(&self.repo)
            .status()
            .context("failed to inspect staged git diff")?;
        if staged.success() {
            return Ok(None);
        }
        run_git(&self.repo, ["commit", "-m", message])?;
        self.latest_commit()
    }

    pub fn cleanup_before_restore(&self) -> Result<()> {
        run_git(&self.repo, ["reset", "--hard", "HEAD"])?;
        run_git(&self.repo, ["clean", "-fd"])?;
        clear_session_report(&self.repo)?;
        Ok(())
    }

    pub fn latest_commit(&self) -> Result<Option<String>> {
        let commit = run_git(&self.repo, ["rev-parse", "HEAD"])?;
        Ok(Some(commit.trim().to_string()))
    }

    pub fn short_stat(&self, range: &str) -> Result<Option<String>> {
        let output = run_git(&self.repo, ["diff", "--shortstat", range])?;
        let trimmed = output.trim();
        if trimmed.is_empty() {
            Ok(None)
        } else {
            Ok(Some(trimmed.to_string()))
        }
    }

    pub fn restore(&self) -> Result<()> {
        self.restore_checkout(&self.original_checkout)
    }

    pub fn repo_path(&self) -> &Path {
        &self.repo
    }

    pub fn branch_exists(&self, branch: &str) -> Result<bool> {
        git_command_succeeds(&self.repo, ["rev-parse", "--verify", branch])
            .context("failed to inspect branch state")
    }

    pub fn delete_branch(&self, branch: &str) -> Result<()> {
        run_git(&self.repo, ["branch", "-D", branch]).map(|_| ())
    }

    pub fn commit_exists(&self, commit: &str) -> Result<bool> {
        let object = format!("{commit}^{{commit}}");
        git_command_succeeds(&self.repo, ["cat-file", "-e", object.as_str()])
            .context("failed to inspect commit state")
    }

    pub fn show_commit_patch(&self, commit: &str) -> Result<String> {
        run_git(&self.repo, ["show", "--stat", "--patch", commit])
    }

    /// Cherry-pick a specific commit onto a target branch, then return to the original checkout.
    pub fn cherry_pick_to_branch(&self, commit: &str, target_branch: &str) -> Result<()> {
        if commit.trim().is_empty() {
            bail!("cannot cherry-pick an empty commit id");
        }

        let restore_to = detect_original_checkout(&self.repo)?;

        if self.branch_exists(target_branch)? {
            run_git(&self.repo, ["checkout", target_branch])?;
        } else {
            let base = self.preferred_target_branch()?;
            run_git(&self.repo, ["checkout", "-b", target_branch, base.as_str()])?;
        }

        let cherry_pick = Command::new("git")
            .args(["cherry-pick", commit])
            .current_dir(&self.repo)
            .output()
            .context("failed to cherry-pick commit")?;

        if !cherry_pick.status.success() {
            let _ = run_git(&self.repo, ["cherry-pick", "--abort"]);
            self.restore_checkout(&restore_to)?;
            bail!(
                "failed to cherry-pick {commit} onto {target_branch}: {}",
                String::from_utf8_lossy(&cherry_pick.stderr).trim()
            );
        }

        self.restore_checkout(&restore_to)?;
        Ok(())
    }

    pub fn preferred_target_branch(&self) -> Result<String> {
        for candidate in ["main", "master"] {
            if self.branch_exists(candidate)? {
                return Ok(candidate.to_string());
            }
        }

        let current = run_git(&self.repo, ["rev-parse", "--abbrev-ref", "HEAD"])?;
        Ok(current.trim().to_string())
    }

    fn restore_checkout(&self, checkout: &OriginalCheckout) -> Result<()> {
        match checkout {
            OriginalCheckout::Branch(branch) => {
                run_git(&self.repo, ["checkout", branch]).map(|_| ())
            }
            OriginalCheckout::Detached(commit) => {
                run_git(&self.repo, ["checkout", "--detach", commit]).map(|_| ())
            }
        }
    }
}

pub fn sidequest_branch_name(mode: WorkMode, scope: impl AsRef<str>) -> String {
    let slug = slugify(scope.as_ref());
    match mode {
        WorkMode::Grind => format!("sidequest/grind/{slug}"),
        WorkMode::Quest => format!("sidequest/quest/{slug}"),
    }
}

pub fn prepare_grind_branch(
    repo: &Path,
    branch: &str,
    ledger: &mut HarvestLedger,
) -> Result<GrindBranchState> {
    let session = GitRepoSession::open(repo)?;
    let target_branch = session.preferred_target_branch()?;
    let pending_count = pending_commit_hashes(ledger, repo, branch).len();

    if pending_count == 0 {
        reset_sidequest_branch(&session, branch, &target_branch)?;
        return Ok(GrindBranchState::Fresh);
    }

    if !session.branch_exists(branch)? {
        let stale_count = mark_pending_entries_stale(
            ledger,
            repo,
            branch,
            "Pending grind work became unreachable because the SideQuest branch was missing.",
        );
        reset_sidequest_branch(&session, branch, &target_branch)?;
        return Ok(GrindBranchState::FreshAfterStale { stale_count });
    }

    let target_head = run_git(repo, ["rev-parse", target_branch.as_str()])?;
    let merge_base = run_git(repo, ["merge-base", target_branch.as_str(), branch])?;
    let has_resolved_entries = ledger.entries.iter().any(|entry| {
        entry.repo_path == repo
            && entry.branch == branch
            && entry.status != HarvestEntryStatus::Pending
    });

    if !has_resolved_entries && merge_base.trim() == target_head.trim() {
        return Ok(GrindBranchState::Continuing);
    }

    refresh_pending_grind_branch(&session, branch, &target_branch, ledger)
}

fn pending_commit_hashes(ledger: &HarvestLedger, repo: &Path, branch: &str) -> Vec<String> {
    let mut pending: Vec<_> = ledger
        .entries
        .iter()
        .filter(|entry| {
            entry.status == HarvestEntryStatus::Pending
                && entry.repo_path == repo
                && entry.branch == branch
        })
        .collect();
    pending.sort_by_key(|entry| entry.created_at);
    pending
        .into_iter()
        .map(|entry| entry.commit.clone())
        .collect()
}

fn reset_sidequest_branch(
    session: &GitRepoSession,
    branch: &str,
    target_branch: &str,
) -> Result<()> {
    let restore_to = detect_original_checkout(session.repo_path())?;
    if matches!(&restore_to, OriginalCheckout::Branch(current) if current == branch) {
        run_git(session.repo_path(), ["checkout", target_branch])?;
    }

    if session.branch_exists(branch)? {
        run_git(session.repo_path(), ["branch", "-f", branch, target_branch])?;
    } else {
        run_git(session.repo_path(), ["branch", branch, target_branch])?;
    }

    session.restore_checkout(&restore_to)
}

fn refresh_pending_grind_branch(
    session: &GitRepoSession,
    branch: &str,
    target_branch: &str,
    ledger: &mut HarvestLedger,
) -> Result<GrindBranchState> {
    let restore_to = detect_original_checkout(session.repo_path())?;
    let pending_commits = pending_commit_hashes(ledger, session.repo_path(), branch);
    let pending_count = pending_commits.len();
    let temp_branch = format!("{branch}__refresh");

    if matches!(&restore_to, OriginalCheckout::Branch(current) if current == branch) {
        run_git(session.repo_path(), ["checkout", target_branch])?;
    }
    if session.branch_exists(&temp_branch)? {
        run_git(session.repo_path(), ["branch", "-D", temp_branch.as_str()])?;
    }

    for commit in &pending_commits {
        if commit.trim().is_empty() || !session.commit_exists(commit)? {
            return cleanup_refresh_failure(
                session,
                branch,
                target_branch,
                ledger,
                &restore_to,
                "Pending grind work became unreachable while refreshing the SideQuest branch.",
                None,
            );
        }
    }

    run_git(
        session.repo_path(),
        ["checkout", "-B", temp_branch.as_str(), target_branch],
    )?;

    let mut rewritten_commits = Vec::with_capacity(pending_count);
    for commit in &pending_commits {
        let cherry_pick = Command::new("git")
            .args(["cherry-pick", commit.as_str()])
            .current_dir(session.repo_path())
            .output()
            .context("failed to refresh pending grind commits")?;
        if !cherry_pick.status.success() {
            let stderr = String::from_utf8_lossy(&cherry_pick.stderr)
                .trim()
                .to_string();
            return cleanup_refresh_failure(
                session,
                branch,
                target_branch,
                ledger,
                &restore_to,
                "Pending grind work became stale after it could not be replayed onto the current main branch.",
                Some(stderr),
            );
        }

        rewritten_commits.push(
            run_git(session.repo_path(), ["rev-parse", "HEAD"])?
                .trim()
                .to_string(),
        );
    }

    if session.branch_exists(branch)? {
        run_git(session.repo_path(), ["branch", "-D", branch])?;
    }
    run_git(
        session.repo_path(),
        ["branch", "-m", temp_branch.as_str(), branch],
    )?;
    session.restore_checkout(&restore_to)?;
    update_pending_entry_commits(ledger, session.repo_path(), branch, &rewritten_commits);

    Ok(GrindBranchState::Rebased {
        old_commit_count: pending_count,
    })
}

fn cleanup_refresh_failure(
    session: &GitRepoSession,
    branch: &str,
    target_branch: &str,
    ledger: &mut HarvestLedger,
    restore_to: &OriginalCheckout,
    note: &str,
    stderr: Option<String>,
) -> Result<GrindBranchState> {
    let refresh_branch = format!("{branch}__refresh");
    let _ = run_git(session.repo_path(), ["cherry-pick", "--abort"]);
    let _ = run_git(session.repo_path(), ["checkout", target_branch]);
    if session.branch_exists(&refresh_branch)? {
        let _ = run_git(
            session.repo_path(),
            ["branch", "-D", refresh_branch.as_str()],
        );
    }

    let stale_note = stderr
        .filter(|text| !text.trim().is_empty())
        .map(|text| format!("{note} {text}"))
        .unwrap_or_else(|| note.to_string());
    let stale_count = mark_pending_entries_stale(ledger, session.repo_path(), branch, stale_note);
    reset_sidequest_branch(session, branch, target_branch)?;
    session.restore_checkout(restore_to)?;

    Ok(GrindBranchState::FreshAfterStale { stale_count })
}

pub fn slugify(input: &str) -> String {
    let mut slug = String::new();
    let mut last_was_dash = false;
    for character in input.chars() {
        if character.is_ascii_alphanumeric() {
            slug.push(character.to_ascii_lowercase());
            last_was_dash = false;
        } else if !last_was_dash {
            slug.push('-');
            last_was_dash = true;
        }
    }
    slug.trim_matches('-').to_string()
}

fn assert_git_repo(path: &Path) -> Result<()> {
    let output = Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(path)
        .output()
        .with_context(|| format!("failed to inspect git state for {}", path.display()))?;
    if !output.status.success() || String::from_utf8_lossy(&output.stdout).trim() != "true" {
        bail!("{} is not a git repository", path.display());
    }
    Ok(())
}

fn detect_original_checkout(repo: &Path) -> Result<OriginalCheckout> {
    let symbolic_ref = Command::new("git")
        .args(["symbolic-ref", "-q", "--short", "HEAD"])
        .current_dir(repo)
        .output()
        .with_context(|| format!("failed to inspect git checkout for {}", repo.display()))?;
    if symbolic_ref.status.success() {
        return Ok(OriginalCheckout::Branch(
            String::from_utf8_lossy(&symbolic_ref.stdout)
                .trim()
                .to_string(),
        ));
    }

    let commit = run_git(repo, ["rev-parse", "HEAD"])?;
    Ok(OriginalCheckout::Detached(commit.trim().to_string()))
}

pub fn run_git<I, S>(repo: &Path, args: I) -> Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let owned: Vec<String> = args
        .into_iter()
        .map(|arg| arg.as_ref().to_string())
        .collect();
    let output = Command::new("git")
        .args(&owned)
        .current_dir(repo)
        .output()
        .with_context(|| format!("failed to run git in {}", repo.display()))?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            owned.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn git_command_succeeds<I, S>(repo: &Path, args: I) -> Result<bool>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let owned: Vec<String> = args
        .into_iter()
        .map(|arg| arg.as_ref().to_string())
        .collect();
    let output = Command::new("git")
        .args(&owned)
        .current_dir(repo)
        .output()
        .with_context(|| format!("failed to run git in {}", repo.display()))?;
    Ok(output.status.success())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn init_repo() -> (tempfile::TempDir, PathBuf) {
        let temp = tempdir().expect("temp dir");
        let path = temp.path().join("repo");
        fs::create_dir_all(&path).expect("repo dir");
        run_git(&path, ["init"]).expect("git init");
        run_git(&path, ["config", "user.name", "SideQuest Tester"]).expect("git config");
        run_git(&path, ["config", "user.email", "sidequest@test.local"]).expect("git config");
        fs::write(path.join("README.md"), "# Repo\n").expect("readme");
        run_git(&path, ["add", "README.md"]).expect("git add");
        run_git(&path, ["commit", "-m", "Initial commit"]).expect("git commit");
        (temp, path)
    }

    #[test]
    fn branch_name_uses_stable_targets() {
        assert_eq!(
            sidequest_branch_name(WorkMode::Grind, "my-app"),
            "sidequest/grind/my-app"
        );
        assert_eq!(
            sidequest_branch_name(WorkMode::Grind, "backend-api"),
            "sidequest/grind/backend-api"
        );
        assert_eq!(
            sidequest_branch_name(WorkMode::Quest, "dotfile-manager"),
            "sidequest/quest/dotfile-manager"
        );
    }

    #[test]
    fn checkout_or_create_sidequest_branch_creates_then_reuses() {
        let (_temp, repo) = init_repo();
        let session = GitRepoSession::open(&repo).expect("session");
        let first = session
            .checkout_or_create_sidequest_branch("sidequest-nightly")
            .expect("first checkout");
        let second = session
            .checkout_or_create_sidequest_branch("sidequest-nightly")
            .expect("second checkout");

        assert_eq!(first, BranchAction::Created);
        assert_eq!(second, BranchAction::CheckedOut);
    }

    #[test]
    fn create_branch_commit_and_restore() {
        let (_temp, repo) = init_repo();
        let original_branch =
            run_git(&repo, ["rev-parse", "--abbrev-ref", "HEAD"]).expect("original branch");
        let session = GitRepoSession::open(&repo).expect("session");
        session
            .checkout_or_create_sidequest_branch("sidequest-nightly")
            .expect("branch");
        fs::write(repo.join("README.md"), "# Repo\n\nUpdated.\n").expect("write file");
        let commit = session
            .commit_all("Update readme")
            .expect("commit")
            .expect("commit sha");
        assert!(!commit.is_empty());
        session.restore().expect("restore");
        let branch = run_git(&repo, ["rev-parse", "--abbrev-ref", "HEAD"]).expect("branch");
        assert_eq!(branch.trim(), original_branch.trim());
    }

    #[test]
    fn cleanup_before_restore_discards_dirty_worktree_changes() {
        let (_temp, repo) = init_repo();
        let original_branch =
            run_git(&repo, ["rev-parse", "--abbrev-ref", "HEAD"]).expect("original branch");
        let session = GitRepoSession::open(&repo).expect("session");
        session.create_branch("sidequest-nightly").expect("branch");
        fs::write(repo.join("README.md"), "# Repo\n\nDirty.\n").expect("write file");
        fs::write(repo.join("scratch.txt"), "temporary").expect("scratch file");
        fs::create_dir_all(repo.join(".sidequest")).expect("report dir");
        fs::write(
            repo.join(".sidequest").join("session-report.json"),
            r#"{"completed":[],"attempted_but_failed":[],"remaining_work":[]}"#,
        )
        .expect("session report");

        session.cleanup_before_restore().expect("cleanup");
        session.restore().expect("restore");

        let branch = run_git(&repo, ["rev-parse", "--abbrev-ref", "HEAD"]).expect("branch");
        let contents = fs::read_to_string(repo.join("README.md")).expect("readme");
        let status = run_git(&repo, ["status", "--porcelain"]).expect("status");

        assert_eq!(branch.trim(), original_branch.trim());
        assert_eq!(contents.trim(), "# Repo");
        assert!(status.trim().is_empty());
        assert!(!repo.join(".sidequest").exists());
    }

    #[test]
    fn commit_all_excludes_sidequest_metadata() {
        let (_temp, repo) = init_repo();
        let session = GitRepoSession::open(&repo).expect("session");
        fs::write(repo.join("README.md"), "# Repo\n\nUpdated.\n").expect("write file");
        fs::create_dir_all(repo.join(".sidequest")).expect("report dir");
        fs::write(
            repo.join(".sidequest").join("session-report.json"),
            r#"{"completed":[],"attempted_but_failed":[],"remaining_work":[]}"#,
        )
        .expect("session report");

        let commit = session
            .commit_all("Update readme")
            .expect("commit")
            .expect("commit sha");
        let tree = run_git(&repo, ["ls-tree", "-r", "--name-only", commit.trim()]).expect("tree");

        assert!(tree.lines().any(|line| line == "README.md"));
        assert!(!tree.lines().any(|line| line.starts_with(".sidequest/")));
    }

    #[test]
    fn slugify_strips_special_characters_and_collapses_dashes() {
        assert_eq!(slugify("Hello, World!"), "hello-world");
        assert_eq!(slugify("---leading---"), "leading");
        assert_eq!(slugify("CamelCase123"), "camelcase123");
        assert_eq!(slugify("a  b  c"), "a-b-c");
        assert_eq!(slugify(""), "");
    }

    #[test]
    fn restore_returns_detached_head_to_original_commit() {
        let (_temp, repo) = init_repo();
        let original_commit = run_git(&repo, ["rev-parse", "HEAD"]).expect("original commit");
        run_git(&repo, ["checkout", "--detach", original_commit.trim()]).expect("detach head");

        let session = GitRepoSession::open(&repo).expect("session");
        session.create_branch("sidequest-nightly").expect("branch");
        fs::write(repo.join("README.md"), "# Repo\n\nDetached.\n").expect("write file");
        session.commit_all("Update readme").expect("commit");

        session.restore().expect("restore");

        let branch = run_git(&repo, ["rev-parse", "--abbrev-ref", "HEAD"]).expect("branch");
        let commit = run_git(&repo, ["rev-parse", "HEAD"]).expect("commit");
        assert_eq!(branch.trim(), "HEAD");
        assert_eq!(commit.trim(), original_commit.trim());
    }

    #[test]
    fn cherry_pick_to_staging_branch_succeeds_for_clean_commit() {
        let (_temp, repo) = init_repo();
        let session = GitRepoSession::open(&repo).expect("session");
        session
            .checkout_or_create_sidequest_branch("sidequest-nightly")
            .expect("nightly branch");
        fs::write(repo.join("README.md"), "# Repo\n\nNightly change.\n").expect("write file");
        let commit = session
            .commit_all("Nightly change")
            .expect("commit")
            .expect("commit sha");
        session.restore().expect("restore");

        session
            .cherry_pick_to_branch(commit.trim(), "sidequest-staged")
            .expect("cherry-pick");
        let log = run_git(
            &repo,
            [
                "log",
                "sidequest-staged",
                "--max-count",
                "1",
                "--pretty=format:%s",
            ],
        )
        .expect("log");
        assert_eq!(log.trim(), "Nightly change");
    }

    #[test]
    fn cherry_pick_conflict_aborts_cleanly() {
        let (_temp, repo) = init_repo();
        let session = GitRepoSession::open(&repo).expect("session");
        let base_branch =
            run_git(&repo, ["rev-parse", "--abbrev-ref", "HEAD"]).expect("base branch");

        fs::write(repo.join("README.md"), "# Repo\n\nMain change.\n").expect("write");
        run_git(&repo, ["add", "README.md"]).expect("add");
        run_git(&repo, ["commit", "-m", "Main branch change"]).expect("commit");

        session
            .checkout_or_create_sidequest_branch("sidequest-nightly")
            .expect("nightly");
        fs::write(
            repo.join("README.md"),
            "# Repo\n\nNightly conflicting change.\n",
        )
        .expect("write nightly");
        let nightly_commit = session
            .commit_all("Nightly conflict")
            .expect("commit")
            .expect("commit");
        session.restore().expect("restore");

        run_git(&repo, ["checkout", "-b", "sidequest-staged"]).expect("create staged");
        fs::write(
            repo.join("README.md"),
            "# Repo\n\nStaged conflicting change.\n",
        )
        .expect("write staged");
        run_git(&repo, ["add", "README.md"]).expect("stage staged");
        run_git(&repo, ["commit", "-m", "Staged conflict"]).expect("commit staged");
        run_git(&repo, ["checkout", base_branch.trim()]).expect("return base");

        let error = session
            .cherry_pick_to_branch(nightly_commit.trim(), "sidequest-staged")
            .expect_err("conflict expected");
        assert!(error.to_string().contains("failed to cherry-pick"));

        let status = run_git(&repo, ["status", "--porcelain"]).expect("status");
        assert!(status.trim().is_empty(), "cherry-pick should abort cleanly");
    }

    #[test]
    fn prepare_grind_branch_replays_pending_commits_onto_updated_main() {
        let (_temp, repo) = init_repo();
        let session = GitRepoSession::open(&repo).expect("session");
        let base = session.preferred_target_branch().expect("base branch");
        let branch = sidequest_branch_name(WorkMode::Grind, "repo");

        session
            .checkout_or_create_sidequest_branch(&branch)
            .expect("grind branch");
        fs::write(repo.join("nightly.txt"), "nightly change\n").expect("write nightly");
        let old_commit = session
            .commit_all("Nightly change")
            .expect("commit")
            .expect("commit sha");
        session.restore().expect("restore");

        fs::write(repo.join("main.txt"), "main change\n").expect("write main");
        run_git(&repo, ["add", "main.txt"]).expect("add main");
        run_git(&repo, ["commit", "-m", "Main branch change"]).expect("commit main");

        let created_at = chrono::Utc::now();
        let mut ledger = HarvestLedger {
            entries: vec![HarvestEntry {
                id: String::new(),
                repo_path: repo.clone(),
                repo_name: "repo".to_string(),
                branch: branch.clone(),
                commit: old_commit.trim().to_string(),
                mode: WorkMode::Grind,
                title: "Nightly change".to_string(),
                summary: "Nightly change".to_string(),
                quest: None,
                provider: ProviderKind::Codex,
                short_stat: None,
                files_changed: None,
                insertions: None,
                deletions: None,
                tests_added: None,
                tests_passing: None,
                created_at,
                night_of: Some(created_at.with_timezone(&chrono::Local).date_naive()),
                status: crate::state::HarvestEntryStatus::Pending,
                looted_at: None,
                clean_exit: true,
                note: None,
            }],
        };

        let state = prepare_grind_branch(&repo, &branch, &mut ledger).expect("prepare branch");
        assert!(matches!(
            state,
            GrindBranchState::Rebased {
                old_commit_count: 1
            }
        ));
        assert_ne!(ledger.entries[0].commit, old_commit.trim());

        let merge_base =
            run_git(&repo, ["merge-base", base.as_str(), branch.as_str()]).expect("merge-base");
        let base_head = run_git(&repo, ["rev-parse", base.as_str()]).expect("base head");
        assert_eq!(merge_base.trim(), base_head.trim());
    }

    #[test]
    fn prepare_grind_branch_marks_conflicting_pending_work_stale() {
        let (_temp, repo) = init_repo();
        let session = GitRepoSession::open(&repo).expect("session");
        let base = session.preferred_target_branch().expect("base branch");
        let branch = sidequest_branch_name(WorkMode::Grind, "repo");

        session
            .checkout_or_create_sidequest_branch(&branch)
            .expect("grind branch");
        fs::write(repo.join("README.md"), "# Repo\n\nNightly change.\n").expect("write nightly");
        let old_commit = session
            .commit_all("Nightly change")
            .expect("commit")
            .expect("commit sha");
        session.restore().expect("restore");

        fs::write(repo.join("README.md"), "# Repo\n\nMain branch change.\n").expect("write main");
        run_git(&repo, ["add", "README.md"]).expect("add main");
        run_git(&repo, ["commit", "-m", "Main branch change"]).expect("commit main");

        let created_at = chrono::Utc::now();
        let mut ledger = HarvestLedger {
            entries: vec![HarvestEntry {
                id: String::new(),
                repo_path: repo.clone(),
                repo_name: "repo".to_string(),
                branch: branch.clone(),
                commit: old_commit.trim().to_string(),
                mode: WorkMode::Grind,
                title: "Nightly change".to_string(),
                summary: "Nightly change".to_string(),
                quest: None,
                provider: ProviderKind::Codex,
                short_stat: None,
                files_changed: None,
                insertions: None,
                deletions: None,
                tests_added: None,
                tests_passing: None,
                created_at,
                night_of: Some(created_at.with_timezone(&chrono::Local).date_naive()),
                status: crate::state::HarvestEntryStatus::Pending,
                looted_at: None,
                clean_exit: true,
                note: None,
            }],
        };

        let state = prepare_grind_branch(&repo, &branch, &mut ledger).expect("prepare branch");
        assert!(matches!(
            state,
            GrindBranchState::FreshAfterStale { stale_count: 1 }
        ));
        assert_eq!(
            ledger.entries[0].status,
            crate::state::HarvestEntryStatus::Stale
        );

        let branch_head = run_git(&repo, ["rev-parse", branch.as_str()]).expect("branch head");
        let base_head = run_git(&repo, ["rev-parse", base.as_str()]).expect("base head");
        assert_eq!(branch_head.trim(), base_head.trim());
    }

    #[test]
    fn pending_banner_includes_repo_breakdown() {
        let entries = vec![
            HarvestEntry {
                id: String::new(),
                repo_path: PathBuf::from("/tmp/myapp"),
                repo_name: String::new(),
                branch: "sidequest/grind/myapp".to_string(),
                commit: "abc123".to_string(),
                mode: WorkMode::Grind,
                title: "Review task".to_string(),
                summary: "Summary".to_string(),
                quest: None,
                provider: ProviderKind::Codex,
                short_stat: None,
                files_changed: None,
                insertions: None,
                deletions: None,
                tests_added: None,
                tests_passing: None,
                created_at: chrono::Utc::now(),
                night_of: None,
                status: crate::state::HarvestEntryStatus::Pending,
                looted_at: None,
                clean_exit: true,
                note: None,
            },
            HarvestEntry {
                id: String::new(),
                repo_path: PathBuf::from("/tmp/myapp"),
                repo_name: String::new(),
                branch: "sidequest/grind/myapp".to_string(),
                commit: "def456".to_string(),
                mode: WorkMode::Grind,
                title: "Build task".to_string(),
                summary: "Summary".to_string(),
                quest: None,
                provider: ProviderKind::Claude,
                short_stat: None,
                files_changed: None,
                insertions: None,
                deletions: None,
                tests_added: None,
                tests_passing: None,
                created_at: chrono::Utc::now(),
                night_of: None,
                status: crate::state::HarvestEntryStatus::Pending,
                looted_at: None,
                clean_exit: true,
                note: None,
            },
        ];
        let banner = format_pending_harvest_banner(&entries);
        assert!(banner.contains("2 tasks"));
        assert!(banner.contains("myapp/"));
        assert!(banner.contains("grind"));
        assert!(banner.contains("sidequest"));
    }
}
