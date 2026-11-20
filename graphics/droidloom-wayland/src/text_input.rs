//! Headless Android IME sessions mapped to ordinary Wayland text-input-v3.

use std::io::{self, Read, Write};
use std::os::fd::{AsFd, AsRawFd, RawFd};
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols::wp::text_input::zv3::client::{
    zwp_text_input_manager_v3::ZwpTextInputManagerV3,
    zwp_text_input_v3::{self, ZwpTextInputV3},
};

use super::keyboard_layout::Hint;
use super::{App, TaskObjectId};

const MAX_RECORD: usize = 16_384;

mod panel;

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct Editor {
    session: u64,
    #[serde(default)]
    show_request: u64,
    task: u64,
    active: bool,
    input_type: u32,
}

impl Editor {
    fn needs_enable(self, previous: Option<Self>) -> bool {
        previous.is_none_or(|old| old.task != self.task || old.session != self.session)
    }
}

#[derive(Default, Serialize)]
struct Edit {
    session: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    commit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    preedit: Option<String>,
    before: u32,
    after: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    dismiss: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    show_request: Option<u64>,
}

pub(super) struct TextInput {
    listener: UnixListener,
    path: PathBuf,
    client: Option<UnixStream>,
    incoming: Vec<u8>,
    outgoing: Vec<u8>,
    pub manager: Option<ZwpTextInputManagerV3>,
    pub resource: Option<ZwpTextInputV3>,
    pub entered: Option<TaskObjectId>,
    editor: Option<Editor>,
    enabled: Option<Editor>,
    serial: u32,
    edit: Edit,
    last_touch: Option<std::time::Instant>,
    panel_manager: Option<panel::DenialTextInputPanelManagerV1>,
    panel: Option<panel::DenialTextInputPanelV1>,
    dismissed: Option<Editor>,
    /// The host keyboard's layout, given to the input method on each
    /// connection and whenever it changes.
    keyboard_layout: Option<Hint>,
}

impl TextInput {
    pub fn remove_resource(&mut self) {
        if let Some(panel) = self.panel.take() {
            panel.destroy();
        }
        self.entered = None;
        self.update(None);
        if let Some(resource) = self.resource.take() {
            resource.destroy();
        }
        self.serial = 0;
    }

    pub fn new(path: PathBuf, manager: Option<ZwpTextInputManagerV3>) -> io::Result<Self> {
        // The parent is the same private directory checked for the main endpoint.
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_socket() => std::fs::remove_file(&path)?,
            Ok(_) => return Err(io::Error::from(io::ErrorKind::AlreadyExists)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let listener = UnixListener::bind(&path)?;
        // The host parent is 0700; the guest mounts this only in a system-owned
        // 0700 directory. The host owner may differ from Android system UID 1000.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666))?;
        listener.set_nonblocking(true)?;
        Ok(Self {
            listener,
            path,
            client: None,
            incoming: Vec::new(),
            outgoing: Vec::new(),
            manager,
            resource: None,
            entered: None,
            editor: None,
            enabled: None,
            serial: 0,
            edit: Edit::default(),
            last_touch: None,
            panel_manager: None,
            panel: None,
            dismissed: None,
            keyboard_layout: None,
        })
    }

    pub fn bind_panel_feedback(
        &mut self,
        globals: &wayland_client::globals::GlobalList,
        qh: &QueueHandle<App>,
    ) {
        self.panel_manager = globals.bind(qh, 1..=1, ()).ok();
    }

    pub fn attach_panel_feedback(&mut self, qh: &QueueHandle<App>) {
        if let (Some(manager), Some(input)) = (&self.panel_manager, &self.resource) {
            self.panel = Some(manager.get_panel(input, qh, ()));
        }
    }

    fn dismiss(&mut self, serial: u32) {
        let Some(editor) = self.enabled else { return };
        if serial != self.serial || self.editor != Some(editor) || self.dismissed == Some(editor) {
            return;
        }
        self.dismissed = Some(editor);
        self.edit = Edit::default();
        self.queue_edit(&Edit {
            session: editor.session,
            dismiss: Some(true),
            show_request: Some(editor.show_request),
            ..Edit::default()
        });
        if droidloom_timing::traced(droidloom_timing::Diagnostic::Input) {
            eprintln!(
                "Droidloom host panel dismissed session={} serial={serial}",
                editor.session
            );
        }
    }

    pub fn fds(&self) -> impl Iterator<Item = (RawFd, i16)> + '_ {
        std::iter::once((self.listener.as_fd().as_raw_fd(), libc::POLLIN)).chain(
            self.client.iter().map(|client| {
                (
                    client.as_fd().as_raw_fd(),
                    libc::POLLIN
                        | if self.outgoing.is_empty() {
                            0
                        } else {
                            libc::POLLOUT
                        },
                )
            }),
        )
    }

