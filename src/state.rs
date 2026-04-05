use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Local, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::{SideQuestPaths, WorkMode};
use crate::oracle::ProviderKind;

const BACKLOG_RETENTION_DAYS: i64 = 7;
const SESSION_REPORT_DIR: &str = ".sidequest";
const SESSION_REPORT_FILE: &str = "session-report.json";
const BUILD_GOAL_FILE: &str = "sidequest-goal.md";

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SessionReport {
    #[serde(default)]
    pub completed: Vec<CompletedWorkItem>,
    #[serde(default)]
    pub attempted_but_failed: Vec<FailedWorkItem>,
    #[serde(default)]
    pub remaining_work: Vec<RemainingWorkItem>,
    #[serde(default)]
    pub quest_completed: Option<bool>,
    #[serde(default, deserialize_with = "deserialize_optional_stringish")]
    pub budget_estimate_at_exit: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletedWorkItem {
    pub mode: WorkMode,
    #[serde(default)]
    pub repository: Option<String>,
    #[serde(default)]
    pub quest: Option<String>,
    #[serde(default)]
    pub branch: Option<String>,
    pub summary: String,
    #[serde(default, deserialize_with = "deserialize_optional_count")]
    pub files_changed: Option<usize>,
    #[serde(default, deserialize_with = "deserialize_optional_count")]
    pub tests_added: Option<usize>,
    #[serde(default, deserialize_with = "deserialize_optional_boolish")]
    pub tests_passing: Option<bool>,
    #[serde(default)]
    pub diff_summary: Option<String>,
    #[serde(default)]
    pub next_step: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FailedWorkItem {
    pub mode: WorkMode,
    #[serde(default)]
    pub repository: Option<String>,
    #[serde(default)]
    pub quest: Option<String>,
    pub summary: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RemainingWorkItem {
    pub mode: WorkMode,
    #[serde(default)]
    pub repository: Option<String>,
    #[serde(default)]
    pub quest: Option<String>,
    pub summary: String,
    #[serde(default)]
    pub next_step: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BacklogItem {
    pub mode: WorkMode,
    #[serde(default)]
    pub repository: Option<String>,
    #[serde(default)]
    pub quest: Option<String>,
    pub summary: String,
    #[serde(default)]
    pub next_step: Option<String>,
    pub updated_at: DateTime<Utc>,
}

impl BacklogItem {
    fn key(&self) -> (&WorkMode, Option<&str>, Option<&str>, &str) {
        (
            &self.mode,
            self.repository.as_deref(),
            self.quest.as_deref(),
            self.summary.as_str(),
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HarvestLedger {
    #[serde(default)]
    pub entries: Vec<HarvestEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarvestEntry {
    #[serde(default)]
    pub id: String,
    pub repo_path: PathBuf,
    #[serde(default)]
    pub repo_name: String,
    pub branch: String,
    #[serde(default, alias = "commit_hash")]
    pub commit: String,
    pub mode: WorkMode,
    pub title: String,
    pub summary: String,
    #[serde(default)]
    pub quest: Option<String>,
    pub provider: ProviderKind,
    #[serde(default)]
    pub short_stat: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_count")]
    pub files_changed: Option<usize>,
    #[serde(default, deserialize_with = "deserialize_optional_count")]
    pub insertions: Option<usize>,
    #[serde(default, deserialize_with = "deserialize_optional_count")]
    pub deletions: Option<usize>,
    #[serde(default)]
    pub tests_added: Option<usize>,
    #[serde(default)]
    pub tests_passing: Option<bool>,
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub night_of: Option<NaiveDate>,
    pub status: HarvestEntryStatus,
    #[serde(default)]
    pub looted_at: Option<DateTime<Utc>>,
    #[serde(default = "default_true")]
    pub clean_exit: bool,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HarvestEntryStatus {
    Pending,
    #[serde(alias = "staged")]
    Accepted,
    Dismissed,
    Acknowledged,
    Stale,
}

impl HarvestEntry {
    pub fn has_commit(&self) -> bool {
        !self.commit.trim().is_empty()
    }

    pub fn is_failed(&self) -> bool {
        !self.clean_exit || !self.has_commit()
    }
}

fn normalize_harvest_entry(entry: &mut HarvestEntry) {
    if entry.repo_name.trim().is_empty() {
        entry.repo_name = entry
            .repo_path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| entry.repo_path.display().to_string());
    }

    if entry.id.trim().is_empty() {
        entry.id = format!(
            "{}:{}:{}",
            entry.repo_path.display(),
            entry.branch,
            entry.created_at.timestamp_millis()
        );
    }

    if entry.night_of.is_none() {
        entry.night_of = Some(entry.created_at.with_timezone(&Local).date_naive());
    }
}

pub fn read_harvest_ledger(paths: &SideQuestPaths) -> Result<HarvestLedger> {
    paths.ensure()?;
    if !paths.harvest_ledger_file.exists() {
        let ledger = HarvestLedger::default();
        write_harvest_ledger(paths, &ledger)?;
        return Ok(ledger);
    }

    let raw = fs::read_to_string(&paths.harvest_ledger_file)
        .with_context(|| format!("failed to read {}", paths.harvest_ledger_file.display()))?;
    if raw.trim().is_empty() {
        return Ok(HarvestLedger::default());
    }

    let mut ledger: HarvestLedger = serde_json::from_str(&raw)
        .with_context(|| format!("failed to parse {}", paths.harvest_ledger_file.display()))?;
    for entry in &mut ledger.entries {
        normalize_harvest_entry(entry);
    }
    Ok(ledger)
}

pub fn write_harvest_ledger(paths: &SideQuestPaths, ledger: &HarvestLedger) -> Result<()> {
    paths.ensure()?;
    let raw = serde_json::to_string_pretty(ledger).context("failed to serialize harvest ledger")?;
    fs::write(&paths.harvest_ledger_file, raw)
        .with_context(|| format!("failed to write {}", paths.harvest_ledger_file.display()))?;
    Ok(())
}

pub fn append_harvest_entry(paths: &SideQuestPaths, entry: HarvestEntry) -> Result<()> {
    let mut ledger = read_harvest_ledger(paths)?;
    let mut entry = entry;
    normalize_harvest_entry(&mut entry);
    ledger.entries.push(entry);
    write_harvest_ledger(paths, &ledger)
}

pub fn read_session_report(path: &Path) -> Result<Option<SessionReport>> {
    if !path.exists() {
        return Ok(None);
    }
    let raw =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let report = serde_json::from_str(&raw)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    Ok(Some(report))
}

pub fn write_last_session_report(paths: &SideQuestPaths, report: &SessionReport) -> Result<()> {
    paths.ensure()?;
    let raw = serde_json::to_string_pretty(report).context("failed to serialize session report")?;
    fs::write(&paths.last_session_report_file, raw).with_context(|| {
        format!(
            "failed to write {}",
            paths.last_session_report_file.display()
        )
    })?;
    Ok(())
}

pub fn read_backlog(paths: &SideQuestPaths) -> Result<Vec<BacklogItem>> {
    if !paths.backlog_file.exists() {
        return Ok(Vec::new());
    }
    let raw = fs::read_to_string(&paths.backlog_file)
        .with_context(|| format!("failed to read {}", paths.backlog_file.display()))?;
    let items: Vec<BacklogItem> = serde_json::from_str(&raw)
        .with_context(|| format!("failed to parse {}", paths.backlog_file.display()))?;
    Ok(items)
}

pub fn write_backlog(paths: &SideQuestPaths, items: &[BacklogItem]) -> Result<()> {
    paths.ensure()?;
    let raw = serde_json::to_string_pretty(items).context("failed to serialize backlog")?;
    fs::write(&paths.backlog_file, raw)
        .with_context(|| format!("failed to write {}", paths.backlog_file.display()))?;
    Ok(())
}

pub fn update_pending_entry_commits(
    ledger: &mut HarvestLedger,
    repo_path: &Path,
    branch: &str,
    commits: &[String],
) -> usize {
    let mut pending_indices: Vec<usize> = ledger
        .entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| {
            entry.status == HarvestEntryStatus::Pending
                && entry.repo_path == repo_path
                && entry.branch == branch
        })
        .map(|(index, _)| index)
        .collect();
    pending_indices.sort_by_key(|index| ledger.entries[*index].created_at);

    let mut updated = 0usize;
    for (index, commit) in pending_indices.iter().zip(commits.iter()) {
        ledger.entries[*index].commit = commit.clone();
        updated += 1;
    }

    for index in pending_indices.into_iter().skip(commits.len()) {
        ledger.entries[index].status = HarvestEntryStatus::Stale;
        ledger.entries[index].looted_at = Some(Utc::now());
        ledger.entries[index].note =
            Some("Pending work became unreachable after branch refresh.".to_string());
    }

    updated
}

pub fn mark_pending_entries_stale(
    ledger: &mut HarvestLedger,
    repo_path: &Path,
    branch: &str,
    note: impl Into<String>,
) -> usize {
    let note = note.into();
    let mut count = 0usize;
    for entry in &mut ledger.entries {
        if entry.status == HarvestEntryStatus::Pending
            && entry.repo_path == repo_path
            && entry.branch == branch
        {
            entry.status = HarvestEntryStatus::Stale;
            entry.looted_at = Some(Utc::now());
            entry.note = Some(note.clone());
            count += 1;
        }
    }
    count
}

pub fn prune_and_merge_backlog(
    existing: &[BacklogItem],
    completed: &[CompletedWorkItem],
    remaining: &[RemainingWorkItem],
    now: DateTime<Utc>,
) -> Vec<BacklogItem> {
    let cutoff = now - Duration::days(BACKLOG_RETENTION_DAYS);
    let mut backlog: Vec<BacklogItem> = existing
        .iter()
        .filter(|item| item.mode == WorkMode::Grind && item.updated_at >= cutoff)
        .cloned()
        .collect();

    backlog.retain(|item| {
        !completed.iter().any(|done| {
            done.mode == WorkMode::Grind
                && done.repository.as_deref() == item.repository.as_deref()
                && done.quest.as_deref() == item.quest.as_deref()
                && done.summary == item.summary
        })
    });

    for item in remaining.iter().filter(|item| item.mode == WorkMode::Grind) {
        let new_item = BacklogItem {
            mode: item.mode,
            repository: item.repository.clone(),
            quest: item.quest.clone(),
            summary: item.summary.clone(),
            next_step: item.next_step.clone(),
            updated_at: now,
        };
        if let Some(existing_item) = backlog
            .iter_mut()
            .find(|existing_item| existing_item.key() == new_item.key())
        {
            existing_item.next_step = new_item.next_step.clone();
            existing_item.updated_at = now;
        } else {
            backlog.push(new_item);
        }
    }

    backlog.sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
    backlog
}

pub fn quest_log_path(paths: &SideQuestPaths, name: &str) -> PathBuf {
    paths.quest_logs_dir.join(format!("{name}.md"))
}

pub fn read_quest_log(paths: &SideQuestPaths, name: &str) -> Result<Option<String>> {
    let preferred = quest_log_path(paths, name);
    if preferred.exists() {
        let raw = fs::read_to_string(&preferred)
            .with_context(|| format!("failed to read {}", preferred.display()))?;
        return Ok(Some(normalize_quest_log_context(&raw)));
    }

    let legacy = paths.quests_dir.join(format!("{name}.md"));
    if legacy.exists() {
        let raw = fs::read_to_string(&legacy)
            .with_context(|| format!("failed to read {}", legacy.display()))?;
        return Ok(Some(normalize_quest_log_context(&raw)));
    }

    Ok(None)
}

pub fn append_quest_log(paths: &SideQuestPaths, name: &str, markdown: &str) -> Result<()> {
    paths.ensure()?;
    let path = quest_log_path(paths, name);
    let normalized = normalize_quest_log_context(markdown);
    fs::write(&path, normalized).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

pub fn report_file_path(repo_root: &Path) -> PathBuf {
    repo_root.join(SESSION_REPORT_DIR).join(SESSION_REPORT_FILE)
}

pub fn build_goal_file_path(repo_root: &Path) -> PathBuf {
    repo_root.join(SESSION_REPORT_DIR).join(BUILD_GOAL_FILE)
}

pub fn read_build_goal(repo_root: &Path) -> Result<Option<String>> {
    let goal_path = build_goal_file_path(repo_root);
    if !goal_path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(&goal_path)
        .with_context(|| format!("failed to read {}", goal_path.display()))?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    Ok(Some(trimmed.to_string()))
}

pub fn write_build_goal(repo_root: &Path, goal: &str) -> Result<PathBuf> {
    let goal_path = build_goal_file_path(repo_root);
    ensure_report_dir(repo_root)?;
    let body = goal.trim();
    fs::write(&goal_path, format!("{body}\n"))
        .with_context(|| format!("failed to write {}", goal_path.display()))?;
    Ok(goal_path)
}

pub fn ensure_report_dir(repo_root: &Path) -> Result<PathBuf> {
    let dir = repo_root.join(SESSION_REPORT_DIR);
    fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
    Ok(dir)
}

pub fn clear_session_report(repo_root: &Path) -> Result<()> {
    let path = report_file_path(repo_root);
    if !path.exists() {
        return Ok(());
    }

    fs::remove_file(&path).with_context(|| format!("failed to remove {}", path.display()))?;
    remove_report_dir_if_empty(repo_root)?;
    Ok(())
}

pub fn take_session_report(repo_root: &Path) -> Result<Option<SessionReport>> {
    let path = report_file_path(repo_root);
    if !path.exists() {
        return Ok(None);
    }

    let raw =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    let report = serde_json::from_str(&raw)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    fs::remove_file(&path).with_context(|| format!("failed to remove {}", path.display()))?;
    remove_report_dir_if_empty(repo_root)?;
    Ok(Some(report))
}

fn deserialize_optional_stringish<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    Ok(value.and_then(stringish_value))
}

fn deserialize_optional_count<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<usize>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    Ok(value.and_then(countish_value))
}

fn deserialize_optional_boolish<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<bool>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    Ok(value.and_then(boolish_value))
}

fn stringish_value(value: Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(text) => {
            let trimmed = text.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        }
        Value::Number(number) => Some(number.to_string()),
        Value::Bool(flag) => Some(flag.to_string()),
        other => Some(other.to_string()),
    }
}

fn countish_value(value: Value) -> Option<usize> {
    match value {
        Value::Null => None,
        Value::Number(number) => number
            .as_u64()
            .and_then(|count| usize::try_from(count).ok())
            .or_else(|| {
                number
                    .as_f64()
                    .filter(|count| count.is_finite() && *count >= 0.0)
                    .map(|count| count.round() as usize)
            }),
        Value::String(text) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                return None;
            }
            trimmed.parse::<usize>().ok().or(Some(1))
        }
        Value::Bool(flag) => Some(usize::from(flag)),
        Value::Array(items) => Some(items.len()),
        Value::Object(map) => Some(map.len()),
    }
}

fn boolish_value(value: Value) -> Option<bool> {
    match value {
        Value::Null => None,
        Value::Bool(flag) => Some(flag),
        Value::Number(number) => number
            .as_i64()
            .map(|count| count != 0)
            .or_else(|| number.as_u64().map(|count| count != 0))
            .or_else(|| number.as_f64().map(|count| count != 0.0)),
        Value::String(text) => match text.trim().to_ascii_lowercase().as_str() {
            "" => None,
            "true" | "yes" | "y" | "pass" | "passed" | "ok" => Some(true),
            "false" | "no" | "n" | "fail" | "failed" => Some(false),
            _ => None,
        },
        Value::Array(items) => Some(!items.is_empty()),
        Value::Object(map) => Some(!map.is_empty()),
    }
}

fn remove_report_dir_if_empty(repo_root: &Path) -> Result<()> {
    let dir = repo_root.join(SESSION_REPORT_DIR);
    if !dir.exists() {
        return Ok(());
    }

    if fs::read_dir(&dir)
        .with_context(|| format!("failed to inspect {}", dir.display()))?
        .next()
        .is_none()
    {
        fs::remove_dir(&dir).with_context(|| format!("failed to remove {}", dir.display()))?;
    }
    Ok(())
}

pub fn format_quest_log_entry(
    completed: &[CompletedWorkItem],
    remaining: &[RemainingWorkItem],
) -> String {
    let mut markdown = String::new();
    markdown.push_str(&format!(
        "## Session — {}\n\n",
        Local::now().format("%Y-%m-%d %H:%M")
    ));

    if completed.is_empty() {
        markdown.push_str("### Completed\n- No quest progress recorded.\n\n");
    } else {
        markdown.push_str("### Completed\n");
        for item in completed {
            markdown.push_str(&format!("- {}\n", item.summary));
        }
        markdown.push('\n');
    }

    if remaining.is_empty() {
        markdown.push_str("### Next steps\n- None recorded.\n\n");
    } else {
        markdown.push_str("### Next steps\n");
        for item in remaining {
            let line = item.next_step.as_deref().unwrap_or(item.summary.as_str());
            markdown.push_str(&format!("- {}\n", line));
        }
        markdown.push('\n');
    }

    markdown
}

pub fn format_quest_log_document(
    completed: &[CompletedWorkItem],
    remaining: &[RemainingWorkItem],
) -> String {
    let mut markdown = String::new();
    markdown.push_str("## Compact summary\n");
    markdown.push_str(&quest_log_summary(completed, remaining));
    markdown.push_str("\n## Latest log\n\n");
    markdown.push_str(&format_quest_log_entry(completed, remaining));
    markdown
}

fn quest_log_summary(completed: &[CompletedWorkItem], remaining: &[RemainingWorkItem]) -> String {
    let completed_items: Vec<String> = completed
        .iter()
        .take(3)
        .map(|item| item.summary.trim().to_string())
        .filter(|item| !item.is_empty())
        .collect();
    let remaining_items: Vec<String> = remaining
        .iter()
        .take(3)
        .map(|item| {
            item.next_step
                .as_deref()
                .unwrap_or(item.summary.as_str())
                .trim()
                .to_string()
        })
        .filter(|item| !item.is_empty())
        .collect();
    format_summary(&completed_items, &remaining_items)
}

fn normalize_quest_log_context(markdown: &str) -> String {
    let trimmed = markdown.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    if trimmed.contains("## Compact summary") && trimmed.contains("## Latest log") {
        return trimmed.to_string();
    }

    let Some(latest_session) = latest_session_block(trimmed) else {
        return trimmed.to_string();
    };
    let completed = extract_section_bullets(latest_session, "Completed");
    let remaining = extract_section_bullets(latest_session, "Next steps");
    let mut normalized = String::new();
    normalized.push_str("## Compact summary\n");
    normalized.push_str(&format_summary(&completed, &remaining));
    normalized.push_str("\n## Latest log\n\n");
    normalized.push_str(latest_session.trim());
    normalized.push('\n');
    normalized
}

fn latest_session_block(markdown: &str) -> Option<&str> {
    markdown
        .rfind("## Session — ")
        .map(|start| &markdown[start..])
}

fn extract_section_bullets(markdown: &str, section: &str) -> Vec<String> {
    let mut bullets = Vec::new();
    let mut in_section = false;
    let heading = format!("### {section}");
    for line in markdown.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("### ") {
            in_section = trimmed == heading;
            continue;
        }
        if in_section && trimmed.starts_with("- ") {
            let item = trimmed.trim_start_matches("- ").trim();
            if !item.is_empty() {
                bullets.push(item.to_string());
            }
        }
    }
    bullets
}

fn format_summary(completed: &[String], remaining: &[String]) -> String {
    let mut summary = String::new();
    if completed.is_empty() {
        summary.push_str("- Completed: none recorded\n");
    } else {
        summary.push_str(&format!("- Completed: {}\n", completed.join("; ")));
    }
    if remaining.is_empty() {
        summary.push_str("- Next steps: none recorded\n");
    } else {
        summary.push_str(&format!("- Next steps: {}\n", remaining.join("; ")));
    }
    summary
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    #[test]
    fn parses_session_report() {
        let raw = r#"{
          "completed": [{"mode":"grind","summary":"Added docs"}],
          "attempted_but_failed": [],
          "remaining_work": [{"mode":"grind","summary":"Add examples"}],
          "budget_estimate_at_exit":"~25% remaining"
        }"#;
        let report: SessionReport = serde_json::from_str(raw).expect("report");
        assert_eq!(report.completed.len(), 1);
        assert_eq!(report.remaining_work.len(), 1);
        assert_eq!(report.completed[0].mode, WorkMode::Grind);
        assert_eq!(report.quest_completed, None);
    }

    #[test]
    fn harvest_ledger_round_trips() {
        let temp = tempfile::tempdir().expect("temp dir");
        let paths = SideQuestPaths::from_root(temp.path());

        let initial = read_harvest_ledger(&paths).expect("initial ledger");
        assert!(initial.entries.is_empty());
        assert!(paths.harvest_ledger_file.exists());

        append_harvest_entry(
            &paths,
            HarvestEntry {
                id: String::new(),
                repo_path: PathBuf::from("/tmp/repo"),
                repo_name: String::new(),
                branch: "sidequest/grind/repo".to_string(),
                commit: "abc123".to_string(),
                mode: WorkMode::Grind,
                title: "Improve build".to_string(),
                summary: "Added build checks".to_string(),
                quest: None,
                provider: ProviderKind::Codex,
                short_stat: Some("1 file changed".to_string()),
                files_changed: None,
                insertions: None,
                deletions: None,
                tests_added: Some(2),
                tests_passing: Some(true),
                created_at: Utc::now(),
                night_of: None,
                status: HarvestEntryStatus::Pending,
                looted_at: None,
                clean_exit: true,
                note: None,
            },
        )
        .expect("append entry");

        let ledger = read_harvest_ledger(&paths).expect("ledger");
        assert_eq!(ledger.entries.len(), 1);
        assert_eq!(ledger.entries[0].status, HarvestEntryStatus::Pending);
        assert_eq!(ledger.entries[0].provider, ProviderKind::Codex);
        assert!(ledger.entries[0].has_commit());
        assert!(!ledger.entries[0].is_failed());
    }

    #[test]
    fn parses_agent_friendly_report_shapes() {
        let raw = r#"{
          "completed": [{
            "mode":"grind",
            "summary":"Added docs",
            "files_changed":["README.md","index.html"],
            "tests_added":["node --check","git diff --check"],
            "tests_passing":"yes"
          }],
          "attempted_but_failed": [],
          "remaining_work": [],
          "budget_estimate_at_exit": 0.25
        }"#;

        let report: SessionReport = serde_json::from_str(raw).expect("report");
        assert_eq!(report.completed.len(), 1);
        assert_eq!(report.completed[0].files_changed, Some(2));
        assert_eq!(report.completed[0].tests_added, Some(2));
        assert_eq!(report.completed[0].tests_passing, Some(true));
        assert_eq!(report.budget_estimate_at_exit.as_deref(), Some("0.25"));
    }

    #[test]
    fn dedupes_and_prunes_backlog() {
        let now = Utc
            .with_ymd_and_hms(2026, 4, 3, 10, 0, 0)
            .single()
            .expect("now");
        let old = now - Duration::days(8);
        let existing = vec![
            BacklogItem {
                mode: WorkMode::Grind,
                repository: Some("/tmp/repo".to_string()),
                quest: None,
                summary: "Add docs".to_string(),
                next_step: Some("Write docs".to_string()),
                updated_at: old,
            },
            BacklogItem {
                mode: WorkMode::Grind,
                repository: Some("/tmp/repo".to_string()),
                quest: None,
                summary: "Add examples".to_string(),
                next_step: None,
                updated_at: now,
            },
        ];
        let remaining = vec![RemainingWorkItem {
            mode: WorkMode::Grind,
            repository: Some("/tmp/repo".to_string()),
            quest: None,
            summary: "Add examples".to_string(),
            next_step: Some("Write CLI example".to_string()),
        }];

        let merged = prune_and_merge_backlog(&existing, &[], &remaining, now);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].summary, "Add examples");
        assert_eq!(merged[0].next_step.as_deref(), Some("Write CLI example"));
    }

    #[test]
    fn reads_legacy_quest_log_if_new_path_is_missing() {
        let temp = tempfile::tempdir().expect("temp dir");
        let paths = SideQuestPaths::from_root(temp.path());
        fs::create_dir_all(&paths.quests_dir).expect("quest dir");
        fs::write(paths.quests_dir.join("legacy.md"), "legacy").expect("legacy");

        let contents = read_quest_log(&paths, "legacy").expect("read quest log");
        assert_eq!(contents.as_deref(), Some("legacy"));
    }

    #[test]
    fn normalizes_legacy_quest_log_to_bounded_context() {
        let temp = tempfile::tempdir().expect("temp dir");
        let paths = SideQuestPaths::from_root(temp.path());
        fs::create_dir_all(&paths.quest_logs_dir).expect("quest dir");
        let quest_log = paths.quest_logs_dir.join("legacy.md");
        fs::write(
            &quest_log,
            "## Session — 2026-04-01 01:00\n\n### Completed\n- Old item\n\n### Next steps\n- Old follow-up\n\n## Session — 2026-04-02 01:00\n\n### Completed\n- New item\n\n### Next steps\n- New follow-up\n",
        )
        .expect("quest log");

        let contents = read_quest_log(&paths, "legacy").expect("read quest log");
        let contents = contents.as_deref().expect("quest log contents");
        assert!(contents.contains("## Compact summary"));
        assert!(contents.contains("## Latest log"));
        assert!(contents.contains("New item"));
        assert!(!contents.contains("Old item"));
    }

    #[test]
    fn takes_session_report_and_removes_repo_artifact() {
        let temp = tempfile::tempdir().expect("temp dir");
        let report_dir = temp.path().join(".sidequest");
        fs::create_dir_all(&report_dir).expect("report dir");
        let report_path = report_dir.join("session-report.json");
        fs::write(
            &report_path,
            r#"{"completed":[],"attempted_but_failed":[],"remaining_work":[]}"#,
        )
        .expect("write report");

        let report = take_session_report(temp.path()).expect("take report");
        assert!(report.is_some());
        assert!(!report_path.exists());
        assert!(!report_dir.exists());
    }

    #[test]
    fn build_goal_round_trips_and_stays_separate_from_session_report() {
        let temp = tempfile::tempdir().expect("temp dir");
        let repo = temp.path();

        let goal_path = write_build_goal(repo, "Ship CLI docs and tests")
            .expect("write build goal should succeed");
        assert!(goal_path.ends_with(".sidequest/sidequest-goal.md"));

        let goal = read_build_goal(repo).expect("read goal");
        assert_eq!(goal.as_deref(), Some("Ship CLI docs and tests"));

        clear_session_report(repo).expect("clearing report should not fail");
        assert!(goal_path.exists());
    }

    #[test]
    fn preserves_invalid_session_report_for_debugging() {
        let temp = tempfile::tempdir().expect("temp dir");
        let report_dir = temp.path().join(".sidequest");
        fs::create_dir_all(&report_dir).expect("report dir");
        let report_path = report_dir.join("session-report.json");
        fs::write(
            &report_path,
            r#"{"completed":[{"mode":"mystery","summary":"oops"}],"attempted_but_failed":[],"remaining_work":[]}"#,
        )
        .expect("write report");

        let error = take_session_report(temp.path()).expect_err("invalid report should fail");
        assert!(error.to_string().contains("failed to parse"));
        assert!(report_path.exists());
        assert!(report_dir.exists());
    }

    #[test]
    fn format_quest_log_entry_includes_completed_and_remaining() {
        let completed = vec![CompletedWorkItem {
            mode: WorkMode::Quest,
            repository: None,
            quest: None,
            branch: None,
            summary: "Shipped feature X".to_string(),
            files_changed: None,
            tests_added: None,
            tests_passing: None,
            diff_summary: None,
            next_step: None,
        }];
        let remaining = vec![RemainingWorkItem {
            mode: WorkMode::Quest,
            repository: None,
            quest: None,
            summary: "Add tests for feature X".to_string(),
            next_step: Some("Write integration tests".to_string()),
        }];

        let entry = format_quest_log_entry(&completed, &remaining);

        assert!(entry.contains("## Session"));
        assert!(entry.contains("### Completed"));
        assert!(entry.contains("Shipped feature X"));
        assert!(entry.contains("### Next steps"));
        assert!(entry.contains("Write integration tests"));
    }

    #[test]
    fn format_quest_log_document_includes_compact_summary() {
        let completed = vec![CompletedWorkItem {
            mode: WorkMode::Quest,
            repository: None,
            quest: None,
            branch: None,
            summary: "Shipped feature X".to_string(),
            files_changed: None,
            tests_added: None,
            tests_passing: None,
            diff_summary: None,
            next_step: None,
        }];

        let document = format_quest_log_document(&completed, &[]);

        assert!(document.contains("## Compact summary"));
        assert!(document.contains("## Latest log"));
        assert!(document.contains("Shipped feature X"));
    }

    #[test]
    fn format_quest_log_entry_handles_empty_lists() {
        let entry = format_quest_log_entry(&[], &[]);

        assert!(entry.contains("No quest progress recorded"));
        assert!(entry.contains("None recorded"));
    }

    #[test]
    fn boolish_value_parses_agent_friendly_strings() {
        assert_eq!(boolish_value(Value::String("yes".into())), Some(true));
        assert_eq!(boolish_value(Value::String("passed".into())), Some(true));
        assert_eq!(boolish_value(Value::String("fail".into())), Some(false));
        assert_eq!(boolish_value(Value::String("no".into())), Some(false));
        assert_eq!(boolish_value(Value::String("".into())), None);
        assert_eq!(boolish_value(Value::String("maybe".into())), None);
        assert_eq!(boolish_value(Value::Null), None);
        assert_eq!(boolish_value(Value::Bool(true)), Some(true));
    }

    #[test]
    fn countish_value_handles_diverse_inputs() {
        assert_eq!(
            countish_value(Value::Number(serde_json::Number::from(5))),
            Some(5)
        );
        assert_eq!(countish_value(Value::String("3".into())), Some(3));
        assert_eq!(
            countish_value(Value::String("not-a-number".into())),
            Some(1)
        );
        assert_eq!(countish_value(Value::String("".into())), None);
        assert_eq!(countish_value(Value::Null), None);
        assert_eq!(
            countish_value(Value::Array(vec![Value::Null, Value::Null])),
            Some(2)
        );
    }
}
