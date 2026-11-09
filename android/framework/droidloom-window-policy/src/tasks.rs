//! Authoritative task registrations from `ActivityTaskManager`, with no shell discovery.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use droidloom_denial_ipc::SeqPacket;
use droidloom_task_control::{
    decode_response, encode_request, ControlErrorCode, ControlRequest, ControlResponse, TaskAction,
    TaskCommand, PROTOCOL_MAJOR, PROTOCOL_MINOR,
};

/// Existing privileged Composer endpoint, shared with `SurfaceFlinger`.
pub const CONTROL_SOCKET: &str = "/dev/socket/droidloom-task-control";

/// An attached Android task incarnation, supplied under the WMS global lock.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Task {
    pub id: u64,
    pub incarnation: u64,
    pub user: u32,
    pub display: u32,
    pub package: String,
    /// A shown window of the task has `FLAG_KEEP_SCREEN_ON`.
    pub keep_screen_on: bool,
}

impl Task {
    /// The same framework task incarnation; its state may differ.
    fn same_identity(&self, other: &Self) -> bool {
        self.id == other.id
            && self.incarnation == other.incarnation
            && self.user == other.user
            && self.display == other.display
            && self.package == other.package
    }
}

/// Stable identities acknowledged by Composer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct Binding {
    pub object: u64,
    pub display: u64,
    pub android_display: u64,
    pub task: u64,
}

struct Client {
    socket: SeqPacket,
    request: u64,
}

impl Client {
    fn connect(path: &Path) -> Result<Self, String> {
        let socket = SeqPacket::connect_nonblocking(path).map_err(|e| e.to_string())?;
        socket.set_nonblocking(false).map_err(|e| e.to_string())?;
        socket
            .set_io_timeout(Duration::from_secs(6))
            .map_err(|e| e.to_string())?;
        // Android init owns this endpoint. The production caller also checks
        // the server's UID; host tests intentionally run under the test user.
        #[cfg(target_os = "android")]
        if !matches!(
            socket.peer_credentials().map_err(|e| e.to_string())?.uid,
            0 | 1000
        ) {
            return Err("task-control peer is not root/system".into());
        }
        let mut client = Self { socket, request: 0 };
        let reply = client.exchange(&ControlRequest::Hello {
            min_major: PROTOCOL_MAJOR,
            max_major: PROTOCOL_MAJOR,
        })?;
        match reply {
            ControlResponse::ServerHello { major, minor }
                if major == PROTOCOL_MAJOR && minor >= PROTOCOL_MINOR =>
            {
                Ok(client)
            }
            _ => Err("incompatible task-control handshake".into()),
        }
    }

    fn next(&mut self) -> Result<u64, String> {
        self.request = self
            .request
            .checked_add(1)
            .ok_or("request identity exhausted")?;
        Ok(self.request)
    }

    fn exchange(&mut self, request: &ControlRequest) -> Result<ControlResponse, String> {
        let bytes = encode_request(request).map_err(|e| e.to_string())?;
        self.socket
            .send_record(&bytes, &[])
            .map_err(|e| e.to_string())?;
        self.receive()
    }

    fn receive(&self) -> Result<ControlResponse, String> {
        let record = self.socket.receive_record().map_err(|e| e.to_string())?;
        if !record.descriptors.is_empty() {
            return Err("task-control reply contains descriptors".into());
        }
        decode_response(&record.bytes).map_err(|e| e.to_string())
    }
}

/// Primitive JNI command facts, resolved against the current Rust registration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct CommandBinding {
    pub serial: u64,
    pub task: u64,
    pub incarnation: u64,
    pub android_display: u64,
    pub kind: u64,
    pub width: u64,
    pub height: u64,
}

/// A separate connection on the existing authenticated endpoint. Only the
/// command receiver thread calls this; publication never waits on its reads.
pub struct CommandStream {
    path: PathBuf,
    client: Option<Client>,
    pending: Option<u64>,
}

