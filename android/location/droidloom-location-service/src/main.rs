//! Android GNSS AIDL V7 service for Droidloom.
//!
//! Positions come from the host user's device broker, which holds dmail's
//! `org.dmail.Location1` on the host while Android navigates: `start` opens a
//! session on the broker's location socket and `stop` ends it, so the
//! modem's engine runs only while Android asks. Each final fix goes to
//! Android as `gnssLocationCb` with the accuracies the engine gives, and each
//! satellite list as `gnssSvStatusCb`, one entry for each satellite and band
//! with its signal type. Every extension
//! is unsupported, also the nullable ones: Android's GNSS code takes a null
//! extension for a real one and crashes system_server on it. The service
//! registers even without a host engine, because Android waits for a HAL its
//! manifest declares.

#![forbid(unsafe_code)]

use std::io;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use android_hardware_gnss::aidl::android::hardware::gnss::{
    GnssConstellationType::GnssConstellationType,
    GnssLocation::{self, GnssLocation as Location},
    GnssSignalType::GnssSignalType,
    IAGnss::IAGnss,
    IAGnssRil::IAGnssRil,
    IGnss::{
        self, BnGnss, GnssAidingData::GnssAidingData, PositionModeOptions::PositionModeOptions,
    },
    IGnssAntennaInfo::IGnssAntennaInfo,
    IGnssBatching::IGnssBatching,
    IGnssCallback::{
        GnssStatusValue::GnssStatusValue, GnssSvFlags::GnssSvFlags, GnssSvInfo::GnssSvInfo,
        GnssSystemInfo::GnssSystemInfo, IGnssCallback,
    },
    IGnssConfiguration::IGnssConfiguration,
    IGnssDebug::IGnssDebug,
    IGnssGeofence::IGnssGeofence,
    IGnssMeasurementInterface::IGnssMeasurementInterface,
    IGnssNavigationMessageInterface::IGnssNavigationMessageInterface,
    IGnssPowerIndication::IGnssPowerIndication,
    IGnssPsds::IGnssPsds,
    gnss_assistance::IGnssAssistanceInterface::IGnssAssistanceInterface,
    measurement_corrections::IMeasurementCorrectionsInterface::IMeasurementCorrectionsInterface,
    visibility_control::IGnssVisibilityControl::IGnssVisibilityControl,
};
use binder::{BinderFeatures, ExceptionCode, Interface, ProcessState, Status, Strong};
use droidloom_denial_ipc::{IpcError, SeqPacket};
use droidloom_device_protocol::{Constellation, Engine, Fix, MAX_RECORD, Message, Satellite};

const SERVICE_NAME: &str = "android.hardware.gnss.IGnss/default";
/// The supervisor binds the broker's location socket here.
const SOCKET: &str = "/dev/socket/droidloom/location";
/// How often a session's reader looks whether it was stopped.
const POLL: Duration = Duration::from_secs(1);

fn unsupported<T>() -> binder::Result<T> {
    Err(Status::new_exception(
        ExceptionCode::UNSUPPORTED_OPERATION,
        None,
    ))
}

fn location(fix: &Fix) -> Location {
    let mut flags = GnssLocation::HAS_LAT_LONG | GnssLocation::HAS_HORIZONTAL_ACCURACY;
    let mut known = |value: Option<f64>, flag: i32| {
        if value.is_some() {
            flags |= flag;
        }
        value.unwrap_or(0.0)
    };
    let altitude = known(fix.altitude, GnssLocation::HAS_ALTITUDE);
    let speed = known(fix.speed, GnssLocation::HAS_SPEED);
    let bearing = known(fix.heading, GnssLocation::HAS_BEARING);
    let vertical = known(fix.vertical_accuracy, GnssLocation::HAS_VERTICAL_ACCURACY);
    let speed_accuracy = known(fix.speed_accuracy, GnssLocation::HAS_SPEED_ACCURACY);
    let bearing_accuracy = known(fix.heading_accuracy, GnssLocation::HAS_BEARING_ACCURACY);
    Location {
        gnssLocationFlags: flags,
        latitudeDegrees: fix.latitude,
        longitudeDegrees: fix.longitude,
        altitudeMeters: altitude,
        speedMetersPerSec: speed,
        bearingDegrees: bearing,
        horizontalAccuracyMeters: fix.accuracy,
        verticalAccuracyMeters: vertical,
        speedAccuracyMetersPerSecond: speed_accuracy,
        bearingAccuracyDegrees: bearing_accuracy,
        timestampMillis: fix.time_ms,
        // No elapsed-realtime stamp: Android stamps the fix as it arrives,
        // which is when the engine reported it.
        ..Location::default()
    }
}

