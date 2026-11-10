//! Whole Android storage as one file: a zstd-compressed tar of `data_dir`
//! whose first member lists every entry. A restore unpacks beside the live
//! storage, checks each entry against that list and only then swaps it in;
//! the replaced storage stays on disk.

use std::collections::BTreeMap;
use std::ffi::CString;
use std::fs;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::host_storage;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

const MANIFEST: &str = "droidloom-backup.json";
const FORMAT: u32 = 1;
const ACTIVE_RELEASE: &str = "/usr/lib/droidloom/active";
const RESTORE_PREFIX: &str = ".restore-";
const TAR_METADATA: [&str; 5] = [
    "--format=posix",
    "--numeric-owner",
    "--acls",
    "--xattrs",
    "--xattrs-include=*",
];

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    format: u32,
    /// Droidloom release that last ran on this storage.
    release: Option<String>,
    entries: Vec<Entry>,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Kind {
    Directory,
    File,
    Symlink,
    Fifo,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    path: String,
    kind: Kind,
    mode: u32,
    uid: u32,
    gid: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mtime: Option<(i64, i64)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    target: Option<String>,
    /// Extended attributes, POSIX ACLs included, as hex values.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    xattrs: BTreeMap<String, String>,
}

fn utf8(path: &Path) -> Result<String> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| format!("{} is not a UTF-8 name", path.display()).into())
}

fn xattrs(path: &Path) -> Result<BTreeMap<String, String>> {
    let c_path = CString::new(path.as_os_str().as_bytes())?;
    let mut names = vec![0_u8; 4096];
    let length = loop {
        // SAFETY: c_path is NUL terminated and names is writable for its length.
        let length =
            unsafe { libc::llistxattr(c_path.as_ptr(), names.as_mut_ptr().cast(), names.len()) };
        if length >= 0 {
            break length.unsigned_abs();
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ERANGE) {
            return Err(format!("list the attributes of {}: {error}", path.display()).into());
        }
        names.resize(names.len() * 4, 0);
    };
    let mut attributes = BTreeMap::new();
    for name in names[..length].split(|byte| *byte == 0).filter(|n| !n.is_empty()) {
        let c_name = CString::new(name)?;
        let mut value = vec![0_u8; 256];
        let size = loop {
            // SAFETY: both strings are NUL terminated and value is writable for its length.
            let size = unsafe {
                libc::lgetxattr(
                    c_path.as_ptr(),
                    c_name.as_ptr(),
                    value.as_mut_ptr().cast(),
                    value.len(),
                )
            };
            if size >= 0 {
                break size.unsigned_abs();
            }
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ERANGE) {
                return Err(format!(
                    "read attribute {} of {}: {error}",
                    String::from_utf8_lossy(name),
                    path.display()
                )
                .into());
            }
            value.resize(value.len() * 4, 0);
        };
        attributes.insert(
            String::from_utf8(name.to_vec())?,
            hex::encode(&value[..size]),
        );
    }
    Ok(attributes)
}

fn sha256(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 1 << 20];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            return Ok(hex::encode(hasher.finalize()));
        }
        hasher.update(&buffer[..read]);
    }
}

