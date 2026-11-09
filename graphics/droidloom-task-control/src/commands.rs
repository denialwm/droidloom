//! Bounded host-window commands. Coalescing never loses the final resize or close.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use crate::CodecError;

const MAX_PENDING: usize = 256;
const RESIZE_DEBOUNCE: Duration = Duration::from_millis(80);

/// Operation applied by the framework under its window-management lock.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskAction {
    /// Mirror a positive host keyboard-focus transition.
    Focus,
    /// Apply host content dimensions, already in Android buffer pixels.
    Resize {
        /// Requested width.
        width: u32,
        /// Requested height.
        height: u32,
    },
    /// Finish/remove the task through normal Android lifecycle handling.
    Close,
    /// The host minimized the window: Android stops the task's activities.
    Hide,
    /// The host restored the window.
    Show,
    /// Lay the task out at the density of the output its window is on; 0
    /// follows the display's density.
    Density {
        /// Dots per inch.
        dpi: u32,
    },
    /// Create tasks at this density: the one the host expects to give the
    /// next window, so its activities start laid out for it. Not for a task:
    /// the command carries the bootstrap display's object and no task.
    NewTaskDensity {
        /// Dots per inch; 0 creates tasks at the display's density.
        dpi: u32,
    },
}

/// The densities a task may be given, besides 0.
pub const TASK_DENSITY_DPI: std::ops::RangeInclusive<u32> = 72..=1280;

/// A host command bound to one registered native window, not just a task ID.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TaskCommand {
    /// Monotonic Composer object identity, replaced when a task is re-registered.
    pub object: u64,
    /// Android task ID.
    pub task: u64,
    /// Framework incarnation captured at publication, not resolved at delivery.
    pub incarnation: u64,
    /// Android logical display.
    pub android_display: u32,
    /// Requested operation.
    pub action: TaskAction,
}

impl TaskCommand {
    /// Validate the supported framework scope and bounded dimensions.
    ///
    /// # Errors
    /// Rejects invalid IDs, other displays and oversized/empty bounds.
    pub fn validate(self) -> Result<(), CodecError> {
        let taskless = matches!(self.action, TaskAction::NewTaskDensity { .. });
        if (self.incarnation == 0) != taskless
            || i64::try_from(self.incarnation).is_err()
            || self.object == 0
            || self.object > i64::MAX as u64
            || (self.task == 0) != taskless
            || self.task > i32::MAX as u64
            || self.android_display != 0
        {
            return Err(CodecError::InvalidValue("task command identity"));
        }
        if let TaskAction::Resize { width, height } = self.action {
            if width == 0 || height == 0 || width > 16384 || height > 16384 {
                return Err(CodecError::InvalidValue("task command bounds"));
            }
        }
        if let TaskAction::Density { dpi } | TaskAction::NewTaskDensity { dpi } = self.action {
            if dpi != 0 && !TASK_DENSITY_DPI.contains(&dpi) {
                return Err(CodecError::InvalidValue("task command density"));
            }
        }
        Ok(())
    }
}

/// A command remains pending until the framework acknowledges its serial.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PendingCommand {
    /// Serial scoped to this Composer process.
    pub serial: u64,
    /// Object-scoped operation.
    pub command: TaskCommand,
    ready: Instant,
}

/// Event-driven command backlog, shared by Composer's sender and event pump.
#[derive(Default)]
pub struct CommandQueue {
    pending: BTreeMap<u64, PendingCommand>,
    closed: BTreeSet<u64>,
    resized: BTreeSet<u64>,
    next: u64,
}

