//! Fixed-size, descriptor-free records for the Android framework bridge.
//!
//! The Wayland compositor remains the physical-input authority. This protocol
//! carries touch and keyboard events already routed to one authenticated
//! Android task surface.

#![no_std]
#![forbid(unsafe_code)]

use core::fmt;

/// Marker at the start of every input record.
pub const MAGIC: [u8; 4] = *b"DLIN";
/// Wire protocol major version.
pub const PROTOCOL_MAJOR: u16 = 7;

/// Build metadata retained by the native sender for offline package verification.
pub const BUILD_COMPATIBILITY: [u8; 26] = compatibility_marker(PROTOCOL_MAJOR);

const fn compatibility_marker(mut version: u16) -> [u8; 26] {
    let mut marker = *b"DROIDLOOM_INPUT_ABI=00000;";
    let mut index = 24;
    loop {
        marker[index] = b'0' + (version % 10) as u8;
        version /= 10;
        if index == 20 {
            break;
        }
        index -= 1;
    }
    marker
}

/// Exact size of one sequenced-packet record.
pub const RECORD_BYTES: usize = 48;
/// Record kind for a touchscreen contact update.
pub const KIND_TOUCH: u8 = 1;
/// Record kind for one Linux input key transition.
pub const KIND_KEY: u8 = 2;
/// Record kind for one Android task-bounds update.
pub const KIND_TASK_BOUNDS: u8 = 3;
/// Record kind for one Android task focus update.
pub const KIND_TASK_FOCUS: u8 = 4;
/// Record kind for a request to remove one Android task.
pub const KIND_TASK_CLOSE: u8 = 5;
/// Record kind for the aggregate host display-power state.
pub const KIND_DISPLAY_POWER: u8 = 6;
/// Highest pointer identity accepted by Android `MotionEvent`.
pub const MAX_POINTER_ID: u32 = 31;
/// Highest key identity defined by Linux's evdev input ABI.
pub const MAX_EVDEV_KEYCODE: u32 = 0x2ff;

/// Invalid task/display/input data at the framework bridge boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InputProtocolError {
    /// Android display IDs are signed nonnegative 32-bit integers.
    AndroidDisplay,
    /// Android accepts pointer identities from zero through 31.
    PointerId,
    /// Linux evdev key identities are bounded by `KEY_MAX`.
    KeyCode,
    /// Touch actions are down, motion, up, or cancel.
    Action,
    /// Logical-to-buffer scale components must both be non-zero.
    Scale,
    /// A scaled signed 16.16 coordinate cannot fit on the wire.
    Coordinate,
    /// Android task identities are positive signed 32-bit integers.
    TaskId,
    /// Task bounds must be non-zero signed 32-bit dimensions.
    TaskExtent,
    /// A framework incarnation must be positive and fit a Java long.
    Incarnation,
    /// Record length, version, reserved fields or kind is invalid.
    Record,
}

impl fmt::Display for InputProtocolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::AndroidDisplay => "Android display ID is outside the signed framework range",
            Self::PointerId => "touch pointer identity exceeds Android's limit",
            Self::KeyCode => "key identity exceeds Linux evdev's limit",
            Self::Action => "input action is outside the protocol range",
            Self::Scale => "logical-to-buffer scale components must be non-zero",
            Self::Coordinate => "scaled touch coordinate exceeds the signed 16.16 wire range",
            Self::TaskId => "Android task ID is outside the positive framework range",
            Self::TaskExtent => "Android task bounds are outside the framework range",
            Self::Incarnation => "input requires a positive framework task incarnation",
            Self::Record => "invalid routed input record",
        })
    }
}

/// Convert one signed 16.16 logical coordinate to buffer pixels.
///
/// # Errors
///
/// Rejects zero scale components and results outside the signed wire range.
pub fn scale_fixed(
    value: i32,
    numerator: u32,
    denominator: u32,
) -> Result<i32, InputProtocolError> {
    if numerator == 0 || denominator == 0 {
        return Err(InputProtocolError::Scale);
    }
    let scaled = i64::from(value)
        .checked_mul(i64::from(numerator))
        .ok_or(InputProtocolError::Coordinate)?
        / i64::from(denominator);
    i32::try_from(scaled).map_err(|_| InputProtocolError::Coordinate)
}

