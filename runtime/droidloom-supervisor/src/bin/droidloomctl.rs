//! User-facing control client for the one warm Droidloom Android cell.

#![forbid(unsafe_code)]

use std::{
    env, fs,
    io::Write,
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};

use clap::{Parser, Subcommand, ValueEnum};
use droidloom_supervisor::boot_notice::BootNotice;
use droidloom_supervisor::control::{
    ControlRequest, DEFAULT_CELL_SPEC, DEFAULT_CONTROL_SOCKET, request, request_with_progress,
};
use droidloom_window_policy::{
    LogicalSize, SessionMode, WindowPolicyPaths, WindowPolicyStore, WindowPreference,
};

#[derive(Debug, Parser)]
#[command(name = "droidloomctl", version, about)]
struct Cli {
    /// Droidloom lifecycle socket.
    #[arg(long, global = true, default_value = DEFAULT_CONTROL_SOCKET)]
    socket: PathBuf,
    /// Control only the Android cell (internal service/recovery operation).
    #[arg(long, global = true, hide = true)]
    cell: bool,
    /// Emit the complete machine-readable response.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Install a standalone APK, updating an existing app while retaining its data.
    Install {
        /// Host path to a standalone APK (split APK bundles are unsupported).
        apk: PathBuf,
        /// Android user identifier.
        #[arg(long, default_value_t = 0)]
        user: u32,
    },
    /// Write Android's whole storage to FILE, or to standard output for -.
    /// Android stops while it runs and starts again after.
    Backup {
        /// New archive file (never overwritten), or - for standard output.
        file: PathBuf,
        /// Cell specification.
        #[arg(long, default_value = DEFAULT_CELL_SPEC)]
        spec: PathBuf,
    },
    /// Replace Android's storage with a backup from FILE, or from standard
    /// input for -. Needs root. The replaced storage stays on disk beside it.
    Restore {
        /// Archive written by backup, or - for standard input.
        file: PathBuf,
        /// Cell specification.
        #[arg(long, default_value = DEFAULT_CELL_SPEC)]
        spec: PathBuf,
    },
    /// Update a source installation, or show pacman package-update instructions.
    Update {
        /// Discard updater-owned build outputs before rebuilding.
        #[arg(long)]
        clean: bool,
        /// Build and verify without activating the result.
        #[arg(long)]
        build_only: bool,
    },
    /// Start Droidloom, including Android, native windows, and the app catalog.
    Start {
        /// Return after starting the services, without waiting for Android readiness.
        #[arg(long)]
        no_wait: bool,
        /// Window mode: desktop (default) or mobile (fill available space).
        #[arg(long, default_value = "desktop")]
        mode: SessionMode,
        /// Cell specification.
        #[arg(long, default_value = DEFAULT_CELL_SPEC)]
        spec: PathBuf,
    },
    /// Stop Droidloom and release its runtime resources.
    Stop,
    /// Restart the complete Droidloom runtime.
    Restart {
        /// Return after restarting the services, without waiting for Android readiness.
        #[arg(long)]
        no_wait: bool,
        /// Window mode; omitted preserves the current session mode.
        #[arg(long)]
        mode: Option<SessionMode>,
        /// Cell specification.
        #[arg(long, default_value = DEFAULT_CELL_SPEC)]
        spec: PathBuf,
    },
    /// Keep the cell running for the session recovery service.
    #[command(hide = true)]
    Supervise {
        /// Authorized cell specification.
        #[arg(long, default_value = DEFAULT_CELL_SPEC)]
        spec: PathBuf,
    },
    /// Report whether the cell is running.
    Status,
    /// Wait for Android boot, showing progress and a deadline without restarting it.
    Wait,
    /// Show recent Android logs, optionally filtered by an installed app's UID.
    Logs {
        /// Android package, including retained logs after its process exits.
        package: Option<String>,
        /// Number of recent logcat entries (output is also byte bounded).
        #[arg(short = 'n', long, default_value_t = 200, value_parser = clap::value_parser!(u32).range(1..=2000))]
        lines: u32,
        /// Android user whose package UID should be selected.
        #[arg(long, default_value_t = 0)]
        user: u32,
    },
    /// Show the global crash buffer and recorded exits for an optional package.
    Crashes {
        /// Select this package's exit history; crash traces still include all apps.
        package: Option<String>,
        /// Number of recent crash logcat entries (output is also byte bounded).
        #[arg(short = 'n', long, default_value_t = 1000, value_parser = clap::value_parser!(u32).range(1..=2000))]
        lines: u32,
    },
    /// Launch one Android application in the boot-managed runtime.
    Launch {
        /// Android package name.
        package: String,
        /// Optional flattened PACKAGE/ACTIVITY component.
        #[arg(long)]
        component: Option<String>,
        /// Restart this app with initial Android task size WIDTHxHEIGHT pixels.
        #[arg(long, value_parser = droidloom_supervisor::control::parse_launch_resolution)]
        resolution: Option<String>,
        /// Android user identifier.
        #[arg(long, default_value_t = 0)]
        user: u32,
        /// Expected cell specification.
        #[arg(long, default_value = DEFAULT_CELL_SPEC)]
        spec: PathBuf,
    },
    /// List Android applications exported to the native launcher.
    Applications {
        /// Android user identifier.
        #[arg(long, default_value_t = 0)]
        user: u32,
    },
    /// Persist Android's built-in-display density, warming the runtime if needed.
    Dpi {
        /// Android density in dots per inch.
        dpi: u32,
        /// Cell specification used when the runtime is cold.
        #[arg(long, default_value = DEFAULT_CELL_SPEC)]
        spec: PathBuf,
    },
    /// List the host's GPUs, or choose the one Android renders on. An NVIDIA
    /// GPU renders through GPU forwarding, AMD and Intel natively.
    Gpu {
        /// auto (NVIDIA when present), amd, intel, nvidia, qualcomm or a PCI address.
        choice: Option<droidloom_supervisor::host_gpu::GpuChoice>,
    },
    /// Persist how size-less initial Wayland configures are resolved.
    WindowMode {
        /// Mobile work-area or fixed windowed policy.
        mode: WindowMode,
        /// Logical width required by windowed mode.
        #[arg(long)]
        width: Option<u32>,
        /// Logical height required by windowed mode.
        #[arg(long)]
        height: Option<u32>,
        /// Apply only to this Android package instead of changing the default.
        #[arg(long)]
        package: Option<String>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum WindowMode {
    /// Fill the standard XDG bounds or logical output extent.
    FitOutput,
    /// Use an explicit logical size, constrained by XDG bounds.
    Windowed,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("droidloomctl: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    if let Command::Gpu { choice } = &cli.command {
        return gpu(choice.as_ref());
    }
    if !cli.cell && cli.socket == PathBuf::from(DEFAULT_CONTROL_SOCKET) {
        let operation = match &cli.command {
            Command::Start {
                spec,
                mode,
                no_wait,
            } if spec == &PathBuf::from(DEFAULT_CELL_SPEC) => {
                Some(("start", Some(*mode), *no_wait))
            }
            Command::Stop => Some(("stop", None, true)),
            Command::Restart {
                spec,
                mode,
                no_wait,
            } if spec == &PathBuf::from(DEFAULT_CELL_SPEC) => Some(("restart", *mode, *no_wait)),
            _ => None,
        };
        if let Some((operation, mode, no_wait)) = operation {
            return session_lifecycle(operation, cli.json, mode, no_wait);
        }
        if matches!(cli.command, Command::Status) && !cli.socket.exists() {
            if cli.json {
                println!(
                    "{}",
                    serde_json::json!({"ok": true, "message": "Droidloom is stopped", "state": "stopped"})
                );
            } else {
                println!("Droidloom is stopped");
            }
            return Ok(());
        }
    }
    let mut report = |message: &str| {
        if !cli.json {
            eprintln!("{message}");
        }
    };
    if let Command::Backup { file, spec } | Command::Restore { file, spec } = &cli.command {
        return storage(&cli, file, spec, &mut report);
    }
    let wait_after_start = !cli.cell
        && matches!(
            cli.command,
            Command::Start { no_wait: false, .. } | Command::Restart { no_wait: false, .. }
        );
    let mut response = if let Command::Install { apk, user } = &cli.command {
        droidloom_supervisor::control::install_apk_with_progress(
            &cli.socket,
            apk,
            *user,
            &mut report,
        )?
    } else {
        let control_request = match cli.command {
            Command::Install { .. } => unreachable!("handled with its APK file descriptor"),
            Command::Backup { .. } | Command::Restore { .. } => {
                unreachable!("handled with the archive's file descriptor")
            }
            Command::Gpu { .. } => unreachable!("handled by package setup"),
            Command::Update { clean, build_only } => {
                if PathBuf::from("/usr/share/droidloom/package.json").is_file() {
                    return Err("Droidloom is managed by pacman. Install the new package pair with sudo pacman -U, or build it with cargo run --locked -j 1 -p droidloom-package -- build. Sudo is needed only to replace package-owned system files and update pacman's database.".into());
                }
                let mut command = droidloom_cpu_placement::command("/usr/bin/droidloom-update");
                if clean {
                    command.arg("--clean");
                }
                if build_only {
                    command.arg("--build-only");
                }
                let status = command.status()?;
                if !status.success() {
                    return Err(format!("update failed: {status}").into());
                }
                return Ok(());
            }
            Command::Start { spec, .. } => ControlRequest::Start { spec },
            Command::Stop => ControlRequest::Stop,
            Command::Restart { spec, .. } => ControlRequest::Restart { spec },
            Command::Supervise { spec } => {
                return Ok(droidloom_supervisor::control::supervise_cell(
                    &cli.socket,
                    &spec,
                    BootNotice::current(),
                )?);
            }
            Command::Status => ControlRequest::Status,
            Command::Wait => ControlRequest::WaitReady,
            Command::Logs {
                package,
                lines,
                user,
            } => ControlRequest::Diagnostics {
                package,
                lines,
                user,
                crashes: false,
            },
            Command::Crashes { package, lines } => ControlRequest::Diagnostics {
                package,
                lines,
                user: 0,
                crashes: true,
            },
            Command::Launch {
                package,
                component,
                resolution,
                user,
                spec,
            } => ControlRequest::Launch {
                spec,
                package,
                component,
                resolution,
                user,
            },
            Command::Applications { user } => ControlRequest::ListApplications { user },
            Command::Dpi { dpi, spec } => ControlRequest::SetDpi { spec, dpi },
            Command::WindowMode {
                mode,
                width,
                height,
                package,
            } => {
                let preference = match (mode, width, height) {
                    (WindowMode::FitOutput, None, None) => WindowPreference::fit_output(),
                    (WindowMode::Windowed, Some(width), Some(height)) => {
                        WindowPreference::windowed(LogicalSize::new(width, height)?)
                    }
                    (WindowMode::FitOutput, _, _) => {
                        return Err("fit-output mode does not accept --width or --height".into());
                    }
                    (WindowMode::Windowed, _, _) => {
                        return Err("windowed mode requires both --width and --height".into());
                    }
                };
                let paths = WindowPolicyPaths::from_environment(
                    env::var_os("XDG_CONFIG_HOME"),
                    env::var_os("XDG_STATE_HOME"),
                    env::var_os("HOME"),
                )?;
                let mut store = WindowPolicyStore::load(paths)?;
                store.set_preference(package.as_deref(), preference)?;
                if cli.json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "ok": true,
                            "mode": match mode {
                                WindowMode::FitOutput => "fit_output",
                                WindowMode::Windowed => "windowed",
                            },
                            "width": width,
                            "height": height,
                            "package": package,
                        }))?
                    );
                } else if let Some(package) = package {
                    println!("set persistent Droidloom window policy for {package}");
                } else {
                    println!("set persistent default Droidloom window policy");
                }
                return Ok(());
            }
        };
        if cli.cell {
            // The session's start shows the boot notice; recovery closes it.
            let notice = matches!(control_request, ControlRequest::Start { .. })
                .then(BootNotice::starting);
            let response = request(&cli.socket, &control_request);
            if let Some(notice) = notice {
                match &response {
                    Ok(response) if response.ok => {}
                    Ok(response) => notice.failed(&response.message),
                    Err(error) => notice.failed(&error.to_string()),
                }
            }
            response?
        } else {
            request_with_progress(&cli.socket, &control_request, &mut report)?
        }
    };
    if response.ok && wait_after_start {
        response = request_with_progress(&cli.socket, &ControlRequest::WaitReady, &mut report)?;
    }
    if cli.json {
        println!("{}", serde_json::to_string_pretty(&response)?);
    } else if response.ok {
        if let Some(diagnostics) = &response.diagnostics {
            print!("{diagnostics}");
            if !diagnostics.ends_with('\n') {
                println!();
            }
        } else {
            println!("{}", response.message);
        }
        if let Some(launch) = &response.launch
            && !launch.is_empty()
        {
            println!("{launch}");
        }
        if let Some(applications) = &response.applications {
            for application in applications {
                println!("{}\t{}", application.name, application.component);
            }
        }
    }
    if response.ok {
        Ok(())
    } else {
        Err(response.message.into())
    }
}

