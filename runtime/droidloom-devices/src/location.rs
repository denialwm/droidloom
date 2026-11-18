//! Android's GNSS service gets the phone's positions from dmail's
//! `org.dmail.Location1` on the system bus. dmail runs the modem's engine
//! while any client holds `Start`, so a session holds it from
//! [`Message::LocationStart`] to [`Message::LocationStop`] on a private bus
//! connection: when the broker or the connection goes, dmail drops the hold
//! by itself. The session passes on the engine's state on each change and
//! every new fix, and starts again when dmail does. Without dmail, or
//! without access to it (group `dmail`), Android hears the engine is
//! unavailable.

use std::collections::HashMap;
use std::io;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use droidloom_denial_ipc::{IpcError, SeqPacket};
use droidloom_device_protocol::{Engine, Fix, MAX_RECORD, Message};
use futures_lite::{StreamExt, future};
use zbus::zvariant::{OwnedValue, Value};

const DMAIL: &str = "org.dmail";
const PATH: &str = "/org/dmail";
const INTERFACE: &str = "org.dmail.Location1";
const CALL_TIMEOUT: Duration = Duration::from_secs(5);

/// Location1's position: latitude, longitude, accuracy, altitude, speed,
/// heading, fix time in Unix ms; NaN for what the fix lacks.
type Position = (f64, f64, f64, f64, f64, f64, i64);

/// What Location1 says, as far as it is known.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Status {
    engine: Engine,
    used: u32,
    tracked: u32,
}

impl Status {
    fn message(self) -> Message {
        Message::LocationState {
            engine: self.engine,
            used: self.used,
            tracked: self.tracked,
        }
    }
}

fn engine(state: &str) -> Engine {
    match state {
        "off" => Engine::Off,
        "searching" => Engine::Searching,
        "fixed" => Engine::Fixed,
        _ => Engine::Unavailable,
    }
}

/// The fix in a Location1 position, none before the first of a session.
fn fix(position: Position, used: u32) -> Option<Fix> {
    let (latitude, longitude, accuracy, altitude, speed, heading, time_ms) = position;
    let known = |value: f64| value.is_finite().then_some(value);
    (time_ms != 0
        && (-90.0..=90.0).contains(&latitude)
        && (-180.0..=180.0).contains(&longitude)
        && accuracy.is_finite()
        && accuracy >= 0.0)
        .then_some(Fix {
            latitude,
            longitude,
            accuracy,
            altitude: known(altitude),
            speed: known(speed),
            heading: known(heading),
            time_ms,
            satellites: used,
        })
}

/// Applies changed Location1 properties; returns a new fix when Position
/// changed to one.
fn apply(status: &mut Status, changed: &HashMap<&str, Value<'_>>) -> Option<Fix> {
    if let Some(state) = changed
        .get("State")
        .and_then(|value| <&str>::try_from(value).ok())
    {
        status.engine = engine(state);
    }
    if let Some(used) = changed
        .get("SatellitesUsed")
        .and_then(|value| u32::try_from(value).ok())
    {
        status.used = used;
    }
    if let Some(tracked) = changed
        .get("SatellitesTracked")
        .and_then(|value| u32::try_from(value).ok())
    {
        status.tracked = tracked;
    }
    changed
        .get("Position")
        .and_then(|value| Position::try_from(value.try_clone().ok()?).ok())
        .and_then(|position| fix(position, status.used))
}

async fn read_all(
    properties: &zbus::fdo::PropertiesProxy<'_>,
) -> zbus::Result<(Status, Option<Fix>)> {
    let all: HashMap<String, OwnedValue> = properties
        .get_all(zbus::names::InterfaceName::from_static_str_unchecked(
            INTERFACE,
        ))
        .await?;
    let changed: HashMap<&str, Value<'_>> = all
        .iter()
        .map(|(name, value)| (name.as_str(), Value::from(value.clone())))
        .collect();
    let mut status = Status {
        engine: Engine::Unavailable,
        used: 0,
        tracked: 0,
    };
    let fix = apply(&mut status, &changed);
    // A fix left from before this session is not current until dmail says so.
    Ok((status, fix.filter(|_| status.engine == Engine::Fixed)))
}