fn constellation(constellation: Constellation) -> GnssConstellationType {
    match constellation {
        Constellation::Gps => GnssConstellationType::GPS,
        Constellation::Glonass => GnssConstellationType::GLONASS,
        Constellation::Galileo => GnssConstellationType::GALILEO,
        Constellation::Beidou => GnssConstellationType::BEIDOU,
        Constellation::Qzss => GnssConstellationType::QZSS,
        Constellation::Sbas => GnssConstellationType::SBAS,
        Constellation::Navic => GnssConstellationType::IRNSS,
    }
}

/// The RINEX code a phone receiver tracks on the band, as Android names it.
fn code(band: &str) -> Option<&'static str> {
    Some(match band {
        "L1 C/A" | "L1" | "G1" | "G2" | "E1" => "C",
        "L1C" | "B1C" | "B2a" => "X",
        "L2C" => "L",
        "L5" | "E5a" | "E5b" => "Q",
        "L1S" => "Z",
        "B1I" | "B2I" => "I",
        _ => return None,
    })
}

/// NavIC broadcasts its L5 SPS signal as code A.
fn signal_code(satellite: &Satellite) -> Option<&'static str> {
    if satellite.constellation == Constellation::Navic && satellite.band == "L5" {
        return Some("A");
    }
    code(&satellite.band)
}

// Android 12 and later read the constellation and carrier frequency from
// signalType, older readers from the deprecated fields: both are filled.
#[allow(deprecated)]
fn satellite(satellite: &Satellite) -> GnssSvInfo {
    let constellation = constellation(satellite.constellation);
    let mut flags = 0;
    for (has, flag) in [
        (satellite.ephemeris, GnssSvFlags::HAS_EPHEMERIS_DATA),
        (satellite.almanac, GnssSvFlags::HAS_ALMANAC_DATA),
        (satellite.used, GnssSvFlags::USED_IN_FIX),
        (
            satellite.frequency.is_some(),
            GnssSvFlags::HAS_CARRIER_FREQUENCY,
        ),
    ] {
        if has {
            flags |= flag.0;
        }
    }
    let signal = satellite
        .frequency
        .zip(signal_code(satellite))
        .map(|(frequency, code)| GnssSignalType {
            constellation,
            carrierFrequencyHz: frequency,
            codeType: code.into(),
        });
    GnssSvInfo {
        svid: i32::from(satellite.number),
        constellation,
        // 0 dB-Hz: not heard.
        cN0Dbhz: satellite.cn0.unwrap_or(0.0),
        basebandCN0DbHz: 0.0,
        elevationDegrees: satellite.elevation.unwrap_or(0.0),
        azimuthDegrees: satellite.azimuth.unwrap_or(0.0),
        // Hz of a GNSS carrier: below 2e9, exact in i64.
        #[allow(clippy::cast_possible_truncation)]
        carrierFrequencyHz: satellite.frequency.map_or(0, |hz| hz.round() as i64),
        svFlag: flags,
        signalType: signal,
        elapsedRealtime: None,
    }
}

#[derive(Default)]
struct State {
    callback: Option<Strong<dyn IGnssCallback>>,
    /// The session running, counted so a stopped reader knows to end.
    session: u64,
    running: bool,
    link: Option<Arc<SeqPacket>>,
}

struct Gnss {
    state: Arc<Mutex<State>>,
}

impl Interface for Gnss {}

