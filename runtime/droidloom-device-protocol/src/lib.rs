//! Records between Android's haptics, camera and fingerprint services and the
//! host user's device broker, one per `SOCK_SEQPACKET` packet, little-endian.
//!
//! Every record starts with [`MAGIC`] and a 32-bit kind. Haptics are fire and
//! forget except the query; the camera list and connect each get one reply.
//! A camera session is braun's own socket, passed beside [`Message::CameraSession`].
//! A fingerprint query gets one reply; a verification sends events until one
//! that ends it ([`Scan::ends`]), also after a cancel.
//!
//! Credential prompts go the other way, from root's supervisor to the broker
//! on their own socket: [`Message::CredentialAsk`] shows the host's sheet, the
//! broker sends what the user typed as [`Message::CredentialSecret`] until the
//! supervisor closes it, or [`Message::CredentialCancelled`] when the user
//! closed it.

use std::fmt;

/// First bytes of every record.
pub const MAGIC: [u8; 8] = *b"DLDEV001";
/// Most steps in one haptic pattern, as hapticd takes them.
pub const MAX_STEPS: usize = 64;
/// Longest haptic pattern, as hapticd takes it.
pub const MAX_PATTERN_MS: u32 = 30_000;
/// Most cameras one list carries.
pub const MAX_CAMERAS: usize = 16;
/// Longest camera id, name or failure text.
pub const MAX_TEXT: usize = 256;
/// Most enrolled fingers one reader reports.
pub const MAX_FINGERS: usize = 16;
/// Longest PIN or password.
pub const MAX_SECRET: usize = 1024;
/// Largest record either side sends.
pub const MAX_RECORD: usize = 8192;

const HAPTICS_QUERY: u32 = 1;
const HAPTICS_DEVICE: u32 = 2;
const HAPTICS_PLAY: u32 = 3;
const HAPTICS_STOP: u32 = 4;
const CAMERA_LIST: u32 = 16;
const CAMERA_CAMERAS: u32 = 17;
const CAMERA_CONNECT: u32 = 18;
const CAMERA_SESSION: u32 = 19;
const CAMERA_UNAVAILABLE: u32 = 20;
const FINGERPRINT_QUERY: u32 = 32;
const FINGERPRINT_READER: u32 = 33;
const FINGERPRINT_VERIFY: u32 = 34;
const FINGERPRINT_CANCEL: u32 = 35;
const FINGERPRINT_SCAN: u32 = 36;
const CREDENTIAL_ASK: u32 = 48;
const CREDENTIAL_SECRET: u32 = 49;
const CREDENTIAL_WRONG: u32 = 50;
const CREDENTIAL_CANCELLED: u32 = 51;
const CREDENTIAL_CLOSE: u32 = 52;

/// One step of a haptic pattern: `intensity` from 0 to 1 for `ms`; 0 is a pause.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Step {
    /// Milliseconds.
    pub ms: u32,
    /// Strength, 0 to 1.
    pub intensity: f32,
}

/// Which way a camera looks.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Facing {
    /// Toward the user.
    Front,
    /// Away from the user.
    Back,
    /// Plugged in.
    External,
}

/// One host camera, as braun describes it.
#[derive(Clone, Debug, PartialEq)]
pub struct Camera {
    /// braun's id.
    pub id: String,
    /// Model name.
    pub name: String,
    /// Which way it looks.
    pub facing: Facing,
    /// Full frame width, upright.
    pub width: u32,
    /// Full frame height, upright.
    pub height: u32,
    /// The main camera's zoom that shows what this one does; 0 when unknown.
    pub zoom: f32,
    /// It can focus.
    pub lens: bool,
}

/// What happened during a fingerprint verification.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Scan {
    /// A finger is on the reader.
    Touch,
    /// The finger left.
    Lift,
    /// The reader could not read that touch; it listens again.
    Retry,
    /// The finger is one of the user's. Ends the verification.
    Match,
    /// The finger is not one of the user's; the reader listens again.
    NoMatch,
    /// The reader is gone, busy or failed. Ends the verification.
    Unavailable,
    /// Android cancelled. Ends the verification.
    Cancelled,
}

