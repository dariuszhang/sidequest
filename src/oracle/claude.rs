use std::process::Command;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use reqwest::blocking::Client;
use serde::Deserialize;
use serde_json::Value;

use crate::config::{AuthMethod, SideQuestConfig};
use crate::oracle::{
    ProviderKind, ProviderOracle, UsageBudget, find_string, find_value,
    normalize_percentage_utilization, parse_percentage, parse_reset_timestamp,
};
use crate::platform::Platform;

const CLAUDE_SERVICE_NAME: &str = "Claude Code-credentials";
const CLAUDE_USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";

#[derive(Debug, Deserialize)]
struct ClaudeUsageWindow {
    utilization: f64,
    resets_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
struct ClaudeUsageResponse {
    five_hour: ClaudeUsageWindow,
    seven_day: ClaudeUsageWindow,
}

pub struct ClaudeOracle;

impl ClaudeOracle {
    pub fn is_configured(platform: &dyn Platform) -> bool {
        platform
            .read_credential(CLAUDE_SERVICE_NAME)
            .ok()
            .flatten()
            .is_some()
    }

    fn fetch_oauth_usage(platform: &dyn Platform) -> Result<UsageBudget> {
        let raw = platform
            .read_credential(CLAUDE_SERVICE_NAME)?
            .ok_or_else(|| anyhow::anyhow!("Claude Code credentials were not found"))?;
        let access_token = extract_access_token(&raw)?;
        let response = Client::new()
            .get(CLAUDE_USAGE_URL)
            .bearer_auth(access_token)
            .header("anthropic-beta", "oauth-2025-04-20")
            .send()
            .context("failed to query Claude usage endpoint")?
            .error_for_status()
            .context("Claude usage endpoint returned an error")?;
        let usage: ClaudeUsageResponse = response
            .json()
            .context("failed to decode Claude usage response")?;
        Ok(UsageBudget::new(
            normalize_percentage_utilization(usage.five_hour.utilization)?,
            usage.five_hour.resets_at,
            normalize_percentage_utilization(usage.seven_day.utilization)?,
            usage.seven_day.resets_at,
        ))
    }

    fn fetch_cli_usage() -> Result<UsageBudget> {
        let output = Command::new("sh")
            .args(["-lc", "printf '/usage\n/exit\n' | claude"])
            .output()
            .context("failed to launch claude CLI for fallback probe")?;
        if !output.status.success() {
            bail!(
                "claude CLI usage probe failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        parse_cli_usage(&String::from_utf8_lossy(&output.stdout))
    }
}

impl ProviderOracle for ClaudeOracle {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Claude
    }

    fn fetch_usage(
        &self,
        config: &SideQuestConfig,
        platform: &dyn Platform,
    ) -> Result<UsageBudget> {
        if !config.providers.claude.enabled {
            bail!("Claude provider is disabled");
        }

        match config.providers.claude.auth_method {
            AuthMethod::Auto => {
                Self::fetch_oauth_usage(platform).or_else(|_| Self::fetch_cli_usage())
            }
            AuthMethod::Oauth => Self::fetch_oauth_usage(platform),
            AuthMethod::Cli => Self::fetch_cli_usage(),
        }
    }
}

fn extract_access_token(raw: &str) -> Result<String> {
    let json: Value =
        serde_json::from_str(raw).context("failed to parse Claude credentials JSON")?;
    find_string(&json, &["accessToken", "access_token"])
        .or_else(|| {
            find_value(&json, &["claudeAiOauth"]).and_then(|oauth| {
                oauth
                    .get("accessToken")
                    .and_then(|value| value.as_str())
                    .map(ToOwned::to_owned)
            })
        })
        .ok_or_else(|| anyhow::anyhow!("Claude credentials did not contain an access token"))
}

fn parse_cli_usage(output: &str) -> Result<UsageBudget> {
    // Keep this parsing isolated here because CLI output can drift over time.
    // The fallback expects lines shaped like:
    // `5h window: 35% used, resets at 2026-04-03T04:00:00Z`
    // `7d window: 62% used, resets at 2026-04-07T00:00:00Z`
    let session_line = output
        .lines()
        .find(|line| line.contains("5h") || line.contains("5-hour"))
        .ok_or_else(|| anyhow::anyhow!("unable to find Claude 5h usage line"))?;
    let weekly_line = output
        .lines()
        .find(|line| line.contains("7d") || line.contains("7-day") || line.contains("week"))
        .ok_or_else(|| anyhow::anyhow!("unable to find Claude weekly usage line"))?;

    let session_utilization = parse_percentage(session_line)?;
    let weekly_utilization = parse_percentage(weekly_line)?;
    let session_resets_at = parse_reset_timestamp(session_line)?;
    let weekly_resets_at = parse_reset_timestamp(weekly_line)?;

    Ok(UsageBudget::new(
        session_utilization,
        session_resets_at,
        weekly_utilization,
        weekly_resets_at,
    ))
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_nested_access_token() {
        let raw = r#"{"claudeAiOauth":{"accessToken":"token-123"}}"#;
        assert_eq!(extract_access_token(raw).expect("token"), "token-123");
    }

    #[test]
    fn parses_cli_usage_output() {
        let output = "\
5h window: 35% used, resets at 2026-04-03T04:00:00Z\n\
7d window: 62% used, resets at 2026-04-07T00:00:00Z\n";
        let usage = parse_cli_usage(output).expect("usage");
        assert!((usage.session_utilization - 0.35).abs() < f64::EPSILON);
        assert!((usage.weekly_utilization - 0.62).abs() < f64::EPSILON);
    }
}
