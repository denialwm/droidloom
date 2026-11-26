//! Android's screen brightness on the host's backlight.
//!
//! Droidloom's SystemUI sends a level when an app changes Android's
//! brightness setting, and while an app's window asks for its own brightness,
//! as video players do. The broker writes the backlight the way the desktop
//! does: straight into sysfs when the user may, otherwise through logind's
//! `SetBrightness` on the user's display session. When no window asks any
//! more, the backlight goes back to the level before the first window's,
//! unless the user changed it meanwhile.
//!
//! The kernel wakes a poll on `actual_brightness` after every write, by
//! anyone. A change the broker did not write goes to Android, so the apps see
//! the level the host shows. Without a backlight Android hears there is none
//! and its changes stay inside it.

use std::fs;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use droidloom_denial_ipc::{IpcError, SeqPacket};
use droidloom_device_protocol::{MAX_RECORD, Message};
use zbus::zvariant::OwnedObjectPath;

use crate::Bus;

const BACKLIGHTS: &str = "/sys/class/backlight";
const LOGIND: &str = "org.freedesktop.login1";

/// The backlight Denial drives too: a panel's own before a platform or
/// firmware one, then by name.
fn kind_priority(kind: &str) -> u8 {
    match kind {
        "raw" => 3,
        "platform" => 2,
        "firmware" => 1,
        _ => 0,
    }
}

struct Backlight {
    name: String,
    folder: PathBuf,
    max: u32,
}

impl Backlight {
    fn find(root: &Path) -> Option<Self> {
        let mut found: Vec<(u8, String, PathBuf, u32)> = fs::read_dir(root)
            .ok()?
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let folder = entry.path();
                let max = read_u32(&folder.join("max_brightness"))?;
                let kind = fs::read_to_string(folder.join("type")).unwrap_or_default();
                let name = entry.file_name().into_string().ok()?;
                (max > 0).then(|| (kind_priority(kind.trim()), name, folder, max))
            })
            .collect();
        found.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
        let (_, name, folder, max) = found.into_iter().next()?;
        Some(Self { name, folder, max })
    }

    /// The level asked for last, by anyone.
    fn raw(&self) -> Option<u32> {
        read_u32(&self.folder.join("brightness"))
    }

    fn raw_for(&self, level: f32) -> u32 {
        // A level is 0 to 1 and max a u32: the product fits.
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss
        )]
        let raw = (f64::from(level) * f64::from(self.max)).round() as u32;
        // Never off: a dark panel is not a brightness.
        raw.clamp(1, self.max)
    }

    fn level_of(&self, raw: u32) -> f32 {
        // Backlight codes are far below f32's exact integers in practice.
        #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
        let level = (f64::from(raw) / f64::from(self.max)) as f32;
        level.clamp(0.0, 1.0)
    }

    fn write(&self, raw: u32, bus: &Bus) -> Result<(), String> {
        let direct = fs::write(self.folder.join("brightness"), raw.to_string());
        if direct.is_ok() {
            return Ok(());
        }
        bus.call(|bus| {
            let user: OwnedObjectPath = bus
                .call_method(
                    Some(LOGIND),
                    "/org/freedesktop/login1/user/self",
                    Some("org.freedesktop.DBus.Properties"),
                    "Get",
                    &("org.freedesktop.login1.User", "Display"),
                )?
                .body()
                .deserialize::<zbus::zvariant::OwnedValue>()
                .and_then(|value| {
                    <(String, OwnedObjectPath)>::try_from(value).map_err(zbus::Error::from)
                })
                .map(|(_, path)| path)?;
            bus.call_method(
                Some(LOGIND),
                user.as_str(),
                Some("org.freedesktop.login1.Session"),
                "SetBrightness",
                &("backlight", self.name.as_str(), raw),
            )
            .map(|_| ())
        })
        .map_err(|error| format!("logind SetBrightness on {} failed: {error}", self.name))
    }
}

fn read_u32(path: &Path) -> Option<u32> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// What the broker knows of the backlight while Android is connected.
#[derive(Debug, Default)]
struct Levels {
    /// The code the broker wrote last.
    written: Option<u32>,
    /// The code before the first window asked, while windows ask.
    before_window: Option<u32>,
    /// The code Android heard last.
    told: Option<u32>,
}

