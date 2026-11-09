//! Android's night mode, which follows the host's dark or light theme.
//! `UiModeManager` keeps the setting across restarts, so one change is enough.

use std::process::Stdio;

use super::{ControlError, diagnostics};

pub(super) fn set_night(pid: u32, night: bool) -> Result<(), ControlError> {
    let value = if night { "yes" } else { "no" };
    let mut command = droidloom_cpu_placement::command("/usr/bin/timeout");
    command
        .args(["--kill-after=1s", "10s", "/usr/bin/nsenter", "--target"])
        .arg(pid.to_string())
        .args([
            "--mount", "--uts", "--ipc", "--net", "--pid", "--cgroup", "--root", "--wd", "--env",
            "--", "/system/bin/cmd", "uimode", "night", value,
        ]);
    let output = diagnostics::capture_with_stdin(&mut command, Stdio::null())?;
    check_result(&output, value)
}

fn check_result(output: &str, value: &str) -> Result<(), ControlError> {
    if output.lines().any(|line| line.trim() == format!("Night mode: {value}")) {
        Ok(())
    } else {
        Err(ControlError::Invalid(format!(
            "Android did not confirm night mode {value}: {}",
            output.trim()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::check_result;

    #[test]
    fn only_the_asked_mode_is_confirmed() {
        assert!(check_result("Night mode: yes\n", "yes").is_ok());
        assert!(check_result("Night mode: no\n", "yes").is_err());
        assert!(check_result("Error: unknown command\n", "no").is_err());
    }
}
