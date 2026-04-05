use std::env;
use std::fs;
use std::io::{self, BufRead, BufReader, IsTerminal, Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio};

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, TimeZone, Utc};
use reqwest::blocking::Client;
use serde_json::{Value, json};

use crate::config::{AuthMethod, SideQuestConfig};
use crate::oracle::{
    ProviderKind, ProviderOracle, UsageBudget, find_f64, find_object, find_string,
    normalize_percentage_utilization, normalize_utilization, parse_percentage,
    parse_reset_timestamp,
};
use crate::platform::Platform;

const CODEX_USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";

pub struct CodexOracle;

impl CodexOracle {
    pub fn is_configured() -> bool {
        auth_file_path().map(|path| path.exists()).unwrap_or(false)
            || cli_reports_logged_in().unwrap_or(false)
    }

    fn fetch_oauth_usage() -> Result<UsageBudget> {
        let auth_path = auth_file_path()?;
        let raw = fs::read_to_string(&auth_path)
            .with_context(|| format!("failed to read {}", auth_path.display()))?;
        let json: Value = serde_json::from_str(&raw).context("failed to parse Codex auth JSON")?;
        let access_token = find_string(&json, &["access_token", "accessToken"])
            .ok_or_else(|| anyhow::anyhow!("Codex auth.json did not include an access token"))?;

        let response = Client::new()
            .get(CODEX_USAGE_URL)
            .bearer_auth(access_token)
            .send()
            .context("failed to query Codex usage endpoint")?
            .error_for_status()
            .context("Codex usage endpoint returned an error")?;
        let value: Value = response
            .json()
            .context("failed to decode Codex usage response")?;
        normalize_usage_response(&value)
    }

    fn fetch_cli_usage() -> Result<UsageBudget> {
        Self::fetch_app_server_usage().or_else(|rpc_error| {
            Self::fetch_pty_cli_usage().map_err(|pty_error| {
                anyhow!(
                    "Codex CLI usage probes failed: app-server RPC: {rpc_error}; PTY fallback: {pty_error}"
                )
            })
        })
    }

    fn fetch_app_server_usage() -> Result<UsageBudget> {
        let mut client = CodexAppServerClient::spawn()?;
        client.initialize()?;
        let response = client.request("account/rateLimits/read", Value::Null)?;
        normalize_rpc_usage_response(&response)
    }

    fn fetch_pty_cli_usage() -> Result<UsageBudget> {
        if !io::stdin().is_terminal() || !io::stdout().is_terminal() || !io::stderr().is_terminal()
        {
            bail!("headless environment: Codex PTY fallback requires an interactive terminal");
        }

        let term = env::var("TERM").unwrap_or_default();
        if term.trim().is_empty() || term == "dumb" {
            bail!(
                "Codex PTY fallback requires a supported terminal (TERM cannot be empty or `dumb`)"
            );
        }

        let output = run_pty_status_probe(&term)?;
        parse_cli_usage(&output)
    }
}

impl ProviderOracle for CodexOracle {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Codex
    }

    fn fetch_usage(
        &self,
        config: &SideQuestConfig,
        _platform: &dyn Platform,
    ) -> Result<UsageBudget> {
        if !config.providers.codex.enabled {
            bail!("Codex provider is disabled");
        }

        match config.providers.codex.auth_method {
            AuthMethod::Auto => Self::fetch_oauth_usage().or_else(|_| Self::fetch_cli_usage()),
            AuthMethod::Oauth => Self::fetch_oauth_usage(),
            AuthMethod::Cli => Self::fetch_cli_usage(),
        }
    }
}

fn auth_file_path() -> Result<PathBuf> {
    if let Some(home) = env::var_os("CODEX_HOME") {
        return Ok(PathBuf::from(home).join("auth.json"));
    }

    let home =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("unable to locate home directory"))?;
    Ok(home.join(".codex").join("auth.json"))
}

fn cli_reports_logged_in() -> Result<bool> {
    let output = Command::new("codex")
        .args(["login", "status"])
        .output()
        .context("failed to run `codex login status`")?;
    if !output.status.success() {
        return Ok(false);
    }

    Ok(String::from_utf8_lossy(&output.stdout)
        .to_ascii_lowercase()
        .contains("logged in"))
}

