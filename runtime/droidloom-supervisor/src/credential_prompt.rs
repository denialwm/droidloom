//! Android's credential prompts, answered with the host's screen lock. An app
//! that asks to confirm the device credential gets the host user's PIN or
//! account password, as on a phone: `system_server` announces each prompt on
//! the framework's `credential-events` socket, the user's device broker shows
//! the compositor's sheet, and what the user types comes back here. Root
//! checks it, the PIN with the gatekeeper provider that holds the lock PIN and
//! the password with PAM, then tells `system_server`, which attests the prompt
//! with Android's own credential. The broker and the compositor judge nothing.
use std::collections::HashMap;
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

use async_io::Async;
use droidloom_denial_ipc::{IpcError, SeqPacket};
use droidloom_device_protocol::{MAX_RECORD, Message};
use futures_lite::{AsyncReadExt, future};

use super::host_lock::{self, RETRY, Watch};
use super::{android_init_pid, framework_control, framework_wire, peer_credentials};
use crate::CellSpec;

/// Largest prompt record `system_server` sends.
const MAX_PROMPT: usize = 16 * 1024;

pub(super) fn start(spec: &CellSpec) -> Option<Watch> {
    let spec = spec.clone();
    host_lock::spawn("credential-prompts", move |stop| async move {
        run(&spec, &stop).await;
    })
}

async fn run(spec: &CellSpec, stop: &Async<OwnedFd>) {
    // Android may take a while to listen; each new failure is logged once.
    let mut last = String::new();
    loop {
        match follow(spec, stop).await {
            Ok(()) => return,
            Err(error) if error != last => {
                eprintln!("credential prompts: {error}");
                last = error;
            }
            Err(_) => {}
        }
        let stopped = future::or(
            async {
                let _ = stop.readable().await;
                true
            },
            async {
                async_io::Timer::after(RETRY).await;
                false
            },
        )
        .await;
        if stopped {
            return;
        }
    }
}

/// A prompt the broker shows.
struct Prompt {
    broker: Arc<SeqPacket>,
    ended: Arc<AtomicBool>,
    thread: JoinHandle<()>,
}

impl Prompt {
    /// Android ended it: the broker closes the sheet, and nothing answers.
    fn end(&self) {
        self.ended.store(true, Ordering::Release);
        // SAFETY: a live socket descriptor owned by the broker handle.
        unsafe { libc::shutdown(self.broker.as_fd().as_raw_fd(), libc::SHUT_RDWR) };
    }
}

/// Follows one `system_server`'s prompts until it goes (Err) or the watch
/// stops (Ok). The prompts it announced end with it.
async fn follow(spec: &CellSpec, stop: &Async<OwnedFd>) -> Result<(), String> {
    let init = android_init_pid(spec)
        .map_err(|error| error.to_string())?
        .ok_or("Android is not running")?;
    let stream =
        framework_control::connect(init, "credential-events").map_err(|error| error.to_string())?;
    host_lock::system_server(&stream)?;
    let mut stream = Async::new(stream).map_err(|error| error.to_string())?;
    let mut prompts: HashMap<u64, Prompt> = HashMap::new();
    let result = loop {
        let record = future::or(
            async {
                let _ = stop.readable().await;
                None
            },
            async { Some(read(&mut stream).await) },
        )
        .await;
        let record = match record {
            None => break Ok(()),
            Some(Ok(record)) => record,
            Some(Err(error)) => break Err(format!("system_server went: {error}")),
        };
        prompts.retain(|_, prompt| !prompt.thread.is_finished());
        let Ok(record) = serde_json::from_slice::<serde_json::Value>(&record) else {
            break Err("system_server sent a prompt that is not JSON".into());
        };
        if let Some(id) = record["end"].as_u64() {
            if let Some(prompt) = prompts.remove(&id) {
                prompt.end();
            }
            continue;
        }
        let Some(id) = record["ask"].as_u64() else {
            break Err("system_server sent an unknown prompt record".into());
        };
        let text = |name: &str| record[name].as_str().unwrap_or_default().to_owned();
        let ask = Ask {
            app: text("app"),
            title: text("title"),
            description: text("description"),
            fingerprint: record["fingerprint"].as_bool().unwrap_or(false),
        };
        match ask.show(spec, init, id) {
            Ok(prompt) => {
                if let Some(older) = prompts.insert(id, prompt) {
                    older.end();
                }
            }
            Err(error) => {
                eprintln!("credential prompt {id} not shown: {error}");
                if let Err(error) = answer(init, id, false) {
                    eprintln!("credential prompt {id} not cancelled: {error}");
                }
            }
        }
    };
    for prompt in prompts.values() {
        prompt.end();
    }
    for (_, prompt) in prompts {
        let _ = prompt.thread.join();
    }
    result
}

