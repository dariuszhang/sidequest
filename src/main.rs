use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use fs2::FileExt;

use sidequest::config::{
    GrindRepoConfig, ProviderConfig, QuestConfig, QuestStatus, SideQuestConfig, SideQuestPaths,
    normalize_user_path, validate_quest_name,
};
use sidequest::daemon::SideQuestDaemon;
use sidequest::harvester::slugify;
use sidequest::loot;
use sidequest::oracle::{OracleService, ProviderKind};
use sidequest::platform::current_platform;
use sidequest::runtime::{
    ControlRequest, ControlRequestKind, append_control_request, next_request_id, read_snapshot,
};
use sidequest::scanner::scan_recent_repositories;
use sidequest::state::{read_harvest_ledger, read_quest_log};
use sidequest::tui::init::{InitInput, InitOutcome, run_init_wizard, run_repo_selector};

#[derive(Debug, Parser)]
#[command(
    name = "sidequest",
    about = "Turn idle AI tokens into overnight magic."
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    Init,
    Grind {
        #[command(subcommand)]
        command: Option<GrindCommand>,
        path: Option<String>,
    },
    Quest {
        #[command(subcommand)]
        command: Option<QuestCommand>,
        goal: Option<String>,
    },
    Loot,
    Run {
        #[arg(long, alias = "force")]
        now: bool,
    },
    Config,
    Install,
    Uninstall,
    Stop,
    Daemon,
}

#[derive(Debug, Subcommand)]
enum GrindCommand {
    List,
    Remove { selector: String },
    Scan,
}

#[derive(Debug, Subcommand)]
enum QuestCommand {
    List,
    Edit { name: String },
    Pause { name: String },
    Resume { name: String },
    Remove { name: String },
    Log { name: String },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        None => status_command(),
        Some(Commands::Init) => init_command(),
        Some(Commands::Grind { command, path }) => grind_command(command, path),
        Some(Commands::Quest { command, goal }) => quest_command(command, goal),
        Some(Commands::Loot) => loot_command(),
        Some(Commands::Run { now }) => run_command(now),
        Some(Commands::Config) => config_command(),
        Some(Commands::Install) => install_command(),
        Some(Commands::Uninstall) => uninstall_command(),
        Some(Commands::Stop) => stop_command(),
        Some(Commands::Daemon) => daemon_command(),
    }
}

fn status_command() -> Result<()> {
    let paths = SideQuestPaths::discover()?;
    paths.ensure()?;
    let config = SideQuestConfig::load_or_create_default_from_paths(&paths)?;
    let snapshot = read_snapshot(&paths)?;
    let ledger = read_harvest_ledger(&paths)?;
    sidequest::tui::status::run_status(&config, snapshot.as_ref(), &ledger.entries)
}

fn init_command() -> Result<()> {
    let platform = current_platform();
    let paths = SideQuestPaths::discover()?;
    paths.ensure()?;

    let mut config = SideQuestConfig::load_or_create_default_from_paths(&paths)?;
    let oracle = OracleService::new(platform.as_ref());
    let detected = oracle.detect_available_providers();

    if detected.contains(&ProviderKind::Claude) {
        config.providers.claude = ProviderConfig::default();
    }
    if detected.contains(&ProviderKind::Codex) {
        config.providers.codex = ProviderConfig::default();
    }

    let scanned = scan_recent_repositories(&config)?;
    let input = InitInput {
        detected_providers: detected,
        scanned_repos: scanned,
        current_config: config.clone(),
    };
    let outcome = run_init_wizard(input)?;

    match outcome {
        InitOutcome::Cancelled => return Ok(()),
        InitOutcome::Completed(result) => {
            if let Some(pref) = result.provider_preference {
                config.provider_preference = pref;
            }
            // Apply enabled/disabled state
            config.providers.claude.enabled =
                result.enabled_providers.contains(&ProviderKind::Claude);
            config.providers.codex.enabled =
                result.enabled_providers.contains(&ProviderKind::Codex);
            sync_grind_selection(&mut config, &result.selected_repos)?;
            if let Some(quest) = result.quest {
                create_quest(&mut config, &paths, &quest.goal)?;
                let quest_count = config.quests.len();
                if quest_count > 0 {
                    let exclude = &config.quests[..quest_count - 1];
                    validate_quest_name(&quest.name, exclude)?;
                    config.quests[quest_count - 1].name = quest.name;
                    config.quests[quest_count - 1].directory = quest.directory;
                }
            }
            if let Some((start, end)) = result.sleep_window {
                config.sleep_window.start = start;
                config.sleep_window.end = end;
            }
            config.save_with_paths(&paths)?;
            install_command()?;
        }
    }
    Ok(())
}

