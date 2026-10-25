//! Shared folders are host directories below `MediaProvider`'s FUSE layer, so a
//! file the host writes there (a screenshot, a download) reaches Android only
//! at `MediaProvider`'s next full scan. This watcher names every changed path to
//! the framework, which asks `MediaProvider` to scan it: new files are indexed
//! and removed ones dropped within a second.
use std::collections::{BTreeSet, HashMap};
use std::ffi::CString;
use std::fs;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::{android_init_pid, framework_control, framework_wire};
use crate::CellSpec;

const ANDROID_ROOT: &str = "/storage/emulated/0";
const MASK: u32 = libc::IN_CLOSE_WRITE
    | libc::IN_MOVED_TO
    | libc::IN_MOVED_FROM
    | libc::IN_DELETE
    | libc::IN_CREATE
    | libc::IN_ONLYDIR
    | libc::IN_DONT_FOLLOW;
/// Quiet time that ends a burst, so a copy or a rename lands as one batch.
const QUIET: Duration = Duration::from_millis(250);
/// A steady stream of changes is still sent this often.
const MAX_DELAY: Duration = Duration::from_secs(2);
/// Wait before retrying while Android's framework is not listening yet.
const RETRY: Duration = Duration::from_secs(2);
/// Beyond this many paths, each shared folder is rescanned whole.
const MAX_PENDING: usize = 1024;
const MAX_BATCH: usize = 256;

/// Watches the cell's shared folders until dropped.
pub(super) struct MediaWatch {
    stop: OwnedFd,
    thread: Option<JoinHandle<()>>,
}

impl Drop for MediaWatch {
    fn drop(&mut self) {
        let value = 1_u64;
        // SAFETY: the eventfd is owned by self and the buffer is 8 bytes.
        unsafe { libc::write(self.stop.as_raw_fd(), (&raw const value).cast(), 8) };
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Starts watching `spec`'s shared folders; none when it shares no folder.
pub(super) fn start(spec: &CellSpec) -> Option<MediaWatch> {
    let folders: Vec<(PathBuf, String)> = spec
        .shared_storage()
        .into_iter()
        .map(|folder| {
            let android = format!("{ANDROID_ROOT}/{}", folder.android_name);
            (folder.source, android)
        })
        .collect();
    if folders.is_empty() {
        return None;
    }
    let (inotify, stop) = match (
        owned(unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) }),
        owned(unsafe { libc::eventfd(0, libc::EFD_CLOEXEC) }),
    ) {
        (Ok(inotify), Ok(stop)) => (inotify, stop),
        (Err(error), _) | (_, Err(error)) => {
            eprintln!("shared-folder media watch unavailable: {error}");
            return None;
        }
    };
    let wake = stop.try_clone().ok()?;
    let spec = spec.clone();
    let thread = std::thread::Builder::new()
        .name("media-scan".into())
        .spawn(move || Watcher::new(inotify, folders).run(&spec, &wake))
        .ok()?;
    Some(MediaWatch {
        stop,
        thread: Some(thread),
    })
}

