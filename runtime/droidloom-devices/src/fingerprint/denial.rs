//! Verification through Denial (`protocol/denial-fingerprint-verify-v1.xml`):
//! Denial runs the reader as it does for its lock screen, lights the panel
//! and marks the sensor, so Android shows no prompt of its own. Other
//! compositors lack the global and the broker asks fprintd itself.

use std::os::fd::{AsFd, OwnedFd};

use async_io::Async;
use droidloom_device_protocol::Scan;
use futures_lite::future;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum};

mod protocol {
    #![allow(
        dead_code,
        non_camel_case_types,
        non_upper_case_globals,
        non_snake_case,
        unused_imports,
        unused_unsafe,
        unused_variables,
        clippy::all,
        missing_docs
    )]
    use wayland_client;
    use wayland_client::protocol::*;
    pub mod __interfaces {
        use wayland_client::backend as wayland_backend;
        use wayland_client::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("protocol/denial-fingerprint-verify-v1.xml");
    }
    use self::__interfaces::*;
    wayland_scanner::generate_client_code!("protocol/denial-fingerprint-verify-v1.xml");
}

use protocol::denial_fingerprint_verification_v1::{
    self as verification, DenialFingerprintVerificationV1,
};
use protocol::denial_fingerprint_verifier_v1::DenialFingerprintVerifierV1;

#[derive(Default)]
struct State {
    /// Rejected fingers not yet reported.
    rejected: u32,
    done: Option<Scan>,
}

impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: wayland_client::protocol::wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

wayland_client::delegate_noop!(State: ignore DenialFingerprintVerifierV1);

impl Dispatch<DenialFingerprintVerificationV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &DenialFingerprintVerificationV1,
        event: verification::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            verification::Event::Ready => {}
            verification::Event::Rejected => state.rejected += 1,
            verification::Event::Done { outcome } => {
                state.done = Some(match outcome {
                    WEnum::Value(verification::Outcome::Match) => Scan::Match,
                    WEnum::Value(verification::Outcome::Cancelled) => Scan::Cancelled,
                    _ => Scan::Unavailable,
                });
            }
        }
    }
}

/// One verification through Denial, reported as the fprintd path reports
/// it, a match included. None when no compositor offers the global.
pub(super) fn verify(
    cancel: &async_channel::Receiver<()>,
    report: &dyn Fn(Scan) -> bool,
) -> Option<Scan> {
    let connection = Connection::connect_to_env().ok()?;
    let (globals, mut queue) = registry_queue_init::<State>(&connection).ok()?;
    let handle = queue.handle();
    let verifier: DenialFingerprintVerifierV1 = globals.bind(&handle, 1..=1, ()).ok()?;
    let verification = verifier.verify(&handle, ());
    verifier.destroy();
    let mut state = State::default();
    let scan = run(&connection, &mut queue, &mut state, cancel, report).unwrap_or_else(|error| {
        eprintln!("droidloom-devices: Denial fingerprint verification failed: {error}");
        Scan::Unavailable
    });
    if state.done.is_none() {
        verification.destroy();
        let _ = connection.flush();
    }
    if scan == Scan::Match {
        report(scan);
    }
    Some(scan)
}

fn run(
    connection: &Connection,
    queue: &mut wayland_client::EventQueue<State>,
    state: &mut State,
    cancel: &async_channel::Receiver<()>,
    report: &dyn Fn(Scan) -> bool,
) -> Result<Scan, Box<dyn std::error::Error>> {
    let fd: OwnedFd = connection.as_fd().try_clone_to_owned()?;
    let readable = Async::new(fd)?;
    loop {
        queue.dispatch_pending(state)?;
        if let Some(scan) = state.done {
            return Ok(scan);
        }
        for _ in 0..std::mem::take(&mut state.rejected) {
            // Android stopped listening.
            if !report(Scan::NoMatch) {
                return Ok(Scan::Cancelled);
            }
        }
        queue.flush()?;
        let Some(guard) = queue.prepare_read() else {
            continue;
        };
        let cancelled = future::block_on(future::or(
            async {
                let _ = readable.readable().await;
                false
            },
            async {
                let _ = cancel.recv().await;
                true
            },
        ));
        if cancelled {
            return Ok(Scan::Cancelled);
        }
        match guard.read() {
            Ok(_) => {}
            Err(wayland_client::backend::WaylandError::Io(error))
                if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(error.into()),
        }
    }
}
