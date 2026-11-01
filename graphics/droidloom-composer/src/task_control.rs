//! Monotonic pre-launch task/display/object reservations.
//!
//! Android cannot provide a real `ActivityTaskManager` task ID until after an
//! activity has launched, while the dedicated logical display must exist
//! before that launch. This registry therefore stages a display and Denial
//! object first, then binds the real task ID without ever inventing a synthetic
//! framework identity. IDs are never reused within one service process.

use std::collections::BTreeMap;

use droidloom_denial_protocol::TaskObjectId;
use thiserror::Error;

use crate::{
    DisplayId, MAX_TASK_WINDOWS, ReservedTaskWindowSpec, TaskId, TaskWindowError, TaskWindowSpec,
};

const MAX_EXTERNAL_ID: u64 = i64::MAX as u64;

/// One task reservation's externally observable lifecycle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskRegistrationPhase {
    /// IDs are allocated but neither endpoint may have observed them yet.
    Reserved,
    /// Composer state and the Denial task object were staged atomically.
    Staged,
    /// Initial configure completed and `SurfaceFlinger` received hotplug.
    Connected,
    /// `ActivityTaskManager` assigned and reported the real task identity.
    Bound,
    /// New presentation is stopped while release/teardown completes.
    Removing,
}

/// Immutable reservation plus optional Android identity and current phase.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskRegistration {
    /// Denial protocol object allocated for this window lifetime.
    pub object: TaskObjectId,
    /// Display/package mapping known before Android launches the activity.
    pub reservation: ReservedTaskWindowSpec,
    /// Real `ActivityTaskManager` identity, present only after binding.
    pub task: Option<TaskId>,
    /// Android logical display ID used to target framework input injection.
    pub android_display: Option<u32>,
    /// Current lifecycle phase.
    pub phase: TaskRegistrationPhase,
}

impl TaskRegistration {
    /// Return the complete task mapping after `ActivityTaskManager` binding.
    pub fn bound_spec(&self) -> Option<TaskWindowSpec> {
        self.task.map(|task| TaskWindowSpec {
            task,
            display: self.reservation.display,
            package: self.reservation.package.clone(),
        })
    }
}

/// Invalid reservation or bridge phase transition.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum TaskControlError {
    /// The bounded live-task table is full.
    #[error("the Android task-registration limit has been reached")]
    TaskLimit,
    /// A live registration already owns this task identity.
    #[error("Android task {0:?} already has a registration")]
    DuplicateTask(TaskId),
    /// No live registration owns this protocol object.
    #[error("Droidloom object {0:?} has no registration")]
    UnknownObject(TaskObjectId),
    /// No live registration owns this task identity.
    #[error("Android task {0:?} has no registration")]
    UnknownTask(TaskId),
    /// A phase transition arrived out of order.
    #[error("Droidloom object {object:?} cannot move from {actual:?} to {requested:?}")]
    InvalidPhase {
        /// Object whose transition was rejected.
        object: TaskObjectId,
        /// Current phase.
        actual: TaskRegistrationPhase,
        /// Requested phase.
        requested: TaskRegistrationPhase,
    },
    /// No positive signed-64-bit display or protocol object IDs remain.
    #[error("task display/object identity space is exhausted")]
    IdentifierExhausted,
    /// Display or package input cannot form a valid reservation.
    #[error(transparent)]
    InvalidSpec(#[from] TaskWindowError),
}

/// Bounded, monotonic reservation table shared by the task-control socket and
/// Composer lifecycle coordinator.
#[derive(Debug)]
pub struct TaskControlRegistry {
    registrations: BTreeMap<TaskObjectId, TaskRegistration>,
    tasks: BTreeMap<TaskId, TaskObjectId>,
    next_display: Option<u64>,
    next_object: Option<u64>,
}

impl Default for TaskControlRegistry {
    fn default() -> Self {
        Self {
            registrations: BTreeMap::new(),
            tasks: BTreeMap::new(),
            next_display: Some(1),
            next_object: Some(1),
        }
    }
}

