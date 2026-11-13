//! The host's screen lock for Android's credential prompts. Root's supervisor
//! connects once per prompt and asks with [`Message::CredentialAsk`]; the
//! broker shows Denial's sheet (`protocol/denial-user-verify-v1.xml`) and
//! hands each credential the user types back to the supervisor, which checks
//! it and answers. The broker judges nothing. Other compositors lack the
//! global, and the prompt ends as cancelled.

use std::collections::VecDeque;
use std::io;
use std::os::fd::{AsFd, AsRawFd};

use droidloom_denial_ipc::{IpcError, SeqPacket};
use droidloom_device_protocol::{MAX_RECORD, Message};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, WEnum};

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
        wayland_scanner::generate_interfaces!("protocol/denial-user-verify-v1.xml");
    }
    use self::__interfaces::*;
    wayland_scanner::generate_client_code!("protocol/denial-user-verify-v1.xml");
}

use protocol::denial_user_verification_v1::{self as verification, DenialUserVerificationV1};
use protocol::denial_user_verifier_v1::{Credential, DenialUserVerifierV1};

#[derive(Default)]
struct State {
    /// Credentials typed and not yet handed on: the PIN or not, and its bytes.
    typed: VecDeque<(bool, Vec<u8>)>,
    cancelled: bool,
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

wayland_client::delegate_noop!(State: ignore DenialUserVerifierV1);

impl Dispatch<DenialUserVerificationV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &DenialUserVerificationV1,
        event: verification::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            verification::Event::Credential { kind, secret } => {
                let pin = matches!(kind, WEnum::Value(Credential::Pin));
                state.typed.push_back((pin, secret.into_bytes()));
            }
            verification::Event::Cancelled => state.cancelled = true,
        }
    }
}

fn send(socket: &SeqPacket, message: &Message) -> bool {
    socket.send_record(&message.encode(), &[]).is_ok()
}

fn wipe(bytes: &mut [u8]) {
    for byte in bytes.iter_mut() {
        // SAFETY: a valid, aligned byte of a live buffer.
        unsafe { std::ptr::write_volatile(byte, 0) };
    }
}

/// Serves one prompt of root's supervisor.
pub fn serve(socket: &SeqPacket) -> io::Result<()> {
    let mut bytes = vec![0; MAX_RECORD];
    let ask = loop {
        match socket.receive_record_into(&mut bytes) {
            Ok(record) => break Message::decode(record.bytes).map_err(io::Error::other)?,
            Err(IpcError::PeerClosed) => return Ok(()),
            Err(IpcError::Io(error)) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(io::Error::other(error)),
        }
    };
    let Message::CredentialAsk {
        app,
        title,
        description,
        pin,
        fingerprint,
    } = ask
    else {
        return Err(io::Error::other("record for another endpoint"));
    };
    let Ok(connection) = Connection::connect_to_env() else {
        send(socket, &Message::CredentialCancelled);
        return Ok(());
    };
    let Ok((globals, mut queue)) = registry_queue_init::<State>(&connection) else {
        send(socket, &Message::CredentialCancelled);
        return Ok(());
    };
    let handle = queue.handle();
    let Ok(verifier) = globals.bind::<DenialUserVerifierV1, _, _>(&handle, 1..=1, ()) else {
        send(socket, &Message::CredentialCancelled);
        return Ok(());
    };
    let mut credentials = Credential::Password as u32;
    if pin {
        credentials |= Credential::Pin as u32;
    }
    if fingerprint {
        credentials |= Credential::Fingerprint as u32;
    }
    let sheet = verifier.verify(app, title, description, credentials, &handle, ());
    verifier.destroy();
    let mut state = State::default();
    let result = run(
        &connection,
        &mut queue,
        &mut state,
        &sheet,
        socket,
        &mut bytes,
    );
    for (_, secret) in &mut state.typed {
        wipe(secret);
    }
    wipe(&mut bytes);
    if !state.cancelled {
        sheet.destroy();
    }
    let _ = connection.flush();
    result
}

fn run(
    connection: &Connection,
    queue: &mut EventQueue<State>,
    state: &mut State,
    sheet: &DenialUserVerificationV1,
    socket: &SeqPacket,
    bytes: &mut [u8],
) -> io::Result<()> {
    let wayland = connection.as_fd().as_raw_fd();
    loop {
        queue.dispatch_pending(state).map_err(io::Error::other)?;
        while let Some((pin, secret)) = state.typed.pop_front() {
            let mut message = Message::CredentialSecret { pin, secret };
            let sent = send(socket, &message);
            if let Message::CredentialSecret { secret, .. } = &mut message {
                wipe(secret);
            }
            if !sent {
                return Ok(());
            }
        }
        if state.cancelled {
            send(socket, &Message::CredentialCancelled);
            return Ok(());
        }
        queue.flush().map_err(io::Error::other)?;
        let Some(guard) = queue.prepare_read() else {
            continue;
        };
        let mut descriptors = [
            libc::pollfd {
                fd: wayland,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: socket.as_fd().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: two valid pollfd entries that outlive the call.
        if unsafe { libc::poll(descriptors.as_mut_ptr(), 2, -1) } < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if descriptors[0].revents != 0 {
            match guard.read() {
                Ok(_) => {}
                Err(wayland_client::backend::WaylandError::Io(error))
                    if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(io::Error::other(error)),
            }
        } else {
            drop(guard);
        }
        if descriptors[1].revents == 0 {
            continue;
        }
        let record = match socket.receive_record_into(bytes) {
            Ok(record) => record,
            Err(IpcError::PeerClosed) => return Ok(()),
            Err(IpcError::Io(error)) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(io::Error::other(error)),
        };
        match Message::decode(record.bytes).map_err(io::Error::other)? {
            Message::CredentialWrong { retry_ms } => sheet.rejected(retry_ms),
            Message::CredentialClose { accepted } => {
                if accepted {
                    sheet.accepted();
                }
                return Ok(());
            }
            _ => return Err(io::Error::other("record for another endpoint")),
        }
    }
}