async fn read(stream: &mut Async<UnixStream>) -> io::Result<Vec<u8>> {
    let mut length = [0; 4];
    stream.read_exact(&mut length).await?;
    let length = u32::from_be_bytes(length) as usize;
    if length == 0 || length > MAX_PROMPT {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let mut record = vec![0; length];
    stream.read_exact(&mut record).await?;
    Ok(record)
}

/// Tells `system_server` how a prompt ended.
fn answer(init: u32, id: u64, confirmed: bool) -> Result<(), String> {
    host_lock::request(
        init,
        framework_wire::CREDENTIAL_ANSWER,
        &serde_json::json!({"user": 0, "id": id, "confirmed": confirmed}),
    )
}

struct Ask {
    app: String,
    title: String,
    description: String,
    /// The fingerprint is also asked for; the sheet waits behind it.
    fingerprint: bool,
}

impl Ask {
    /// Asks the user's broker to show the sheet; the prompt's thread then
    /// checks what the user types.
    fn show(self, spec: &CellSpec, init: u32, id: u64) -> Result<Prompt, String> {
        let account = Account::of(spec.host_uid).ok_or("the cell owner has no host account")?;
        let broker = Arc::new(broker(spec.host_uid)?);
        let pin = pin::Native::load(&account.home, spec.host_uid);
        let message = Message::CredentialAsk {
            app: self.app,
            title: self.title,
            description: self.description,
            pin: pin.is_some(),
            fingerprint: self.fingerprint,
        };
        broker
            .send_record(&message.encode(), &[])
            .map_err(|error| format!("the device broker did not take the prompt: {error}"))?;
        let ended = Arc::new(AtomicBool::new(false));
        let thread = std::thread::Builder::new()
            .name("credential-prompt".into())
            .spawn({
                let (broker, ended) = (Arc::clone(&broker), Arc::clone(&ended));
                move || {
                    let confirmed = check(&broker, &account, pin.as_ref());
                    if ended.load(Ordering::Acquire) {
                        return;
                    }
                    let answered = answer(init, id, confirmed);
                    if let Err(error) = &answered {
                        eprintln!("credential prompt {id} not answered: {error}");
                    }
                    if confirmed {
                        let close = Message::CredentialClose {
                            accepted: answered.is_ok(),
                        };
                        let _ = broker.send_record(&close.encode(), &[]);
                    }
                }
            })
            .map_err(|error| error.to_string())?;
        Ok(Prompt {
            broker,
            ended,
            thread,
        })
    }
}

/// The user's device broker, which shows the compositor's sheet.
fn broker(uid: u32) -> Result<SeqPacket, String> {
    let path = format!("/run/user/{uid}/droidloom/credential.sock");
    let metadata = fs::symlink_metadata(&path)
        .map_err(|error| format!("the device broker is not listening: {error}"))?;
    if !metadata.file_type().is_socket() || metadata.uid() != uid {
        return Err("the device broker's socket is not the cell owner's".into());
    }
    let broker = SeqPacket::connect_nonblocking(&path).map_err(|error| error.to_string())?;
    broker
        .set_nonblocking(false)
        .map_err(|error| error.to_string())?;
    match broker.peer_credentials() {
        Ok(peer) if peer.uid == uid => Ok(broker),
        _ => Err("the device broker is not the cell owner's".into()),
    }
}

/// Checks each credential the user types until one proves them (true) or the
/// sheet closes (false). A wrong one asks again.
fn check(broker: &SeqPacket, account: &Account, pin: Option<&pin::Native>) -> bool {
    let mut bytes = vec![0; MAX_RECORD];
    let confirmed = loop {
        let record = match broker.receive_record_into(&mut bytes) {
            Ok(record) => record,
            Err(IpcError::Io(error)) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break false,
        };
        let message = Message::decode(record.bytes);
        wipe(&mut bytes);
        let (is_pin, mut secret) = match message {
            Ok(Message::CredentialSecret { pin, secret }) => (pin, secret),
            Ok(Message::CredentialCancelled) => break false,
            _ => {
                eprintln!("credential prompt: the device broker sent another record");
                break false;
            }
        };
        let checked = match (is_pin, pin) {
            (true, Some(pin)) => pin.verify(&secret),
            (true, None) => Checked::Wrong(0),
            (false, _) => pam::authenticate(&account.name, &secret),
        };
        wipe(&mut secret);
        let retry_ms = match checked {
            Checked::Matched => break true,
            Checked::Wrong(retry_ms) => retry_ms,
            Checked::Unavailable(error) => {
                eprintln!("credential prompt: {error}");
                0
            }
        };
        let wrong = Message::CredentialWrong { retry_ms };
        if broker.send_record(&wrong.encode(), &[]).is_err() {
            break false;
        }
    };
    wipe(&mut bytes);
    confirmed
}

enum Checked {
    Matched,
    /// Ask again after this many milliseconds, or 0.
    Wrong(u32),
    Unavailable(String),
}

fn wipe(bytes: &mut [u8]) {
    for byte in bytes.iter_mut() {
        // SAFETY: a valid, aligned byte of a live buffer.
        unsafe { std::ptr::write_volatile(byte, 0) };
    }
}

/// The cell owner's account name and home.
struct Account {
    name: CString,
    home: PathBuf,
}

impl Account {
    fn of(uid: u32) -> Option<Self> {
        let mut buffer = vec![0u8; 16 * 1024];
        // SAFETY: zero is a valid passwd; getpwuid_r fills it on success.
        let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
        let mut found = std::ptr::null_mut();
        // SAFETY: all output pointers refer to live, appropriately sized storage.
        let status = unsafe {
            libc::getpwuid_r(
                uid,
                &raw mut entry,
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &raw mut found,
            )
        };
        if status != 0 || found.is_null() || entry.pw_name.is_null() || entry.pw_dir.is_null() {
            return None;
        }
        // SAFETY: getpwuid_r succeeded; its strings live in buffer until copied.
        let (name, home) = unsafe {
            (
                CStr::from_ptr(entry.pw_name).to_owned(),
                CStr::from_ptr(entry.pw_dir).to_str().ok()?.to_owned(),
            )
        };
        let home = PathBuf::from(home);
        (home.is_absolute() && home != Path::new("/")).then_some(Self { name, home })
    }
}

/// The lock PIN, as the gatekeeper provider holds it. Denial keeps the
/// provider's handle in the user's PIN record; only the provider can tell
/// whether a PIN matches it, so the user's own record cannot prove a PIN.
mod pin {
    use super::{
        Checked, File, MetadataExt, OpenOptions, OpenOptionsExt, Path, Read, UnixStream, Write, fs,
        io, peer_credentials, wipe,
    };
    use std::os::unix::fs::FileTypeExt;
    use std::time::Duration;

    /// A hardware provider, such as a Goodix phone's TEE.
    const HARDWARE: &str = "/run/gatekeeper/worker.sock";
    /// Phonewave's software provider, for phones without a hardware one.
    const SOFTWARE: &str = "/run/gatekeeper/software.sock";
    /// The providers' Gatekeeper UID for the lock PIN.
    const UID: u32 = 941_003;
    const VERIFY: u32 = 3;
    const MAX_HANDLE: usize = 4096;
    const MAX_RECORD_BYTES: u64 = 32 * 1024;
    const TOKEN: usize = 69;
    const LIMIT: Duration = Duration::from_secs(25);

    fn provider() -> Option<&'static str> {
        [HARDWARE, SOFTWARE].into_iter().find(|path| {
            fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_socket())
        })
    }

    pub(super) struct Native {
        sid: u64,
        handle: Vec<u8>,
    }

    impl Native {
        /// The user's PIN, when it is set, on and held by a provider here.
        pub(super) fn load(home: &Path, uid: u32) -> Option<Self> {
            provider()?;
            Self::load_record(home, uid)
        }

        pub(super) fn load_record(home: &Path, uid: u32) -> Option<Self> {
            let mut file: File = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
                .open(home.join(".local/state/denial/lock-pin"))
                .ok()?;
            let metadata = file.metadata().ok()?;
            if !metadata.is_file() || metadata.uid() != uid || metadata.len() > MAX_RECORD_BYTES {
                return None;
            }
            let mut text = String::new();
            Read::by_ref(&mut file)
                .take(MAX_RECORD_BYTES)
                .read_to_string(&mut text)
                .ok()?;
            let record: serde_json::Value = serde_json::from_str(&text).ok()?;
            if record["version"].as_u64() != Some(2) || record["enabled"].as_bool() == Some(false) {
                return None;
            }
            let native = &record["gatekeeper"];
            if native["uid"].as_u64() != Some(u64::from(UID)) {
                return None;
            }
            let sid = native["secure_user_id"].as_u64().filter(|sid| *sid != 0)?;
            let handle = native["handle"]
                .as_array()
                .filter(|bytes| !bytes.is_empty() && bytes.len() <= MAX_HANDLE)?
                .iter()
                .map(|byte| byte.as_u64().and_then(|byte| u8::try_from(byte).ok()))
                .collect::<Option<Vec<u8>>>()?;
            Some(Self { sid, handle })
        }

        pub(super) fn verify(&self, pin: &[u8]) -> Checked {
            if !(4..=12).contains(&pin.len()) || !pin.iter().all(u8::is_ascii_digit) {
                return Checked::Wrong(0);
            }
            match self.call(pin) {
                Ok(checked) => checked,
                Err(error) => Checked::Unavailable(format!("PIN provider: {error}")),
            }
        }

        fn call(&self, pin: &[u8]) -> io::Result<Checked> {
            let path = provider().ok_or(io::ErrorKind::NotFound)?;
            let mut stream = UnixStream::connect(path)?;
            if peer_credentials(&stream)
                .map_err(|error| io::Error::other(error.to_string()))?
                .uid
                != 0
            {
                return Err(io::Error::other("the provider is not root's"));
            }
            stream.set_read_timeout(Some(LIMIT))?;
            stream.set_write_timeout(Some(LIMIT))?;
            let mut request = vec![0u8; 48];
            request[..8].copy_from_slice(b"NXGK1\0\0\0");
            request[8..12].copy_from_slice(&VERIFY.to_le_bytes());
            request[24..32].copy_from_slice(&self.sid.to_le_bytes());
            // Both are bounded: the handle by MAX_HANDLE, the PIN by twelve digits.
            #[allow(clippy::cast_possible_truncation)]
            for (at, length) in [(32, self.handle.len()), (36, pin.len())] {
                request[at..at + 4].copy_from_slice(&(length as u32).to_le_bytes());
            }
            request.extend_from_slice(&self.handle);
            request.extend_from_slice(pin);
            let sent = stream.write_all(&request);
            wipe(&mut request);
            sent?;
            let mut reply = [0u8; 40];
            stream.read_exact(&mut reply)?;
            let u32_at = |at: usize| u32::from_le_bytes(reply[at..at + 4].try_into().unwrap());
            let u64_at = |at: usize| u64::from_le_bytes(reply[at..at + 8].try_into().unwrap());
            if &reply[..8] != b"NXGR1\0\0\0" || u32_at(20) != UID || u32_at(36) != 0 {
                return Err(io::ErrorKind::InvalidData.into());
            }
            let i32_at = |at: usize| i32::from_le_bytes(reply[at..at + 4].try_into().unwrap());
            let (status, service, retry) = (i32_at(8), i32_at(12), u32_at(16));
            if status != 0 {
                return Ok(match (status, service, retry) {
                    (status, -1, 0) if status == -libc::EACCES => Checked::Wrong(0),
                    (status, -2, ms) if status == -libc::EAGAIN && ms > 0 => Checked::Wrong(ms),
                    _ => Checked::Unavailable(format!("PIN provider status {status}")),
                });
            }
            if u64_at(24) != self.sid || u32_at(32) as usize != TOKEN {
                return Err(io::ErrorKind::InvalidData.into());
            }
            let mut token = [0u8; TOKEN];
            stream.read_exact(&mut token)?;
            let sid = u64::from_le_bytes(token[9..17].try_into().unwrap());
            wipe(&mut token);
            if sid != self.sid {
                return Err(io::ErrorKind::InvalidData.into());
            }
            Ok(Checked::Matched)
        }
    }

    impl Drop for Native {
        fn drop(&mut self) {
            wipe(&mut self.handle);
        }
    }
}