/// The archive travels as a descriptor this client opened, so droidloomd
/// writes or reads only what the caller may. Standard output carries the
/// archive for `backup -`, so every message goes to standard error.
fn storage(
    cli: &Cli,
    path: &std::path::Path,
    spec: &std::path::Path,
    report: &mut dyn FnMut(&str),
) -> Result<(), Box<dyn std::error::Error>> {
    use std::io::IsTerminal;
    use std::os::fd::AsFd;
    let restore = matches!(cli.command, Command::Restore { .. });
    let stream = path == std::path::Path::new("-");
    let file = match (restore, stream) {
        (false, true) => {
            if std::io::stdout().is_terminal() {
                return Err("refusing to write the backup to a terminal".into());
            }
            fs::File::from(std::io::stdout().as_fd().try_clone_to_owned()?)
        }
        (true, true) => fs::File::from(std::io::stdin().as_fd().try_clone_to_owned()?),
        (false, false) => fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .map_err(|error| format!("create {}: {error}", path.display()))?,
        (true, false) => {
            fs::File::open(path).map_err(|error| format!("open {}: {error}", path.display()))?
        }
    };
    let request = if restore {
        ControlRequest::Restore { spec: spec.into() }
    } else {
        ControlRequest::Backup { spec: spec.into() }
    };
    let response =
        droidloom_supervisor::control::storage_with_progress(&cli.socket, &request, &file, report)?;
    if cli.json {
        eprintln!("{}", serde_json::to_string_pretty(&response)?);
    } else if response.ok {
        eprintln!("{}", response.message);
    }
    if response.ok {
        Ok(())
    } else {
        Err(response.message.into())
    }
}