/// The password file, where there is one, and every entry below `root`'s data
/// and metadata directories, in path order. Without the password Android
/// cannot unlock the storage. Sockets are left out: Android makes them again
/// at start.
fn walk(root: &Path) -> Result<Vec<Entry>> {
    let device = fs::symlink_metadata(root)?.dev();
    let mut entries = Vec::new();
    let mut pending: Vec<PathBuf> = host_storage::DIRECTORIES
        .iter()
        .rev()
        .map(PathBuf::from)
        .collect();
    if fs::symlink_metadata(root.join(host_storage::CREDENTIAL)).is_ok() {
        pending.push(PathBuf::from(host_storage::CREDENTIAL));
    }
    while let Some(relative) = pending.pop() {
        let path = root.join(&relative);
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.dev() != device {
            return Err(format!(
                "{} is another filesystem; is Android still running?",
                path.display()
            )
            .into());
        }
        let file_type = metadata.file_type();
        let mut entry = Entry {
            path: utf8(&relative)?,
            kind: Kind::Directory,
            mode: metadata.mode() & 0o7777,
            uid: metadata.uid(),
            gid: metadata.gid(),
            size: None,
            mtime: None,
            sha256: None,
            target: None,
            xattrs: xattrs(&path)?,
        };
        if file_type.is_dir() {
            let mut children = fs::read_dir(&path)?
                .map(|child| child.map(|child| relative.join(child.file_name())))
                .collect::<io::Result<Vec<_>>>()?;
            children.sort_unstable_by(|a, b| b.cmp(a));
            pending.extend(children);
        } else if file_type.is_file() {
            entry.kind = Kind::File;
            entry.size = Some(metadata.size());
            entry.mtime = Some((metadata.mtime(), metadata.mtime_nsec()));
            entry.sha256 = Some(sha256(&path)?);
        } else if file_type.is_symlink() {
            entry.kind = Kind::Symlink;
            entry.target = Some(utf8(&fs::read_link(&path)?)?);
        } else if file_type.is_fifo() {
            entry.kind = Kind::Fifo;
        } else if file_type.is_socket() {
            continue;
        } else {
            return Err(format!("{} is a device node", path.display()).into());
        }
        entries.push(entry);
    }
    Ok(entries)
}

fn release() -> Option<String> {
    fs::read_link(ACTIVE_RELEASE)
        .ok()
        .and_then(|target| target.file_name()?.to_str().map(str::to_owned))
}

fn finish(mut child: Child, name: &str) -> Result<()> {
    let status = child.wait()?;
    if !status.success() {
        return Err(format!("{name} failed: {status}").into());
    }
    Ok(())
}

/// Write `data_dir` as an archive to standard output. Android must be stopped.
pub fn backup(data_dir: &Path) -> Result<()> {
    if unsafe { libc::geteuid() } != 0 {
        return Err("a storage backup requires root".into());
    }
    host_storage::validate(data_dir)?;
    let entries = walk(data_dir)?;
    let bytes: u64 = entries.iter().filter_map(|entry| entry.size).sum();
    eprintln!(
        "storage-backup: {} entries, {bytes} bytes of files in {}",
        entries.len(),
        data_dir.display()
    );
    let work = tempfile::tempdir()?;
    let mut list = Vec::new();
    for entry in &entries {
        list.extend_from_slice(entry.path.as_bytes());
        list.push(0);
    }
    fs::write(work.path().join("list"), list)?;
    let manifest = Manifest {
        format: FORMAT,
        release: release(),
        entries,
    };
    fs::write(work.path().join(MANIFEST), serde_json::to_vec(&manifest)?)?;

    let mut tar = Command::new("tar")
        .args(["--create", "--file=-", "--sparse", "--no-recursion"])
        .args(TAR_METADATA)
        .arg("-C")
        .arg(work.path())
        .arg(MANIFEST)
        .arg("-C")
        .arg(data_dir)
        .args(["--null", "--verbatim-files-from", "--files-from"])
        .arg(work.path().join("list"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()?;
    let archive = tar.stdout.take().expect("tar output is piped");
    let zstd = Command::new("zstd")
        .args(["--quiet", "--threads=0", "-3", "--stdout"])
        .stdin(archive)
        .spawn()?;
    let tar_result = finish(tar, "tar");
    finish(zstd, "zstd")?;
    tar_result?;
    eprintln!("storage-backup: archive written");
    Ok(())
}

fn utc_stamp() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    let days = i64::try_from(seconds / 86_400).unwrap_or(0);
    let time = seconds % 86_400;
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}{month:02}{day:02}-{:02}{:02}{:02}",
        time / 3600,
        time / 60 % 60,
        time % 60
    )
}

