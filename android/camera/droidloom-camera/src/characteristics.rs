//! What Android is told about a braun camera: a LIMITED, backward
//! compatible camera whose frames come upright, so its sensor orientation
//! is 0, with braun's auto exposure, white balance and focus behind 3A
//! modes that are always on.
//!
//! Zoom is a crop region, which the camera service also derives from
//! `CONTROL_ZOOM_RATIO`. Every YUV and private size is scaled from braun's
//! preview, at most 1024 wide; a JPEG is braun's photo, at full size or at
//! any of those sizes.

use droidloom_device_protocol::{Camera, Facing};

use crate::metadata::Metadata;
use crate::tags::{self, af_mode, format, intent};

/// The most digital zoom.
pub const MAX_ZOOM: f32 = 4.0;
/// Frames per second a stream runs at most.
pub const FRAME_NS: i64 = 33_333_333;
/// How long braun takes a photo: the burst, then developing it.
pub const PHOTO_STALL_NS: i64 = 3_500_000_000;
/// Requests in flight before the camera service waits.
pub const PIPELINE_DEPTH: u8 = 4;
/// The focus sweep's length, in frames, that results report as scanning.
pub const FOCUS_SCAN_FRAMES: u32 = 10;

/// Sizes offered for preview, video and image analysis, both ways round:
/// the camera is upright, so an app holding the phone upright asks for
/// portrait ones.
const STREAM_SIZES: [(u32, u32); 14] = [
    (1080, 1920),
    (1080, 1440),
    (720, 1280),
    (720, 960),
    (480, 640),
    (240, 320),
    (144, 176),
    (1920, 1080),
    (1440, 1080),
    (1280, 720),
    (960, 720),
    (640, 480),
    (320, 240),
    (176, 144),
];

/// Pixel pitch the physical sensor size is worked out from.
const PITCH_MM: f32 = 0.0018;
/// Focal length that gives the main camera a usual phone field of view
/// over a sensor of `MAIN_WIDTH_MM`.
const MAIN_FOCAL_MM: f32 = 4.7;
const MAIN_WIDTH_MM: f32 = 5.5;

/// The sizes `camera` streams in YUV and private formats.
pub fn stream_sizes(camera: &Camera) -> Vec<(u32, u32)> {
    STREAM_SIZES
        .into_iter()
        .filter(|&(width, height)| width <= camera.width && height <= camera.height)
        .collect()
}

fn physical_size(camera: &Camera) -> [f32; 2] {
    #[allow(clippy::cast_precision_loss)]
    [camera.width as f32 * PITCH_MM, camera.height as f32 * PITCH_MM]
}

/// The camera's focal length, its field of view relative to the main one.
pub fn focal_length(camera: &Camera) -> f32 {
    let zoom = if camera.zoom > 0.0 { camera.zoom } else { 1.0 };
    MAIN_FOCAL_MM * zoom * physical_size(camera)[0] / MAIN_WIDTH_MM
}

/// The largest JPEG braun writes for `camera`.
pub fn jpeg_max_size(camera: &Camera) -> i32 {
    i32::try_from(u64::from(camera.width) * u64::from(camera.height)).unwrap_or(i32::MAX)
}

fn int(value: u32) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

