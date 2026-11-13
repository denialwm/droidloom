//! Reproducible preparation of the Mesa source used by the Android vendor image.
//!
//! The network is deliberately outside this tool. It accepts an already
//! downloaded archive, verifies both hashes from Droidloom's source lock,
//! validates every archive path, applies an ordered patch set, and publishes a
//! content-addressed prepared tree atomically.

use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256, Sha512};
use tempfile::Builder;
use thiserror::Error;

const MANIFEST_NAME: &str = ".droidloom-mesa-source.json";

/// Successful source-preparation result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrepareResult {
    /// Mesa release version.
    pub version: String,
    /// Digest of every prepared source path and byte.
    pub tree_sha256: String,
    /// Published source directory.
    pub output: PathBuf,
}

#[derive(Debug, Deserialize)]
struct SourceLock {
    mesa: MesaLock,
}

#[derive(Debug, Deserialize)]
struct MesaLock {
    url: String,
    version: String,
    sha256: String,
    sha512: String,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
struct PreparedManifest {
    schema_version: u32,
    version: String,
    source_url: String,
    archive_sha256: String,
    archive_sha512: String,
    patches: Vec<PatchIdentity>,
    tree_sha256: String,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
struct PatchIdentity {
    name: String,
    sha256: String,
}

/// Failure while validating or preparing Mesa.
#[derive(Debug, Error)]
pub enum PrepareError {
    /// Filesystem operation failed.
    #[error("filesystem operation failed: {0}")]
    Io(#[from] io::Error),
    /// JSON parsing or serialization failed.
    #[error("invalid JSON: {0}")]
    Json(#[from] serde_json::Error),
    /// The lock or an input violates the preparation contract.
    #[error("invalid Mesa input: {0}")]
    Invalid(String),
    /// An external archive or patch command failed.
    #[error("{program} failed ({status}): {stderr}")]
    Command {
        /// Executable that failed.
        program: &'static str,
        /// Process exit status.
        status: String,
        /// Captured diagnostic output.
        stderr: String,
    },
}

/// Verify and prepare the exact Mesa source tree described by `lock_path`.
///
/// # Errors
///
/// Returns an error when the lock, archive, patch set, prepared tree, or an
/// external `tar`/`patch` operation fails validation.
pub fn prepare(
    lock_path: &Path,
    archive_path: &Path,
    patch_dir: &Path,
    output_path: &Path,
) -> Result<PrepareResult, PrepareError> {
    let archive_path = fs::canonicalize(archive_path)?;
    let patch_dir = fs::canonicalize(patch_dir)?;
    let lock: SourceLock = serde_json::from_reader(File::open(lock_path)?)?;
    validate_lock(&lock.mesa)?;
    validate_output(output_path)?;

    let archive_sha256 = hash_file::<Sha256>(&archive_path)?;
    let archive_sha512 = hash_file::<Sha512>(&archive_path)?;
    require_equal("archive SHA-256", &lock.mesa.sha256, &archive_sha256)?;
    require_equal("archive SHA-512", &lock.mesa.sha512, &archive_sha512)?;

    let patches = patch_identities(&patch_dir)?;
    let expected = PreparedManifest {
        schema_version: 2,
        version: lock.mesa.version.clone(),
        source_url: lock.mesa.url.clone(),
        archive_sha256,
        archive_sha512,
        patches,
        tree_sha256: String::new(),
    };

    if output_path.exists() {
        return verify_existing(output_path, expected);
    }

    let parent = output_path.parent().ok_or_else(|| {
        PrepareError::Invalid(format!("output has no parent: {}", output_path.display()))
    })?;
    fs::create_dir_all(parent)?;
    let staging = Builder::new()
        .prefix(".droidloom-mesa-stage-")
        .tempdir_in(parent)?;

    validate_archive_paths(&archive_path, &lock.mesa.version)?;
    run(
        "tar",
        Command::new("tar")
            .arg("-xJf")
            .arg(&archive_path)
            .arg("-C")
            .arg(staging.path())
            .arg("--strip-components=1")
            .arg("--no-same-owner")
            .arg("--no-same-permissions"),
    )?;

    for patch in &expected.patches {
        let patch_path = patch_dir.join(&patch.name);
        run(
            "patch",
            Command::new("patch")
                .current_dir(staging.path())
                .arg("--batch")
                .arg("--forward")
                .arg("-p1")
                .arg("--input")
                .arg(&patch_path),
        )?;
    }

    // Mesa generates this architecture-independent enum header as part of its
    // Meson build. Minigbm's AMD DRI loader consumes the same format ABI, but
    // Soong may compile minigbm before Meson has produced its private copy.
    // Materialize it in the pinned source tree so both build graphs share the
    // exact header generated by this Mesa release.
    let format_dir = staging.path().join("src/util/format");
    let generated = run(
        "python3",
        Command::new("python3")
            .current_dir(&format_dir)
            .arg("u_format_table.py")
            .arg("u_format.yaml")
            .arg("--enums"),
    )?;
    if generated.stdout.is_empty() {
        return Err(PrepareError::Invalid(
            "Mesa format-header generator produced no output".into(),
        ));
    }
    fs::write(format_dir.join("u_format_gen.h"), generated.stdout)?;

    require_prepared_files(staging.path())?;
    let mut manifest = expected;
    manifest.tree_sha256 = hash_tree(staging.path())?;
    let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
    fs::write(staging.path().join(MANIFEST_NAME), manifest_bytes)?;

    let staged_path = staging.keep();
    if let Err(error) = fs::rename(&staged_path, output_path) {
        let _ = fs::remove_dir_all(&staged_path);
        return Err(error.into());
    }

    Ok(PrepareResult {
        version: manifest.version,
        tree_sha256: manifest.tree_sha256,
        output: output_path.to_path_buf(),
    })
}

/// Digest of the inputs a prepared tree was made from: the Mesa archive and
/// every patch, by name and content. Unlike the tree digest it ignores files
/// the host's tools leave behind, so equal inputs give equal identities.
///
/// # Errors
///
/// Returns an error when the prepared tree has no readable manifest.
pub fn source_identity(output_path: &Path) -> Result<String, PrepareError> {
    let manifest: PreparedManifest =
        serde_json::from_reader(File::open(output_path.join(MANIFEST_NAME))?)?;
    let mut digest = Sha256::new();
    digest.update(manifest.archive_sha256.as_bytes());
    for patch in &manifest.patches {
        digest.update(b"\n");
        digest.update(patch.name.as_bytes());
        digest.update(b" ");
        digest.update(patch.sha256.as_bytes());
    }
    Ok(hex::encode(digest.finalize()))
}

fn validate_lock(lock: &MesaLock) -> Result<(), PrepareError> {
    if !lock.url.starts_with("https://archive.mesa3d.org/mesa-") || !lock.url.ends_with(".tar.xz") {
        return Err(PrepareError::Invalid(format!(
            "unexpected Mesa release URL {:?}",
            lock.url
        )));
    }
    if lock.version.is_empty()
        || !lock
            .version
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'.' || byte == b'-')
    {
        return Err(PrepareError::Invalid(format!(
            "unsafe Mesa version {:?}",
            lock.version
        )));
    }
    validate_hex("Mesa SHA-256", &lock.sha256, 64)?;
    validate_hex("Mesa SHA-512", &lock.sha512, 128)
}

fn validate_hex(label: &str, value: &str, length: usize) -> Result<(), PrepareError> {
    if value.len() != length
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(PrepareError::Invalid(format!(
            "{label} must be {length} lowercase hexadecimal characters"
        )));
    }
    Ok(())
}

fn validate_output(output: &Path) -> Result<(), PrepareError> {
    if output.as_os_str().is_empty() || output == Path::new("/") {
        return Err(PrepareError::Invalid(format!(
            "unsafe output path {}",
            output.display()
        )));
    }
    Ok(())
}

fn validate_archive_paths(archive: &Path, version: &str) -> Result<(), PrepareError> {
    let output = run("tar", Command::new("tar").arg("-tJf").arg(archive))?;
    let listing = String::from_utf8(output.stdout)
        .map_err(|_| PrepareError::Invalid("archive contains a non-UTF-8 path".into()))?;
    let root = format!("mesa-{version}");
    let mut entries = 0_u64;
    for line in listing.lines() {
        entries += 1;
        let path = Path::new(line);
        let mut components = path.components();
        if components.next() != Some(Component::Normal(OsStr::new(&root)))
            || components
                .any(|component| !matches!(component, Component::Normal(_) | Component::CurDir))
        {
            return Err(PrepareError::Invalid(format!(
                "unsafe or unexpected archive path {line:?}"
            )));
        }
    }
    if entries == 0 {
        return Err(PrepareError::Invalid("Mesa archive is empty".into()));
    }
    Ok(())
}

fn patch_identities(patch_dir: &Path) -> Result<Vec<PatchIdentity>, PrepareError> {
    let mut paths = fs::read_dir(patch_dir)?
        .map(|entry| entry.map(|value| value.path()))
        .collect::<Result<Vec<_>, _>>()?;
    paths.retain(|path| path.extension() == Some(OsStr::new("patch")));
    paths.sort();
    if paths.is_empty() {
        return Err(PrepareError::Invalid(format!(
            "no .patch files in {}",
            patch_dir.display()
        )));
    }
    paths
        .into_iter()
        .map(|path| {
            let name = path
                .file_name()
                .and_then(OsStr::to_str)
                .ok_or_else(|| PrepareError::Invalid("non-UTF-8 patch filename".into()))?
                .to_owned();
            Ok(PatchIdentity {
                name,
                sha256: hash_file::<Sha256>(&path)?,
            })
        })
        .collect()
}

fn verify_existing(
    output_path: &Path,
    mut expected: PreparedManifest,
) -> Result<PrepareResult, PrepareError> {
    let manifest_path = output_path.join(MANIFEST_NAME);
    if !manifest_path.is_file() {
        return Err(PrepareError::Invalid(format!(
            "refusing unmanaged existing output {}",
            output_path.display()
        )));
    }
    let actual: PreparedManifest = serde_json::from_reader(File::open(manifest_path)?)?;
    expected.tree_sha256.clone_from(&actual.tree_sha256);
    if actual != expected {
        return Err(PrepareError::Invalid(format!(
            "prepared manifest does not match current inputs at {}",
            output_path.display()
        )));
    }
    let tree_sha256 = hash_tree(output_path)?;
    require_equal(
        "prepared Mesa tree SHA-256",
        &actual.tree_sha256,
        &tree_sha256,
    )?;
    require_prepared_files(output_path)?;
    Ok(PrepareResult {
        version: actual.version,
        tree_sha256,
        output: output_path.to_path_buf(),
    })
}

fn require_prepared_files(root: &Path) -> Result<(), PrepareError> {
    for relative in [
        "VERSION",
        "meson.build",
        "android/Android.mk",
        "android/mesa3d_cross.mk",
        "src/util/format/u_format_gen.h",
        "src/freedreno/common/freedreno_common.h",
        "src/freedreno/vulkan/meson.build",
    ] {
        if !root.join(relative).is_file() {
            return Err(PrepareError::Invalid(format!(
                "prepared Mesa tree is missing {relative}"
            )));
        }
    }
    Ok(())
}

fn run(program: &'static str, command: &mut Command) -> Result<Output, PrepareError> {
    let output = command.output()?;
    if output.status.success() {
        return Ok(output);
    }
    Err(PrepareError::Command {
        program,
        status: output.status.to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
    })
}

fn require_equal(label: &str, expected: &str, actual: &str) -> Result<(), PrepareError> {
    if expected == actual {
        return Ok(());
    }
    Err(PrepareError::Invalid(format!(
        "{label} mismatch: expected {expected}, got {actual}"
    )))
}

fn hash_file<D: Digest + Default>(path: &Path) -> Result<String, PrepareError> {
    let mut file = File::open(path)?;
    let mut digest = D::default();
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Ok(hex::encode(digest.finalize()))
}

fn hash_tree(root: &Path) -> Result<String, PrepareError> {
    let mut paths = Vec::new();
    collect_paths(root, root, &mut paths)?;
    paths.sort();
    let mut digest = Sha256::new();
    for relative in paths {
        let bytes = relative.as_os_str().as_encoded_bytes();
        digest.update((bytes.len() as u64).to_le_bytes());
        digest.update(bytes);
        let full = root.join(&relative);
        let metadata = fs::symlink_metadata(&full)?;
        if metadata.file_type().is_symlink() {
            digest.update(b"L");
            let target = fs::read_link(full)?;
            let target = target.as_os_str().as_encoded_bytes();
            digest.update((target.len() as u64).to_le_bytes());
            digest.update(target);
        } else {
            digest.update(b"F");
            let mut file = File::open(full)?;
            io::copy(&mut file, &mut DigestWriter(&mut digest))?;
        }
    }
    Ok(hex::encode(digest.finalize()))
}

fn collect_paths(root: &Path, current: &Path, output: &mut Vec<PathBuf>) -> io::Result<()> {
    for entry in fs::read_dir(current)? {
        let entry = entry?;
        let path = entry.path();
        let relative = path
            .strip_prefix(root)
            .expect("collected path must remain below root");
        if relative == Path::new(MANIFEST_NAME) {
            continue;
        }
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.is_dir() {
            collect_paths(root, &path, output)?;
        } else if metadata.is_file() || metadata.file_type().is_symlink() {
            output.push(relative.to_path_buf());
        }
    }
    Ok(())
}

struct DigestWriter<'a, D: Digest>(&'a mut D);

impl<D: Digest> io::Write for DigestWriter<'_, D> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.0.update(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_release_identity() {
        assert!(
            validate_lock(&MesaLock {
                url: "https://archive.mesa3d.org/mesa-26.1.7.tar.xz".into(),
                version: "26.1.7".into(),
                sha256: "a".repeat(64),
                sha512: "b".repeat(128),
            })
            .is_ok()
        );
    }

    #[test]
    fn rejects_unsafe_version() {
        assert!(
            validate_lock(&MesaLock {
                url: "https://archive.mesa3d.org/mesa-../bad.tar.xz".into(),
                version: "../bad".into(),
                sha256: "a".repeat(64),
                sha512: "b".repeat(128),
            })
            .is_err()
        );
    }

    #[test]
    fn tree_hash_is_stable_and_detects_changes() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("nested")).unwrap();
        fs::write(root.path().join("nested/file"), b"one").unwrap();
        let first = hash_tree(root.path()).unwrap();
        assert_eq!(first, hash_tree(root.path()).unwrap());
        fs::write(root.path().join("nested/file"), b"two").unwrap();
        assert_ne!(first, hash_tree(root.path()).unwrap());
    }

    #[test]
    fn tree_hash_ignores_its_manifest() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("source"), b"same").unwrap();
        let first = hash_tree(root.path()).unwrap();
        fs::write(root.path().join(MANIFEST_NAME), b"metadata").unwrap();
        assert_eq!(first, hash_tree(root.path()).unwrap());
    }
}