impl TaskControlRegistry {
    /// Construct a registry with explicit first IDs for deterministic restore
    /// or boundary testing.
    ///
    /// # Errors
    ///
    /// Rejects zero or values that cannot cross signed AIDL boundaries.
    pub fn with_first_ids(first_display: u64, first_object: u64) -> Result<Self, TaskControlError> {
        if !(1..=MAX_EXTERNAL_ID).contains(&first_display)
            || !(1..=MAX_EXTERNAL_ID).contains(&first_object)
        {
            return Err(TaskControlError::IdentifierExhausted);
        }
        Ok(Self {
            registrations: BTreeMap::new(),
            tasks: BTreeMap::new(),
            next_display: Some(first_display),
            next_object: Some(first_object),
        })
    }

    /// Reserve fresh display and Denial-object identities before app launch.
    /// A later cancellation consumes rather than reuses these IDs.
    ///
    /// # Errors
    ///
    /// Rejects malformed packages, table capacity, and identity exhaustion.
    pub fn reserve(&mut self, package: String) -> Result<TaskRegistration, TaskControlError> {
        if self.registrations.len() >= MAX_TASK_WINDOWS {
            return Err(TaskControlError::TaskLimit);
        }
        let display = self
            .next_display
            .ok_or(TaskControlError::IdentifierExhausted)?;
        let object = self
            .next_object
            .ok_or(TaskControlError::IdentifierExhausted)?;
        let object = TaskObjectId(object);
        let registration = TaskRegistration {
            object,
            reservation: ReservedTaskWindowSpec {
                display: DisplayId(display),
                package,
            },
            task: None,
            android_display: None,
            phase: TaskRegistrationPhase::Reserved,
        };
        registration.reservation.validate()?;

        self.next_display = next_external_id(display);
        self.next_object = next_external_id(object.0);
        self.registrations.insert(object, registration.clone());
        Ok(registration)
    }

    /// Mark a reservation visible to both Composer and Denial endpoints.
    ///
    /// # Errors
    ///
    /// Rejects unknown objects and transitions other than `Reserved -> Staged`.
    pub fn mark_staged(&mut self, object: TaskObjectId) -> Result<(), TaskControlError> {
        self.transition(
            object,
            TaskRegistrationPhase::Reserved,
            TaskRegistrationPhase::Staged,
        )
    }

    /// Mark initial configure/hotplug completion, making the display launchable.
    ///
    /// # Errors
    ///
    /// Rejects unknown objects and transitions other than `Staged -> Connected`.
    pub fn mark_connected(&mut self, object: TaskObjectId) -> Result<(), TaskControlError> {
        self.transition(
            object,
            TaskRegistrationPhase::Staged,
            TaskRegistrationPhase::Connected,
        )
    }

    /// Bind the real task ID returned after launch on the connected display.
    ///
    /// # Errors
    ///
    /// Requires `Connected`, a nonzero ID, and unique task ownership.
    pub fn bind_task(
        &mut self,
        object: TaskObjectId,
        task: TaskId,
        android_display: u32,
    ) -> Result<TaskWindowSpec, TaskControlError> {
        if task.0 == 0 {
            return Err(TaskWindowError::InvalidTask.into());
        }
        if self.tasks.contains_key(&task) {
            return Err(TaskControlError::DuplicateTask(task));
        }
        let registration = self
            .registrations
            .get_mut(&object)
            .ok_or(TaskControlError::UnknownObject(object))?;
        if registration.phase != TaskRegistrationPhase::Connected {
            return Err(TaskControlError::InvalidPhase {
                object,
                actual: registration.phase,
                requested: TaskRegistrationPhase::Bound,
            });
        }
        registration.task = Some(task);
        registration.android_display = Some(android_display);
        registration.phase = TaskRegistrationPhase::Bound;
        self.tasks.insert(task, object);
        Ok(TaskWindowSpec {
            task,
            display: registration.reservation.display,
            package: registration.reservation.package.clone(),
        })
    }