enum Event {
    Changed(Option<zbus::fdo::PropertiesChanged>),
    Owner(Option<Option<zbus::names::UniqueName<'static>>>),
    Cancel,
}

/// Holds Location1 until `cancel`, reporting through `report` until it
/// returns false.
async fn hold(
    cancel: &async_channel::Receiver<()>,
    report: &dyn Fn(&Message) -> bool,
) -> zbus::Result<()> {
    let connection = zbus::connection::Builder::system()?
        .method_timeout(CALL_TIMEOUT)
        .build()
        .await?;
    let location = zbus::Proxy::new(&connection, DMAIL, PATH, INTERFACE).await?;
    let properties = zbus::fdo::PropertiesProxy::new(&connection, DMAIL, PATH).await?;
    // Subscribed before starting, so the first change is not missed.
    let mut changes = properties.receive_properties_changed().await?;
    let mut owners = location.receive_owner_changed().await?;
    location.call_method("Start", &()).await?;
    let (mut status, first) = read_all(&properties).await?;
    if !report(&status.message()) || first.is_some_and(|fix| !report(&Message::LocationFix(fix))) {
        return Ok(());
    }
    loop {
        let event = future::race(
            future::race(async { Event::Changed(changes.next().await) }, async {
                Event::Owner(owners.next().await)
            }),
            async {
                let _ = cancel.recv().await;
                Event::Cancel
            },
        )
        .await;
        let before = status;
        let fix = match event {
            Event::Changed(Some(signal)) => {
                let Ok(args) = signal.args() else { continue };
                if args.interface_name.as_str() != INTERFACE {
                    continue;
                }
                apply(&mut status, args.changed_properties())
            }
            // dmail came back without the hold: take it again.
            Event::Owner(Some(Some(_))) => {
                location.call_method("Start", &()).await?;
                let (now, fix) = read_all(&properties).await?;
                status = now;
                fix
            }
            Event::Owner(Some(None)) => {
                status.engine = Engine::Unavailable;
                None
            }
            Event::Changed(None) | Event::Owner(None) => {
                return Err(zbus::Error::Failure("system bus connection ended".into()));
            }
            Event::Cancel => break,
        };
        if status != before && !report(&status.message()) {
            break;
        }
        if let Some(fix) = fix
            && !report(&Message::LocationFix(fix))
        {
            break;
        }
    }
    // Leaving the bus drops the hold too; Stop says so at once.
    let _ = location.call_method("Stop", &()).await;
    let _ = connection.close().await;
    Ok(())
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

/// Serves one connection of Android's GNSS service.
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
            Message::LocationStart => {
                if running
                    .as_ref()
                    .is_some_and(|session| !session.thread.is_finished())
                {
                    continue;
                }
                if let Some(last) = running.take() {
                    last.finish();
                }
                let (cancel, cancelled) = async_channel::bounded(1);
                let reporter = Arc::clone(socket);
                let thread =
                    std::thread::Builder::new()
                        .name("location".into())
                        .spawn(move || {
                            let report = |message: &Message| send(&reporter, message);
                            if let Err(error) = future::block_on(hold(&cancelled, &report)) {
                                eprintln!("droidloom-devices: no location from dmail: {error}");
                                report(&Message::LocationState {
                                    engine: Engine::Unavailable,
                                    used: 0,
                                    tracked: 0,
                                });
                            }
                        })?;
                running = Some(Running { cancel, thread });
            }
            Message::LocationStop => {
                if let Some(session) = running.take() {
                    session.finish();
                }
            }
            _ => break Err(io::Error::other("record for another endpoint")),
        }
    };
    if let Some(session) = running {
        session.finish();
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn location1_changes_become_records() {
        let mut status = Status {
            engine: Engine::Off,
            used: 0,
            tracked: 0,
        };
        let mut changed = HashMap::new();
        changed.insert("State", Value::from("fixed"));
        changed.insert("SatellitesUsed", Value::from(9u32));
        changed.insert("SatellitesTracked", Value::from(21u32));
        changed.insert(
            "Position",
            Value::from((
                45.46,
                9.19,
                10.0,
                f64::NAN,
                0.4,
                f64::NAN,
                1_795_000_000_000i64,
            )),
        );
        assert_eq!(
            apply(&mut status, &changed),
            Some(Fix {
                latitude: 45.46,
                longitude: 9.19,
                accuracy: 10.0,
                altitude: None,
                speed: Some(0.4),
                heading: None,
                time_ms: 1_795_000_000_000,
                satellites: 9,
            })
        );
        assert_eq!(
            status,
            Status {
                engine: Engine::Fixed,
                used: 9,
                tracked: 21,
            }
        );
    }

    #[test]
    fn no_fix_before_the_first() {
        let nan = f64::NAN;
        assert_eq!(fix((nan, nan, nan, nan, nan, nan, 0), 0), None);
        assert_eq!(engine("searching"), Engine::Searching);
        assert_eq!(engine("something new"), Engine::Unavailable);
    }
}