/// Keys apps may set; results echo them.
fn request_keys(camera: &Camera) -> Vec<u32> {
    let mut keys = vec![
        tags::COLOR_CORRECTION_ABERRATION_MODE,
        tags::CONTROL_AE_ANTIBANDING_MODE,
        tags::CONTROL_AE_EXPOSURE_COMPENSATION,
        tags::CONTROL_AE_LOCK,
        tags::CONTROL_AE_MODE,
        tags::CONTROL_AE_TARGET_FPS_RANGE,
        tags::CONTROL_AE_PRECAPTURE_TRIGGER,
        tags::CONTROL_AF_MODE,
        tags::CONTROL_AF_TRIGGER,
        tags::CONTROL_AWB_LOCK,
        tags::CONTROL_AWB_MODE,
        tags::CONTROL_CAPTURE_INTENT,
        tags::CONTROL_EFFECT_MODE,
        tags::CONTROL_MODE,
        tags::CONTROL_SCENE_MODE,
        tags::CONTROL_VIDEO_STABILIZATION_MODE,
        tags::EDGE_MODE,
        tags::FLASH_MODE,
        tags::JPEG_ORIENTATION,
        tags::JPEG_QUALITY,
        tags::JPEG_THUMBNAIL_QUALITY,
        tags::JPEG_THUMBNAIL_SIZE,
        tags::LENS_APERTURE,
        tags::LENS_FILTER_DENSITY,
        tags::LENS_FOCAL_LENGTH,
        tags::LENS_OPTICAL_STABILIZATION_MODE,
        tags::NOISE_REDUCTION_MODE,
        tags::SCALER_CROP_REGION,
        tags::SENSOR_TEST_PATTERN_MODE,
        tags::STATISTICS_FACE_DETECT_MODE,
        tags::STATISTICS_LENS_SHADING_MAP_MODE,
    ];
    if camera.lens {
        keys.push(tags::CONTROL_AF_REGIONS);
    }
    keys.sort_unstable();
    keys
}

fn result_keys(camera: &Camera) -> Vec<u32> {
    let mut keys = request_keys(camera);
    keys.extend([
        tags::CONTROL_AE_STATE,
        tags::CONTROL_AF_STATE,
        tags::CONTROL_AWB_STATE,
        tags::FLASH_STATE,
        tags::LENS_FOCUS_DISTANCE,
        tags::LENS_STATE,
        tags::REQUEST_PIPELINE_DEPTH,
        tags::SENSOR_FRAME_DURATION,
        tags::SENSOR_ROLLING_SHUTTER_SKEW,
        tags::SENSOR_TIMESTAMP,
        tags::STATISTICS_SCENE_FLICKER,
    ]);
    keys.sort_unstable();
    keys
}

fn as_i32(keys: &[u32]) -> Vec<i32> {
    keys.iter().map(|&key| i32::try_from(key).expect("tags fit i32")).collect()
}