/// Encode one routed touchscreen sample.
///
/// # Errors
///
/// Rejects display and pointer identities Android cannot represent.
#[allow(clippy::too_many_arguments)]
pub fn encode_touch(
    android_display: u32,
    task_id: u64,
    timestamp_nanos: u64,
    action: u8,
    pointer_id: u32,
    x_fixed: i32,
    y_fixed: i32,
    pressure: u16,
    incarnation: u64,
) -> Result<[u8; RECORD_BYTES], InputProtocolError> {
    if incarnation == 0 || incarnation > i64::MAX as u64 {
        return Err(InputProtocolError::Incarnation);
    }
    if android_display > i32::MAX as u32 {
        return Err(InputProtocolError::AndroidDisplay);
    }
    let task_id = u32::try_from(task_id).map_err(|_| InputProtocolError::TaskId)?;
    if task_id == 0 || task_id > i32::MAX as u32 {
        return Err(InputProtocolError::TaskId);
    }
    if pointer_id > MAX_POINTER_ID {
        return Err(InputProtocolError::PointerId);
    }
    if action > 3 {
        return Err(InputProtocolError::Action);
    }

    let mut record = [0_u8; RECORD_BYTES];
    record[0..4].copy_from_slice(&MAGIC);
    record[4..6].copy_from_slice(&PROTOCOL_MAJOR.to_le_bytes());
    record[6] = KIND_TOUCH;
    record[7] = action;
    record[8..12].copy_from_slice(&android_display.to_le_bytes());
    record[12..16].copy_from_slice(&pointer_id.to_le_bytes());
    record[16..24].copy_from_slice(&timestamp_nanos.to_le_bytes());
    record[24..28].copy_from_slice(&x_fixed.to_le_bytes());
    record[28..32].copy_from_slice(&y_fixed.to_le_bytes());
    record[32..34].copy_from_slice(&pressure.to_le_bytes());
    // Bytes 34..36 are reserved and remain zero.
    record[36..40].copy_from_slice(&task_id.to_le_bytes());
    record[40..48].copy_from_slice(&incarnation.to_le_bytes());
    Ok(record)
}

/// Encode one routed Linux input key transition.
///
/// The key code remains in evdev space on the wire. The Android framework
/// bridge converts it to a `KeyEvent` code while retaining the original scan
/// code for diagnostics and policy.
///
/// # Errors
///
/// Rejects display IDs, key identities, and actions outside their platform
/// ranges.
#[allow(clippy::too_many_arguments)]
pub fn encode_key(
    android_display: u32,
    task_id: u64,
    timestamp_nanos: u64,
    action: u8,
    keycode: u32,
    repeat: u16,
    route_serial: u64,
    incarnation: u64,
) -> Result<[u8; RECORD_BYTES], InputProtocolError> {
    if incarnation == 0 || incarnation > i64::MAX as u64 {
        return Err(InputProtocolError::Incarnation);
    }
    if android_display > i32::MAX as u32 {
        return Err(InputProtocolError::AndroidDisplay);
    }
    let task_id = u32::try_from(task_id).map_err(|_| InputProtocolError::TaskId)?;
    if task_id == 0 || task_id > i32::MAX as u32 {
        return Err(InputProtocolError::TaskId);
    }
    if keycode > MAX_EVDEV_KEYCODE {
        return Err(InputProtocolError::KeyCode);
    }
    if action > 1 {
        return Err(InputProtocolError::Action);
    }

    let mut record = [0_u8; RECORD_BYTES];
    record[0..4].copy_from_slice(&MAGIC);
    record[4..6].copy_from_slice(&PROTOCOL_MAJOR.to_le_bytes());
    record[6] = KIND_KEY;
    record[7] = action;
    record[8..12].copy_from_slice(&android_display.to_le_bytes());
    record[12..16].copy_from_slice(&keycode.to_le_bytes());
    record[16..24].copy_from_slice(&timestamp_nanos.to_le_bytes());
    record[24..26].copy_from_slice(&repeat.to_le_bytes());
    // Bytes 26..28 are reserved and remain zero.
    record[28..36].copy_from_slice(&route_serial.to_le_bytes());
    record[36..40].copy_from_slice(&task_id.to_le_bytes());
    record[40..48].copy_from_slice(&incarnation.to_le_bytes());
    Ok(record)
}

