//! Android's fingerprint service verifies against the fingers the user
//! enrolled in fprintd. Each verification owns a private system-bus
//! connection pinned to fprintd's owner, claims the reader, and stops and
//! releases it however it ends; closing the connection is the fallback. The
//! reader is claimed only while Android waits for a finger, so Denial's lock
//! screen finds it free.

use std::io;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use droidloom_denial_ipc::{IpcError, SeqPacket};
use droidloom_device_protocol::{MAX_RECORD, Message, Scan};
use futures_lite::{StreamExt, future};
use zbus::blocking::{Connection, Proxy, connection::Builder};
use zbus::zvariant::OwnedObjectPath;

const SERVICE: &str = "net.reactivated.Fprint";
const MANAGER: &str = "/net/reactivated/Fprint/Manager";
const MANAGER_INTERFACE: &str = "net.reactivated.Fprint.Manager";
const DEVICE_INTERFACE: &str = "net.reactivated.Fprint.Device";
const CALL_TIMEOUT: Duration = Duration::from_secs(3);

/// fprintd's first reader on `connection`, pinned to the daemon's current
/// owner, and the fingers the user enrolled on it.
fn reader(connection: &Connection) -> zbus::Result<Option<(Proxy<'static>, Vec<String>)>> {
    let manager = Proxy::new(connection, SERVICE, MANAGER, MANAGER_INTERFACE)?;
    // Activates fprintd before its owner is resolved.
    let devices: Vec<OwnedObjectPath> = manager.call("GetDevices", &())?;
    let owner =
        zbus::blocking::fdo::DBusProxy::new(connection)?.get_name_owner(SERVICE.try_into()?)?;
    let Some(path) = devices.into_iter().next() else {
        return Ok(None);
    };
    let device = Proxy::new(connection, owner, path, DEVICE_INTERFACE)?;
    // An empty username is the calling user.
    let fingers = match device.call::<_, _, Vec<String>>("ListEnrolledFingers", &("",)) {
        Ok(fingers) => fingers,
        Err(zbus::Error::MethodError(name, _, _)) if name.ends_with(".NoEnrolledPrints") => {
            Vec::new()
        }
        Err(error) => return Err(error),
    };
    Ok(Some((device, fingers)))
}

fn query() -> Message {
    let found = Builder::system()
        .and_then(|builder| builder.method_timeout(CALL_TIMEOUT).build())
        .and_then(|connection| reader(&connection));
    match found {
        Ok(Some((_, fingers))) => Message::FingerprintReader {
            present: true,
            fingers,
        },
        Ok(None) => Message::FingerprintReader {
            present: false,
            fingers: Vec::new(),
        },
        Err(error) => {
            eprintln!("droidloom-devices: no fingerprint reader from fprintd: {error}");
            Message::FingerprintReader {
                present: false,
                fingers: Vec::new(),
            }
        }
    }
}

fn status(status: &str, done: bool) -> Scan {
    match (status, done) {
        ("verify-match", true) => Scan::Match,
        ("verify-no-match", true) => Scan::NoMatch,
        (
            "verify-retry-scan"
            | "verify-swipe-too-short"
            | "verify-finger-not-centered"
            | "verify-remove-and-retry"
            | "verify-too-fast",
            false,
        ) => Scan::Retry,
        // Unknown statuses and inconsistent completion fail closed.
        _ => Scan::Unavailable,
    }
}

/// Verifies on a claimed reader until a match, a cancel or a failure,
/// reporting each event; a no-match listens again on the same claim.
fn verify_claimed(
    device: &Proxy<'_>,
    cancel: &async_channel::Receiver<()>,
    report: &dyn Fn(Scan) -> bool,
) -> zbus::Result<Scan> {
    // Subscribed before starting, so even an immediate result arrives.
    let mut statuses = future::block_on(device.inner().receive_signal("VerifyStatus"))?;
    // The property stream stays inert until the cache is first read.
    let _ = device.get_property::<bool>("finger-present");
    let mut fingers = future::block_on(
        device
            .inner()
            .receive_property_changed::<bool>("finger-present"),
    );
    let mut owner = future::block_on(device.inner().receive_owner_changed())?;
    enum Event {
        Status(Option<zbus::Message>),
        Finger(Option<bool>),
        Gone,
        Cancel,
    }
    loop {
        device.call::<_, _, ()>("VerifyStart", &("any",))?;
        loop {
            let event = future::block_on(future::race(
                future::race(async { Event::Status(statuses.next().await) }, async {
                    owner.next().await;
                    Event::Gone
                }),
                future::race(
                    async {
                        let present = match fingers.next().await {
                            Some(change) => change.get().await.ok(),
                            None => None,
                        };
                        Event::Finger(present)
                    },
                    async {
                        let _ = cancel.recv().await;
                        Event::Cancel
                    },
                ),
            ));
            let scan = match event {
                Event::Status(Some(message)) => {
                    let (text, done): (String, bool) = message.body().deserialize()?;
                    status(&text, done)
                }
                Event::Finger(Some(true)) => Scan::Touch,
                Event::Finger(Some(false)) => Scan::Lift,
                Event::Status(None) | Event::Finger(None) | Event::Gone => Scan::Unavailable,
                Event::Cancel => Scan::Cancelled,
            };
            if scan.ends() {
                return Ok(scan);
            }
            if !report(scan) {
                return Ok(Scan::Cancelled);
            }
            if scan == Scan::NoMatch {
                break;
            }
        }
        // A finished verification must stop before the next starts.
        device.call::<_, _, ()>("VerifyStop", &())?;
    }
}