/// GPU choice is package setup's: it rewrites the root-owned configuration.
fn gpu(choice: Option<&droidloom_supervisor::host_gpu::GpuChoice>) -> Result<(), Box<dyn std::error::Error>> {
    if !PathBuf::from("/usr/share/droidloom/package.json").is_file() {
        return Err("choosing the GPU needs the pacman package; a source installation reads \"gpu\" from /etc/droidloom/cell.json at droidloomctl update".into());
    }
    let mut command = droidloom_cpu_placement::command("/usr/lib/droidloom/droidloom-package-helper");
    command.arg("gpu");
    if let Some(choice) = choice {
        command.arg(choice.to_string());
    }
    if !command.status()?.success() {
        return Err("the GPU choice was not changed".into());
    }
    Ok(())
}

fn session_lifecycle(
    operation: &str,
    json: bool,
    requested_mode: Option<SessionMode>,
    no_wait: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if operation != "stop" && PathBuf::from("/usr/share/droidloom/package.json").is_file() {
        let status =
            droidloom_cpu_placement::command("/usr/lib/droidloom/droidloom-package-helper")
                .arg("prepare")
                .status()?;
        if !status.success() {
            return Err("Droidloom setup did not complete; the runtime was not started".into());
        }
        let status = droidloom_cpu_placement::command("systemctl")
            .args(["--user", "daemon-reload"])
            .status()?;
        if !status.success() {
            return Err("could not reload the installed Droidloom user service".into());
        }
    }
    let mut operation = operation;
    if operation != "stop" {
        let runtime =
            PathBuf::from(env::var_os("XDG_RUNTIME_DIR").ok_or("XDG_RUNTIME_DIR is missing")?);
        if !runtime.is_absolute() {
            return Err("XDG_RUNTIME_DIR must be absolute".into());
        }
        let directory = runtime.join("droidloom");
        match fs::DirBuilder::new().mode(0o700).create(&directory) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        let metadata = fs::symlink_metadata(&directory)?;
        if !metadata.is_dir() || metadata.uid() != fs::metadata(&runtime)?.uid() {
            return Err("Droidloom runtime directory must be a session-owned directory".into());
        }
        // Older clients created this directory with the default 0755 mode.
        // Keep the presenter's private-socket directory invariant on every start.
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
        let path = directory.join("session.env");
        let previous = match fs::read_to_string(&path) {
            Ok(value) => value
                .trim()
                .strip_prefix("DROIDLOOM_MODE=")
                .ok_or("invalid session mode file")?
                .parse::<SessionMode>()?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => SessionMode::Desktop,
            Err(error) => return Err(error.into()),
        };
        let mode = requested_mode.unwrap_or(previous);
        // A start with a different mode must actually change the running service.
        if operation == "start"
            && mode != previous
            && droidloom_cpu_placement::command("systemctl")
                .args(["--user", "is-active", "--quiet", "droidloom.service"])
                .status()?
                .success()
        {
            operation = "restart";
        }
        let temporary = directory.join(format!(".session-{}.env", std::process::id()));
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        writeln!(file, "DROIDLOOM_MODE={mode}")?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
    }
    // Import the caller's live session without requiring a logout after install.
    if operation != "stop" {
        let variables: Vec<_> = ["WAYLAND_DISPLAY", "DISPLAY", "XDG_CURRENT_DESKTOP"]
            .into_iter()
            .filter(|name| env::var_os(name).is_some())
            .collect();
        if !variables.is_empty() {
            let status = droidloom_cpu_placement::command("systemctl")
                .args(["--user", "import-environment"])
                .args(variables)
                .status()?;
            if !status.success() {
                return Err("could not import the graphical session environment".into());
            }
        }
    }
    if !json {
        eprintln!("Waiting for Droidloom services to {operation} (up to 160 seconds)...");
    }
    let mut child = droidloom_cpu_placement::command("systemctl")
        .args(["--user", operation, "droidloom.service"])
        .spawn()?;
    let started = Instant::now();
    let mut next_update = Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() >= Duration::from_secs(160) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("Droidloom services did not {operation} within 160 seconds. The systemd job may still be running. Check the logs with:\n  journalctl --user -b -u droidloom.service -n 100 --no-pager\n  journalctl -b -u droidloomd.service -n 200 --no-pager").into());
        }
        if started.elapsed() >= next_update {
            if !json {
                eprintln!(
                    "Waiting for Droidloom services to {operation} ({} seconds elapsed; {} seconds remaining)...",
                    started.elapsed().as_secs(),
                    160 - started.elapsed().as_secs()
                );
            }
            next_update += Duration::from_secs(10);
        }
        thread::sleep(Duration::from_millis(100));
    };
    if !status.success() {
        return Err(
            format!("{operation} failed; see journalctl --user -u droidloom.service").into(),
        );
    }
    let message = if operation == "stop" {
        "Droidloom is stopped"
    } else if no_wait {
        "Droidloom services are running; Android readiness has not been checked. Use `droidloomctl wait` to follow boot progress."
    } else {
        let response = request_with_progress(
            &PathBuf::from(DEFAULT_CONTROL_SOCKET),
            &ControlRequest::WaitReady,
            &mut |message| {
                if !json {
                    eprintln!("{message}");
                }
            },
        )?;
        if !response.ok {
            return Err(response.message.into());
        }
        "Droidloom is running; Android is ready"
    };
    if json {
        println!(
            "{}",
            serde_json::json!({"ok": true, "message": message, "state": if operation == "stop" { "stopped" } else { "running" }})
        );
    } else {
        println!("{message}");
    }
    Ok(())
}