fn rename_noreplace(from: &Path, to: &Path) -> Result<()> {
    let old = CString::new(from.as_os_str().as_bytes())?;
    let new = CString::new(to.as_os_str().as_bytes())?;
    // SAFETY: both paths are NUL terminated.
    if unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            old.as_ptr(),
            libc::AT_FDCWD,
            new.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    } != 0
    {
        let error = io::Error::last_os_error();
        return Err(format!("rename {} to {}: {error}", from.display(), to.display()).into());
    }
    Ok(())
}

fn compare(expected: &[Entry], found: &[Entry]) -> Result<()> {
    let found: BTreeMap<&str, &Entry> = found.iter().map(|e| (e.path.as_str(), e)).collect();
    let mut problems = Vec::new();
    for entry in expected {
        match found.get(entry.path.as_str()) {
            None => problems.push(format!("{} is missing", entry.path)),
            Some(restored) if *restored != entry => {
                problems.push(format!("{} differs: {restored:?}", entry.path));
            }
            Some(_) => {}
        }
    }
    let listed: std::collections::BTreeSet<&str> =
        expected.iter().map(|e| e.path.as_str()).collect();
    problems.extend(
        found
            .keys()
            .filter(|path| !listed.contains(*path))
            .map(|path| format!("{path} is not in the backup's list")),
    );
    if problems.is_empty() {
        return Ok(());
    }
    let shown = problems.iter().take(10).cloned().collect::<Vec<_>>();
    Err(format!(
        "the unpacked backup does not match its list ({} problems):\n{}",
        problems.len(),
        shown.join("\n")
    )
    .into())
}

