use std::collections::HashSet;
use std::env;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::Duration as StdDuration;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Duration, Local, Utc};

use crate::config::{SideQuestConfig, SideQuestPaths, WorkMode};
use crate::harvester::{GitRepoSession, sidequest_branch_name};
use crate::oracle::{OracleService, ProviderKind, UsageBudget};
use crate::scheduler::{MINIMUM_START_BUDGET, calculate_spendable_budget};
use crate::state::{
    CompletedWorkItem, FailedWorkItem, RemainingWorkItem, SessionReport, clear_session_report,
    ensure_report_dir, report_file_path, take_session_report,
};

const WATCHDOG_POLL_SECONDS: u64 = 120;
const CUTOFF_GUARD_MINUTES: i64 = 5;

#[derive(Debug, Clone)]
pub struct TaskSpec {
    pub mode: WorkMode,
    pub repo_path: PathBuf,
    pub title: String,
    pub quest: Option<String>,
    pub prompt: String,
    pub commit_message: String,
}

#[derive(Debug, Clone)]
pub struct TaskOutcome {
    pub provider: ProviderKind,
    pub branch: String,
    pub commit: Option<String>,
    pub log_path: PathBuf,
    pub short_stat: Option<String>,
    pub summary: String,
    pub report: SessionReport,
    pub report_found: bool,
    pub shutdown_state: ShutdownState,
    pub stop_reason: Option<StopReason>,
    pub clean_exit: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownState {
    Completed,
    GracefulTimeout,
    ForcedTimeout,
}

impl ShutdownState {
    fn timed_out(self) -> bool {
        !matches!(self, Self::Completed)
    }

    fn forced(self) -> bool {
        matches!(self, Self::ForcedTimeout)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    MorningProtection,
    StopRequested,
}

impl StopReason {
    pub fn message(self) -> &'static str {
        match self {
            Self::MorningProtection => "SideQuest stopped the run to protect the morning session",
            Self::StopRequested => "SideQuest stopped the run because the user requested a stop",
        }
    }
}

#[derive(Debug, Clone)]
pub struct AgentSpawner {
    paths: SideQuestPaths,
}

pub trait TaskObserver {
    fn on_branch_created(&mut self, _branch: &str) {}
    fn on_agent_spawned(&mut self, _log_path: &Path) {}
    fn should_stop(&mut self) -> bool {
        false
    }
}

pub struct NoopTaskObserver;

impl TaskObserver for NoopTaskObserver {}

struct ExecutionRequest<'a> {
    provider: ProviderKind,
    cutoff_time: DateTime<Local>,
    task: &'a TaskSpec,
    session: &'a GitRepoSession,
    branch: &'a str,
}

impl AgentSpawner {
    pub fn new(paths: SideQuestPaths) -> Self {
        Self { paths }
    }

    pub fn run_task(
        &self,
        config: &SideQuestConfig,
        oracle: &OracleService<'_>,
        provider: ProviderKind,
        cutoff_time: DateTime<Local>,
        task: &TaskSpec,
        observer: &mut dyn TaskObserver,
    ) -> Result<TaskOutcome> {
        self.paths.ensure()?;
        if !task.repo_path.join(".git").exists() && task.mode == WorkMode::Quest {
            GitRepoSession::initialize(&task.repo_path, &task.title)?;
        }
        clear_session_report(&task.repo_path)?;

        let session = GitRepoSession::open(&task.repo_path)?;
        if session.has_changes()? {
            bail!(
                "{} has uncommitted changes; SideQuest will not run in a dirty repository",
                task.repo_path.display()
            );
        }
        let branch_scope = task
            .quest
            .clone()
            .or_else(|| {
                task.repo_path
                    .file_name()
                    .map(|name| name.to_string_lossy().to_string())
            })
            .unwrap_or_else(|| task.title.clone());
        let branch = sidequest_branch_name(task.mode, &branch_scope);
        let _branch_action = session.checkout_or_create_sidequest_branch(&branch)?;
        observer.on_branch_created(&branch);

        let result = self.execute_on_branch(
            config,
            oracle,
            ExecutionRequest {
                provider,
                cutoff_time,
                task,
                session: &session,
                branch: &branch,
            },
            observer,
        );
        let restore_result = match &result {
            Ok(_) => session.restore(),
            Err(_) => match session.cleanup_before_restore() {
                Ok(()) => session.restore(),
                Err(error) => Err(error.context(
                    "task failed, and SideQuest could not clean up the SideQuest branch before restoring the original checkout",
                )),
            },
        };
        match (result, restore_result) {
            (Ok(outcome), Ok(())) => Ok(outcome),
            (Err(error), Ok(())) => Err(error),
            (Ok(_), Err(restore_error)) => Err(restore_error
                .context("task finished, but SideQuest failed to restore the original branch")),
            (Err(error), Err(restore_error)) => Err(error.context(format!(
                "task also failed to restore the original branch: {restore_error}"
            ))),
        }
    }

