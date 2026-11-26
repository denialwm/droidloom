//! Per-user audio endpoint entry point: Android's streams on one socket, its
//! media volume on another ([`droidloom_audio::volume`]).
use clap::Parser;
use droidloom_audio::{Options, serve_connection, volume::serve_volume};
use droidloom_audio_protocol::{IpcError, SeqPacket, SeqPacketListener};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[derive(Parser)]
#[command(about = "Per-user PipeWire bridge for private Android audio")]
struct Arguments {
    /// Delegated socket; parent must be owned by this user and mode 0700.
    #[arg(long)]
    socket: PathBuf,
    /// Host-mapped Android audioserver UID (preview: 1041).
    #[arg(long, default_value_t = 1041)]
    peer_uid: u32,
    /// Media volume socket, when not started by its socket unit.
    #[arg(long)]
    volume_socket: Option<PathBuf>,
    /// Host-mapped UID of system_server, which hands SystemUI the volume socket.
    #[arg(long, default_value_t = 1000)]
    volume_peer_uid: u32,
    /// Keep Android off the host microphone; also DROIDLOOM_AUDIO_CAPTURE=0.
    /// Otherwise Android's own per-app microphone permission decides.
    #[arg(long)]
    deny_capture: bool,
    /// Stable PipeWire target name; otherwise use session routing policy.
    #[arg(long)]
    target: Option<String>,
    /// Optional isolated PipeWire remote.
    #[arg(long)]
    remote: Option<String>,
    /// Explicit loopback capture from a sink monitor for diagnostics.
    #[arg(long)]
    capture_sink: bool,
}

fn run(arguments: Arguments) -> io::Result<()> {
    // SAFETY: credential query has no side effects.
    let uid = unsafe { libc::geteuid() };
    if uid == 0 {
        return Err(io::Error::other("audio must run as the host user"));
    }
    check_peer_uid_mapping(
        &std::fs::read_to_string("/proc/self/uid_map")?,
        arguments.peer_uid,
    )?;
    let parent = arguments
        .socket
        .parent()
        .ok_or_else(|| io::Error::other("socket needs a parent"))?;
    if !parent.exists() {
        std::fs::create_dir(parent)?;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
    }
    let metadata = std::fs::symlink_metadata(parent)?;
    if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o777 != 0o700 {
        return Err(io::Error::other(
            "audio socket parent must be user-owned and mode 0700",
        ));
    }
    let activated = std::env::var("LISTEN_PID")
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        == Some(std::process::id());
    let mut volume = None;
    let listener = if activated {
        let count = match std::env::var("LISTEN_FDS").as_deref() {
            Ok("1") => 1,
            Ok("2") => 2,
            _ => {
                return Err(io::Error::other(
                    "audio needs one or two activation sockets",
                ));
            }
        };
        let mut streams = None;
        for raw in 3..3 + count {
            // SAFETY: fcntl validates the inherited descriptor before we own it;
            // mark it close-on-exec so later subprocesses cannot retain the listener.
            if unsafe { libc::fcntl(raw, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: systemd's matching PID transfers FDs 3.. to this process.
            // The audited adapter verifies each is a listening seqpacket.
            let fd = unsafe { OwnedFd::from_raw_fd(raw) };
            let is_volume = bound_path(&fd)?.file_name() == Some("volume.sock".as_ref());
            let listener = SeqPacketListener::from_owned_fd(fd).map_err(io::Error::other)?;
            if is_volume {
                volume = Some(listener);
            } else {
                streams = Some(listener);
            }
        }
        streams.ok_or_else(|| io::Error::other("no audio activation socket"))?
    } else {
        if let Some(path) = &arguments.volume_socket {
            let listener = SeqPacketListener::bind(path).map_err(io::Error::other)?;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o666))?;
            volume = Some(listener);
        }
        SeqPacketListener::bind(&arguments.socket).map_err(io::Error::other)?
    };
    // Parent directory protects the host pathname. The supervisor projects only
    // this socket inode into Android, where its audioserver UID needs access.
    // The socket unit has already set the activated inode's permissions. Leave
    // that inode untouched so the broker can run with ProtectSystem=strict.
    if !activated {
        std::fs::set_permissions(&arguments.socket, std::fs::Permissions::from_mode(0o666))?;
    }
    if let Some(listener) = volume {
        check_peer_uid_mapping(
            &std::fs::read_to_string("/proc/self/uid_map")?,
            arguments.volume_peer_uid,
        )?;
        let (peer_uid, remote) = (arguments.volume_peer_uid, arguments.remote.clone());
        std::thread::Builder::new()
            .name("volume".into())
            .spawn(move || serve_volumes(&listener, peer_uid, remote.as_deref()))?;
    }
    let options = Options {
        peer_uid: arguments.peer_uid,
        allow_capture: !arguments.deny_capture
            && std::env::var("DROIDLOOM_AUDIO_CAPTURE").as_deref() != Ok("0"),
        target: arguments.target,
        remote: arguments.remote,
        capture_sink: arguments.capture_sink,
    };
    let clients = Arc::new(AtomicUsize::new(0));
    loop {
        let socket = match listener.accept() {
            Ok(socket) => socket,
            Err(IpcError::Io(error)) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(io::Error::other(error)),
        };
        if clients.fetch_add(1, Ordering::Relaxed) >= 16 {
            clients.fetch_sub(1, Ordering::Relaxed);
            continue;
        }
        let options = options.clone();
        let clients = Arc::clone(&clients);
        std::thread::spawn(move || {
            let result = serve_connection(socket, &options);
            if let Err(error) = result {
                if error.kind() != io::ErrorKind::UnexpectedEof {
                    eprintln!("droidloom-audio: stream ended: {error}");
                }
            }
            clients.fetch_sub(1, Ordering::Relaxed);
        });
    }
}

