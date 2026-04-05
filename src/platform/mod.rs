pub mod linux;
pub mod macos;
pub mod windows;

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};

pub trait SleepGuard: Send {}

pub trait Platform: Send + Sync {
    fn name(&self) -> &'static str;
    fn read_credential(&self, service: &str) -> Result<Option<String>>;
    fn send_notification(&self, title: &str, body: &str) -> Result<()>;
    fn begin_sleep_prevention(&self, reason: &str) -> Result<Option<Box<dyn SleepGuard>>>;
    fn install_autostart(
        &self,
        binary_path: &Path,
        sidequest_root: &Path,
        log_dir: &Path,
    ) -> Result<()>;
    fn uninstall_autostart(&self, sidequest_root: &Path) -> Result<()>;
    fn install_shell_hook(&self, sidequest_root: &Path) -> Result<()>;
    fn uninstall_shell_hook(&self, sidequest_root: &Path) -> Result<()>;
}

pub(crate) fn home_dir() -> Result<PathBuf> {
    dirs::home_dir().ok_or_else(|| anyhow!("unable to locate home directory"))
}

pub(crate) fn remove_block(contents: &str, begin: &str, end: &str) -> String {
    if let Some(start) = contents.find(begin)
        && let Some(end_index) = contents[start..].find(end)
    {
        let end_pos = start + end_index + end.len();
        let mut result = String::new();
        result.push_str(contents[..start].trim_end_matches('\n'));
        result.push('\n');
        result.push_str(contents[end_pos..].trim_start_matches('\n'));
        return result;
    }
    contents.to_string()
}

pub(crate) fn build_path_env(standard_entries: &[&str]) -> Result<String> {
    let mut entries = Vec::new();
    let mut seen = HashSet::new();

    let mut push_entry = |entry: String| {
        if !entry.is_empty() && seen.insert(entry.clone()) {
            entries.push(entry);
        }
    };

    if let Some(path) = std::env::var_os("PATH") {
        for path in std::env::split_paths(&path) {
            push_entry(path.to_string_lossy().to_string());
        }
    }

    let home = home_dir()?;
    push_entry(home.join(".local/bin").to_string_lossy().to_string());
    push_entry(home.join(".cargo/bin").to_string_lossy().to_string());
    for entry in standard_entries {
        push_entry((*entry).to_string());
    }

    Ok(entries.join(":"))
}

pub fn current_platform() -> Box<dyn Platform> {
    #[cfg(target_os = "macos")]
    {
        Box::new(macos::MacosPlatform)
    }
    #[cfg(target_os = "linux")]
    {
        Box::new(linux::LinuxPlatform)
    }
    #[cfg(target_os = "windows")]
    {
        Box::new(windows::WindowsPlatform)
    }
}
