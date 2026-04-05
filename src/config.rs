use std::collections::HashSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use chrono::NaiveTime;
use clap::ValueEnum;
use serde::{Deserialize, Serialize};

use crate::oracle::ProviderKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum WorkMode {
    Grind,
    Quest,
}

impl WorkMode {
    pub fn all() -> [Self; 2] {
        [Self::Grind, Self::Quest]
    }
}

impl std::fmt::Display for WorkMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let label = match self {
            Self::Grind => "grind",
            Self::Quest => "quest",
        };
        write!(f, "{label}")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AuthMethod {
    #[default]
    Auto,
    Oauth,
    Cli,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum QuestStatus {
    #[default]
    Active,
    Paused,
    Completed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SleepWindow {
    pub start: String,
    pub end: String,
}

impl SleepWindow {
    pub fn start_time(&self) -> Result<NaiveTime> {
        parse_time(&self.start)
    }

    pub fn end_time(&self) -> Result<NaiveTime> {
        parse_time(&self.end)
    }
}

impl Default for SleepWindow {
    fn default() -> Self {
        Self {
            start: "23:00".to_string(),
            end: "07:00".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub enabled: bool,
    #[serde(default)]
    pub auth_method: AuthMethod,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            auth_method: AuthMethod::Auto,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProvidersConfig {
    #[serde(default)]
    pub claude: ProviderConfig,
    #[serde(default)]
    pub codex: ProviderConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrindRepoConfig {
    #[serde(default)]
    pub name: String,
    pub path: String,
}

impl GrindRepoConfig {
    pub fn expanded_path(&self) -> Result<PathBuf> {
        configured_path(&self.path, "grind path")
    }

    pub fn resolved_name(&self) -> Result<String> {
        let trimmed = self.name.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }

        let expanded = self.expanded_path()?;
        Ok(infer_repository_name(&expanded))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuestConfig {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal_file: Option<PathBuf>,
    pub directory: String,
    #[serde(default)]
    pub status: QuestStatus,
}

impl QuestConfig {
    pub fn expanded_directory(&self) -> Result<PathBuf> {
        configured_path(&self.directory, "quest directory")
    }

    pub fn resolve_goal(&self) -> Result<String> {
        match (&self.goal, &self.goal_file) {
            (Some(text), None) if !text.trim().is_empty() => Ok(text.trim().to_string()),
            (None, Some(path)) => {
                let expanded = configured_path(&path.to_string_lossy(), "quest goal_file")?;
                let raw = fs::read_to_string(&expanded).with_context(|| {
                    format!("failed to read quest goal from {}", expanded.display())
                })?;
                let trimmed = raw.trim();
                if trimmed.is_empty() {
                    bail!("quest `{}` goal file is empty", self.name);
                }
                Ok(trimmed.to_string())
            }
            _ => bail!(
                "quest `{}` must set exactly one of `goal` or `goal_file`",
                self.name
            ),
        }
    }

    pub fn goal_label(&self) -> String {
        if let Some(path) = &self.goal_file {
            return path.display().to_string();
        }

        self.goal
            .as_deref()
            .unwrap_or_default()
            .lines()
            .next()
            .unwrap_or_default()
            .trim()
            .to_string()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotificationsConfig {
    pub desktop: bool,
    pub terminal_banner: bool,
}

impl Default for NotificationsConfig {
    fn default() -> Self {
        Self {
            desktop: true,
            terminal_banner: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionPromptsConfig {
    #[serde(default = "default_grind_action_prompt")]
    pub grind: String,
    #[serde(default = "default_quest_action_prompt")]
    pub quest: String,
    #[serde(default, skip_serializing, rename = "review")]
    legacy_review: Option<String>,
    #[serde(default, skip_serializing, rename = "build")]
    legacy_build: Option<String>,
    #[serde(default, skip_serializing, rename = "side_quest")]
    legacy_side_quest: Option<String>,
}

impl Default for ActionPromptsConfig {
    fn default() -> Self {
        Self {
            grind: default_grind_action_prompt(),
            quest: default_quest_action_prompt(),
            legacy_review: None,
            legacy_build: None,
            legacy_side_quest: None,
        }
    }
}

impl ActionPromptsConfig {
    fn normalize_legacy(&mut self) {
        if self.quest == default_quest_action_prompt()
            && let Some(side_quest) = self.legacy_side_quest.take()
            && !side_quest.trim().is_empty()
        {
            self.quest = side_quest.trim().to_string();
        }

        if self.grind == default_grind_action_prompt() {
            let mut legacy_parts = Vec::new();
            if let Some(review) = self.legacy_review.take()
                && !review.trim().is_empty()
            {
                legacy_parts.push(review.trim().to_string());
            }
            if let Some(build) = self.legacy_build.take()
                && !build.trim().is_empty()
            {
                legacy_parts.push(build.trim().to_string());
            }

            if !legacy_parts.is_empty() {
                self.grind = legacy_parts.join("\n");
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptsConfig {
    #[serde(default = "default_work_delegation_prompt")]
    pub work_delegation: String,
    #[serde(default)]
    pub actions: ActionPromptsConfig,
}

impl Default for PromptsConfig {
    fn default() -> Self {
        Self {
            work_delegation: default_work_delegation_prompt(),
            actions: ActionPromptsConfig::default(),
        }
    }
}

impl PromptsConfig {
    pub fn validate(&self) -> Result<()> {
        ensure_prompt_text(&self.work_delegation, "prompts.work_delegation")?;
        ensure_prompt_text(&self.actions.grind, "prompts.actions.grind")?;
        ensure_prompt_text(&self.actions.quest, "prompts.actions.quest")?;
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SideQuestConfig {
    #[serde(default)]
    pub sleep_window: SleepWindow,
    #[serde(default = "default_safety_margin")]
    pub safety_margin: f64,
    #[serde(default)]
    pub prefer_quests: bool,
    #[serde(default = "default_provider_preference")]
    pub provider_preference: Vec<ProviderKind>,
    #[serde(default)]
    pub providers: ProvidersConfig,
    #[serde(default, alias = "repositories")]
    pub grind: Vec<GrindRepoConfig>,
    #[serde(default, alias = "quest_repo_directory")]
    pub quest_projects_directory: Option<String>,
    #[serde(default)]
    pub quests: Vec<QuestConfig>,
    #[serde(default)]
    pub notifications: NotificationsConfig,
    #[serde(default)]
    pub prompts: PromptsConfig,
}

impl Default for SideQuestConfig {
    fn default() -> Self {
        Self {
            sleep_window: SleepWindow::default(),
            safety_margin: default_safety_margin(),
            prefer_quests: false,
            provider_preference: default_provider_preference(),
            providers: ProvidersConfig::default(),
            grind: Vec::new(),
            quest_projects_directory: None,
            quests: Vec::new(),
            notifications: NotificationsConfig::default(),
            prompts: PromptsConfig::default(),
        }
    }
}

impl SideQuestConfig {
    pub fn load_or_default() -> Result<Self> {
        let paths = SideQuestPaths::discover()?;
        Self::load_or_default_from_paths(&paths)
    }

    pub fn load_or_default_from_paths(paths: &SideQuestPaths) -> Result<Self> {
        if !paths.config_file.exists() {
            return Ok(Self::default());
        }
        Self::load_from(&paths.config_file)
    }

    pub fn load_or_create_default() -> Result<Self> {
        let paths = SideQuestPaths::discover()?;
        Self::load_or_create_default_from_paths(&paths)
    }

    pub fn load_or_create_default_from_paths(paths: &SideQuestPaths) -> Result<Self> {
        paths.ensure()?;
        if paths.config_file.exists() {
            return Self::load_from(&paths.config_file);
        }

        let config = Self::default();
        config.save_to(&paths.config_file)?;
        Ok(config)
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let mut config: Self = serde_yaml::from_str(&raw)
            .with_context(|| format!("failed to parse {}", path.display()))?;
        config.prompts.actions.normalize_legacy();
        config.validate()?;
        Ok(config)
    }

    pub fn save(&self) -> Result<PathBuf> {
        let paths = SideQuestPaths::discover()?;
        self.save_with_paths(&paths)
    }

    pub fn save_with_paths(&self, paths: &SideQuestPaths) -> Result<PathBuf> {
        paths.ensure()?;
        self.save_to(&paths.config_file)?;
        Ok(paths.config_file.clone())
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        self.validate()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let raw = serde_yaml::to_string(self).context("failed to serialize config")?;
        fs::write(path, raw).with_context(|| format!("failed to write {}", path.display()))?;
        Ok(())
    }

    pub fn validate(&self) -> Result<()> {
        let start = self.sleep_window.start_time()?;
        let end = self.sleep_window.end_time()?;
        if start == end {
            bail!("sleep window start and end must be different");
        }
        if !(0.0..0.9).contains(&self.safety_margin) {
            bail!("safety_margin must be between 0.0 and 0.9");
        }
        ensure_unique_providers(&self.provider_preference, "provider_preference")?;
        if self.provider_preference.is_empty() {
            bail!("provider_preference must contain at least one provider");
        }

        let mut grind_paths = HashSet::new();
        for repo in &self.grind {
            if repo.path.trim().is_empty() {
                bail!("grind path cannot be empty");
            }
            let expanded = repo.expanded_path()?;
            repo.resolved_name()?;
            if !grind_paths.insert(expanded.clone()) {
                bail!(
                    "grind directory {} is configured more than once",
                    expanded.display()
                );
            }
        }

        if let Some(directory) = &self.quest_projects_directory {
            if directory.trim().is_empty() {
                bail!("quest_projects_directory cannot be empty when set");
            }
            configured_path(directory, "quest_projects_directory")?;
        }

        let mut quest_names = HashSet::new();
        for quest in &self.quests {
            validate_quest_name_value(&quest.name, &quest_names)?;
            quest_names.insert(quest.name.clone());
            quest.expanded_directory()?;

            match (&quest.goal, &quest.goal_file) {
                (Some(text), None) if !text.trim().is_empty() => {}
                (None, Some(path)) => {
                    configured_path(&path.to_string_lossy(), "quest goal_file")?;
                }
                _ => bail!(
                    "quest `{}` must set exactly one of `goal` or `goal_file`",
                    quest.name
                ),
            }
        }

        self.prompts.validate()?;
        Ok(())
    }

    pub fn upsert_grind_repo(&mut self, mut repository: GrindRepoConfig) -> Result<()> {
        let normalized_path = normalize_user_path(&repository.path)?;
        repository.path = normalized_path.display().to_string();
        repository.name = normalize_repository_name(&repository.name, &normalized_path);
        let path = repository.expanded_path()?;
        self.grind
            .retain(|existing| existing.expanded_path().map(|p| p != path).unwrap_or(true));
        self.grind.push(repository);
        self.validate()
    }

    pub fn remove_grind_repo(&mut self, selector: &str) -> Result<usize> {
        let trimmed_selector = selector.trim();
        if trimmed_selector.is_empty() {
            bail!("grind selector cannot be empty");
        }

        let normalized_selector = normalize_user_path(trimmed_selector).ok();
        let mut retained = Vec::with_capacity(self.grind.len());
        let mut removed = 0usize;
        for repository in std::mem::take(&mut self.grind) {
            if repository_matches_selector(
                &repository,
                trimmed_selector,
                normalized_selector.as_deref(),
            )? {
                removed += 1;
            } else {
                retained.push(repository);
            }
        }
        self.grind = retained;
        self.validate()?;
        Ok(removed)
    }

    pub fn grind_repositories_matching_selector(
        &self,
        selector: &str,
    ) -> Result<Vec<GrindRepoConfig>> {
        let trimmed_selector = selector.trim();
        if trimmed_selector.is_empty() {
            return Ok(Vec::new());
        }

        let normalized_selector = normalize_user_path(trimmed_selector).ok();
        let mut matches = Vec::new();
        for repo in &self.grind {
            if repository_matches_selector(repo, trimmed_selector, normalized_selector.as_deref())?
            {
                matches.push(repo.clone());
            }
        }
        Ok(matches)
    }

    pub fn default_quest_projects_directory(&self) -> Result<PathBuf> {
        match self.quest_projects_directory.as_deref() {
            Some(path) => configured_path(path, "quest_projects_directory"),
            None => dirs::home_dir()
                .map(|home| home.join("sidequest-projects"))
                .ok_or_else(|| anyhow!("unable to locate home directory")),
        }
    }
}

#[derive(Debug, Clone)]
pub struct SideQuestPaths {
    pub root: PathBuf,
    pub config_file: PathBuf,
    pub harvests_dir: PathBuf,
    pub quests_dir: PathBuf,
    pub state_dir: PathBuf,
    pub quest_logs_dir: PathBuf,
    pub logs_dir: PathBuf,
    pub backlog_file: PathBuf,
    pub last_session_report_file: PathBuf,
    pub runtime_snapshot_file: PathBuf,
    pub runtime_events_file: PathBuf,
    pub control_requests_file: PathBuf,
    pub harvest_ledger_file: PathBuf,
    pub latest_harvest_file: PathBuf,
    pub daemon_log_file: PathBuf,
}

impl SideQuestPaths {
    pub fn from_root(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let state_dir = root.join("state");
        let logs_dir = root.join("logs");
        Self {
            config_file: root.join("config.yaml"),
            harvests_dir: root.join("harvests"),
            quests_dir: root.join("quests"),
            quest_logs_dir: state_dir.join("quests"),
            backlog_file: state_dir.join("backlog.json"),
            last_session_report_file: state_dir.join("last-session-report.json"),
            runtime_snapshot_file: state_dir.join("runtime-snapshot.json"),
            runtime_events_file: state_dir.join("runtime-events.jsonl"),
            control_requests_file: state_dir.join("control-requests.jsonl"),
            harvest_ledger_file: state_dir.join("harvest-ledger.json"),
            daemon_log_file: logs_dir.join("daemon.log"),
            latest_harvest_file: root.join("latest_harvest"),
            state_dir,
            logs_dir,
            root,
        }
    }

    pub fn discover() -> Result<Self> {
        let root = match env::var_os("SIDEQUEST_HOME") {
            Some(path) => PathBuf::from(path),
            None => dirs::home_dir()
                .map(|dir| dir.join(".sidequest"))
                .ok_or_else(|| anyhow!("unable to locate home directory"))?,
        };

        Ok(Self::from_root(root))
    }

    pub fn ensure(&self) -> Result<()> {
        fs::create_dir_all(&self.root)
            .with_context(|| format!("failed to create {}", self.root.display()))?;
        fs::create_dir_all(&self.harvests_dir)
            .with_context(|| format!("failed to create {}", self.harvests_dir.display()))?;
        fs::create_dir_all(&self.quests_dir)
            .with_context(|| format!("failed to create {}", self.quests_dir.display()))?;
        fs::create_dir_all(&self.state_dir)
            .with_context(|| format!("failed to create {}", self.state_dir.display()))?;
        fs::create_dir_all(&self.quest_logs_dir)
            .with_context(|| format!("failed to create {}", self.quest_logs_dir.display()))?;
        fs::create_dir_all(&self.logs_dir)
            .with_context(|| format!("failed to create {}", self.logs_dir.display()))?;
        Ok(())
    }

    pub fn instance_lock_file(&self) -> PathBuf {
        self.state_dir.join("daemon.lock")
    }
}

pub fn normalize_user_path(input: &str) -> Result<PathBuf> {
    let expanded = expand_home(input)?;
    Ok(if expanded.is_absolute() {
        expanded
    } else {
        env::current_dir()
            .context("failed to read current directory")?
            .join(expanded)
    })
}

pub fn expand_home(input: &str) -> Result<PathBuf> {
    if input == "~" {
        return dirs::home_dir().ok_or_else(|| anyhow!("unable to locate home directory"));
    }

    if let Some(rest) = input.strip_prefix("~/") {
        return dirs::home_dir()
            .map(|home| home.join(rest))
            .ok_or_else(|| anyhow!("unable to locate home directory"));
    }

    Ok(PathBuf::from(input))
}

fn configured_path(input: &str, label: &str) -> Result<PathBuf> {
    let expanded = expand_home(input)?;
    if expanded.is_absolute() {
        return Ok(expanded);
    }

    bail!(
        "{label} `{input}` must be absolute; rewrite existing relative config entries before running SideQuest"
    )
}

fn default_safety_margin() -> f64 {
    0.15
}

fn default_provider_preference() -> Vec<ProviderKind> {
    vec![ProviderKind::Claude, ProviderKind::Codex]
}

fn default_work_delegation_prompt() -> String {
    include_str!("../defaults/prompts/work_delegation.txt")
        .trim()
        .to_string()
}

fn default_grind_action_prompt() -> String {
    include_str!("../defaults/prompts/action_grind.txt")
        .trim()
        .to_string()
}

fn default_quest_action_prompt() -> String {
    include_str!("../defaults/prompts/action_quest.txt")
        .trim()
        .to_string()
}

fn parse_time(input: &str) -> Result<NaiveTime> {
    NaiveTime::parse_from_str(input, "%H:%M")
        .with_context(|| format!("expected time in HH:MM format, got {input}"))
}

fn ensure_unique_providers(providers: &[ProviderKind], label: &str) -> Result<()> {
    let mut unique = HashSet::new();
    for provider in providers {
        if !unique.insert(*provider) {
            bail!("{label} contains duplicate provider {provider}");
        }
    }
    Ok(())
}

fn ensure_prompt_text(value: &str, label: &str) -> Result<()> {
    if value.trim().is_empty() {
        bail!("{label} cannot be empty");
    }
    Ok(())
}

fn normalize_repository_name(input: &str, normalized_path: &Path) -> String {
    let trimmed = input.trim();
    if !trimmed.is_empty() {
        return trimmed.to_string();
    }
    infer_repository_name(normalized_path)
}

fn infer_repository_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().to_string())
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| path.display().to_string())
}

fn repository_matches_selector(
    repository: &GrindRepoConfig,
    selector: &str,
    normalized_selector: Option<&Path>,
) -> Result<bool> {
    if repository.name == selector {
        return Ok(true);
    }

    let path = repository.expanded_path()?;
    if let Some(normalized_selector) = normalized_selector
        && path == normalized_selector
    {
        return Ok(true);
    }

    Ok(path.display().to_string() == selector)
}

fn validate_quest_name_value(name: &str, existing: &HashSet<String>) -> Result<()> {
    let trimmed = name.trim();

    if trimmed.is_empty() {
        bail!("quest name cannot be empty");
    }
    if trimmed.len() > 48 {
        bail!("quest name must be 48 characters or fewer");
    }

    let first = trimmed.chars().next().unwrap_or('0');
    if !first.is_ascii_lowercase() {
        bail!("quest name must start with a lowercase letter");
    }
    if !trimmed
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        bail!("quest name may only contain lowercase letters, digits, and hyphens");
    }
    if trimmed.starts_with('-') || trimmed.ends_with('-') || trimmed.contains("--") {
        bail!("quest name cannot start/end with hyphens or contain consecutive hyphens");
    }
    if existing.contains(trimmed) {
        bail!(
            "quest `{}` already exists; use `sidequest quest edit {}` to modify it",
            trimmed,
            trimmed
        );
    }

    Ok(())
}

pub fn validate_quest_name(name: &str, existing_quests: &[QuestConfig]) -> Result<()> {
    let existing = existing_quests
        .iter()
        .map(|quest| quest.name.clone())
        .collect::<HashSet<_>>();
    validate_quest_name_value(name, &existing)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_quest_name_rules() {
        let err = validate_quest_name("Hello", &[]).expect_err("invalid");
        assert!(
            err.to_string()
                .contains("must start with a lowercase letter")
        );

        let err = validate_quest_name("bad--name", &[]).expect_err("invalid");
        assert!(
            err.to_string()
                .contains("cannot start/end with hyphens or contain consecutive hyphens")
        );

        validate_quest_name("valid-name-2", &[]).expect("valid");
    }

    #[test]
    fn reads_goal_from_goal_file() {
        let temp = tempfile::tempdir().expect("temp dir");
        let goal_path = temp.path().join("goal.md");
        fs::write(&goal_path, "Ship the next useful slice.\n").expect("goal");

        let quest = QuestConfig {
            name: "demo".to_string(),
            goal: None,
            goal_file: Some(goal_path),
            directory: temp.path().join("quest").display().to_string(),
            status: QuestStatus::Active,
        };

        assert_eq!(
            quest.resolve_goal().expect("resolved"),
            "Ship the next useful slice."
        );
    }

    #[test]
    fn legacy_repositories_alias_loads_into_grind() {
        let raw = r#"
sleep_window:
  start: "23:00"
  end: "07:00"
provider_preference:
  - claude
repositories:
  - name: "app"
    path: "/tmp/app"
quests: []
"#;

        let config: SideQuestConfig = serde_yaml::from_str(raw).expect("config");
        assert_eq!(config.grind.len(), 1);
        assert_eq!(config.grind[0].name, "app");
    }

    #[test]
    fn rejects_relative_default_quest_projects_directory() {
        let config = SideQuestConfig {
            quest_projects_directory: Some("relative/quests".to_string()),
            ..SideQuestConfig::default()
        };

        let err = config.validate().expect_err("invalid");
        assert!(
            err.to_string()
                .contains("quest_projects_directory `relative/quests` must be absolute")
        );
    }

    #[test]
    fn upsert_grind_repo_normalizes_relative_paths() {
        let temp = tempfile::tempdir().expect("temp dir");
        let cwd = env::current_dir().expect("cwd");
        env::set_current_dir(temp.path()).expect("set cwd");

        let mut config = SideQuestConfig::default();
        config
            .upsert_grind_repo(GrindRepoConfig {
                name: String::new(),
                path: "repo".to_string(),
            })
            .expect("upsert");

        env::set_current_dir(cwd).expect("restore cwd");

        assert_eq!(config.grind.len(), 1);
        assert_eq!(config.grind[0].name, "repo");
        assert!(config.grind[0].path.ends_with("/repo"));
    }
}
