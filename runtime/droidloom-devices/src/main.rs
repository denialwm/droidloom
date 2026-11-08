//! The host user's broker for Android's haptics and cameras.
//!
//! Android's vibrator service sends patterns on one socket; they go to
//! hapticd on the system bus, over one connection kept for the broker's life
//! because hapticd cancels a connection's effects when it leaves. Android's
//! camera service asks on the other for the host's cameras and for braun
//! sessions: braun lets the user of an active local session in, and the
//! session's socket goes to Android as it is. Neither D-Bus nor a device node
//! enters Android. Without hapticd or braun, Android hears there is no
//! vibrator or camera.

use std::collections::HashMap;
use std::io;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use clap::Parser;
use droidloom_denial_ipc::{IpcError, SeqPacket, SeqPacketListener};
use droidloom_device_protocol::{Camera, Facing, MAX_RECORD, Message, Step};
use zbus::blocking::Connection;
use zbus::zvariant::OwnedValue;

const HAPTICD: &str = "org.hapticd";
const HAPTICD_PATH: &str = "/org/hapticd/Haptics";
const HAPTICD_INTERFACE: &str = "org.hapticd.Haptics1";
const BRAUN: &str = "org.braun";
const BRAUN_PATH: &str = "/org/braun";
const BRAUN_INTERFACE: &str = "org.braun.Camera1";
const PROPERTIES: &str = "org.freedesktop.DBus.Properties";
/// Android has one vibrator and one camera service; leave room for restarts.
const MAX_CLIENTS: usize = 8;

#[derive(Parser)]
#[command(about = "Host user's broker for Android's haptics and cameras")]
struct Arguments {
    /// Haptics socket, when not started by its socket unit.
    #[arg(long)]
    haptics_socket: Option<PathBuf>,
    /// Camera socket, when not started by its socket unit.
    #[arg(long)]
    camera_socket: Option<PathBuf>,
    /// Host-mapped UID of Android's vibrator service (system).
    #[arg(long, default_value_t = 1000)]
    haptics_uid: u32,
    /// Host-mapped UID of Android's camera provider (cameraserver).
    #[arg(long, default_value_t = 1047)]
    camera_uid: u32,
}

#[derive(Clone, Copy)]
enum Endpoint {
    Haptics,
    Camera,
}

/// The system bus, connected when first needed and again after a failure.
struct Bus(Mutex<Option<Connection>>);

impl Bus {
    fn call<T>(&self, call: impl FnOnce(&Connection) -> zbus::Result<T>) -> zbus::Result<T> {
        let mut connection = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if connection.is_none() {
            *connection = Some(
                zbus::blocking::connection::Builder::system()?
                    .method_timeout(Duration::from_secs(3))
                    .build()?,
            );
        }
        let result = call(connection.as_ref().expect("connected"));
        if let Err(zbus::Error::InputOutput(_)) = &result {
            *connection = None;
        }
        result
    }
}

struct Haptics {
    bus: Bus,
    /// The effect playing; only this connection can cancel it.
    playing: Mutex<Option<u64>>,
}

impl Haptics {
    fn device(&self) -> Message {
        let found = self.bus.call(|bus| {
            let devices: Vec<(String, String, String, bool)> = bus
                .call_method(
                    Some(HAPTICD),
                    HAPTICD_PATH,
                    Some(HAPTICD_INTERFACE),
                    "ListDevices",
                    &(),
                )?
                .body()
                .deserialize()?;
            let default: String = bus
                .call_method(
                    Some(HAPTICD),
                    HAPTICD_PATH,
                    Some(PROPERTIES),
                    "Get",
                    &(HAPTICD_INTERFACE, "DefaultDevice"),
                )
                .ok()
                .and_then(|reply| reply.body().deserialize::<OwnedValue>().ok())
                .and_then(|value| String::try_from(value).ok())
                .unwrap_or_default();
            Ok(devices
                .iter()
                .find(|device| device.0 == default)
                .or(devices.first())
                .map(|device| device.3))
        });
        match found {
            Ok(Some(intensity)) => Message::HapticsDevice {
                present: true,
                intensity,
            },
            Ok(None) => Message::HapticsDevice {
                present: false,
                intensity: false,
            },
            Err(error) => {
                eprintln!("droidloom-devices: no vibrator from hapticd: {error}");
                Message::HapticsDevice {
                    present: false,
                    intensity: false,
                }
            }
        }
    }

