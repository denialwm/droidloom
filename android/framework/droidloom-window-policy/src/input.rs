//! Routed input state, independent of JNI, Binder and Android event objects.

use crate::input_keys::android_keycode;
use droidloom_input_protocol::{Payload, Sample};
use std::collections::{BTreeMap, BTreeSet};

const FLAG_CANCELED: i32 = 0x20;
/// Event kinds the Android adapter builds: a touchscreen motion, a key and a
/// mouse scroll.
pub const KIND_TOUCH: i32 = 1;
pub const KIND_KEY: i32 = 2;
pub const KIND_SCROLL: i32 = 7;
/// `AMOTION_EVENT_ACTION_SCROLL`.
const ACTION_SCROLL: i32 = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Target {
    pub task: i32,
    pub display: i32,
    pub incarnation: u64,
    pub token: u64,
    pub left: i32,
    pub top: i32,
}

impl Target {
    fn identity(self) -> (i32, i32, u64, u64) {
        (self.task, self.display, self.incarnation, self.token)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
#[repr(C)]
pub struct Pointer {
    pub id: i32,
    pub x: f32,
    pub y: f32,
    pub pressure: f32,
}

/// Stack-only C ABI to the small Android dispatch adapter.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
#[repr(C)]
#[allow(clippy::struct_field_names)] // Android's eventTime/downTime terminology.
pub struct Event {
    pub kind: i32,
    pub display: i32,
    pub action: i32,
    pub key_code: i32,
    pub scan_code: i32,
    pub meta: i32,
    pub repeat: i32,
    pub flags: i32,
    pub count: i32,
    pub down_time: u64,
    pub event_time: u64,
    pub serial: u64,
    pub pointers: [Pointer; 32],
    /// Scroll notches, positive up (`AXIS_VSCROLL`) and right (`AXIS_HSCROLL`).
    pub vertical_scroll: f32,
    pub horizontal_scroll: f32,
}

#[derive(Clone, Copy, Debug)]
pub struct RoutedEvent {
    pub target: Target,
    pub event: Event,
}

#[derive(Clone)]
struct Gesture {
    down_time: u64,
    last_time: u64,
    pointers: BTreeMap<u32, Pointer>,
}

#[derive(Clone, Copy)]
struct Key {
    down_time: u64,
    last_time: u64,
    code: i32,
}

#[derive(Clone, Default)]
pub struct Engine {
    targets: BTreeMap<i32, Target>,
    gestures: BTreeMap<i32, Gesture>,
    keys: BTreeMap<(i32, u32), Key>,
    locks: BTreeMap<i32, i32>,
    // Android resolves injected touchscreen input to one device per display,
    // not one device per task. Keep the first task's ownership until release;
    // contacts begun on another task must never become a new DOWN mid-contact.
    suppressed: BTreeSet<(i32, u32)>,
}

impl Engine {
    /// Update coordinates without interrupting a gesture; replacing its actual
    /// application or task incarnation cancels the old input before rebinding.
    pub fn publish(&mut self, target: Target, now: u64) -> Vec<RoutedEvent> {
        let cancelled = if self
            .targets
            .get(&target.task)
            .is_some_and(|old| old.identity() != target.identity())
        {
            self.cancel_task(target.task, now)
        } else {
            Vec::new()
        };
        self.targets.insert(target.task, target);
        cancelled
    }

    pub fn retire(&mut self, task: i32, incarnation: u64, now: u64) -> Vec<RoutedEvent> {
        if self
            .targets
            .get(&task)
            .is_none_or(|target| target.incarnation != incarnation)
        {
            return Vec::new();
        }
        let cancelled = self.cancel_task(task, now);
        self.targets.remove(&task);
        cancelled
    }

    pub fn reset(&mut self, now: u64) -> Vec<RoutedEvent> {
        self.suppressed.clear();
        let tasks: Vec<_> = self.targets.keys().copied().collect();
        tasks
            .into_iter()
            .flat_map(|task| self.cancel_task(task, now))
            .collect()
    }

    pub fn process(&mut self, sample: Sample) -> Result<Option<RoutedEvent>, &'static str> {
        let Some(target) = self.targets.get(&sample.task).copied().filter(|target| {
            target.display == sample.display && target.incarnation == sample.incarnation
        }) else {
            return Ok(None);
        };
        let event = match sample.payload {
            Payload::Touch { x, y, pressure } => {
                if self.suppress_touch(sample) {
                    return Ok(None);
                }
                self.touch(sample, target, x, y, pressure)?
            }
            Payload::Scroll {
                x,
                y,
                vertical,
                horizontal,
            } => Self::scroll(sample, target, x, y, vertical, horizontal),
            Payload::Key { repeat, serial } => self.key(sample, repeat, serial)?,
        };
        Ok(Some(RoutedEvent { target, event }))
    }

    fn suppress_touch(&mut self, sample: Sample) -> bool {
        let identity = (sample.task, sample.code);
        if self.suppressed.contains(&identity) {
            if sample.action == 2 {
                self.suppressed.remove(&identity);
            } else if sample.action == 3 {
                self.suppressed.retain(|(task, _)| *task != sample.task);
            }
            return true;
        }
        if self.gestures.keys().any(|task| {
            *task != sample.task
                && self
                    .targets
                    .get(task)
                    .is_some_and(|t| t.display == sample.display)
        }) {
            if sample.action == 0 {
                self.suppressed.insert(identity);
            }
            return true;
        }
        false
    }

    /// Commit only the retirement Android accepted. Failed cancellations keep
    /// their original pointer set/token available for a later reset attempt.
    pub fn acknowledge_cancel(&mut self, event: &RoutedEvent) {
        if event.event.kind == KIND_TOUCH {
            self.gestures.remove(&event.target.task);
        } else if event.event.kind == KIND_KEY {
            if let Ok(scan_code) = u32::try_from(event.event.scan_code) {
                self.keys.remove(&(event.target.task, scan_code));
            }
        }
    }

    fn touch(
        &mut self,
        sample: Sample,
        target: Target,
        x: f32,
        y: f32,
        pressure: f32,
    ) -> Result<Event, &'static str> {
        if sample.code > droidloom_input_protocol::MAX_POINTER_ID || sample.action > 3 {
            return Err("invalid touch sample");
        }
        if sample.action == 0 {
            if self
                .gestures
                .get(&sample.task)
                .is_some_and(|g| g.pointers.contains_key(&sample.code))
            {
                return Err("duplicate touch down");
            }
            self.gestures.entry(sample.task).or_insert_with(|| Gesture {
                down_time: sample.timestamp,
                last_time: sample.timestamp,
                pointers: BTreeMap::new(),
            });
        }
        let gesture = self
            .gestures
            .get_mut(&sample.task)
            .ok_or("touch has no down")?;
        if sample.action != 0 && !gesture.pointers.contains_key(&sample.code) {
            return Err("touch pointer has no down");
        }
        gesture.pointers.insert(
            sample.code,
            Pointer {
                id: i32::try_from(sample.code).map_err(|_| "pointer id")?,
                x,
                y,
                pressure,
            },
        );
        let index = gesture
            .pointers
            .keys()
            .position(|id| *id == sample.code)
            .ok_or("pointer missing")?;
        let count = gesture.pointers.len();
        let action = match (sample.action, count) {
            (0, 1) => 0,
            (0, _) => 5 | (i32::try_from(index).unwrap() << 8),
            (1, _) => 2,
            (2, 1) => 1,
            (2, _) => 6 | (i32::try_from(index).unwrap() << 8),
            (3, _) => 3,
            _ => return Err("touch action"),
        };
        let event = Self::motion(target, gesture, action, sample.timestamp);
        if sample.action == 2 {
            gesture.pointers.remove(&sample.code);
        }
        if sample.action == 3 || gesture.pointers.is_empty() {
            self.gestures.remove(&sample.task);
        }
        Ok(event)
    }