    fn execute_on_branch(
        &self,
        config: &SideQuestConfig,
        oracle: &OracleService<'_>,
        request: ExecutionRequest<'_>,
        observer: &mut dyn TaskObserver,
    ) -> Result<TaskOutcome> {
        let agent_logs = self.paths.logs_dir.join("agents");
        fs::create_dir_all(&agent_logs)
            .with_context(|| format!("failed to create {}", agent_logs.display()))?;
        ensure_report_dir(&request.task.repo_path)?;
        let report_path = report_file_path(&request.task.repo_path);
        if report_path.exists() {
            fs::remove_file(&report_path)
                .with_context(|| format!("failed to clear {}", report_path.display()))?;
        }
        let log_path = agent_logs.join(format!(
            "{}-{}.log",
            request.branch.replace('/', "_"),
            Local::now().timestamp()
        ));
        let log_file = File::create(&log_path)
            .with_context(|| format!("failed to create {}", log_path.display()))?;
        let log_file_err = log_file
            .try_clone()
            .with_context(|| format!("failed to clone {}", log_path.display()))?;

        let mut child = build_agent_command(
            request.provider,
            &request.task.repo_path,
            &request.task.prompt,
        )?
        .stdout(Stdio::from(log_file))
        .stderr(Stdio::from(log_file_err))
        .stdin(Stdio::null())
        .spawn()
        .with_context(|| format!("failed to spawn {} agent", request.provider))?;
        observer.on_agent_spawned(&log_path);

        let shutdown = monitor_child(
            &mut child,
            request.cutoff_time,
            request.provider,
            config,
            oracle,
            observer,
        )?;
        let status = child
            .wait()
            .context("failed while waiting for agent process to exit")?;
        let clean_exit = status.success() && !shutdown.state.forced();
        let parsed_report = take_session_report(&request.task.repo_path)?;
        let report_found = parsed_report.is_some();

        let commit = if clean_exit {
            let commit_message = sidequest_commit_message(request.task, request.provider);
            request.session.commit_all(&commit_message)?
        } else {
            None
        };
        let short_stat = if commit.is_some() {
            request.session.short_stat("HEAD^..HEAD")?
        } else {
            None
        };
        let report = parsed_report.unwrap_or_else(|| {
            synthesize_report(
                request.task,
                request.branch,
                commit.as_deref(),
                short_stat.as_deref(),
                shutdown.state,
                shutdown.reason,
                clean_exit,
            )
        });
        let summary = summarize_outcome(
            request.branch,
            &commit,
            shutdown.state,
            shutdown.reason,
            clean_exit,
            report_found,
        );

        Ok(TaskOutcome {
            provider: request.provider,
            branch: request.branch.to_string(),
            commit,
            log_path,
            short_stat,
            summary,
            report,
            report_found,
            shutdown_state: shutdown.state,
            stop_reason: shutdown.reason,
            clean_exit,
        })
    }
}

fn sidequest_commit_message(task: &TaskSpec, provider: ProviderKind) -> String {
    let subject = format!("[sidequest/{}] {}", task.mode, task.title.trim());
    let body = task.commit_message.trim();
    let body = if body.is_empty() {
        format!("SideQuest completed: {}", task.title.trim())
    } else {
        body.to_string()
    };

    format!(
        "{subject}\n\n{body}\n\nSideQuest-Mode: {}\nSideQuest-Quest: {}\nSideQuest-Provider: {}\nSideQuest-Timestamp: {}\n",
        task.mode,
        task.quest.clone().unwrap_or_default(),
        provider,
        Utc::now().to_rfc3339()
    )
}