impl Levels {
    /// The backlight reads `raw`: Android hears it unless it came from
    /// Android. A change by anyone else is the user's, and stays.
    fn observed(&mut self, raw: u32) -> bool {
        if Some(raw) == self.written || Some(raw) == self.told {
            return false;
        }
        self.written = None;
        self.before_window = None;
        self.told = Some(raw);
        true
    }

    /// The code to write for Android's request, if the backlight should change.
    fn set(&mut self, current: u32, raw: u32, window: bool) -> Option<u32> {
        if window {
            self.before_window.get_or_insert(current);
        } else {
            self.before_window = None;
        }
        self.wrote(current, raw)
    }

    /// The code to go back to when no window asks any more.
    fn release(&mut self, current: u32) -> Option<u32> {
        let before = self.before_window.take()?;
        // The user moved it meanwhile: theirs stays.
        if self.written.is_some_and(|written| written != current) {
            return None;
        }
        self.wrote(current, before)
    }

    fn wrote(&mut self, current: u32, raw: u32) -> Option<u32> {
        self.written = Some(raw);
        self.told = Some(raw);
        (raw != current).then_some(raw)
    }
}

fn send(socket: &SeqPacket, message: &Message) -> io::Result<()> {
    socket
        .send_record(&message.encode(), &[])
        .map_err(io::Error::other)
}

/// Polls `actual_brightness` until `stop` is readable, telling Android each
/// change it did not ask for.
fn watch(
    backlight: &Backlight,
    levels: &Mutex<Levels>,
    socket: &SeqPacket,
    stop: &OwnedFd,
) -> io::Result<()> {
    let actual = fs::File::open(backlight.folder.join("actual_brightness"))?;
    loop {
        // sysfs re-arms the poll on each read from the start.
        let mut bytes = [0u8; 16];
        // SAFETY: the buffer is writable for its length; the descriptor is open.
        unsafe {
            libc::pread(
                actual.as_raw_fd(),
                bytes.as_mut_ptr().cast(),
                bytes.len(),
                0,
            )
        };
        if let Some(raw) = backlight.raw() {
            let tell = levels
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .observed(raw);
            if tell {
                send(
                    socket,
                    &Message::BrightnessState {
                        level: backlight.level_of(raw),
                    },
                )?;
            }
        }
        let mut fds = [
            libc::pollfd {
                fd: actual.as_raw_fd(),
                events: libc::POLLPRI | libc::POLLERR,
                revents: 0,
            },
            libc::pollfd {
                fd: stop.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: two valid pollfds; no timeout, a change or the stop wakes it.
        if unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) } < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if fds[1].revents != 0 {
            return Ok(());
        }
    }
}

/// Serves one Android connection.
pub fn serve(socket: &Arc<SeqPacket>, bus: &Bus) -> io::Result<()> {
    let Some(backlight) = Backlight::find(Path::new(BACKLIGHTS)).map(Arc::new) else {
        send(socket, &Message::BrightnessUnavailable)?;
        return drain(socket);
    };
    let levels = Arc::new(Mutex::new(Levels::default()));
    // SAFETY: eventfd takes no pointers; the result is checked.
    let raw = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: eventfd returned a new descriptor that nothing else owns.
    let stop = Arc::new(unsafe { OwnedFd::from_raw_fd(raw) });
    let watcher = {
        let (backlight, levels, socket, stop) = (
            Arc::clone(&backlight),
            Arc::clone(&levels),
            Arc::clone(socket),
            Arc::clone(&stop),
        );
        std::thread::Builder::new()
            .name("brightness".into())
            .spawn(move || watch(&backlight, &levels, &socket, &stop))?
    };
    let served = requests(socket, &backlight, &levels, bus);
    // SystemUI went while a window held the backlight: the host gets its own back.
    let restore = backlight.raw().and_then(|current| {
        levels
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .release(current)
    });
    if let Some(raw) = restore {
        if let Err(error) = backlight.write(raw, bus) {
            eprintln!("droidloom-devices: {error}");
        }
    }
    let one = 1u64.to_ne_bytes();
    // SAFETY: eight readable bytes to an open eventfd.
    unsafe { libc::write(stop.as_raw_fd(), one.as_ptr().cast(), one.len()) };
    let watched = watcher
        .join()
        .map_err(|_| io::Error::other("brightness watcher panicked"))?;
    served.and(watched)
}

