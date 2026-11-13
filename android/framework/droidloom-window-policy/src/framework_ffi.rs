//! Owned, root-authenticated framework RPC, coalesced catalog events and the
//! credential prompts the host shows for the framework.
use droidloom_task_control::framework as wire;
use std::{
    collections::VecDeque,
    fs,
    io::{self, Write},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::{
            fs::{FileTypeExt, MetadataExt, PermissionsExt},
            net::{UnixListener, UnixStream},
        },
    },
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex, OnceLock,
    },
    time::Duration,
};

#[unsafe(no_mangle)]
pub static DROIDLOOM_FRAMEWORK_CONTROL_ABI: [u8; 34] = *b"DROIDLOOM_FRAMEWORK_CONTROL_ABI=1;";
struct Listener {
    socket: UnixListener,
    path: PathBuf,
    inode: u64,
}
impl Drop for Listener {
    fn drop(&mut self) {
        if fs::symlink_metadata(&self.path)
            .is_ok_and(|m| m.ino() == self.inode && m.file_type().is_socket())
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}
fn bind(path: &Path) -> io::Result<Listener> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_socket() && m.uid() == unsafe { libc::geteuid() } => {
            if UnixStream::connect(path).is_ok() {
                return Err(io::ErrorKind::AddrInUse.into());
            }
            fs::remove_file(path)?;
        }
        Ok(_) => return Err(io::ErrorKind::PermissionDenied.into()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let socket = UnixListener::bind(path)?;
    let inode = fs::symlink_metadata(path)?.ino();
    let listener = Listener {
        socket,
        path: path.to_owned(),
        inode,
    };
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}
fn allowed(stream: &UnixStream, expected: u32) -> bool {
    let mut credentials = libc::ucred {
        pid: 0,
        uid: u32::MAX,
        gid: u32::MAX,
    };
    let mut length = size_of::<libc::ucred>() as libc::socklen_t;
    (unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut credentials).cast(),
            &raw mut length,
        ) == 0
    }) && length as usize == size_of::<libc::ucred>()
        && credentials.uid == expected
}
struct Receiver {
    listener: Listener,
    pending: Option<(UnixStream, u16)>,
}
impl Receiver {
    fn next(&mut self, uid: u32) -> io::Result<(u16, Vec<u8>)> {
        self.pending = None;
        loop {
            let (mut stream, _) = match self.listener.socket.accept() {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                result => result?,
            };
            if !allowed(&stream, uid) {
                continue;
            }
            stream.set_read_timeout(Some(Duration::from_secs(2)))?;
            stream.set_write_timeout(Some(Duration::from_secs(5)))?;
            let Ok((opcode, request)) = wire::read(&mut stream, wire::MAX_REQUEST) else {
                continue;
            };
            if !(wire::LAUNCH..=wire::CREDENTIAL_ANSWER).contains(&opcode) {
                continue;
            }
            self.pending = Some((stream, opcode));
            return Ok((opcode, request));
        }
    }
    fn reply(&mut self, bytes: &[u8], success: bool) -> bool {
        let Some((mut stream, opcode)) = self.pending.take() else {
            return false;
        };
        wire::write(
            &mut stream,
            opcode | if success { 0 } else { wire::ERROR },
            bytes,
            wire::MAX_RESPONSE,
        )
        .is_ok()
    }
}
struct Events {
    wake: OwnedFd,
    generation: AtomicU64,
    stopped: AtomicBool,
}
/// Credential prompts for root's supervisor on `credential-events`: every open
/// prompt when it connects, then each new one and each end, as a big-endian
/// `u32` length and JSON.
struct Prompts {
    wake: OwnedFd,
    queue: Mutex<PromptQueue>,
    stopped: AtomicBool,
}
#[derive(Default)]
struct PromptQueue {
    open: Vec<(u64, Vec<u8>)>,
    outbox: VecDeque<Vec<u8>>,
}
const MAX_PROMPT: usize = 16 * 1024;
/// BiometricService shows one prompt at a time; a few cover a racing end.
const MAX_OPEN_PROMPTS: usize = 4;
struct State {
    receiver: Mutex<Receiver>,
    events: Arc<Events>,
    prompts: Arc<Prompts>,
    workers: Vec<std::thread::JoinHandle<()>>,
}
impl Drop for State {
    fn drop(&mut self) {
        self.events.stopped.store(true, Ordering::Release);
        signal(&self.events.wake);
        self.prompts.stopped.store(true, Ordering::Release);
        signal(&self.prompts.wake);
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}
fn eventfd() -> io::Result<OwnedFd> {
    let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}
fn signal(wake: &OwnedFd) {
    let value = 1_u64;
    unsafe {
        libc::write(wake.as_raw_fd(), (&raw const value).cast(), 8);
    }
}
fn drain(wake: &OwnedFd) {
    let mut count = 0_u64;
    unsafe {
        libc::read(wake.as_raw_fd(), (&raw mut count).cast(), 8);
    }
}
static STATE: OnceLock<State> = OnceLock::new();

fn start(root: &Path, uid: u32) -> io::Result<State> {
    let listener = bind(&root.join("control"))?;
    let watched = bind(&root.join("catalog-events"))?;
    watched.socket.set_nonblocking(true)?;
    let asked = bind(&root.join("credential-events"))?;
    asked.socket.set_nonblocking(true)?;
    let events = Arc::new(Events {
        wake: eventfd()?,
        generation: AtomicU64::new(1),
        stopped: AtomicBool::new(false),
    });
    let prompts = Arc::new(Prompts {
        wake: eventfd()?,
        queue: Mutex::default(),
        stopped: AtomicBool::new(false),
    });
    let mut state = State {
        receiver: Mutex::new(Receiver {
            listener,
            pending: None,
        }),
        events: Arc::clone(&events),
        prompts: Arc::clone(&prompts),
        workers: Vec::new(),
    };
    state.workers.push(
        std::thread::Builder::new()
            .name("DroidloomCatalog".into())
            .spawn(move || watch(watched, events, uid))?,
    );
    state.workers.push(
        std::thread::Builder::new()
            .name("DroidloomPrompts".into())
            .spawn(move || serve_prompts(asked, prompts, uid))?,
    );
    Ok(state)
}
fn watch(listener: Listener, events: Arc<Events>, uid: u32) {
    let mut peer: Option<UnixStream> = None;
    loop {
        let mut descriptors = [
            libc::pollfd {
                fd: listener.socket.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: events.wake.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: peer.as_ref().map_or(-1, AsRawFd::as_raw_fd),
                events: libc::POLLIN | libc::POLLRDHUP,
                revents: 0,
            },
        ];
        let result = unsafe { libc::poll(descriptors.as_mut_ptr(), 3, -1) };
        if result < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return;
        }
        if events.stopped.load(Ordering::Acquire) {
            return;
        }
        if descriptors[2].revents != 0 {
            peer = None;
        } // EOF or unexpected client data
        let mut changed = false;
        if descriptors[1].revents & libc::POLLIN != 0 {
            drain(&events.wake);
            changed = true;
        }
        if descriptors[0].revents & libc::POLLIN != 0 {
            while let Ok((stream, _)) = listener.socket.accept() {
                if allowed(&stream, uid) {
                    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
                    peer = Some(stream);
                    changed = true;
                }
            }
        }
        if changed {
            if let Some(stream) = &mut peer {
                if stream
                    .write_all(&events.generation.load(Ordering::Acquire).to_be_bytes())
                    .is_err()
                {
                    peer = None;
                }
            }
        }
    }
}
fn send_prompt(stream: &mut UnixStream, record: &[u8]) -> bool {
    stream
        .write_all(&(record.len() as u32).to_be_bytes())
        .and_then(|()| stream.write_all(record))
        .is_ok()
}
fn serve_prompts(listener: Listener, prompts: Arc<Prompts>, uid: u32) {
    let mut peer: Option<UnixStream> = None;
    loop {
        let mut descriptors = [
            libc::pollfd {
                fd: listener.socket.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: prompts.wake.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: peer.as_ref().map_or(-1, AsRawFd::as_raw_fd),
                events: libc::POLLIN | libc::POLLRDHUP,
                revents: 0,
            },
        ];
        let result = unsafe { libc::poll(descriptors.as_mut_ptr(), 3, -1) };
        if result < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return;
        }
        if prompts.stopped.load(Ordering::Acquire) {
            return;
        }
        if descriptors[2].revents != 0 {
            peer = None;
        } // EOF or unexpected client data
        if descriptors[1].revents & libc::POLLIN != 0 {
            drain(&prompts.wake);
        }
        let mut queue = prompts
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let outbox = std::mem::take(&mut queue.outbox);
        if let Some(stream) = &mut peer {
            if !outbox.iter().all(|record| send_prompt(stream, record)) {
                peer = None;
            }
        }
        if descriptors[0].revents & libc::POLLIN != 0 {
            while let Ok((mut stream, _)) = listener.socket.accept() {
                if allowed(&stream, uid)
                    && stream
                        .set_write_timeout(Some(Duration::from_secs(2)))
                        .is_ok()
                    && queue
                        .open
                        .iter()
                        .all(|(_, record)| send_prompt(&mut stream, record))
                {
                    peer = Some(stream);
                }
            }
        }
    }
}
impl Prompts {
    fn change(&self, change: impl FnOnce(&mut PromptQueue) -> bool) -> bool {
        let changed = change(
            &mut self
                .queue
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        if changed {
            signal(&self.wake);
        }
        changed
    }
    fn ask(&self, id: u64, record: Vec<u8>) -> bool {
        self.change(|queue| {
            if queue.open.len() >= MAX_OPEN_PROMPTS
                || queue.open.iter().any(|(open, _)| *open == id)
            {
                return false;
            }
            queue.outbox.push_back(record.clone());
            queue.open.push((id, record));
            true
        })
    }
    fn end(&self, id: u64) {
        self.change(|queue| {
            let before = queue.open.len();
            queue.open.retain(|(open, _)| *open != id);
            if queue.open.len() == before {
                return false;
            }
            queue
                .outbox
                .push_back(format!("{{\"end\":{id}}}").into_bytes());
            true
        });
    }
}
#[unsafe(no_mangle)]
pub extern "C" fn droidloom_framework_start() -> bool {
    if STATE.get().is_some() {
        return true;
    }
    match start(Path::new("/dev/socket/droidloom-framework"), 0) {
        Ok(state) => STATE.set(state).is_ok(),
        Err(error) => {
            eprintln!("Droidloom framework startup: {error}");
            false
        }
    }
}
#[unsafe(no_mangle)]
pub extern "C" fn droidloom_framework_catalog_changed() {
    if let Some(state) = STATE.get() {
        state.events.generation.fetch_add(1, Ordering::AcqRel);
        signal(&state.events.wake);
    }
}
#[unsafe(no_mangle)]
/// # Safety
/// `buffer` must reference `length` initialized bytes, kept alive for this call.
pub unsafe extern "C" fn droidloom_framework_credential_ask(
    id: u64,
    buffer: *const u8,
    length: usize,
) -> bool {
    if buffer.is_null() || length == 0 || length > MAX_PROMPT {
        return false;
    }
    let record = unsafe { std::slice::from_raw_parts(buffer, length) }.to_vec();
    STATE
        .get()
        .is_some_and(|state| state.prompts.ask(id, record))
}
#[unsafe(no_mangle)]
pub extern "C" fn droidloom_framework_credential_end(id: u64) {
    if let Some(state) = STATE.get() {
        state.prompts.end(id);
    }
}
#[unsafe(no_mangle)]
/// # Safety
/// `buffer` must reference `capacity` writable bytes, kept alive for this call.
pub unsafe extern "C" fn droidloom_framework_next(buffer: *mut u8, capacity: usize) -> i32 {
    if buffer.is_null() || capacity < wire::MAX_REQUEST + 2 {
        return -1;
    }
    let Some(state) = STATE.get() else {
        return -1;
    };
    let mut receiver = state
        .receiver
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Ok((opcode, request)) = receiver.next(0) else {
        return -1;
    };
    unsafe {
        std::ptr::copy_nonoverlapping(opcode.to_be_bytes().as_ptr(), buffer, 2);
        std::ptr::copy_nonoverlapping(request.as_ptr(), buffer.add(2), request.len());
    }
    (request.len() + 2) as i32
}
#[unsafe(no_mangle)]
/// # Safety
/// `buffer` must reference `length` initialized bytes, kept alive for this call.
pub unsafe extern "C" fn droidloom_framework_reply(
    buffer: *const u8,
    length: usize,
    success: bool,
) -> bool {
    if buffer.is_null() || length > wire::MAX_RESPONSE {
        return false;
    }
    let Some(state) = STATE.get() else {
        return false;
    };
    let mut receiver = state
        .receiver
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    receiver.reply(
        unsafe { std::slice::from_raw_parts(buffer, length) },
        success,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    #[test]
    fn rpc_drops_bad_frames_and_replies_once_to_each_authenticated_request() {
        let dir = tempfile::tempdir().unwrap();
        let state = start(dir.path(), unsafe { libc::geteuid() }).unwrap();
        assert!(bind(&dir.path().join("control")).is_err());
        let worker = std::thread::spawn(move || {
            let mut receiver = state.receiver.lock().unwrap();
            let request = receiver.next(unsafe { libc::geteuid() }).unwrap();
            assert_eq!(request, (wire::LAUNCH, b"{\"user\":0}".to_vec()));
            assert!(receiver.reply(b"invalid launch", false));
            assert!(!receiver.reply(b"duplicate", true));
            let request = receiver.next(unsafe { libc::geteuid() }).unwrap();
            assert_eq!(request.0, wire::CATALOG);
            assert!(receiver.reply(b"[]", true));
        });
        let path = dir.path().join("control");
        let mut bad = UnixStream::connect(&path).unwrap();
        wire::write(&mut bad, 99, b"{}", wire::MAX_REQUEST).unwrap();
        drop(bad);
        let mut bad = UnixStream::connect(&path).unwrap();
        bad.write_all(b"DLFC\0\x01\0\x01\0\0\x40\x01").unwrap();
        drop(bad);
        for (op, body, expected, reply) in [
            (
                wire::LAUNCH,
                &b"{\"user\":0}"[..],
                wire::LAUNCH | wire::ERROR,
                &b"invalid launch"[..],
            ),
            (
                wire::CATALOG,
                &b"{\"user\":0}"[..],
                wire::CATALOG,
                &b"[]"[..],
            ),
        ] {
            let mut stream = UnixStream::connect(&path).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            wire::write(&mut stream, op, body, wire::MAX_REQUEST).unwrap();
            assert_eq!(
                wire::read(&mut stream, wire::MAX_RESPONSE).unwrap(),
                (expected, reply.to_vec())
            );
            assert_eq!(stream.read(&mut [0]).unwrap(), 0);
        }
        worker.join().unwrap();
    }
    #[test]
    fn wrong_peer_identity_cannot_submit_requests_or_watch_catalog() {
        let dir = tempfile::tempdir().unwrap();
        let state = start(dir.path(), u32::MAX).unwrap();
        let mut stream = UnixStream::connect(dir.path().join("catalog-events")).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        assert_eq!(stream.read(&mut [0; 8]).unwrap(), 0);
        let mut receiver = state.receiver.lock().unwrap();
        receiver.listener.socket.set_nonblocking(true).unwrap();
        let mut stream = UnixStream::connect(dir.path().join("control")).unwrap();
        assert_eq!(
            receiver.next(u32::MAX).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        assert!(receiver.pending.is_none());
        assert_eq!(stream.read(&mut [0]).unwrap(), 0);
    }
    #[test]
    fn owned_paths_reject_unsafe_replacements_and_events_reconnect_without_polling() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blocked");
        fs::write(&path, "retain").unwrap();
        assert!(bind(&path).is_err());
        fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink("missing", &path).unwrap();
        assert!(bind(&path).is_err());
        let state = start(dir.path(), unsafe { libc::geteuid() }).unwrap();
        let mut first = UnixStream::connect(dir.path().join("catalog-events")).unwrap();
        first
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut generation = [0; 8];
        first.read_exact(&mut generation).unwrap();
        assert_eq!(u64::from_be_bytes(generation), 1);
        drop(first);
        let mut second = UnixStream::connect(dir.path().join("catalog-events")).unwrap();
        second
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        second.read_exact(&mut generation).unwrap();
        assert_eq!(u64::from_be_bytes(generation), 1);
        state.events.generation.store(7, Ordering::Release);
        let one = 1_u64;
        unsafe {
            libc::write(state.events.wake.as_raw_fd(), (&raw const one).cast(), 8);
        }
        second.read_exact(&mut generation).unwrap();
        assert_eq!(u64::from_be_bytes(generation), 7);
        assert!(allowed(&second, unsafe { libc::geteuid() }));
        assert!(!allowed(&second, u32::MAX));
    }
    fn prompt(stream: &mut UnixStream) -> Vec<u8> {
        let mut length = [0; 4];
        stream.read_exact(&mut length).unwrap();
        let mut record = vec![0; u32::from_be_bytes(length) as usize];
        stream.read_exact(&mut record).unwrap();
        record
    }
    #[test]
    fn credential_prompts_reach_root_once_and_replay_while_open() {
        let dir = tempfile::tempdir().unwrap();
        let state = start(dir.path(), unsafe { libc::geteuid() }).unwrap();
        assert!(state.prompts.ask(7, b"{\"ask\":7}".to_vec()));
        assert!(
            !state.prompts.ask(7, b"{\"ask\":7}".to_vec()),
            "one record per prompt"
        );
        let path = dir.path().join("credential-events");
        let mut first = UnixStream::connect(&path).unwrap();
        first
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        assert_eq!(prompt(&mut first), b"{\"ask\":7}");
        assert!(state.prompts.ask(8, b"{\"ask\":8}".to_vec()));
        assert_eq!(prompt(&mut first), b"{\"ask\":8}");
        state.prompts.end(7);
        state.prompts.end(7);
        assert_eq!(prompt(&mut first), b"{\"end\":7}");
        drop(first);
        let mut second = UnixStream::connect(&path).unwrap();
        second
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        assert_eq!(
            prompt(&mut second),
            b"{\"ask\":8}",
            "a new supervisor sees what is open"
        );
        drop(second);
        drop(state);
        let other = tempfile::tempdir().unwrap();
        let _state = start(other.path(), u32::MAX).unwrap();
        let mut stranger = UnixStream::connect(other.path().join("credential-events")).unwrap();
        stranger
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        assert_eq!(stranger.read(&mut [0; 4]).unwrap(), 0);
    }
}
