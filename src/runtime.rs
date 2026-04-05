use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::error::Category as JsonErrorCategory;

use crate::config::{SideQuestPaths, WorkMode};
use crate::harvester::{HarvestRecord, HarvestTask, HarvestTaskStatus};
use crate::oracle::{OracleSnapshot, ProviderKind};
use crate::scheduler::{DecisionKind, SchedulerDecision};
use crate::state::{CompletedWorkItem, FailedWorkItem, RemainingWorkItem, SessionReport};

pub const STALE_HEARTBEAT_MINUTES: i64 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendHealth {
    Healthy,
    Stale,
    Offline,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeStatus {
    Starting,
    Idle,
    Running,
    Backoff,
    Stopped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeRunStatus {
    Running,
    Completed,
    Failed,
    Stopped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeEventKind {
    DaemonStarted,
    Heartbeat,
    SchedulerEvaluated,
    Waiting,
    TaskSelected,
    BranchCreated,
    AgentSpawned,
    StopRequested,
    TaskCompleted,
    TaskStopped,
    TaskFailed,
    HarvestWritten,
    BackoffStarted,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlRequestKind {
    RunNow,
    StopActiveRun,
    HarvestCompleted,
    StopDaemon,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlRequest {
    pub id: i64,
    pub created_at: DateTime<Utc>,
    pub kind: ControlRequestKind,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RuntimeTaskState {
    #[serde(default)]
    pub completed: Vec<CompletedWorkItem>,
    #[serde(default)]
    pub failed: Vec<FailedWorkItem>,
    #[serde(default)]
    pub remaining: Vec<RemainingWorkItem>,
}

impl RuntimeTaskState {
    pub fn from_report(report: &SessionReport) -> Self {
        Self {
            completed: report.completed.clone(),
            failed: report.attempted_but_failed.clone(),
            remaining: report.remaining_work.clone(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.completed.is_empty() && self.failed.is_empty() && self.remaining.is_empty()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeRunState {
    pub provider: ProviderKind,
    pub mode: WorkMode,
    pub repo_path: String,
    pub title: String,
    #[serde(default)]
    pub quest: Option<String>,
    #[serde(default)]
    pub branch: Option<String>,
    pub started_at: DateTime<Utc>,
    pub cutoff_time: DateTime<Utc>,
    #[serde(default)]
    pub completed_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub log_path: Option<String>,
    pub status: RuntimeRunStatus,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub commit: Option<String>,
    #[serde(default)]
    pub short_stat: Option<String>,
    #[serde(default)]
    pub report_found: bool,
    #[serde(default)]
    pub clean_exit: bool,
    #[serde(default)]
    pub stop_reason: Option<String>,
    #[serde(default)]
    pub task_state: RuntimeTaskState,
}

impl RuntimeRunState {
    pub fn log_path_buf(&self) -> Option<PathBuf> {
        self.log_path.as_ref().map(PathBuf::from)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeHarvestSummary {
    pub finished_at: DateTime<Utc>,
    #[serde(default)]
    pub providers: Vec<ProviderKind>,
    #[serde(default)]
    pub tasks: Vec<RuntimeHarvestTask>,
    #[serde(default)]
    pub markdown_path: Option<String>,
    #[serde(default)]
    pub banner: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeHarvestTask {
    pub status: RuntimeHarvestTaskStatus,
    pub mode: WorkMode,
    pub title: String,
    pub repository: String,
    pub provider: ProviderKind,
    pub branch: String,
    pub summary: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeHarvestTaskStatus {
    Completed,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeDecision {
    pub kind: DecisionKindSerde,
    pub reason: String,
    #[serde(default)]
    pub provider: Option<RuntimeDecisionProvider>,
    pub wake_time: DateTime<Utc>,
    pub cutoff_time: DateTime<Utc>,
    pub next_check_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionKindSerde {
    RunNow,
    OutsideSleepWindow,
    AfterCutoff,
    LowBudget,
    AwaitingReset,
    NoProviders,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeDecisionProvider {
    pub provider: ProviderKind,
    pub spendable_budget: f64,
}

impl From<&SchedulerDecision> for RuntimeDecision {
    fn from(decision: &SchedulerDecision) -> Self {
        Self {
            kind: match decision.kind {
                DecisionKind::RunNow => DecisionKindSerde::RunNow,
                DecisionKind::OutsideSleepWindow => DecisionKindSerde::OutsideSleepWindow,
                DecisionKind::AfterCutoff => DecisionKindSerde::AfterCutoff,
                DecisionKind::LowBudget => DecisionKindSerde::LowBudget,
                DecisionKind::AwaitingReset => DecisionKindSerde::AwaitingReset,
                DecisionKind::NoProviders => DecisionKindSerde::NoProviders,
            },
            reason: decision.reason.clone(),
            provider: decision
                .provider
                .as_ref()
                .map(|provider| RuntimeDecisionProvider {
                    provider: provider.provider,
                    spendable_budget: provider.spendable_budget,
                }),
            wake_time: decision.wake_time.to_utc(),
            cutoff_time: decision.cutoff_time.to_utc(),
            next_check_at: decision.next_check_at.to_utc(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeEvent {
    pub id: i64,
    pub at: DateTime<Utc>,
    pub kind: RuntimeEventKind,
    pub message: String,
    #[serde(default)]
    pub run_title: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeSnapshot {
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub daemon_started_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub daemon_heartbeat_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub backend_pid: Option<u32>,
    pub backend_health: BackendHealth,
    pub status: RuntimeStatus,
    #[serde(default)]
    pub last_processed_control_id: Option<i64>,
    #[serde(default)]
    pub oracle_snapshot: Option<OracleSnapshot>,
    #[serde(default)]
    pub scheduler_decision: Option<RuntimeDecision>,
    #[serde(default)]
    pub active_run: Option<RuntimeRunState>,
    #[serde(default)]
    pub last_run: Option<RuntimeRunState>,
    #[serde(default)]
    pub last_session_report: Option<SessionReport>,
    #[serde(default)]
    pub latest_harvest: Option<RuntimeHarvestSummary>,
    #[serde(default)]
    pub pending_harvest_count: usize,
    #[serde(default)]
    pub harvest_completed_for_session: bool,
    #[serde(default)]
    pub latest_event_message: Option<String>,
}

impl Default for RuntimeSnapshot {
    fn default() -> Self {
        Self {
            updated_at: Utc::now(),
            daemon_started_at: None,
            daemon_heartbeat_at: None,
            backend_pid: None,
            backend_health: BackendHealth::Offline,
            status: RuntimeStatus::Stopped,
            last_processed_control_id: None,
            oracle_snapshot: None,
            scheduler_decision: None,
            active_run: None,
            last_run: None,
            last_session_report: None,
            latest_harvest: None,
            pending_harvest_count: 0,
            harvest_completed_for_session: false,
            latest_event_message: None,
        }
    }
}

impl RuntimeSnapshot {
    pub fn backend_health_at(&self, now: DateTime<Utc>) -> BackendHealth {
        let Some(heartbeat) = self.daemon_heartbeat_at else {
            return BackendHealth::Offline;
        };
        if now - heartbeat > Duration::minutes(STALE_HEARTBEAT_MINUTES) {
            BackendHealth::Stale
        } else {
            BackendHealth::Healthy
        }
    }
}

impl From<&HarvestRecord> for RuntimeHarvestSummary {
    fn from(record: &HarvestRecord) -> Self {
        Self {
            finished_at: record.finished_at.to_utc(),
            providers: record.providers.clone(),
            tasks: record.tasks.iter().map(RuntimeHarvestTask::from).collect(),
            markdown_path: None,
            banner: Some(record.banner()),
        }
    }
}

impl From<&HarvestTask> for RuntimeHarvestTask {
    fn from(task: &HarvestTask) -> Self {
        Self {
            status: match task.status {
                HarvestTaskStatus::Completed => RuntimeHarvestTaskStatus::Completed,
                HarvestTaskStatus::Failed => RuntimeHarvestTaskStatus::Failed,
            },
            mode: task.mode,
            title: task.title.clone(),
            repository: task.repository.display().to_string(),
            provider: task.provider,
            branch: task.branch.clone(),
            summary: task.summary.clone(),
        }
    }
}

pub fn read_snapshot(paths: &SideQuestPaths) -> Result<Option<RuntimeSnapshot>> {
    if !paths.runtime_snapshot_file.exists() {
        return Ok(None);
    }

    let raw = fs::read_to_string(&paths.runtime_snapshot_file)
        .with_context(|| format!("failed to read {}", paths.runtime_snapshot_file.display()))?;
    if raw.trim().is_empty() {
        return Ok(None);
    }

    let snapshot = serde_json::from_str(&raw)
        .or_else(|error| match error.classify() {
            JsonErrorCategory::Eof => Ok(RuntimeSnapshot::default()),
            _ => Err(error),
        })
        .with_context(|| format!("failed to parse {}", paths.runtime_snapshot_file.display()))?;
    Ok(Some(snapshot))
}

pub fn write_snapshot(paths: &SideQuestPaths, snapshot: &RuntimeSnapshot) -> Result<()> {
    paths.ensure()?;
    let raw =
        serde_json::to_string_pretty(snapshot).context("failed to serialize runtime snapshot")?;
    write_atomic(&paths.runtime_snapshot_file, raw.as_bytes())?;
    Ok(())
}

pub fn append_event(paths: &SideQuestPaths, event: &RuntimeEvent) -> Result<()> {
    append_json_line(&paths.runtime_events_file, event)
}

pub fn read_events(paths: &SideQuestPaths, limit: usize) -> Result<Vec<RuntimeEvent>> {
    read_json_lines::<RuntimeEvent>(&paths.runtime_events_file, limit)
}

pub fn append_control_request(paths: &SideQuestPaths, request: &ControlRequest) -> Result<()> {
    append_json_line(&paths.control_requests_file, request)
}

pub fn read_control_requests_after(
    paths: &SideQuestPaths,
    last_processed_id: Option<i64>,
) -> Result<Vec<ControlRequest>> {
    let mut requests = read_json_lines::<ControlRequest>(&paths.control_requests_file, usize::MAX)?;
    if let Some(last_id) = last_processed_id {
        requests.retain(|request| request.id > last_id);
    }
    Ok(requests)
}

pub fn next_request_id() -> i64 {
    Utc::now()
        .timestamp_nanos_opt()
        .unwrap_or_else(|| Utc::now().timestamp_micros() * 1_000)
}

fn append_json_line<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    serde_json::to_writer(&mut file, value).context("failed to encode json line")?;
    writeln!(file).context("failed to terminate json line")?;
    Ok(())
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let Some(parent) = path.parent() else {
        anyhow::bail!("{} has no parent directory", path.display());
    };
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;

    let temp_path = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("snapshot"),
        next_request_id()
    ));
    let mut temp_file = File::create(&temp_path)
        .with_context(|| format!("failed to create {}", temp_path.display()))?;
    temp_file
        .write_all(bytes)
        .with_context(|| format!("failed to write {}", temp_path.display()))?;
    temp_file
        .sync_all()
        .with_context(|| format!("failed to sync {}", temp_path.display()))?;
    drop(temp_file);

    #[cfg(windows)]
    if path.exists() {
        fs::remove_file(path).with_context(|| format!("failed to replace {}", path.display()))?;
    }

    fs::rename(&temp_path, path)
        .with_context(|| format!("failed to replace {}", path.display()))?;
    Ok(())
}

fn read_json_lines<T>(path: &Path, limit: usize) -> Result<Vec<T>>
where
    T: for<'de> Deserialize<'de>,
{
    if !path.exists() {
        return Ok(Vec::new());
    }

    let file = OpenOptions::new()
        .read(true)
        .open(path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    let reader = BufReader::new(file);
    let mut items = Vec::new();
    for line in reader.lines() {
        let line = line.with_context(|| format!("failed to read {}", path.display()))?;
        if line.trim().is_empty() {
            continue;
        }
        let item = serde_json::from_str(&line)
            .with_context(|| format!("failed to parse {} line", path.display()))?;
        items.push(item);
    }
    if items.len() > limit {
        Ok(items.split_off(items.len() - limit))
    } else {
        Ok(items)
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use tempfile::tempdir;

    use super::*;

    fn test_paths(root: &Path) -> SideQuestPaths {
        SideQuestPaths::from_root(root)
    }

    #[test]
    fn marks_backend_stale_after_heartbeat_timeout() {
        let heartbeat = Utc
            .with_ymd_and_hms(2026, 4, 3, 0, 0, 0)
            .single()
            .expect("heartbeat");
        let snapshot = RuntimeSnapshot {
            daemon_heartbeat_at: Some(heartbeat),
            ..RuntimeSnapshot::default()
        };
        let now = heartbeat + Duration::minutes(STALE_HEARTBEAT_MINUTES + 1);
        assert_eq!(snapshot.backend_health_at(now), BackendHealth::Stale);
    }

    #[test]
    fn appends_and_reads_requests_and_events() {
        let temp = tempdir().expect("temp dir");
        let paths = test_paths(temp.path());
        let request = ControlRequest {
            id: 10,
            created_at: Utc::now(),
            kind: ControlRequestKind::RunNow,
        };
        append_control_request(&paths, &request).expect("append request");
        let event = RuntimeEvent {
            id: 11,
            at: Utc::now(),
            kind: RuntimeEventKind::Heartbeat,
            message: "tick".to_string(),
            run_title: None,
        };
        append_event(&paths, &event).expect("append event");

        let requests = read_control_requests_after(&paths, Some(9)).expect("requests");
        let events = read_events(&paths, 10).expect("events");
        assert_eq!(requests.len(), 1);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].message, "tick");
    }

    #[test]
    fn treats_empty_runtime_snapshot_as_missing() {
        let temp = tempdir().expect("temp dir");
        let paths = test_paths(temp.path());
        paths.ensure().expect("ensure");
        fs::write(&paths.runtime_snapshot_file, "").expect("empty snapshot");

        let snapshot = read_snapshot(&paths).expect("snapshot");
        assert!(snapshot.is_none());
    }

    #[test]
    fn treats_truncated_runtime_snapshot_as_default() {
        let temp = tempdir().expect("temp dir");
        let paths = test_paths(temp.path());
        paths.ensure().expect("ensure");
        fs::write(&paths.runtime_snapshot_file, "{\"status\":").expect("truncated snapshot");

        let snapshot = read_snapshot(&paths)
            .expect("snapshot")
            .expect("default snapshot");
        assert_eq!(snapshot.status, RuntimeStatus::Stopped);
        assert_eq!(snapshot.backend_health, BackendHealth::Offline);
    }

    #[test]
    fn atomically_overwrites_runtime_snapshot_contents() {
        let temp = tempdir().expect("temp dir");
        let paths = test_paths(temp.path());
        let first = RuntimeSnapshot {
            status: RuntimeStatus::Starting,
            ..RuntimeSnapshot::default()
        };
        let second = RuntimeSnapshot {
            status: RuntimeStatus::Running,
            ..RuntimeSnapshot::default()
        };

        write_snapshot(&paths, &first).expect("first snapshot");
        write_snapshot(&paths, &second).expect("second snapshot");

        let snapshot = read_snapshot(&paths)
            .expect("snapshot")
            .expect("stored snapshot");
        assert_eq!(snapshot.status, RuntimeStatus::Running);
    }
}