impl CommandQueue {
    /// Coalesce superseded resizes/focus/visibility transitions; close supersedes its object's work.
    ///
    /// An object's first resize is delivered at once: the task still has
    /// Android's provisional launch bounds, and its window is not presented
    /// until the host size is applied. Later resizes are debounced.
    ///
    /// # Errors
    /// Rejects malformed commands, bounded backlog exhaustion and serial overflow.
    pub fn push(&mut self, command: TaskCommand, now: Instant) -> Result<(), CodecError> {
        command.validate()?;
        if self.closed.contains(&command.object) {
            return Ok(());
        }
        let serial = self
            .next
            .checked_add(1)
            .filter(|serial| i64::try_from(*serial).is_ok())
            .ok_or(CodecError::InvalidValue("task command serial"))?;
        self.pending.retain(|_, entry| match command.action {
            TaskAction::Close => entry.command.object != command.object,
            TaskAction::Focus => entry.command.action != TaskAction::Focus,
            TaskAction::Resize { .. } => {
                !(entry.command.object == command.object
                    && matches!(entry.command.action, TaskAction::Resize { .. }))
            }
            TaskAction::Hide | TaskAction::Show => {
                !(entry.command.object == command.object
                    && matches!(entry.command.action, TaskAction::Hide | TaskAction::Show))
            }
            TaskAction::Density { .. } => {
                !(entry.command.object == command.object
                    && matches!(entry.command.action, TaskAction::Density { .. }))
            }
            TaskAction::NewTaskDensity { .. } => {
                !matches!(entry.command.action, TaskAction::NewTaskDensity { .. })
            }
        });
        if self.pending.len() >= MAX_PENDING {
            return Err(CodecError::InvalidValue("task command backlog"));
        }
        self.next = serial;
        if command.action == TaskAction::Close {
            self.closed.insert(command.object);
        }
        let ready = now
            + if matches!(command.action, TaskAction::Resize { .. })
                && !self.resized.insert(command.object)
            {
                RESIZE_DEBOUNCE
            } else {
                Duration::ZERO
            };
        self.pending.insert(
            self.next,
            PendingCommand {
                serial: self.next,
                command,
                ready,
            },
        );
        Ok(())
    }

    /// Peek without removing: a disconnected receiver must not lose host close.
    #[must_use]
    pub fn ready(&self, now: Instant) -> Option<PendingCommand> {
        self.pending
            .values()
            .find(|entry| entry.ready <= now)
            .copied()
    }

    /// Deadline for the next coalesced resize. None means sleep until an event.
    #[must_use]
    pub fn deadline(&self) -> Option<Instant> {
        self.pending.values().map(|entry| entry.ready).min()
    }

    /// Consume only the exact delivered serial, preserving newer replacements.
    pub fn acknowledge(&mut self, serial: u64) {
        self.pending.remove(&serial);
    }