fn grind_command(command: Option<GrindCommand>, path: Option<String>) -> Result<()> {
    let mut config = SideQuestConfig::load_or_create_default()?;
    match command {
        Some(GrindCommand::List) => grind_list_command(&config),
        Some(GrindCommand::Remove { selector }) => {
            let removed = config.remove_grind_repo(&selector)?;
            if removed == 0 {
                println!("No matching grind repo was configured.");
            } else {
                config.save()?;
                println!(
                    "Removed {} repo{} from nightly grind.",
                    removed,
                    plural(removed)
                );
            }
            Ok(())
        }
        Some(GrindCommand::Scan) => {
            let scanned = scan_recent_repositories(&config)?;
            if let Some(selected) = run_repo_selector(scanned)? {
                sync_grind_selection(&mut config, &selected)?;
                config.save()?;
                println!(
                    "Nightly grind now tracks {} repo{}.",
                    config.grind.len(),
                    plural(config.grind.len())
                );
            }
            Ok(())
        }
        None => {
            let adding_current_dir = path.is_none();
            let target = match path {
                Some(ref path) => normalize_user_path(path)?,
                None => std::env::current_dir().context("failed to read current directory")?,
            };

            if !target.join(".git").exists() {
                bail!("{} is not a git repository", target.display());
            }

            if adding_current_dir
                && config
                    .grind
                    .iter()
                    .filter_map(|repo| repo.expanded_path().ok())
                    .any(|repo_path| repo_path == target)
            {
                return grind_list_command(&config);
            }

            config.upsert_grind_repo(GrindRepoConfig {
                name: String::new(),
                path: target.display().to_string(),
            })?;
            config.save()?;
            println!("Added {} to nightly grind.", target.display());
            Ok(())
        }
    }
}

fn grind_list_command(config: &SideQuestConfig) -> Result<()> {
    if config.grind.is_empty() {
        println!("Nightly grind is empty.");
        return Ok(());
    }

    let mut repos = config.grind.clone();
    repos.sort_by(|left, right| {
        left.name
            .cmp(&right.name)
            .then_with(|| left.path.cmp(&right.path))
    });
    println!("Nightly grind ({} repos):", repos.len());
    for repo in repos {
        println!(
            "  {:<18} {}",
            repo.resolved_name()?,
            display_path(&repo.expanded_path()?)
        );
    }
    Ok(())
}

fn quest_command(command: Option<QuestCommand>, goal: Option<String>) -> Result<()> {
    let mut config = SideQuestConfig::load_or_create_default()?;
    let paths = SideQuestPaths::discover()?;
    paths.ensure()?;

    match command {
        Some(QuestCommand::List) => {
            if config.quests.is_empty() {
                println!("No quests configured.");
                return Ok(());
            }
            println!("Quests:");
            for quest in &config.quests {
                println!(
                    "  {:<18} {:<10} {} ({})",
                    quest.name,
                    format!("{:?}", quest.status).to_lowercase(),
                    quest.goal_label(),
                    display_path(&quest.expanded_directory()?)
                );
            }
            Ok(())
        }
        Some(QuestCommand::Edit { name }) => {
            let quest = config
                .quests
                .iter_mut()
                .find(|quest| quest.name == name)
                .with_context(|| format!("unknown quest `{name}`"))?;
            let goal_path = ensure_editable_goal_file(&paths, quest)?;
            config.save_with_paths(&paths)?;
            open_in_editor(&goal_path)?;
            println!("Quest goal updated at {}.", goal_path.display());
            Ok(())
        }
        Some(QuestCommand::Pause { name }) => {
            set_quest_status(&mut config, &name, QuestStatus::Paused)
        }
        Some(QuestCommand::Resume { name }) => {
            set_quest_status(&mut config, &name, QuestStatus::Active)
        }
        Some(QuestCommand::Remove { name }) => {
            let Some(index) = config.quests.iter().position(|quest| quest.name == name) else {
                bail!("unknown quest `{name}`");
            };
            let quest = &config.quests[index];
            println!(
                "This will remove quest `{}` from SideQuest.\nThe directory {} will not be deleted.",
                quest.name, quest.directory
            );
            if ask_yes_no("Remove?", false)? {
                config.quests.remove(index);
                config.save()?;
                println!("Quest `{name}` removed.");
            }
            Ok(())
        }
        Some(QuestCommand::Log { name }) => {
            if let Some(contents) = read_quest_log(&paths, &name)? {
                print!("{contents}");
            } else {
                println!("No quest log exists for `{name}` yet.");
            }
            Ok(())
        }
        None => {
            let goal = match goal {
                Some(goal) => goal,
                None => ask_text("Quest goal (text or path to .md/.txt)")?,
            };
            if goal.trim().is_empty() {
                bail!("quest goal cannot be empty");
            }
            create_quest(&mut config, &paths, &goal)
        }
    }
}