fn owned(fd: i32) -> io::Result<OwnedFd> {
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a fresh descriptor returned by the kernel, owned from here on.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

struct Watcher {
    inotify: OwnedFd,
    roots: Vec<(PathBuf, String)>,
    /// Watch descriptor → host directory and its Android path.
    watches: HashMap<i32, (PathBuf, String)>,
    pending: BTreeSet<String>,
    first: Option<Instant>,
    last: Instant,
    retry_at: Option<Instant>,
}

impl Watcher {
    fn new(inotify: OwnedFd, roots: Vec<(PathBuf, String)>) -> Self {
        let mut watcher = Self {
            inotify,
            roots: roots.clone(),
            watches: HashMap::new(),
            pending: BTreeSet::new(),
            first: None,
            last: Instant::now(),
            retry_at: None,
        };
        for (host, android) in roots {
            watcher.add_tree(host, android);
        }
        watcher
    }

    fn run(mut self, spec: &CellSpec, stop: &OwnedFd) {
        let mut buffer = vec![0_u8; 64 * 1024];
        loop {
            let mut fds = [
                libc::pollfd {
                    fd: self.inotify.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: stop.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            let timeout = self.timeout();
            // SAFETY: two valid pollfds over descriptors owned for the whole call.
            let ready = unsafe { libc::poll(fds.as_mut_ptr(), 2, timeout) };
            if ready < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                return;
            }
            if fds[1].revents != 0 {
                return;
            }
            if fds[0].revents & libc::POLLIN != 0 {
                self.read_events(&mut buffer);
            }
            if self.due() {
                self.flush(spec);
            }
        }
    }

    /// Milliseconds until the pending batch is due; -1 with nothing pending.
    fn timeout(&self) -> i32 {
        let Some(first) = self.first else { return -1 };
        let due = match self.retry_at {
            Some(retry) => retry,
            None => (self.last + QUIET).min(first + MAX_DELAY),
        };
        let left = due.saturating_duration_since(Instant::now());
        i32::try_from(left.as_millis()).unwrap_or(i32::MAX)
    }

    fn due(&self) -> bool {
        self.first.is_some() && self.timeout() == 0
    }

    fn add_tree(&mut self, host: PathBuf, android: String) {
        let mut stack = vec![(host, android)];
        while let Some((host, android)) = stack.pop() {
            let Ok(path) = CString::new(host.as_os_str().as_bytes()) else {
                continue;
            };
            // SAFETY: a valid descriptor and NUL-terminated path.
            let wd =
                unsafe { libc::inotify_add_watch(self.inotify.as_raw_fd(), path.as_ptr(), MASK) };
            if wd < 0 {
                continue;
            }
            self.watches.insert(wd, (host.clone(), android.clone()));
            let Ok(entries) = fs::read_dir(&host) else {
                continue;
            };
            for entry in entries.flatten() {
                // file_type does not follow symlinks.
                if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                    if let Some(name) = entry.file_name().to_str() {
                        stack.push((entry.path(), format!("{android}/{name}")));
                    }
                }
            }
        }
    }

    /// Drops the watches below a directory that moved away; a move inside the
    /// shared folders adds them again from its new place.
    fn forget_tree(&mut self, host: &Path) {
        let gone: Vec<i32> = self
            .watches
            .iter()
            .filter(|(_, (path, _))| path.starts_with(host))
            .map(|(wd, _)| *wd)
            .collect();
        for wd in gone {
            self.watches.remove(&wd);
            // SAFETY: wd came from this inotify instance.
            unsafe { libc::inotify_rm_watch(self.inotify.as_raw_fd(), wd) };
        }
    }

    fn read_events(&mut self, buffer: &mut [u8]) {
        loop {
            // SAFETY: reads into an owned buffer of the given length.
            let length = unsafe {
                libc::read(
                    self.inotify.as_raw_fd(),
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                )
            };
            let Ok(length) = usize::try_from(length) else {
                return;
            };
            if length == 0 {
                return;
            }
            let mut offset = 0;
            let header = std::mem::size_of::<libc::inotify_event>();
            while offset + header <= length {
                // SAFETY: the kernel wrote a whole event header at this offset.
                let event: libc::inotify_event =
                    unsafe { std::ptr::read_unaligned(buffer[offset..].as_ptr().cast()) };
                let name_end = (offset + header + event.len as usize).min(length);
                let name = &buffer[offset + header..name_end];
                let name = &name[..name
                    .iter()
                    .position(|byte| *byte == 0)
                    .unwrap_or(name.len())];
                self.event(event.wd, event.mask, name);
                offset = name_end;
            }
        }
    }

    fn event(&mut self, wd: i32, mask: u32, name: &[u8]) {
        if mask & libc::IN_Q_OVERFLOW != 0 {
            self.rescan_roots();
            return;
        }
        if mask & libc::IN_IGNORED != 0 {
            self.watches.remove(&wd);
            return;
        }
        let Some((host, android)) = self.watches.get(&wd).cloned() else {
            return;
        };
        let Ok(name) = std::str::from_utf8(name) else {
            return;
        };
        if name.is_empty() {
            return;
        }
        let host = host.join(name);
        let android = format!("{android}/{name}");
        let directory = mask & libc::IN_ISDIR != 0;
        if directory && mask & libc::IN_MOVED_FROM != 0 {
            self.forget_tree(&host);
        }
        if directory && mask & (libc::IN_CREATE | libc::IN_MOVED_TO) != 0 {
            self.add_tree(host, android.clone());
        } else if mask & libc::IN_CREATE != 0 {
            // A new file is scanned once its writer closes it.
            return;
        }
        self.queue(android);
    }

    fn queue(&mut self, android: String) {
        let now = Instant::now();
        self.first.get_or_insert(now);
        self.last = now;
        if self.pending.len() >= MAX_PENDING {
            self.rescan_roots();
        } else {
            self.pending.insert(android);
        }
    }

    fn rescan_roots(&mut self) {
        let now = Instant::now();
        self.first.get_or_insert(now);
        self.last = now;
        self.pending = self
            .roots
            .iter()
            .map(|(_, android)| android.clone())
            .collect();
    }

    fn flush(&mut self, spec: &CellSpec) {
        let paths: Vec<String> = self.pending.iter().cloned().collect();
        for batch in batches(&paths) {
            if let Err(error) = send(spec, batch) {
                // Android may still be starting; MediaProvider's own boot scan
                // covers what happened before it listens.
                eprintln!("shared-folder media scan deferred: {error}");
                self.retry_at = Some(Instant::now() + RETRY);
                return;
            }
            for path in batch {
                self.pending.remove(path);
            }
        }
        self.first = None;
        self.retry_at = None;
    }
}

/// Splits paths into requests within the framework's batch and size limits.
fn batches(paths: &[String]) -> Vec<&[String]> {
    let budget = framework_wire::MAX_REQUEST - 64;
    let mut out = Vec::new();
    let (mut start, mut bytes) = (0, 0);
    for (index, path) in paths.iter().enumerate() {
        let size = serde_json::to_string(path).map_or(usize::MAX / 2, |json| json.len() + 1);
        if index > start && (index - start == MAX_BATCH || bytes + size > budget) {
            out.push(&paths[start..index]);
            (start, bytes) = (index, 0);
        }
        bytes += size;
    }
    if start < paths.len() {
        out.push(&paths[start..]);
    }
    out
}

fn send(spec: &CellSpec, paths: &[String]) -> Result<(), String> {
    let init = android_init_pid(spec)
        .map_err(|error| error.to_string())?
        .ok_or("Android is not running")?;
    let mut stream =
        framework_control::connect(init, "control").map_err(|error| error.to_string())?;
    let timeout = Some(Duration::from_secs(5));
    stream
        .set_read_timeout(timeout)
        .map_err(|error| error.to_string())?;
    stream
        .set_write_timeout(timeout)
        .map_err(|error| error.to_string())?;
    let request = serde_json::json!({"user": 0, "paths": paths});
    let request = serde_json::to_vec(&request).map_err(|error| error.to_string())?;
    framework_wire::write(
        &mut stream,
        framework_wire::MEDIA_SCAN,
        &request,
        framework_wire::MAX_REQUEST,
    )
    .map_err(|error| error.to_string())?;
    let (reply, bytes) = framework_wire::read(&mut stream, framework_wire::MAX_RESPONSE)
        .map_err(|error| error.to_string())?;
    if reply == framework_wire::MEDIA_SCAN {
        Ok(())
    } else {
        Err(format!(
            "Android framework rejected the scan: {}",
            String::from_utf8_lossy(&bytes)
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batches_respect_count_and_size() {
        let short: Vec<String> = (0..600)
            .map(|i| format!("{ANDROID_ROOT}/Pictures/{i}.png"))
            .collect();
        let sizes: Vec<usize> = batches(&short).iter().map(|batch| batch.len()).collect();
        assert_eq!(sizes, [256, 256, 88]);

        let long: Vec<String> = (0..8)
            .map(|i| format!("{ANDROID_ROOT}/{}{i}", "x".repeat(3000)))
            .collect();
        for batch in batches(&long) {
            let json = serde_json::to_vec(&serde_json::json!({"user": 0, "paths": batch})).unwrap();
            assert!(json.len() <= framework_wire::MAX_REQUEST);
        }
        assert_eq!(
            batches(&long)
                .iter()
                .map(|batch| batch.len())
                .sum::<usize>(),
            8
        );
    }

    #[test]
    fn host_changes_queue_android_paths_once() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("Screenshots")).unwrap();
        let inotify =
            owned(unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) }).unwrap();
        let android = format!("{ANDROID_ROOT}/Pictures");
        let mut watcher = Watcher::new(inotify, vec![(root.path().to_owned(), android.clone())]);
        assert_eq!(watcher.watches.len(), 2);

        fs::write(root.path().join("Screenshots/shot.png"), b"png").unwrap();
        fs::create_dir(root.path().join("Album")).unwrap();
        fs::write(root.path().join("note.txt"), b"a").unwrap();
        fs::remove_file(root.path().join("note.txt")).unwrap();
        let mut buffer = vec![0_u8; 64 * 1024];
        watcher.read_events(&mut buffer);

        let pending: Vec<&str> = watcher.pending.iter().map(String::as_str).collect();
        assert_eq!(
            pending,
            [
                format!("{android}/Album"),
                format!("{android}/Screenshots/shot.png"),
                format!("{android}/note.txt"),
            ]
        );
        assert_eq!(watcher.watches.len(), 3);
        assert!(watcher.first.is_some());
    }
}