fn normalize_usage_response(value: &Value) -> Result<UsageBudget> {
    let session_window =
        find_object(value, &["five_hour", "fiveHour", "session"]).ok_or_else(|| {
            anyhow::anyhow!("unable to locate session usage window in Codex response")
        })?;
    let weekly_window = find_object(value, &["seven_day", "sevenDay", "weekly"])
        .ok_or_else(|| anyhow::anyhow!("unable to locate weekly usage window in Codex response"))?;

    let session_utilization = find_f64(session_window, &["utilization", "used_fraction", "usage"])
        .ok_or_else(|| {
            anyhow::anyhow!("unable to parse session utilization from Codex response")
        })?;
    let weekly_utilization = find_f64(weekly_window, &["utilization", "used_fraction", "usage"])
        .ok_or_else(|| anyhow::anyhow!("unable to parse weekly utilization from Codex response"))?;
    let session_reset = find_string(session_window, &["resets_at", "resetsAt", "reset_at"])
        .ok_or_else(|| {
            anyhow::anyhow!("unable to parse session reset timestamp from Codex response")
        })?;
    let weekly_reset = find_string(weekly_window, &["resets_at", "resetsAt", "reset_at"])
        .ok_or_else(|| {
            anyhow::anyhow!("unable to parse weekly reset timestamp from Codex response")
        })?;

    Ok(UsageBudget::new(
        normalize_utilization(session_utilization)?,
        DateTime::parse_from_rfc3339(&session_reset)
            .with_context(|| format!("invalid Codex session reset timestamp `{session_reset}`"))?
            .to_utc(),
        normalize_utilization(weekly_utilization)?,
        DateTime::parse_from_rfc3339(&weekly_reset)
            .with_context(|| format!("invalid Codex weekly reset timestamp `{weekly_reset}`"))?
            .to_utc(),
    ))
}

fn normalize_rpc_usage_response(value: &Value) -> Result<UsageBudget> {
    let snapshot = codex_rate_limit_snapshot(value)?;
    let mut primary = parse_rpc_window(snapshot, "primary")?;
    let mut secondary = parse_rpc_window(snapshot, "secondary")?;

    if let (Some(left), Some(right)) =
        (primary.window_duration_mins, secondary.window_duration_mins)
        && left > right
    {
        std::mem::swap(&mut primary, &mut secondary);
    }

    Ok(UsageBudget::new(
        normalize_percentage_utilization(primary.used_percent)?,
        primary.resets_at,
        normalize_percentage_utilization(secondary.used_percent)?,
        secondary.resets_at,
    ))
}

fn codex_rate_limit_snapshot(value: &Value) -> Result<&Value> {
    if let Some(snapshot) = value
        .get("rateLimitsByLimitId")
        .and_then(|limits| limits.get("codex"))
    {
        return Ok(snapshot);
    }

    if let Some(snapshot) = value.get("rateLimits") {
        return Ok(snapshot);
    }

    if let Some(limits) = value.get("rateLimitsByLimitId").and_then(Value::as_object)
        && limits.len() == 1
    {
        return limits
            .values()
            .next()
            .ok_or_else(|| anyhow!("Codex app-server returned an empty rateLimitsByLimitId map"));
    }

    bail!("unable to locate Codex rate limit snapshot in app-server response")
}

#[derive(Debug, Clone)]
struct RpcWindow {
    used_percent: f64,
    resets_at: DateTime<Utc>,
    window_duration_mins: Option<i64>,
}

fn parse_rpc_window(snapshot: &Value, field: &str) -> Result<RpcWindow> {
    let window = snapshot
        .get(field)
        .ok_or_else(|| anyhow!("Codex app-server response did not include a `{field}` window"))?;
    let used_percent = window
        .get("usedPercent")
        .and_then(value_as_f64)
        .ok_or_else(|| anyhow!("Codex `{field}` window did not include `usedPercent`"))?;
    let resets_at = window
        .get("resetsAt")
        .and_then(value_as_i64)
        .ok_or_else(|| anyhow!("Codex `{field}` window did not include `resetsAt`"))?;
    let window_duration_mins = window.get("windowDurationMins").and_then(value_as_i64);

    Ok(RpcWindow {
        used_percent,
        resets_at: timestamp_to_utc(resets_at)?,
        window_duration_mins,
    })
}

fn value_as_f64(value: &Value) -> Option<f64> {
    match value {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.parse().ok(),
        _ => None,
    }
}

fn value_as_i64(value: &Value) -> Option<i64> {
    match value {
        Value::Number(number) => number.as_i64(),
        Value::String(text) => text.parse().ok(),
        _ => None,
    }
}