impl CommandStream {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            client: None,
            pending: None,
        }
    }

    pub fn next(&mut self) -> Result<(u64, TaskCommand), String> {
        let result = self.receive();
        if result.is_err() {
            self.client = None;
            self.pending = None;
        }
        result
    }

    fn receive(&mut self) -> Result<(u64, TaskCommand), String> {
        if self.pending.is_some() {
            return Err("previous task command was not acknowledged".into());
        }
        if self.client.is_none() {
            let mut client = Client::connect(&self.path)?;
            let request_id = client.next()?;
            match client.exchange(&ControlRequest::Subscribe { request_id })? {
                ControlResponse::Subscribed { request_id: actual } if actual == request_id => {}
                _ => return Err("task command subscription failed".into()),
            }
            self.client = Some(client);
        }
        let client = self
            .client
            .as_ref()
            .ok_or("task command stream disconnected")?;
        // No idle timer wakeups. Socket I/O stays bounded once a record arrives.
        client.socket.wait_readable().map_err(|e| e.to_string())?;
        match client.receive()? {
            ControlResponse::TaskCommand {
                request_id,
                command,
            } => {
                self.pending = Some(request_id);
                Ok((request_id, command))
            }
            _ => Err("unexpected task command stream record".into()),
        }
    }

    pub fn acknowledge(&mut self, serial: u64) -> Result<(), String> {
        let result = (|| {
            if self.pending != Some(serial) {
                return Err("task command acknowledgement serial mismatch".into());
            }
            let client = self
                .client
                .as_ref()
                .ok_or("task command stream disconnected")?;
            let bytes = encode_request(&ControlRequest::Acknowledge { request_id: serial })
                .map_err(|e| e.to_string())?;
            client
                .socket
                .send_record(&bytes, &[])
                .map_err(|e| e.to_string())?;
            self.pending = None;
            Ok(())
        })();
        if result.is_err() {
            self.client = None;
            self.pending = None;
        }
        result
    }
}

/// Called only on the framework's dedicated backend worker, outside WMS locks.
pub struct Backend {
    path: PathBuf,
    client: Option<Client>,
    epoch: u64,
    registered: BTreeMap<u64, (Task, Binding, u64)>,
}

