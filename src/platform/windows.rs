use std::path::Path;

use anyhow::{Result, bail};

use crate::platform::{Platform, SleepGuard};

pub struct WindowsPlatform;

impl Platform for WindowsPlatform {
    fn name(&self) -> &'static str {
        "windows"
    }

    fn read_credential(&self, _service: &str) -> Result<Option<String>> {
        Ok(None)
    }

    fn send_notification(&self, _title: &str, _body: &str) -> Result<()> {
        Ok(())
    }

    fn begin_sleep_prevention(&self, _reason: &str) -> Result<Option<Box<dyn SleepGuard>>> {
        Ok(None)
    }

    fn install_autostart(
        &self,
        _binary_path: &Path,
        _sidequest_root: &Path,
        _log_dir: &Path,
    ) -> Result<()> {
        bail!("autostart installation is not implemented on Windows yet")
    }

    fn uninstall_autostart(&self, _sidequest_root: &Path) -> Result<()> {
        Ok(())
    }

    fn install_shell_hook(&self, _sidequest_root: &Path) -> Result<()> {
        Ok(())
    }

    fn uninstall_shell_hook(&self, _sidequest_root: &Path) -> Result<()> {
        Ok(())
    }
}