    /// A mouse scroll is one event with no contact: nothing to hold or cancel.
    #[allow(clippy::cast_precision_loss)] // Native MotionEvent coordinates are f32.
    fn scroll(
        sample: Sample,
        target: Target,
        x: f32,
        y: f32,
        vertical: f32,
        horizontal: f32,
    ) -> Event {
        let mut event = Event {
            kind: KIND_SCROLL,
            display: target.display,
            action: ACTION_SCROLL,
            count: 1,
            down_time: sample.timestamp,
            event_time: sample.timestamp,
            vertical_scroll: vertical,
            horizontal_scroll: horizontal,
            ..Event::default()
        };
        event.pointers[0] = Pointer {
            id: 0,
            x: x + target.left as f32,
            y: y + target.top as f32,
            pressure: 0.0,
        };
        event
    }

    #[allow(clippy::cast_precision_loss)] // Native MotionEvent coordinates are f32.
    fn motion(target: Target, gesture: &mut Gesture, action: i32, now: u64) -> Event {
        gesture.last_time = now.max(gesture.last_time);
        let mut event = Event {
            kind: KIND_TOUCH,
            display: target.display,
            action,
            flags: if action == 3 { FLAG_CANCELED } else { 0 },
            count: i32::try_from(gesture.pointers.len()).unwrap(),
            down_time: gesture.down_time,
            event_time: gesture.last_time,
            ..Event::default()
        };
        for (slot, pointer) in event.pointers.iter_mut().zip(gesture.pointers.values()) {
            *slot = Pointer {
                x: pointer.x + target.left as f32,
                y: pointer.y + target.top as f32,
                pressure: if action == 1 || action == 3 {
                    0.0
                } else {
                    pointer.pressure
                },
                ..*pointer
            };
        }
        event
    }

