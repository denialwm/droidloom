//! Android's GNSS service gets the phone's positions from dmail's
//! `org.dmail.Location1` on the system bus. dmail runs the modem's engine
//! while any client holds `Start`, so a session holds it from
//! [`Message::LocationStart`] to [`Message::LocationStop`] on a private bus
//! connection: when the broker or the connection goes, dmail drops the hold
//! by itself. The session passes on the engine's state on each change, every
//! new fix with the accuracies dmail announced just before it, and the
//! satellite list on each change; it starts again when dmail does. A dmail
//! without `Accuracies` or `Satellites` gives fixes without them. Without dmail, or
//! without access to it (group `dmail`), Android hears the engine is
//! unavailable.

use std::collections::HashMap;
use std::io;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use droidloom_denial_ipc::{IpcError, SeqPacket};
use droidloom_device_protocol::{
    Constellation, Engine, Fix, MAX_RECORD, MAX_SATELLITES, Message, Satellite,
};
use futures_lite::{StreamExt, future};
use zbus::zvariant::{OwnedValue, Value};

const DMAIL: &str = "org.dmail";
const PATH: &str = "/org/dmail";
const INTERFACE: &str = "org.dmail.Location1";
const CALL_TIMEOUT: Duration = Duration::from_secs(5);

/// Location1's position: latitude, longitude, accuracy, altitude, speed,
/// heading, fix time in Unix ms; NaN for what the fix lacks.
type Position = (f64, f64, f64, f64, f64, f64, i64);

/// Location1's accuracies: vertical (m), speed (m/s), heading (degrees).
type Accuracies = (f64, f64, f64);

/// One Location1 satellite entry: constellation, number, band, frequency,
/// C/N0, elevation, azimuth, tracked, used, ephemeris, almanac.
type SatelliteEntry = (
    String,
    u16,
    String,
    f64,
    f64,
    f64,
    f64,
    bool,
    bool,
    bool,
    bool,
);

/// What Location1 says, as far as it is known.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Status {
    engine: Engine,
    used: u32,
    tracked: u32,
}

/// The session's view of Location1.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Known {
    status: Status,
    accuracies: Accuracies,
}

/// What a change brings besides the status.
#[derive(Debug, Default, PartialEq)]
struct News {
    fix: Option<Fix>,
    satellites: Option<Vec<Satellite>>,
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

fn known(value: f64) -> Option<f64> {
    value.is_finite().then_some(value)
}

fn known_f32(value: f64) -> Option<f32> {
    // Degrees and dB-Hz: well within f32.
    #[allow(clippy::cast_possible_truncation)]
    value.is_finite().then_some(value as f32)
}

fn constellation(name: &str) -> Option<Constellation> {
    Some(match name {
        "gps" => Constellation::Gps,
        "glonass" => Constellation::Glonass,
        "galileo" => Constellation::Galileo,
        "beidou" => Constellation::Beidou,
        "qzss" => Constellation::Qzss,
        "sbas" => Constellation::Sbas,
        "navic" => Constellation::Navic,
        _ => return None,
    })
}

/// The satellites in Location1's list, leaving out systems Android has no
/// name for.
fn satellites(entries: Vec<SatelliteEntry>) -> Vec<Satellite> {
    entries
        .into_iter()
        .filter_map(
            |(
                system,
                number,
                band,
                frequency,
                cn0,
                elevation,
                azimuth,
                tracked,
                used,
                ephemeris,
                almanac,
            )| {
                Some(Satellite {
                    constellation: constellation(&system)?,
                    number,
                    band,
                    frequency: known(frequency),
                    cn0: known_f32(cn0),
                    elevation: known_f32(elevation),
                    azimuth: known_f32(azimuth),
                    tracked,
                    used,
                    ephemeris,
                    almanac,
                })
            },
        )
        .take(MAX_SATELLITES)
        .collect()
}

/// The fix in a Location1 position, none before the first of a session.
fn fix(position: Position, used: u32, accuracies: Accuracies) -> Option<Fix> {
    let (latitude, longitude, accuracy, altitude, speed, heading, time_ms) = position;
    let (vertical, speed_accuracy, heading_accuracy) = accuracies;
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
            vertical_accuracy: known(vertical),
            speed_accuracy: known(speed_accuracy),
            heading_accuracy: known(heading_accuracy),
            time_ms,
            satellites: used,
        })
}