/// The static metadata of `camera`.
pub fn characteristics(camera: &Camera) -> Metadata {
    let mut out = Metadata::new();
    let (width, height) = (int(camera.width), int(camera.height));
    // Off and fast, whatever is asked: braun processes as it does.
    out.set_u8(tags::COLOR_CORRECTION_AVAILABLE_ABERRATION_MODES, &[0, 1]);
    // Off and auto.
    out.set_u8(tags::CONTROL_AE_AVAILABLE_ANTIBANDING_MODES, &[0, 3]);
    // On.
    out.set_u8(tags::CONTROL_AE_AVAILABLE_MODES, &[1]);
    out.set_i32(tags::CONTROL_AE_AVAILABLE_TARGET_FPS_RANGES, &[15, 30, 30, 30]);
    out.set_i32(tags::CONTROL_AE_COMPENSATION_RANGE, &[0, 0]);
    out.set_rational(tags::CONTROL_AE_COMPENSATION_STEP, &[(1, 3)]);
    if camera.lens {
        out.set_u8(
            tags::CONTROL_AF_AVAILABLE_MODES,
            &[af_mode::OFF, af_mode::AUTO, af_mode::CONTINUOUS_VIDEO, af_mode::CONTINUOUS_PICTURE],
        );
    } else {
        out.set_u8(tags::CONTROL_AF_AVAILABLE_MODES, &[af_mode::OFF]);
    }
    out.set_u8(tags::CONTROL_AVAILABLE_EFFECTS, &[0]);
    out.set_u8(tags::CONTROL_AVAILABLE_SCENE_MODES, &[0]);
    out.set_u8(tags::CONTROL_AVAILABLE_VIDEO_STABILIZATION_MODES, &[0]);
    // Auto.
    out.set_u8(tags::CONTROL_AWB_AVAILABLE_MODES, &[1]);
    // Exposure, white balance, focus.
    out.set_i32(tags::CONTROL_MAX_REGIONS, &[0, 0, i32::from(camera.lens)]);
    out.set_u8(tags::CONTROL_AE_LOCK_AVAILABLE, &[1]);
    out.set_u8(tags::CONTROL_AWB_LOCK_AVAILABLE, &[1]);
    // Auto.
    out.set_u8(tags::CONTROL_AVAILABLE_MODES, &[1]);
    out.set_u8(tags::EDGE_AVAILABLE_EDGE_MODES, &[0, 1]);
    out.set_u8(tags::FLASH_INFO_AVAILABLE, &[0]);
    out.set_i32(tags::JPEG_AVAILABLE_THUMBNAIL_SIZES, &[0, 0]);
    out.set_i32(tags::JPEG_MAX_SIZE, &[jpeg_max_size(camera)]);
    out.set_u8(
        tags::LENS_FACING,
        &[match camera.facing {
            Facing::Front => 0,
            Facing::Back => 1,
            Facing::External => 2,
        }],
    );
    out.set_f32(tags::LENS_INFO_AVAILABLE_APERTURES, &[1.8]);
    out.set_f32(tags::LENS_INFO_AVAILABLE_FILTER_DENSITIES, &[0.0]);
    out.set_f32(tags::LENS_INFO_AVAILABLE_FOCAL_LENGTHS, &[focal_length(camera)]);
    out.set_u8(tags::LENS_INFO_AVAILABLE_OPTICAL_STABILIZATION, &[0]);
    // Diopters: 10 cm for a focusing lens; a fixed one is at infinity.
    out.set_f32(tags::LENS_INFO_HYPERFOCAL_DISTANCE, &[if camera.lens { 0.5 } else { 0.0 }]);
    out.set_f32(tags::LENS_INFO_MINIMUM_FOCUS_DISTANCE, &[if camera.lens { 10.0 } else { 0.0 }]);
    // Uncalibrated.
    out.set_u8(tags::LENS_INFO_FOCUS_DISTANCE_CALIBRATION, &[0]);
    out.set_u8(tags::NOISE_REDUCTION_AVAILABLE_NOISE_REDUCTION_MODES, &[0, 1]);
    // Raw, processed, stalling.
    out.set_i32(tags::REQUEST_MAX_NUM_OUTPUT_STREAMS, &[0, 3, 1]);
    out.set_i32(tags::REQUEST_MAX_NUM_INPUT_STREAMS, &[0]);
    out.set_u8(tags::REQUEST_PIPELINE_MAX_DEPTH, &[PIPELINE_DEPTH]);
    out.set_i32(tags::REQUEST_PARTIAL_RESULT_COUNT, &[1]);
    // Backward compatible.
    out.set_u8(tags::REQUEST_AVAILABLE_CAPABILITIES, &[0]);
    out.set_f32(tags::SCALER_AVAILABLE_MAX_DIGITAL_ZOOM, &[MAX_ZOOM]);

    let mut configurations = Vec::new();
    let mut durations = Vec::new();
    let mut stalls = Vec::new();
    let sizes = stream_sizes(camera);
    for &(width, height) in &sizes {
        for kind in [format::IMPLEMENTATION_DEFINED, format::YCBCR_420_888] {
            // Output.
            configurations.extend([kind, int(width), int(height), 0]);
            durations.extend([i64::from(kind), i64::from(width), i64::from(height), FRAME_NS]);
        }
    }
    let photo_sizes = sizes.into_iter().map(|(width, height)| (int(width), int(height)));
    for (width, height) in photo_sizes.chain([(width, height)]) {
        configurations.extend([format::BLOB, width, height, 0]);
        durations.extend([i64::from(format::BLOB), i64::from(width), i64::from(height), FRAME_NS]);
        stalls.extend([i64::from(format::BLOB), i64::from(width), i64::from(height), PHOTO_STALL_NS]);
    }
    out.set_i32(tags::SCALER_AVAILABLE_STREAM_CONFIGURATIONS, &configurations);
    out.set_i64(tags::SCALER_AVAILABLE_MIN_FRAME_DURATIONS, &durations);
    out.set_i64(tags::SCALER_AVAILABLE_STALL_DURATIONS, &stalls);
    // Centre only.
    out.set_u8(tags::SCALER_CROPPING_TYPE, &[0]);
    out.set_i32(tags::SENSOR_ORIENTATION, &[0]);
    out.set_i32(tags::SENSOR_AVAILABLE_TEST_PATTERN_MODES, &[0]);
    out.set_i32(tags::SENSOR_INFO_ACTIVE_ARRAY_SIZE, &[0, 0, width, height]);
    out.set_i32(tags::SENSOR_INFO_PRE_CORRECTION_ACTIVE_ARRAY_SIZE, &[0, 0, width, height]);
    out.set_i32(tags::SENSOR_INFO_PIXEL_ARRAY_SIZE, &[width, height]);
    out.set_f32(tags::SENSOR_INFO_PHYSICAL_SIZE, &physical_size(camera));
    out.set_i64(tags::SENSOR_INFO_MAX_FRAME_DURATION, &[PHOTO_STALL_NS]);
    // Realtime: timestamps are CLOCK_BOOTTIME.
    out.set_u8(tags::SENSOR_INFO_TIMESTAMP_SOURCE, &[1]);
    out.set_u8(tags::STATISTICS_INFO_AVAILABLE_FACE_DETECT_MODES, &[0]);
    out.set_i32(tags::STATISTICS_INFO_MAX_FACE_COUNT, &[0]);
    out.set_u8(tags::STATISTICS_INFO_AVAILABLE_LENS_SHADING_MAP_MODES, &[0]);
    // Limited.
    out.set_u8(tags::INFO_SUPPORTED_HARDWARE_LEVEL, &[0]);
    // Unknown.
    out.set_i32(tags::SYNC_MAX_LATENCY, &[-1]);

    out.set_i32(tags::REQUEST_AVAILABLE_REQUEST_KEYS, &as_i32(&request_keys(camera)));
    out.set_i32(tags::REQUEST_AVAILABLE_RESULT_KEYS, &as_i32(&result_keys(camera)));
    let mut keys: Vec<u32> = out.tags().collect();
    keys.push(tags::REQUEST_AVAILABLE_CHARACTERISTICS_KEYS);
    keys.sort_unstable();
    out.set_i32(tags::REQUEST_AVAILABLE_CHARACTERISTICS_KEYS, &as_i32(&keys));
    out
}