fn build_agent_command(provider: ProviderKind, repo_path: &Path, prompt: &str) -> Result<Command> {
    let executable = resolve_provider_executable(provider)?;
    Ok(build_agent_command_for_executable(
        provider,
        &executable,
        repo_path,
        prompt,
    ))
}

fn build_agent_command_for_executable(
    provider: ProviderKind,
    executable: &Path,
    repo_path: &Path,
    prompt: &str,
) -> Command {
    let mut command = match provider {
        ProviderKind::Claude => {
            let mut cmd = Command::new(executable);
            cmd.args([
                "--print",
                "--output-format",
                "json",
                "--dangerously-skip-permissions",
                "--permission-mode",
                "bypassPermissions",
            ])
            .arg(prompt);
            cmd
        }
        ProviderKind::Codex => {
            let mut cmd = Command::new(executable);
            cmd.args(["exec", "--dangerously-bypass-approvals-and-sandbox", "-C"])
                .arg(repo_path)
                .arg(prompt);
            cmd
        }
    };
    command.current_dir(repo_path);
    command
}

fn resolve_provider_executable(provider: ProviderKind) -> Result<PathBuf> {
    let binary_name = provider_binary_name(provider);
    for candidate in candidate_executable_paths(binary_name, env::var_os("PATH"), dirs::home_dir())
    {
        if candidate.is_file() {
            return Ok(candidate);
        }
    }

    bail!(
        "could not find `{}` executable; searched PATH plus standard install locations",
        binary_name
    )
}

fn provider_binary_name(provider: ProviderKind) -> &'static str {
    match provider {
        ProviderKind::Claude => "claude",
        ProviderKind::Codex => "codex",
    }
}

fn candidate_executable_paths(
    binary_name: &str,
    path_var: Option<std::ffi::OsString>,
    home_dir: Option<PathBuf>,
) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    let mut seen = HashSet::new();

    let mut push_candidate = |path: PathBuf| {
        if seen.insert(path.clone()) {
            candidates.push(path);
        }
    };

    if let Some(path_var) = path_var {
        for dir in env::split_paths(&path_var) {
            if !dir.as_os_str().is_empty() {
                push_candidate(dir.join(binary_name));
            }
        }
    }

    if let Some(home_dir) = home_dir {
        push_candidate(home_dir.join(".local").join("bin").join(binary_name));
        push_candidate(home_dir.join(".cargo").join("bin").join(binary_name));
    }

    for dir in [
        "/opt/homebrew/bin",
        "/usr/local/bin",
        "/usr/bin",
        "/bin",
        "/usr/sbin",
        "/sbin",
    ] {
        push_candidate(PathBuf::from(dir).join(binary_name));
    }

    candidates
}

fn monitor_child(
    child: &mut Child,
    cutoff_time: DateTime<Local>,
    provider: ProviderKind,
    config: &SideQuestConfig,
    oracle: &OracleService<'_>,
    observer: &mut dyn TaskObserver,
) -> Result<ShutdownResult> {
    let mut last_known_good_budget: Option<UsageBudget> = None;
    loop {
        if child
            .try_wait()
            .context("failed to poll child process")?
            .is_some()
        {
            return Ok(ShutdownResult {
                state: ShutdownState::Completed,
                reason: None,
            });
        }

        if observer.should_stop() {
            return terminate_process(child, Some(StopReason::StopRequested));
        }

        if Local::now() + Duration::minutes(CUTOFF_GUARD_MINUTES) >= cutoff_time {
            return terminate_process(child, Some(StopReason::MorningProtection));
        }

        let snapshot = oracle.snapshot(config);
        if let Some((usage, budget)) = resolved_spendable_budget(
            provider,
            &snapshot.budgets,
            last_known_good_budget.as_ref(),
            config.safety_margin,
        ) {
            last_known_good_budget = Some(usage);
            if budget < MINIMUM_START_BUDGET {
                return terminate_process(child, Some(StopReason::MorningProtection));
            }
        }

        thread::sleep(StdDuration::from_secs(WATCHDOG_POLL_SECONDS));
    }
}

