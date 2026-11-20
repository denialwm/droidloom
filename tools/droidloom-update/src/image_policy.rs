//! Derive the desktop system_ext image without legacy vendor compatibility APEXes.
use crate::util::*;
use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    process::Command,
};

const LEGACY_VNDK: [&str; 4] = [
    "apex/com.android.vndk.v31.apex",
    "apex/com.android.vndk.v32.apex",
    "apex/com.android.vndk.v33.apex",
    "apex/com.android.vndk.v34.apex",
];

fn desktop_vendor(properties: &str) -> Result<()> {
    let properties: BTreeMap<_, _> = properties
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.starts_with('#') {
                None
            } else {
                line.split_once('=')
            }
        })
        .collect();
    if properties.get("ro.product.vendor.device") != Some(&"droidloom_x86_64")
        || properties.get("ro.vendor.build.version.sdk") != Some(&"37")
        || !matches!(
            properties.get("ro.vendor.product.cpu.abilist"),
            Some(&"x86_64" | &"x86_64,arm64-v8a")
        )
        || ["ro.vndk.version", "ro.product.vndk.version"]
            .iter()
            .any(|key| properties.get(key).is_some_and(|value| !value.is_empty()))
    {
        return fail(
            "legacy VNDK removal requires the Android 17 Droidloom x86_64 vendor without a VNDK selection",
        );
    }
    Ok(())
}