fn requests(
    socket: &SeqPacket,
    backlight: &Backlight,
    levels: &Mutex<Levels>,
    bus: &Bus,
) -> io::Result<()> {
    let mut bytes = vec![0; MAX_RECORD];
    loop {
        let record = match socket.receive_record_into(&mut bytes) {
            Ok(record) => record,
            Err(IpcError::PeerClosed) => return Ok(()),
            Err(IpcError::Io(error)) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(io::Error::other(error)),
        };
        let message = Message::decode(record.bytes).map_err(io::Error::other)?;
        let Some(current) = backlight.raw() else {
            continue;
        };
        let target = {
            let mut levels = levels.lock().unwrap_or_else(PoisonError::into_inner);
            match message {
                Message::BrightnessSet { level, window } => {
                    levels.set(current, backlight.raw_for(level), window)
                }
                Message::BrightnessRelease => levels.release(current),
                _ => return Err(io::Error::other("record for another endpoint")),
            }
        };
        if let Some(raw) = target {
            if let Err(error) = backlight.write(raw, bus) {
                eprintln!("droidloom-devices: {error}");
            }
        }
    }
}

/// Reads until Android closes, for a host without a backlight.
fn drain(socket: &SeqPacket) -> io::Result<()> {
    let mut bytes = vec![0; MAX_RECORD];
    loop {
        match socket.receive_record_into(&mut bytes) {
            Ok(_) => {}
            Err(IpcError::PeerClosed) => return Ok(()),
            Err(IpcError::Io(error)) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(io::Error::other(error)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_window_level_goes_back_when_released() {
        let mut levels = Levels::default();
        assert_eq!(levels.set(400, 8190, true), Some(8190));
        // A second window's level keeps the first one's starting point.
        assert_eq!(levels.set(8190, 6000, true), Some(6000));
        assert!(!levels.observed(6000));
        assert_eq!(levels.release(6000), Some(400));
        assert!(!levels.observed(400));
        assert_eq!(levels.release(400), None);
    }

    #[test]
    fn the_users_change_during_a_window_stays() {
        let mut levels = Levels::default();
        assert_eq!(levels.set(400, 8190, true), Some(8190));
        assert!(levels.observed(3000));
        assert_eq!(levels.release(3000), None);
    }

    #[test]
    fn a_setting_change_ends_the_window_starting_point() {
        let mut levels = Levels::default();
        levels.set(400, 8190, true);
        assert_eq!(levels.set(8190, 2000, false), Some(2000));
        assert_eq!(levels.release(2000), None);
    }

    #[test]
    fn the_host_level_is_told_once() {
        let mut levels = Levels::default();
        assert!(levels.observed(400));
        assert!(!levels.observed(400));
        assert_eq!(levels.set(400, 400, false), None);
        assert!(!levels.observed(400));
    }

    #[test]
    fn levels_map_to_codes_and_never_off() {
        let backlight = Backlight {
            name: "panel".into(),
            folder: PathBuf::new(),
            max: 8190,
        };
        assert_eq!(backlight.raw_for(0.0), 1);
        assert_eq!(backlight.raw_for(1.0), 8190);
        assert_eq!(backlight.raw_for(0.5), 4095);
        assert!((backlight.level_of(4095) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn the_panels_own_backlight_wins() {
        let root = std::env::temp_dir().join(format!("droidloom-backlight-{}", std::process::id()));
        for (name, kind) in [("acpi_video0", "firmware"), ("amdgpu_bl1", "raw")] {
            let folder = root.join(name);
            fs::create_dir_all(&folder).unwrap();
            fs::write(folder.join("max_brightness"), "255\n").unwrap();
            fs::write(folder.join("type"), format!("{kind}\n")).unwrap();
        }
        assert_eq!(Backlight::find(&root).unwrap().name, "amdgpu_bl1");
        fs::remove_dir_all(&root).unwrap();
    }
}
