//! The density each Android task is laid out at, by the output its window is
//! on. `$XDG_CONFIG_HOME/droidloom/density-v1.json` gives dots per inch per
//! unit of output scale: `dpiPerScale` for every output, and `outputs` by
//! connector name (`DP-1`) for one. A task on an output of scale 1.5 with
//! 230 per scale is laid out at 345 dpi, so its contents keep their size in
//! the output's logical units on every screen. Without the file, or without
//! a value for the output, tasks take `DEFAULT_DPI_PER_SCALE`; a value out
//! of Android's range leaves Android's display density. The file is watched,
//! so a change reaches open windows at once.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// The densities Android accepts for a task.
const DPI: std::ops::RangeInclusive<u32> = 72..=1280;
/// Wayland fractional scales are in 120ths.
const SCALE_DENOMINATOR: f64 = 120.0;
/// The density per unit of scale when nothing is chosen: 460 dpi at scale 2,
/// which draws apps the size a stock Android phone does side by side.
/// Android's own 160 per scale drew them smaller than the host's.
const DEFAULT_DPI_PER_SCALE: f64 = 230.0;

#[derive(Default, Deserialize, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
struct File {
    dpi_per_scale: Option<f64>,
    #[serde(default)]
    outputs: BTreeMap<String, f64>,
}

pub struct Densities {
    path: Option<PathBuf>,
    /// The file's folder, watched for the file being written or replaced.
    watch: Option<OwnedFd>,
    file: File,
}

impl Densities {
    /// Reads the file under the XDG configuration folder and watches it.
    pub fn load(config_home: Option<OsString>, home: Option<OsString>) -> Self {
        let root = config_home
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .or_else(|| {
                home.map(PathBuf::from)
                    .filter(|path| path.is_absolute())
                    .map(|home| home.join(".config"))
            });
        let path = root.map(|root| root.join("droidloom/density-v1.json"));
        let watch = path.as_deref().and_then(|path| {
            watch(path)
                .map_err(|error| eprintln!("Droidloom cannot watch {}: {error}", path.display()))
                .ok()
        });
        let mut densities = Self {
            path,
            watch,
            file: File::default(),
        };
        densities.reload();
        densities
    }

    /// The watch to poll for readability.
    pub fn fd(&self) -> Option<RawFd> {
        self.watch.as_ref().map(AsRawFd::as_raw_fd)
    }

    /// Drains the watch and reads the file again; true when it changed.
    pub fn changed(&mut self) -> bool {
        if let Some(watch) = &self.watch {
            let mut events = [0_u8; 4096];
            // SAFETY: the buffer is writable for its length and the watch is
            // a live nonblocking inotify descriptor.
            while unsafe { libc::read(watch.as_raw_fd(), events.as_mut_ptr().cast(), events.len()) }
                > 0
            {}
        }
        self.reload()
    }

    fn reload(&mut self) -> bool {
        let read = match self.path.as_deref().map(fs::read) {
            Some(Ok(bytes)) => serde_json::from_slice(&bytes).unwrap_or_else(|error| {
                eprintln!("Droidloom ignores its density file: {error}");
                File::default()
            }),
            Some(Err(error)) if error.kind() != io::ErrorKind::NotFound => {
                eprintln!("Droidloom cannot read its density file: {error}");
                File::default()
            }
            _ => File::default(),
        };
        let changed = read != self.file;
        self.file = read;
        changed
    }

    /// The density for a task on the output named `output`, at `scale_120`;
    /// 0 leaves Android's display density.
    pub fn dpi(&self, output: Option<&str>, scale_120: u32) -> u32 {
        dpi(&self.file, output, scale_120)
    }
}

fn dpi(file: &File, output: Option<&str>, scale_120: u32) -> u32 {
    let per_scale = output
        .and_then(|name| file.outputs.get(name).copied())
        .or(file.dpi_per_scale)
        .unwrap_or(DEFAULT_DPI_PER_SCALE);
    Some((per_scale * f64::from(scale_120) / SCALE_DENOMINATOR).round())
        .filter(|dpi| dpi.is_finite() && *dpi >= 0.0 && *dpi <= f64::from(u32::MAX))
        .map(|dpi| dpi as u32)
        .filter(|dpi| DPI.contains(dpi))
        .unwrap_or(0)
}

/// Watches the folder holding `path`, made when missing, for files written
/// or moved into it.
fn watch(path: &Path) -> io::Result<OwnedFd> {
    let folder = path
        .parent()
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
    fs::create_dir_all(folder)?;
    // SAFETY: plain syscall without pointers.
    let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: inotify_init1 returned a new descriptor owned by nothing else.
    let watch = unsafe { OwnedFd::from_raw_fd(fd) };
    let mut name = folder.as_os_str().as_bytes().to_vec();
    name.push(0);
    // SAFETY: `name` is NUL-terminated and outlives the call.
    let added = unsafe {
        libc::inotify_add_watch(
            watch.as_raw_fd(),
            name.as_ptr().cast(),
            libc::IN_CLOSE_WRITE | libc::IN_MOVED_TO | libc::IN_DELETE,
        )
    };
    if added < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(watch)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_output_of_its_own_wins_over_every_output() {
        let file: File =
            serde_json::from_str(r#"{"dpiPerScale": 230, "outputs": {"DP-1": 200}}"#).unwrap();
        assert_eq!(dpi(&file, Some("DSI-1"), 240), 460);
        assert_eq!(dpi(&file, Some("DP-1"), 180), 300);
        assert_eq!(dpi(&file, None, 120), 230);
    }

    #[test]
    fn nothing_set_takes_the_default() {
        assert_eq!(dpi(&File::default(), Some("DSI-1"), 240), 460);
        let file: File = serde_json::from_str(r#"{"outputs": {"DP-1": 200}}"#).unwrap();
        assert_eq!(dpi(&file, Some("HDMI-A-1"), 120), 230);
    }

    #[test]
    fn out_of_range_leaves_android_alone() {
        let file: File = serde_json::from_str(r#"{"outputs": {"DP-1": 20}}"#).unwrap();
        assert_eq!(dpi(&file, Some("DP-1"), 120), 0);
    }
}