    /// Stop admitting new work before coordinated teardown.
    ///
    /// # Errors
    ///
    /// Rejects unknown, reserved, or already-removing registrations.
    pub fn begin_removal(&mut self, object: TaskObjectId) -> Result<(), TaskControlError> {
        let registration = self
            .registrations
            .get_mut(&object)
            .ok_or(TaskControlError::UnknownObject(object))?;
        if !matches!(
            registration.phase,
            TaskRegistrationPhase::Staged
                | TaskRegistrationPhase::Connected
                | TaskRegistrationPhase::Bound
        ) {
            return Err(TaskControlError::InvalidPhase {
                object,
                actual: registration.phase,
                requested: TaskRegistrationPhase::Removing,
            });
        }
        registration.phase = TaskRegistrationPhase::Removing;
        Ok(())
    }

    /// Forget a reservation that failed before either endpoint observed it.
    ///
    /// # Errors
    ///
    /// Requires an existing registration in the `Reserved` phase.
    pub fn cancel_reservation(
        &mut self,
        object: TaskObjectId,
    ) -> Result<TaskRegistration, TaskControlError> {
        self.remove_in_phase(object, TaskRegistrationPhase::Reserved)
    }

    /// Complete teardown after both endpoints dropped the task.
    ///
    /// # Errors
    ///
    /// Requires an existing registration in the `Removing` phase.
    pub fn finish_removal(
        &mut self,
        object: TaskObjectId,
    ) -> Result<TaskRegistration, TaskControlError> {
        self.remove_in_phase(object, TaskRegistrationPhase::Removing)
    }

    /// Every live registration's protocol object.
    pub fn objects(&self) -> impl Iterator<Item = TaskObjectId> + '_ {
        self.registrations.keys().copied()
    }

    /// Borrow one live registration by protocol object.
    ///
    /// # Errors
    ///
    /// Rejects an unknown object.
    pub fn get(&self, object: TaskObjectId) -> Result<&TaskRegistration, TaskControlError> {
        self.registrations
            .get(&object)
            .ok_or(TaskControlError::UnknownObject(object))
    }

    /// Resolve a real Android task ID to its registration.
    ///
    /// # Errors
    ///
    /// Rejects an unknown or not-yet-bound task.
    pub fn for_task(&self, task: TaskId) -> Result<&TaskRegistration, TaskControlError> {
        let object = self
            .tasks
            .get(&task)
            .copied()
            .ok_or(TaskControlError::UnknownTask(task))?;
        self.get(object)
    }

    /// Resolve a staged logical display to its registration.
    ///
    /// # Errors
    ///
    /// Rejects a display without a live registration.
    pub fn for_display(&self, display: DisplayId) -> Result<&TaskRegistration, TaskControlError> {
        self.registrations
            .values()
            .find(|registration| registration.reservation.display == display)
            .ok_or(TaskControlError::InvalidSpec(
                TaskWindowError::UnknownDisplay(display),
            ))
    }

    fn transition(
        &mut self,
        object: TaskObjectId,
        expected: TaskRegistrationPhase,
        requested: TaskRegistrationPhase,
    ) -> Result<(), TaskControlError> {
        let registration = self
            .registrations
            .get_mut(&object)
            .ok_or(TaskControlError::UnknownObject(object))?;
        if registration.phase != expected {
            return Err(TaskControlError::InvalidPhase {
                object,
                actual: registration.phase,
                requested,
            });
        }
        registration.phase = requested;
        Ok(())
    }

    fn remove_in_phase(
        &mut self,
        object: TaskObjectId,
        expected: TaskRegistrationPhase,
    ) -> Result<TaskRegistration, TaskControlError> {
        let actual = self.get(object)?.phase;
        if actual != expected {
            return Err(TaskControlError::InvalidPhase {
                object,
                actual,
                requested: expected,
            });
        }
        let registration = self
            .registrations
            .remove(&object)
            .ok_or(TaskControlError::UnknownObject(object))?;
        if let Some(task) = registration.task {
            self.tasks.remove(&task);
        }
        Ok(registration)
    }
}