fn stop_and_release(device: &Proxy<'_>) {
    let _ = device.call::<_, _, ()>("VerifyStop", &());
    let _ = device.call::<_, _, ()>("Release", &());
}

/// One verification: every event goes to `report`, and the one that ends it
/// is returned once the reader is let go, except a match, which is reported
/// first so Android hears it at once.
fn verify(cancel: &async_channel::Receiver<()>, report: &dyn Fn(Scan) -> bool) -> Scan {
    let connection =
        match Builder::system().and_then(|builder| builder.method_timeout(CALL_TIMEOUT).build()) {
            Ok(connection) => connection,
            Err(error) => {
                eprintln!("droidloom-devices: fingerprint verification unavailable: {error}");
                return Scan::Unavailable;
            }
        };
    let device = match reader(&connection) {
        Ok(Some((device, fingers))) if !fingers.is_empty() => device,
        Ok(_) => return Scan::Unavailable,
        Err(error) => {
            eprintln!("droidloom-devices: fingerprint verification unavailable: {error}");
            return Scan::Unavailable;
        }
    };
    if let Err(error) = device.call::<_, _, ()>("Claim", &("",)) {
        // Denial's lock screen, or another client, holds the reader.
        eprintln!("droidloom-devices: fingerprint reader not claimed: {error}");
        return Scan::Unavailable;
    }
    let scan = verify_claimed(&device, cancel, report).unwrap_or_else(|error| {
        eprintln!("droidloom-devices: fingerprint verification failed: {error}");
        Scan::Unavailable
    });
    if scan == Scan::Match {
        report(scan);
    }
    stop_and_release(&device);
    let _ = connection.close();
    scan
}

fn send(socket: &SeqPacket, message: &Message) -> bool {
    socket.send_record(&message.encode(), &[]).is_ok()
}

struct Running {
    cancel: async_channel::Sender<()>,
    thread: JoinHandle<()>,
}

impl Running {
    fn finish(self) {
        let _ = self.cancel.try_send(());
        let _ = self.thread.join();
    }
}

/// Serves one connection of Android's fingerprint service.
pub fn serve(socket: &Arc<SeqPacket>) -> io::Result<()> {
    let mut bytes = vec![0; MAX_RECORD];
    let mut running: Option<Running> = None;
    let result = loop {
        let record = match socket.receive_record_into(&mut bytes) {
            Ok(record) => record,
            Err(IpcError::PeerClosed) => break Ok(()),
            Err(IpcError::Io(error)) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => break Err(io::Error::other(error)),
        };
        let message = match Message::decode(record.bytes) {
            Ok(message) => message,
            Err(error) => break Err(io::Error::other(error)),
        };
        match message {
            Message::FingerprintQuery => {
                if !send(socket, &query()) {
                    break Ok(());
                }
            }
            Message::FingerprintVerify => {
                // The last verification let the reader go before this one claims it.
                if let Some(last) = running.take() {
                    if !last.thread.is_finished() {
                        running = Some(last);
                        break Err(io::Error::other("a verification is already running"));
                    }
                    last.finish();
                }
                let (cancel, cancelled) = async_channel::bounded(1);
                let reporter = Arc::clone(socket);
                let thread = std::thread::Builder::new()
                    .name("fingerprint".into())
                    .spawn(move || {
                        let report = |scan| send(&reporter, &Message::FingerprintScan(scan));
                        let scan = verify(&cancelled, &report);
                        if scan != Scan::Match {
                            report(scan);
                        }
                    })?;
                running = Some(Running { cancel, thread });
            }
            Message::FingerprintCancel => {
                if let Some(running) = &running {
                    let _ = running.cancel.try_send(());
                }
            }
            _ => break Err(io::Error::other("record for another endpoint")),
        }
    };
    if let Some(running) = running {
        running.finish();
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fprintd_statuses_become_scans() {
        assert_eq!(status("verify-match", true), Scan::Match);
        assert_eq!(status("verify-no-match", true), Scan::NoMatch);
        assert_eq!(status("verify-swipe-too-short", false), Scan::Retry);
        assert_eq!(status("verify-match", false), Scan::Unavailable);
        assert_eq!(status("verify-disconnected", true), Scan::Unavailable);
    }
}