fn report(state: &Mutex<State>, session: u64, call: impl FnOnce(&Strong<dyn IGnssCallback>)) {
    let callback = {
        let state = state.lock().unwrap_or_else(PoisonError::into_inner);
        if !state.running || state.session != session {
            return;
        }
        state.callback.clone()
    };
    if let Some(callback) = callback {
        call(&callback);
    }
}

/// Passes the broker's records on until the session stops or the broker goes.
fn read(state: &Arc<Mutex<State>>, session: u64, socket: &SeqPacket) {
    let mut bytes = vec![0; MAX_RECORD];
    let mut engine = None;
    loop {
        {
            let state = state.lock().unwrap_or_else(PoisonError::into_inner);
            if !state.running || state.session != session {
                return;
            }
        }
        let record = match socket.receive_record_into(&mut bytes) {
            Ok(record) => record,
            Err(IpcError::Io(error))
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) =>
            {
                continue;
            }
            Err(error) => {
                eprintln!("droidloom location: broker left: {error}");
                return;
            }
        };
        match Message::decode(record.bytes) {
            Ok(Message::LocationFix(fix)) => {
                report(state, session, |callback| {
                    let _ = callback.gnssLocationCb(&location(&fix));
                });
            }
            Ok(Message::LocationSatellites(satellites)) => {
                let list: Vec<GnssSvInfo> = satellites.iter().map(satellite).collect();
                report(state, session, |callback| {
                    let _ = callback.gnssSvStatusCb(&list);
                });
            }
            Ok(Message::LocationState {
                engine: now,
                used,
                tracked,
            }) => {
                if engine != Some(now) {
                    eprintln!(
                        "droidloom location: host engine {}, {used} satellites used, {tracked} heard",
                        match now {
                            Engine::Off => "off",
                            Engine::Searching => "searching",
                            Engine::Fixed => "fixed",
                            Engine::Unavailable => "unavailable",
                        }
                    );
                    engine = Some(now);
                }
            }
            Ok(_) => {}
            Err(error) => {
                eprintln!("droidloom location: {error}");
                return;
            }
        }
    }
}

impl Gnss {
    fn stop_session(&self) -> bool {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if !state.running {
            return false;
        }
        state.running = false;
        if let Some(link) = state.link.take() {
            let _ = link.send_record(&Message::LocationStop.encode(), &[]);
        }
        true
    }

    fn status(&self, status: GnssStatusValue) {
        let callback = self
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .callback
            .clone();
        if let Some(callback) = callback {
            let _ = callback.gnssStatusCb(status);
        }
    }
}

impl IGnss::IGnss for Gnss {
    fn setCallback(&self, callback: &Strong<dyn IGnssCallback>) -> binder::Result<()> {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .callback = Some(callback.clone());
        // No scheduling, assistance, measurements or other extras: Android
        // runs the engine continuously and keeps its own fix interval.
        let _ = callback.gnssSetCapabilitiesCb(0);
        let _ = callback.gnssSetSystemInfoCb(&GnssSystemInfo {
            yearOfHw: 2026,
            name: "Droidloom host GNSS (dmail)".into(),
        });
        Ok(())
    }

    fn close(&self) -> binder::Result<()> {
        self.stop_session();
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .callback = None;
        Ok(())
    }

    fn start(&self) -> binder::Result<()> {
        {
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            if state.running {
                return Ok(());
            }
            let socket = SeqPacket::connect(SOCKET)
                .and_then(|socket| socket.set_io_timeout(POLL).map(|()| socket))
                .and_then(|socket| {
                    socket
                        .send_record(&Message::LocationStart.encode(), &[])
                        .map(|()| socket)
                });
            let socket = match socket {
                Ok(socket) => Arc::new(socket),
                Err(error) => {
                    eprintln!("droidloom location: no broker: {error}");
                    return Err(Status::new_service_specific_error(
                        IGnss::ERROR_GENERIC,
                        None,
                    ));
                }
            };
            state.session += 1;
            state.running = true;
            state.link = Some(Arc::clone(&socket));
            let (shared, session) = (Arc::clone(&self.state), state.session);
            std::thread::spawn(move || read(&shared, session, &socket));
        }
        self.status(GnssStatusValue::SESSION_BEGIN);
        Ok(())
    }