impl Scan {
    /// No further event of this verification follows.
    pub fn ends(self) -> bool {
        matches!(self, Scan::Match | Scan::Unavailable | Scan::Cancelled)
    }
}

const SCANS: [Scan; 7] = [
    Scan::Touch,
    Scan::Lift,
    Scan::Retry,
    Scan::Match,
    Scan::NoMatch,
    Scan::Unavailable,
    Scan::Cancelled,
];

/// A record.
#[derive(Clone, Debug, PartialEq)]
pub enum Message {
    /// Android asks whether the host has a vibrator.
    HapticsQuery,
    /// The host's default vibrator.
    HapticsDevice {
        /// There is one.
        present: bool,
        /// It takes strengths between off and full.
        intensity: bool,
    },
    /// Play this pattern now, replacing the one playing.
    HapticsPlay(Vec<Step>),
    /// Stop the pattern playing.
    HapticsStop,
    /// Android asks for the host's cameras.
    CameraList,
    /// The host's cameras.
    Cameras(Vec<Camera>),
    /// Android asks for a braun session.
    CameraConnect,
    /// A braun session; its socket travels beside this record.
    CameraSession,
    /// No session, and why.
    CameraUnavailable(String),
    /// Android asks for the host's fingerprint reader.
    FingerprintQuery,
    /// The host's fingerprint reader.
    FingerprintReader {
        /// There is one.
        present: bool,
        /// The fingers the user enrolled, by fprintd's names.
        fingers: Vec<String>,
    },
    /// Start one verification against the user's enrolled fingers.
    FingerprintVerify,
    /// Stop the verification running.
    FingerprintCancel,
    /// An event of the verification running.
    FingerprintScan(Scan),
    /// Ask the user for their screen lock on behalf of an app.
    CredentialAsk {
        /// The app's name.
        app: String,
        /// What the app asked with; may be empty.
        title: String,
        /// May be empty.
        description: String,
        /// The PIN can prove the user.
        pin: bool,
    },
    /// What the user typed: the PIN, or else the account password.
    CredentialSecret {
        /// It is the PIN.
        pin: bool,
        /// The bytes typed; wiped by whoever drops them.
        secret: Vec<u8>,
    },
    /// It did not prove the user; ask again after `retry_ms`.
    CredentialWrong {
        /// Milliseconds to wait first, or 0.
        retry_ms: u32,
    },
    /// The user closed the sheet.
    CredentialCancelled,
    /// Close the sheet: `accepted` when the credential proved the user.
    CredentialClose {
        /// The user proved themself.
        accepted: bool,
    },
}

/// A record that does not decode.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Invalid(&'static str);

impl fmt::Display for Invalid {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "invalid device record: {}", self.0)
    }
}

impl std::error::Error for Invalid {}

struct Writer(Vec<u8>);

impl Writer {
    fn new(kind: u32) -> Self {
        let mut bytes = Vec::with_capacity(64);
        bytes.extend_from_slice(&MAGIC);
        bytes.extend_from_slice(&kind.to_le_bytes());
        Self(bytes)
    }
    fn u8(&mut self, value: u8) {
        self.0.push(value);
    }
    fn u32(&mut self, value: u32) {
        self.0.extend_from_slice(&value.to_le_bytes());
    }
    fn f32(&mut self, value: f32) {
        self.0.extend_from_slice(&value.to_le_bytes());
    }
    fn text(&mut self, value: &str) {
        let mut end = value.len().min(MAX_TEXT);
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        // MAX_TEXT fits a u16.
        #[allow(clippy::cast_possible_truncation)]
        self.0.extend_from_slice(&(end as u16).to_le_bytes());
        self.0.extend_from_slice(&value.as_bytes()[..end]);
    }
    fn secret(&mut self, value: &[u8]) {
        let value = &value[..value.len().min(MAX_SECRET)];
        // MAX_SECRET fits a u16.
        #[allow(clippy::cast_possible_truncation)]
        self.0
            .extend_from_slice(&(value.len() as u16).to_le_bytes());
        self.0.extend_from_slice(value);
    }
}

struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    fn take<const N: usize>(&mut self) -> Result<[u8; N], Invalid> {
        let (head, rest) = self.0.split_at_checked(N).ok_or(Invalid("short"))?;
        self.0 = rest;
        Ok(head.try_into().expect("split at N"))
    }
    fn u8(&mut self) -> Result<u8, Invalid> {
        Ok(self.take::<1>()?[0])
    }
    fn u32(&mut self) -> Result<u32, Invalid> {
        self.take().map(u32::from_le_bytes)
    }
    fn f32(&mut self) -> Result<f32, Invalid> {
        let value = f32::from_le_bytes(self.take()?);
        value
            .is_finite()
            .then_some(value)
            .ok_or(Invalid("not a number"))
    }
    fn text(&mut self) -> Result<String, Invalid> {
        let length = usize::from(u16::from_le_bytes(self.take()?));
        if length > MAX_TEXT {
            return Err(Invalid("text too long"));
        }
        let (head, rest) = self.0.split_at_checked(length).ok_or(Invalid("short"))?;
        self.0 = rest;
        String::from_utf8(head.to_vec()).map_err(|_| Invalid("text not UTF-8"))
    }
    fn secret(&mut self) -> Result<Vec<u8>, Invalid> {
        let length = usize::from(u16::from_le_bytes(self.take()?));
        if length > MAX_SECRET {
            return Err(Invalid("secret too long"));
        }
        let (head, rest) = self.0.split_at_checked(length).ok_or(Invalid("short"))?;
        self.0 = rest;
        Ok(head.to_vec())
    }
    fn count(&mut self, most: usize) -> Result<usize, Invalid> {
        usize::try_from(self.u32()?)
            .ok()
            .filter(|count| *count <= most)
            .ok_or(Invalid("too many"))
    }
    fn end(&self) -> Result<(), Invalid> {
        self.0
            .is_empty()
            .then_some(())
            .ok_or(Invalid("trailing bytes"))
    }
}

/// Whether hapticd takes this pattern: 1 to 64 steps, at most 30 s.
pub fn valid_pattern(steps: &[Step]) -> bool {
    !steps.is_empty()
        && steps.len() <= MAX_STEPS
        && steps
            .iter()
            .all(|step| step.intensity.is_finite() && (0.0..=1.0).contains(&step.intensity))
        && steps
            .iter()
            .try_fold(0u32, |total, step| total.checked_add(step.ms))
            .is_some_and(|total| total > 0 && total <= MAX_PATTERN_MS)
}