fn timestamp_to_utc(raw: i64) -> Result<DateTime<Utc>> {
    let (seconds, nanos) = if raw.abs() >= 1_000_000_000_000 {
        let seconds = raw.div_euclid(1_000);
        let millis = raw.rem_euclid(1_000) as u32;
        (seconds, millis * 1_000_000)
    } else {
        (raw, 0)
    };

    Utc.timestamp_opt(seconds, nanos)
        .single()
        .ok_or_else(|| anyhow!("invalid Codex app-server timestamp `{raw}`"))
}

#[cfg(unix)]
fn run_pty_status_probe(term: &str) -> Result<String> {
    let mut command = pty_probe_command(term);
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to launch Codex CLI PTY probe")?;

    {
        let stdin = child
            .stdin
            .as_mut()
            .ok_or_else(|| anyhow!("failed to open stdin for Codex CLI PTY probe"))?;
        stdin.write_all(b"/status\n/exit\n")?;
        stdin.flush()?;
    }

    let output = child
        .wait_with_output()
        .context("failed while waiting for Codex CLI PTY probe")?;
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    if !output.status.success() {
        bail!("codex CLI PTY status probe failed: {}", combined.trim());
    }

    Ok(combined)
}

#[cfg(not(unix))]
fn run_pty_status_probe(_term: &str) -> Result<String> {
    bail!("Codex PTY fallback is only implemented on Unix platforms")
}

#[cfg(target_os = "macos")]
fn pty_probe_command(term: &str) -> Command {
    let mut command = Command::new("script");
    command
        .env("TERM", term)
        .args(["-q", "/dev/null", "codex", "--no-alt-screen"]);
    command
}

#[cfg(all(unix, not(target_os = "macos")))]
fn pty_probe_command(term: &str) -> Command {
    let mut command = Command::new("script");
    command
        .env("TERM", term)
        .args(["-qfec", "codex --no-alt-screen", "/dev/null"]);
    command
}

struct CodexAppServerClient {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    stderr: Option<ChildStderr>,
    next_id: u64,
}

impl CodexAppServerClient {
    fn spawn() -> Result<Self> {
        let mut child = Command::new("codex")
            .arg("app-server")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("failed to launch `codex app-server`")?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("failed to open stdin for `codex app-server`"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("failed to open stdout for `codex app-server`"))?;
        let stderr = child.stderr.take();

        Ok(Self {
            child,
            stdin: Some(stdin),
            stdout: BufReader::new(stdout),
            stderr,
            next_id: 1,
        })
    }

    fn initialize(&mut self) -> Result<()> {
        let _ = self.request(
            "initialize",
            json!({
                "clientInfo": {
                    "name": "sidequest",
                    "version": env!("CARGO_PKG_VERSION"),
                }
            }),
        )?;
        self.notify("initialized")
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let request_id = self.next_id;
        self.next_id += 1;
        self.write_message(&json!({
            "id": request_id,
            "method": method,
            "params": params,
        }))?;
        self.read_response(request_id, method)
    }

    fn notify(&mut self, method: &str) -> Result<()> {
        self.write_message(&json!({ "method": method }))
    }

    fn write_message(&mut self, value: &Value) -> Result<()> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| anyhow!("Codex app-server stdin is no longer available"))?;
        serde_json::to_writer(&mut *stdin, value)
            .context("failed to encode Codex app-server message")?;
        stdin.write_all(b"\n")?;
        stdin.flush()?;
        Ok(())
    }

    fn read_response(&mut self, expected_id: u64, method: &str) -> Result<Value> {
        loop {
            let mut line = String::new();
            let bytes = self
                .stdout
                .read_line(&mut line)
                .context("failed to read from `codex app-server`")?;
            if bytes == 0 {
                let stderr = self.read_stderr();
                if stderr.is_empty() {
                    bail!("`codex app-server` closed before responding to `{method}`");
                }
                bail!(
                    "`codex app-server` closed before responding to `{method}`: {}",
                    stderr.trim()
                );
            }

            if line.trim().is_empty() {
                continue;
            }

            let payload: Value = serde_json::from_str(line.trim()).with_context(|| {
                format!(
                    "failed to decode Codex app-server payload `{}`",
                    line.trim()
                )
            })?;
            if payload
                .get("id")
                .and_then(value_as_i64)
                .map(|value| value as u64)
                != Some(expected_id)
            {
                continue;
            }

            if let Some(error) = payload.get("error") {
                bail!("`codex app-server` returned an error for `{method}`: {error}");
            }

            return payload.get("result").cloned().ok_or_else(|| {
                anyhow!("`codex app-server` response for `{method}` did not include `result`")
            });
        }
    }

    fn read_stderr(&mut self) -> String {
        let Some(mut stderr) = self.stderr.take() else {
            return String::new();
        };
        let mut message = String::new();
        let _ = stderr.read_to_string(&mut message);
        message
    }
}