impl Backend {
    /// Bind the native window object to the current framework task incarnation.
    /// Old objects, moved tasks and registrations from a prior connection fail closed.
    pub fn resolve_command(&self, serial: u64, command: TaskCommand) -> Option<CommandBinding> {
        if serial == 0 || serial > i64::MAX as u64 {
            return None;
        }
        command.validate().ok()?;
        if let TaskAction::NewTaskDensity { dpi } = command.action {
            // For the tasks to come: nothing registered to check it against.
            return Some(CommandBinding {
                serial,
                task: 0,
                incarnation: 0,
                android_display: 0,
                kind: 7,
                width: u64::from(dpi),
                height: 0,
            });
        }
        let (task, binding, epoch) = self.registered.get(&command.task)?;
        if binding.object != command.object
            || task.incarnation != command.incarnation
            || task.display != command.android_display
            || task.user != 0
            || *epoch != self.epoch
        {
            return None;
        }
        let (kind, width, height) = match command.action {
            TaskAction::Focus => (1, 0, 0),
            TaskAction::Resize { width, height } => (2, u64::from(width), u64::from(height)),
            TaskAction::Close => (3, 0, 0),
            TaskAction::Hide => (4, 0, 0),
            TaskAction::Show => (5, 0, 0),
            TaskAction::Density { dpi } => (6, u64::from(dpi), 0),
            TaskAction::NewTaskDensity { .. } => unreachable!("resolved above"),
        };
        Some(CommandBinding {
            serial,
            task: task.id,
            incarnation: task.incarnation,
            android_display: u64::from(task.display),
            kind,
            width,
            height,
        })
    }

    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            client: None,
            epoch: 0,
            registered: BTreeMap::new(),
        }
    }

    fn disconnect(&mut self) {
        self.client = None;
        // Keep identities until reconnect: failed removals must remain retryable.
    }

    /// Connect/reconnect and return the connection epoch. No task enumeration.
    pub fn epoch(&mut self) -> Result<u64, String> {
        if self
            .client
            .as_ref()
            .is_some_and(|c| c.socket.peer_closed().unwrap_or(true))
        {
            self.disconnect();
        }
        if self.client.is_none() {
            self.client = Some(Client::connect(&self.path)?);
            self.epoch = self
                .epoch
                .checked_add(1)
                .ok_or("connection epoch exhausted")?;
            // The next register revalidates every binding with Composer. Retain
            // old entries until each is replaced or explicitly removed.
        }
        Ok(self.epoch)
    }

    /// Register one exact framework identity, including reused Android task IDs.
    pub fn register(&mut self, task: Task) -> Result<Binding, String> {
        self.publish(task, false)
    }

    /// An explicit desktop launch requests host activation even for a cached task.
    pub fn activate(&mut self, task: Task) -> Result<Binding, String> {
        self.publish(task, true)
    }

    fn publish(&mut self, task: Task, activate: bool) -> Result<Binding, String> {
        if task.user != 0
            || task.display != 0
            || task.incarnation == 0
            || task.id == 0
            || task.id > i32::MAX as u64
        {
            return Err("task must be an attached user-0/display-0 application".into());
        }
        self.epoch()?;
        if let Some((known, binding, epoch)) = self.registered.get(&task.id) {
            if known == &task && self.epoch == *epoch && !activate {
                return Ok(*binding);
            }
            if !known.same_identity(&task) {
                self.retire(known.id, known.incarnation)?;
            }
        }
        let result = self.register_inner(&task, activate);
        match result {
            Ok(binding) => {
                self.registered.insert(task.id, (task, binding, self.epoch));
                Ok(binding)
            }
            Err(error) => {
                self.disconnect();
                Err(error)
            }
        }
    }

    fn register_inner(&mut self, task: &Task, activate: bool) -> Result<Binding, String> {
        let client = self.client.as_mut().ok_or("task-control disconnected")?;
        let request_id = client.next()?;
        match client.exchange(&ControlRequest::PublishFramework {
            request_id,
            package: task.package.clone(),
            task: task.id,
            android_display: task.display,
            incarnation: task.incarnation,
            activate,
            keep_screen_on: task.keep_screen_on,
        })? {
            ControlResponse::Bound {
                request_id: actual,
                object,
                task: actual_task,
                display,
                android_display,
            } if actual == request_id
                && actual_task == task.id
                && android_display == task.display
                && object > 0
                && i64::try_from(object).is_ok()
                && display > 0
                && i64::try_from(display).is_ok() =>
            {
                Ok(Binding {
                    object,
                    display,
                    android_display: u64::from(android_display),
                    task: task.id,
                })
            }
            reply => Err(format!("invalid task binding: {reply:?}")),
        }
    }

    /// Ignore a stale removal instead of deleting a newer task incarnation.
    pub fn retire(&mut self, task: u64, incarnation: u64) -> Result<(), String> {
        let Some((known, binding, _)) = self.registered.get(&task) else {
            return Ok(());
        };
        if known.incarnation != incarnation {
            return Ok(());
        }
        let object = binding.object;
        self.epoch()?;
        let client = self.client.as_mut().ok_or("task-control disconnected")?;
        let request_id = client.next()?;
        let result = client.exchange(&ControlRequest::Remove { request_id, object });
        match result {
            Ok(ControlResponse::Removed {
                request_id: actual,
                object: removed,
            }) if actual == request_id && removed == object => {}
            Ok(ControlResponse::Error {
                request_id: actual,
                code: ControlErrorCode::NotFound,
                ..
            }) if actual == request_id => {}
            Ok(ControlResponse::Error {
                request_id: actual,
                code,
                message,
            }) if actual == request_id => {
                // A busy teardown is retryable on this healthy connection.
                // Reconnecting would invalidate every other task's bindings.
                return Err(format!("task removal deferred ({code:?}): {message}"));
            }
            reply => {
                self.disconnect();
                return Err(format!("task removal failed: {reply:?}"));
            }
        }
        self.registered.remove(&task);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use droidloom_denial_ipc::SeqPacketListener;
    use droidloom_task_control::{decode_request, encode_response};
    use std::thread;

    fn receive(socket: &SeqPacket) -> ControlRequest {
        let record = socket.receive_record().unwrap();
        assert!(record.descriptors.is_empty());
        decode_request(&record.bytes).unwrap()
    }
    fn send(socket: &SeqPacket, reply: &ControlResponse) {
        socket
            .send_record(&encode_response(reply).unwrap(), &[])
            .unwrap();
    }
    fn hello(listener: &SeqPacketListener) -> SeqPacket {
        let socket = listener.accept().unwrap();
        assert!(matches!(receive(&socket), ControlRequest::Hello { .. }));
        send(
            &socket,
            &ControlResponse::ServerHello {
                major: PROTOCOL_MAJOR,
                minor: PROTOCOL_MINOR,
            },
        );
        socket
    }
    fn register(socket: &SeqPacket, object: u64, expected_activation: bool) {
        register_state(socket, object, expected_activation, false);
    }
    fn register_state(
        socket: &SeqPacket,
        object: u64,
        expected_activation: bool,
        expected_keep_screen_on: bool,
    ) {
        let ControlRequest::PublishFramework {
            request_id,
            task,
            android_display,
            package,
            activate,
            incarnation,
            keep_screen_on,
        } = receive(socket)
        else {
            panic!("expected registration")
        };
        assert_eq!(package, "org.example.app");
        assert_eq!(activate, expected_activation);
        assert_eq!(keep_screen_on, expected_keep_screen_on);
        assert!(incarnation > 0);
        send(
            socket,
            &ControlResponse::Bound {
                request_id,
                object,
                task,
                display: object + 1,
                android_display,
            },
        );
    }
    fn remove(socket: &SeqPacket, expected: u64) {
        let ControlRequest::Remove { request_id, object } = receive(socket) else {
            panic!("expected removal")
        };
        assert_eq!(object, expected);
        send(socket, &ControlResponse::Removed { request_id, object });
    }
    fn task(incarnation: u64) -> Task {
        Task {
            id: 42,
            incarnation,
            user: 0,
            display: 0,
            package: "org.example.app".into(),
            keep_screen_on: false,
        }
    }

    #[test]
    fn keep_screen_on_republishes_the_same_window() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("control");
        let listener = SeqPacketListener::bind(&path).unwrap();
        let server = thread::spawn(move || {
            let socket = hello(&listener);
            register_state(&socket, 100, false, false);
            register_state(&socket, 100, false, true); // No removal before it.
            register_state(&socket, 100, false, false);
        });
        let mut backend = Backend::new(path);
        let first = backend.register(task(1)).unwrap();
        let mut held = task(1);
        held.keep_screen_on = true;
        assert_eq!(backend.register(held.clone()).unwrap(), first);
        assert_eq!(backend.register(held).unwrap(), first); // Unchanged: no I/O.
        assert_eq!(backend.register(task(1)).unwrap(), first);
        server.join().unwrap();
    }

    #[test]
    fn object_identity_guards_reused_tasks_and_connection_epochs() {
        let mut backend = Backend::new("/absent");
        let binding = Binding {
            object: 100,
            display: 101,
            android_display: 0,
            task: 42,
        };
        backend.registered.insert(42, (task(1), binding, 0));
        let mut command = TaskCommand {
            object: 100,
            task: 42,
            incarnation: 1,
            android_display: 0,
            action: TaskAction::Close,
        };
        assert_eq!(backend.resolve_command(1, command).unwrap().incarnation, 1);
        backend.registered.insert(
            42,
            (
                task(2),
                Binding {
                    object: 200,
                    ..binding
                },
                0,
            ),
        );
        assert!(backend.resolve_command(1, command).is_none());
        command.object = 200;
        assert!(backend.resolve_command(1, command).is_none());
        command.incarnation = 2;
        assert_eq!(backend.resolve_command(1, command).unwrap().incarnation, 2);
        backend.epoch = 1;
        assert!(backend.resolve_command(1, command).is_none());
        let new_tasks = TaskCommand {
            object: 5,
            task: 0,
            incarnation: 0,
            android_display: 0,
            action: TaskAction::NewTaskDensity { dpi: 460 },
        };
        let resolved = backend.resolve_command(2, new_tasks).unwrap();
        assert_eq!((resolved.task, resolved.kind, resolved.width), (0, 7, 460));
    }

    #[test]
    fn command_stream_acknowledges_on_its_own_connection_and_reconnects() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("control");
        let listener = SeqPacketListener::bind(&path).unwrap();
        let server = thread::spawn(move || {
            for serial in [9, 10] {
                let socket = hello(&listener);
                let ControlRequest::Subscribe { request_id } = receive(&socket) else {
                    panic!("expected subscription");
                };
                send(&socket, &ControlResponse::Subscribed { request_id });
                send(
                    &socket,
                    &ControlResponse::TaskCommand {
                        request_id: serial,
                        command: TaskCommand {
                            object: 100,
                            task: 42,
                            incarnation: 1,
                            android_display: 0,
                            action: TaskAction::Close,
                        },
                    },
                );
                assert_eq!(
                    receive(&socket),
                    ControlRequest::Acknowledge { request_id: serial }
                );
            }
        });
        let mut stream = CommandStream::new(path);
        let (serial, command) = stream.next().unwrap();
        assert_eq!(serial, 9);
        assert_eq!(command.action, TaskAction::Close);
        stream.acknowledge(serial).unwrap();
        assert!(stream.next().is_err()); // server disconnected after acknowledged command
        let (serial, _) = stream.next().unwrap();
        assert_eq!(serial, 10);
        stream.acknowledge(serial).unwrap();
        server.join().unwrap();
    }

    #[test]
    fn deferred_removal_keeps_other_window_commands_valid() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("control");
        let listener = SeqPacketListener::bind(&path).unwrap();
        let server = thread::spawn(move || {
            let socket = hello(&listener);
            register(&socket, 100, false);
            register(&socket, 200, false);
            let ControlRequest::Remove { request_id, object } = receive(&socket) else {
                panic!("expected removal");
            };
            assert_eq!(object, 100);
            send(
                &socket,
                &ControlResponse::Error {
                    request_id,
                    code: ControlErrorCode::Backend,
                    message: "buffers still in flight".into(),
                },
            );
            remove(&socket, 100); // Retry on the same connection, without a new hello.
        });
        let mut backend = Backend::new(path);
        backend.register(task(1)).unwrap();
        let mut other = task(2);
        other.id = 43;
        backend.register(other).unwrap();
        assert!(backend.retire(42, 1).is_err());
        let resize = TaskCommand {
            object: 200,
            task: 43,
            incarnation: 2,
            android_display: 0,
            action: TaskAction::Resize {
                width: 900,
                height: 700,
            },
        };
        assert!(backend.resolve_command(7, resize).is_some());
        backend.retire(42, 1).unwrap();
        assert!(backend.resolve_command(8, resize).is_some());
        server.join().unwrap();
    }

    #[test]
    fn registration_is_idempotent_and_stale_removals_cannot_remove_reused_ids() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("control");
        let listener = SeqPacketListener::bind(&path).unwrap();
        let server = thread::spawn(move || {
            let socket = hello(&listener);
            register(&socket, 100, false);
            register(&socket, 100, true); // Explicit reopen requests host activation.
            remove(&socket, 100);
            register(&socket, 200, false);
            remove(&socket, 200);
        });
        let mut backend = Backend::new(path);
        let first = backend.register(task(1)).unwrap();
        assert_eq!(backend.register(task(1)).unwrap(), first);
        assert_eq!(backend.activate(task(1)).unwrap(), first);
        assert_eq!(backend.register(task(2)).unwrap().object, 200);
        backend.retire(42, 1).unwrap();
        backend.retire(42, 2).unwrap();
        server.join().unwrap();
    }

    #[test]
    fn reconnect_revalidates_cached_bindings_even_after_a_health_check() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("control");
        let listener = SeqPacketListener::bind(&path).unwrap();
        let (closed, wait) = std::sync::mpsc::channel();
        let server = thread::spawn(move || {
            {
                let socket = hello(&listener);
                register(&socket, 100, false);
            }
            closed.send(()).unwrap();
            let socket = hello(&listener);
            register(&socket, 300, false);
            remove(&socket, 300);
        });
        let mut backend = Backend::new(path);
        assert_eq!(backend.register(task(1)).unwrap().object, 100);
        wait.recv().unwrap();
        assert_eq!(backend.epoch().unwrap(), 2);
        assert_eq!(backend.register(task(1)).unwrap().object, 300);
        backend.retire(42, 1).unwrap();
        server.join().unwrap();
    }

    #[test]
    fn invalid_scope_never_connects() {
        let mut backend = Backend::new("/absent");
        let mut invalid = task(1);
        invalid.user = 10;
        assert!(backend.register(invalid).is_err());
        let mut invalid = task(1);
        invalid.display = 1;
        assert!(backend.register(invalid).is_err());
        assert!(backend.register(task(0)).is_err());
        assert_eq!(backend.epoch, 0);
    }

    #[test]
    fn old_composer_without_window_command_support_is_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("control");
        let listener = SeqPacketListener::bind(&path).unwrap();
        let server = thread::spawn(move || {
            let socket = listener.accept().unwrap();
            assert!(matches!(receive(&socket), ControlRequest::Hello { .. }));
            send(
                &socket,
                &ControlResponse::ServerHello {
                    major: PROTOCOL_MAJOR,
                    minor: 1,
                },
            );
        });
        let mut backend = Backend::new(path);
        assert!(backend.register(task(1)).is_err());
        assert!(backend.client.is_none());
        server.join().unwrap();
    }
}