fn resolved_spendable_budget(
    provider: ProviderKind,
    budgets: &[(ProviderKind, UsageBudget)],
    last_known_good: Option<&UsageBudget>,
    safety_margin: f64,
) -> Option<(UsageBudget, f64)> {
    budgets
        .iter()
        .find(|(kind, _)| *kind == provider)
        .map(|(_, usage)| usage)
        .or(last_known_good)
        .map(|usage| {
            (
                usage.clone(),
                calculate_spendable_budget(usage, safety_margin),
            )
        })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ShutdownResult {
    state: ShutdownState,
    reason: Option<StopReason>,
}

fn terminate_process(child: &mut Child, reason: Option<StopReason>) -> Result<ShutdownResult> {
    #[cfg(unix)]
    {
        let _ = Command::new("kill")
            .args(["-TERM", &child.id().to_string()])
            .status();
        thread::sleep(StdDuration::from_secs(60));
    }
    if child.try_wait()?.is_none() {
        child.kill().context("failed to stop child process")?;
        return Ok(ShutdownResult {
            state: ShutdownState::ForcedTimeout,
            reason,
        });
    }
    Ok(ShutdownResult {
        state: ShutdownState::GracefulTimeout,
        reason,
    })
}

fn synthesize_report(
    task: &TaskSpec,
    branch: &str,
    commit: Option<&str>,
    short_stat: Option<&str>,
    shutdown_state: ShutdownState,
    stop_reason: Option<StopReason>,
    clean_exit: bool,
) -> SessionReport {
    let repository = Some(task.repo_path.display().to_string());
    let quest = task.quest.clone();
    let summary = if commit.is_some() {
        format!("Preserved work for {}", task.title)
    } else {
        task.title.clone()
    };

    if clean_exit && commit.is_some() {
        return SessionReport {
            completed: vec![CompletedWorkItem {
                mode: task.mode,
                repository,
                quest,
                branch: Some(branch.to_string()),
                summary,
                files_changed: None,
                tests_added: None,
                tests_passing: None,
                diff_summary: short_stat.map(ToOwned::to_owned),
                next_step: None,
            }],
            attempted_but_failed: Vec::new(),
            remaining_work: Vec::new(),
            quest_completed: None,
            budget_estimate_at_exit: None,
        };
    }

    let failure_reason = if shutdown_state.timed_out() {
        stop_reason
            .map(|reason| reason.message().to_string())
            .unwrap_or_else(|| StopReason::MorningProtection.message().to_string())
    } else if !clean_exit {
        "The agent exited without completing the task".to_string()
    } else {
        "The agent exited without producing a structured session report".to_string()
    };

    SessionReport {
        completed: Vec::new(),
        attempted_but_failed: vec![FailedWorkItem {
            mode: task.mode,
            repository: repository.clone(),
            quest: quest.clone(),
            summary: task.title.clone(),
            reason: failure_reason,
        }],
        remaining_work: vec![RemainingWorkItem {
            mode: task.mode,
            repository,
            quest,
            summary: task.title.clone(),
            next_step: Some("Re-run this task with fresh budget".to_string()),
        }],
        quest_completed: None,
        budget_estimate_at_exit: None,
    }
}

fn summarize_outcome(
    branch: &str,
    commit: &Option<String>,
    shutdown_state: ShutdownState,
    stop_reason: Option<StopReason>,
    clean_exit: bool,
    report_found: bool,
) -> String {
    if commit.is_some() && clean_exit {
        if report_found {
            return format!("completed on branch `{branch}` and committed cleanly");
        }
        return format!("completed on branch `{branch}` with a synthesized session report");
    }
    if shutdown_state.timed_out() {
        let reason = stop_reason
            .map(|reason| match reason {
                StopReason::MorningProtection => "near cutoff",
                StopReason::StopRequested => "after a user stop request",
            })
            .unwrap_or("near cutoff");
        if shutdown_state.forced() {
            return format!(
                "stopped {reason} on branch `{branch}` after a forced shutdown; preserved state for later review"
            );
        }
        return format!(
            "stopped {reason} on branch `{branch}` after a graceful shutdown; preserved state for later review"
        );
    }
    if !clean_exit {
        return format!(
            "agent exited early on branch `{branch}`; preserved any changes for later review"
        );
    }
    "agent exited cleanly but did not produce committed changes".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::ffi::OsString;

    fn task_spec(mode: WorkMode) -> TaskSpec {
        TaskSpec {
            mode,
            repo_path: PathBuf::from("/tmp/repo"),
            title: "Example task".to_string(),
            quest: (mode == WorkMode::Quest).then(|| "example-quest".to_string()),
            prompt: "prompt".to_string(),
            commit_message: "commit".to_string(),
        }
    }

    #[test]
    fn synthesizes_completed_report_for_clean_exit_with_commit() {
        let report = synthesize_report(
            &task_spec(WorkMode::Grind),
            "sidequest/build/example-1",
            Some("abc123"),
            Some("1 file changed"),
            ShutdownState::Completed,
            None,
            true,
        );

        assert_eq!(report.completed.len(), 1);
        assert!(report.attempted_but_failed.is_empty());
        assert!(report.remaining_work.is_empty());
    }

    #[test]
    fn synthesizes_remaining_work_for_interrupted_run() {
        let report = synthesize_report(
            &task_spec(WorkMode::Grind),
            "sidequest/review/example-1",
            None,
            None,
            ShutdownState::ForcedTimeout,
            Some(StopReason::MorningProtection),
            false,
        );

        assert!(report.completed.is_empty());
        assert_eq!(report.attempted_but_failed.len(), 1);
        assert_eq!(report.remaining_work.len(), 1);
        assert_eq!(report.remaining_work[0].mode, WorkMode::Grind);
    }

    #[test]
    fn resolved_spendable_budget_uses_last_known_good_budget() {
        let now = chrono::Utc
            .with_ymd_and_hms(2026, 4, 3, 10, 0, 0)
            .single()
            .expect("timestamp");
        let budget = UsageBudget::new(0.25, now, 0.25, now);

        let resolved = resolved_spendable_budget(ProviderKind::Claude, &[], Some(&budget), 0.0)
            .expect("budget");

        assert!((resolved.1 - 0.75).abs() < f64::EPSILON);
    }

    #[test]
    fn resolved_spendable_budget_returns_none_when_budget_is_unknown() {
        let resolved = resolved_spendable_budget(ProviderKind::Claude, &[], None, 0.0);
        assert!(resolved.is_none());
    }

    #[test]
    fn candidate_executable_paths_prioritize_path_then_standard_locations() {
        let home = PathBuf::from("/Users/tester");
        let paths = candidate_executable_paths(
            "claude",
            Some(OsString::from("/tmp/bin:/opt/homebrew/bin")),
            Some(home),
        );

        assert_eq!(paths[0], PathBuf::from("/tmp/bin/claude"));
        assert_eq!(paths[1], PathBuf::from("/opt/homebrew/bin/claude"));
        assert!(paths.contains(&PathBuf::from("/Users/tester/.local/bin/claude")));
        assert!(paths.contains(&PathBuf::from("/Users/tester/.cargo/bin/claude")));
        assert_eq!(
            paths
                .iter()
                .filter(|path| path.as_path() == Path::new("/opt/homebrew/bin/claude"))
                .count(),
            1
        );
    }

    #[test]
    fn builds_supported_claude_command() {
        let command = build_agent_command_for_executable(
            ProviderKind::Claude,
            Path::new("/tmp/claude"),
            Path::new("/tmp/repo"),
            "hello world",
        );

        let args: Vec<_> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().to_string())
            .collect();

        assert_eq!(
            args,
            vec![
                "--print".to_string(),
                "--output-format".to_string(),
                "json".to_string(),
                "--dangerously-skip-permissions".to_string(),
                "--permission-mode".to_string(),
                "bypassPermissions".to_string(),
                "hello world".to_string(),
            ]
        );
    }

    #[test]
    fn builds_supported_codex_command() {
        let command =
            build_agent_command(ProviderKind::Codex, Path::new("/tmp/repo"), "hello world")
                .expect("command");

        let args: Vec<_> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().to_string())
            .collect();

        assert_eq!(
            args,
            vec![
                "exec".to_string(),
                "--dangerously-bypass-approvals-and-sandbox".to_string(),
                "-C".to_string(),
                "/tmp/repo".to_string(),
                "hello world".to_string(),
            ]
        );
    }

    #[test]
    fn commit_message_contains_sidequest_trailers() {
        let task = task_spec(WorkMode::Grind);
        let message = sidequest_commit_message(&task, ProviderKind::Codex);

        assert!(message.contains("[sidequest/grind] Example task"));
        assert!(message.contains("SideQuest-Mode: grind"));
        assert!(message.contains("SideQuest-Quest: "));
        assert!(message.contains("SideQuest-Provider: codex"));
        assert!(message.contains("SideQuest-Timestamp: "));
    }
}