/// Encode a requested content size for one Android task.
///
/// The Android task stays at a private origin because the Wayland compositor
/// owns its real desktop position. Its pixel bounds track the toplevel's
/// physical Wayland buffer size. The scale remains explicit for coordinate
/// conversion only; a task's density travels separately, as the output's
/// density setting, never derived from this scale here.
///
/// # Errors
///
/// Rejects task identities and dimensions Android cannot represent.
pub fn encode_task_bounds(
    task_id: u64,
    android_display: u32,
    width: u32,
    height: u32,
    scale_numerator: u32,
    scale_denominator: u32,
) -> Result<[u8; RECORD_BYTES], InputProtocolError> {
    let task_id = u32::try_from(task_id).map_err(|_| InputProtocolError::TaskId)?;
    if task_id == 0 || task_id > i32::MAX as u32 {
        return Err(InputProtocolError::TaskId);
    }
    if width == 0 || width > i32::MAX as u32 || height == 0 || height > i32::MAX as u32 {
        return Err(InputProtocolError::TaskExtent);
    }
    if android_display > i32::MAX as u32 {
        return Err(InputProtocolError::AndroidDisplay);
    }
    if scale_numerator == 0 || scale_denominator == 0 {
        return Err(InputProtocolError::Scale);
    }

    let mut record = [0_u8; RECORD_BYTES];
    record[0..4].copy_from_slice(&MAGIC);
    record[4..6].copy_from_slice(&PROTOCOL_MAJOR.to_le_bytes());
    record[6] = KIND_TASK_BOUNDS;
    record[8..12].copy_from_slice(&task_id.to_le_bytes());
    record[12..16].copy_from_slice(&width.to_le_bytes());
    record[16..20].copy_from_slice(&height.to_le_bytes());
    record[20..24].copy_from_slice(&android_display.to_le_bytes());
    record[24..28].copy_from_slice(&scale_numerator.to_le_bytes());
    record[28..32].copy_from_slice(&scale_denominator.to_le_bytes());
    Ok(record)
}

/// Encode the host keyboard-focus state for one Android task.
///
/// # Errors
///
/// Rejects task identities Android cannot represent.
pub fn encode_task_focus(
    task_id: u64,
    focused: bool,
) -> Result<[u8; RECORD_BYTES], InputProtocolError> {
    let task_id = u32::try_from(task_id).map_err(|_| InputProtocolError::TaskId)?;
    if task_id == 0 || task_id > i32::MAX as u32 {
        return Err(InputProtocolError::TaskId);
    }

    let mut record = [0_u8; RECORD_BYTES];
    record[0..4].copy_from_slice(&MAGIC);
    record[4..6].copy_from_slice(&PROTOCOL_MAJOR.to_le_bytes());
    record[6] = KIND_TASK_FOCUS;
    record[7] = u8::from(focused);
    record[8..12].copy_from_slice(&task_id.to_le_bytes());
    Ok(record)
}

/// Encode a request to finish and remove one Android task.
///
/// # Errors
///
/// Rejects task identities Android cannot represent.
pub fn encode_task_close(task_id: u64) -> Result<[u8; RECORD_BYTES], InputProtocolError> {
    let task_id = u32::try_from(task_id).map_err(|_| InputProtocolError::TaskId)?;
    if task_id == 0 || task_id > i32::MAX as u32 {
        return Err(InputProtocolError::TaskId);
    }

    let mut record = [0_u8; RECORD_BYTES];
    record[0..4].copy_from_slice(&MAGIC);
    record[4..6].copy_from_slice(&PROTOCOL_MAJOR.to_le_bytes());
    record[6] = KIND_TASK_CLOSE;
    record[8..12].copy_from_slice(&task_id.to_le_bytes());
    Ok(record)
}

/// Encode whether any physical host display is powered on.
pub fn encode_display_power(powered: bool) -> [u8; RECORD_BYTES] {
    let mut record = [0_u8; RECORD_BYTES];
    record[0..4].copy_from_slice(&MAGIC);
    record[4..6].copy_from_slice(&PROTOCOL_MAJOR.to_le_bytes());
    record[6] = KIND_DISPLAY_POWER;
    record[7] = u8::from(powered);
    record
}

/// Validated sample addressed to one framework task incarnation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sample {
    /// Android logical display selected by the framework.
    pub display: i32,
    /// Attached Android leaf task.
    pub task: i32,
    /// Framework lifetime identity, preventing task ID reuse.
    pub incarnation: u64,
    /// Monotonic nanoseconds from the host input source.
    pub timestamp: u64,
    /// Wire touch or key action.
    pub action: u8,
    /// Pointer ID or evdev scan code.
    pub code: u32,
    /// Kind-specific validated data.
    pub payload: Payload,
}

