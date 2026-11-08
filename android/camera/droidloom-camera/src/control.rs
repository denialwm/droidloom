//! A session's settings from request to request, what braun is to do for
//! them, and the 3A states results report. braun meters, balances and
//! focuses by itself; Android hears that exposure and white balance have
//! converged, and a focus trigger scans for a few frames, then locks.

use crate::characteristics::{FOCUS_SCAN_FRAMES, FRAME_NS, MAX_ZOOM, PIPELINE_DEPTH};
use crate::metadata::Metadata;
use crate::tags::{self, af_mode, af_state, trigger};

/// `ControlAeState` values.
mod ae_state {
    pub const INACTIVE: u8 = 0;
    pub const CONVERGED: u8 = 2;
    pub const LOCKED: u8 = 3;
    pub const PRECAPTURE: u8 = 5;
}

/// Frames a precapture sequence reports before converging.
const PRECAPTURE_FRAMES: u32 = 2;

/// What braun is to do before the next frame.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Actions {
    /// A new zoom.
    pub zoom: Option<f32>,
    /// Focus on this point of the preview, 0 to 1.
    pub focus: Option<(f32, f32)>,
}

/// One session's settings and 3A.
pub struct Control {
    width: i32,
    height: i32,
    lens: bool,
    settings: Metadata,
    zoom: f32,
    crop: [i32; 4],
    af_mode: u8,
    af_state: u8,
    scan_left: u32,
    precapture_left: u32,
}

impl Control {
    /// A session on a camera of `width` x `height`, focusing if `lens`.
    pub fn new(width: u32, height: u32, lens: bool) -> Self {
        let width = i32::try_from(width).unwrap_or(i32::MAX);
        let height = i32::try_from(height).unwrap_or(i32::MAX);
        Self {
            width,
            height,
            lens,
            settings: Metadata::new(),
            zoom: 1.0,
            crop: [0, 0, width, height],
            af_mode: af_mode::OFF,
            af_state: af_state::INACTIVE,
            scan_left: 0,
            precapture_left: 0,
        }
    }

    /// Whether a request has given settings yet: the first must.
    pub fn has_settings(&self) -> bool {
        !self.settings.is_empty()
    }

    /// Takes the next request's settings, empty for the last ones again, and
    /// returns what braun is to do and the settings the result echoes.
    pub fn request(&mut self, settings: Metadata) -> (Actions, Metadata) {
        if !settings.is_empty() {
            self.settings = settings;
        }
        let settings = self.settings.clone();
        // Triggers fire once; repeated settings are idle.
        self.settings.set_u8(tags::CONTROL_AF_TRIGGER, &[trigger::IDLE]);
        self.settings.set_u8(tags::CONTROL_AE_PRECAPTURE_TRIGGER, &[trigger::IDLE]);

        let mut actions = Actions::default();
        if let Some(region) = settings.i32s(tags::SCALER_CROP_REGION)
            && let [_, _, width, height] = region[..]
            && width > 0
            && height > 0
        {
            #[allow(clippy::cast_precision_loss)]
            let zoom = (self.width as f32 / width as f32)
                .min(self.height as f32 / height as f32)
                .clamp(1.0, MAX_ZOOM);
            #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
            let (seen_x, seen_y) = ((self.width as f32 / zoom) as i32, (self.height as f32 / zoom) as i32);
            // braun crops the middle; the region's centre is not moved.
            self.crop = [
                (self.width - seen_x) / 2,
                (self.height - seen_y) / 2,
                seen_x,
                seen_y,
            ];
            if (zoom - self.zoom).abs() > 0.001 {
                self.zoom = zoom;
                actions.zoom = Some(zoom);
            }
        }

        let mode = settings.u8(tags::CONTROL_AF_MODE).unwrap_or(af_mode::OFF);
        let mode = if self.lens { mode } else { af_mode::OFF };
        if mode != self.af_mode {
            self.af_mode = mode;
            self.af_state = af_state::INACTIVE;
            self.scan_left = 0;
        }
        let continuous = matches!(mode, af_mode::CONTINUOUS_PICTURE | af_mode::CONTINUOUS_VIDEO);
        match settings.u8(tags::CONTROL_AF_TRIGGER) {
            Some(trigger::START) if mode != af_mode::OFF => {
                actions.focus = Some(self.focus_point(&settings));
                self.af_state = af_state::ACTIVE_SCAN;
                self.scan_left = FOCUS_SCAN_FRAMES;
            }
            Some(trigger::CANCEL) => {
                self.af_state = af_state::INACTIVE;
                self.scan_left = 0;
            }
            _ if self.af_state == af_state::ACTIVE_SCAN => {
                self.scan_left = self.scan_left.saturating_sub(1);
                if self.scan_left == 0 {
                    self.af_state = af_state::FOCUSED_LOCKED;
                }
            }
            _ if continuous && self.af_state == af_state::INACTIVE => {
                self.af_state = af_state::PASSIVE_FOCUSED;
            }
            _ => {}
        }

        match settings.u8(tags::CONTROL_AE_PRECAPTURE_TRIGGER) {
            Some(trigger::START) => self.precapture_left = PRECAPTURE_FRAMES,
            Some(trigger::CANCEL) => self.precapture_left = 0,
            _ => self.precapture_left = self.precapture_left.saturating_sub(1),
        }
        (actions, settings)
    }

