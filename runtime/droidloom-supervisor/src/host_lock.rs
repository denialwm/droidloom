//! Android's keyguard follows the host session's lock. The compositor tells
//! logind through `LockedHint`, as GNOME, KDE and Denial do; this watcher
//! hands each change to the framework, which reports its keyguard showing
//! while the host is locked. Keys that need an unlocked device then lock, and
//! the framework verifies Android's own credential again at the unlock.
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::thread::JoinHandle;
use std::time::Duration;

use async_io::{Async, Timer};
use futures_lite::{StreamExt, future};
use zbus::zvariant::OwnedObjectPath;
use zbus::{Connection, Proxy};

use super::{android_init_pid, framework_control, framework_wire};
use crate::CellSpec;

const LOGIN1: &str = "org.freedesktop.login1";
/// Wait before retrying while Android's framework or the session is not there.
const RETRY: Duration = Duration::from_secs(2);

/// Follows the host lock until dropped.
pub(super) struct HostLockWatch {
    stop: OwnedFd,
    thread: Option<JoinHandle<()>>,
}

impl Drop for HostLockWatch {
    fn drop(&mut self) {
        let value = 1_u64;
        // SAFETY: the eventfd is owned by self and the buffer is 8 bytes.
        unsafe { libc::write(self.stop.as_raw_fd(), (&raw const value).cast(), 8) };
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub(super) fn start(spec: &CellSpec) -> Option<HostLockWatch> {
    // SAFETY: eventfd takes scalar arguments and returns a fresh descriptor.
    let stop = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
    if stop < 0 {
        eprintln!(
            "host lock watch unavailable: {}",
            io::Error::last_os_error()
        );
        return None;
    }
    // SAFETY: a fresh descriptor returned by the kernel, owned from here on.
    let stop = unsafe { OwnedFd::from_raw_fd(stop) };
    let wake = stop.try_clone().ok()?;
    let spec = spec.clone();
    let thread = std::thread::Builder::new()
        .name("host-lock".into())
        .spawn(move || {
            let Ok(wake) = Async::new(wake) else {
                return;
            };
            future::block_on(run(&spec, &wake));
        })
        .ok()?;
    Some(HostLockWatch {
        stop,
        thread: Some(thread),
    })
}

enum Event {
    Changed(Option<bool>),
    SessionGone,
    Retry,
    Stop,
}

async fn run(spec: &CellSpec, stop: &Async<OwnedFd>) {
    let mut announced = None;
    loop {
        if let Err(error) = follow(spec, stop, &mut announced).await {
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
    announced: &mut Option<bool>,
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
    let mut failed = false;
    loop {
        if *announced != Some(locked) {
            match announce(spec, locked) {
                Ok(()) => {
                    *announced = Some(locked);
                    failed = false;
                }
                // Android's framework may not be listening yet; it is asked again.
                Err(error) if !failed => {
                    eprintln!("host lock not announced yet, retrying: {error}");
                    failed = true;
                }
                Err(_) => {}
            }
        }
        let retry = *announced != Some(locked);
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
                async {
                    let _ = stop.readable().await;
                    Event::Stop
                },
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
            Event::Retry => {}
            Event::Stop => return Ok(()),
        }
    }
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

fn announce(spec: &CellSpec, locked: bool) -> Result<(), String> {
    let init = android_init_pid(spec)
        .map_err(|error| error.to_string())?
        .ok_or("Android is not running")?;
    let mut stream =
        framework_control::connect(init, "control").map_err(|error| error.to_string())?;
    let timeout = Some(Duration::from_secs(5));
    stream
        .set_read_timeout(timeout)
        .map_err(|error| error.to_string())?;
    stream
        .set_write_timeout(timeout)
        .map_err(|error| error.to_string())?;
    let request = serde_json::to_vec(&serde_json::json!({"user": 0, "locked": locked}))
        .map_err(|error| error.to_string())?;
    framework_wire::write(
        &mut stream,
        framework_wire::HOST_LOCK,
        &request,
        framework_wire::MAX_REQUEST,
    )
    .map_err(|error| error.to_string())?;
    let (reply, bytes) = framework_wire::read(&mut stream, framework_wire::MAX_RESPONSE)
        .map_err(|error| error.to_string())?;
    if reply == framework_wire::HOST_LOCK {
        Ok(())
    } else {
        Err(format!(
            "Android framework rejected the host lock: {}",
            String::from_utf8_lossy(&bytes)
        ))
    }
}