/// Kind-specific input data.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Payload {
    /// Task-local physical pixel coordinates and normalized pressure.
    Touch {
        /// Horizontal task-local coordinate.
        x: f32,
        /// Vertical task-local coordinate.
        y: f32,
        /// Normalized contact pressure.
        pressure: f32,
    },
    /// Keyboard state and end-to-end trace identity.
    Key {
        /// Host-generated repeat count.
        repeat: u16,
        /// Host route sequence number.
        serial: u64,
    },
}

/// One validated native input receiver record.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Record {
    /// Task-scoped touch or key sample.
    Input(Sample),
    /// Whether any physical host output is powered.
    DisplayPower(bool),
}

/// Decode exactly one descriptor-free packet. Retired task-control kinds are rejected.
///
/// # Errors
/// Rejects malformed, incompatible or out-of-range records.
// Exact length is checked before the fixed offsets; Android represents native
// MotionEvent coordinates as f32, including the 16.16 conversion's rounding.
#[allow(clippy::missing_panics_doc, clippy::cast_precision_loss)]
pub fn decode(bytes: &[u8]) -> Result<Record, InputProtocolError> {
    if bytes.len() != RECORD_BYTES
        || bytes[..4] != MAGIC
        || bytes[4..6] != PROTOCOL_MAJOR.to_le_bytes()
    {
        return Err(InputProtocolError::Record);
    }
    if bytes[6] == KIND_DISPLAY_POWER {
        if bytes[7] > 1 || bytes[8..].iter().any(|byte| *byte != 0) {
            return Err(InputProtocolError::Record);
        }
        return Ok(Record::DisplayPower(bytes[7] != 0));
    }
    let word = |offset| u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());
    let wide = |offset| u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap());
    let display = i32::try_from(word(8)).map_err(|_| InputProtocolError::AndroidDisplay)?;
    let task = i32::try_from(word(36)).map_err(|_| InputProtocolError::TaskId)?;
    if task <= 0 {
        return Err(InputProtocolError::TaskId);
    }
    let incarnation = wide(40);
    if incarnation == 0 || incarnation > i64::MAX as u64 {
        return Err(InputProtocolError::Incarnation);
    }
    let timestamp = wide(16);
    if timestamp == 0 || timestamp > i64::MAX as u64 {
        return Err(InputProtocolError::Record);
    }
    let action = bytes[7];
    let code = word(12);
    let payload = match bytes[6] {
        KIND_TOUCH if action <= 3 && code <= MAX_POINTER_ID && bytes[34..36] == [0, 0] => {
            Payload::Touch {
                x: i32::from_le_bytes(bytes[24..28].try_into().unwrap()) as f32 / 65536.0,
                y: i32::from_le_bytes(bytes[28..32].try_into().unwrap()) as f32 / 65536.0,
                pressure: f32::from(u16::from_le_bytes(bytes[32..34].try_into().unwrap()))
                    / 65535.0,
            }
        }
        KIND_KEY if action <= 1 && code <= MAX_EVDEV_KEYCODE && bytes[26..28] == [0, 0] => {
            Payload::Key {
                repeat: u16::from_le_bytes(bytes[24..26].try_into().unwrap()),
                serial: wide(28),
            }
        }
        _ => return Err(InputProtocolError::Record),
    };
    Ok(Record::Input(Sample {
        display,
        task,
        incarnation,
        timestamp,
        action,
        code,
        payload,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decoder_preserves_incarnation_and_rejects_malformed_packets() {
        let record = encode_touch(0, 42, 100, 0, 2, 5 << 16, -(3 << 16), u16::MAX, 7).unwrap();
        let Record::Input(sample) = decode(&record).unwrap() else {
            panic!("input expected")
        };
        assert_eq!(sample.incarnation, 7);
        assert_eq!(
            sample.payload,
            Payload::Touch {
                x: 5.0,
                y: -3.0,
                pressure: 1.0
            }
        );
        assert!(decode(&record[..40]).is_err());
        for (offset, value) in [(4, 6), (6, 3), (7, 4), (12, 32), (34, 1), (40, 0)] {
            let mut malformed = record;
            malformed[offset] = value;
            assert!(decode(&malformed).is_err(), "offset {offset}");
        }
        let key = encode_key(0, 42, 100, 0, 30, 2, 15, 7).unwrap();
        let Record::Input(sample) = decode(&key).unwrap() else {
            panic!("key expected")
        };
        assert_eq!(
            sample.payload,
            Payload::Key {
                repeat: 2,
                serial: 15
            }
        );
        assert_eq!(
            decode(&encode_display_power(false)),
            Ok(Record::DisplayPower(false))
        );
    }

    #[test]
    fn embedded_compatibility_tracks_the_encoder_version() {
        for version in [0, 4, 5, 99, 10000, u16::MAX] {
            let marker = compatibility_marker(version);
            let decoded = core::str::from_utf8(&marker)
                .unwrap()
                .strip_prefix("DROIDLOOM_INPUT_ABI=")
                .unwrap()
                .strip_suffix(';')
                .unwrap()
                .parse::<u16>()
                .unwrap();
            assert_eq!(decoded, version);
        }
        assert_eq!(BUILD_COMPATIBILITY, compatibility_marker(PROTOCOL_MAJOR));
    }

    #[test]
    fn display_power_record_has_stable_layout() {
        let record = encode_display_power(true);
        assert_eq!(&record[0..4], b"DLIN");
        assert_eq!(&record[4..6], &PROTOCOL_MAJOR.to_le_bytes());
        assert_eq!(record[6], KIND_DISPLAY_POWER);
        assert_eq!(record[7], 1);
        assert!(record[8..].iter().all(|byte| *byte == 0));
    }

    #[test]
    fn touch_record_has_stable_little_endian_layout() {
        let record = encode_touch(
            3,
            29,
            0x0102_0304_0506_0708,
            1,
            7,
            0x0012_8000,
            0x0034_4000,
            32_768,
            42,
        )
        .unwrap();

        assert_eq!(&record[0..4], b"DLIN");
        assert_eq!(&record[4..6], &PROTOCOL_MAJOR.to_le_bytes());
        assert_eq!(record[6], KIND_TOUCH);
        assert_eq!(record[7], 1);
        assert_eq!(&record[8..12], &3_u32.to_le_bytes());
        assert_eq!(&record[12..16], &7_u32.to_le_bytes());
        assert_eq!(&record[16..24], &0x0102_0304_0506_0708_u64.to_le_bytes());
        assert_eq!(&record[24..28], &0x0012_8000_i32.to_le_bytes());
        assert_eq!(&record[28..32], &0x0034_4000_i32.to_le_bytes());
        assert_eq!(&record[32..34], &32_768_u16.to_le_bytes());
        assert_eq!(&record[34..36], &[0, 0]);
        assert_eq!(&record[36..40], &29_u32.to_le_bytes());
    }

    #[test]
    fn unrepresentable_android_ids_fail_closed() {
        assert_eq!(
            encode_touch(i32::MAX as u32 + 1, 29, 1, 0, 0, 0, 0, u16::MAX, 42),
            Err(InputProtocolError::AndroidDisplay)
        );
        assert_eq!(
            encode_touch(0, 29, 1, 0, 32, 0, 0, u16::MAX, 42),
            Err(InputProtocolError::PointerId)
        );
        assert_eq!(
            encode_touch(0, 29, 1, 4, 0, 0, 0, u16::MAX, 42),
            Err(InputProtocolError::Action)
        );
        assert_eq!(
            encode_touch(0, 0, 1, 0, 0, 0, 0, u16::MAX, 42),
            Err(InputProtocolError::TaskId)
        );
    }

    #[test]
    fn key_record_has_stable_little_endian_layout() {
        let record = encode_key(
            3,
            29,
            0x0102_0304_0506_0708,
            1,
            30,
            7,
            0x1112_1314_1516_1718,
            42,
        )
        .unwrap();

        assert_eq!(&record[0..4], b"DLIN");
        assert_eq!(&record[4..6], &PROTOCOL_MAJOR.to_le_bytes());
        assert_eq!(record[6], KIND_KEY);
        assert_eq!(record[7], 1);
        assert_eq!(&record[8..12], &3_u32.to_le_bytes());
        assert_eq!(&record[12..16], &30_u32.to_le_bytes());
        assert_eq!(&record[16..24], &0x0102_0304_0506_0708_u64.to_le_bytes());
        assert_eq!(&record[24..26], &7_u16.to_le_bytes());
        assert_eq!(&record[26..28], &[0; 2]);
        assert_eq!(&record[28..36], &0x1112_1314_1516_1718_u64.to_le_bytes());
        assert_eq!(&record[36..40], &29_u32.to_le_bytes());
    }

    #[test]
    fn invalid_key_records_fail_closed() {
        assert_eq!(
            encode_key(i32::MAX as u32 + 1, 29, 1, 0, 30, 0, 1, 42),
            Err(InputProtocolError::AndroidDisplay)
        );
        assert_eq!(
            encode_key(0, 29, 1, 0, MAX_EVDEV_KEYCODE + 1, 0, 1, 42),
            Err(InputProtocolError::KeyCode)
        );
        assert_eq!(
            encode_key(0, 29, 1, 2, 30, 0, 1, 42),
            Err(InputProtocolError::Action)
        );
        assert_eq!(
            encode_key(0, 0, 1, 0, 30, 0, 1, 42),
            Err(InputProtocolError::TaskId)
        );
    }

    #[test]
    fn scales_logical_coordinates_to_two_x_buffer_pixels() {
        assert_eq!(scale_fixed(320 << 16, 2, 1), Ok(640 << 16));
    }

    #[test]
    fn supports_fractional_and_negative_scales() {
        assert_eq!(scale_fixed(200 << 16, 3, 2), Ok(300 << 16));
        assert_eq!(scale_fixed(-(20 << 16), 3, 2), Ok(-(30 << 16)));
    }

    #[test]
    fn invalid_scales_and_wire_overflow_fail_closed() {
        assert_eq!(scale_fixed(1, 0, 1), Err(InputProtocolError::Scale));
        assert_eq!(scale_fixed(1, 1, 0), Err(InputProtocolError::Scale));
        assert_eq!(
            scale_fixed(i32::MAX, 2, 1),
            Err(InputProtocolError::Coordinate)
        );
    }

    #[test]
    fn task_bounds_record_has_stable_little_endian_layout() {
        let record = encode_task_bounds(29, 0, 800, 600, 132, 120).unwrap();

        assert_eq!(&record[0..4], b"DLIN");
        assert_eq!(&record[4..6], &PROTOCOL_MAJOR.to_le_bytes());
        assert_eq!(record[6], KIND_TASK_BOUNDS);
        assert_eq!(record[7], 0);
        assert_eq!(&record[8..12], &29_u32.to_le_bytes());
        assert_eq!(&record[12..16], &800_u32.to_le_bytes());
        assert_eq!(&record[16..20], &600_u32.to_le_bytes());
        assert_eq!(&record[20..24], &0_u32.to_le_bytes());
        assert_eq!(&record[24..28], &132_u32.to_le_bytes());
        assert_eq!(&record[28..32], &120_u32.to_le_bytes());
        assert!(record[32..].iter().all(|byte| *byte == 0));
    }

    #[test]
    fn invalid_task_bounds_fail_closed() {
        assert_eq!(
            encode_task_bounds(0, 0, 800, 600, 1, 1),
            Err(InputProtocolError::TaskId)
        );
        assert_eq!(
            encode_task_bounds(29, 0, 0, 600, 1, 1),
            Err(InputProtocolError::TaskExtent)
        );
        assert_eq!(
            encode_task_bounds(29, 0, 800, 600, 0, 1),
            Err(InputProtocolError::Scale)
        );
    }

    #[test]
    fn task_focus_record_has_stable_little_endian_layout() {
        let record = encode_task_focus(29, true).unwrap();

        assert_eq!(&record[0..4], b"DLIN");
        assert_eq!(&record[4..6], &PROTOCOL_MAJOR.to_le_bytes());
        assert_eq!(record[6], KIND_TASK_FOCUS);
        assert_eq!(record[7], 1);
        assert_eq!(&record[8..12], &29_u32.to_le_bytes());
        assert!(record[12..].iter().all(|byte| *byte == 0));
    }

    #[test]
    fn invalid_task_focus_fails_closed() {
        assert_eq!(encode_task_focus(0, true), Err(InputProtocolError::TaskId));
    }

    #[test]
    fn task_close_record_has_stable_little_endian_layout() {
        let record = encode_task_close(29).unwrap();

        assert_eq!(&record[0..4], b"DLIN");
        assert_eq!(&record[4..6], &PROTOCOL_MAJOR.to_le_bytes());
        assert_eq!(record[6], KIND_TASK_CLOSE);
        assert_eq!(record[7], 0);
        assert_eq!(&record[8..12], &29_u32.to_le_bytes());
        assert!(record[12..].iter().all(|byte| *byte == 0));
    }

    #[test]
    fn invalid_task_close_fails_closed() {
        assert_eq!(encode_task_close(0), Err(InputProtocolError::TaskId));
    }
}
