//! Android notifications on the desktop session bus. No work runs on the render loop.
use crate::activation::HostGrants;
use async_io::Async;
use futures_lite::{
    StreamExt, future,
    io::{AsyncReadExt, AsyncWriteExt},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap},
    fs, io,
    os::{
        fd::AsRawFd,
        unix::{
            fs::{FileTypeExt, PermissionsExt},
            net::{UnixListener, UnixStream},
        },
    },
    path::PathBuf,
    time::Duration,
};
use zbus::{Connection, MatchRule, Message, MessageStream, Proxy, message::Type, zvariant::Value};
const NAME: &str = "org.freedesktop.Notifications";
const PATH: &str = "/org/freedesktop/Notifications";
// Room for a 96 px large icon in hex next to the longest texts.
const MAX_FRAME: usize = 262144;
const MAX_ACTIVE: usize = 256;
type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

pub struct Bridge {
    stop: UnixStream,
    path: PathBuf,
}
impl Bridge {
    pub fn start(path: PathBuf, grants: HostGrants) -> io::Result<Self> {
        match fs::symlink_metadata(&path) {
            Ok(m) if m.file_type().is_socket() => fs::remove_file(&path)?,
            Ok(_) => return Err(io::Error::other("notification endpoint is not a socket")),
            Err(e) if e.kind() == io::ErrorKind::NotFound => (),
            Err(e) => return Err(e),
        }
        let listener = Async::new(UnixListener::bind(&path)?)?;
        // Private host parent and system-only guest mount protect this path.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o666))?;
        let (stop, stopped) = UnixStream::pair()?;
        let stopped = Async::new(stopped)?;
        let state_path = path.with_extension("state");
        std::thread::Builder::new()
            .name("droidloom-notifications".into())
            .spawn(move || {
                droidloom_cpu_placement::current(droidloom_cpu_placement::Role::Background);
                future::block_on(async move {
                    loop {
                        let result = future::race(run(&listener, &state_path, &grants), async {
                            let mut b = [0];
                            let _ = (&stopped).read(&mut b).await;
                            Ok(())
                        })
                        .await;
                        match result {
                            Ok(()) => break,
                            Err(_) => eprintln!(
                                "Droidloom notifications: session bus disconnected; retrying"
                            ),
                        }
                        let stop = future::race(
                            async {
                                async_io::Timer::after(Duration::from_secs(2)).await;
                                false
                            },
                            async {
                                let mut b = [0];
                                let _ = (&stopped).read(&mut b).await;
                                true
                            },
                        )
                        .await;
                        if stop {
                            break;
                        }
                    }
                });
            })?;
        eprintln!("DROIDLOOM_NOTIFICATIONS_ABI=3;");
        Ok(Self { stop, path })
    }
}
impl Drop for Bridge {
    fn drop(&mut self) {
        let _ = self.stop.shutdown(std::net::Shutdown::Both);
        let _ = fs::remove_file(&self.path);
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Action {
    key: String,
    label: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Note {
    key: String,
    version: String,
    package: String,
    app: String,
    title: String,
    body: String,
    actions: Vec<Action>,
    urgency: u8,
    ongoing: bool,
    clearable: bool,
    /// The 48 px app icon as RGBA hex, only when Android names no launcher
    /// activity; otherwise the desktop entry carries the icon.
    icon: String,
    /// Android's heads-up decision for this post.
    alert: bool,
    /// Android's group key; empty when the notification is not grouped.
    group: String,
    summary: bool,
    /// Milliseconds since the epoch.
    when: i64,
    /// Percent, or -1 without a determinate progress bar.
    progress: i32,
    category: String,
    user: u32,
    /// The launcher activity, `package/class`, or empty.
    component: String,
    /// The 96 px large icon (a contact photo, say) as RGBA hex, or empty.
    large_icon: String,
    /// The last MessagingStyle messages, oldest first.
    messages: Vec<ChatMessage>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ChatMessage {
    sender: String,
    text: String,
}
fn rgba_hex(hex: &str, size: usize) -> bool {
    hex.is_empty() || (hex.len() == size * size * 8 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
}
impl Note {
    fn valid(&self) -> bool {
        !self.key.is_empty()
            && self.key.len() <= 2048
            && !self.version.is_empty()
            && self.version.len() <= 96
            && self.package.len() <= 256
            && self
                .package
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_')
            && self.app.len() <= 1024
            && self.title.len() <= 8192
            && self.body.len() <= 32768
            && [&self.key, &self.version, &self.app, &self.title, &self.body]
                .iter()
                .all(|s| !s.contains('\0'))
            && self.urgency <= 2
            && rgba_hex(&self.icon, 48)
            && rgba_hex(&self.large_icon, 96)
            && self.group.len() <= 512
            && self.category.len() <= 64
            && (-1..=100).contains(&self.progress)
            && (self.component.is_empty()
                || (self.component.len() <= 511
                    && self
                        .component
                        .strip_prefix(&self.package)
                        .is_some_and(|class| class.len() > 1 && class.starts_with('/'))))
            && [&self.group, &self.category, &self.component]
                .iter()
                .all(|s| !s.contains('\0'))
            && self.messages.len() <= 5
            && self.messages.iter().all(|m| {
                m.sender.len() <= 1024
                    && m.text.len() <= 4096
                    && !m.sender.contains('\0')
                    && !m.text.contains('\0')
            })
            && self.actions.len() <= 17
            && self.actions.iter().all(|a| {
                (a.key == "default"
                    || a.key
                        .strip_prefix("action-")
                        .is_some_and(|n| n.parse::<u8>().is_ok_and(|i| i < 16)))
                    && a.label.len() <= 1024
                    && !a.label.contains('\0')
            })
    }
}
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum Frame {
    Hello { abi: u32 },
    Begin,
    Post { notification: Note },
    Remove { key: String, version: String },
    End,
}
struct Peer {
    socket: Async<UnixStream>,
    buffer: Vec<u8>,
    hello: bool,
    snapshot: Option<BTreeMap<String, Note>>,
    known: BTreeMap<String, String>,
    synced: bool,
}
enum Update {
    Snapshot(BTreeMap<String, Note>),
    Post(Note),
    Remove { key: String, version: String },
}
impl Peer {
    fn ingest(&mut self, bytes: &[u8]) -> Result<Vec<Update>> {
        self.buffer.extend_from_slice(bytes);
        let mut complete = vec![];
        loop {
            if self.buffer.len() < 4 {
                break;
            }
            let length = u32::from_be_bytes(self.buffer[..4].try_into()?) as usize;
            if length == 0 || length > MAX_FRAME {
                return Err("invalid notification frame".into());
            }
            if self.buffer.len() < length + 4 {
                break;
            }
            let frame: Frame = serde_json::from_slice(&self.buffer[4..length + 4])?;
            self.buffer.drain(..length + 4);
            match frame {
                Frame::Hello { abi: 3 } if !self.hello => self.hello = true,
                Frame::Begin if self.hello && self.snapshot.is_none() => {
                    self.snapshot = Some(BTreeMap::new())
                }
                Frame::Post { notification: n } if self.hello && n.valid() => {
                    if let Some(snapshot) = self.snapshot.as_mut() {
                        if snapshot.len() >= MAX_ACTIVE
                            || snapshot.insert(n.key.clone(), n).is_some()
                        {
                            return Err("invalid notification snapshot".into());
                        }
                    } else {
                        if !self.synced
                            || (!self.known.contains_key(&n.key) && self.known.len() >= MAX_ACTIVE)
                        {
                            return Err(
                                "notification delta before snapshot or over capacity".into()
                            );
                        }
                        self.known.insert(n.key.clone(), n.version.clone());
                        complete.push(Update::Post(n));
                    }
                }
                Frame::End if self.hello => {
                    let snapshot = self.snapshot.take().ok_or("end outside snapshot")?;
                    self.known = snapshot
                        .iter()
                        .map(|(key, note)| (key.clone(), note.version.clone()))
                        .collect();
                    self.synced = true;
                    complete.push(Update::Snapshot(snapshot));
                }
                Frame::Remove { key, version }
                    if self.hello
                        && self.synced
                        && self.snapshot.is_none()
                        && !key.is_empty()
                        && key.len() <= 2048
                        && !version.is_empty()
                        && version.len() <= 96 =>
                {
                    if self.known.get(&key) == Some(&version) {
                        self.known.remove(&key);
                        complete.push(Update::Remove { key, version });
                    }
                }
                _ => return Err("invalid notification sequence".into()),
            }
        }
        Ok(complete)
    }
    async fn command(&self, note: &Note, action: Option<&str>) -> Result<()> {
        let mut command = serde_json::json!({"type":if action.is_some(){"action"}else{"dismiss"},"key":note.key,"version":note.version});
        if let Some(a) = action {
            command["action"] = a.into();
        }
        self.send(command).await
    }
    async fn send(&self, command: serde_json::Value) -> Result<()> {
        let body = serde_json::to_vec(&command)?;
        let mut data = (body.len() as u32).to_be_bytes().to_vec();
        data.extend(body);
        future::race(
            async { (&self.socket).write_all(&data).await.map_err(Into::into) },
            async {
                async_io::Timer::after(Duration::from_secs(3)).await;
                Err("notification command timeout".into())
            },
        )
        .await
    }
}
#[derive(Default, Serialize, Deserialize)]
struct Record {
    id: Option<u32>,
    version: String,
    hidden: bool,
    #[serde(skip)]
    note: Option<Note>,
}
#[derive(Default, Serialize, Deserialize)]
struct State {
    #[serde(default)]
    bus_id: String,
    owner: String,
    records: BTreeMap<String, Record>,
}
impl State {
    fn save(&self, path: &PathBuf) -> io::Result<()> {
        let tmp = path.with_extension("new");
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(&serde_json::to_vec(self)?)?;
        fs::rename(tmp, path)
    }
    fn owner(&mut self, owner: String) {
        if self.owner != owner {
            self.owner = owner;
            for record in self.records.values_mut() {
                record.id = None;
                record.hidden = false;
            }
        }
    }
}
struct Desktop<'a> {
    proxy: Proxy<'a>,
    capabilities: Vec<String>,
}
impl Desktop<'_> {
    /// `quiet` marks a replay of what Android already showed: Denial adds or
    /// updates the card without a banner. Other desktops ignore the `x-denial`
    /// hints.
    async fn notify(&self, n: &Note, replaces: u32, quiet: bool) -> zbus::Result<u32> {
        let actions: Vec<&str> = if self.capabilities.iter().any(|c| c == "actions") {
            n.actions
                .iter()
                .flat_map(|a| [a.key.as_str(), a.label.as_str()])
                .collect()
        } else {
            vec![]
        };
        let (icon, hints) = notify_hints(n, quiet);
        let body = if self.capabilities.iter().any(|c| c == "body-markup") {
            escape_markup(&n.body)
        } else {
            n.body.clone()
        };
        self.proxy
            .call(
                "Notify",
                &(
                    &n.app,
                    replaces,
                    icon,
                    if n.title.is_empty() { &n.app } else { &n.title },
                    &body,
                    actions,
                    hints,
                    // Android's ongoing flag governs cancellation, not popup lifetime.
                    // Let the desktop choose its normal timeout; expiry leaves Android active.
                    -1i32,
                ),
            )
            .await
    }
    async fn close(&self, id: u32) {
        let _: zbus::Result<()> = self.proxy.call("CloseNotification", &(id,)).await;
    }
}
/// The Notify icon name and hints for one Android notification. The desktop
/// entry names the icon when Android named a launcher activity.
fn notify_hints(n: &Note, quiet: bool) -> (&'static str, HashMap<&'static str, Value<'_>>) {
    let mut hints: HashMap<&'static str, Value<'_>> = HashMap::new();
    hints.insert("urgency", Value::from(n.urgency));
    hints.insert("resident", Value::from(n.ongoing));
    hints.insert("x-denial-clearable", Value::from(n.clearable));
    // Android retains sound policy, including channel preferences and DND.
    hints.insert("suppress-sound", Value::from(true));
    hints.insert("x-denial-alert", Value::from(n.alert && !quiet));
    let desktop_entry = (!n.component.is_empty()).then(|| {
        format!(
            "droidloom-{}",
            droidloom_contracts::applications::application_id(n.user, &n.component)
        )
    });
    if let Some(entry) = &desktop_entry {
        hints.insert("desktop-entry", Value::from(entry.clone()));
    }
    // Content artwork when Android has it, else the app icon itself.
    let (size, pixels) = if n.large_icon.is_empty() {
        (48i32, &n.icon)
    } else {
        (96, &n.large_icon)
    };
    if let Ok(pixels) = hex::decode(pixels)
        && !pixels.is_empty()
    {
        hints.insert(
            "image-data",
            Value::from((size, size, size * 4, true, 8i32, 4i32, pixels)),
        );
    }
    if n.progress >= 0 {
        hints.insert("value", Value::from(n.progress));
    }
    if !n.category.is_empty() {
        hints.insert("category", Value::from(n.category.as_str()));
    }
    if !n.group.is_empty() {
        hints.insert("x-denial-group", Value::from(n.group.as_str()));
    }
    if n.summary {
        hints.insert("x-denial-group-summary", Value::from(true));
    }
    hints.insert("x-denial-time", Value::from(n.when));
    if !n.messages.is_empty() {
        let messages: Vec<(&str, &str)> = n
            .messages
            .iter()
            .map(|m| (m.sender.as_str(), m.text.as_str()))
            .collect();
        hints.insert("x-denial-messages", Value::from(messages));
    }
    let icon = if desktop_entry.is_some() {
        ""
    } else {
        "application-x-executable"
    };
    (icon, hints)
}
fn escape_markup(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
async fn reconcile(
    state: &mut State,
    snapshot: BTreeMap<String, Note>,
    desktop: &Desktop<'_>,
) -> Result<()> {
    let removed: Vec<_> = state
        .records
        .keys()
        .filter(|k| !snapshot.contains_key(*k))
        .cloned()
        .collect();
    for key in removed {
        if let Some(record) = state.records.remove(&key) {
            if let Some(id) = record.id {
                desktop.close(id).await;
            }
        }
    }
    for (key, note) in snapshot {
        upsert(state, key, note, desktop, true).await?;
    }
    Ok(())
}
async fn upsert(
    state: &mut State,
    key: String,
    note: Note,
    desktop: &Desktop<'_>,
    quiet: bool,
) -> Result<()> {
    let record = state.records.entry(key).or_default();
    if record.version != note.version {
        record.hidden = false;
    }
    let changed = record.version != note.version;
    record.version = note.version.clone();
    record.note = Some(note.clone());
    if !state.owner.is_empty() && !record.hidden && (changed || record.id.is_none()) {
        record.id = Some(desktop.notify(&note, record.id.unwrap_or(0), quiet).await?);
    }
    Ok(())
}
enum Event {
    Accept(io::Result<(Async<UnixStream>, std::os::unix::net::SocketAddr)>),
    Data(io::Result<Vec<u8>>),
    Signal(Option<zbus::Result<Message>>),
    Owner(Option<zbus::Result<Message>>),
}
async fn read_peer(peer: Option<&Peer>) -> Event {
    let Some(peer) = peer else {
        return future::pending().await;
    };
    let mut bytes = vec![0; 8192];
    Event::Data(match (&peer.socket).read(&mut bytes).await {
        Ok(n) => {
            bytes.truncate(n);
            Ok(bytes)
        }
        Err(e) => Err(e),
    })
}
async fn run(
    listener: &Async<UnixListener>,
    state_path: &PathBuf,
    grants: &HostGrants,
) -> Result<()> {
    let conn = zbus::connection::Builder::session()?
        .method_timeout(Duration::from_secs(3))
        .build()
        .await?;
    serve(listener, state_path, conn, 1000, grants).await
}
async fn serve(
    listener: &Async<UnixListener>,
    state_path: &PathBuf,
    conn: Connection,
    peer_uid: u32,
    grants: &HostGrants,
) -> Result<()> {
    let rule = MatchRule::builder()
        .msg_type(Type::Signal)
        .interface(NAME)?
        .path(PATH)?
        .build();
    let mut signals = MessageStream::for_match_rule(rule, &conn, Some(128)).await?;
    let rule = MatchRule::builder()
        .msg_type(Type::Signal)
        .sender("org.freedesktop.DBus")?
        .interface("org.freedesktop.DBus")?
        .member("NameOwnerChanged")?
        .add_arg(NAME)?
        .build();
    let mut owners = MessageStream::for_match_rule(rule, &conn, Some(16)).await?;
    let bus = zbus::fdo::DBusProxy::new(&conn).await?;
    let proxy = Proxy::new(&conn, NAME, PATH, NAME).await?;
    let capabilities = proxy.call("GetCapabilities", &()).await.unwrap_or_default();
    let owner = bus
        .get_name_owner(NAME.try_into()?)
        .await
        .map(|s| s.to_string())
        .unwrap_or_default();
    let mut desktop = Desktop {
        proxy,
        capabilities,
    };
    let mut state: State = fs::read(state_path)
        .ok()
        .filter(|b| b.len() < 1024 * 1024)
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    if state.records.len() > MAX_ACTIVE {
        state = State::default();
    }
    let bus_id = bus.get_id().await?.to_string();
    if state.bus_id != bus_id {
        state.owner(String::new());
        state.bus_id = bus_id;
    }
    state.owner(owner);
    let mut peer: Option<Peer> = None;
    // Denial emits ActivationToken right before ActionInvoked for one tap.
    let mut activation: Option<(u32, String)> = None;
    loop {
        let event = future::race(
            future::race(
                async { Event::Accept(listener.accept().await) },
                read_peer(peer.as_ref()),
            ),
            future::race(async { Event::Signal(signals.next().await) }, async {
                Event::Owner(owners.next().await)
            }),
        )
        .await;
        match event {
            Event::Accept(Ok((socket, _))) => {
                if !authenticated_peer(socket.get_ref(), peer_uid)? {
                    continue;
                }
                peer = Some(Peer {
                    socket,
                    buffer: vec![],
                    hello: false,
                    snapshot: None,
                    known: BTreeMap::new(),
                    synced: false,
                });
                eprintln!("Droidloom notifications: Android connected");
            }
            Event::Accept(Err(e)) => return Err(e.into()),
            Event::Data(Ok(bytes)) if !bytes.is_empty() => {
                let had_hello = peer.as_ref().unwrap().hello;
                match peer.as_mut().unwrap().ingest(&bytes) {
                    Ok(snapshots) => {
                        if !had_hello && peer.as_ref().unwrap().hello {
                            if peer
                                .as_ref()
                                .unwrap()
                                .send(serde_json::json!({"type":"hello","abi":3}))
                                .await
                                .is_err()
                            {
                                peer = None;
                                continue;
                            }
                        }
                        let changed = !snapshots.is_empty();
                        for update in snapshots {
                            match update {
                                Update::Snapshot(snapshot) => {
                                    reconcile(&mut state, snapshot, &desktop).await?
                                }
                                Update::Post(note) => {
                                    upsert(&mut state, note.key.clone(), note, &desktop, false)
                                        .await?
                                }
                                Update::Remove { key, version } => {
                                    if state
                                        .records
                                        .get(&key)
                                        .is_some_and(|record| record.version == version)
                                    {
                                        if let Some(record) = state.records.remove(&key) {
                                            if let Some(id) = record.id {
                                                desktop.close(id).await;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        if changed {
                            state.save(state_path)?;
                        }
                    }
                    Err(error) => {
                        eprintln!(
                            "Droidloom notifications: invalid Android frame ({error}); disconnected"
                        );
                        peer = None;
                    }
                }
            }
            Event::Data(_) => peer = None,
            Event::Owner(Some(Ok(message))) => {
                let (_, _, owner): (String, String, String) = message.body().deserialize()?;
                if owner == state.owner {
                    continue;
                }
                state.owner(owner);
                desktop.capabilities = if state.owner.is_empty() {
                    vec![]
                } else {
                    desktop
                        .proxy
                        .call("GetCapabilities", &())
                        .await
                        .unwrap_or_default()
                };
                let snapshot = state
                    .records
                    .iter()
                    .filter_map(|(k, r)| r.note.clone().map(|n| (k.clone(), n)))
                    .collect();
                reconcile(&mut state, snapshot, &desktop).await?;
                state.save(state_path)?;
            }
            Event::Signal(Some(Ok(message))) => {
                if message.header().sender().map(|s| s.as_str()) != Some(state.owner.as_str()) {
                    continue;
                }
                match message.header().member().map(|s| s.as_str()) {
                    Some("ActivationToken") => {
                        let (id, token): (u32, String) = message.body().deserialize()?;
                        if !token.is_empty() && token.len() <= 1024 {
                            activation = Some((id, token));
                        }
                    }
                    Some("ActionInvoked") => {
                        let (id, action): (u32, String) = message.body().deserialize()?;
                        let token = activation
                            .take()
                            .filter(|(token_id, _)| *token_id == id)
                            .map(|(_, token)| token);
                        if let Some(note) = state
                            .records
                            .values()
                            .find(|r| r.id == Some(id))
                            .and_then(|r| r.note.as_ref())
                        {
                            if note.actions.iter().any(|a| a.key == action) {
                                if let Some(token) = token {
                                    grants.grant(&note.package, token);
                                }
                                if let Some(p) = peer.as_ref() {
                                    if p.command(note, Some(&action)).await.is_err() {
                                        peer = None;
                                    }
                                }
                            }
                        }
                    }
                    Some("NotificationClosed") => {
                        let (id, reason): (u32, u32) = message.body().deserialize()?;
                        if let Some(record) = state.records.values_mut().find(|r| r.id == Some(id))
                        {
                            record.id = None;
                            record.hidden = true;
                            if reason == 2 {
                                if let (Some(note), Some(p)) = (&record.note, &peer) {
                                    if note.clearable && p.command(note, None).await.is_err() {
                                        peer = None;
                                    }
                                }
                            }
                        }
                        state.save(state_path)?;
                    }
                    _ => (),
                }
            }
            Event::Owner(_) | Event::Signal(_) => return Err("session bus closed".into()),
        }
    }
}
fn authenticated_peer(s: &UnixStream, expected_uid: u32) -> io::Result<bool> {
    let mut cred = libc::ucred {
        pid: 0,
        uid: !0,
        gid: !0,
    };
    let mut length = std::mem::size_of_val(&cred) as libc::socklen_t;
    // SAFETY: live socket and writable ucred buffer of the supplied size.
    if unsafe {
        libc::getsockopt(
            s.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut cred).cast(),
            &raw mut length,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(cred.uid == expected_uid)
}

#[cfg(test)]
mod tests;
