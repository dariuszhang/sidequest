use std::fs;
use std::path::Path;
use std::process::{Child, Command};

use anyhow::{Context, Result, bail};

use crate::platform::{Platform, SleepGuard, build_path_env, home_dir, remove_block};

const LABEL: &str = "dev.sidequest.daemon";
const ZSH_BEGIN: &str = "# >>> sidequest >>>";
const ZSH_END: &str = "# <<< sidequest <<<";

pub struct MacosPlatform;

struct CaffeinateGuard {
    child: Child,
}

impl Drop for CaffeinateGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl SleepGuard for CaffeinateGuard {}

impl Platform for MacosPlatform {
    fn name(&self) -> &'static str {
        "macos"
    }

    fn read_credential(&self, service: &str) -> Result<Option<String>> {
        let output = Command::new("security")
            .args(["find-generic-password", "-s", service, "-w"])
            .output()
            .context("failed to run macOS security command")?;
        if output.status.success() {
            return Ok(Some(
                String::from_utf8_lossy(&output.stdout).trim().to_string(),
            ));
        }

        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("could not be found") {
            return Ok(None);
        }

        bail!("failed to read credential `{service}`: {}", stderr.trim())
    }

    fn send_notification(&self, title: &str, body: &str) -> Result<()> {
        let script = format!(
            "display notification \"{}\" with title \"{}\"",
            escape_applescript(body),
            escape_applescript(title)
        );
        Command::new("osascript")
            .args(["-e", &script])
            .status()
            .context("failed to send macOS notification")?;
        Ok(())
    }

    fn begin_sleep_prevention(&self, _reason: &str) -> Result<Option<Box<dyn SleepGuard>>> {
        let child = Command::new("caffeinate")
            .args(["-dimsu"])
            .spawn()
            .context("failed to start macOS sleep-prevention guard")?;
        Ok(Some(Box::new(CaffeinateGuard { child })))
    }

    fn install_autostart(
        &self,
        binary_path: &Path,
        _sidequest_root: &Path,
        log_dir: &Path,
    ) -> Result<()> {
        let launch_agents = home_dir()?.join("Library").join("LaunchAgents");
        fs::create_dir_all(&launch_agents)
            .with_context(|| format!("failed to create {}", launch_agents.display()))?;
        fs::create_dir_all(log_dir)
            .with_context(|| format!("failed to create {}", log_dir.display()))?;

        let plist_path = launch_agents.join(format!("{LABEL}.plist"));
        let plist = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{binary}</string>
        <string>daemon</string>
    </array>
    <key>EnvironmentVariables</key>
    <dict>
        <key>PATH</key>
        <string>{path_env}</string>
    </dict>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardOutPath</key>
    <string>{stdout}</string>
    <key>StandardErrorPath</key>
    <string>{stderr}</string>
    <key>ProcessType</key>
    <string>Background</string>
</dict>
</plist>
"#,
            label = LABEL,
            binary = binary_path.display(),
            path_env = launchd_path_env()?,
            stdout = log_dir.join("launchd.out.log").display(),
            stderr = log_dir.join("launchd.err.log").display(),
        );
        fs::write(&plist_path, plist)
            .with_context(|| format!("failed to write {}", plist_path.display()))?;

        let _ = Command::new("launchctl")
            .args(["unload", plist_path.to_string_lossy().as_ref()])
            .status();
        Command::new("launchctl")
            .args(["load", plist_path.to_string_lossy().as_ref()])
            .status()
            .context("failed to load launchd agent")?;
        Ok(())
    }

    fn uninstall_autostart(&self, _sidequest_root: &Path) -> Result<()> {
        let plist_path = home_dir()?
            .join("Library")
            .join("LaunchAgents")
            .join(format!("{LABEL}.plist"));
        if plist_path.exists() {
            let _ = Command::new("launchctl")
                .args(["unload", plist_path.to_string_lossy().as_ref()])
                .status();
            fs::remove_file(&plist_path)
                .with_context(|| format!("failed to remove {}", plist_path.display()))?;
        }
        Ok(())
    }

    fn install_shell_hook(&self, sidequest_root: &Path) -> Result<()> {
        let zshrc = home_dir()?.join(".zshrc");
        let existing = fs::read_to_string(&zshrc).unwrap_or_default();
        if existing.contains(ZSH_BEGIN) {
            return Ok(());
        }

        let hook = format!(
            "\n{begin}\nif [ -f \"{latest}\" ]; then\n  if [ ! -f \"{seen}\" ] || [ \"{latest}\" -nt \"{seen}\" ]; then\n    cat \"{latest}\"\n    touch \"{seen}\"\n  fi\nfi\n{end}\n",
            begin = ZSH_BEGIN,
            end = ZSH_END,
            latest = sidequest_root.join("latest_harvest").display(),
            seen = sidequest_root.join(".harvest_seen").display(),
        );
        fs::write(&zshrc, format!("{existing}{hook}"))
            .with_context(|| format!("failed to update {}", zshrc.display()))?;
        Ok(())
    }

    fn uninstall_shell_hook(&self, _sidequest_root: &Path) -> Result<()> {
        let zshrc = home_dir()?.join(".zshrc");
        if !zshrc.exists() {
            return Ok(());
        }

        let contents = fs::read_to_string(&zshrc)
            .with_context(|| format!("failed to read {}", zshrc.display()))?;
        let stripped = remove_block(&contents, ZSH_BEGIN, ZSH_END);
        if stripped != contents {
            fs::write(&zshrc, stripped)
                .with_context(|| format!("failed to update {}", zshrc.display()))?;
        }
        Ok(())
    }
}

fn escape_applescript(input: &str) -> String {
    input.replace('\\', "\\\\").replace('"', "\\\"")
}

fn launchd_path_env() -> Result<String> {
    build_path_env(&[
        "/opt/homebrew/bin",
        "/usr/local/bin",
        "/usr/bin",
        "/bin",
        "/usr/sbin",
        "/sbin",
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launchd_path_env_includes_standard_provider_locations() {
        let path_env = launchd_path_env().expect("path env");

        assert!(path_env.contains("/opt/homebrew/bin"));
        assert!(path_env.contains("/usr/local/bin"));
        assert!(path_env.contains("/usr/bin"));
        assert!(path_env.contains("/bin"));
        assert!(path_env.contains("/.local/bin"));
        assert!(path_env.contains("/.cargo/bin"));
    }
}
