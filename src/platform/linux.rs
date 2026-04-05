use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};

use anyhow::{Context, Result, bail};

use crate::platform::{Platform, SleepGuard, build_path_env, home_dir, remove_block};

const SERVICE_NAME: &str = "sidequest";
const BASH_BEGIN: &str = "# >>> sidequest >>>";
const BASH_END: &str = "# <<< sidequest <<<";

pub struct LinuxPlatform;

struct SystemdInhibitGuard {
    child: Child,
}

impl Drop for SystemdInhibitGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl SleepGuard for SystemdInhibitGuard {}

impl Platform for LinuxPlatform {
    fn name(&self) -> &'static str {
        "linux"
    }

    fn read_credential(&self, service: &str) -> Result<Option<String>> {
        let output = Command::new("secret-tool")
            .args(["lookup", "service", service])
            .output()
            .context("failed to run secret-tool — is libsecret installed?")?;
        if output.status.success() {
            let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if value.is_empty() {
                return Ok(None);
            }
            return Ok(Some(value));
        }
        Ok(None)
    }

    fn send_notification(&self, title: &str, body: &str) -> Result<()> {
        let _ = Command::new("notify-send").args([title, body]).status();
        Ok(())
    }

    fn begin_sleep_prevention(&self, reason: &str) -> Result<Option<Box<dyn SleepGuard>>> {
        let child = Command::new("systemd-inhibit")
            .args([
                "--what=idle:sleep",
                &format!("--why={reason}"),
                "--mode=block",
                "cat",
            ])
            .stdin(std::process::Stdio::piped())
            .spawn()
            .context("failed to start systemd-inhibit sleep-prevention guard")?;
        Ok(Some(Box::new(SystemdInhibitGuard { child })))
    }

    fn install_autostart(
        &self,
        binary_path: &Path,
        _sidequest_root: &Path,
        log_dir: &Path,
    ) -> Result<()> {
        let systemd_user_dir = systemd_user_dir()?;
        fs::create_dir_all(&systemd_user_dir)
            .with_context(|| format!("failed to create {}", systemd_user_dir.display()))?;
        fs::create_dir_all(log_dir)
            .with_context(|| format!("failed to create {}", log_dir.display()))?;

        let service_path = systemd_user_dir.join(format!("{SERVICE_NAME}.service"));
        let unit = format!(
            "\
[Unit]
Description=SideQuest daemon
After=default.target

[Service]
Type=simple
ExecStart={binary} daemon
Restart=on-failure
RestartSec=10
Environment=PATH={path_env}
StandardOutput=append:{stdout}
StandardError=append:{stderr}

[Install]
WantedBy=default.target
",
            binary = binary_path.display(),
            path_env = linux_path_env()?,
            stdout = log_dir.join("daemon.out.log").display(),
            stderr = log_dir.join("daemon.err.log").display(),
        );
        fs::write(&service_path, unit)
            .with_context(|| format!("failed to write {}", service_path.display()))?;

        run_systemctl(
            ["--user", "daemon-reload"],
            "failed to reload systemd user units",
        )?;
        run_systemctl(
            ["--user", "enable", "--now", SERVICE_NAME],
            "failed to enable sidequest systemd service",
        )?;
        Ok(())
    }

    fn uninstall_autostart(&self, _sidequest_root: &Path) -> Result<()> {
        let service_path = systemd_user_dir()?.join(format!("{SERVICE_NAME}.service"));
        if service_path.exists() {
            run_systemctl(
                ["--user", "disable", "--now", SERVICE_NAME],
                "failed to disable sidequest systemd service",
            )?;
            fs::remove_file(&service_path)
                .with_context(|| format!("failed to remove {}", service_path.display()))?;
            run_systemctl(
                ["--user", "daemon-reload"],
                "failed to reload systemd user units",
            )?;
        }
        Ok(())
    }

    fn install_shell_hook(&self, sidequest_root: &Path) -> Result<()> {
        let bashrc = home_dir()?.join(".bashrc");
        let existing = fs::read_to_string(&bashrc).unwrap_or_default();
        if existing.contains(BASH_BEGIN) {
            return Ok(());
        }

        let hook = format!(
            "\n{begin}\nif [ -f \"{latest}\" ]; then\n  if [ ! -f \"{seen}\" ] || [ \"{latest}\" -nt \"{seen}\" ]; then\n    cat \"{latest}\"\n    touch \"{seen}\"\n  fi\nfi\n{end}\n",
            begin = BASH_BEGIN,
            end = BASH_END,
            latest = sidequest_root.join("latest_harvest").display(),
            seen = sidequest_root.join(".harvest_seen").display(),
        );
        fs::write(&bashrc, format!("{existing}{hook}"))
            .with_context(|| format!("failed to update {}", bashrc.display()))?;
        Ok(())
    }

    fn uninstall_shell_hook(&self, _sidequest_root: &Path) -> Result<()> {
        let bashrc = home_dir()?.join(".bashrc");
        if !bashrc.exists() {
            return Ok(());
        }

        let contents = fs::read_to_string(&bashrc)
            .with_context(|| format!("failed to read {}", bashrc.display()))?;
        let stripped = remove_block(&contents, BASH_BEGIN, BASH_END);
        if stripped != contents {
            fs::write(&bashrc, stripped)
                .with_context(|| format!("failed to update {}", bashrc.display()))?;
        }
        Ok(())
    }
}

fn systemd_user_dir() -> Result<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(xdg).join("systemd/user"));
    }
    Ok(home_dir()?.join(".config/systemd/user"))
}

fn linux_path_env() -> Result<String> {
    build_path_env(&["/usr/local/bin", "/usr/bin", "/bin", "/usr/sbin", "/sbin"])
}

fn run_systemctl<const N: usize>(args: [&str; N], context: &str) -> Result<()> {
    let output = Command::new("systemctl")
        .args(args)
        .output()
        .with_context(|| format!("{context}: could not run systemctl"))?;
    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    bail!("{context}: {}", stderr.trim());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_path_env_includes_standard_locations() {
        let path_env = linux_path_env().expect("path env");

        assert!(path_env.contains("/usr/local/bin"));
        assert!(path_env.contains("/usr/bin"));
        assert!(path_env.contains("/bin"));
        assert!(path_env.contains("/.local/bin"));
        assert!(path_env.contains("/.cargo/bin"));
    }

    #[test]
    fn systemd_service_unit_path_respects_xdg() {
        // When XDG_CONFIG_HOME is set, systemd_user_dir uses it
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", "/tmp/test-xdg-config");
        }
        let dir = systemd_user_dir().expect("systemd user dir");
        assert_eq!(dir, PathBuf::from("/tmp/test-xdg-config/systemd/user"));
        unsafe {
            std::env::remove_var("XDG_CONFIG_HOME");
        }
    }

    #[test]
    fn remove_block_strips_sidequest_section() {
        let input = "before\n# >>> sidequest >>>\nstuff\n# <<< sidequest <<<\nafter\n";
        let result = remove_block(input, BASH_BEGIN, BASH_END);
        assert!(!result.contains("sidequest"));
        assert!(result.contains("before"));
        assert!(result.contains("after"));
    }

    #[test]
    fn remove_block_no_op_when_absent() {
        let input = "just some content\n";
        let result = remove_block(input, BASH_BEGIN, BASH_END);
        assert_eq!(result, input);
    }
}
