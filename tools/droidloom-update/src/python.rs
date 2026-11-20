//! Checksum-pinned, pure-Python build dependencies kept outside the system interpreter.
use crate::util::*;
use serde::Deserialize;
use std::{
    fs,
    path::{Component, Path},
    process::Command,
};
#[derive(Deserialize)]
struct LockFile {
    schema: u32,
    packages: Vec<Package>,
}
#[derive(Deserialize)]
struct Package {
    filename: String,
    url: String,
    sha256: String,
    source_directory: Option<String>,
}
fn check_paths(list: &str) -> Result<()> {
    for name in list.lines() {
        if Path::new(name)
            .components()
            .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
        {
            return fail("unsafe Python dependency archive path");
        }
    }
    Ok(())
}
/// Prepares the locked modules in `work/python`, once per lock: builds of
/// both architectures read them, and Mesa's makefile names the path.
pub fn prepare(repo: &Path, work: &Path) -> Result<()> {
    let bytes = fs::read(repo.join("android/manifest/build-python-lock.json"))?;
    let marker = work.join("python.lock-sha256");
    let identity = sha256_hex(&bytes);
    let destination = work.join("python");
    if destination.is_dir() && fs::read_to_string(&marker).ok().as_deref() == Some(&identity) {
        return Ok(());
    }
    let lock: LockFile = serde_json::from_slice(&bytes)?;
    if lock.schema != 1 {
        return fail("unsupported Python dependency lock");
    }
    let cache = work.join("python-archives");
    fs::create_dir_all(&cache)?;
    let staging = tempfile::Builder::new()
        .prefix("python-")
        .tempdir_in(work)?;
    let libraries = staging.path().join("libraries");
    fs::create_dir(&libraries)?;
    for package in lock.packages {
        if Path::new(&package.filename).components().count() != 1 {
            return fail("invalid dependency filename");
        }
        let archive = cache.join(&package.filename);
        crate::assemble::download(&package.url, &archive, &package.sha256)?;
        if let Some(directory) = package.source_directory {
            // MarkupSafe and PyYAML ship supported pure-Python fallbacks. No optional
            // C extension is needed by the build generators, or mixed across Pythons.
            check_paths(&output(Command::new("tar").arg("-tzf").arg(&archive))?)?;
            let source = staging.path().join(&package.filename);
            fs::create_dir(&source)?;
            run(Command::new("tar")
                .arg("-xzf")
                .arg(&archive)
                .arg("-C")
                .arg(&source)
                .args(["--no-same-owner", "--no-same-permissions"]))?;
            let root = source
                .join(package.filename.trim_end_matches(".tar.gz"))
                .join(&directory);
            let destination = libraries.join(
                Path::new(&directory)
                    .file_name()
                    .ok_or("dependency has no module")?,
            );
            for file in files(&root)? {
                copy(&file, &destination.join(file.strip_prefix(&root)?))?;
            }
        } else {
            check_paths(&output(Command::new("unzip").arg("-Z1").arg(&archive))?)?;
            run(Command::new("unzip")
                .args(["-q", "-o"])
                .arg(&archive)
                .arg("-d")
                .arg(&libraries))?;
        }
    }
    if destination.exists() {
        fs::remove_dir_all(&destination)?;
    }
    fs::rename(libraries, destination)?;
    write(&marker, identity)?;
    Ok(())
}
