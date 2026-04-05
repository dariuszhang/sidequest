use std::collections::{BTreeSet, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration as StdDuration;

use anyhow::{Context, Result};
use chrono::{Duration, Local, Utc};
use fs2::FileExt;

#[path = "daemon/workspace.rs"]
mod workspace;

use crate::config::{QuestStatus, SideQuestConfig, SideQuestPaths, WorkMode};
use crate::harvester::{
    GrindBranchState, HarvestRecord, HarvestTask, HarvestTaskStatus, Harvester,
    format_pending_harvest_banner, prepare_grind_branch, run_git, short_pending_harvest_banner,
    sidequest_branch_name,
};
use crate::oracle::{OracleService, ProviderFailure};
use crate::platform::{Platform, SleepGuard};
use crate::prompts::{TaskPromptSpec, build_task_prompt};
use crate::runtime::{
    BackendHealth, ControlRequestKind, RuntimeDecision, RuntimeEvent, RuntimeEventKind,
    RuntimeHarvestSummary, RuntimeRunState, RuntimeRunStatus, RuntimeSnapshot, RuntimeStatus,
    RuntimeTaskState, append_event, next_request_id, read_control_requests_after, read_snapshot,
    write_snapshot,
};
use crate::scheduler::{DEFAULT_POLL_MINUTES, DecisionKind, SchedulerDecision, evaluate};
use crate::spawner::{AgentSpawner, TaskObserver, TaskOutcome, TaskSpec};
use crate::state::{
    BacklogItem, CompletedWorkItem, FailedWorkItem, HarvestEntry, HarvestEntryStatus,
    RemainingWorkItem, SessionReport, append_harvest_entry, append_quest_log,
    format_quest_log_document, prune_and_merge_backlog, read_backlog, read_harvest_ledger,
    read_quest_log, read_session_report, report_file_path, write_backlog, write_harvest_ledger,
    write_last_session_report,
};
use workspace::{quoted_commit_log, repo_can_accept_sidequest_run, scan_build_opportunities};

const ERROR_BACKOFF_START_SECONDS: u64 = 60;
const ERROR_BACKOFF_MAX_SECONDS: u64 = 15 * 60;

#[derive(Debug, Clone)]
pub struct RunReport {
    pub initial_decision: SchedulerDecision,
    pub final_decision: SchedulerDecision,
    pub harvest_path: Option<PathBuf>,
    pub tasks: Vec<HarvestTask>,
    pub provider_failures: Vec<ProviderFailure>,
    pub daemon_stop_requested: bool,
}

#[derive(Debug, Clone)]
struct WorkCandidate {
    id: String,
    mode: WorkMode,
    repo_path: PathBuf,
    title: String,
    summary: String,
    score: usize,
    context: String,
    commit_message: String,
    repository: Option<String>,
    quest: Option<String>,
}

#[derive(Debug, Clone)]
struct RunPlan {
    chosen: WorkCandidate,
    remaining: Vec<RemainingWorkItem>,
}

struct RuntimeTaskObserver<'a> {
    paths: &'a SideQuestPaths,
    snapshot: &'a mut RuntimeSnapshot,
    run_title: String,
    stop_recorded: bool,
    stop_kind: Option<ControlRequestKind>,
}