    /// The point a focus trigger aims at: the first weighted region's
    /// centre, else the middle, as a fraction of the cropped preview.
    fn focus_point(&self, settings: &Metadata) -> (f32, f32) {
        let region = settings
            .i32s(tags::CONTROL_AF_REGIONS)
            .and_then(|regions| regions.chunks_exact(5).find(|region| region[4] > 0).map(<[i32]>::to_vec));
        let Some(region) = region else {
            return (0.5, 0.5);
        };
        let [left, top, width, height] = self.crop;
        #[allow(clippy::cast_precision_loss)]
        let fraction = |low: i32, high: i32, start: i32, length: i32| {
            (((low + high) as f32 / 2.0 - start as f32) / length as f32).clamp(0.0, 1.0)
        };
        (
            fraction(region[0], region[2], left, width),
            fraction(region[1], region[3], top, height),
        )
    }

    /// The result of a request whose settings `request` returned, its frame
    /// taken at `timestamp` on `CLOCK_BOOTTIME`.
    pub fn result(&self, mut settings: Metadata, timestamp: i64) -> Metadata {
        let ae = if settings.u8(tags::CONTROL_AE_MODE) == Some(0) {
            ae_state::INACTIVE
        } else if self.precapture_left > 0 {
            ae_state::PRECAPTURE
        } else if settings.u8(tags::CONTROL_AE_LOCK) == Some(1) {
            ae_state::LOCKED
        } else {
            ae_state::CONVERGED
        };
        let awb = if settings.u8(tags::CONTROL_AWB_LOCK) == Some(1) { 3 } else { 2 };
        settings.set_i32(tags::SCALER_CROP_REGION, &self.crop);
        settings.set_u8(tags::CONTROL_AE_STATE, &[ae]);
        settings.set_u8(tags::CONTROL_AF_STATE, &[self.af_state]);
        settings.set_u8(tags::CONTROL_AWB_STATE, &[awb]);
        // Unavailable.
        settings.set_u8(tags::FLASH_STATE, &[0]);
        // Stationary; braun moves the lens between frames.
        settings.set_u8(tags::LENS_STATE, &[0]);
        settings.set_f32(tags::LENS_FOCUS_DISTANCE, &[0.0]);
        settings.set_u8(tags::REQUEST_PIPELINE_DEPTH, &[PIPELINE_DEPTH]);
        settings.set_i64(tags::SENSOR_FRAME_DURATION, &[FRAME_NS]);
        settings.set_i64(tags::SENSOR_ROLLING_SHUTTER_SKEW, &[FRAME_NS / 2]);
        settings.set_i64(tags::SENSOR_TIMESTAMP, &[timestamp]);
        // No flicker detected.
        settings.set_u8(tags::STATISTICS_SCENE_FLICKER, &[0]);
        settings
    }