    fn play(&self, steps: &[Step]) {
        let pattern: Vec<(u32, f64)> = steps
            .iter()
            .map(|step| (step.ms, f64::from(step.intensity)))
            .collect();
        let effect = self.bus.call(|bus| {
            bus.call_method(
                Some(HAPTICD),
                HAPTICD_PATH,
                Some(HAPTICD_INTERFACE),
                "PlayPattern",
                &("", pattern),
            )?
            .body()
            .deserialize::<u64>()
        });
        let mut playing = self.playing.lock().unwrap_or_else(PoisonError::into_inner);
        match effect {
            Ok(effect) => *playing = Some(effect),
            Err(error) => eprintln!("droidloom-devices: hapticd did not play: {error}"),
        }
    }

    fn stop(&self) {
        let Some(effect) = self
            .playing
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        else {
            return;
        };
        // An effect that already ended is no longer hapticd's to cancel.
        let _ = self.bus.call(|bus| {
            bus.call_method(
                Some(HAPTICD),
                HAPTICD_PATH,
                Some(HAPTICD_INTERFACE),
                "Cancel",
                &effect,
            )
            .map(drop)
        });
    }
}

fn camera_from(entry: &HashMap<String, OwnedValue>) -> Option<Camera> {
    let text = |key: &str| {
        entry
            .get(key)
            .and_then(|value| <&str>::try_from(value).ok())
            .map(str::to_owned)
    };
    let number = |key: &str| entry.get(key).and_then(|value| u32::try_from(value).ok());
    let flag = |key: &str| {
        entry
            .get(key)
            .and_then(|value| bool::try_from(value).ok())
            .unwrap_or(false)
    };
    let facing = match text("facing")?.as_str() {
        "front" => Facing::Front,
        "back" => Facing::Back,
        _ => Facing::External,
    };
    // Narrowing: braun reports zoom as a double near 1.
    #[allow(clippy::cast_possible_truncation)]
    let zoom = entry
        .get("zoom")
        .and_then(|value| f64::try_from(value).ok())
        .map_or(0.0, |zoom| zoom as f32);
    Some(Camera {
        id: text("id")?,
        name: text("name").unwrap_or_default(),
        facing,
        width: number("width")?,
        height: number("height")?,
        zoom,
        lens: flag("lens"),
    })
}

fn cameras(bus: &Bus) -> Vec<Camera> {
    let listed = bus.call(|bus| {
        let value: OwnedValue = bus
            .call_method(
                Some(BRAUN),
                BRAUN_PATH,
                Some(PROPERTIES),
                "Get",
                &(BRAUN_INTERFACE, "Cameras"),
            )?
            .body()
            .deserialize()?;
        Vec::<HashMap<String, OwnedValue>>::try_from(value).map_err(zbus::Error::from)
    });
    match listed {
        Ok(entries) => entries.iter().filter_map(camera_from).collect(),
        Err(error) => {
            eprintln!("droidloom-devices: no cameras from braun: {error}");
            Vec::new()
        }
    }
}

fn braun_session(bus: &Bus) -> Result<OwnedFd, String> {
    bus.call(|bus| {
        bus.call_method(Some(BRAUN), BRAUN_PATH, Some(BRAUN_INTERFACE), "Connect", &())?
            .body()
            .deserialize::<zbus::zvariant::OwnedFd>()
    })
    .map(OwnedFd::from)
    .map_err(|error| format!("braun refused a session: {error}"))
}

fn send(socket: &SeqPacket, message: &Message) -> io::Result<()> {
    socket
        .send_record(&message.encode(), &[])
        .map_err(io::Error::other)
}

fn serve(socket: &SeqPacket, endpoint: Endpoint, haptics: &Haptics, bus: &Bus) -> io::Result<()> {
    let mut bytes = vec![0; MAX_RECORD];
    loop {
        let record = match socket.receive_record_into(&mut bytes) {
            Ok(record) => record,
            Err(IpcError::PeerClosed) => return Ok(()),
            Err(IpcError::Io(error)) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(io::Error::other(error)),
        };
        let message = Message::decode(record.bytes).map_err(io::Error::other)?;
        match (endpoint, message) {
            (Endpoint::Haptics, Message::HapticsQuery) => send(socket, &haptics.device())?,
            (Endpoint::Haptics, Message::HapticsPlay(steps)) => haptics.play(&steps),
            (Endpoint::Haptics, Message::HapticsStop) => haptics.stop(),
            (Endpoint::Camera, Message::CameraList) => {
                send(socket, &Message::Cameras(cameras(bus)))?;
            }
            (Endpoint::Camera, Message::CameraConnect) => match braun_session(bus) {
                Ok(session) => socket
                    .send_record(&Message::CameraSession.encode(), &[session.as_fd()])
                    .map_err(io::Error::other)?,
                Err(reason) => {
                    eprintln!("droidloom-devices: {reason}");
                    send(socket, &Message::CameraUnavailable(reason))?;
                }
            },
            _ => return Err(io::Error::other("record for the other endpoint")),
        }
    }
}