impl Drop for CodexAppServerClient {
    fn drop(&mut self) {
        let _ = self.stdin.take();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn parse_cli_usage(output: &str) -> Result<UsageBudget> {
    // Keep CLI parsing isolated here for the same reason as the Claude probe.
    // The fallback expects lines like:
    // `5h window: 40% used, resets at 2026-04-03T04:00:00Z`
    // `7d window: 55% used, resets at 2026-04-07T00:00:00Z`
    let session_line = output
        .lines()
        .find(|line| line.contains("5h") || line.contains("session"))
        .ok_or_else(|| anyhow::anyhow!("unable to find Codex 5h usage line"))?;
    let weekly_line = output
        .lines()
        .find(|line| line.contains("7d") || line.contains("week"))
        .ok_or_else(|| anyhow::anyhow!("unable to find Codex weekly usage line"))?;

    let session_utilization = parse_percentage(session_line)?;
    let weekly_utilization = parse_percentage(weekly_line)?;
    let session_reset = parse_reset_timestamp(session_line)?;
    let weekly_reset = parse_reset_timestamp(weekly_line)?;

    Ok(UsageBudget::new(
        session_utilization,
        session_reset,
        weekly_utilization,
        weekly_reset,
    ))
}
#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn normalizes_codex_usage_shape() {
        let response = serde_json::json!({
            "five_hour": {
                "utilization": 0.4,
                "resets_at": "2026-04-03T04:00:00Z"
            },
            "seven_day": {
                "utilization": 0.55,
                "resets_at": "2026-04-07T00:00:00Z"
            }
        });
        let usage = normalize_usage_response(&response).expect("usage");
        assert!((usage.session_utilization - 0.4).abs() < f64::EPSILON);
        assert!((usage.weekly_utilization - 0.55).abs() < f64::EPSILON);
    }

    #[test]
    fn normalizes_rpc_usage_with_codex_bucket() {
        let session_reset = Utc
            .with_ymd_and_hms(2026, 4, 3, 4, 0, 0)
            .single()
            .expect("session reset");
        let weekly_reset = Utc
            .with_ymd_and_hms(2026, 4, 7, 0, 0, 0)
            .single()
            .expect("weekly reset");
        let response = serde_json::json!({
            "rateLimitsByLimitId": {
                "codex": {
                    "primary": {
                        "usedPercent": 40,
                        "resetsAt": session_reset.timestamp_millis(),
                        "windowDurationMins": 300
                    },
                    "secondary": {
                        "usedPercent": 55,
                        "resetsAt": weekly_reset.timestamp_millis(),
                        "windowDurationMins": 10080
                    }
                }
            }
        });

        let usage = normalize_rpc_usage_response(&response).expect("usage");
        assert!((usage.session_utilization - 0.4).abs() < f64::EPSILON);
        assert_eq!(usage.session_resets_at, session_reset);
        assert!((usage.weekly_utilization - 0.55).abs() < f64::EPSILON);
        assert_eq!(usage.weekly_resets_at, weekly_reset);
    }

    #[test]
    fn normalizes_rpc_usage_with_legacy_snapshot() {
        let session_reset = Utc
            .with_ymd_and_hms(2026, 4, 3, 4, 0, 0)
            .single()
            .expect("session reset");
        let weekly_reset = Utc
            .with_ymd_and_hms(2026, 4, 7, 0, 0, 0)
            .single()
            .expect("weekly reset");
        let response = serde_json::json!({
            "rateLimits": {
                "primary": {
                    "usedPercent": 25,
                    "resetsAt": session_reset.timestamp(),
                    "windowDurationMins": 300
                },
                "secondary": {
                    "usedPercent": 65,
                    "resetsAt": weekly_reset.timestamp(),
                    "windowDurationMins": 10080
                }
            }
        });

        let usage = normalize_rpc_usage_response(&response).expect("usage");
        assert!((usage.session_utilization - 0.25).abs() < f64::EPSILON);
        assert!((usage.weekly_utilization - 0.65).abs() < f64::EPSILON);
    }

    #[test]
    fn rejects_rpc_usage_without_complete_windows() {
        let response = serde_json::json!({
            "rateLimitsByLimitId": {
                "codex": {
                    "primary": {
                        "usedPercent": 40,
                        "resetsAt": 1775188800,
                        "windowDurationMins": 300
                    }
                }
            }
        });

        assert!(normalize_rpc_usage_response(&response).is_err());
    }
}