pub fn stage(source: &Path, destination: &Path, vendor_properties: &Path) -> Result<()> {
    desktop_vendor(&fs::read_to_string(vendor_properties)?)?;
    fs::create_dir_all(destination.parent().ok_or("image has no parent")?)?;
    // A single fakeroot session preserves otherwise privileged ownership and
    // security xattrs across extraction, mkfs, and independent re-extraction.
    // Cargo can replace this updater while it builds the host tools. Read the
    // running executable directly, even when its old pathname was unlinked.
    let worker = tempfile::Builder::new()
        .prefix("image-worker-")
        .tempdir_in(destination.parent().ok_or("image has no parent")?)?;
    let executable = worker.path().join("droidloom-update");
    fs::copy("/proc/self/exe", &executable)?;
    crate::util::mode(&executable, 0o755)?;
    run(Command::new("fakeroot")
        .arg("--")
        .arg(&executable)
        .arg("prune-desktop-image")
        .arg("--image")
        .arg(source)
        .arg("--destination")
        .arg(destination)
        .arg("--vendor-properties")
        .arg(vendor_properties))
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Entry {
    uid: u32,
    gid: u32,
    mode: u32,
    modified: (i64, i64),
    content: String,
    attributes: String,
}

pub(crate) fn inventory(root: &Path) -> Result<BTreeMap<PathBuf, Entry>> {
    fn visit(root: &Path, relative: &Path, entries: &mut BTreeMap<PathBuf, Entry>) -> Result<()> {
        let path = root.join(relative);
        let meta = fs::symlink_metadata(&path)?;
        let content = if meta.is_file() {
            hash(&path)?
        } else if meta.file_type().is_symlink() {
            fs::read_link(&path)?.to_string_lossy().into_owned()
        } else if meta.is_dir() {
            String::new()
        } else {
            return fail(format!(
                "unexpected object in system_ext: {}",
                relative.display()
            ));
        };
        let attributes = output(
            Command::new("getfattr")
                .args([
                    "--absolute-names",
                    "--dump",
                    "--no-dereference",
                    "--encoding=hex",
                    "--match=-",
                ])
                .arg(&path),
        )?
        .lines()
        .filter(|line| !line.starts_with("# file:"))
        .collect::<Vec<_>>()
        .join("\n");
        entries.insert(
            relative.to_owned(),
            Entry {
                uid: meta.uid(),
                gid: meta.gid(),
                mode: meta.mode(),
                modified: (meta.mtime(), meta.mtime_nsec()),
                content,
                attributes,
            },
        );
        if meta.is_dir() {
            for entry in fs::read_dir(path)? {
                visit(root, &relative.join(entry?.file_name()), entries)?;
            }
        }
        Ok(())
    }
    let mut entries = BTreeMap::new();
    visit(root, Path::new(""), &mut entries)?;
    Ok(entries)
}

/// The time every derived image gives the files it adds: the same as each
/// image's creation time.
pub(crate) const FIXED_TIME: (i64, i64) = (1_230_768_000, 0);

/// Modification time of `path` itself, not of a symlink's target.
pub(crate) fn modified(path: &Path) -> Option<(i64, i64)> {
    fs::symlink_metadata(path)
        .ok()
        .map(|meta| (meta.mtime(), meta.mtime_nsec()))
}

/// Sets the modification time of `path` itself, not of a symlink's target.
pub(crate) fn set_modified(path: &Path, (seconds, nanoseconds): (i64, i64)) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let name = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    let times = [
        libc::timespec {
            tv_sec: 0,
            tv_nsec: libc::UTIME_OMIT,
        },
        libc::timespec {
            tv_sec: seconds,
            tv_nsec: nanoseconds,
        },
    ];
    if unsafe {
        libc::utimensat(
            libc::AT_FDCWD,
            name.as_ptr(),
            times.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

pub(crate) fn extract(image: &Path, destination: &Path) -> Result<()> {
    fs::create_dir(destination)?;
    run(Command::new("fsck.erofs")
        .arg("--preserve")
        .arg("--xattrs")
        .arg(format!("--extract={}", destination.display()))
        .arg(image))
}

pub fn prune(image: &Path, destination: &Path, vendor_properties: &Path) -> Result<()> {
    if std::env::var_os("FAKEROOTKEY").is_none() {
        return fail("image derivation must run inside fakeroot to preserve Android metadata");
    }
    desktop_vendor(&fs::read_to_string(vendor_properties)?)?;
    if destination.exists() {
        return fail("derived image destination already exists");
    }
    let work = tempfile::Builder::new()
        .prefix("vndk-prune-")
        .tempdir_in(destination.parent().ok_or("image has no parent")?)?;
    let tree = work.path().join("tree");
    extract(image, &tree)?;
    let mut expected = inventory(&tree)?;
    for relative in LEGACY_VNDK {
        if !fs::symlink_metadata(tree.join(relative))?.is_file() {
            return fail(format!("expected a regular legacy VNDK APEX: {relative}"));
        }
        fs::remove_file(tree.join(relative))?;
        expected
            .remove(Path::new(relative))
            .ok_or("missing VNDK inventory entry")?;
    }
    // Removing directory entries changes the staging directory's mtime only.
    let apex = expected
        .get(Path::new("apex"))
        .ok_or("missing apex directory")?;
    fs::File::open(tree.join("apex"))?.set_modified(
        std::time::UNIX_EPOCH
            + std::time::Duration::new(apex.modified.0.try_into()?, apex.modified.1.try_into()?),
    )?;
    let rebuilt = work.path().join("system_ext.img");
    run(Command::new("mkfs.erofs")
        // makepkg exports SOURCE_DATE_EPOCH, which mkfs.erofs takes over -T as
        // the creation time: the image would differ in every build.
        .env_remove("SOURCE_DATE_EPOCH")
        .args([
            "-zlz4hc,level=9",
            "--workers=2",
            "-T1230768000",
            "--mkfs-time",
            "--preserve-mtime",
            "-U7b895cb0-54d5-4a4d-950d-50cda078053f",
        ])
        .arg(&rebuilt)
        .arg(&tree))?;
    let checked = work.path().join("checked");
    extract(&rebuilt, &checked)?;
    let actual = inventory(&checked)?;
    if actual != expected {
        let changed: Vec<_> = expected
            .keys()
            .chain(actual.keys())
            .filter(|path| expected.get(*path) != actual.get(*path))
            .collect();
        for path in changed.iter().take(4) {
            eprintln!(
                "{path:?}: expected {:?}; actual {:?}",
                expected.get(*path),
                actual.get(*path)
            );
        }
        return fail(format!(
            "derived system_ext changed retained file contents or metadata: {changed:?}"
        ));
    }
    eprintln!(
        "Removed four legacy VNDK APEXes; system_ext: {} -> {} bytes; retained contents, ownership, modes, timestamps and xattrs verified",
        fs::metadata(image)?.len(),
        fs::metadata(&rebuilt)?.len()
    );
    fs::rename(rebuilt, destination)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    const PROPERTIES: &str = "ro.product.vendor.device=droidloom_x86_64\nro.vendor.build.version.sdk=37\nro.vendor.product.cpu.abilist=x86_64\n";
    #[test]
    fn rejects_legacy_vendor_and_other_products() {
        desktop_vendor(PROPERTIES).unwrap();
        for properties in [
            PROPERTIES.replace("sdk=37", "sdk=34"),
            PROPERTIES.replace("droidloom_x86_64", "phone"),
            format!("{PROPERTIES}ro.vndk.version=34\n"),
            format!("{PROPERTIES}ro.product.vndk.version=31\n"),
        ] {
            assert!(desktop_vendor(&properties).is_err());
        }
    }
    #[test]
    fn repack_preserves_android_metadata_and_adbd() {
        if std::env::var_os("DROIDLOOM_IMAGE_POLICY_TEST").is_none() {
            run(Command::new("fakeroot")
                .arg("--")
                .arg(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "image_policy::tests::repack_preserves_android_metadata_and_adbd",
                    "--nocapture",
                ])
                .env("DROIDLOOM_IMAGE_POLICY_TEST", "1"))
            .unwrap();
            return;
        }
        use std::os::unix::fs::{PermissionsExt, symlink};
        let work = tempfile::tempdir().unwrap();
        let root = work.path().join("input");
        fs::create_dir_all(root.join("apex")).unwrap();
        for file in LEGACY_VNDK
            .into_iter()
            .chain(["apex/com.android.adbd.apex"])
        {
            fs::write(root.join(file), vec![42; 8192]).unwrap();
        }
        let adbd = root.join("apex/com.android.adbd.apex");
        run(Command::new("chown").arg("1000:2000").arg(&adbd)).unwrap();
        fs::set_permissions(&adbd, fs::Permissions::from_mode(0o640)).unwrap();
        run(Command::new("setfattr")
            .args(["-n", "security.selinux", "-v", "u:object_r:system_file:s0"])
            .arg(&adbd))
        .unwrap();
        symlink("com.android.adbd.apex", root.join("apex/retained-link")).unwrap();
        let properties = work.path().join("vendor.prop");
        fs::write(&properties, PROPERTIES).unwrap();
        let original = work.path().join("original.img");
        run(Command::new("mkfs.erofs")
            .args(["-zlz4hc", "--workers=2"])
            .arg(&original)
            .arg(&root))
        .unwrap();
        let destination = work.path().join("derived.img");
        prune(&original, &destination, &properties).unwrap();
        let check = work.path().join("verify");
        extract(&destination, &check).unwrap();
        let retained = fs::metadata(check.join("apex/com.android.adbd.apex")).unwrap();
        assert_eq!(
            (retained.uid(), retained.gid(), retained.mode() & 0o777),
            (1000, 2000, 0o640)
        );
        assert!(LEGACY_VNDK.iter().all(|file| !check.join(file).exists()));
        assert!(prune(&original, &destination, &properties).is_err());
    }
}