    pub fn note_touch(&mut self) {
        self.last_touch = Some(std::time::Instant::now());
    }

    pub fn pump(&mut self) -> io::Result<()> {
        match self.listener.accept() {
            Ok((client, _)) => {
                if !framework_peer(&client)? {
                    return Ok(());
                }
                self.disconnect();
                client.set_nonblocking(true)?;
                self.client = Some(client);
                self.queue_keyboard_layout();
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(error),
        }
        let Some(client) = self.client.as_mut() else {
            return Ok(());
        };
        let mut bytes = [0; 4096];
        // Bounded per dispatch: text input cannot starve presentation.
        match client.read(&mut bytes) {
            Ok(0) => {
                self.disconnect();
                return Ok(());
            }
            Ok(count) => self.incoming.extend_from_slice(&bytes[..count]),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(error),
        }
        if self.incoming.len() > MAX_RECORD {
            return Err(io::ErrorKind::InvalidData.into());
        }
        while let Some(end) = self.incoming.iter().position(|byte| *byte == b'\n') {
            let editor: Editor = serde_json::from_slice(&self.incoming[..end])
                .map_err(|_| io::Error::from(io::ErrorKind::InvalidData))?;
            self.incoming.drain(..=end);
            self.editor = Some(editor);
        }
        if !self.outgoing.is_empty() {
            match client.write(&self.outgoing) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(count) => {
                    self.outgoing.drain(..count);
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    pub fn disconnect(&mut self) {
        self.dismissed = None;
        self.client = None;
        self.editor = None;
        self.incoming.clear();
        self.outgoing.clear();
        self.edit = Edit::default();
        self.update(None);
    }

    pub fn update(&mut self, focused_task: Option<u64>) {
        let desired = self.editor.filter(|editor| {
            editor.active
                && editor.task != 0
                && Some(editor.task) == focused_task
                && self.entered.is_some()
        });
        if desired == self.enabled {
            return;
        }
        let previous = self.enabled;
        self.edit = Edit::default();
        self.enabled = desired;
        let Some(resource) = &self.resource else {
            self.enabled = None;
            return;
        };
        if let Some(editor) = desired {
            // Android reports both startInput and showSoftInput for one tap.
            // Re-enabling consumes the compositor's touch authorization twice
            // and dismisses the panel immediately. A show request refreshes
            // the existing editor, without resetting its Wayland lifecycle.
            if editor.needs_enable(previous) {
                resource.enable();
            }
            let (hint, purpose) = content_type(editor.input_type);
            resource.set_content_type(hint, purpose);
        } else {
            resource.disable();
        }
        resource.commit();
        self.serial = self.serial.wrapping_add(1);
        if droidloom_timing::traced(droidloom_timing::Diagnostic::Input) {
            eprintln!(
                "Droidloom text-input editor={desired:?} focused_task={focused_task:?} serial={} touch_age_ms={:?}",
                self.serial,
                self.last_touch.map(|touch| touch.elapsed().as_millis())
            );
        }
    }

    fn done(&mut self, serial: u32) {
        let mut edit = std::mem::take(&mut self.edit);
        let Some(editor) = self.enabled else { return };
        if serial != self.serial || self.editor != Some(editor) {
            return;
        }
        if edit.commit.is_none() && edit.preedit.is_none() && edit.before == 0 && edit.after == 0 {
            return;
        }
        edit.session = editor.session;
        self.queue_edit(&edit);
    }

    /// Android types the host keyboard in the layout Android names for it.
    pub fn set_keyboard_layout(&mut self, layout: Option<Hint>) {
        if layout.is_none() || layout == self.keyboard_layout {
            return;
        }
        self.keyboard_layout = layout;
        if self.client.is_some() {
            self.queue_keyboard_layout();
        }
    }

    fn queue_keyboard_layout(&mut self) {
        if let Some(layout) = self.keyboard_layout {
            self.queue(&serde_json::json!({
                "keyboard_layout": { "language": layout.language, "type": layout.kind }
            }));
        }
    }

    fn queue_edit(&mut self, edit: &Edit) {
        self.queue(edit);
    }

    fn queue(&mut self, record: &impl Serialize) {
        if let Ok(mut bytes) = serde_json::to_vec(record) {
            bytes.push(b'\n');
            if self.outgoing.len() + bytes.len() <= MAX_RECORD {
                self.outgoing.extend(bytes);
            } else {
                self.disconnect();
            }
        }
    }
}

impl Drop for TextInput {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn framework_peer(client: &UnixStream) -> io::Result<bool> {
    let mut credentials = libc::ucred {
        pid: 0,
        uid: u32::MAX,
        gid: u32::MAX,
    };
    let mut length = libc::socklen_t::try_from(std::mem::size_of::<libc::ucred>())
        .map_err(|_| io::Error::from(io::ErrorKind::InvalidData))?;
    // SAFETY: the socket and writable credential/length storage live for the call.
    let result = unsafe {
        libc::getsockopt(
            client.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            std::ptr::from_mut(&mut credentials).cast(),
            &raw mut length,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(credentials.uid == 1000)
}

fn content_type(
    input_type: u32,
) -> (
    zwp_text_input_v3::ContentHint,
    zwp_text_input_v3::ContentPurpose,
) {
    use zwp_text_input_v3::{ContentHint as H, ContentPurpose as P};
    let class = input_type & 0xf;
    let variation = input_type & 0xff0;
    if (class == 1 && matches!(variation, 0x80 | 0x90 | 0xe0)) || (class == 2 && variation == 0x10)
    {
        return (H::HiddenText | H::SensitiveData, P::Password);
    }
    let purpose = match (class, variation) {
        (2, _) => P::Number,
        (3, _) => P::Phone,
        (1, 0x10) => P::Url,
        (1, 0x20 | 0xd0) => P::Email,
        (4, 0x10) => P::Date,
        (4, 0x20) => P::Time,
        (4, _) => P::Datetime,
        _ => P::Normal,
    };
    (H::empty(), purpose)
}

impl Dispatch<ZwpTextInputV3, ()> for App {
    fn event(
        state: &mut Self,
        _: &ZwpTextInputV3,
        event: zwp_text_input_v3::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwp_text_input_v3::Event::Enter { surface } => {
                state.text_input.entered = state.task_for_surface(&surface);
                state.update_text_input();
            }
            zwp_text_input_v3::Event::Leave { .. } => {
                state.text_input.entered = None;
                state.text_input.update(None);
            }
            zwp_text_input_v3::Event::CommitString { text } => state.text_input.edit.commit = text,
            zwp_text_input_v3::Event::PreeditString { text, .. } => {
                state.text_input.edit.preedit = Some(text.unwrap_or_default());
            }
            zwp_text_input_v3::Event::DeleteSurroundingText {
                before_length,
                after_length,
            } => {
                state.text_input.edit.before = before_length;
                state.text_input.edit.after = after_length;
            }
            zwp_text_input_v3::Event::Done { serial } => state.text_input.done(serial),
            _ => {}
        }
    }
}

wayland_client::delegate_noop!(App: ignore ZwpTextInputManagerV3);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dismissal_is_session_bound_idempotent_and_does_not_submit_text() {
        let mut input = fixture();
        let editor = Editor {
            session: 8,
            show_request: 4,
            task: 143,
            active: true,
            input_type: 1,
        };
        input.editor = Some(editor);
        input.enabled = Some(editor);
        input.serial = 3;
        input.dismiss(2);
        assert!(input.outgoing.is_empty());
        input.dismiss(3);
        let message: serde_json::Value = serde_json::from_slice(&input.outgoing).unwrap();
        assert_eq!(message["dismiss"], true);
        assert_eq!(message["session"], 8);
        assert_eq!(message["show_request"], 4);
        assert!(message.get("commit").is_none());
        input.outgoing.clear();
        input.dismiss(3);
        assert!(input.outgoing.is_empty());
        input.editor = Some(Editor {
            show_request: 5,
            ..editor
        });
        input.dismiss(3);
        assert!(input.outgoing.is_empty());
        input.enabled = input.editor;
        input.serial = 4;
        input.dismiss(4);
        assert!(!input.outgoing.is_empty());
    }

    #[test]
    fn showing_an_existing_editor_does_not_reenable_it() {
        let started = Editor {
            session: 1,
            show_request: 0,
            task: 10,
            active: true,
            input_type: 1,
        };
        let shown = Editor {
            show_request: 1,
            ..started
        };
        let reshown = Editor {
            show_request: 2,
            ..started
        };
        assert!(started.needs_enable(None));
        assert_ne!(started, shown); // A fresh show still sends a state commit.
        assert!(!shown.needs_enable(Some(started)));
        assert!(!reshown.needs_enable(Some(shown)));
        assert!(
            Editor {
                session: 2,
                ..shown
            }
            .needs_enable(Some(shown))
        );
        assert!(Editor { task: 11, ..shown }.needs_enable(Some(shown)));
    }
    fn fixture() -> TextInput {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        TextInput::new(
            std::env::temp_dir().join(format!("droidloom-text-{}-{id}.sock", std::process::id())),
            None,
        )
        .unwrap()
    }

    #[test]
    fn edits_are_committed_as_one_session_bound_transaction() {
        let mut input = fixture();
        let editor = Editor {
            session: 8,
            show_request: 0,
            task: 143,
            active: true,
            input_type: 1,
        };
        input.editor = Some(editor);
        input.enabled = Some(editor);
        input.serial = 3;
        input.edit.commit = Some("caffè 🦊".into());
        input.edit.before = 2;
        input.done(3);
        let edit: serde_json::Value = serde_json::from_slice(&input.outgoing).unwrap();
        assert_eq!(edit["session"], 8);
        assert_eq!(edit["commit"], "caffè 🦊");
        assert_eq!(edit["before"], 2);
    }

    #[test]
    fn old_done_or_changed_android_editor_never_receives_text() {
        let mut input = fixture();
        let editor = Editor {
            session: 8,
            show_request: 0,
            task: 143,
            active: true,
            input_type: 1,
        };
        input.editor = Some(editor);
        input.enabled = Some(editor);
        input.serial = 3;
        input.edit.commit = Some("stale".into());
        input.done(2);
        assert!(input.outgoing.is_empty());
        input.editor = Some(Editor {
            session: 9,
            ..editor
        });
        input.edit.commit = Some("wrong editor".into());
        input.done(3);
        assert!(input.outgoing.is_empty());
        input.disconnect();
        assert!(input.enabled.is_none());
    }

    #[test]
    fn fragmented_editor_records_wait_for_the_complete_message() {
        let mut input = fixture();
        let (server, mut client) = UnixStream::pair().unwrap();
        server.set_nonblocking(true).unwrap();
        input.client = Some(server);
        client.write_all(b"{\"session\":1,\"task\":143,").unwrap();
        input.pump().unwrap();
        assert!(input.editor.is_none());
        client
            .write_all(b"\"active\":true,\"input_type\":1}\n")
            .unwrap();
        input.pump().unwrap();
        assert_eq!(input.editor.unwrap().task, 143);
        drop(client);
        input.pump().unwrap();
        assert!(input.editor.is_none());
    }

    #[test]
    fn android_editor_purposes_and_password_privacy() {
        use zwp_text_input_v3::{ContentHint as H, ContentPurpose as P};
        assert_eq!(content_type(0x21).1, P::Email);
        assert_eq!(content_type(3).1, P::Phone);
        assert_eq!(content_type(2).1, P::Number);
        for value in [0x81, 0x91, 0xe1, 0x12] {
            assert_eq!(
                content_type(value),
                (H::HiddenText | H::SensitiveData, P::Password)
            );
        }
    }
}
