//! Android writable state lives directly on the host filesystem.

use std::fs;
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

/// Names below `CellSpec::data_dir`, bind mounted at the matching Android paths.
pub const DIRECTORIES: [&str; 2] = ["data", "metadata"];
/// The file below `CellSpec::data_dir` with the password that unlocks Android's
/// user. Root alone reads it, and it is no part of Android's storage.
pub const CREDENTIAL: &str = "credential";

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

pub(crate) fn check_parent(parent: &Path) -> io::Result<()> {
    // Ancestors must prevent the desktop user from replacing any mount source.
    for path in parent.ancestors() {
        let m = fs::symlink_metadata(path)?;
        if !m.is_dir() || m.uid() != 0 || m.mode() & 0o022 != 0 {
            return Err(invalid(format!(
                "unsafe Android storage parent: {}",
                path.display()
            )));
        }
    }
    Ok(())
}

fn reject_legacy(parent: &Path) -> io::Result<()> {
    for name in ["data.img", "metadata.img"] {
        match fs::symlink_metadata(parent.join(name)) {
            Ok(_) => {
                return Err(invalid(format!(
                    "{} contains legacy Android images; stop Droidloom and use migrate-storage into a new directory before activation",
                    parent.display()
                )));
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => (),
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Validate direct directories without following links or accepting old images.
/// Android owns the inner directory modes/IDs; the outer parent stays private.
pub fn validate(parent: &Path) -> io::Result<()> {
    check_parent(parent)?;
    reject_legacy(parent)?;
    for name in DIRECTORIES {
        let path = parent.join(name);
        if !fs::symlink_metadata(&path)?.is_dir() {
            return Err(invalid(format!(
                "{} is not a direct directory",
                path.display()
            )));
        }
    }
    Ok(())
}

/// Provision a new directory layout; never replace images or reset existing data.
///
/// Each new directory starts like the root of a freshly formatted image
/// (0755, root): Android's system services must traverse `/metadata` before
/// init adjusts anything below it, or aconfigd cannot create the flag storage
/// and system_server aborts. The private parent keeps host users out.
pub fn prepare(parent: &Path) -> io::Result<()> {
    check_parent(parent)?;
    reject_legacy(parent)?;
    for name in DIRECTORIES {
        let path = parent.join(name);
        match fs::symlink_metadata(&path) {
            Ok(m) if m.is_dir() => (),
            Ok(_) => {
                return Err(invalid(format!(
                    "unsafe Android storage: {}",
                    path.display()
                )));
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                fs::create_dir(&path)?;
                fs::set_permissions(&path, fs::Permissions::from_mode(0o755))?;
            }
            Err(e) => return Err(e),
        }
    }
    fs::File::open(parent)?.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn legacy_storage_is_never_silently_reset() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("data.img"), b"existing user data").unwrap();
        assert!(reject_legacy(root.path()).is_err());
        assert!(!root.path().join("data").exists());
        assert_eq!(
            fs::read(root.path().join("data.img")).unwrap(),
            b"existing user data"
        );
    }
}