fn loot_command() -> Result<()> {
    let paths = SideQuestPaths::discover()?;
    paths.ensure()?;
    loot::run(&paths)
}

fn run_command(now: bool) -> Result<()> {
    let config = SideQuestConfig::load_or_create_default()?;
    let daemon = SideQuestDaemon::new(current_platform())?;
    let report = match daemon.run_once(&config, now) {
        Ok(report) => report,
        Err(error) if is_instance_already_active(&error) => {
            println!("Another SideQuest run is already active. Try again after it finishes.");
            return Ok(());
        }
        Err(error) => return Err(error),
    };

    if report.tasks.is_empty() {
        println!("No task launched: {}", report.final_decision.reason);
    } else {
        println!(
            "{} task{} completed. Run `sidequest loot` to review.",
            report.tasks.len(),
            plural(report.tasks.len())
        );
    }
    Ok(())
}

fn config_command() -> Result<()> {
    let config = SideQuestConfig::load_or_create_default()?;
    let path = config.save()?;
    open_in_editor(&path)?;
    println!("{}", path.display());
    Ok(())
}

fn install_command() -> Result<()> {
    let paths = SideQuestPaths::discover()?;
    paths.ensure()?;
    let platform = current_platform();
    let config = SideQuestConfig::load_or_create_default()?;
    let binary = std::env::current_exe().context("failed to locate current sidequest binary")?;
    platform.install_autostart(&binary, &paths.root, &paths.logs_dir)?;
    if config.notifications.terminal_banner {
        platform.install_shell_hook(&paths.root)?;
    } else {
        platform.uninstall_shell_hook(&paths.root)?;
    }
    println!("Installed autostart and shell hooks.");
    Ok(())
}

fn uninstall_command() -> Result<()> {
    let paths = SideQuestPaths::discover()?;
    let platform = current_platform();
    platform.uninstall_autostart(&paths.root)?;
    platform.uninstall_shell_hook(&paths.root)?;
    println!("Removed autostart and shell hooks.");
    Ok(())
}

fn stop_command() -> Result<()> {
    let paths = SideQuestPaths::discover()?;
    paths.ensure()?;
    if !daemon_is_running(&paths)? {
        println!("SideQuest daemon is not running.");
        return Ok(());
    }

    append_control_request(
        &paths,
        &ControlRequest {
            id: next_request_id(),
            created_at: chrono::Utc::now(),
            kind: ControlRequestKind::StopDaemon,
        },
    )?;
    println!("Stop requested. The daemon will shut down shortly.");
    Ok(())
}

fn daemon_command() -> Result<()> {
    let daemon = SideQuestDaemon::new(current_platform())?;
    daemon.run_forever()
}

fn create_quest(
    config: &mut SideQuestConfig,
    paths: &SideQuestPaths,
    goal_input: &str,
) -> Result<()> {
    let (goal, goal_file) = resolve_goal_input(goal_input)?;
    let suggested = goal
        .clone()
        .or_else(|| {
            goal_file
                .as_ref()
                .and_then(|path| path.file_stem())
                .map(|name| name.to_string_lossy().to_string())
        })
        .unwrap_or_else(|| "quest".to_string());
    let default_name = slugify(&suggested);
    let name_prompt = format!("Name this quest [{}]", default_name);
    let name_input = ask_text(&name_prompt)?;
    let name = if name_input.trim().is_empty() {
        default_name
    } else {
        name_input
    };
    validate_quest_name(&name, &config.quests)?;

    let default_directory = config
        .default_quest_projects_directory()?
        .join(&name)
        .display()
        .to_string();
    let directory_input = ask_text(&format!("Directory [{}]", default_directory))?;
    let directory = if directory_input.trim().is_empty() {
        default_directory
    } else {
        normalize_user_path(&directory_input)?.display().to_string()
    };

    config.quests.push(QuestConfig {
        name: name.clone(),
        goal,
        goal_file,
        directory,
        status: QuestStatus::Active,
    });
    config.save_with_paths(paths)?;
    println!("Quest `{name}` created.");
    Ok(())
}

