//! Opt-in progress frames and a wall-clock deadline for lifecycle clients.

use std::fs;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::{
    ControlError, ControlRequest, ControlResponse, MAX_MESSAGE_BYTES, MAX_RESPONSE_BYTES, apk,
    io_error,
};

pub(super) const LOG_HINT: &str = "Check the logs with:\n  journalctl -b -u droidloomd.service -n 200 --no-pager\n  journalctl --user -b -u droidloom.service -n 100 --no-pager";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(250);
/// A backup or restore moves the whole storage, many gigabytes on a phone.
const STORAGE_TIMEOUT: Duration = Duration::from_secs(3 * 60 * 60);
const UPDATE_INTERVAL: Duration = Duration::from_secs(10);

// Legacy clients still send a bare ControlRequest and receive one JSON object.
// The envelope opts into newline-delimited progress followed by the same result.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ProgressRequest {
    pub request: ControlRequest,
}

#[derive(Deserialize)]
#[serde(untagged)]
pub(super) enum IncomingRequest {
    Progress(ProgressRequest),
    Legacy(ControlRequest),
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProgressFrame {
    progress: String,
}

#[derive(Default)]
pub(super) struct Reporter {
    client: Option<UnixStream>,
    enabled: bool,
    shutdown: Option<OwnedFd>,
}

impl Reporter {
    pub fn new(stream: &UnixStream, enabled: bool) -> Result<Self, ControlError> {
        Ok(Self {
            client: Some(
                stream
                    .try_clone()
                    .map_err(|e| io_error("open lifecycle progress stream", e))?,
            ),
            enabled,
            shutdown: None,
        })
    }

    pub fn with_shutdown(mut self, shutdown: &super::Shutdown) -> Result<Self, ControlError> {
        // SAFETY: shutdown owns this descriptor throughout duplication.
        let fd = unsafe { BorrowedFd::borrow_raw(shutdown.as_raw_fd()) };
        self.shutdown = Some(
            fd.try_clone_to_owned()
                .map_err(|e| io_error("watch request cancellation", e))?,
        );
        Ok(self)
    }

    fn poll(&self, input: Option<&UnixStream>, timeout: Duration) -> io::Result<bool> {
        let entry = |fd, events| libc::pollfd {
            fd,
            events,
            revents: 0,
        };
        let mut fds = [
            entry(input.map_or(-1, AsRawFd::as_raw_fd), libc::POLLIN),
            entry(
                self.shutdown.as_ref().map_or(-1, AsRawFd::as_raw_fd),
                libc::POLLIN,
            ),
            // Clients normally half-close their write side after the request.
            // Only full disconnect (HUP), not RDHUP, cancels their wait.
            entry(self.client.as_ref().map_or(-1, AsRawFd::as_raw_fd), 0),
        ];
        let deadline = Instant::now() + timeout;
        loop {
            let millis = deadline
                .saturating_duration_since(Instant::now())
                .as_millis();
            let millis = i32::try_from(millis).unwrap_or(i32::MAX);
            // SAFETY: every descriptor stays owned until poll returns.
            let result = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, millis) };
            if result < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            if fds[1].revents != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    "daemon is shutting down",
                ));
            }
            if fds[2].revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    "request client disconnected",
                ));
            }
            if fds[0].revents & libc::POLLNVAL != 0 {
                return Err(io::Error::other("framework descriptor became invalid"));
            }
            return Ok(fds[0].revents != 0);
        }
    }

    pub fn shutting_down(&self) -> bool {
        self.shutdown.as_ref().is_some_and(|fd| {
            let mut entry = libc::pollfd {
                fd: fd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: the descriptor stays owned until poll returns.
            unsafe { libc::poll(&raw mut entry, 1, 0) > 0 }
        })
    }

    pub fn check_cancelled(&self) -> io::Result<()> {
        self.poll(None, Duration::ZERO).map(|_| ())
    }

    pub fn reader<'a>(
        &'a self,
        stream: &'a mut UnixStream,
        timeout: Duration,
    ) -> CancellableReader<'a> {
        CancellableReader {
            context: self,
            stream,
            deadline: Instant::now() + timeout,
        }
    }

    pub fn report(&self, message: &str) {
        if !self.enabled {
            return;
        }
        if let Some(mut stream) = self.client.as_ref() {
            let mut encoded = serde_json::to_vec(&ProgressFrame {
                progress: message.into(),
            })
            .expect("progress strings serialize");
            encoded.push(b'\n');
            let _ = stream.write_all(&encoded);
        }
    }
}

pub(super) struct CancellableReader<'a> {
    context: &'a Reporter,
    stream: &'a mut UnixStream,
    deadline: Instant,
}
impl Read for CancellableReader<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() || !self.context.poll(Some(self.stream), remaining)? {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "framework response deadline expired",
            ));
        }
        self.stream.read(buffer)
    }
}

