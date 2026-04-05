pub mod claude;
pub mod codex;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::SideQuestConfig;
use crate::platform::Platform;

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ValueEnum, Ord, PartialOrd,
)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    Claude,
    Codex,
}

impl std::fmt::Display for ProviderKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let label = match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        };
        write!(f, "{label}")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageBudget {
    pub session_utilization: f64,
    pub session_resets_at: DateTime<Utc>,
    pub weekly_utilization: f64,
    pub weekly_resets_at: DateTime<Utc>,
}

impl UsageBudget {
    pub fn new(
        session_utilization: f64,
        session_resets_at: DateTime<Utc>,
        weekly_utilization: f64,
        weekly_resets_at: DateTime<Utc>,
    ) -> Self {
        Self {
            session_utilization,
            session_resets_at,
            weekly_utilization,
            weekly_resets_at,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderFailure {
    pub provider: ProviderKind,
    pub message: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OracleSnapshot {
    pub budgets: Vec<(ProviderKind, UsageBudget)>,
    pub failures: Vec<ProviderFailure>,
}

pub trait ProviderOracle {
    fn kind(&self) -> ProviderKind;
    fn fetch_usage(&self, config: &SideQuestConfig, platform: &dyn Platform)
    -> Result<UsageBudget>;
}

pub struct OracleService<'a> {
    platform: &'a dyn Platform,
}

impl<'a> OracleService<'a> {
    pub fn new(platform: &'a dyn Platform) -> Self {
        Self { platform }
    }

    pub fn snapshot(&self, config: &SideQuestConfig) -> OracleSnapshot {
        let providers: Vec<Box<dyn ProviderOracle>> =
            vec![Box::new(claude::ClaudeOracle), Box::new(codex::CodexOracle)];

        let mut snapshot = OracleSnapshot::default();
        for provider in providers {
            match provider.fetch_usage(config, self.platform) {
                Ok(usage) => snapshot.budgets.push((provider.kind(), usage)),
                Err(error) => snapshot.failures.push(ProviderFailure {
                    provider: provider.kind(),
                    message: error.to_string(),
                }),
            }
        }
        snapshot
    }

    pub fn detect_available_providers(&self) -> Vec<ProviderKind> {
        let mut detected = Vec::new();
        if claude::ClaudeOracle::is_configured(self.platform) {
            detected.push(ProviderKind::Claude);
        }
        if codex::CodexOracle::is_configured() {
            detected.push(ProviderKind::Codex);
        }
        detected
    }
}

pub(crate) fn find_value<'a>(value: &'a Value, candidates: &[&str]) -> Option<&'a Value> {
    match value {
        Value::Object(map) => {
            for candidate in candidates {
                if let Some(found) = map.get(*candidate) {
                    return Some(found);
                }
            }
            for nested in map.values() {
                if let Some(found) = find_value(nested, candidates) {
                    return Some(found);
                }
            }
            None
        }
        Value::Array(items) => items.iter().find_map(|item| find_value(item, candidates)),
        _ => None,
    }
}

pub(crate) fn find_object<'a>(value: &'a Value, candidates: &[&str]) -> Option<&'a Value> {
    let found = find_value(value, candidates)?;
    found.as_object().map(|_| found)
}

pub(crate) fn find_f64(value: &Value, candidates: &[&str]) -> Option<f64> {
    let found = find_value(value, candidates)?;
    match found {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.parse().ok(),
        _ => None,
    }
}

pub(crate) fn find_string(value: &Value, candidates: &[&str]) -> Option<String> {
    find_value(value, candidates).and_then(|found| found.as_str().map(ToOwned::to_owned))
}

pub(crate) fn normalize_utilization(value: f64) -> Result<f64> {
    if (0.0..=1.0).contains(&value) {
        return Ok(value);
    }
    if (1.0..=100.0).contains(&value) {
        return Ok(value / 100.0);
    }
    Err(anyhow::anyhow!(
        "utilization value {value} is outside the expected range"
    ))
}

pub(crate) fn normalize_percentage_utilization(value: f64) -> Result<f64> {
    if !(0.0..=100.0).contains(&value) {
        return Err(anyhow::anyhow!(
            "percentage utilization value {value} is outside the expected range"
        ));
    }
    Ok(value / 100.0)
}

pub(crate) fn parse_percentage(line: &str) -> Result<f64> {
    let token = line
        .split_whitespace()
        .find(|part| part.ends_with('%'))
        .ok_or_else(|| anyhow::anyhow!("unable to find percentage in `{line}`"))?;
    let percentage = token
        .trim_end_matches('%')
        .trim()
        .parse::<f64>()
        .with_context(|| format!("failed to parse percentage from `{token}`"))?;
    normalize_percentage_utilization(percentage)
}

pub(crate) fn parse_reset_timestamp(line: &str) -> Result<DateTime<Utc>> {
    let timestamp = line
        .split("resets at ")
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("unable to find reset timestamp in `{line}`"))?
        .trim();
    Ok(DateTime::parse_from_rfc3339(timestamp)
        .with_context(|| format!("failed to parse reset timestamp `{timestamp}`"))?
        .to_utc())
}

#[cfg(test)]
mod tests {
    use super::{normalize_percentage_utilization, normalize_utilization};

    #[test]
    fn normalizes_fraction_and_percentage_formats() {
        assert!((normalize_utilization(0.42).expect("fraction") - 0.42).abs() < f64::EPSILON);
        assert!((normalize_utilization(96.0).expect("percentage") - 0.96).abs() < f64::EPSILON);
        assert!(normalize_utilization(101.0).is_err());
    }

    #[test]
    fn normalizes_percentage_values_with_low_integer_usage() {
        assert!(
            (normalize_percentage_utilization(1.0).expect("one percent") - 0.01).abs()
                < f64::EPSILON
        );
        assert!(
            (normalize_percentage_utilization(51.0).expect("fifty one percent") - 0.51).abs()
                < f64::EPSILON
        );
        assert!(normalize_percentage_utilization(101.0).is_err());
    }
}
