//! Privileged single-cell Droidloom lifecycle daemon.

#![deny(unsafe_op_in_unsafe_fn)]

use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI32, Ordering};

use clap::Parser;
use droidloom_supervisor::control::{
    DEFAULT_CONTROL_SOCKET, DEFAULT_SPEC_DIRECTORY, DEFAULT_SUPERVISOR, DaemonConfig, Shutdown,
    serve,
};

static SHUTDOWN_FD: AtomicI32 = AtomicI32::new(-1);

#[derive(Debug, Parser)]
#[command(name = "droidloomd", version, about)]
struct Cli {
    /// Root-owned local lifecycle socket.
    #[arg(long, default_value = DEFAULT_CONTROL_SOCKET)]
    socket: PathBuf,
    /// Small supervisor executable providing the private namespace entry.
    #[arg(long, default_value = DEFAULT_SUPERVISOR)]
    supervisor: PathBuf,
    /// Root-owned cell-specification directory trusted for user requests.
    #[arg(long, default_value = DEFAULT_SPEC_DIRECTORY)]
    spec_directory: PathBuf,
}

extern "C" fn request_shutdown(_signal: libc::c_int) {
    // SAFETY: the initialized eventfd outlives installed handlers. write(2) is
    // async-signal-safe; preserve the interrupted operation's errno.
    unsafe {
        let saved = *libc::__errno_location();
        let value = 1_u64;
        libc::write(
            SHUTDOWN_FD.load(Ordering::Relaxed),
            (&raw const value).cast(),
            8,
        );
        *libc::__errno_location() = saved;
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("droidloomd: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    eprintln!("{}", droidloom_supervisor::control::FRAMEWORK_CONTROL_ABI);
    droidloom_cpu_placement::initialize(droidloom_cpu_placement::Role::Background);
    droidloom_supervisor::cgroup_delegation::enable_delegated_controllers();
    let shutdown = Shutdown::new()?;
    SHUTDOWN_FD.store(shutdown.as_raw_fd(), Ordering::Relaxed);
    install_signal_handlers()?;
    let cli = Cli::parse();
    let config = DaemonConfig {
        socket: cli.socket,
        supervisor: cli.supervisor,
        spec_directory: cli.spec_directory,
    };
    let result = serve(&config, &shutdown);
    SHUTDOWN_FD.store(-1, Ordering::Relaxed);
    result?;
    Ok(())
}

fn install_signal_handlers() -> Result<(), std::io::Error> {
    for signal in [libc::SIGTERM, libc::SIGINT] {
        let previous =
            unsafe { libc::signal(signal, request_shutdown as *const () as libc::sighandler_t) };
        if previous == libc::SIG_ERR {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}