pub(super) fn request(
    socket: &Path,
    request: &ControlRequest,
    file: Option<&fs::File>,
    progress: Option<&mut dyn FnMut(&str)>,
) -> Result<ControlResponse, ControlError> {
    let started = Instant::now();
    let mut stream = connect(socket).map_err(|source| io_error("connect to droidloomd", source))?;
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .map_err(|e| io_error("set lifecycle request write timeout", e))?;
    let encoded = if progress.is_some() {
        serde_json::to_vec(&serde_json::json!({"request": request}))?
    } else {
        serde_json::to_vec(request)?
    };
    if encoded.len() as u64 > MAX_MESSAGE_BYTES {
        return Err(ControlError::Invalid(
            "lifecycle request is too large".into(),
        ));
    }
    let remaining = if let Some(file) = file {
        apk::send_file(&stream, encoded[0], file)?;
        &encoded[1..]
    } else {
        &encoded[..]
    };
    stream
        .write_all(remaining)
        .and_then(|()| stream.shutdown(std::net::Shutdown::Write))
        .map_err(|e| io_error("send lifecycle request", e))?;
    let timeout = match request {
        ControlRequest::Backup { .. } | ControlRequest::Restore { .. } => STORAGE_TIMEOUT,
        _ => REQUEST_TIMEOUT,
    };
    read_response(
        &mut stream,
        started,
        timeout,
        UPDATE_INTERVAL,
        progress,
    )
}

fn connect(path: &Path) -> io::Result<UnixStream> {
    // Blocking connect can wait indefinitely when a busy daemon's accept queue
    // fills. A nonblocking AF_UNIX connect reports that condition as EAGAIN.
    // SAFETY: zero initializes a valid sockaddr_un. The pathname is bounded
    // and NUL terminated below before passing the address to connect.
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    let bytes = path.as_os_str().as_bytes();
    if bytes.is_empty() || bytes.len() >= address.sun_path.len() || bytes.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid lifecycle socket path",
        ));
    }
    address.sun_family =
        libc::sa_family_t::try_from(libc::AF_UNIX).expect("AF_UNIX fits sa_family_t");
    for (target, byte) in address.sun_path.iter_mut().zip(bytes) {
        *target = libc::c_char::from_ne_bytes([*byte]);
    }
    // SAFETY: these constants create a new owned Unix stream descriptor.
    let fd = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            0,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fd is newly created and ownership moves into this stream.
    let stream = unsafe { UnixStream::from_raw_fd(fd) };
    let length = libc::socklen_t::try_from(
        std::mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1,
    )
    .expect("bounded Unix socket address fits socklen_t");
    // SAFETY: address and length describe the initialized pathname above; the
    // descriptor remains owned by stream throughout the call.
    if unsafe {
        libc::connect(
            stream.as_raw_fd(),
            std::ptr::from_ref(&address).cast(),
            length,
        )
    } != 0
    {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::WouldBlock {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                format!("Droidloom is busy and its request queue is full.\n{LOG_HINT}"),
            ));
        }
        return Err(error);
    }
    stream.set_nonblocking(false)?;
    Ok(stream)
}