    /// Retiring the object invalidates pending and closed-window identities.
    pub fn discard(&mut self, object: u64) {
        self.pending
            .retain(|_, entry| entry.command.object != object);
        self.closed.remove(&object);
        self.resized.remove(&object);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(object: u64, action: TaskAction) -> TaskCommand {
        TaskCommand {
            object,
            task: 42,
            incarnation: 1,
            android_display: 0,
            action,
        }
    }

    #[test]
    fn a_density_replaces_the_last_and_goes_before_a_debounced_resize() {
        let mut queue = CommandQueue::default();
        let now = Instant::now();
        let resize = |width| command(7, TaskAction::Resize { width, height: 600 });
        queue.push(resize(400), now).unwrap();
        let first = queue.ready(now).unwrap();
        queue.acknowledge(first.serial);
        queue.push(resize(500), now).unwrap();
        queue
            .push(command(7, TaskAction::Density { dpi: 460 }), now)
            .unwrap();
        queue
            .push(command(7, TaskAction::Density { dpi: 230 }), now)
            .unwrap();
        let density = queue.ready(now).unwrap();
        assert_eq!(density.command.action, TaskAction::Density { dpi: 230 });
        queue.acknowledge(density.serial);
        assert!(queue.ready(now).is_none());
        assert!(
            command(7, TaskAction::Density { dpi: 40 })
                .validate()
                .is_err()
        );
        assert!(
            command(7, TaskAction::Density { dpi: 0 })
                .validate()
                .is_ok()
        );
    }

    #[test]
    fn a_new_task_density_names_no_task_and_replaces_the_last() {
        let now = Instant::now();
        let new_tasks = |dpi| TaskCommand {
            object: 3,
            task: 0,
            incarnation: 0,
            android_display: 0,
            action: TaskAction::NewTaskDensity { dpi },
        };
        let mut queue = CommandQueue::default();
        queue.push(new_tasks(460), now).unwrap();
        queue
            .push(command(7, TaskAction::Density { dpi: 460 }), now)
            .unwrap();
        queue.push(new_tasks(345), now).unwrap();
        let density = queue.ready(now).unwrap();
        assert_eq!(density.command.action, TaskAction::Density { dpi: 460 });
        queue.acknowledge(density.serial);
        assert_eq!(queue.ready(now).unwrap().command, new_tasks(345));
        assert!(new_tasks(40).validate().is_err());
        assert!(
            command(7, TaskAction::NewTaskDensity { dpi: 460 })
                .validate()
                .is_err()
        );
        assert!(
            TaskCommand {
                task: 0,
                incarnation: 0,
                ..command(7, TaskAction::Focus)
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn first_resize_is_immediate_and_later_resizes_are_debounced() {
        let mut queue = CommandQueue::default();
        let now = Instant::now();
        let resize = |width| command(7, TaskAction::Resize { width, height: 600 });
        queue.push(resize(400), now).unwrap();
        let first = queue.ready(now).unwrap();
        queue.acknowledge(first.serial);
        queue.push(resize(500), now).unwrap();
        assert!(queue.ready(now).is_none());
        assert!(queue.ready(now + RESIZE_DEBOUNCE).is_some());
        queue.discard(7);
        queue.push(resize(600), now).unwrap(); // a re-registered object starts again
        assert!(queue.ready(now).is_some());
    }

    #[test]
    fn resize_debounce_retains_final_size_and_stale_ack_cannot_remove_successor() {
        let mut queue = CommandQueue::default();
        let now = Instant::now();
        queue.resized.insert(7);
        queue
            .push(
                command(
                    7,
                    TaskAction::Resize {
                        width: 500,
                        height: 600,
                    },
                ),
                now,
            )
            .unwrap();
        assert!(queue.ready(now).is_none());
        let old = queue.ready(now + RESIZE_DEBOUNCE).unwrap();
        queue
            .push(
                command(
                    7,
                    TaskAction::Resize {
                        width: 700,
                        height: 800,
                    },
                ),
                now,
            )
            .unwrap();
        queue.acknowledge(old.serial);
        let new = queue.ready(now + RESIZE_DEBOUNCE).unwrap();
        assert_eq!(
            new.command.action,
            TaskAction::Resize {
                width: 700,
                height: 800
            }
        );
        assert_ne!(new.serial, old.serial);
        queue.acknowledge(new.serial);
        assert!(queue.deadline().is_none());
    }

    #[test]
    fn close_survives_disconnect_and_supersedes_pending_and_late_operations() {
        let mut queue = CommandQueue::default();
        let now = Instant::now();
        queue
            .push(
                command(
                    7,
                    TaskAction::Resize {
                        width: 500,
                        height: 600,
                    },
                ),
                now,
            )
            .unwrap();
        queue.push(command(7, TaskAction::Focus), now).unwrap();
        queue.push(command(7, TaskAction::Close), now).unwrap();
        let close = queue.ready(now).unwrap();
        assert_eq!(close.command.action, TaskAction::Close);
        assert_eq!(queue.ready(now).unwrap(), close); // retry after reconnect
        queue.push(command(7, TaskAction::Focus), now).unwrap();
        queue.acknowledge(close.serial);
        assert!(queue.ready(now).is_none());
        queue.discard(7);
        queue.push(command(8, TaskAction::Focus), now).unwrap(); // reused task ID, new object
        assert_eq!(queue.ready(now).unwrap().command.object, 8);
    }

    #[test]
    fn latest_focus_wins_without_waiting_for_resize_debounce() {
        let mut queue = CommandQueue::default();
        let now = Instant::now();
        queue.resized.insert(7);
        queue
            .push(
                command(
                    7,
                    TaskAction::Resize {
                        width: 500,
                        height: 600,
                    },
                ),
                now,
            )
            .unwrap();
        queue.push(command(7, TaskAction::Focus), now).unwrap();
        queue.push(command(8, TaskAction::Focus), now).unwrap();
        assert_eq!(queue.ready(now).unwrap().command.object, 8);
    }

    #[test]
    fn latest_visibility_wins_and_restore_precedes_its_focus() {
        let mut queue = CommandQueue::default();
        let now = Instant::now();
        queue.push(command(7, TaskAction::Hide), now).unwrap();
        queue.push(command(8, TaskAction::Hide), now).unwrap();
        queue.push(command(7, TaskAction::Show), now).unwrap();
        queue.push(command(7, TaskAction::Focus), now).unwrap();
        let order: Vec<_> = std::iter::from_fn(|| {
            let entry = queue.ready(now)?;
            queue.acknowledge(entry.serial);
            Some((entry.command.object, entry.command.action))
        })
        .collect();
        assert_eq!(
            order,
            [
                (8, TaskAction::Hide),
                (7, TaskAction::Show),
                (7, TaskAction::Focus)
            ]
        );
    }
}
