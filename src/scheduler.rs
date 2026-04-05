use anyhow::{Result, anyhow};
use chrono::{DateTime, Duration, Local, LocalResult, NaiveDate, TimeZone};

use crate::config::SideQuestConfig;
use crate::oracle::{ProviderKind, UsageBudget};

pub const SESSION_WINDOW_HOURS: i64 = 5;
pub const CUTOFF_BUFFER_MINUTES: i64 = 30;
pub const DEFAULT_POLL_MINUTES: i64 = 10;
pub const MINIMUM_START_BUDGET: f64 = 0.10;

#[derive(Debug, Clone)]
pub struct ProviderBudget {
    pub provider: ProviderKind,
    pub usage: UsageBudget,
    pub spendable_budget: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionKind {
    RunNow,
    OutsideSleepWindow,
    AfterCutoff,
    LowBudget,
    AwaitingReset,
    NoProviders,
}

#[derive(Debug, Clone)]
pub struct SchedulerDecision {
    pub kind: DecisionKind,
    pub reason: String,
    pub provider: Option<ProviderBudget>,
    pub wake_time: DateTime<Local>,
    pub cutoff_time: DateTime<Local>,
    pub next_check_at: DateTime<Local>,
}

#[derive(Debug, Clone)]
struct SleepWindowBounds {
    current_end: DateTime<Local>,
    next_start: DateTime<Local>,
    in_window: bool,
}

pub fn calculate_spendable_budget(usage: &UsageBudget, safety_margin: f64) -> f64 {
    let session_available = (1.0 - usage.session_utilization) - safety_margin;
    let weekly_available = (1.0 - usage.weekly_utilization) - safety_margin;
    session_available.min(weekly_available).max(0.0)
}

pub fn evaluate(
    now: DateTime<Local>,
    config: &SideQuestConfig,
    usages: &[(ProviderKind, UsageBudget)],
    force_window: bool,
) -> Result<SchedulerDecision> {
    let window = sleep_window_bounds(now, config)?;
    let cutoff_time = window.current_end
        - Duration::hours(SESSION_WINDOW_HOURS)
        - Duration::minutes(CUTOFF_BUFFER_MINUTES);

    if !force_window && !window.in_window {
        return Ok(SchedulerDecision {
            kind: DecisionKind::OutsideSleepWindow,
            reason: format!(
                "outside the questing window; next night watch starts at {}",
                window.next_start.format("%Y-%m-%d %H:%M")
            ),
            provider: None,
            wake_time: window.current_end,
            cutoff_time,
            next_check_at: window.next_start,
        });
    }

    if now >= cutoff_time {
        return Ok(SchedulerDecision {
            kind: DecisionKind::AfterCutoff,
            reason: format!(
                "current time is at or after cutoff {}; preserving the morning session",
                cutoff_time.format("%Y-%m-%d %H:%M")
            ),
            provider: None,
            wake_time: window.current_end,
            cutoff_time,
            next_check_at: window.next_start,
        });
    }

    let budgets: Vec<_> = usages
        .iter()
        .map(|(provider, usage)| ProviderBudget {
            provider: *provider,
            usage: usage.clone(),
            spendable_budget: calculate_spendable_budget(usage, config.safety_margin),
        })
        .collect();

    let ordered_budgets = preferred_provider_order(config, &budgets);
    if let Some(runnable_budget) = ordered_budgets
        .iter()
        .find(|budget| budget.spendable_budget >= MINIMUM_START_BUDGET)
        .cloned()
    {
        return Ok(SchedulerDecision {
            kind: DecisionKind::RunNow,
            reason: format!(
                "quest launch is safe with {:.0}% spendable budget on {}",
                runnable_budget.spendable_budget * 100.0,
                runnable_budget.provider
            ),
            provider: Some(runnable_budget),
            wake_time: window.current_end,
            cutoff_time,
            next_check_at: now + Duration::minutes(DEFAULT_POLL_MINUTES),
        });
    }

    if let Some(preferred_budget) = ordered_budgets.into_iter().next() {
        if let Some(reset_at) = earliest_meaningful_reset_before_cutoff(
            &budgets,
            cutoff_time,
            now,
            config.safety_margin,
        ) {
            return Ok(SchedulerDecision {
                kind: DecisionKind::AwaitingReset,
                reason: format!(
                    "budget is low now, but a provider window recharges at {}",
                    reset_at.format("%Y-%m-%d %H:%M")
                ),
                provider: Some(preferred_budget),
                wake_time: window.current_end,
                cutoff_time,
                next_check_at: reset_at,
            });
        }

        return Ok(SchedulerDecision {
            kind: DecisionKind::LowBudget,
            reason: format!(
                "best available budget is only {:.0}%; SideQuest will wait at camp",
                preferred_budget.spendable_budget * 100.0
            ),
            provider: Some(preferred_budget),
            wake_time: window.current_end,
            cutoff_time,
            next_check_at: (now + Duration::minutes(DEFAULT_POLL_MINUTES)).min(cutoff_time),
        });
    }

    Ok(SchedulerDecision {
        kind: DecisionKind::NoProviders,
        reason: "no enabled providers reported a usable budget".to_string(),
        provider: None,
        wake_time: window.current_end,
        cutoff_time,
        next_check_at: window.next_start,
    })
}

fn preferred_provider_order(
    config: &SideQuestConfig,
    budgets: &[ProviderBudget],
) -> Vec<ProviderBudget> {
    let mut ordered = Vec::new();

    for provider in &config.provider_preference {
        if let Some(budget) = budgets.iter().find(|budget| budget.provider == *provider) {
            ordered.push(budget.clone());
        }
    }

    for budget in budgets {
        if !ordered
            .iter()
            .any(|existing| existing.provider == budget.provider)
        {
            ordered.push(budget.clone());
        }
    }

    ordered
}

fn earliest_meaningful_reset_before_cutoff(
    budgets: &[ProviderBudget],
    cutoff_time: DateTime<Local>,
    now: DateTime<Local>,
    safety_margin: f64,
) -> Option<DateTime<Local>> {
    let mut candidates = Vec::new();

    for budget in budgets {
        let session_available = (1.0 - budget.usage.session_utilization) - safety_margin;
        let weekly_available = (1.0 - budget.usage.weekly_utilization) - safety_margin;
        let session_reset = budget.usage.session_resets_at.with_timezone(&Local);
        let weekly_reset = budget.usage.weekly_resets_at.with_timezone(&Local);

        // A session reset only helps if session is currently the blocking window and
        // weekly headroom would allow a runnable budget after that reset.
        let spendable_after_session_reset = (1.0 - safety_margin).min(weekly_available).max(0.0);
        if session_available < MINIMUM_START_BUDGET
            && spendable_after_session_reset >= MINIMUM_START_BUDGET
            && session_reset > now
            && session_reset < cutoff_time
        {
            candidates.push(session_reset);
        }

        // A weekly reset only helps if weekly is currently the blocking window and
        // session headroom would allow a runnable budget after that reset.
        let spendable_after_weekly_reset = session_available.min(1.0 - safety_margin).max(0.0);
        if weekly_available < MINIMUM_START_BUDGET
            && spendable_after_weekly_reset >= MINIMUM_START_BUDGET
            && weekly_reset > now
            && weekly_reset < cutoff_time
        {
            candidates.push(weekly_reset);
        }
    }

    candidates.into_iter().min()
}

fn sleep_window_bounds(
    now: DateTime<Local>,
    config: &SideQuestConfig,
) -> Result<SleepWindowBounds> {
    let start_time = config.sleep_window.start_time()?;
    let end_time = config.sleep_window.end_time()?;
    let today = now.date_naive();

    if start_time < end_time {
        let today_start = localize(today, start_time)?;
        let today_end = localize(today, end_time)?;
        if now < today_start {
            return Ok(SleepWindowBounds {
                current_end: today_end,
                next_start: today_start,
                in_window: false,
            });
        }

        if now < today_end {
            return Ok(SleepWindowBounds {
                current_end: today_end,
                next_start: localize(today + Duration::days(1), start_time)?,
                in_window: true,
            });
        }

        let next_start = localize(today + Duration::days(1), start_time)?;
        Ok(SleepWindowBounds {
            current_end: localize(today + Duration::days(1), end_time)?,
            next_start,
            in_window: false,
        })
    } else {
        let evening_start = localize(today, start_time)?;
        let morning_end = localize(today, end_time)?;

        if now.time() >= start_time {
            return Ok(SleepWindowBounds {
                current_end: localize(today + Duration::days(1), end_time)?,
                next_start: localize(today + Duration::days(1), start_time)?,
                in_window: true,
            });
        }

        if now.time() < end_time {
            return Ok(SleepWindowBounds {
                current_end: morning_end,
                next_start: evening_start,
                in_window: true,
            });
        }

        Ok(SleepWindowBounds {
            current_end: localize(today + Duration::days(1), end_time)?,
            next_start: evening_start,
            in_window: false,
        })
    }
}

fn localize(day: NaiveDate, time: chrono::NaiveTime) -> Result<DateTime<Local>> {
    let naive = day.and_time(time);
    match Local.from_local_datetime(&naive) {
        LocalResult::Single(value) => Ok(value),
        LocalResult::Ambiguous(first, _) => Ok(first),
        LocalResult::None => Err(anyhow!("local time {} does not exist", naive)),
    }
}

#[cfg(test)]
mod tests {
    use chrono::{FixedOffset, NaiveDate, TimeZone};

