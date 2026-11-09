//! User-session Android application catalog and XDG launcher reconciler.

#![forbid(unsafe_code)]

use droidloom_supervisor::control::subscribe_applications;
use std::env;
use std::io::Read;
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use clap::Parser;
use droidloom_applications::{CatalogPaths, ControlCatalog, reconcile, remove_managed};
use droidloom_supervisor::control::DEFAULT_CONTROL_SOCKET;

mod theme;

#[derive(Debug, Parser)]
#[command(name = "droidloom-applications", version, about)]
struct Cli {
    /// Remove only entries recorded in Droidloom's catalog; Android need not be running.
    #[arg(long)]
    remove_managed: bool,
    /// Droidloom lifecycle socket.
    #[arg(long, default_value = DEFAULT_CONTROL_SOCKET)]
    socket: PathBuf,
    /// Android user whose launchable activities are exported.
    #[arg(long, default_value_t = 0)]
    user: u32,
    /// Reconcile once and exit instead of watching for package changes.
    #[arg(long)]
    once: bool,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("droidloom-applications: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    eprintln!("{}", droidloom_supervisor::control::FRAMEWORK_CONTROL_ABI);
    let cli = Cli::parse();
    droidloom_cpu_placement::initialize(droidloom_cpu_placement::Role::Background);
    let paths = CatalogPaths::from_environment(
        env::var_os("XDG_DATA_HOME"),
        env::var_os("XDG_STATE_HOME"),
        env::var_os("HOME"),
    )?;
    if cli.remove_managed {
        let count = remove_managed(&paths)?;
        println!("Removed {count} Droidloom launcher/icon entries; Android data retained.");
        return Ok(());
    }
    let mut catalog = ControlCatalog::new(cli.socket.clone(), cli.user);

    if cli.once {
        reconcile(&paths, &mut catalog)?;
        return Ok(());
    }

    theme::follow(cli.socket.clone());
    loop {
        let mut events = match subscribe_applications(&cli.socket) {
            Ok(stream) => stream,
            Err(error) => {
                eprintln!("droidloom-applications: subscription deferred: {error}");
                thread::sleep(Duration::from_secs(2));
                continue;
            }
        };
        loop {
            // The initial generation forces a full reconciliation on every reconnect.
            // A healthy idle catalog sleeps here with no periodic polling or ART startup.
            let mut generation = [0; 8];
            if events.read_exact(&mut generation).is_err() {
                break;
            }
            loop {
                match reconcile(&paths, &mut catalog) {
                    Ok(summary) => {
                        if summary.changed() {
                            eprintln!(
                                "droidloom-applications: {} launchers, {} icons updated, {} stale entries removed",
                                summary.applications,
                                summary.icons_updated,
                                summary.entries_removed
                            );
                        }
                        if !summary.retry_needed {
                            break;
                        }
                    }
                    Err(error) => {
                        // Droidloom is boot-managed independently of this user helper.
                        // A temporary Android restart must not erase the last good XDG
                        // catalog or turn the helper into a service failure loop.
                        eprintln!("droidloom-applications: catalog refresh deferred: {error}");
                    }
                }
                // Backoff is used only for failed reconciliation, never a healthy idle refresh.
                thread::sleep(Duration::from_secs(2));
            }
        }
        thread::sleep(Duration::from_secs(2));
    }
}