impl TaskObserver for RuntimeTaskObserver<'_> {
    fn on_branch_created(&mut self, branch: &str) {
        if let Some(run) = self.snapshot.active_run.as_mut() {
            run.branch = Some(branch.to_string());
        }
        let _ = record_runtime_event(
            self.paths,
            self.snapshot,
            RuntimeEventKind::BranchCreated,
            format!("checked out SideQuest branch `{branch}`"),
            Some(self.run_title.clone()),
        );
    }

    fn on_agent_spawned(&mut self, log_path: &Path) {
        if let Some(run) = self.snapshot.active_run.as_mut() {
            run.log_path = Some(log_path.display().to_string());
        }
        let _ = record_runtime_event(
            self.paths,
            self.snapshot,
            RuntimeEventKind::AgentSpawned,
            format!("spawned agent log at {}", log_path.display()),
            Some(self.run_title.clone()),
        );
    }

    fn should_stop(&mut self) -> bool {
        if self.stop_recorded {
            return true;
        }
        let Ok(requests) =
            read_control_requests_after(self.paths, self.snapshot.last_processed_control_id)
        else {
            return false;
        };
        let Some(request) = requests.into_iter().find(|request| {
            matches!(
                request.kind,
                ControlRequestKind::StopActiveRun | ControlRequestKind::StopDaemon
            )
        }) else {
            return false;
        };

        self.snapshot.last_processed_control_id = Some(request.id);
        self.stop_recorded = true;
        self.stop_kind = Some(request.kind);
        let message = match request.kind {
            ControlRequestKind::StopActiveRun => "received stop request for active run",
            ControlRequestKind::StopDaemon => {
                "received daemon stop request; stopping active run before shutdown"
            }
            ControlRequestKind::RunNow => unreachable!("run-now is not a stop signal"),
            ControlRequestKind::HarvestCompleted => {
                unreachable!("harvest-completed is not a stop signal")
            }
        };
        let _ = record_runtime_event(
            self.paths,
            self.snapshot,
            RuntimeEventKind::StopRequested,
            message.to_string(),
            Some(self.run_title.clone()),
        );
        true
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WaitOutcome {
    TimedOut,
    RunNowRequested,
    StopDaemonRequested,
}

#[derive(Debug, Clone, Copy, Default)]
struct ConsumedControls {
    daemon_stop_requested: bool,
}

struct InstanceLock {
    file: File,
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

pub struct SideQuestDaemon {
    paths: SideQuestPaths,
    platform: Box<dyn Platform>,
}

impl SideQuestDaemon {
    pub fn new(platform: Box<dyn Platform>) -> Result<Self> {
        let paths = SideQuestPaths::discover()?;
        paths.ensure()?;
        Ok(Self { paths, platform })
    }

    pub fn run_once(&self, config: &SideQuestConfig, force_window: bool) -> Result<RunReport> {
        let _lock = self
            .try_acquire_instance_lock()?
            .ok_or_else(|| anyhow::anyhow!("another SideQuest instance is already active"))?;
        let mut sleep_guard = None;
        self.run_once_locked(config, force_window, &mut sleep_guard, false)
    }

    pub fn run_forever(&self) -> Result<()> {
        let _lock = match self.try_acquire_instance_lock()? {
            Some(lock) => lock,
            None => {
                let _ = self.append_daemon_log(
                    "daemon already active; exiting quietly because another instance holds the lock",
                );
                return Ok(());
            }
        };
        let mut sleep_guard: Option<Box<dyn SleepGuard>> = None;
        let mut error_backoff = StdDuration::from_secs(ERROR_BACKOFF_START_SECONDS);
        let mut runtime_snapshot = self.load_runtime_snapshot()?;
        daemon_heartbeat(&mut runtime_snapshot, RuntimeStatus::Starting);
        write_snapshot(&self.paths, &runtime_snapshot)?;
        record_runtime_event(
            &self.paths,
            &mut runtime_snapshot,
            RuntimeEventKind::DaemonStarted,
            "SideQuest daemon started".to_string(),
            None,
        )?;

        loop {
            match self.run_forever_tick(&mut sleep_guard) {
                Ok((report, wait)) => {
                    error_backoff = StdDuration::from_secs(ERROR_BACKOFF_START_SECONDS);
                    let _ = self.append_daemon_log(&format!(
                        "tick: {} ({} tasks)",
                        report.final_decision.reason,
                        report.tasks.len()
                    ));
                    if report.daemon_stop_requested {
                        self.mark_daemon_stopped(
                            "daemon stop request completed; shutting down daemon loop",
                        )?;
                        let _ = self.append_daemon_log(
                            "daemon stop request completed; shutting down daemon loop",
                        );
                        return Ok(());
                    }
                    match self.wait_for_next_tick(wait)? {
                        WaitOutcome::RunNowRequested => continue,
                        WaitOutcome::StopDaemonRequested => {
                            self.mark_daemon_stopped(
                                "received daemon stop request while idle; shutting down daemon loop",
                            )?;
                            let _ = self.append_daemon_log(
                                "received daemon stop request while idle; shutting down daemon loop",
                            );
                            return Ok(());
                        }
                        WaitOutcome::TimedOut => {}
                    }
                }
                Err(error) => {
                    let _ = self.append_daemon_log(&format!(
                        "daemon tick failed; SideQuest will retry after backoff: {}",
                        error
                    ));
                    let mut snapshot = self.load_runtime_snapshot()?;
                    daemon_heartbeat(&mut snapshot, RuntimeStatus::Backoff);
                    record_runtime_event(
                        &self.paths,
                        &mut snapshot,
                        RuntimeEventKind::BackoffStarted,
                        format!(
                            "daemon tick failed; retrying in {} seconds",
                            error_backoff.as_secs()
                        ),
                        None,
                    )?;
                    match self.wait_for_next_tick(error_backoff)? {
                        WaitOutcome::RunNowRequested => {
                            error_backoff = StdDuration::from_secs(ERROR_BACKOFF_START_SECONDS);
                            continue;
                        }
                        WaitOutcome::StopDaemonRequested => {
                            self.mark_daemon_stopped(
                                "received daemon stop request during backoff; shutting down daemon loop",
                            )?;
                            let _ = self.append_daemon_log(
                                "received daemon stop request during backoff; shutting down daemon loop",
                            );
                            return Ok(());
                        }
                        WaitOutcome::TimedOut => {}
                    }
                    error_backoff = StdDuration::from_secs(
                        error_backoff
                            .as_secs()
                            .saturating_mul(2)
                            .clamp(ERROR_BACKOFF_START_SECONDS, ERROR_BACKOFF_MAX_SECONDS),
                    );
                }
            }
        }
    }

    fn run_forever_tick(
        &self,
        sleep_guard: &mut Option<Box<dyn SleepGuard>>,
    ) -> Result<(RunReport, StdDuration)> {
        let config = SideQuestConfig::load_or_create_default_from_paths(&self.paths)
            .context("failed to load SideQuest config for daemon tick")?;
        let report = self.run_once_locked(&config, false, sleep_guard, true)?;
        if self.should_hold_sleep_guard(&report.final_decision) {
            self.sync_sleep_guard(&config, sleep_guard)?;
        } else {
            sleep_guard.take();
        }
        let now = Local::now();
        let wait = if report.final_decision.next_check_at > now {
            (report.final_decision.next_check_at - now)
                .to_std()
                .unwrap_or_else(|_| StdDuration::from_secs(DEFAULT_POLL_MINUTES as u64 * 60))
        } else {
            StdDuration::from_secs(DEFAULT_POLL_MINUTES as u64 * 60)
        };
        Ok((report, wait))
    }

    fn run_once_locked(
        &self,
        config: &SideQuestConfig,
        force_window: bool,
        sleep_guard: &mut Option<Box<dyn SleepGuard>>,
        daemon_mode: bool,
    ) -> Result<RunReport> {
        let mut runtime_config = config.clone();
        let oracle = OracleService::new(self.platform.as_ref());
        let harvester = Harvester::new(self.paths.clone());
        let spawner = AgentSpawner::new(self.paths.clone());
        let mut completed = HashSet::new();
        let mut harvested_tasks = Vec::new();
        let mut used_providers = BTreeSet::new();
        let mut daemon_stop_requested = false;
        let mut backlog = read_backlog(&self.paths)?;
        let mut runtime_snapshot = self.load_runtime_snapshot()?;

        self.sync_sleep_guard(config, sleep_guard)?;
        let initial_snapshot = oracle.snapshot(&runtime_config);
        let initial_decision = evaluate(
            Local::now(),
            &runtime_config,
            &initial_snapshot.budgets,
            force_window,
        )?;
        let mut current_decision = initial_decision.clone();
        let provider_failures = initial_snapshot.failures.clone();
        runtime_snapshot.pending_harvest_count = self.pending_harvest_count()?;
        if initial_decision.kind == DecisionKind::OutsideSleepWindow {
            runtime_snapshot.harvest_completed_for_session = false;
        }
        let controls = self.consume_control_requests(&mut runtime_snapshot)?;
        daemon_stop_requested |= controls.daemon_stop_requested;
        daemon_heartbeat(
            &mut runtime_snapshot,
            if daemon_mode {
                RuntimeStatus::Idle
            } else {
                RuntimeStatus::Starting
            },
        );
        runtime_snapshot.oracle_snapshot = Some(initial_snapshot.clone());
        runtime_snapshot.scheduler_decision = Some(RuntimeDecision::from(&initial_decision));
        runtime_snapshot.last_session_report =
            read_session_report(&self.paths.last_session_report_file)?;
        write_snapshot(&self.paths, &runtime_snapshot)?;
        record_runtime_event(
            &self.paths,
            &mut runtime_snapshot,
            RuntimeEventKind::SchedulerEvaluated,
            initial_decision.reason.clone(),
            None,
        )?;

        while current_decision.kind == DecisionKind::RunNow {
            let controls = self.consume_control_requests(&mut runtime_snapshot)?;
            daemon_stop_requested |= controls.daemon_stop_requested;
            if runtime_snapshot.harvest_completed_for_session {
                current_decision = SchedulerDecision {
                    kind: DecisionKind::AfterCutoff,
                    reason: "harvest completed for this sleep window; waiting until the next night"
                        .to_string(),
                    next_check_at: Local::now() + Duration::minutes(DEFAULT_POLL_MINUTES),
                    ..current_decision
                };
                runtime_snapshot.scheduler_decision =
                    Some(RuntimeDecision::from(&current_decision));
                write_snapshot(&self.paths, &runtime_snapshot)?;
                break;
            }

            let provider = match current_decision.provider.clone() {
                Some(provider) => provider,
                None => break,
            };

            let candidates = self.collect_candidates(&runtime_config, &backlog)?;
            let Some(plan) = self.plan_run(&runtime_config, &candidates, &completed) else {
                current_decision = SchedulerDecision {
                    reason: "no eligible grind or quest work was found".to_string(),
                    ..current_decision
                };
                runtime_snapshot.scheduler_decision =
                    Some(RuntimeDecision::from(&current_decision));
                runtime_snapshot.active_run = None;
                daemon_heartbeat(
                    &mut runtime_snapshot,
                    if daemon_mode {
                        RuntimeStatus::Idle
                    } else {
                        RuntimeStatus::Stopped
                    },
                );
                write_snapshot(&self.paths, &runtime_snapshot)?;
                break;
            };

            if plan.chosen.mode == WorkMode::Grind {
                let branch_name = plan.chosen.branch_name();
                let mut ledger = read_harvest_ledger(&self.paths)?;
                let branch_state =
                    prepare_grind_branch(&plan.chosen.repo_path, &branch_name, &mut ledger)?;
                write_harvest_ledger(&self.paths, &ledger)?;
                let branch_message = match branch_state {
                    GrindBranchState::Fresh => {
                        format!("prepared fresh grind branch `{branch_name}`")
                    }
                    GrindBranchState::Continuing => {
                        format!("continuing on existing grind branch `{branch_name}`")
                    }
                    GrindBranchState::Rebased { old_commit_count } => format!(
                        "refreshed `{branch_name}` onto the current main branch with {old_commit_count} pending commit{}",
                        if old_commit_count == 1 { "" } else { "s" }
                    ),
                    GrindBranchState::FreshAfterStale { stale_count } => format!(
                        "reset `{branch_name}` after {stale_count} stale pending commit{} could not be replayed safely",
                        if stale_count == 1 { "" } else { "s" }
                    ),
                };
                let _ = self.append_daemon_log(&branch_message);
            }

            let task_spec = TaskSpec {
                mode: plan.chosen.mode,
                repo_path: plan.chosen.repo_path.clone(),
                title: plan.chosen.title.clone(),
                quest: plan.chosen.quest.clone(),
                prompt: build_task_prompt(&TaskPromptSpec {
                    mode: plan.chosen.mode,
                    title: &plan.chosen.title,
                    context: &plan.chosen.context,
                    remaining: &plan.remaining,
                    prompts: &runtime_config.prompts,
                    spendable_budget: provider.spendable_budget,
                    cutoff_time: current_decision.cutoff_time,
                    report_path: &report_file_path(&plan.chosen.repo_path),
                }),
                commit_message: plan.chosen.commit_message.clone(),
            };
            runtime_snapshot.active_run = Some(RuntimeRunState {
                provider: provider.provider,
                mode: task_spec.mode,
                repo_path: task_spec.repo_path.display().to_string(),
                title: task_spec.title.clone(),
                quest: task_spec.quest.clone(),
                branch: None,
                started_at: Utc::now(),
                cutoff_time: current_decision.cutoff_time.to_utc(),
                completed_at: None,
                log_path: None,
                status: RuntimeRunStatus::Running,
                summary: None,
                commit: None,
                short_stat: None,
                report_found: false,
                clean_exit: false,
                stop_reason: None,
                task_state: RuntimeTaskState::default(),
            });
            daemon_heartbeat(&mut runtime_snapshot, RuntimeStatus::Running);
            record_runtime_event(
                &self.paths,
                &mut runtime_snapshot,
                RuntimeEventKind::TaskSelected,
                format!(
                    "selected {} task `{}` on {}",
                    task_spec.mode, task_spec.title, provider.provider
                ),
                Some(task_spec.title.clone()),
            )?;

            let run_result = {
                let mut observer = RuntimeTaskObserver {
                    paths: &self.paths,
                    snapshot: &mut runtime_snapshot,
                    run_title: task_spec.title.clone(),
                    stop_recorded: false,
                    stop_kind: None,
                };
                let result = spawner.run_task(
                    &runtime_config,
                    &oracle,
                    provider.provider,
                    current_decision.cutoff_time,
                    &task_spec,
                    &mut observer,
                );
                daemon_stop_requested |=
                    matches!(observer.stop_kind, Some(ControlRequestKind::StopDaemon));
                result
            };
            let outcome = match run_result {
                Ok(outcome) => outcome,
                Err(error) => {
                    runtime_snapshot.active_run = None;
                    runtime_snapshot.scheduler_decision =
                        Some(RuntimeDecision::from(&current_decision));
                    daemon_heartbeat(
                        &mut runtime_snapshot,
                        if daemon_mode {
                            RuntimeStatus::Idle
                        } else {
                            RuntimeStatus::Stopped
                        },
                    );
                    record_runtime_event(
                        &self.paths,
                        &mut runtime_snapshot,
                        RuntimeEventKind::Error,
                        format!(
                            "task `{}` failed before completion: {}",
                            plan.chosen.title, error
                        ),
                        Some(plan.chosen.title.clone()),
                    )?;
                    let _ = self.append_daemon_log(&format!(
                        "task failed: {} ({})",
                        plan.chosen.title, error
                    ));
                    if harvested_tasks.is_empty() {
                        return Err(error);
                    }

                    current_decision = SchedulerDecision {
                        reason: format!(
                            "stopped after task failure in `{}`; earlier work was preserved",
                            plan.chosen.title
                        ),
                        next_check_at: Local::now() + Duration::minutes(DEFAULT_POLL_MINUTES),
                        ..current_decision
                    };
                    break;
                }
            };
            used_providers.insert(outcome.provider);
            write_last_session_report(&self.paths, &outcome.report)?;
            let last_run = runtime_run_from_outcome(
                runtime_snapshot.active_run.as_ref(),
                &task_spec,
                &outcome,
                current_decision.cutoff_time,
            );
            runtime_snapshot.last_run = Some(last_run);
            runtime_snapshot.last_session_report = Some(outcome.report.clone());
            runtime_snapshot.active_run = None;
            daemon_heartbeat(
                &mut runtime_snapshot,
                if daemon_mode {
                    RuntimeStatus::Idle
                } else {
                    RuntimeStatus::Stopped
                },
            );
            let event_kind = if outcome.stop_reason.is_some() {
                RuntimeEventKind::TaskStopped
            } else if outcome.report.completed.is_empty() {
                RuntimeEventKind::TaskFailed
            } else {
                RuntimeEventKind::TaskCompleted
            };
            record_runtime_event(
                &self.paths,
                &mut runtime_snapshot,
                event_kind,
                outcome.summary.clone(),
                Some(task_spec.title.clone()),
            )?;

            let mut run_tasks = self.harvest_tasks_from_outcome(&plan.chosen, &outcome);
            harvested_tasks.append(&mut run_tasks);
            for entry in self.harvest_entries_from_outcome(&plan.chosen, &outcome) {
                append_harvest_entry(&self.paths, entry)?;
            }
            runtime_snapshot.pending_harvest_count = self.pending_harvest_count()?;

            if let Some(quest_name) = &plan.chosen.quest {
                let quest_completed: Vec<CompletedWorkItem> = outcome
                    .report
                    .completed
                    .iter()
                    .filter(|item| item.quest.as_deref() == Some(quest_name.as_str()))
                    .cloned()
                    .collect();
                let quest_remaining: Vec<RemainingWorkItem> = outcome
                    .report
                    .remaining_work
                    .iter()
                    .filter(|item| item.quest.as_deref() == Some(quest_name.as_str()))
                    .cloned()
                    .collect();
                let entry = format_quest_log_document(&quest_completed, &quest_remaining);
                append_quest_log(&self.paths, quest_name, &entry)?;

                if quest_is_complete_for_now(quest_name, &outcome.report) {
                    complete_quest(&self.paths, &mut runtime_config, quest_name)?;
                }
            }

            if should_mark_candidate_complete(&plan.chosen, &outcome.report) {
                completed.insert(plan.chosen.id.clone());
            }

            backlog = prune_and_merge_backlog(
                &backlog,
                &outcome.report.completed,
                &outcome.report.remaining_work,
                Utc::now(),
            );
            write_backlog(&self.paths, &backlog)?;

            if daemon_stop_requested {
                current_decision = SchedulerDecision {
                    reason: "daemon stop requested by user".to_string(),
                    next_check_at: Local::now(),
                    ..current_decision
                };
                runtime_snapshot.scheduler_decision =
                    Some(RuntimeDecision::from(&current_decision));
                write_snapshot(&self.paths, &runtime_snapshot)?;
                break;
            }

            let snapshot = oracle.snapshot(&runtime_config);
            current_decision = evaluate(
                Local::now(),
                &runtime_config,
                &snapshot.budgets,
                force_window,
            )?;
            runtime_snapshot.oracle_snapshot = Some(snapshot);
            runtime_snapshot.scheduler_decision = Some(RuntimeDecision::from(&current_decision));
            write_snapshot(&self.paths, &runtime_snapshot)?;
            self.sync_sleep_guard(config, sleep_guard)?;
        }

        let harvest_path = if harvested_tasks.is_empty() {
            None
        } else {
            let providers = if used_providers.is_empty() {
                initial_decision
                    .provider
                    .as_ref()
                    .map(|provider| vec![provider.provider])
                    .unwrap_or_default()
            } else {
                used_providers.into_iter().collect()
            };
            let record = HarvestRecord {
                finished_at: Local::now(),
                providers,
                tasks: harvested_tasks.clone(),
                spend_used_fraction: None,
            };
            let harvest_path = harvester.write_harvest(&record)?;
            let mut harvest_summary = RuntimeHarvestSummary::from(&record);
            harvest_summary.markdown_path = Some(harvest_path.display().to_string());
            runtime_snapshot.latest_harvest = Some(harvest_summary);
            record_runtime_event(
                &self.paths,
                &mut runtime_snapshot,
                RuntimeEventKind::HarvestWritten,
                format!("wrote harvest to {}", harvest_path.display()),
                None,
            )?;
            Some(harvest_path)
        };

        let pending_entries = self.pending_harvest_entries()?;
        runtime_snapshot.pending_harvest_count = pending_entries.len();
        let banner = format_pending_harvest_banner(&pending_entries);
        fs::write(&self.paths.latest_harvest_file, banner).with_context(|| {
            format!(
                "failed to write {}",
                self.paths.latest_harvest_file.display()
            )
        })?;
        if config.notifications.desktop
            && !harvested_tasks.is_empty()
            && !pending_entries.is_empty()
        {
            let _ = self.platform.send_notification(
                "SideQuest loot ready",
                &short_pending_harvest_banner(&pending_entries),
            );
        }

        runtime_snapshot.scheduler_decision = Some(RuntimeDecision::from(&current_decision));
        runtime_snapshot.active_run = None;
        daemon_heartbeat(
            &mut runtime_snapshot,
            if daemon_mode {
                RuntimeStatus::Idle
            } else {
                RuntimeStatus::Stopped
            },
        );
        write_snapshot(&self.paths, &runtime_snapshot)?;

        Ok(RunReport {
            initial_decision,
            final_decision: current_decision,
            harvest_path,
            tasks: harvested_tasks,
            provider_failures,
            daemon_stop_requested,
        })
    }

    fn load_runtime_snapshot(&self) -> Result<RuntimeSnapshot> {
        Ok(read_snapshot(&self.paths)?.unwrap_or_default())
    }

    fn pending_harvest_count(&self) -> Result<usize> {
        Ok(self.pending_harvest_entries()?.len())
    }

    fn pending_harvest_entries(&self) -> Result<Vec<HarvestEntry>> {
        let ledger = read_harvest_ledger(&self.paths)?;
        Ok(ledger
            .entries
            .into_iter()
            .filter(|entry| entry.status == HarvestEntryStatus::Pending)
            .collect())
    }

    fn consume_control_requests(&self, snapshot: &mut RuntimeSnapshot) -> Result<ConsumedControls> {
        let mut controls = ConsumedControls::default();
        let requests =
            read_control_requests_after(&self.paths, snapshot.last_processed_control_id)?;
        for request in requests {
            snapshot.last_processed_control_id = Some(request.id);
            match request.kind {
                ControlRequestKind::RunNow => {
                    snapshot.harvest_completed_for_session = false;
                }
                ControlRequestKind::StopActiveRun => {}
                ControlRequestKind::HarvestCompleted => {
                    snapshot.harvest_completed_for_session = true;
                }
                ControlRequestKind::StopDaemon => {
                    controls.daemon_stop_requested = true;
                }
            }
        }
        Ok(controls)
    }

    fn wait_for_next_tick(&self, wait: StdDuration) -> Result<WaitOutcome> {
        let deadline = std::time::Instant::now() + wait;
        loop {
            let now = std::time::Instant::now();
            if now >= deadline {
                return Ok(WaitOutcome::TimedOut);
            }

            let mut snapshot = self.load_runtime_snapshot()?;
            let status = if snapshot.active_run.is_some() {
                RuntimeStatus::Running
            } else {
                RuntimeStatus::Idle
            };
            daemon_heartbeat(&mut snapshot, status);
            let pending =
                read_control_requests_after(&self.paths, snapshot.last_processed_control_id)?;
            let mut run_now = false;
            for request in pending {
                snapshot.last_processed_control_id = Some(request.id);
                match request.kind {
                    ControlRequestKind::RunNow => {
                        run_now = true;
                        snapshot.harvest_completed_for_session = false;
                        record_runtime_event(
                            &self.paths,
                            &mut snapshot,
                            RuntimeEventKind::Waiting,
                            "received run-now request".to_string(),
                            None,
                        )?;
                    }
                    ControlRequestKind::StopActiveRun => {
                        record_runtime_event(
                            &self.paths,
                            &mut snapshot,
                            RuntimeEventKind::StopRequested,
                            "stop request ignored because no run is active".to_string(),
                            None,
                        )?;
                    }
                    ControlRequestKind::HarvestCompleted => {
                        snapshot.harvest_completed_for_session = true;
                        record_runtime_event(
                            &self.paths,
                            &mut snapshot,
                            RuntimeEventKind::Waiting,
                            "harvest marked complete for this sleep window".to_string(),
                            None,
                        )?;
                    }
                    ControlRequestKind::StopDaemon => {
                        record_runtime_event(
                            &self.paths,
                            &mut snapshot,
                            RuntimeEventKind::StopRequested,
                            "received daemon stop request while idle".to_string(),
                            None,
                        )?;
                        return Ok(WaitOutcome::StopDaemonRequested);
                    }
                }
            }

            if run_now {
                return Ok(WaitOutcome::RunNowRequested);
            }

            write_snapshot(&self.paths, &snapshot)?;
            let remaining = deadline.saturating_duration_since(now);
            thread::sleep(remaining.min(StdDuration::from_secs(1)));
        }
    }

    fn mark_daemon_stopped(&self, message: &str) -> Result<()> {
        let mut snapshot = self.load_runtime_snapshot()?;
        snapshot.active_run = None;
        daemon_heartbeat(&mut snapshot, RuntimeStatus::Stopped);
        record_runtime_event(
            &self.paths,
            &mut snapshot,
            RuntimeEventKind::StopRequested,
            message.to_string(),
            None,
        )?;
        Ok(())
    }

    pub fn append_daemon_log(&self, line: &str) -> Result<()> {
        self.paths.ensure()?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.paths.daemon_log_file)
            .with_context(|| format!("failed to open {}", self.paths.daemon_log_file.display()))?;
        writeln!(
            file,
            "[{}] {}",
            Local::now().format("%Y-%m-%d %H:%M:%S"),
            line
        )?;
        Ok(())
    }

    pub fn paths(&self) -> &SideQuestPaths {
        &self.paths
    }

    fn try_acquire_instance_lock(&self) -> Result<Option<InstanceLock>> {
        self.paths.ensure()?;
        let lock_path = self.paths.instance_lock_file();
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .with_context(|| format!("failed to open {}", lock_path.display()))?;

        match file.try_lock_exclusive() {
            Ok(()) => {
                file.set_len(0)
                    .with_context(|| format!("failed to reset {}", lock_path.display()))?;
                writeln!(
                    file,
                    "pid={} started_at={}",
                    std::process::id(),
                    Local::now().format("%Y-%m-%d %H:%M:%S")
                )?;
                Ok(Some(InstanceLock { file }))
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => Ok(None),
            Err(error) => {
                Err(error).with_context(|| format!("failed to lock {}", lock_path.display()))
            }
        }
    }

    fn sync_sleep_guard(
        &self,
        config: &SideQuestConfig,
        sleep_guard: &mut Option<Box<dyn SleepGuard>>,
    ) -> Result<()> {
        if self.sleep_window_active(config)? {
            if sleep_guard.is_none() {
                *sleep_guard = self
                    .platform
                    .begin_sleep_prevention("SideQuest overnight window")?;
            }
        } else {
            sleep_guard.take();
        }
        Ok(())
    }

    fn sleep_window_active(&self, config: &SideQuestConfig) -> Result<bool> {
        let decision = evaluate(Local::now(), config, &[], false)?;
        Ok(decision.kind != DecisionKind::OutsideSleepWindow)
    }

    fn should_hold_sleep_guard(&self, decision: &SchedulerDecision) -> bool {
        decision.kind != DecisionKind::OutsideSleepWindow
            && decision.next_check_at < decision.wake_time
    }

    fn collect_candidates(
        &self,
        config: &SideQuestConfig,
        backlog: &[BacklogItem],
    ) -> Result<Vec<WorkCandidate>> {
        let mut candidates = Vec::new();
        candidates.extend(self.collect_grind_candidates(config)?);
        candidates.extend(self.collect_backlog_candidates(backlog)?);
        candidates.extend(self.collect_quest_candidates(config)?);

        let mut deduped = Vec::new();
        let mut seen = HashSet::new();
        for candidate in candidates {
            if seen.insert(candidate.id.clone()) {
                deduped.push(candidate);
            }
        }
        Ok(deduped)
    }

    fn plan_run(
        &self,
        config: &SideQuestConfig,
        candidates: &[WorkCandidate],
        completed: &HashSet<String>,
    ) -> Option<RunPlan> {
        let mode_order = if config.prefer_quests {
            [WorkMode::Quest, WorkMode::Grind]
        } else {
            [WorkMode::Grind, WorkMode::Quest]
        };

        for mode in mode_order {
            let mut mode_candidates: Vec<WorkCandidate> = candidates
                .iter()
                .filter(|candidate| candidate.mode == mode && !completed.contains(&candidate.id))
                .cloned()
                .collect();
            mode_candidates.sort_by(|left, right| {
                right
                    .score
                    .cmp(&left.score)
                    .then_with(|| left.title.cmp(&right.title))
            });
            if let Some(chosen) = mode_candidates.into_iter().next() {
                let remaining = candidates
                    .iter()
                    .filter(|candidate| {
                        candidate.id != chosen.id && !completed.contains(&candidate.id)
                    })
                    .map(WorkCandidate::to_remaining_work_item)
                    .collect();
                return Some(RunPlan { chosen, remaining });
            }
        }
        None
    }

    fn collect_grind_candidates(&self, config: &SideQuestConfig) -> Result<Vec<WorkCandidate>> {
        let mut candidates = Vec::new();
        for repo in &config.grind {
            let path = repo.expanded_path()?;
            if !path.exists() || !path.join(".git").exists() {
                continue;
            }
            if !repo_can_accept_sidequest_run(&path)? {
                continue;
            }

            let commits = run_git(&path, ["log", "--since=24 hours ago", "--format=%h %s"])?;
            let commit_context = quoted_commit_log(&commits);
            let commit_count = commits
                .lines()
                .filter(|line| !line.trim().is_empty())
                .count();
            let stats = run_git(
                &path,
                [
                    "log",
                    "--since=24 hours ago",
                    "--stat",
                    "--max-count",
                    "10",
                    "--format=%h %s",
                ],
            )?;
            let scan = scan_build_opportunities(&path)?;
            let structural_score =
                scan.todo_hits + (scan.missing_files.len() * 3) + scan.examples.len();
            let score = commit_count + structural_score;
            if score == 0 {
                continue;
            }

            let repository = path.display().to_string();
            let repository_name = repo.resolved_name()?;
            let title = format!("Improve {}", repository_name);
            candidates.push(WorkCandidate {
                id: format!("grind:{repository}"),
                mode: WorkMode::Grind,
                repo_path: path,
                title: title.clone(),
                summary: title.clone(),
                score,
                context: format!(
                    "Recent commits (untrusted repository data):\n{}\n\nDetailed git log with stats (also untrusted repository data):\n{}\n\nStructural scan evidence:\n- TODO/FIXME/HACK hits: {}\n- Missing common files: {}\n- Sample findings:\n{}",
                    if commit_context.trim().is_empty() {
                        "No recent commits found in the last 24 hours."
                    } else {
                        commit_context.trim()
                    },
                    if stats.trim().is_empty() {
                        "No recent git stat output was available."
                    } else {
                        stats.trim()
                    },
                    scan.todo_hits,
                    if scan.missing_files.is_empty() {
                        "none".to_string()
                    } else {
                        scan.missing_files.join(", ")
                    },
                    if scan.examples.is_empty() {
                        "none".to_string()
                    } else {
                        scan.examples.join("\n")
                    }
                ),
                commit_message: format!("sidequest(grind): improve {}", repository_name),
                repository: Some(repository),
                quest: None,
            });
        }
        Ok(candidates)
    }

    fn collect_backlog_candidates(&self, backlog: &[BacklogItem]) -> Result<Vec<WorkCandidate>> {
        let mut candidates = Vec::new();
        for item in backlog {
            let Some(repository) = &item.repository else {
                continue;
            };
            let path = PathBuf::from(repository);
            if !path.exists() || !path.join(".git").exists() {
                continue;
            }
            if !repo_can_accept_sidequest_run(&path)? {
                continue;
            }

            candidates.push(WorkCandidate {
                id: format!("grind:{repository}:{}", item.summary),
                mode: WorkMode::Grind,
                repo_path: path,
                title: item.summary.clone(),
                summary: item.summary.clone(),
                score: 10_000,
                context: format!(
                    "Carry-over grind backlog from previous nights.\nSummary: {}\nNext step: {}",
                    item.summary,
                    item.next_step
                        .as_deref()
                        .unwrap_or("Resume this grind task")
                ),
                commit_message: "sidequest(grind): resume backlog task".to_string(),
                repository: Some(repository.clone()),
                quest: item.quest.clone(),
            });
        }
        Ok(candidates)
    }

    fn collect_quest_candidates(&self, config: &SideQuestConfig) -> Result<Vec<WorkCandidate>> {
        let mut candidates = Vec::new();
        for quest in config
            .quests
            .iter()
            .filter(|quest| quest.status == QuestStatus::Active)
        {
            let path = quest.expanded_directory()?;
            if path.join(".git").exists() && !repo_can_accept_sidequest_run(&path)? {
                continue;
            }
            let title = format!("Advance quest {}", quest.name);
            let progress_log = read_quest_log(&self.paths, &quest.name)?
                .unwrap_or_else(|| "No previous quest progress was recorded.".to_string());
            candidates.push(WorkCandidate {
                id: format!("quest:{}", quest.name),
                mode: WorkMode::Quest,
                repo_path: path.clone(),
                title: title.clone(),
                summary: title,
                score: 1,
                context: format!(
                    "Quest goal:\n{}\n\nPrevious progress:\n{}",
                    quest.resolve_goal()?,
                    progress_log
                ),
                commit_message: format!("sidequest(quest): advance {}", quest.name),
                repository: Some(path.display().to_string()),
                quest: Some(quest.name.clone()),
            });
        }
        Ok(candidates)
    }

    fn harvest_tasks_from_outcome(
        &self,
        candidate: &WorkCandidate,
        outcome: &TaskOutcome,
    ) -> Vec<HarvestTask> {
        let mut tasks = Vec::new();
        for item in &outcome.report.completed {
            tasks.push(HarvestTask {
                status: HarvestTaskStatus::Completed,
                mode: item.mode,
                title: item.summary.clone(),
                repository: item
                    .repository
                    .as_ref()
                    .map(PathBuf::from)
                    .unwrap_or_else(|| candidate.repo_path.clone()),
                provider: outcome.provider,
                branch: item
                    .branch
                    .clone()
                    .unwrap_or_else(|| outcome.branch.clone()),
                summary: if outcome.report_found {
                    outcome.summary.clone()
                } else {
                    item.summary.clone()
                },
                commit: outcome.commit.clone(),
                short_stat: item
                    .diff_summary
                    .clone()
                    .or_else(|| outcome.short_stat.clone()),
                tests_added: item.tests_added,
                tests_passing: item.tests_passing,
                next_step: item.next_step.clone(),
            });
        }

        for item in &outcome.report.attempted_but_failed {
            let next_step = matching_next_step(item, &outcome.report.remaining_work);
            tasks.push(HarvestTask {
                status: HarvestTaskStatus::Failed,
                mode: item.mode,
                title: item.summary.clone(),
                repository: item
                    .repository
                    .as_ref()
                    .map(PathBuf::from)
                    .unwrap_or_else(|| candidate.repo_path.clone()),
                provider: outcome.provider,
                branch: outcome.branch.clone(),
                summary: item.reason.clone(),
                commit: outcome.commit.clone(),
                short_stat: outcome.short_stat.clone(),
                tests_added: None,
                tests_passing: None,
                next_step,
            });
        }

        tasks
    }

    fn harvest_entries_from_outcome(
        &self,
        candidate: &WorkCandidate,
        outcome: &TaskOutcome,
    ) -> Vec<HarvestEntry> {
        let mut entries = Vec::new();
        let created_at = Utc::now();
        for item in &outcome.report.completed {
            let repo_path = item
                .repository
                .as_ref()
                .map(PathBuf::from)
                .unwrap_or_else(|| candidate.repo_path.clone());
            entries.push(HarvestEntry {
                id: String::new(),
                repo_name: repo_path
                    .file_name()
                    .map(|name| name.to_string_lossy().to_string())
                    .unwrap_or_else(|| repo_path.display().to_string()),
                repo_path,
                branch: item
                    .branch
                    .clone()
                    .unwrap_or_else(|| outcome.branch.clone()),
                commit: outcome.commit.clone().unwrap_or_default(),
                mode: item.mode,
                title: item.summary.clone(),
                summary: if outcome.report_found {
                    outcome.summary.clone()
                } else {
                    item.summary.clone()
                },
                quest: item.quest.clone(),
                provider: outcome.provider,
                short_stat: item
                    .diff_summary
                    .clone()
                    .or_else(|| outcome.short_stat.clone()),
                files_changed: item.files_changed,
                insertions: None,
                deletions: None,
                tests_added: item.tests_added,
                tests_passing: item.tests_passing,
                created_at,
                night_of: Some(created_at.with_timezone(&Local).date_naive()),
                status: HarvestEntryStatus::Pending,
                looted_at: None,
                clean_exit: outcome.clean_exit,
                note: None,
            });
        }

        for item in &outcome.report.attempted_but_failed {
            let repo_path = item
                .repository
                .as_ref()
                .map(PathBuf::from)
                .unwrap_or_else(|| candidate.repo_path.clone());
            entries.push(HarvestEntry {
                id: String::new(),
                repo_name: repo_path
                    .file_name()
                    .map(|name| name.to_string_lossy().to_string())
                    .unwrap_or_else(|| repo_path.display().to_string()),
                repo_path,
                branch: outcome.branch.clone(),
                commit: outcome.commit.clone().unwrap_or_default(),
                mode: item.mode,
                title: item.summary.clone(),
                summary: item.reason.clone(),
                quest: item.quest.clone(),
                provider: outcome.provider,
                short_stat: outcome.short_stat.clone(),
                files_changed: None,
                insertions: None,
                deletions: None,
                tests_added: None,
                tests_passing: None,
                created_at,
                night_of: Some(created_at.with_timezone(&Local).date_naive()),
                status: HarvestEntryStatus::Pending,
                looted_at: None,
                clean_exit: false,
                note: matching_next_step(item, &outcome.report.remaining_work),
            });
        }

        entries
    }
}

fn daemon_heartbeat(snapshot: &mut RuntimeSnapshot, status: RuntimeStatus) {
    let now = Utc::now();
    if snapshot.daemon_started_at.is_none() {
        snapshot.daemon_started_at = Some(now);
    }
    snapshot.updated_at = now;
    snapshot.daemon_heartbeat_at = Some(now);
    snapshot.backend_pid = Some(std::process::id());
    snapshot.backend_health = BackendHealth::Healthy;
    snapshot.status = status;
}

fn record_runtime_event(
    paths: &SideQuestPaths,
    snapshot: &mut RuntimeSnapshot,
    kind: RuntimeEventKind,
    message: String,
    run_title: Option<String>,
) -> Result<()> {
    snapshot.latest_event_message = Some(message.clone());
    snapshot.updated_at = Utc::now();
    append_event(
        paths,
        &RuntimeEvent {
            id: next_request_id(),
            at: Utc::now(),
            kind,
            message,
            run_title,
        },
    )?;
    write_snapshot(paths, snapshot)
}

fn runtime_run_from_outcome(
    active_run: Option<&RuntimeRunState>,
    task_spec: &TaskSpec,
    outcome: &TaskOutcome,
    cutoff_time: chrono::DateTime<Local>,
) -> RuntimeRunState {
    let mut run = active_run.cloned().unwrap_or(RuntimeRunState {
        provider: outcome.provider,
        mode: task_spec.mode,
        repo_path: task_spec.repo_path.display().to_string(),
        title: task_spec.title.clone(),
        quest: task_spec.quest.clone(),
        branch: Some(outcome.branch.clone()),
        started_at: Utc::now(),
        cutoff_time: cutoff_time.to_utc(),
        completed_at: None,
        log_path: Some(outcome.log_path.display().to_string()),
        status: RuntimeRunStatus::Running,
        summary: None,
        commit: None,
        short_stat: None,
        report_found: false,
        clean_exit: false,
        stop_reason: None,
        task_state: RuntimeTaskState::default(),
    });
    run.branch = Some(outcome.branch.clone());
    run.completed_at = Some(Utc::now());
    run.log_path = Some(outcome.log_path.display().to_string());
    run.summary = Some(outcome.summary.clone());
    run.commit = outcome.commit.clone();
    run.short_stat = outcome.short_stat.clone();
    run.report_found = outcome.report_found;
    run.clean_exit = outcome.clean_exit;
    run.stop_reason = outcome
        .stop_reason
        .map(|reason| reason.message().to_string());
    run.task_state = RuntimeTaskState::from_report(&outcome.report);
    run.status = if outcome.stop_reason.is_some() {
        RuntimeRunStatus::Stopped
    } else if outcome.clean_exit && !outcome.report.completed.is_empty() {
        RuntimeRunStatus::Completed
    } else {
        RuntimeRunStatus::Failed
    };
    run
}

impl WorkCandidate {
    fn branch_name(&self) -> String {
        let scope = self
            .quest
            .clone()
            .or_else(|| {
                self.repo_path
                    .file_name()
                    .map(|name| name.to_string_lossy().to_string())
            })
            .unwrap_or_else(|| self.title.clone());
        sidequest_branch_name(self.mode, scope)
    }

    fn to_remaining_work_item(&self) -> RemainingWorkItem {
        RemainingWorkItem {
            mode: self.mode,
            repository: self.repository.clone(),
            quest: self.quest.clone(),
            summary: self.summary.clone(),
            next_step: None,
        }
    }
}

fn matching_next_step(failed: &FailedWorkItem, remaining: &[RemainingWorkItem]) -> Option<String> {
    remaining
        .iter()
        .find(|item| {
            item.mode == failed.mode
                && item.repository == failed.repository
                && item.quest == failed.quest
        })
        .and_then(|item| {
            item.next_step
                .clone()
                .or_else(|| Some(item.summary.clone()))
        })
}

fn should_mark_candidate_complete(candidate: &WorkCandidate, report: &SessionReport) -> bool {
    if candidate.mode != WorkMode::Quest {
        return true;
    }

    let Some(quest_name) = &candidate.quest else {
        return true;
    };

    !report
        .remaining_work
        .iter()
        .any(|item| item.quest.as_deref() == Some(quest_name.as_str()))
}

fn quest_is_complete_for_now(quest_name: &str, report: &SessionReport) -> bool {
    if report.quest_completed != Some(true) {
        return false;
    }

    let has_completed = report
        .completed
        .iter()
        .any(|item| item.quest.as_deref() == Some(quest_name));
    let has_failed = report
        .attempted_but_failed
        .iter()
        .any(|item| item.quest.as_deref() == Some(quest_name));
    let has_remaining = report
        .remaining_work
        .iter()
        .any(|item| item.quest.as_deref() == Some(quest_name));

    has_completed && !has_failed && !has_remaining
}

fn complete_quest(
    paths: &SideQuestPaths,
    config: &mut SideQuestConfig,
    quest_name: &str,
) -> Result<()> {
    let Some(quest) = config
        .quests
        .iter_mut()
        .find(|quest| quest.name == quest_name)
    else {
        return Ok(());
    };

    if quest.status == QuestStatus::Completed {
        return Ok(());
    }

    quest.status = QuestStatus::Completed;
    config.save_with_paths(paths)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{GrindRepoConfig, SleepWindow};
    use crate::platform::{Platform, SleepGuard};
    use crate::runtime::{ControlRequest, ControlRequestKind, append_control_request};
    use chrono::Duration as ChronoDuration;
    use std::fs;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
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

    #[derive(Clone)]
    struct DummyPlatform {
        sleep_guard_calls: Arc<AtomicUsize>,
    }

    struct DummySleepGuard;

    impl SleepGuard for DummySleepGuard {}

    impl Platform for DummyPlatform {
        fn name(&self) -> &'static str {
            "dummy"
        }

        fn read_credential(&self, _service: &str) -> Result<Option<String>> {
            Ok(None)
        }

        fn send_notification(&self, _title: &str, _body: &str) -> Result<()> {
            Ok(())
        }

        fn begin_sleep_prevention(&self, _reason: &str) -> Result<Option<Box<dyn SleepGuard>>> {
            self.sleep_guard_calls.fetch_add(1, Ordering::SeqCst);
            Ok(Some(Box::new(DummySleepGuard)))
        }

        fn install_autostart(
            &self,
            _binary_path: &Path,
            _sidequest_root: &Path,
            _log_dir: &Path,
        ) -> Result<()> {
            Ok(())
        }

        fn uninstall_autostart(&self, _sidequest_root: &Path) -> Result<()> {
            Ok(())
        }

        fn install_shell_hook(&self, _sidequest_root: &Path) -> Result<()> {
            Ok(())
        }

        fn uninstall_shell_hook(&self, _sidequest_root: &Path) -> Result<()> {
            Ok(())
        }
    }

    fn test_paths(root: &Path) -> SideQuestPaths {
        SideQuestPaths::from_root(root)
    }

    #[test]
    fn matches_next_step_for_failed_item() {
        let failed = FailedWorkItem {
            mode: WorkMode::Grind,
            repository: Some("/tmp/repo".to_string()),
            quest: None,
            summary: "Add docs".to_string(),
            reason: "tests failed".to_string(),
        };
        let remaining = vec![RemainingWorkItem {
            mode: WorkMode::Grind,
            repository: Some("/tmp/repo".to_string()),
            quest: None,
            summary: "Add docs".to_string(),
            next_step: Some("Fix failing tests and retry".to_string()),
        }];

        assert_eq!(
            matching_next_step(&failed, &remaining).as_deref(),
            Some("Fix failing tests and retry")
        );
    }

    #[test]
    fn candidate_converts_to_remaining_work_item() {
        let candidate = WorkCandidate {
            id: "build:/tmp/repo:task".to_string(),
            mode: WorkMode::Grind,
            repo_path: PathBuf::from("/tmp/repo"),
            title: "Task".to_string(),
            summary: "Task".to_string(),
            score: 1,
            context: "Context".to_string(),
            commit_message: "commit".to_string(),
            repository: Some("/tmp/repo".to_string()),
            quest: None,
        };

        let remaining = candidate.to_remaining_work_item();
        assert_eq!(remaining.mode, WorkMode::Grind);
        assert_eq!(remaining.repository.as_deref(), Some("/tmp/repo"));
    }

    #[test]
    fn repo_can_accept_run_leaves_stale_session_report_for_launch_cleanup() {
        let (_temp, repo) = init_repo();
        let report_path = report_file_path(&repo);
        fs::create_dir_all(report_path.parent().expect("report dir")).expect("report dir");
        fs::write(
            &report_path,
            r#"{"completed":[],"attempted_but_failed":[],"remaining_work":[]}"#,
        )
        .expect("report");

        let ready = repo_can_accept_sidequest_run(&repo).expect("repo check");

        assert!(ready);
        assert!(report_path.exists());
    }

    #[test]
    fn repo_can_accept_run_rejects_dirty_worktree() {
        let (_temp, repo) = init_repo();
        fs::write(repo.join("README.md"), "# Repo\n\nDirty.\n").expect("write file");

        let ready = repo_can_accept_sidequest_run(&repo).expect("repo check");

        assert!(!ready);
    }

    #[test]
    fn repo_can_accept_run_allows_quoted_sidequest_rename_entries() {
        let (_temp, repo) = init_repo();
        fs::create_dir_all(repo.join(".sidequest")).expect("sidequest dir");
        fs::write(repo.join(".sidequest/session report.json"), "{}").expect("session report");
        run_git(&repo, ["add", ".sidequest/session report.json"]).expect("git add");
        run_git(&repo, ["commit", "-m", "add session report"]).expect("git commit");

        run_git(
            &repo,
            [
                "mv",
                ".sidequest/session report.json",
                ".sidequest/report renamed.json",
            ],
        )
        .expect("git mv");

        let ready = repo_can_accept_sidequest_run(&repo).expect("repo check");
        assert!(ready);
    }

    #[test]
    fn repo_can_accept_run_rejects_quoted_non_sidequest_rename_entries() {
        let (_temp, repo) = init_repo();
        fs::write(repo.join("notes old.txt"), "notes").expect("notes");
        run_git(&repo, ["add", "notes old.txt"]).expect("git add");
        run_git(&repo, ["commit", "-m", "add notes"]).expect("git commit");

        run_git(&repo, ["mv", "notes old.txt", "notes new.txt"]).expect("git mv");

        let ready = repo_can_accept_sidequest_run(&repo).expect("repo check");
        assert!(!ready);
    }

    #[test]
    fn repo_can_accept_run_rejects_rename_into_sidequest_directory() {
        let (_temp, repo) = init_repo();
        fs::write(repo.join("notes old.txt"), "notes").expect("notes");
        run_git(&repo, ["add", "notes old.txt"]).expect("git add");
        run_git(&repo, ["commit", "-m", "add notes"]).expect("git commit");

        fs::create_dir_all(repo.join(".sidequest")).expect("sidequest dir");
        run_git(&repo, ["mv", "notes old.txt", ".sidequest/notes new.txt"]).expect("git mv");

        let ready = repo_can_accept_sidequest_run(&repo).expect("repo check");
        assert!(!ready);
    }

    #[test]
    fn side_quest_with_remaining_work_is_not_marked_complete() {
        let candidate = WorkCandidate {
            id: "side-quest:anydash".to_string(),
            mode: WorkMode::Quest,
            repo_path: PathBuf::from("/tmp/repo"),
            title: "Advance side quest anydash".to_string(),
            summary: "Advance side quest anydash".to_string(),
            score: 1,
            context: "Context".to_string(),
            commit_message: "commit".to_string(),
            repository: Some("/tmp/repo".to_string()),
            quest: Some("anydash".to_string()),
        };
        let report = SessionReport {
            completed: Vec::new(),
            attempted_but_failed: Vec::new(),
            remaining_work: vec![RemainingWorkItem {
                mode: WorkMode::Quest,
                repository: Some("/tmp/repo".to_string()),
                quest: Some("anydash".to_string()),
                summary: "Keep building".to_string(),
                next_step: Some("Hand off to the next provider".to_string()),
            }],
            quest_completed: Some(false),
            budget_estimate_at_exit: None,
        };

        assert!(!should_mark_candidate_complete(&candidate, &report));
    }

    #[test]
    fn quest_completion_requires_no_failures_or_remaining_work() {
        let report = SessionReport {
            completed: vec![CompletedWorkItem {
                mode: WorkMode::Quest,
                repository: Some("/tmp/repo".to_string()),
                quest: Some("anydash".to_string()),
                branch: None,
                summary: "Shipped AnyDash".to_string(),
                files_changed: None,
                tests_added: None,
                tests_passing: None,
                diff_summary: None,
                next_step: None,
            }],
            attempted_but_failed: Vec::new(),
            remaining_work: Vec::new(),
            quest_completed: Some(true),
            budget_estimate_at_exit: None,
        };

        assert!(quest_is_complete_for_now("anydash", &report));
    }

    #[test]
    fn quest_completion_does_not_follow_empty_remaining_work_without_signal() {
        let report = SessionReport {
            completed: vec![CompletedWorkItem {
                mode: WorkMode::Quest,
                repository: Some("/tmp/repo".to_string()),
                quest: Some("anydash".to_string()),
                branch: None,
                summary: "Shipped AnyDash".to_string(),
                files_changed: None,
                tests_added: None,
                tests_passing: None,
                diff_summary: None,
                next_step: None,
            }],
            attempted_but_failed: Vec::new(),
            remaining_work: Vec::new(),
            quest_completed: None,
            budget_estimate_at_exit: None,
        };

        assert!(!quest_is_complete_for_now("anydash", &report));
    }

    #[test]
    fn complete_quest_persists_to_daemon_root_config() {
        let temp = tempdir().expect("temp dir");
        let paths = test_paths(temp.path());
        paths.ensure().expect("ensure");
        let mut config = SideQuestConfig {
            quests: vec![crate::config::QuestConfig {
                name: "anydash".to_string(),
                goal: Some("Ship a thing".to_string()),
                goal_file: None,
                directory: temp.path().join("quests/anydash").display().to_string(),
                status: QuestStatus::Active,
            }],
            ..SideQuestConfig::default()
        };
        config.save_to(&paths.config_file).expect("save config");

        complete_quest(&paths, &mut config, "anydash").expect("complete quest");

        let reloaded = SideQuestConfig::load_from(&paths.config_file).expect("reload config");
        assert_eq!(reloaded.quests[0].status, QuestStatus::Completed);
    }

    #[test]
    fn instance_lock_blocks_concurrent_acquisition() {
        let temp = tempdir().expect("temp dir");
        let platform = DummyPlatform {
            sleep_guard_calls: Arc::new(AtomicUsize::new(0)),
        };
        let daemon = SideQuestDaemon {
            paths: test_paths(temp.path()),
            platform: Box::new(platform),
        };

        let first = daemon
            .try_acquire_instance_lock()
            .expect("first lock")
            .expect("lock should be available");
        assert!(
            daemon
                .try_acquire_instance_lock()
                .expect("second lock")
                .is_none(),
            "second acquisition should be blocked"
        );
        drop(first);
        assert!(
            daemon
                .try_acquire_instance_lock()
                .expect("third lock")
                .is_some(),
            "lock should be available again after release"
        );
    }

    #[test]
    fn sleep_guard_tracks_active_night_window() {
        let temp = tempdir().expect("temp dir");
        let counter = Arc::new(AtomicUsize::new(0));
        let daemon = SideQuestDaemon {
            paths: test_paths(temp.path()),
            platform: Box::new(DummyPlatform {
                sleep_guard_calls: counter.clone(),
            }),
        };
        let now = Local::now();
        let active_window = SleepWindow {
            start: (now - ChronoDuration::minutes(30))
                .format("%H:%M")
                .to_string(),
            end: (now + ChronoDuration::minutes(30))
                .format("%H:%M")
                .to_string(),
        };
        let idle_window = SleepWindow {
            start: (now + ChronoDuration::hours(2)).format("%H:%M").to_string(),
            end: (now + ChronoDuration::hours(3)).format("%H:%M").to_string(),
        };
        let active_config = SideQuestConfig {
            sleep_window: active_window,
            ..SideQuestConfig::default()
        };
        let idle_config = SideQuestConfig {
            sleep_window: idle_window,
            ..SideQuestConfig::default()
        };

        let mut guard: Option<Box<dyn SleepGuard>> = None;
        daemon
            .sync_sleep_guard(&active_config, &mut guard)
            .expect("activate sleep guard");
        assert!(guard.is_some());
        assert_eq!(counter.load(Ordering::SeqCst), 1);

        daemon
            .sync_sleep_guard(&idle_config, &mut guard)
            .expect("release sleep guard");
        assert!(guard.is_none());
    }

    #[test]
    fn wait_loop_stops_when_daemon_stop_request_arrives() {
        let temp = tempdir().expect("temp dir");
        let daemon = SideQuestDaemon {
            paths: test_paths(temp.path()),
            platform: Box::new(DummyPlatform {
                sleep_guard_calls: Arc::new(AtomicUsize::new(0)),
            }),
        };
        append_control_request(
            daemon.paths(),
            &ControlRequest {
                id: 1,
                created_at: Utc::now(),
                kind: ControlRequestKind::StopDaemon,
            },
        )
        .expect("append request");

        let outcome = daemon
            .wait_for_next_tick(StdDuration::from_secs(5))
            .expect("wait outcome");

        assert_eq!(outcome, WaitOutcome::StopDaemonRequested);
    }

    #[test]
    fn build_scan_skips_generated_vendored_and_binary_files() {
        let (_temp, repo) = init_repo();

        fs::create_dir_all(repo.join("node_modules/pkg")).expect("node_modules");
        fs::write(
            repo.join("node_modules/pkg/ignored.rs"),
            "// TODO: should be ignored\n",
        )
        .expect("ignored file");

        fs::create_dir_all(repo.join("target")).expect("target dir");
        let mut large = String::new();
        large.extend(std::iter::repeat_n(
            'a',
            (workspace::MAX_SCAN_FILE_BYTES as usize) + 1024,
        ));
        large.push_str("\n// TODO: should be ignored\n");
        fs::write(repo.join("target/large.rs"), large).expect("large file");

        fs::write(repo.join("binary.bin"), [0u8, b'T', b'O', b'D', b'O']).expect("binary file");

        fs::write(repo.join("src.rs"), "// TODO: should be counted\n").expect("scan file");

        let scan = scan_build_opportunities(&repo).expect("scan");

        assert_eq!(scan.todo_hits, 1);
        assert_eq!(scan.examples.len(), 1);
        assert!(scan.examples[0].contains("src.rs"));
    }

    #[test]
    fn grind_candidate_includes_recent_commit_context() {
        let (_temp, repo) = init_repo();
        fs::write(repo.join("Dockerfile"), "FROM scratch\n").expect("dockerfile");
        fs::create_dir_all(repo.join(".github/workflows")).expect("workflows dir");
        fs::write(
            repo.join(".github/workflows/build.yml"),
            "name: build\non: [push]\n",
        )
        .expect("workflow file");
        run_git(&repo, ["add", "Dockerfile", ".github/workflows/build.yml"]).expect("git add");
        run_git(&repo, ["commit", "-m", "Add required build files"]).expect("git commit");

        let daemon = SideQuestDaemon {
            paths: test_paths(repo.parent().expect("parent")),
            platform: Box::new(DummyPlatform {
                sleep_guard_calls: Arc::new(AtomicUsize::new(0)),
            }),
        };
        let config = SideQuestConfig {
            grind: vec![GrindRepoConfig {
                name: "repo".to_string(),
                path: repo.display().to_string(),
            }],
            ..SideQuestConfig::default()
        };

        let candidates = daemon
            .collect_grind_candidates(&config)
            .expect("collect grind candidates");
        assert_eq!(candidates.len(), 1);
        let candidate = &candidates[0];
        assert_eq!(candidate.mode, WorkMode::Grind);
        assert!(candidate.context.contains("Recent commits"));
        assert!(candidate.context.contains("Add required build files"));
    }

    #[test]
    fn quoted_commit_log_escapes_and_formats_entries() {
        let input = "abc1234 Fix the \"auth\" race\ndef5678 Add tests\n";
        let output = quoted_commit_log(input);

        assert!(output.contains(r#"- `abc1234` "Fix the \"auth\" race""#));
        assert!(output.contains(r#"- `def5678` "Add tests""#));
    }

    #[test]
    fn quoted_commit_log_returns_fallback_for_empty_input() {
        assert!(quoted_commit_log("").contains("none recorded"));
        assert!(quoted_commit_log("  \n  \n").contains("none recorded"));
    }

    #[test]
    fn build_scan_works_when_repo_lives_under_a_skip_directory_name() {
        let temp = tempdir().expect("temp dir");
        // Place the repo under a directory named "vendor", which is in SKIP_SCAN_DIRECTORIES
        let vendor_dir = temp.path().join("vendor").join("myproject");
        fs::create_dir_all(&vendor_dir).expect("vendor dir");
        run_git(&vendor_dir, ["init"]).expect("git init");
        run_git(&vendor_dir, ["config", "user.name", "SideQuest Tester"]).expect("git config");
        run_git(
            &vendor_dir,
            ["config", "user.email", "sidequest@test.local"],
        )
        .expect("git config");
        fs::write(vendor_dir.join("README.md"), "# Repo\n").expect("readme");
        fs::write(vendor_dir.join("lib.rs"), "// TODO: this should be found\n").expect("lib.rs");
        run_git(&vendor_dir, ["add", "."]).expect("git add");
        run_git(&vendor_dir, ["commit", "-m", "Initial commit"]).expect("git commit");

        let scan = scan_build_opportunities(&vendor_dir).expect("scan");

        assert_eq!(
            scan.todo_hits, 1,
            "should find TODO inside a repo whose parent directory is named 'vendor'"
        );
    }
}