/// The path a listening socket is bound to.
fn bound_path(fd: &OwnedFd) -> io::Result<PathBuf> {
    // SAFETY: zero is a valid empty sockaddr_un.
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    let mut length = libc::socklen_t::try_from(std::mem::size_of::<libc::sockaddr_un>())
        .expect("sockaddr_un fits socklen_t");
    // SAFETY: the address and length describe writable storage of that size.
    if unsafe {
        libc::getsockname(
            fd.as_raw_fd(),
            (&raw mut address).cast(),
            &raw mut length,
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    let path: Vec<u8> = address
        .sun_path
        .iter()
        .take_while(|byte| **byte != 0)
        .map(|byte| u8::from_ne_bytes(byte.to_ne_bytes()))
        .collect();
    Ok(PathBuf::from(std::ffi::OsStr::from_bytes(&path)))
}

fn endpoint_of(path: &Path) -> Option<Endpoint> {
    match path.file_name()?.to_str()? {
        "haptics.sock" => Some(Endpoint::Haptics),
        "camera.sock" => Some(Endpoint::Camera),
        _ => None,
    }
}

/// Binds `path` when the broker runs without its socket unit.
fn bind(path: &Path) -> io::Result<SeqPacketListener> {
    // SAFETY: credential query has no side effects.
    let uid = unsafe { libc::geteuid() };
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("socket needs a parent"))?;
    if !parent.exists() {
        std::fs::create_dir(parent)?;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
    }
    let metadata = std::fs::symlink_metadata(parent)?;
    if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o777 != 0o700 {
        return Err(io::Error::other(
            "device socket parent must be user-owned and mode 0700",
        ));
    }
    let listener = SeqPacketListener::bind(path).map_err(io::Error::other)?;
    // The parent guards the path; Android's services reach the inode the
    // supervisor binds into the cell.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o666))?;
    Ok(listener)
}

fn listeners(arguments: &Arguments) -> io::Result<Vec<(Endpoint, SeqPacketListener)>> {
    let activated = std::env::var("LISTEN_PID")
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        == Some(std::process::id());
    if !activated {
        let mut listeners = Vec::new();
        if let Some(path) = &arguments.haptics_socket {
            listeners.push((Endpoint::Haptics, bind(path)?));
        }
        if let Some(path) = &arguments.camera_socket {
            listeners.push((Endpoint::Camera, bind(path)?));
        }
        return Ok(listeners);
    }
    let count: i32 = std::env::var("LISTEN_FDS")
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|count| (1..=2).contains(count))
        .ok_or_else(|| io::Error::other("devices needs one or two activation sockets"))?;
    let mut listeners = Vec::new();
    for raw in 3..3 + count {
        // SAFETY: fcntl validates the inherited descriptor before we own it.
        if unsafe { libc::fcntl(raw, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: systemd passed descriptors 3.. to this PID.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        let endpoint = endpoint_of(&bound_path(&fd)?)
            .ok_or_else(|| io::Error::other("activation socket is neither haptics nor camera"))?;
        listeners.push((
            endpoint,
            SeqPacketListener::from_owned_fd(fd).map_err(io::Error::other)?,
        ));
    }
    Ok(listeners)
}

fn check_peer_uid_mapping(mapping: &str, peer_uid: u32) -> io::Result<()> {
    for line in mapping.lines() {
        let fields = line
            .split_whitespace()
            .map(str::parse::<u32>)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| io::Error::other("invalid broker UID mapping"))?;
        if fields.len() != 3 {
            return Err(io::Error::other("invalid broker UID mapping"));
        }
        if peer_uid
            .checked_sub(fields[0])
            .is_some_and(|offset| offset < fields[2])
        {
            return Ok(());
        }
    }
    Err(io::Error::other(format!(
        "Android peer UID {peer_uid} is not mapped in the device broker's user namespace; \
         remove filesystem/user namespacing from its user service"
    )))
}

fn run(arguments: &Arguments) -> io::Result<()> {
    // SAFETY: credential query has no side effects.
    if unsafe { libc::geteuid() } == 0 {
        return Err(io::Error::other("devices must run as the host user"));
    }
    let mapping = std::fs::read_to_string("/proc/self/uid_map")?;
    check_peer_uid_mapping(&mapping, arguments.haptics_uid)?;
    check_peer_uid_mapping(&mapping, arguments.camera_uid)?;
    let listeners = listeners(arguments)?;
    if listeners.is_empty() {
        return Err(io::Error::other("no device socket to serve"));
    }
    let haptics = Arc::new(Haptics {
        bus: Bus(Mutex::new(None)),
        playing: Mutex::new(None),
    });
    let bus = Arc::new(Bus(Mutex::new(None)));
    let clients = Arc::new(AtomicUsize::new(0));
    let mut accepting = Vec::new();
    for (endpoint, listener) in listeners {
        let (haptics, bus, clients) = (Arc::clone(&haptics), Arc::clone(&bus), Arc::clone(&clients));
        let peer_uid = match endpoint {
            Endpoint::Haptics => arguments.haptics_uid,
            Endpoint::Camera => arguments.camera_uid,
        };
        accepting.push(std::thread::spawn(move || -> io::Result<()> {
            loop {
                let socket = match listener.accept() {
                    Ok(socket) => socket,
                    Err(IpcError::Io(error)) if error.kind() == io::ErrorKind::Interrupted => {
                        continue;
                    }
                    Err(error) => return Err(io::Error::other(error)),
                };
                match socket.peer_credentials() {
                    Ok(peer) if peer.uid == peer_uid => {}
                    Ok(peer) => {
                        eprintln!(
                            "droidloom-devices: rejected peer UID {}, expected {peer_uid}",
                            peer.uid
                        );
                        continue;
                    }
                    Err(_) => continue,
                }
                if clients.fetch_add(1, Ordering::Relaxed) >= MAX_CLIENTS {
                    clients.fetch_sub(1, Ordering::Relaxed);
                    continue;
                }
                let (haptics, bus, clients) =
                    (Arc::clone(&haptics), Arc::clone(&bus), Arc::clone(&clients));
                std::thread::spawn(move || {
                    if let Err(error) = serve(&socket, endpoint, &haptics, &bus) {
                        eprintln!("droidloom-devices: client ended: {error}");
                    }
                    if matches!(endpoint, Endpoint::Haptics) {
                        haptics.stop();
                    }
                    clients.fetch_sub(1, Ordering::Relaxed);
                });
            }
        }));
    }
    for thread in accepting {
        thread
            .join()
            .map_err(|_| io::Error::other("accept thread panicked"))??;
    }
    Ok(())
}

fn main() {
    if let Err(error) = run(&Arguments::parse()) {
        eprintln!("droidloom-devices: {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn braun_cameras_become_records() {
        let mut entry = HashMap::new();
        entry.insert("id".to_owned(), OwnedValue::from(zbus::zvariant::Str::from("imx890")));
        entry.insert("name".to_owned(), OwnedValue::from(zbus::zvariant::Str::from("IMX890")));
        entry.insert("facing".to_owned(), OwnedValue::from(zbus::zvariant::Str::from("back")));
        entry.insert("width".to_owned(), OwnedValue::from(3072u32));
        entry.insert("height".to_owned(), OwnedValue::from(4096u32));
        entry.insert("lens".to_owned(), OwnedValue::from(true));
        entry.insert("zoom".to_owned(), OwnedValue::from(1.0f64));
        assert_eq!(
            camera_from(&entry),
            Some(Camera {
                id: "imx890".into(),
                name: "IMX890".into(),
                facing: Facing::Back,
                width: 3072,
                height: 4096,
                zoom: 1.0,
                lens: true,
            })
        );
        entry.remove("width");
        assert_eq!(camera_from(&entry), None);
    }

    #[test]
    fn sockets_are_told_apart_by_name() {
        assert!(matches!(
            endpoint_of(Path::new("/run/user/1000/droidloom/haptics.sock")),
            Some(Endpoint::Haptics)
        ));
        assert!(matches!(
            endpoint_of(Path::new("/run/user/1000/droidloom/camera.sock")),
            Some(Endpoint::Camera)
        ));
        assert!(endpoint_of(Path::new("/run/user/1000/droidloom/audio.sock")).is_none());
    }
}