impl Message {
    /// The record's bytes.
    pub fn encode(&self) -> Vec<u8> {
        let writer = match self {
            Message::HapticsQuery => Writer::new(HAPTICS_QUERY),
            Message::HapticsDevice { present, intensity } => {
                let mut writer = Writer::new(HAPTICS_DEVICE);
                writer.u8(u8::from(*present));
                writer.u8(u8::from(*intensity));
                writer
            }
            Message::HapticsPlay(steps) => {
                let mut writer = Writer::new(HAPTICS_PLAY);
                let steps = &steps[..steps.len().min(MAX_STEPS)];
                // MAX_STEPS fits a u32.
                #[allow(clippy::cast_possible_truncation)]
                writer.u32(steps.len() as u32);
                for step in steps {
                    writer.u32(step.ms);
                    writer.f32(step.intensity);
                }
                writer
            }
            Message::HapticsStop => Writer::new(HAPTICS_STOP),
            Message::CameraList => Writer::new(CAMERA_LIST),
            Message::Cameras(cameras) => {
                let mut writer = Writer::new(CAMERA_CAMERAS);
                let cameras = &cameras[..cameras.len().min(MAX_CAMERAS)];
                #[allow(clippy::cast_possible_truncation)]
                writer.u32(cameras.len() as u32);
                for camera in cameras {
                    writer.text(&camera.id);
                    writer.text(&camera.name);
                    writer.u8(match camera.facing {
                        Facing::Front => 0,
                        Facing::Back => 1,
                        Facing::External => 2,
                    });
                    writer.u8(u8::from(camera.lens));
                    writer.u32(camera.width);
                    writer.u32(camera.height);
                    writer.f32(camera.zoom);
                }
                writer
            }
            Message::CameraConnect => Writer::new(CAMERA_CONNECT),
            Message::CameraSession => Writer::new(CAMERA_SESSION),
            Message::CameraUnavailable(reason) => {
                let mut writer = Writer::new(CAMERA_UNAVAILABLE);
                writer.text(reason);
                writer
            }
            Message::FingerprintQuery => Writer::new(FINGERPRINT_QUERY),
            Message::FingerprintReader { present, fingers } => {
                let mut writer = Writer::new(FINGERPRINT_READER);
                writer.u8(u8::from(*present));
                let fingers = &fingers[..fingers.len().min(MAX_FINGERS)];
                // MAX_FINGERS fits a u32.
                #[allow(clippy::cast_possible_truncation)]
                writer.u32(fingers.len() as u32);
                for finger in fingers {
                    writer.text(finger);
                }
                writer
            }
            Message::FingerprintVerify => Writer::new(FINGERPRINT_VERIFY),
            Message::FingerprintCancel => Writer::new(FINGERPRINT_CANCEL),
            Message::FingerprintScan(scan) => {
                let mut writer = Writer::new(FINGERPRINT_SCAN);
                // SCANS has seven entries.
                #[allow(clippy::cast_possible_truncation)]
                writer.u8(SCANS.iter().position(|known| known == scan).unwrap_or(0) as u8);
                writer
            }
            Message::CredentialAsk {
                app,
                title,
                description,
                pin,
            } => {
                let mut writer = Writer::new(CREDENTIAL_ASK);
                writer.text(app);
                writer.text(title);
                writer.text(description);
                writer.u8(u8::from(*pin));
                writer
            }
            Message::CredentialSecret { pin, secret } => {
                let mut writer = Writer::new(CREDENTIAL_SECRET);
                writer.u8(u8::from(*pin));
                writer.secret(secret);
                writer
            }
            Message::CredentialWrong { retry_ms } => {
                let mut writer = Writer::new(CREDENTIAL_WRONG);
                writer.u32(*retry_ms);
                writer
            }
            Message::CredentialCancelled => Writer::new(CREDENTIAL_CANCELLED),
            Message::CredentialClose { accepted } => {
                let mut writer = Writer::new(CREDENTIAL_CLOSE);
                writer.u8(u8::from(*accepted));
                writer
            }
        };
        writer.0
    }