/// The account password, through PAM's `login` stack as the lock screen
/// checks it. libpam is loaded when first needed.
mod pam {
    use super::{CStr, CString, Checked, c_char, c_int, c_void, wipe};

    #[repr(C)]
    struct Message {
        style: c_int,
        text: *const c_char,
    }
    #[repr(C)]
    struct Response {
        text: *mut c_char,
        code: c_int,
    }
    #[repr(C)]
    struct Conversation {
        call: unsafe extern "C" fn(
            c_int,
            *mut *const Message,
            *mut *mut Response,
            *mut c_void,
        ) -> c_int,
        data: *mut c_void,
    }
    type Start = unsafe extern "C" fn(
        *const c_char,
        *const c_char,
        *const Conversation,
        *mut *mut c_void,
    ) -> c_int;
    type Step = unsafe extern "C" fn(*mut c_void, c_int) -> c_int;
    type SetItem = unsafe extern "C" fn(*mut c_void, c_int, *const c_void) -> c_int;

    const SUCCESS: c_int = 0;
    const BUF_ERR: c_int = 5;
    const CONV_ERR: c_int = 19;
    const AUTH_ERR: c_int = 7;
    const CRED_INSUFFICIENT: c_int = 8;
    const AUTHINFO_UNAVAIL: c_int = 9;
    const USER_UNKNOWN: c_int = 10;
    const MAXTRIES: c_int = 11;
    const PROMPT_ECHO_OFF: c_int = 1;
    const PROMPT_ECHO_ON: c_int = 2;
    const TTY: c_int = 3;
    const SILENT: c_int = 0x8000;
    const DISALLOW_NULL_AUTHTOK: c_int = 0x0001;
    const MAX_MESSAGES: usize = 32;