    /// The `JPEG_ORIENTATION` of the last settings, in degrees.
    pub fn jpeg_orientation(settings: &Metadata) -> i32 {
        settings
            .i32s(tags::JPEG_ORIENTATION)
            .and_then(|values| values.first().copied())
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(af: u8, trigger: u8) -> Metadata {
        let mut settings = Metadata::new();
        settings.set_u8(tags::CONTROL_AF_MODE, &[af]);
        settings.set_u8(tags::CONTROL_AF_TRIGGER, &[trigger]);
        settings.set_i32(tags::SCALER_CROP_REGION, &[0, 0, 3072, 4096]);
        settings
    }

    #[test]
    fn a_trigger_scans_then_locks_once() {
        let mut control = Control::new(3072, 4096, true);
        let mut start = settings(af_mode::AUTO, trigger::START);
        start.set_i32(tags::CONTROL_AF_REGIONS, &[0, 0, 1536, 1024, 1]);
        let (actions, echoed) = control.request(start);
        assert_eq!(actions.focus, Some((0.25, 0.125)));
        assert_eq!(echoed.u8(tags::CONTROL_AF_TRIGGER), Some(trigger::START));
        let result = control.result(echoed, 7);
        assert_eq!(result.u8(tags::CONTROL_AF_STATE), Some(af_state::ACTIVE_SCAN));
        for _ in 0..FOCUS_SCAN_FRAMES {
            let (actions, echoed) = control.request(Metadata::new());
            assert_eq!(actions.focus, None);
            assert_eq!(echoed.u8(tags::CONTROL_AF_TRIGGER), Some(trigger::IDLE));
        }
        assert_eq!(control.af_state, af_state::FOCUSED_LOCKED);
    }

    #[test]
    fn continuous_focus_is_passive_and_fixed_lenses_ignore_triggers() {
        let mut control = Control::new(3072, 4096, true);
        control.request(settings(af_mode::CONTINUOUS_PICTURE, trigger::IDLE));
        assert_eq!(control.af_state, af_state::PASSIVE_FOCUSED);
        let mut fixed = Control::new(1728, 2304, false);
        let (actions, _) = fixed.request(settings(af_mode::AUTO, trigger::START));
        assert_eq!(actions.focus, None);
        assert_eq!(fixed.af_state, af_state::INACTIVE);
    }

    #[test]
    fn crop_regions_zoom_from_the_middle() {
        let mut control = Control::new(3072, 4096, true);
        let mut zoomed = settings(af_mode::OFF, trigger::IDLE);
        zoomed.set_i32(tags::SCALER_CROP_REGION, &[768, 1024, 1536, 2048]);
        let (actions, echoed) = control.request(zoomed);
        assert_eq!(actions.zoom, Some(2.0));
        let result = control.result(echoed, 1);
        assert_eq!(result.i32s(tags::SCALER_CROP_REGION).unwrap(), [768, 1024, 1536, 2048]);
        let (actions, _) = control.request(Metadata::new());
        assert_eq!(actions.zoom, None);
        let mut far = settings(af_mode::OFF, trigger::IDLE);
        far.set_i32(tags::SCALER_CROP_REGION, &[0, 0, 100, 100]);
        assert_eq!(control.request(far).0.zoom, Some(MAX_ZOOM));
    }

    #[test]
    fn precapture_converges() {
        let mut control = Control::new(3072, 4096, true);
        let mut start = settings(af_mode::OFF, trigger::IDLE);
        start.set_u8(tags::CONTROL_AE_PRECAPTURE_TRIGGER, &[trigger::START]);
        let (_, echoed) = control.request(start);
        assert_eq!(control.result(echoed, 0).u8(tags::CONTROL_AE_STATE), Some(ae_state::PRECAPTURE));
        let mut last = None;
        for _ in 0..PRECAPTURE_FRAMES {
            last = Some(control.request(Metadata::new()).1);
        }
        assert_eq!(control.result(last.unwrap(), 0).u8(tags::CONTROL_AE_STATE), Some(ae_state::CONVERGED));
    }
}