    fn key(&mut self, sample: Sample, repeat: u16, serial: u64) -> Result<Event, &'static str> {
        let code = android_keycode(sample.code).ok_or("unsupported evdev key")?;
        let identity = (sample.task, sample.code);
        let previous = self.keys.get(&identity).copied();
        let pressed = sample.action == 0;
        if pressed && repeat == 0 && previous.is_some() {
            return Err("duplicate key down");
        }
        if (!pressed || repeat != 0) && previous.is_none() {
            return Err("key has no down");
        }
        let key = Key {
            down_time: previous.map_or(sample.timestamp, |key| key.down_time),
            last_time: previous.map_or(sample.timestamp, |key| sample.timestamp.max(key.last_time)),
            code,
        };
        if pressed {
            self.keys.insert(identity, key);
            if repeat == 0 {
                let bit = match code {
                    115 => 0x10_0000,
                    143 => 0x20_0000,
                    116 => 0x40_0000,
                    _ => 0,
                };
                *self.locks.entry(sample.task).or_default() ^= bit;
            }
        } else {
            self.keys.remove(&identity);
        }
        Ok(Event {
            kind: KIND_KEY,
            display: sample.display,
            action: i32::from(!pressed),
            key_code: code,
            scan_code: i32::try_from(sample.code).map_err(|_| "key scan code")?,
            repeat: i32::from(repeat),
            meta: self.meta(sample.task),
            flags: 0x48,
            down_time: key.down_time,
            event_time: key.last_time,
            serial,
            ..Event::default()
        })
    }

    fn meta(&self, task: i32) -> i32 {
        let mut meta = self.locks.get(&task).copied().unwrap_or(0);
        for ((owner, _), key) in &self.keys {
            if *owner != task {
                continue;
            }
            meta |= match key.code {
                59 => 0x41,
                60 => 0x81,
                57 => 0x12,
                58 => 0x22,
                113 => 0x3000,
                114 => 0x5000,
                117 => 0x3_0000,
                118 => 0x5_0000,
                _ => 0,
            };
        }
        meta
    }

    fn cancel_task(&mut self, task: i32, now: u64) -> Vec<RoutedEvent> {
        self.suppressed.retain(|(owner, _)| *owner != task);
        let Some(target) = self.targets.get(&task).copied() else {
            return Vec::new();
        };
        let mut cancelled = Vec::new();
        if let Some(mut gesture) = self.gestures.remove(&task) {
            cancelled.push(RoutedEvent {
                target,
                event: Self::motion(target, &mut gesture, 3, now),
            });
        }
        let keys: Vec<_> = self
            .keys
            .keys()
            .filter(|(owner, _)| *owner == task)
            .copied()
            .collect();
        for identity in keys {
            let key = self.keys.remove(&identity).unwrap();
            cancelled.push(RoutedEvent {
                target,
                event: Event {
                    kind: KIND_KEY,
                    display: target.display,
                    action: 1,
                    key_code: key.code,
                    scan_code: i32::try_from(identity.1).unwrap(),
                    flags: 0x68,
                    meta: self.meta(task),
                    down_time: key.down_time,
                    event_time: now.max(key.last_time),
                    ..Event::default()
                },
            });
        }
        self.locks.remove(&task);
        cancelled
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn target() -> Target {
        Target {
            task: 42,
            display: 0,
            incarnation: 7,
            token: 9,
            left: 30,
            top: 40,
        }
    }
    fn touch(id: u32, action: u8) -> Sample {
        Sample {
            display: 0,
            task: 42,
            incarnation: 7,
            timestamp: 100,
            action,
            code: id,
            payload: Payload::Touch {
                x: 2.5,
                y: 3.0,
                pressure: 1.0,
            },
        }
    }
    fn key(scan: u32, action: u8, repeat: u16) -> Sample {
        Sample {
            payload: Payload::Key { repeat, serial: 55 },
            code: scan,
            action,
            ..touch(0, 0)
        }
    }
    #[test]
    fn a_scroll_is_one_mouse_event_at_the_window_position() {
        let mut engine = Engine::default();
        engine.publish(target(), 1);
        let held = engine.process(touch(7, 0)).unwrap().unwrap().event;
        let scroll = engine
            .process(Sample {
                payload: Payload::Scroll {
                    x: 2.5,
                    y: 3.0,
                    vertical: -1.5,
                    horizontal: 0.25,
                },
                ..touch(0, 0)
            })
            .unwrap()
            .unwrap()
            .event;
        assert_eq!(
            (scroll.kind, scroll.action, scroll.count),
            (KIND_SCROLL, 8, 1)
        );
        assert_eq!((scroll.pointers[0].x, scroll.pointers[0].y), (32.5, 43.0));
        assert_eq!(
            (scroll.vertical_scroll, scroll.horizontal_scroll),
            (-1.5, 0.25)
        );
        assert_eq!((scroll.down_time, scroll.event_time), (100, 100));
        // A scroll holds nothing: the touch still lifts as its own gesture.
        let lifted = engine.process(touch(7, 2)).unwrap().unwrap().event;
        assert_eq!((lifted.action, lifted.down_time), (1, held.down_time));
        assert!(engine.reset(200).is_empty());
    }

    #[test]
    fn multi_touch_indices_and_bounds_follow_the_same_live_task() {
        let mut engine = Engine::default();
        engine.publish(target(), 1);
        let first = engine.process(touch(7, 0)).unwrap().unwrap().event;
        assert_eq!((first.action, first.count), (0, 1));
        assert_eq!((first.pointers[0].x, first.pointers[0].y), (32.5, 43.0));
        let second = engine.process(touch(2, 0)).unwrap().unwrap().event;
        assert_eq!((second.action, second.count), (5, 2));
        assert_eq!((second.pointers[0].id, second.pointers[1].id), (2, 7));
        assert!(engine
            .publish(
                Target {
                    left: 50,
                    ..target()
                },
                101
            )
            .is_empty());
        let moved = engine
            .process(Sample {
                timestamp: 90,
                ..touch(7, 1)
            })
            .unwrap()
            .unwrap()
            .event;
        assert_eq!((moved.pointers[1].x, moved.event_time), (52.5, 100));
        let up = engine.process(touch(7, 2)).unwrap().unwrap().event;
        assert_eq!(up.action, 6 | 1 << 8);
        assert_eq!(
            engine.process(touch(2, 2)).unwrap().unwrap().event.action,
            1
        );
        assert!(engine.process(touch(2, 1)).is_err());
    }
    #[test]
    fn token_and_incarnation_changes_cancel_old_input_and_reject_stale_samples() {
        let mut engine = Engine::default();
        engine.publish(target(), 1);
        engine.process(touch(0, 0)).unwrap();
        engine.process(key(30, 0, 0)).unwrap();
        let new = Target {
            incarnation: 8,
            token: 10,
            ..target()
        };
        let cancelled = engine.publish(new, 200);
        assert_eq!(cancelled.len(), 2);
        assert!(cancelled.iter().all(|event| event.target.token == 9));
        assert_eq!(cancelled[0].event.action, 3);
        assert_eq!(cancelled[0].event.flags & FLAG_CANCELED, FLAG_CANCELED);
        assert_eq!(cancelled[1].event.flags & 0x20, 0x20);
        assert!(engine.process(touch(0, 0)).unwrap().is_none());
        assert!(engine.retire(42, 7, 201).is_empty());
        assert!(engine
            .process(Sample {
                incarnation: 8,
                ..touch(0, 0)
            })
            .unwrap()
            .is_some());
        assert_eq!(engine.publish(Target { token: 11, ..new }, 202).len(), 1);
    }
    #[test]
    fn modifiers_are_isolated_and_releasing_one_shift_keeps_the_other() {
        let mut engine = Engine::default();
        engine.publish(target(), 1);
        engine.publish(
            Target {
                task: 43,
                ..target()
            },
            1,
        );
        engine.process(key(42, 0, 0)).unwrap();
        engine.process(key(54, 0, 0)).unwrap();
        let left_up = engine.process(key(42, 1, 0)).unwrap().unwrap().event;
        assert_eq!(left_up.meta, 0x81);
        let other = engine
            .process(Sample {
                task: 43,
                ..key(30, 0, 0)
            })
            .unwrap()
            .unwrap()
            .event;
        assert_eq!(other.meta, 0);
        assert_eq!(engine.reset(200).len(), 2);
        assert!(engine.process(key(54, 1, 0)).is_err());
    }
    #[test]
    fn repeats_keep_down_time_and_route_trace_serial() {
        let mut engine = Engine::default();
        engine.publish(target(), 1);
        assert!(engine.process(key(30, 0, 1)).is_err());
        engine.process(key(30, 0, 0)).unwrap();
        let repeated = engine
            .process(Sample {
                timestamp: 300,
                ..key(30, 0, 2)
            })
            .unwrap()
            .unwrap()
            .event;
        assert_eq!(
            (
                repeated.down_time,
                repeated.event_time,
                repeated.repeat,
                repeated.serial
            ),
            (100, 300, 2, 55)
        );
        let up = engine.process(key(30, 1, 0)).unwrap().unwrap().event;
        assert_eq!(up.event_time, 300);
    }
    #[test]
    fn retirement_only_cancels_the_selected_app() {
        let mut engine = Engine::default();
        engine.publish(target(), 1);
        engine.publish(
            Target {
                task: 43,
                ..target()
            },
            1,
        );
        engine.process(key(30, 0, 0)).unwrap();
        engine
            .process(Sample {
                task: 43,
                ..touch(0, 0)
            })
            .unwrap();
        assert_eq!(engine.retire(42, 7, 200).len(), 1);
        assert!(engine
            .process(Sample {
                task: 43,
                ..touch(0, 1)
            })
            .unwrap()
            .is_some());
        assert!(engine
            .process(Sample {
                display: 1,
                task: 43,
                ..touch(0, 1)
            })
            .unwrap()
            .is_none());
    }

    #[test]
    fn cross_task_contacts_keep_one_display_owner_until_their_physical_up() {
        let mut engine = Engine::default();
        engine.publish(target(), 1);
        engine.publish(
            Target {
                task: 43,
                ..target()
            },
            1,
        );
        engine.process(touch(0, 0)).unwrap();
        let other = |id, action| Sample {
            task: 43,
            ..touch(id, action)
        };
        // A second app cannot start another DeviceId(-1) stream or contribute
        // coordinates to the first app's pointer set.
        assert!(engine.process(other(1, 0)).unwrap().is_none());
        assert!(engine.process(other(1, 1)).unwrap().is_none());
        let second = engine.process(touch(2, 0)).unwrap().unwrap().event;
        assert_eq!((second.action, second.count), (5 | 1 << 8, 2));
        let cancelled = engine.retire(42, 7, 200);
        assert_eq!(
            (cancelled[0].event.count, cancelled[0].event.flags),
            (2, FLAG_CANCELED)
        );
        assert!(engine.process(other(1, 1)).unwrap().is_none());
        assert!(engine.process(other(1, 2)).unwrap().is_none());
        assert_eq!(
            engine.process(other(1, 0)).unwrap().unwrap().event.action,
            0
        );
        assert_eq!(
            engine.process(other(1, 2)).unwrap().unwrap().event.action,
            1
        );
    }

    #[test]
    fn separate_displays_can_hold_independent_gestures_and_reset_exact_sets() {
        let mut engine = Engine::default();
        engine.publish(target(), 1);
        engine.publish(
            Target {
                task: 43,
                display: 1,
                ..target()
            },
            1,
        );
        engine.process(touch(0, 0)).unwrap();
        engine.process(touch(1, 0)).unwrap();
        let other = Sample {
            task: 43,
            display: 1,
            ..touch(0, 0)
        };
        assert_eq!(engine.process(other).unwrap().unwrap().event.action, 0);
        let cancelled = engine.reset(200);
        assert_eq!(cancelled.len(), 2);
        assert_eq!(cancelled[0].event.count, 2);
        assert_eq!(cancelled[1].event.count, 1);
        assert!(cancelled.iter().all(|e| e.event.flags == FLAG_CANCELED));
        assert_eq!(
            engine.process(touch(0, 0)).unwrap().unwrap().event.action,
            0
        );
    }
}