    struct Api {
        start: Start,
        end: Step,
        authenticate: Step,
        account: Step,
        set_item: SetItem,
    }

    fn api() -> Result<&'static Api, String> {
        static API: std::sync::OnceLock<Result<Api, String>> = std::sync::OnceLock::new();
        API.get_or_init(|| {
            // SAFETY: a fixed SONAME; the handle is kept for the process's life.
            let library = unsafe { libc::dlopen(c"libpam.so.0".as_ptr(), libc::RTLD_NOW) };
            if library.is_null() {
                return Err("libpam.so.0 is not installed".into());
            }
            let symbol = |name: &CStr| {
                // SAFETY: a live handle and a NUL-terminated name.
                let address = unsafe { libc::dlsym(library, name.as_ptr()) };
                (!address.is_null())
                    .then_some(address)
                    .ok_or_else(|| format!("libpam lacks {name:?}"))
            };
            // SAFETY: each symbol has the C signature libpam documents.
            unsafe {
                Ok(Api {
                    start: std::mem::transmute::<*mut c_void, Start>(symbol(c"pam_start")?),
                    end: std::mem::transmute::<*mut c_void, Step>(symbol(c"pam_end")?),
                    authenticate: std::mem::transmute::<*mut c_void, Step>(symbol(
                        c"pam_authenticate",
                    )?),
                    account: std::mem::transmute::<*mut c_void, Step>(symbol(c"pam_acct_mgmt")?),
                    set_item: std::mem::transmute::<*mut c_void, SetItem>(symbol(c"pam_set_item")?),
                })
            }
        })
        .as_ref()
        .map_err(Clone::clone)
    }

    /// Answers every hidden prompt with the password and nothing else.
    unsafe extern "C" fn converse(
        count: c_int,
        messages: *mut *const Message,
        responses: *mut *mut Response,
        data: *mut c_void,
    ) -> c_int {
        let Some(count) = usize::try_from(count)
            .ok()
            .filter(|count| (1..=MAX_MESSAGES).contains(count))
        else {
            return CONV_ERR;
        };
        if messages.is_null() || responses.is_null() {
            return CONV_ERR;
        }
        // SAFETY: data is the password CString authenticate passes.
        let password = unsafe { &*data.cast::<CString>() };
        // SAFETY: PAM frees the array and the strings it holds with free().
        let replies =
            unsafe { libc::calloc(count, std::mem::size_of::<Response>()) }.cast::<Response>();
        if replies.is_null() {
            return BUF_ERR;
        }
        for index in 0..count {
            // SAFETY: PAM passes `count` message pointers.
            let message = unsafe { *messages.add(index) };
            if message.is_null() {
                continue;
            }
            // SAFETY: a valid message from PAM.
            let style = unsafe { (*message).style };
            if style == PROMPT_ECHO_OFF {
                // SAFETY: a NUL-terminated copy PAM frees.
                let copy = unsafe { libc::strdup(password.as_ptr()) };
                if copy.is_null() {
                    for earlier in 0..index {
                        // SAFETY: replies holds `count` zeroed or filled entries.
                        unsafe { libc::free((*replies.add(earlier)).text.cast()) };
                    }
                    // SAFETY: allocated above.
                    unsafe { libc::free(replies.cast()) };
                    return BUF_ERR;
                }
                // SAFETY: index < count.
                unsafe { (*replies.add(index)).text = copy };
            } else if style == PROMPT_ECHO_ON {
                // Nobody types a visible answer here, such as another user name.
                for earlier in 0..index {
                    // SAFETY: as above.
                    unsafe { libc::free((*replies.add(earlier)).text.cast()) };
                }
                // SAFETY: allocated above.
                unsafe { libc::free(replies.cast()) };
                return CONV_ERR;
            }
        }
        // SAFETY: PAM owns the array from here on.
        unsafe { *responses = replies };
        SUCCESS
    }

    pub(super) fn authenticate(user: &CStr, password: &[u8]) -> Checked {
        let api = match api() {
            Ok(api) => api,
            Err(error) => return Checked::Unavailable(error),
        };
        if password.is_empty() {
            return Checked::Wrong(0);
        }
        let Ok(mut password) = CString::new(password) else {
            return Checked::Wrong(0);
        };
        let conversation = Conversation {
            call: converse,
            data: (&raw mut password).cast(),
        };
        let mut handle = std::ptr::null_mut();
        // SAFETY: the service, user and conversation outlive the balanced pam_end.
        let mut result = unsafe {
            (api.start)(
                c"login".as_ptr(),
                user.as_ptr(),
                &raw const conversation,
                &raw mut handle,
            )
        };
        let checked = if result != SUCCESS || handle.is_null() {
            Checked::Unavailable(format!("PAM did not start: {result}"))
        } else {
            // SAFETY: handle is live until the pam_end below.
            unsafe {
                (api.set_item)(handle, TTY, c"droidloom".as_ptr().cast());
                result = (api.authenticate)(handle, DISALLOW_NULL_AUTHTOK);
                if result == SUCCESS {
                    result = (api.account)(handle, SILENT);
                }
                (api.end)(handle, result);
            }
            match result {
                SUCCESS => Checked::Matched,
                AUTH_ERR | USER_UNKNOWN | MAXTRIES | CRED_INSUFFICIENT | AUTHINFO_UNAVAIL => {
                    Checked::Wrong(0)
                }
                other => Checked::Unavailable(format!("PAM failed: {other}")),
            }
        };
        let mut bytes = password.into_bytes();
        wipe(&mut bytes);
        checked
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pin_records_need_the_providers_handle() {
        let home = tempfile::tempdir().unwrap();
        let state = home.path().join(".local/state/denial");
        fs::create_dir_all(&state).unwrap();
        // SAFETY: getuid has no preconditions.
        let uid = unsafe { libc::getuid() };
        let write = |record: serde_json::Value| {
            fs::write(state.join("lock-pin"), record.to_string()).unwrap();
        };
        let native = |record: serde_json::Value| {
            write(record);
            pin::Native::load_record(home.path(), uid)
        };
        let gatekeeper = serde_json::json!({"uid": 941_003, "secure_user_id": 5, "handle": [1, 2]});
        assert!(native(serde_json::json!({"version": 1, "length": 6, "hash": "x"})).is_none());
        assert!(
            native(
                serde_json::json!({"version": 2, "length": 6, "enabled": false,
                "gatekeeper": gatekeeper})
            )
            .is_none()
        );
        assert!(
            native(serde_json::json!({"version": 2, "length": 6,
                "gatekeeper": {"uid": 1, "secure_user_id": 5, "handle": [1]}}))
            .is_none()
        );
        assert!(
            native(serde_json::json!({"version": 2, "length": 6, "gatekeeper": gatekeeper}))
                .is_some()
        );
        assert!(
            pin::Native::load_record(home.path(), uid.wrapping_add(1)).is_none(),
            "another user's record"
        );
    }
}
