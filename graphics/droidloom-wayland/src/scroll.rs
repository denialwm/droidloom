//! Wheel and touchpad scrolling. Android takes a scroll as a mouse
//! `ACTION_SCROLL` in wheel notches, positive up and right; Wayland sends it
//! per axis, positive down and right, as high-resolution wheel steps
//! (`value120`, 120 per notch), older whole steps (`discrete`), and a
//! continuous amount in logical pixels (`absolute`), which is all a touchpad
//! sends. A wheel moves Android's notch per notch; a touchpad moves the
//! content as far as the fingers move, as in native toolkits.

use smithay_client_toolkit::seat::pointer::AxisScroll;

/// Android's scroll factor: content moves 64 dp per notch
/// (`config_verticalScrollFactor`, `config_horizontalScrollFactor`).
const NOTCH_DP: f64 = 64.0;
/// A single frame cannot scroll further than this many notches.
const MAX_NOTCHES: f64 = 64.0;

/// One axis of one pointer frame, summed over its axis events.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Axis {
    value120: i32,
    discrete: i32,
    absolute: f64,
}

impl Axis {
    pub fn add(&mut self, scroll: &AxisScroll) {
        self.value120 = self.value120.saturating_add(scroll.value120);
        self.discrete = self.discrete.saturating_add(scroll.discrete);
        self.absolute += scroll.absolute;
    }

    /// Notches in Wayland's direction. The finest wheel field wins: a
    /// compositor sends the same motion in every field it knows.
    pub fn notches(self, notch_logical: f64) -> f64 {
        let notches = if self.value120 != 0 {
            f64::from(self.value120) / 120.0
        } else if self.discrete != 0 {
            f64::from(self.discrete)
        } else if notch_logical > 0.0 {
            self.absolute / notch_logical
        } else {
            0.0
        };
        notches.clamp(-MAX_NOTCHES, MAX_NOTCHES)
    }
}

/// Logical pixels one notch moves a task's content: 64 dp at the task's
/// density and scale.
pub fn notch_logical(dpi: f64, scale_120: u32) -> f64 {
    let scale = if scale_120 == 0 {
        1.0
    } else {
        f64::from(scale_120) / 120.0
    };
    NOTCH_DP * dpi / 160.0 / scale
}

/// A frame's scroll in Android's notches: (vertical, positive up;
/// horizontal, positive right).
pub fn android_notches(vertical: Axis, horizontal: Axis, notch_logical: f64) -> (f64, f64) {
    (
        -vertical.notches(notch_logical),
        horizontal.notches(notch_logical),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn axis(absolute: f64, discrete: i32, value120: i32) -> Axis {
        let mut axis = Axis::default();
        axis.add(&AxisScroll {
            absolute,
            discrete,
            value120,
            ..AxisScroll::default()
        });
        axis
    }

    #[test]
    fn a_wheel_notch_down_is_one_android_notch_down() {
        // KWin, Mutter and wlroots send value120 and 15 pixels for a notch.
        let (vertical, horizontal) = android_notches(axis(15.0, 1, 120), Axis::default(), 92.0);
        assert_eq!((vertical, horizontal), (-1.0, 0.0));
        // A high-resolution wheel sends fractions of a notch.
        assert_eq!(
            android_notches(axis(7.5, 0, -60), Axis::default(), 92.0).0,
            0.5
        );
        // A compositor without value120 still counts whole notches.
        assert_eq!(
            android_notches(axis(10.0, 2, 0), Axis::default(), 92.0).0,
            -2.0
        );
    }

    #[test]
    fn a_touchpad_moves_content_as_far_as_the_fingers() {
        let (vertical, horizontal) = android_notches(axis(46.0, 0, 0), axis(-23.0, 0, 0), 92.0);
        assert_eq!((vertical, horizontal), (-0.5, -0.25));
        assert_eq!(
            android_notches(axis(1.0e9, 0, 0), Axis::default(), 92.0).0,
            -MAX_NOTCHES
        );
        assert_eq!(
            android_notches(axis(5.0, 0, 0), Axis::default(), 0.0).0,
            0.0
        );
    }

    #[test]
    fn events_of_one_frame_add_up() {
        let mut vertical = Axis::default();
        vertical.add(&AxisScroll {
            value120: 120,
            ..AxisScroll::default()
        });
        vertical.add(&AxisScroll {
            absolute: 15.0,
            ..AxisScroll::default()
        });
        assert_eq!(vertical.notches(92.0), 1.0);
    }

    #[test]
    fn a_notch_is_64_dp_at_the_task_density() {
        // 460 dpi at scale 2: 64 dp is 184 buffer pixels, 92 logical ones.
        assert_eq!(notch_logical(460.0, 240), 92.0);
        assert_eq!(notch_logical(160.0, 120), 64.0);
        assert_eq!(notch_logical(160.0, 0), 64.0);
    }
}