/// Unpack an archive from standard input, check it, and make it `data_dir`.
/// Android must be stopped. The replaced storage is renamed, never removed.
pub fn restore(data_dir: &Path) -> Result<()> {
    if unsafe { libc::geteuid() } != 0 {
        return Err("a storage restore requires root".into());
    }
    let name = data_dir
        .file_name()
        .ok_or("data_dir has no name")?
        .to_str()
        .ok_or("data_dir is not a UTF-8 name")?;
    let parent = data_dir.parent().ok_or("data_dir has no parent")?;
    host_storage::check_parent(parent)?;
    // A restore that stopped part way leaves only its own unpacked copy.
    for stale in fs::read_dir(parent)? {
        let stale = stale?;
        if stale.file_name().as_bytes().starts_with(RESTORE_PREFIX.as_bytes()) {
            eprintln!(
                "storage-restore: removing an unfinished restore at {}",
                stale.path().display()
            );
            fs::remove_dir_all(stale.path())?;
        }
    }
    let staging = tempfile::Builder::new()
        .prefix(RESTORE_PREFIX)
        .tempdir_in(parent)?;
    fs::set_permissions(staging.path(), fs::Permissions::from_mode(0o700))?;

    let mut zstd = Command::new("zstd")
        .args(["--decompress", "--quiet", "--stdout"])
        .stdout(Stdio::piped())
        .spawn()?;
    let archive = zstd.stdout.take().expect("zstd output is piped");
    let tar = Command::new("tar")
        .args([
            "--extract",
            "--file=-",
            "--same-owner",
            "--same-permissions",
        ])
        .args(TAR_METADATA)
        .arg("-C")
        .arg(staging.path())
        .stdin(archive)
        .spawn()?;
    let zstd_result = finish(zstd, "zstd");
    finish(tar, "tar")?;
    zstd_result?;

    let manifest_path = staging.path().join(MANIFEST);
    let manifest: Manifest = serde_json::from_slice(&fs::read(&manifest_path)?)?;
    if manifest.format != FORMAT {
        return Err(format!("backup format {} is not {FORMAT}", manifest.format).into());
    }
    fs::remove_file(&manifest_path)?;
    compare(&manifest.entries, &walk(staging.path())?)?;
    host_storage::validate(staging.path())?;
    let directory = fs::File::open(staging.path())?;
    // SAFETY: the descriptor stays open for the call.
    if unsafe { libc::syncfs(directory.as_raw_fd()) } != 0 {
        return Err(io::Error::last_os_error().into());
    }

    let mut report = format!(
        "restored {} entries into {}",
        manifest.entries.len(),
        data_dir.display()
    );
    let staging = staging.keep();
    if fs::symlink_metadata(data_dir).is_ok() {
        let previous = parent.join(format!("{name}.before-restore-{}", utc_stamp()));
        rename_noreplace(data_dir, &previous)?;
        report.push_str(&format!(
            "; the previous storage is kept at {}",
            previous.display()
        ));
    }
    rename_noreplace(&staging, data_dir)?;
    fs::File::open(parent)?.sync_all()?;
    let current = release();
    if manifest.release.is_some() && manifest.release != current {
        report.push_str(&format!(
            "\nThe backup comes from Droidloom release {}, and this phone runs {}. If Android cannot unlock its accounts, restore on release {0}.",
            manifest.release.as_deref().unwrap_or("unknown"),
            current.as_deref().unwrap_or("unknown"),
        ));
    }
    let mut stdout = io::stdout().lock();
    writeln!(stdout, "{report}")?;
    stdout.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str) -> Entry {
        Entry {
            path: path.into(),
            kind: Kind::File,
            mode: 0o600,
            uid: 1,
            gid: 1,
            size: Some(3),
            mtime: Some((1, 2)),
            sha256: Some("aa".into()),
            target: None,
            xattrs: BTreeMap::new(),
        }
    }

    #[test]
    fn compare_names_missing_changed_and_extra_entries() {
        let expected = [entry("data/a"), entry("data/b")];
        let mut changed = entry("data/a");
        changed.mode = 0o644;
        let found = [changed, entry("data/c")];
        let message = compare(&expected, &found).unwrap_err().to_string();
        assert!(message.contains("3 problems"), "{message}");
        assert!(message.contains("data/a differs"));
        assert!(message.contains("data/b is missing"));
        assert!(message.contains("data/c is not in the backup's list"));
        assert!(compare(&expected, &[entry("data/a"), entry("data/b")]).is_ok());
    }

    #[test]
    fn walk_lists_every_entry_and_skips_sockets() {
        let root = tempfile::tempdir().unwrap();
        for name in host_storage::DIRECTORIES {
            fs::create_dir(root.path().join(name)).unwrap();
        }
        fs::create_dir(root.path().join("data/app")).unwrap();
        fs::write(root.path().join("data/app/file"), b"abc").unwrap();
        std::os::unix::fs::symlink("file", root.path().join("data/app/link")).unwrap();
        let _socket =
            std::os::unix::net::UnixListener::bind(root.path().join("data/app/sock")).unwrap();
        let entries = walk(root.path()).unwrap();
        let paths: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(
            paths,
            ["data", "data/app", "data/app/file", "data/app/link", "metadata"]
        );
        assert_eq!(
            entries[2].sha256.as_deref(),
            Some("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
        );
        assert_eq!(entries[3].target.as_deref(), Some("file"));
    }

    #[test]
    fn walk_lists_the_password_file() {
        let root = tempfile::tempdir().unwrap();
        for name in host_storage::DIRECTORIES {
            fs::create_dir(root.path().join(name)).unwrap();
        }
        fs::write(root.path().join(host_storage::CREDENTIAL), b"abc").unwrap();
        fs::write(root.path().join("other"), b"abc").unwrap();
        let entries = walk(root.path()).unwrap();
        let paths: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, ["credential", "data", "metadata"]);
        assert_eq!(entries[0].kind, Kind::File);
    }

    #[test]
    fn utc_stamp_is_a_sortable_date() {
        let stamp = utc_stamp();
        assert_eq!(stamp.len(), 15);
        assert!(stamp.starts_with("20"));
        assert_eq!(&stamp[8..9], "-");
    }
}