    fn stop(&self) -> binder::Result<()> {
        if self.stop_session() {
            self.status(GnssStatusValue::SESSION_END);
        }
        Ok(())
    }

    fn setPositionMode(&self, _options: &PositionModeOptions) -> binder::Result<()> {
        // dmail reports once a second whatever the mode; Android throttles.
        Ok(())
    }

    fn injectTime(
        &self,
        _time_ms: i64,
        _time_reference_ms: i64,
        _uncertainty_ms: i32,
    ) -> binder::Result<()> {
        // dmail gives the engine the host's synchronized time itself.
        Ok(())
    }

    fn injectLocation(&self, _location: &Location) -> binder::Result<()> {
        Ok(())
    }

    fn injectBestLocation(&self, _location: &Location) -> binder::Result<()> {
        Ok(())
    }

    fn deleteAidingData(&self, _aiding_data_flags: GnssAidingData) -> binder::Result<()> {
        Ok(())
    }

    fn startSvStatus(&self) -> binder::Result<()> {
        Ok(())
    }

    fn stopSvStatus(&self) -> binder::Result<()> {
        Ok(())
    }

    fn startNmea(&self) -> binder::Result<()> {
        Ok(())
    }

    fn stopNmea(&self) -> binder::Result<()> {
        Ok(())
    }

    fn getExtensionPsds(&self) -> binder::Result<Option<Strong<dyn IGnssPsds>>> {
        unsupported()
    }

    fn getExtensionGnssConfiguration(&self) -> binder::Result<Strong<dyn IGnssConfiguration>> {
        unsupported()
    }

    fn getExtensionGnssMeasurement(&self) -> binder::Result<Strong<dyn IGnssMeasurementInterface>> {
        unsupported()
    }

    fn getExtensionGnssPowerIndication(&self) -> binder::Result<Strong<dyn IGnssPowerIndication>> {
        unsupported()
    }

    fn getExtensionGnssBatching(&self) -> binder::Result<Option<Strong<dyn IGnssBatching>>> {
        unsupported()
    }

    fn getExtensionGnssGeofence(&self) -> binder::Result<Option<Strong<dyn IGnssGeofence>>> {
        unsupported()
    }

    fn getExtensionGnssNavigationMessage(
        &self,
    ) -> binder::Result<Option<Strong<dyn IGnssNavigationMessageInterface>>> {
        unsupported()
    }

    fn getExtensionAGnss(&self) -> binder::Result<Strong<dyn IAGnss>> {
        unsupported()
    }

    fn getExtensionAGnssRil(&self) -> binder::Result<Strong<dyn IAGnssRil>> {
        unsupported()
    }

    fn getExtensionGnssDebug(&self) -> binder::Result<Strong<dyn IGnssDebug>> {
        unsupported()
    }

    fn getExtensionGnssVisibilityControl(
        &self,
    ) -> binder::Result<Strong<dyn IGnssVisibilityControl>> {
        unsupported()
    }

    fn getExtensionGnssAntennaInfo(&self) -> binder::Result<Strong<dyn IGnssAntennaInfo>> {
        unsupported()
    }

    fn getExtensionMeasurementCorrections(
        &self,
    ) -> binder::Result<Option<Strong<dyn IMeasurementCorrectionsInterface>>> {
        unsupported()
    }

    fn getExtensionGnssAssistanceInterface(
        &self,
    ) -> binder::Result<Strong<dyn IGnssAssistanceInterface>> {
        unsupported()
    }
}

fn main() {
    let gnss = BnGnss::new_binder(
        Gnss {
            state: Arc::new(Mutex::new(State::default())),
        },
        BinderFeatures::default(),
    );
    ProcessState::start_thread_pool();
    binder::add_service(SERVICE_NAME, gnss.as_binder()).expect("register the GNSS HAL");
    ProcessState::join_thread_pool();
}