    /// The record in `bytes`.
    ///
    /// # Errors
    /// A record of another protocol, an unknown kind, or one whose fields do
    /// not fit their bounds.
    pub fn decode(bytes: &[u8]) -> Result<Self, Invalid> {
        let mut reader = Reader(bytes);
        if reader.take::<8>()? != MAGIC {
            return Err(Invalid("not a device record"));
        }
        let message = match reader.u32()? {
            HAPTICS_QUERY => Message::HapticsQuery,
            HAPTICS_DEVICE => Message::HapticsDevice {
                present: reader.u8()? != 0,
                intensity: reader.u8()? != 0,
            },
            HAPTICS_PLAY => {
                let count = reader.count(MAX_STEPS)?;
                let mut steps = Vec::with_capacity(count);
                for _ in 0..count {
                    steps.push(Step {
                        ms: reader.u32()?,
                        intensity: reader.f32()?,
                    });
                }
                if !valid_pattern(&steps) {
                    return Err(Invalid("pattern out of bounds"));
                }
                Message::HapticsPlay(steps)
            }
            HAPTICS_STOP => Message::HapticsStop,
            CAMERA_LIST => Message::CameraList,
            CAMERA_CAMERAS => {
                let count = reader.count(MAX_CAMERAS)?;
                let mut cameras = Vec::with_capacity(count);
                for _ in 0..count {
                    cameras.push(Camera {
                        id: reader.text()?,
                        name: reader.text()?,
                        facing: match reader.u8()? {
                            0 => Facing::Front,
                            1 => Facing::Back,
                            2 => Facing::External,
                            _ => return Err(Invalid("unknown facing")),
                        },
                        lens: reader.u8()? != 0,
                        width: reader.u32()?,
                        height: reader.u32()?,
                        zoom: reader.f32()?,
                    });
                }
                Message::Cameras(cameras)
            }
            CAMERA_CONNECT => Message::CameraConnect,
            CAMERA_SESSION => Message::CameraSession,
            CAMERA_UNAVAILABLE => Message::CameraUnavailable(reader.text()?),
            FINGERPRINT_QUERY => Message::FingerprintQuery,
            FINGERPRINT_READER => {
                let present = reader.u8()? != 0;
                let count = reader.count(MAX_FINGERS)?;
                let mut fingers = Vec::with_capacity(count);
                for _ in 0..count {
                    fingers.push(reader.text()?);
                }
                Message::FingerprintReader { present, fingers }
            }
            FINGERPRINT_VERIFY => Message::FingerprintVerify,
            FINGERPRINT_CANCEL => Message::FingerprintCancel,
            FINGERPRINT_SCAN => Message::FingerprintScan(
                *SCANS
                    .get(usize::from(reader.u8()?))
                    .ok_or(Invalid("unknown scan"))?,
            ),
            CREDENTIAL_ASK => Message::CredentialAsk {
                app: reader.text()?,
                title: reader.text()?,
                description: reader.text()?,
                pin: reader.u8()? != 0,
            },
            CREDENTIAL_SECRET => Message::CredentialSecret {
                pin: reader.u8()? != 0,
                secret: reader.secret()?,
            },
            CREDENTIAL_WRONG => Message::CredentialWrong {
                retry_ms: reader.u32()?,
            },
            CREDENTIAL_CANCELLED => Message::CredentialCancelled,
            CREDENTIAL_CLOSE => Message::CredentialClose {
                accepted: reader.u8()? != 0,
            },
            _ => return Err(Invalid("unknown kind")),
        };
        reader.end()?;
        Ok(message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_round_trip() {
        for message in [
            Message::HapticsQuery,
            Message::HapticsDevice {
                present: true,
                intensity: false,
            },
            Message::HapticsPlay(vec![
                Step {
                    ms: 12,
                    intensity: 0.7,
                },
                Step {
                    ms: 60,
                    intensity: 0.0,
                },
            ]),
            Message::HapticsStop,
            Message::CameraList,
            Message::Cameras(vec![Camera {
                id: "imx890".into(),
                name: "IMX890".into(),
                facing: Facing::Back,
                width: 3072,
                height: 4096,
                zoom: 1.0,
                lens: true,
            }]),
            Message::CameraConnect,
            Message::CameraSession,
            Message::CameraUnavailable("braun is not installed".into()),
            Message::FingerprintQuery,
            Message::FingerprintReader {
                present: true,
                fingers: vec!["right-thumb".into(), "left-index-finger".into()],
            },
            Message::FingerprintVerify,
            Message::FingerprintCancel,
            Message::CredentialAsk {
                app: "Play Store".into(),
                title: "Confirm it's you".into(),
                description: String::new(),
                pin: true,
            },
            Message::CredentialSecret {
                pin: false,
                secret: b"correct horse".to_vec(),
            },
            Message::CredentialWrong { retry_ms: 30_000 },
            Message::CredentialCancelled,
            Message::CredentialClose { accepted: true },
        ]
        .into_iter()
        .chain(SCANS.map(Message::FingerprintScan))
        {
            assert_eq!(Message::decode(&message.encode()), Ok(message));
        }
    }

    #[test]
    fn patterns_hapticd_refuses_do_not_decode() {
        let long = Message::HapticsPlay(vec![Step {
            ms: 30_001,
            intensity: 1.0,
        }]);
        assert!(Message::decode(&long.encode()).is_err());
        let silent = Message::HapticsPlay(vec![Step {
            ms: 0,
            intensity: 1.0,
        }]);
        assert!(Message::decode(&silent.encode()).is_err());
        let loud = Message::HapticsPlay(vec![Step {
            ms: 10,
            intensity: 1.5,
        }]);
        assert!(Message::decode(&loud.encode()).is_err());
    }

    #[test]
    fn other_records_are_refused() {
        assert!(Message::decode(b"DLAUD001\x01\0\0\0").is_err());
        let mut trailing = Message::HapticsStop.encode();
        trailing.push(0);
        assert!(Message::decode(&trailing).is_err());
    }
}