const fn next_external_id(current: u64) -> Option<u64> {
    match current.checked_add(1) {
        Some(next) if next <= MAX_EXTERNAL_ID => Some(next),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn separate_reservations_receive_monotonic_display_and_object_ids() {
        let mut registry = TaskControlRegistry::default();
        let mail = registry.reserve("org.example.mail".to_owned()).unwrap();
        let maps = registry.reserve("org.example.maps".to_owned()).unwrap();

        assert_eq!(mail.reservation.display, DisplayId(1));
        assert_eq!(mail.object, TaskObjectId(1));
        assert_eq!(maps.reservation.display, DisplayId(2));
        assert_eq!(maps.object, TaskObjectId(2));
        assert_eq!(mail.task, None);
    }

    #[test]
    fn cancelled_ids_are_never_reused() {
        let mut registry = TaskControlRegistry::default();
        let first = registry.reserve("org.example.first".to_owned()).unwrap();
        registry.cancel_reservation(first.object).unwrap();
        let next = registry.reserve("org.example.second".to_owned()).unwrap();

        assert_eq!(next.reservation.display, DisplayId(2));
        assert_eq!(next.object, TaskObjectId(2));
    }

    #[test]
    fn objects_lists_only_live_registrations() {
        let mut registry = TaskControlRegistry::default();
        let first = registry.reserve("org.example.first".to_owned()).unwrap();
        let second = registry.reserve("org.example.second".to_owned()).unwrap();
        registry.cancel_reservation(first.object).unwrap();

        assert_eq!(registry.objects().collect::<Vec<_>>(), [second.object]);
    }

    #[test]
    fn real_task_binds_only_after_display_connects() {
        let mut registry = TaskControlRegistry::default();
        let registration = registry.reserve("org.example.app".to_owned()).unwrap();
        assert!(matches!(
            registry.bind_task(registration.object, TaskId(10), 3),
            Err(TaskControlError::InvalidPhase { .. })
        ));
        registry.mark_staged(registration.object).unwrap();
        registry.mark_connected(registration.object).unwrap();
        let spec = registry
            .bind_task(registration.object, TaskId(10), 3)
            .unwrap();
        assert_eq!(spec.task, TaskId(10));
        assert_eq!(
            registry.get(registration.object).unwrap().android_display,
            Some(3)
        );
        assert_eq!(
            registry.for_task(TaskId(10)).unwrap().object,
            registration.object
        );
    }

    #[test]
    fn lifecycle_removal_accepts_bound_and_unbound_windows() {
        let mut registry = TaskControlRegistry::default();
        let bound = registry.reserve("org.example.bound".to_owned()).unwrap();
        registry.mark_staged(bound.object).unwrap();
        registry.mark_connected(bound.object).unwrap();
        registry.bind_task(bound.object, TaskId(10), 3).unwrap();
        registry.begin_removal(bound.object).unwrap();
        let removed = registry.finish_removal(bound.object).unwrap();
        assert_eq!(removed.phase, TaskRegistrationPhase::Removing);
        assert!(matches!(
            registry.get(bound.object),
            Err(TaskControlError::UnknownObject(_))
        ));

        let unbound = registry.reserve("org.example.unbound".to_owned()).unwrap();
        registry.mark_staged(unbound.object).unwrap();
        registry.begin_removal(unbound.object).unwrap();
        registry.finish_removal(unbound.object).unwrap();
    }

    #[test]
    fn malformed_and_exhausted_reservations_fail_before_insertion() {
        let mut registry = TaskControlRegistry::default();
        assert_eq!(
            registry.reserve("not/a/package".to_owned()),
            Err(TaskControlError::InvalidSpec(
                TaskWindowError::InvalidPackage
            ))
        );

        let mut last = TaskControlRegistry::with_first_ids(MAX_EXTERNAL_ID, MAX_EXTERNAL_ID)
            .expect("last signed IDs are valid");
        last.reserve("org.example.last".to_owned()).unwrap();
        assert_eq!(
            last.reserve("org.example.exhausted".to_owned()),
            Err(TaskControlError::IdentifierExhausted)
        );
    }
}
