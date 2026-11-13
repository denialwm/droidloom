//! Android's keyguard follows the host session's lock. The compositor tells
//! logind through `LockedHint`, as GNOME, KDE and Denial do; this watcher
//! hands each change to the framework, which reports its keyguard showing
//! while the host is locked. Keys that need an unlocked device then lock, and
//! the framework verifies Android's own credential again at the unlock.
//!
//! That credential stays with root (`authentication.rs`): each new
//! `system_server` gets it from here before the lock state, and nothing else
//! does.
use std::ffi::CString;
use std::fs;
use std::future::Future;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::thread::JoinHandle;
use std::time::Duration;

use async_io::{Async, Timer};
use futures_lite::{StreamExt, future};
use zbus::zvariant::OwnedObjectPath;
use zbus::{Connection, Proxy};

use super::{android_init_pid, framework_control, framework_wire, peer_credentials};
use crate::CellSpec;

const LOGIN1: &str = "org.freedesktop.login1";
/// Wait before retrying while Android's framework or the session is not there.
pub(super) const RETRY: Duration = Duration::from_secs(2);
const FRAMEWORK: &str = "dev/socket/droidloom-framework";

/// A thread that runs until dropped.
pub(super) struct Watch {
    stop: OwnedFd,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Watch {
    fn drop(&mut self) {
        let value = 1_u64;
        // SAFETY: the eventfd is owned by self and the buffer is 8 bytes.
        unsafe { libc::write(self.stop.as_raw_fd(), (&raw const value).cast(), 8) };
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Runs `run` on its own thread; its argument turns readable when the watch
/// is dropped.
pub(super) fn spawn<F, R>(name: &str, run: F) -> Option<Watch>
where
    F: FnOnce(Async<OwnedFd>) -> R + Send + 'static,
    R: Future<Output = ()>,
{
    // SAFETY: eventfd takes scalar arguments and returns a fresh descriptor.
    let stop = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
    if stop < 0 {
        eprintln!("{name} unavailable: {}", io::Error::last_os_error());
        return None;
    }
    // SAFETY: a fresh descriptor returned by the kernel, owned from here on.
    let stop = unsafe { OwnedFd::from_raw_fd(stop) };
    let wake = stop.try_clone().ok()?;
    let thread = std::thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            let Ok(wake) = Async::new(wake) else {
                return;
            };
            future::block_on(run(wake));
        })
        .ok()?;
    Some(Watch {
        stop,
        thread: Some(thread),
    })
}

pub(super) fn start(spec: &CellSpec) -> Option<Watch> {
    let spec = spec.clone();
    spawn("host-lock", move |stop| async move { run(&spec, &stop).await })
}

enum Event {
    Changed(Option<bool>),
    SessionGone,
    Framework,
    Retry,
    Stop,
}

/// What the current `system_server` has been told.
#[derive(Default)]
struct Framework {
    /// Android's init and the control socket's inode, which a new
    /// `system_server` replaces, once it has the credential.
    told: Option<(u32, u64)>,
    locked: Option<bool>,
    /// Wakes when a `system_server` creates its control socket.
    changes: Option<Async<OwnedFd>>,
    credential_failed: bool,
    lock_failed: bool,
}

async fn run(spec: &CellSpec, stop: &Async<OwnedFd>) {
    let mut framework = Framework::default();
    loop {
        if let Err(error) = follow(spec, stop, &mut framework).await {
            eprintln!("host lock watch: {error}");
        } else {
            return;
        }
        if wait(stop, RETRY).await {
            return;
        }
    }
}

/// Follows the user's graphical session until the watch stops (Ok) or the
/// session goes (Err, so the caller looks for the next one).
async fn follow(
    spec: &CellSpec,
    stop: &Async<OwnedFd>,
    framework: &mut Framework,
) -> Result<(), String> {
    let error = |error: zbus::Error| error.to_string();
    let connection = Connection::system().await.map_err(error)?;
    let manager = Proxy::new(
        &connection,
        LOGIN1,
        "/org/freedesktop/login1",
        "org.freedesktop.login1.Manager",
    )
    .await
    .map_err(error)?;
    let mut removed = manager.receive_signal("SessionRemoved").await.map_err(error)?;
    let user: OwnedObjectPath = manager
        .call("GetUser", &(spec.host_uid,))
        .await
        .map_err(error)?;
    let user = Proxy::new(&connection, LOGIN1, user, "org.freedesktop.login1.User")
        .await
        .map_err(error)?;
    let (id, session): (String, OwnedObjectPath) =
        user.get_property("Display").await.map_err(error)?;
    if id.is_empty() {
        return Err("the user has no graphical session yet".into());
    }
    let session = Proxy::new(&connection, LOGIN1, session, "org.freedesktop.login1.Session")
        .await
        .map_err(error)?;
    let mut locked: bool = session.get_property("LockedHint").await.map_err(error)?;
    let mut changes = session.receive_property_changed::<bool>("LockedHint").await;
    loop {
        tell(spec, framework, locked);
        let retry = framework.told.is_none() || framework.locked != Some(locked);
        let event = future::or(
            future::or(
                async {
                    let value = match changes.next().await {
                        Some(change) => change.get().await.ok(),
                        None => None,
                    };
                    Event::Changed(value)
                },
                async {
                    while let Some(message) = removed.next().await {
                        if message
                            .body()
                            .deserialize::<(String, OwnedObjectPath)>()
                            .is_ok_and(|(gone, _)| gone == id)
                        {
                            break;
                        }
                    }
                    Event::SessionGone
                },
            ),
            future::or(
                future::or(
                    async {
                        let _ = stop.readable().await;
                        Event::Stop
                    },
                    async {
                        match &framework.changes {
                            Some(changes) => {
                                let _ = changes.readable().await;
                                drain(changes);
                                Event::Framework
                            }
                            None => future::pending().await,
                        }
                    },
                ),
                async {
                    if retry {
                        Timer::after(RETRY).await;
                        Event::Retry
                    } else {
                        future::pending().await
                    }
                },
            ),
        )
        .await;
        match event {
            Event::Changed(Some(value)) => locked = value,
            Event::Changed(None) | Event::SessionGone => {
                return Err(format!("session {id} ended"));
            }
            Event::Framework | Event::Retry => {}
            Event::Stop => return Ok(()),
        }
    }
}

/// Gives a new `system_server` the credential, then the lock state when it
/// changed. Failures are logged once each and retried.
fn tell(spec: &CellSpec, framework: &mut Framework, locked: bool) {
    let current = android_init_pid(spec)
        .ok()
        .flatten()
        .and_then(|init| endpoint(init).map(|inode| (init, inode)));
    if current.is_none() || framework.told != current {
        framework.told = None;
        match give_credential(spec) {
            Ok(told) => {
                framework.told = Some(told);
                framework.locked = None;
                framework.credential_failed = false;
                if framework.changes.is_none() {
                    framework.changes = changes(told.0);
                }
            }
            // Android's framework may not be listening yet; it is asked again.
            Err(error) if !framework.credential_failed => {
                eprintln!("Android credential not given yet, retrying: {error}");
                framework.credential_failed = true;
            }
            Err(_) => {}
        }
    }
    let Some((init, _)) = framework.told else {
        return;
    };
    if framework.locked == Some(locked) {
        return;
    }
    match request(
        init,
        framework_wire::HOST_LOCK,
        &serde_json::json!({"user": 0, "locked": locked}),
    ) {
        Ok(()) => {
            framework.locked = Some(locked);
            framework.lock_failed = false;
        }
        Err(error) if !framework.lock_failed => {
            eprintln!("host lock not announced yet, retrying: {error}");
            framework.lock_failed = true;
        }
        Err(_) => {}
    }
}

/// The control socket's inode, which changes with each `system_server`.
fn endpoint(init: u32) -> Option<u64> {
    fs::symlink_metadata(format!("/proc/{init}/root/{FRAMEWORK}/control"))
        .ok()
        .map(|metadata| metadata.ino())
}

/// Wakes when a `system_server` creates its control socket again.
fn changes(init: u32) -> Option<Async<OwnedFd>> {
    // SAFETY: inotify_init1 takes scalar flags and returns a fresh descriptor.
    let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
    if fd < 0 {
        return None;
    }
    // SAFETY: a fresh descriptor returned by the kernel, owned from here on.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let path = CString::new(format!("/proc/{init}/root/{FRAMEWORK}")).ok()?;
    // SAFETY: a valid inotify descriptor and a NUL-terminated path.
    let watch = unsafe {
        libc::inotify_add_watch(
            fd.as_raw_fd(),
            path.as_ptr(),
            libc::IN_CREATE | libc::IN_MOVED_TO,
        )
    };
    if watch < 0 {
        return None;
    }
    Async::new(fd).ok()
}

fn drain(changes: &Async<OwnedFd>) {
    let mut buffer = [0_u8; 4096];
    // SAFETY: the buffer is valid for its length; the descriptor is nonblocking.
    while unsafe {
        libc::read(
            changes.get_ref().as_raw_fd(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
        )
    } > 0
    {}
}

async fn wait(stop: &Async<OwnedFd>, timeout: Duration) -> bool {
    future::or(
        async {
            let _ = stop.readable().await;
            true
        },
        async {
            Timer::after(timeout).await;
            false
        },
    )
    .await
}

fn connect(init: u32) -> Result<UnixStream, String> {
    let stream = framework_control::connect(init, "control").map_err(|error| error.to_string())?;
    let timeout = Some(Duration::from_secs(5));
    stream
        .set_read_timeout(timeout)
        .map_err(|error| error.to_string())?;
    stream
        .set_write_timeout(timeout)
        .map_err(|error| error.to_string())?;
    Ok(stream)
}

fn exchange(stream: &mut UnixStream, op: u16, request: &serde_json::Value) -> Result<(), String> {
    let request = serde_json::to_vec(request).map_err(|error| error.to_string())?;
    framework_wire::write(stream, op, &request, framework_wire::MAX_REQUEST)
        .map_err(|error| error.to_string())?;
    let (reply, bytes) = framework_wire::read(stream, framework_wire::MAX_RESPONSE)
        .map_err(|error| error.to_string())?;
    if reply == op {
        Ok(())
    } else {
        Err(format!(
            "Android framework refused: {}",
            String::from_utf8_lossy(&bytes)
        ))
    }
}

pub(super) fn request(init: u32, op: u16, request: &serde_json::Value) -> Result<(), String> {
    exchange(&mut connect(init)?, op, request)
}

/// Gives the credential to `system_server`, and returns what it was given to.
fn give_credential(spec: &CellSpec) -> Result<(u32, u64), String> {
    let init = android_init_pid(spec)
        .map_err(|error| error.to_string())?
        .ok_or("Android is not running")?;
    let inode = endpoint(init).ok_or("the framework is not listening yet")?;
    let password =
        crate::authentication::read_credential(spec).ok_or("no Android credential on the host")?;
    let mut stream = connect(init)?;
    system_server(&stream)?;
    exchange(
        &mut stream,
        framework_wire::CREDENTIAL,
        &serde_json::json!({"user": 0, "password": password}),
    )?;
    Ok((init, inode))
}

/// The credential goes to `system_server` alone: a process of Android's system
/// user forked by the cell's zygote, which runs as root. Android's system user
/// is the host user and could listen on the framework's socket itself, but
/// zygote's children are closed to it.
pub(super) fn system_server(stream: &UnixStream) -> Result<(), String> {
    let peer = peer_credentials(stream).map_err(|error| error.to_string())?;
    let pid = peer.pid;
    let field = |pid: i32, name: &str| -> Option<String> {
        fs::read_to_string(format!("/proc/{pid}/status"))
            .ok()?
            .lines()
            .find_map(|line| line.strip_prefix(name))
            .and_then(|value| value.split_whitespace().next())
            .map(str::to_owned)
    };
    let name = fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
    let zygote: i32 = field(pid, "PPid:")
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let zygote_program = fs::read_link(format!("/proc/{zygote}/exe")).unwrap_or_default();
    let forked_by_zygote = field(zygote, "Uid:").as_deref() == Some("0")
        && zygote_program
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("app_process"));
    if peer.uid == 1000 && name.trim_end() == "system_server" && forked_by_zygote {
        Ok(())
    } else {
        Err(format!(
            "the framework's socket is held by PID {pid}, not system_server"
        ))
    }
}