fn read_response(
    stream: &mut UnixStream,
    started: Instant,
    timeout: Duration,
    update_interval: Duration,
    mut progress: Option<&mut dyn FnMut(&str)>,
) -> Result<ControlResponse, ControlError> {
    let mut last_progress =
        "Waiting for Droidloom to respond; another request may be in progress".to_owned();
    if let Some(report) = progress.as_mut() {
        report(&format!(
            "{last_progress} (up to {} seconds).",
            timeout.as_secs()
        ));
    }
    let mut next_update = started + update_interval;
    let deadline = started + timeout;
    let mut pending = Vec::new();
    let mut total_bytes = 0_u64;
    let mut buffer = [0; 8192];
    loop {
        let now = Instant::now();
        if now >= deadline {
            return Err(ControlError::Invalid(format!(
                "Droidloom did not finish within {} seconds. Android or the lifecycle service may be stuck.\nLast progress: {last_progress}.\n{LOG_HINT}\nThe operation may still finish in the background; check its result before retrying.",
                timeout.as_secs()
            )));
        }
        if now >= next_update {
            if let Some(report) = progress.as_mut() {
                report(&format!(
                    "{last_progress} ({} seconds elapsed; {} seconds remaining).",
                    started.elapsed().as_secs(),
                    deadline.saturating_duration_since(now).as_secs()
                ));
            }
            next_update = now + update_interval;
        }
        stream
            .set_read_timeout(Some(
                deadline.saturating_duration_since(now).min(update_interval),
            ))
            .map_err(|e| io_error("set lifecycle response timeout", e))?;
        let count = match stream.read(&mut buffer) {
            Ok(count) => count,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) =>
            {
                continue;
            }
            Err(e) => return Err(io_error("read lifecycle response", e)),
        };
        total_bytes += count as u64;
        if total_bytes > MAX_RESPONSE_BYTES {
            return Err(ControlError::Invalid(
                "lifecycle response is too large".into(),
            ));
        }
        pending.extend_from_slice(&buffer[..count]);
        while let Some(end) = pending
            .iter()
            .position(|byte| *byte == b'\n')
            .or_else(|| (count == 0 && !pending.is_empty()).then_some(pending.len()))
        {
            let frame: Vec<_> = pending.drain(..end).collect();
            if pending.first() == Some(&b'\n') {
                pending.remove(0);
            }
            if let Ok(frame) = serde_json::from_slice::<ProgressFrame>(&frame) {
                last_progress = frame.progress;
                if let Some(report) = progress.as_mut() {
                    report(&last_progress);
                }
                next_update = Instant::now() + update_interval;
            } else {
                let response: ControlResponse = serde_json::from_slice(&frame)?;
                if !response.ok && response.message.contains("missing field `command`") {
                    return Err(ControlError::Invalid("The running Droidloom daemon does not support boot progress. Update the runtime package and run `droidloomctl restart` to load the matching daemon.".into()));
                }
                return Ok(response);
            }
        }
        if count == 0 {
            return Err(ControlError::Invalid(format!(
                "Droidloom disconnected before reporting a result.\n{LOG_HINT}"
            )));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use std::thread;

    const SUCCESS: &[u8] = br#"{"ok":true,"state":"running","message":"Android is ready"}"#;

    #[test]
    fn progress_envelope_preserves_strict_request_validation() {
        for invalid in [
            r#"{"request":{"command":"install","user":0,"user":10}}"#,
            r#"{"request":{"command":"wait_ready"},"request":{"command":"stop"}}"#,
            r#"{"request":{"command":"wait_ready"},"unexpected":true}"#,
            r#"{"request":{"command":"install","user":0,"unexpected":true}}"#,
        ] {
            assert!(
                serde_json::from_str::<IncomingRequest>(invalid).is_err(),
                "{invalid}"
            );
        }
    }

    #[test]
    fn fragmented_progress_and_result_are_delivered_before_disconnect() {
        let (mut client, mut server) = UnixStream::pair().unwrap();
        let worker = thread::spawn(move || {
            for part in [
                b"{\"pro".as_slice(),
                b"gress\":\"Android boot phase: input service\"}\n",
                SUCCESS,
                b"\n",
            ] {
                server.write_all(part).unwrap();
            }
            // The final newline is enough; the client need not wait for EOF.
            thread::sleep(Duration::from_millis(100));
        });
        let mut messages = Vec::new();
        let response = read_response(
            &mut client,
            Instant::now(),
            Duration::from_secs(1),
            Duration::from_millis(10),
            Some(&mut |message| messages.push(message.to_owned())),
        )
        .unwrap();
        assert!(response.ok);
        assert!(
            messages
                .iter()
                .any(|message| message == "Android boot phase: input service")
        );
        worker.join().unwrap();
    }

    #[test]
    fn legacy_result_without_newline_still_works() {
        let (mut client, mut server) = UnixStream::pair().unwrap();
        server.write_all(SUCCESS).unwrap();
        drop(server);
        assert!(
            read_response(
                &mut client,
                Instant::now(),
                Duration::from_secs(1),
                UPDATE_INTERVAL,
                None
            )
            .unwrap()
            .ok
        );
    }

    #[test]
    fn silent_and_chatty_daemons_both_have_a_wall_clock_deadline() {
        for chatty in [false, true] {
            let (mut client, mut server) = UnixStream::pair().unwrap();
            let worker = thread::spawn(move || {
                let until = Instant::now() + Duration::from_millis(200);
                while Instant::now() < until {
                    if chatty
                        && server
                            .write_all(b"{\"progress\":\"waiting for Android boot\"}\n")
                            .is_err()
                    {
                        break;
                    }
                    thread::sleep(Duration::from_millis(5));
                }
            });
            let started = Instant::now();
            let mut messages = Vec::new();
            let error = read_response(
                &mut client,
                started,
                Duration::from_millis(60),
                Duration::from_millis(10),
                Some(&mut |message| messages.push(message.to_owned())),
            )
            .unwrap_err()
            .to_string();
            assert!(started.elapsed() < Duration::from_secs(1));
            assert!(error.contains("may be stuck"));
            assert!(error.contains("journalctl -b -u droidloomd.service"));
            assert!(error.contains("may still finish in the background"));
            if !chatty {
                assert!(
                    messages
                        .iter()
                        .any(|message| message.contains("seconds remaining"))
                );
            }
            drop(client);
            worker.join().unwrap();
        }
    }

    #[test]
    fn partial_response_and_progress_only_disconnect_do_not_report_success() {
        for bytes in [b"{\"ok\":true".as_slice(), b"{\"progress\":\"booting\"}\n"] {
            let (mut client, mut server) = UnixStream::pair().unwrap();
            server.write_all(bytes).unwrap();
            drop(server);
            assert!(
                read_response(
                    &mut client,
                    Instant::now(),
                    Duration::from_secs(1),
                    UPDATE_INTERVAL,
                    None
                )
                .is_err()
            );
        }
    }

    #[test]
    fn full_accept_queue_is_reported_without_blocking_connect() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("busy.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        // SAFETY: listener owns an open socket; a zero backlog allows one
        // queued Linux connection, which is kept alive below.
        assert_eq!(unsafe { libc::listen(listener.as_raw_fd(), 0) }, 0);
        let _queued = connect(&socket).unwrap();
        let started = Instant::now();
        let error = connect(&socket).unwrap_err().to_string();
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(error.contains("request queue is full"));
    }
}

#[cfg(test)]
mod cancellation_tests {
    use super::*;
    use std::{net::Shutdown as SocketShutdown, sync::Arc, thread};

    #[test]
    fn half_closed_request_still_receives_framework_response() {
        let (client, server) = UnixStream::pair().unwrap();
        client.shutdown(SocketShutdown::Write).unwrap();
        let context = Reporter::new(&server, false).unwrap();
        context.check_cancelled().unwrap();
        let (mut framework, mut peer) = UnixStream::pair().unwrap();
        peer.write_all(b"response").unwrap();
        let mut received = [0; 8];
        context
            .reader(&mut framework, Duration::from_secs(1))
            .read_exact(&mut received)
            .unwrap();
        assert_eq!(&received, b"response");
    }

    #[test]
    fn shutdown_interrupts_stalled_framework_reply() {
        let (client, server) = UnixStream::pair().unwrap();
        let shutdown = Arc::new(super::super::Shutdown::new().unwrap());
        let context = Reporter::new(&server, false)
            .unwrap()
            .with_shutdown(&shutdown)
            .unwrap();
        let (mut framework, _peer) = UnixStream::pair().unwrap();
        let trigger = Arc::clone(&shutdown);
        let worker = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            trigger.request().unwrap();
        });
        let start = Instant::now();
        let error = context
            .reader(&mut framework, Duration::from_secs(120))
            .read_exact(&mut [0; 1])
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::ConnectionAborted);
        assert!(start.elapsed() < Duration::from_secs(1));
        worker.join().unwrap();
        drop(client);
    }

    #[test]
    fn client_disconnect_interrupts_partial_framework_frame() {
        let (client, server) = UnixStream::pair().unwrap();
        let context = Reporter::new(&server, false).unwrap();
        let (mut framework, mut peer) = UnixStream::pair().unwrap();
        peer.write_all(b"partial").unwrap();
        let worker = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            drop(client);
        });
        let start = Instant::now();
        let error = context
            .reader(&mut framework, Duration::from_secs(120))
            .read_exact(&mut [0; 32])
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::ConnectionAborted);
        assert!(start.elapsed() < Duration::from_secs(1));
        worker.join().unwrap();
    }

    #[test]
    fn response_deadline_is_not_reset_by_partial_reads() {
        let (_client, server) = UnixStream::pair().unwrap();
        let context = Reporter::new(&server, false).unwrap();
        let (mut framework, mut peer) = UnixStream::pair().unwrap();
        peer.write_all(b"x").unwrap();
        let mut reader = context.reader(&mut framework, Duration::from_millis(30));
        reader.read_exact(&mut [0; 1]).unwrap();
        thread::sleep(Duration::from_millis(40));
        peer.write_all(b"y").unwrap();
        assert_eq!(
            reader.read_exact(&mut [0; 1]).unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
    }

    #[test]
    fn shutdown_remains_cancelled_for_readiness_checks() {
        let (_client, server) = UnixStream::pair().unwrap();
        let shutdown = super::super::Shutdown::new().unwrap();
        let context = Reporter::new(&server, false)
            .unwrap()
            .with_shutdown(&shutdown)
            .unwrap();
        shutdown.request().unwrap();
        for _ in 0..2 {
            assert_eq!(
                context.check_cancelled().unwrap_err().kind(),
                io::ErrorKind::ConnectionAborted
            );
        }
    }
}
