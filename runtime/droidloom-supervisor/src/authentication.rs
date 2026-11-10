//! What Android's authentication takes from root. Android's system user is the
//! host user in a cell, so what would let that user unlock Android or sign a
//! fingerprint match stays where only root reads it:
//! - the key `KeyMint` and Droidloom's authentication service derive their shared
//!   HMAC key from, fresh at each start, readable by `KeyMint` and root;
//! - Android's hidden credential, beside the cell's storage on the host. The
//!   supervisor gives it to `system_server` alone (`control/host_lock.rs`).
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use crate::CellSpec;

/// Where `KeyMint` and the authentication service read the agreement key.
const AGREEMENT_KEY: &str = "dev/droidloom-agreement-key";
/// `KeyMint`'s user in the cell, `nobody`.
const KEYMINT_UID: u32 = 9999;
const CREDENTIAL_LENGTH: usize = 32;
const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnpqrstuvwxyz23456789";
/// Where releases before this one kept the credential, inside Android's storage.
const LEGACY_DIRECTORIES: [&str; 3] = ["data", "system", "droidloom"];
const LEGACY_CREDENTIAL: &str = "credential";

fn credential_path(spec: &CellSpec) -> PathBuf {
    spec.data_dir.join(crate::host_storage::CREDENTIAL)
}

fn random<const N: usize>() -> io::Result<[u8; N]> {
    let mut bytes = [0; N];
    let mut filled = 0;
    while filled < N {
        // SAFETY: the pointer and length describe the unfilled part of `bytes`.
        let read = unsafe { libc::getrandom(bytes[filled..].as_mut_ptr().cast(), N - filled, 0) };
        if read < 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
            continue;
        }
        filled += usize::try_from(read).unwrap_or(0);
    }
    Ok(bytes)
}

fn valid(text: &[u8]) -> bool {
    text.len() == CREDENTIAL_LENGTH && text.iter().all(|byte| ALPHABET.contains(byte))
}

fn read_credential_from(file: File) -> Option<String> {
    let mut text = Vec::new();
    file.take(CREDENTIAL_LENGTH as u64 + 1)
        .read_to_end(&mut text)
        .ok()?;
    valid(&text)
        .then(|| String::from_utf8(text).ok())
        .flatten()
}

fn read_credential_file(path: &Path) -> Option<String> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .ok()?;
    read_credential_from(file)
}

/// Android's hidden credential, as root keeps it.
pub(crate) fn read_credential(spec: &CellSpec) -> Option<String> {
    read_credential_file(&credential_path(spec))
}

fn open_at(directory: &OwnedFd, name: &str, flags: libc::c_int) -> io::Result<OwnedFd> {
    let name = CString::new(name).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    // SAFETY: a valid directory descriptor and a NUL-terminated name.
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a fresh descriptor returned by the kernel, owned from here on.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn unlink_at(directory: &OwnedFd, name: &str, flags: libc::c_int) -> io::Result<()> {
    let name = CString::new(name).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    // SAFETY: a valid directory descriptor and a NUL-terminated name.
    if unsafe { libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), flags) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// The folders down to the legacy credential, each opened without following a
/// link: Android's storage belongs to the host user, who could swap any of
/// them for one. The cell's data folder itself is root's.
fn legacy_directories(spec: &CellSpec) -> Option<Vec<OwnedFd>> {
    let mut directories = vec![OwnedFd::from(File::open(&spec.data_dir).ok()?)];
    for name in LEGACY_DIRECTORIES {
        let parent = directories.last()?;
        directories.push(open_at(parent, name, libc::O_RDONLY | libc::O_DIRECTORY).ok()?);
    }
    Some(directories)
}

fn write_credential(path: &Path, text: &[u8]) -> io::Result<()> {
    let fresh = path.with_extension("new");
    let _ = fs::remove_file(&fresh);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&fresh)?;
    file.write_all(text)?;
    file.sync_all()?;
    fs::rename(&fresh, path)?;
    fs::File::open(path.parent().unwrap_or(Path::new("/")))?.sync_all()
}

fn generate() -> io::Result<Vec<u8>> {
    Ok(random::<CREDENTIAL_LENGTH>()?
        .iter()
        .map(|byte| ALPHABET[usize::from(*byte) % ALPHABET.len()])
        .collect())
}

/// Keeps the credential on the host: the one an earlier release stored in
/// Android's storage moves here and leaves Android's storage, or a new one is
/// made for a cell that never had one.
fn prepare_credential(spec: &CellSpec) -> io::Result<()> {
    let path = credential_path(spec);
    let legacy = legacy_directories(spec);
    let folder = legacy.as_ref().and_then(|directories| directories.last());
    if read_credential_file(&path).is_none() {
        let text = match folder
            .and_then(|folder| open_at(folder, LEGACY_CREDENTIAL, libc::O_RDONLY).ok())
            .and_then(|file| read_credential_from(File::from(file)))
        {
            Some(text) => text.into_bytes(),
            None => generate()?,
        };
        write_credential(&path, &text)?;
    }
    if let Some(directories) = &legacy {
        match unlink_at(&directories[3], LEGACY_CREDENTIAL, 0) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
            _ => {}
        }
        let _ = unlink_at(&directories[2], LEGACY_DIRECTORIES[2], libc::AT_REMOVEDIR);
    }
    Ok(())
}

/// Before Android starts: the agreement key in the cell's private `/dev`, and
/// the credential on the host.
pub(crate) fn prepare(spec: &CellSpec, root: &Path) -> io::Result<()> {
    let key = root.join(AGREEMENT_KEY);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o400)
        .open(&key)?;
    file.write_all(&random::<32>()?)?;
    std::os::unix::fs::fchown(&file, Some(KEYMINT_UID), Some(KEYMINT_UID))?;
    file.set_permissions(fs::Permissions::from_mode(0o400))?;
    prepare_credential(spec)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_credentials_are_valid() {
        let text = generate().unwrap();
        assert!(valid(&text));
        assert_ne!(text, generate().unwrap());
    }

    #[test]
    fn credential_files_must_be_exact_and_not_links() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("credential");
        write_credential(&file, b"ABCDEFGHJKLMNPQRSTUVWXYZabcdefgh").unwrap();
        assert_eq!(
            read_credential_file(&file).as_deref(),
            Some("ABCDEFGHJKLMNPQRSTUVWXYZabcdefgh")
        );
        let link = directory.path().join("link");
        std::os::unix::fs::symlink(&file, &link).unwrap();
        assert_eq!(read_credential_file(&link), None);
        fs::write(&file, b"short").unwrap();
        assert_eq!(read_credential_file(&file), None);
    }
}
