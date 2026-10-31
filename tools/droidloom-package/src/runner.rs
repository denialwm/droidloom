//! Manual, repository-scoped runner. No boot service or host administrator access.
use super::{Result, require_user, run};
use clap::Subcommand;
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

const ROOT: &str = "/mnt/puck/logix/droidloom";
const UNIT: &str = "droidloom-actions-runner";
const NAME: &str = "droidloom-puck";

#[derive(Subcommand)]
pub enum Action {
    /// Download the current official x64 runner into /mnt/puck (stopped).
    Setup,
    /// Register and start a runner for one job; run before pushing main.
    Start,
    /// Stop the runner and remove its GitHub registration; keep build caches.
    Stop,
    /// Stop remaining containers in this job's package workspace.
    Cleanup,
    /// Show the user service status and GitHub registration.
    Status,
}

fn api(endpoint: &str, method: &str) -> Result<serde_json::Value> {
    let output = Command::new("gh")
        .args([
            "api",
            "--method",
            method,
            &format!("repos/denialwm/droidloom/{endpoint}"),
        ])
        .output()?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).into_owned().into());
    }
    if output.stdout.is_empty() {
        return Ok(serde_json::Value::Null);
    }
    Ok(serde_json::from_slice(&output.stdout)?)
}

fn active() -> bool {
    Command::new("systemctl")
        .args(["--user", "is-active", "--quiet", UNIT])
        .status()
        .is_ok_and(|s| s.success())
}

fn unregister(directory: &Path) -> Result<()> {
    let runners = api("actions/runners", "GET")?;
    for runner in runners["runners"]
        .as_array()
        .ok_or("missing runners list")?
    {
        if runner["name"] == NAME {
            let id = runner["id"].as_u64().ok_or("missing runner ID")?;
            api(&format!("actions/runners/{id}"), "DELETE")?;
        }
    }
    for name in [".runner", ".credentials", ".credentials_rsaparams"] {
        let file = directory.join(name);
        if file.exists() {
            fs::remove_file(file)?;
        }
    }
    Ok(())
}

pub fn execute(action: Action) -> Result<()> {
    require_user()?;
    let root = PathBuf::from(ROOT);
    let directory = root.join("runner");
    match action {
        Action::Setup => {
            if active() {
                return Err("stop the runner before setup".into());
            }
            fs::create_dir_all(&directory)?;
            let output = Command::new("gh")
                .args(["api", "repos/actions/runner/releases/latest"])
                .output()?;
            if !output.status.success() {
                return Err("cannot read official runner release".into());
            }
            let release: serde_json::Value = serde_json::from_slice(&output.stdout)?;
            let asset = release["assets"]
                .as_array()
                .ok_or("missing runner assets")?
                .iter()
                .find(|asset| {
                    asset["name"].as_str().is_some_and(|name| {
                        name.starts_with("actions-runner-linux-x64-") && name.ends_with(".tar.gz")
                    })
                })
                .ok_or("no Linux x64 runner")?;
            let url = asset["browser_download_url"]
                .as_str()
                .ok_or("missing runner URL")?;
            let archive = root.join("runner.tar.gz");
            run(Command::new("curl")
                .args(["--fail", "--location", "--retry", "3", "--output"])
                .arg(&archive)
                .arg(url))?;
            // GitHub supplies a digest with the authenticated download metadata.
            let expected = asset["digest"]
                .as_str()
                .and_then(|digest| digest.strip_prefix("sha256:"))
                .ok_or("missing runner checksum")?;
            let output = Command::new("sha256sum").arg(&archive).output()?;
            if !output.status.success()
                || String::from_utf8_lossy(&output.stdout)
                    .split_whitespace()
                    .next()
                    != Some(expected)
            {
                return Err("runner download checksum mismatch".into());
            }
            run(Command::new("tar")
                .args(["-xzf"])
                .arg(&archive)
                .arg("-C")
                .arg(&directory))?;
            fs::remove_file(archive)?;
            println!("Runner installed and stopped in {}", directory.display());
        }
        Action::Start => {
            if active() {
                return Err("runner is already active".into());
            }
            if !directory.join("run.sh").is_file() {
                return Err("run runner setup first".into());
            }
            unregister(&directory)?;
            let token = api("actions/runners/registration-token", "POST")?;
            // Do not print the registration token or place it in the service environment.
            // The job also cross-compiles aarch64; fail here, not hours into it.
            if !root.join("sysroot/usr/lib/pkgconfig").is_dir() {
                return Err(format!(
                    "no ARM64 sysroot at {}/sysroot (an Arch Linux ARM root with the host programs' libraries)",
                    root.display()
                )
                .into());
            }
            let status = Command::new(directory.join("config.sh"))
                .current_dir(&directory)
                .args([
                    "--unattended",
                    "--ephemeral",
                    "--url",
                    "https://github.com/denialwm/droidloom",
                    "--name",
                    NAME,
                    "--labels",
                    "droidloom-builder",
                    "--work",
                ])
                .arg(root.join("work"))
                .arg("--token")
                .arg(
                    token["token"]
                        .as_str()
                        .ok_or("missing registration token")?,
                )
                .stdin(Stdio::null())
                .status()?;
            if !status.success() {
                return Err("runner registration failed".into());
            }
            // A transient user service survives this terminal, never starts at boot,
            // and cannot sit listening indefinitely if the intended push is forgotten.
            let result = run(Command::new("systemd-run")
                .args([
                    "--user",
                    "--collect",
                    "--unit",
                    UNIT,
                    "--property=RuntimeMaxSec=24h",
                    "--property=TimeoutStopSec=120",
                    "--property=KillMode=control-group",
                ])
                .arg(format!("--working-directory={}", directory.display()))
                .arg(format!("--setenv=PATH={}", std::env::var("PATH")?))
                .arg(format!(
                    "--setenv=DROIDLOOM_PACKAGE_WORK={}/cache/arch",
                    root.display()
                ))
                .arg(format!(
                    "--setenv=CARGO_TARGET_DIR={}/cache/host-target",
                    root.display()
                ))
                .arg(format!(
                    "--setenv=CARGO_HOME={}/cache/cargo-home",
                    root.display()
                ))
                .arg(format!(
                    "--setenv=DROIDLOOM_ARM64_SYSROOT={}/sysroot",
                    root.display()
                ))
                .arg(directory.join("run.sh")));
            if result.is_err() {
                unregister(&directory)?;
            }
            result?;
            println!("Runner armed for one job. Follow logs: journalctl --user -fu {UNIT}");
        }
        Action::Stop => {
            if active() {
                run(Command::new("systemctl").args(["--user", "stop", UNIT]))?;
            }
            unregister(&directory)?;
            let work = root.join("cache/arch");
            if work.join("containers").exists() {
                run(super::podman(&work).args(["stop", "--all", "--time", "10"]))?;
            }
            println!("Runner stopped and unregistered; caches retained in {ROOT}");
        }
        Action::Status => {
            println!(
                "Local runner: {}",
                if active() { "running" } else { "stopped" }
            );
            let runners = api("actions/runners", "GET")?;
            for runner in runners["runners"]
                .as_array()
                .ok_or("missing runners list")?
            {
                if runner["name"] == NAME {
                    println!("{runner}");
                }
            }
        }
        Action::Cleanup => {
            let repo = super::repository(None)?;
            let work = super::build_workspace(&repo)?;
            if work.join("containers").exists() {
                run(super::podman(&work).args(["stop", "--all", "--time", "10"]))?;
            }
        }
    }
    Ok(())
}