/// The settings a request of `template` starts from (`RequestTemplate`:
/// preview 1, still capture 2, video record 3, video snapshot 4), or
/// `None` for templates this camera does not offer.
pub fn default_request(camera: &Camera, template: i32) -> Option<Metadata> {
    let capture = u8::try_from(template).ok()?;
    let video = match capture {
        intent::PREVIEW | intent::STILL_CAPTURE => false,
        intent::VIDEO_RECORD | intent::VIDEO_SNAPSHOT => true,
        _ => return None,
    };
    let mut out = Metadata::new();
    out.set_u8(tags::COLOR_CORRECTION_ABERRATION_MODE, &[1]);
    out.set_u8(tags::CONTROL_AE_ANTIBANDING_MODE, &[3]);
    out.set_i32(tags::CONTROL_AE_EXPOSURE_COMPENSATION, &[0]);
    out.set_u8(tags::CONTROL_AE_LOCK, &[0]);
    out.set_u8(tags::CONTROL_AE_MODE, &[1]);
    out.set_i32(tags::CONTROL_AE_TARGET_FPS_RANGE, if video { &[30, 30] } else { &[15, 30] });
    out.set_u8(tags::CONTROL_AE_PRECAPTURE_TRIGGER, &[tags::trigger::IDLE]);
    let focus = match (camera.lens, video) {
        (false, _) => af_mode::OFF,
        (true, true) => af_mode::CONTINUOUS_VIDEO,
        (true, false) => af_mode::CONTINUOUS_PICTURE,
    };
    out.set_u8(tags::CONTROL_AF_MODE, &[focus]);
    out.set_u8(tags::CONTROL_AF_TRIGGER, &[tags::trigger::IDLE]);
    out.set_u8(tags::CONTROL_AWB_LOCK, &[0]);
    out.set_u8(tags::CONTROL_AWB_MODE, &[1]);
    out.set_u8(tags::CONTROL_CAPTURE_INTENT, &[capture]);
    out.set_u8(tags::CONTROL_EFFECT_MODE, &[0]);
    out.set_u8(tags::CONTROL_MODE, &[1]);
    out.set_u8(tags::CONTROL_SCENE_MODE, &[0]);
    out.set_u8(tags::CONTROL_VIDEO_STABILIZATION_MODE, &[0]);
    out.set_u8(tags::EDGE_MODE, &[1]);
    out.set_u8(tags::FLASH_MODE, &[0]);
    out.set_i32(tags::JPEG_ORIENTATION, &[0]);
    out.set_u8(tags::JPEG_QUALITY, &[95]);
    out.set_u8(tags::JPEG_THUMBNAIL_QUALITY, &[90]);
    out.set_i32(tags::JPEG_THUMBNAIL_SIZE, &[0, 0]);
    out.set_f32(tags::LENS_APERTURE, &[1.8]);
    out.set_f32(tags::LENS_FILTER_DENSITY, &[0.0]);
    out.set_f32(tags::LENS_FOCAL_LENGTH, &[focal_length(camera)]);
    out.set_u8(tags::LENS_OPTICAL_STABILIZATION_MODE, &[0]);
    out.set_u8(tags::NOISE_REDUCTION_MODE, &[1]);
    out.set_i32(tags::SCALER_CROP_REGION, &[0, 0, int(camera.width), int(camera.height)]);
    out.set_i32(tags::SENSOR_TEST_PATTERN_MODE, &[0]);
    out.set_u8(tags::STATISTICS_FACE_DETECT_MODE, &[0]);
    out.set_u8(tags::STATISTICS_LENS_SHADING_MAP_MODE, &[0]);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn camera(facing: Facing, width: u32, height: u32, lens: bool) -> Camera {
        Camera { id: "imx890".into(), name: "IMX890".into(), facing, width, height, zoom: 0.0, lens }
    }

    #[test]
    fn keys_list_every_static_entry() {
        let back = camera(Facing::Back, 3072, 4096, true);
        let metadata = characteristics(&back);
        let listed = metadata.i32s(tags::REQUEST_AVAILABLE_CHARACTERISTICS_KEYS).unwrap();
        let present = as_i32(&metadata.tags().collect::<Vec<_>>());
        assert_eq!(listed, present);
        assert_eq!(metadata.i32s(tags::SENSOR_INFO_ACTIVE_ARRAY_SIZE).unwrap(), [0, 0, 3072, 4096]);
        let request = default_request(&back, 1).unwrap();
        let keys = metadata.i32s(tags::REQUEST_AVAILABLE_REQUEST_KEYS).unwrap();
        for tag in request.tags() {
            assert!(keys.contains(&i32::try_from(tag).unwrap()), "{tag:#x}");
        }
        assert_eq!(Metadata::decode(&metadata.encode()).unwrap(), metadata);
    }

    #[test]
    fn sizes_fit_the_sensor() {
        let front = camera(Facing::Front, 1728, 2304, false);
        let sizes = stream_sizes(&front);
        assert!(sizes.contains(&(1080, 1920)));
        assert!(!sizes.contains(&(1920, 1080)));
        let metadata = characteristics(&front);
        assert_eq!(metadata.u8(tags::LENS_FACING), Some(0));
        let configurations = metadata.i32s(tags::SCALER_AVAILABLE_STREAM_CONFIGURATIONS).unwrap();
        assert_eq!(configurations.len(), (sizes.len() * 3 + 1) * 4);
        assert!(configurations.chunks(4).any(|entry| entry == [format::BLOB, 1440, 1080, 0]));
        assert_eq!(&configurations[configurations.len() - 4..], &[format::BLOB, 1728, 2304, 0]);
        assert_eq!(default_request(&front, 2).unwrap().u8(tags::CONTROL_AF_MODE), Some(af_mode::OFF));
    }

    #[test]
    fn templates() {
        let back = camera(Facing::Back, 3072, 4096, true);
        assert_eq!(
            default_request(&back, 3).unwrap().u8(tags::CONTROL_AF_MODE),
            Some(af_mode::CONTINUOUS_VIDEO)
        );
        assert!(default_request(&back, 5).is_none());
        assert!(default_request(&back, 6).is_none());
    }
}