/// Applies changed Location1 properties; returns a new fix when Position
/// changed to one, and the satellites when they changed.
fn apply(known: &mut Known, changed: &HashMap<&str, Value<'_>>) -> News {
    let status = &mut known.status;
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
    if let Some(accuracies) = changed
        .get("Accuracies")
        .and_then(|value| Accuracies::try_from(value.try_clone().ok()?).ok())
    {
        known.accuracies = accuracies;
    }
    News {
        fix: changed
            .get("Position")
            .and_then(|value| Position::try_from(value.try_clone().ok()?).ok())
            .and_then(|position| fix(position, known.status.used, known.accuracies)),
        satellites: changed
            .get("Satellites")
            .and_then(|value| Vec::<SatelliteEntry>::try_from(value.try_clone().ok()?).ok())
            .map(satellites),
    }
}

const NONE: Accuracies = (f64::NAN, f64::NAN, f64::NAN);

async fn read_all(properties: &zbus::fdo::PropertiesProxy<'_>) -> zbus::Result<(Known, News)> {
    let all: HashMap<String, OwnedValue> = properties
        .get_all(zbus::names::InterfaceName::from_static_str_unchecked(
            INTERFACE,
        ))
        .await?;
    let changed: HashMap<&str, Value<'_>> = all
        .iter()
        .map(|(name, value)| (name.as_str(), Value::from(value.clone())))
        .collect();
    let mut known = Known {
        status: Status {
            engine: Engine::Unavailable,
            used: 0,
            tracked: 0,
        },
        accuracies: NONE,
    };
    let mut news = apply(&mut known, &changed);
    // A fix left from before this session is not current until dmail says so.
    if known.status.engine != Engine::Fixed {
        news.fix = None;
    }
    Ok((known, news))
}

/// Reports a change; false once the other side is gone.
fn tell(report: &dyn Fn(&Message) -> bool, news: News) -> bool {
    news.satellites
        .is_none_or(|satellites| report(&Message::LocationSatellites(satellites)))
        && news
            .fix
            .is_none_or(|fix| report(&Message::LocationFix(fix)))
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
    let (mut known, first) = read_all(&properties).await?;
    if !report(&known.status.message()) || !tell(report, first) {
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
        let before = known.status;
        let news = match event {
            Event::Changed(Some(signal)) => {
                let Ok(args) = signal.args() else { continue };
                if args.interface_name.as_str() != INTERFACE {
                    continue;
                }
                apply(&mut known, args.changed_properties())
            }
            // dmail came back without the hold: take it again.
            Event::Owner(Some(Some(_))) => {
                location.call_method("Start", &()).await?;
                let (now, news) = read_all(&properties).await?;
                known = now;
                news
            }
            Event::Owner(Some(None)) => {
                known.status.engine = Engine::Unavailable;
                News::default()
            }
            Event::Changed(None) | Event::Owner(None) => {
                return Err(zbus::Error::Failure("system bus connection ended".into()));
            }
            Event::Cancel => break,
        };
        if known.status != before && !report(&known.status.message()) {
            break;
        }
        if !tell(report, news) {
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
        let mut known = Known {
            status: Status {
                engine: Engine::Off,
                used: 0,
                tracked: 0,
            },
            accuracies: NONE,
        };
        let mut changed = HashMap::new();
        changed.insert("Accuracies", Value::from((7.0, 0.47, f64::NAN)));
        changed.insert(
            "Satellites",
            Value::from(vec![
                (
                    "glonass".to_owned(),
                    18u16,
                    "G1".to_owned(),
                    1_600_312_500.0,
                    19.6,
                    38.5,
                    f64::NAN,
                    true,
                    true,
                    true,
                    false,
                ),
                (
                    "elsewhere".to_owned(),
                    1u16,
                    String::new(),
                    f64::NAN,
                    f64::NAN,
                    f64::NAN,
                    f64::NAN,
                    false,
                    false,
                    false,
                    false,
                ),
            ]),
        );
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
            apply(&mut known, &changed),
            News {
                fix: Some(Fix {
                    latitude: 45.46,
                    longitude: 9.19,
                    accuracy: 10.0,
                    altitude: None,
                    speed: Some(0.4),
                    heading: None,
                    vertical_accuracy: Some(7.0),
                    speed_accuracy: Some(0.47),
                    heading_accuracy: None,
                    time_ms: 1_795_000_000_000,
                    satellites: 9,
                }),
                satellites: Some(vec![Satellite {
                    constellation: Constellation::Glonass,
                    number: 18,
                    band: "G1".into(),
                    frequency: Some(1_600_312_500.0),
                    cn0: Some(19.6),
                    elevation: Some(38.5),
                    azimuth: None,
                    tracked: true,
                    used: true,
                    ephemeris: true,
                    almanac: false,
                }]),
            }
        );
        assert_eq!(
            known.status,
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
        assert_eq!(fix((nan, nan, nan, nan, nan, nan, 0), 0, NONE), None);
        assert_eq!(engine("searching"), Engine::Searching);
        assert_eq!(engine("something new"), Engine::Unavailable);
    }
}