/// Serves Android's media volume, one SystemUI connection at a time.
fn serve_volumes(listener: &SeqPacketListener, peer_uid: u32, remote: Option<&str>) {
    loop {
        let socket = match listener.accept() {
            Ok(socket) => socket,
            Err(IpcError::Io(error)) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => {
                eprintln!("droidloom-audio: volume socket failed: {error}");
                return;
            }
        };
        if !peer_is(&socket, peer_uid) {
            continue;
        }
        if let Err(error) = serve_volume(socket, remote) {
            eprintln!("droidloom-audio: volume connection ended: {error}");
        }
    }
}

fn peer_is(socket: &SeqPacket, uid: u32) -> bool {
    match socket.peer_credentials() {
        Ok(peer) if peer.uid == uid => true,
        Ok(peer) => {
            eprintln!(
                "droidloom-audio: rejected volume peer UID {}, expected {uid}",
                peer.uid
            );
            false
        }
        Err(_) => false,
    }
}

/// The path a listening socket is bound to.
fn bound_path(fd: &OwnedFd) -> io::Result<PathBuf> {
    // SAFETY: zero is a valid empty sockaddr_un.
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    let mut length = libc::socklen_t::try_from(std::mem::size_of::<libc::sockaddr_un>())
        .expect("sockaddr_un fits socklen_t");
    // SAFETY: the address and length describe writable storage of that size.
    if unsafe { libc::getsockname(fd.as_raw_fd(), (&raw mut address).cast(), &raw mut length) } < 0
    {
        return Err(io::Error::last_os_error());
    }
    let path: Vec<u8> = address
        .sun_path
        .iter()
        .take_while(|byte| **byte != 0)
        .map(|byte| u8::from_ne_bytes(byte.to_ne_bytes()))
        .collect();
    Ok(Path::new(std::ffi::OsStr::from_bytes(&path)).to_path_buf())
}

fn check_peer_uid_mapping(mapping: &str, peer_uid: u32) -> io::Result<()> {
    for line in mapping.lines() {
        let fields = line
            .split_whitespace()
            .map(str::parse::<u32>)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| io::Error::other("invalid audio broker UID mapping"))?;
        if fields.len() != 3 {
            return Err(io::Error::other("invalid audio broker UID mapping"));
        }
        if peer_uid
            .checked_sub(fields[0])
            .is_some_and(|offset| offset < fields[2])
        {
            return Ok(());
        }
    }
    Err(io::Error::other(format!(
        "Android peer UID {peer_uid} is not mapped in the audio broker's user namespace; \
         remove filesystem/user namespacing from its user service"
    )))
}

fn main() {
    if let Err(error) = run(Arguments::parse()) {
        eprintln!("droidloom-audio: {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::check_peer_uid_mapping;

    #[test]
    fn cross_uid_endpoint_requires_a_visible_peer_uid() {
        assert!(check_peer_uid_mapping("0 0 4294967295\n", 1041).is_ok());
        assert!(check_peer_uid_mapping("1000 1000 1\n", 1041).is_err());
        assert!(check_peer_uid_mapping("1000 1000 1\n1041 1041 1\n", 1041).is_ok());
        assert!(check_peer_uid_mapping("1041 1041 1\n", 1042).is_err());
    }

    #[test]
    fn malformed_uid_mappings_are_rejected() {
        for mapping in ["", "1000 1000", "0 0 4294967296", "0 0 invalid"] {
            assert!(check_peer_uid_mapping(mapping, 1041).is_err());
        }
    }
}
