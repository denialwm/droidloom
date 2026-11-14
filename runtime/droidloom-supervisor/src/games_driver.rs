//! The OpenGL driver Android games use: ANGLE over the Vulkan driver, or
//! Mesa's own. Android gives ANGLE to apps of the game category while
//! `debug.graphics.angle.force_enable_angle_for_games` is 1; the cell's init
//! sets it from the persistent choice at every start, and the image's default
//! stands until a choice is made. Games take it when they next start.

use std::process::Stdio;

use super::{ControlError, GlDriver, diagnostics};

const CHOICE: &str = "persist.vendor.droidloom.games_gl_driver";
const SWITCH: &str = "debug.graphics.angle.force_enable_angle_for_games";

/// The driver games start with now.
pub(super) fn read(pid: u32) -> Result<GlDriver, ControlError> {
    let value = in_cell(pid, &["/system/bin/getprop", SWITCH])?;
    Ok(driver_of(&value))
}

/// Keeps `driver` for games from now on, across restarts.
pub(super) fn set(pid: u32, driver: GlDriver) -> Result<(), ControlError> {
    let (choice, switch) = match driver {
        GlDriver::Angle => ("angle", "1"),
        GlDriver::Mesa => ("mesa", "0"),
    };
    in_cell(pid, &["/system/bin/setprop", CHOICE, choice])?;
    in_cell(pid, &["/system/bin/setprop", SWITCH, switch])?;
    let now = read(pid)?;
    if now == driver {
        Ok(())
    } else {
        Err(ControlError::Invalid(format!(
            "Android kept games on {now:?} instead of {driver:?}"
        )))
    }
}

fn driver_of(switch: &str) -> GlDriver {
    if switch.trim() == "1" {
        GlDriver::Angle
    } else {
        GlDriver::Mesa
    }
}

fn in_cell(pid: u32, command: &[&str]) -> Result<String, ControlError> {
    let mut nsenter = droidloom_cpu_placement::command("/usr/bin/timeout");
    nsenter
        .args(["--kill-after=1s", "10s", "/usr/bin/nsenter", "--target"])
        .arg(pid.to_string())
        .args([
            "--mount", "--uts", "--ipc", "--net", "--pid", "--cgroup", "--root", "--wd", "--env",
            "--",
        ])
        .args(command);
    diagnostics::capture_with_stdin(&mut nsenter, Stdio::null())
}

#[cfg(test)]
mod tests {
    use super::{GlDriver, driver_of};

    #[test]
    fn only_a_set_switch_means_angle() {
        assert_eq!(driver_of("1\n"), GlDriver::Angle);
        assert_eq!(driver_of("0\n"), GlDriver::Mesa);
        assert_eq!(driver_of("\n"), GlDriver::Mesa);
    }
}