fn resolve_goal_input(input: &str) -> Result<(Option<String>, Option<PathBuf>)> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        bail!("quest goal cannot be empty");
    }

    let looks_like_file = trimmed.ends_with(".md") || trimmed.ends_with(".txt");
    if looks_like_file || Path::new(trimmed).exists() {
        let path = normalize_user_path(trimmed)?;
        if !path.exists() {
            bail!("quest goal file {} does not exist", path.display());
        }
        return Ok((None, Some(path)));
    }

    Ok((Some(trimmed.to_string()), None))
}

fn ensure_editable_goal_file(paths: &SideQuestPaths, quest: &mut QuestConfig) -> Result<PathBuf> {
    if let Some(path) = &quest.goal_file {
        return Ok(path.clone());
    }

    paths.ensure()?;
    let path = paths.quests_dir.join(format!("{}.md", quest.name));
    if !path.exists() {
        fs::write(&path, format!("{}\n", quest.resolve_goal()?))
            .with_context(|| format!("failed to write {}", path.display()))?;
    }
    quest.goal = None;
    quest.goal_file = Some(path.clone());
    Ok(path)
}

fn set_quest_status(config: &mut SideQuestConfig, name: &str, status: QuestStatus) -> Result<()> {
    let quest = config
        .quests
        .iter_mut()
        .find(|quest| quest.name == name)
        .with_context(|| format!("unknown quest `{name}`"))?;
    quest.status = status;
    config.save()?;
    println!(
        "Quest `{name}` is now {}.",
        match status {
            QuestStatus::Active => "active",
            QuestStatus::Paused => "paused",
            QuestStatus::Completed => "completed",
        }
    );
    Ok(())
}

fn sync_grind_selection(config: &mut SideQuestConfig, selected: &[PathBuf]) -> Result<()> {
    config.grind.clear();
    for path in selected {
        config.upsert_grind_repo(GrindRepoConfig {
            name: String::new(),
            path: path.display().to_string(),
        })?;
    }
    Ok(())
}

fn daemon_is_running(paths: &SideQuestPaths) -> Result<bool> {
    let lock_path = paths.instance_lock_file();
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("failed to open {}", lock_path.display()))?;
    match file.try_lock_exclusive() {
        Ok(()) => {
            file.unlock()
                .with_context(|| format!("failed to unlock {}", lock_path.display()))?;
            Ok(false)
        }
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(true),
        Err(error) => Err(error).with_context(|| format!("failed to lock {}", lock_path.display())),
    }
}

fn is_instance_already_active(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .to_string()
            .contains("another SideQuest instance is already active")
    })
}

fn open_in_editor(path: &Path) -> Result<()> {
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "nano".to_string());
    let status = Command::new("sh")
        .arg("-lc")
        .arg(format!("{editor} {}", shell_escape(path)))
        .status()
        .with_context(|| format!("failed to launch editor `{editor}`"))?;
    if !status.success() {
        bail!("editor `{editor}` exited unsuccessfully");
    }
    Ok(())
}

fn ask_yes_no(prompt: &str, default_yes: bool) -> Result<bool> {
    let suffix = if default_yes { "[Y/n]" } else { "[y/N]" };
    print!("{prompt} {suffix} ");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let trimmed = input.trim().to_ascii_lowercase();
    if trimmed.is_empty() {
        return Ok(default_yes);
    }
    Ok(matches!(trimmed.as_str(), "y" | "yes"))
}

fn ask_text(prompt: &str) -> Result<String> {
    print!("{prompt}: ");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    Ok(input.trim().to_string())
}

fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

fn display_path(path: &Path) -> String {
    if let Some(home) = dirs::home_dir()
        && let Ok(stripped) = path.strip_prefix(&home)
    {
        return format!("~/{}", stripped.display());
    }
    path.display().to_string()
}

fn shell_escape(path: &Path) -> String {
    let raw = path.display().to_string();
    format!("'{}'", raw.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_inline_goal_input() {
        let (goal, goal_file) = resolve_goal_input("Ship a tiny useful CLI").expect("goal");
        assert_eq!(goal.as_deref(), Some("Ship a tiny useful CLI"));
        assert!(goal_file.is_none());
    }

    #[test]
    fn expands_shell_escape() {
        let escaped = shell_escape(Path::new("/tmp/it's-here"));
        assert_eq!(escaped, "'/tmp/it'\\''s-here'");
    }
}