    use super::*;
    use crate::config::SideQuestConfig;
    use crate::oracle::UsageBudget;

    fn usage(session_utilization: f64, weekly_utilization: f64) -> UsageBudget {
        let offset = FixedOffset::east_opt(0).expect("offset");
        UsageBudget::new(
            session_utilization,
            offset
                .with_ymd_and_hms(2026, 4, 3, 0, 0, 0)
                .single()
                .expect("session reset")
                .to_utc(),
            weekly_utilization,
            offset
                .with_ymd_and_hms(2026, 4, 7, 0, 0, 0)
                .single()
                .expect("weekly reset")
                .to_utc(),
        )
    }

    fn usage_with_resets(
        session_utilization: f64,
        weekly_utilization: f64,
        session_reset: DateTime<Local>,
        weekly_reset: DateTime<Local>,
    ) -> UsageBudget {
        UsageBudget::new(
            session_utilization,
            session_reset.to_utc(),
            weekly_utilization,
            weekly_reset.to_utc(),
        )
    }

    fn local_datetime(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> DateTime<Local> {
        let naive = NaiveDate::from_ymd_opt(year, month, day)
            .expect("date")
            .and_hms_opt(hour, minute, 0)
            .expect("time");
        Local
            .from_local_datetime(&naive)
            .earliest()
            .expect("local time")
    }

    #[test]
    fn calculates_spendable_budget_from_tightest_window() {
        let spendable = calculate_spendable_budget(&usage(0.25, 0.80), 0.15);
        assert!((spendable - 0.05).abs() < f64::EPSILON);
    }

    #[test]
    fn waits_until_sleep_window_when_daytime() {
        let now = local_datetime(2026, 4, 2, 14, 0);
        let decision = evaluate(
            now,
            &SideQuestConfig::default(),
            &[(ProviderKind::Claude, usage(0.10, 0.10))],
            false,
        )
        .expect("decision");

        assert_eq!(decision.kind, DecisionKind::OutsideSleepWindow);
        assert_eq!(
            decision.next_check_at.time().format("%H:%M").to_string(),
            "23:00"
        );
    }

    #[test]
    fn respects_cutoff_before_wake_time() {
        let now = local_datetime(2026, 4, 3, 1, 45);
        let decision = evaluate(
            now,
            &SideQuestConfig::default(),
            &[(ProviderKind::Claude, usage(0.10, 0.10))],
            false,
        )
        .expect("decision");

        assert_eq!(decision.kind, DecisionKind::AfterCutoff);
        assert_eq!(
            decision.cutoff_time.time().format("%H:%M").to_string(),
            "01:30"
        );
    }

    #[test]
    fn prefers_first_runnable_provider_in_config_order() {
        let now = local_datetime(2026, 4, 2, 23, 0);
        let config = SideQuestConfig::default();
        let decision = evaluate(
            now,
            &config,
            &[
                (ProviderKind::Claude, usage(0.6, 0.2)),
                (ProviderKind::Codex, usage(0.2, 0.2)),
            ],
            false,
        )
        .expect("decision");

        assert_eq!(
            decision.provider.as_ref().map(|provider| provider.provider),
            Some(ProviderKind::Claude)
        );
    }

    #[test]
    fn uses_provider_preference_as_tie_breaker() {
        let now = local_datetime(2026, 4, 2, 23, 0);
        let config = SideQuestConfig::default();
        let decision = evaluate(
            now,
            &config,
            &[
                (ProviderKind::Claude, usage(0.2, 0.2)),
                (ProviderKind::Codex, usage(0.2, 0.2)),
            ],
            false,
        )
        .expect("decision");

        assert_eq!(
            decision.provider.as_ref().map(|provider| provider.provider),
            Some(ProviderKind::Claude)
        );
    }

    #[test]
    fn falls_back_to_next_provider_when_preferred_is_drained() {
        let config = SideQuestConfig::default();
        let now = local_datetime(2026, 4, 2, 23, 0);
        let decision = evaluate(
            now,
            &config,
            &[
                (ProviderKind::Claude, usage(0.90, 0.2)),
                (ProviderKind::Codex, usage(0.20, 0.2)),
            ],
            false,
        )
        .expect("decision");

        assert_eq!(
            decision.provider.as_ref().map(|provider| provider.provider),
            Some(ProviderKind::Codex)
        );
    }

    #[test]
    fn does_not_wait_for_session_reset_when_weekly_is_the_only_limiter() {
        let config = SideQuestConfig::default();
        let now = local_datetime(2026, 4, 2, 23, 0);
        let usage = usage_with_resets(
            0.12,
            0.86,
            now + Duration::minutes(20),
            now + Duration::days(3),
        );
        let decision =
            evaluate(now, &config, &[(ProviderKind::Codex, usage)], false).expect("decision");

        assert_eq!(decision.kind, DecisionKind::LowBudget);
    }

    #[test]
    fn waits_for_session_reset_when_it_can_restore_runnable_budget() {
        let config = SideQuestConfig::default();
        let now = local_datetime(2026, 4, 2, 23, 0);
        let usage = usage_with_resets(
            0.95,
            0.20,
            now + Duration::minutes(20),
            now + Duration::days(3),
        );
        let decision =
            evaluate(now, &config, &[(ProviderKind::Codex, usage)], false).expect("decision");

        assert_eq!(decision.kind, DecisionKind::AwaitingReset);
        assert_eq!(
            decision.next_check_at.time().format("%H:%M").to_string(),
            "23:20"
        );
    }
}
