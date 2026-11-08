use crate::{bundle, util::*};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};
const BASE: &str = "/usr/lib/droidloom";
const ALIASES: &[(&str, &str)] = &[
    ("/usr/bin/droidloomd", "usr/bin/droidloomd"),
    ("/usr/bin/droidloomctl", "usr/bin/droidloomctl"),
    (
        "/usr/bin/droidloom-supervisor",
        "usr/bin/droidloom-supervisor",
    ),
    ("/usr/bin/droidloom-wayland", "usr/bin/droidloom-wayland"),
    ("/usr/bin/droidloom-audio", "usr/bin/droidloom-audio"),
    ("/usr/bin/droidloom-gpu-server", "usr/bin/droidloom-gpu-server"),
    ("/usr/bin/droidloom-devices", "usr/bin/droidloom-devices"),
    (
        "/usr/bin/droidloom-applications",
        "usr/bin/droidloom-applications",
    ),
    ("/usr/bin/droidloom-update", "usr/bin/droidloom-update"),
    ("/usr/bin/droidloom-doctor", "usr/bin/droidloom-doctor"),
    ("/usr/lib/droidloom/current", "usr/lib/droidloom/runtime"),
    (
        "/usr/lib/droidloom/storage/vold",
        "usr/lib/droidloom/storage/vold",
    ),
    (
        "/var/lib/droidloom/images/current",
        "var/lib/droidloom/images",
    ),
    ("/etc/droidloom/cell.json", "etc/droidloom/cell.json"),
    (
        "/usr/lib/systemd/system/droidloomd.service",
        "usr/lib/systemd/system/droidloomd.service",
    ),
    (
        "/usr/lib/systemd/user/droidloom.service",
        "usr/lib/systemd/user/droidloom.service",
    ),
    (
        "/usr/lib/systemd/user/droidloom-applications.service",
        "usr/lib/systemd/user/droidloom-applications.service",
    ),
    (
        "/usr/lib/systemd/user/droidloom-recovery.service",
        "usr/lib/systemd/user/droidloom-recovery.service",
    ),
    (
        "/usr/lib/environment.d/60-droidloom.conf",
        "usr/lib/environment.d/60-droidloom.conf",
    ),
    (
        "/usr/lib/systemd/user/droidloom-audio.service",
        "usr/lib/systemd/user/droidloom-audio.service",
    ),
    (
        "/usr/lib/systemd/user/droidloom-audio.socket",
        "usr/lib/systemd/user/droidloom-audio.socket",
    ),
    (
        "/usr/lib/systemd/user/droidloom-gpu.service",
        "usr/lib/systemd/user/droidloom-gpu.service",
    ),
    (
        "/usr/lib/systemd/user/droidloom-gpu.socket",
        "usr/lib/systemd/user/droidloom-gpu.socket",
    ),
    (
        "/usr/lib/systemd/user/droidloom-devices.service",
        "usr/lib/systemd/user/droidloom-devices.service",
    ),
    (
        "/usr/lib/systemd/user/droidloom-devices.socket",
        "usr/lib/systemd/user/droidloom-devices.socket",
    ),
    (
        "/etc/polkit-1/rules.d/49-droidloom.rules",
        "etc/polkit-1/rules.d/49-droidloom.rules",
    ),
];
fn user(uid: u32) -> Result<String> {
    let record = output(Command::new("getent").args(["passwd", &uid.to_string()]))?;
    Ok(record.split(':').next().ok_or("unknown user")?.into())
}
fn user_command(uid: u32, program: &str) -> Result<Command> {
    let mut c = Command::new("runuser");
    c.args([
        "-u",
        &user(uid)?,
        "--",
        "env",
        &format!("XDG_RUNTIME_DIR=/run/user/{uid}"),
        &format!("DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/{uid}/bus"),
        program,
    ]);
    Ok(c)
}
fn start(uid: u32) -> Result<()> {
    // Starting the unit directly retains the session mode selected by droidloomctl.
    run(user_command(uid, "systemctl")?.args(["--user", "start", "droidloom.service"]))
}
fn reload(uid: u32) -> Result<()> {
    run(Command::new("systemctl").arg("daemon-reload"))?;
    run(user_command(uid, "systemctl")?.args(["--user", "daemon-reload"]))
}
fn stop(uid: u32) -> Result<()> {
    // systemctl works with the previous CLI too, and never targets the compositor.
    if Path::new("/usr/lib/systemd/user/droidloom.service").exists() {
        run(user_command(uid, "systemctl")?.args(["--user", "stop", "droidloom.service"]))?;
    }
    if Path::new("/usr/lib/systemd/system/droidloomd.service").exists() {
        run(Command::new("systemctl").args(["stop", "droidloomd.service"]))?;
    }
    Ok(())
}
#[derive(Serialize, Deserialize)]
struct Saved {
    path: String,
    link: Option<PathBuf>,
    file: Option<String>,
}
#[derive(Serialize, Deserialize)]
struct Journal {
    uid: u32,
    previous: Option<PathBuf>,
    was_active: bool,
    aliases: Vec<Saved>,
}
fn restore(base: &Path, journal: &Journal) -> Result<()> {
    stop(journal.uid)?;
    restore_files(base, journal)?;
    reload(journal.uid)?;
    if journal.was_active {
        start(journal.uid)?;
    }
    fs::remove_file(base.join("transaction.json"))?;
    fs::File::open(base)?.sync_all()?;
    Ok(())
}
fn restore_files(base: &Path, journal: &Journal) -> Result<()> {
    if let Some(previous) = &journal.previous {
        atomic_link(previous, &base.join("active"))?;
    }
    for saved in &journal.aliases {
        let path = Path::new(&saved.path);
        if fs::symlink_metadata(path).is_ok() {
            fs::remove_file(path)?;
        }
        if let Some(link) = &saved.link {
            atomic_link(link, path)?;
        }
        if let Some(file) = &saved.file {
            copy(&base.join("transaction-backup").join(file), path)?;
            fs::File::open(path)?.sync_all()?;
        }
        fs::File::open(path.parent().ok_or("alias lacks parent")?)?.sync_all()?;
    }
    if journal.previous.is_none() && base.join("active").symlink_metadata().is_ok() {
        fs::remove_file(base.join("active"))?;
    }
    Ok(())
}
pub fn recover(uid: u32) -> Result<()> {
    refuse_package_installation()?;
    if unsafe { libc::geteuid() } != 0 {
        return fail("recovery requires administrator authentication");
    }
    let base = Path::new(BASE);
    let _lock = Lock::acquire(&base.join("update.lock"))?;
    let path = base.join("transaction.json");
    if !path.exists() {
        return Ok(());
    }
    let journal: Journal = serde_json::from_slice(&fs::read(path)?)?;
    if uid != journal.uid {
        return fail("interrupted update belongs to a different desktop owner");
    }
    eprintln!("Recovering the previous Droidloom installation before rebuilding");
    restore(base, &journal)
}
fn stage_bundle(payload: &Path, private: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    for source in files(payload)? {
        let relative = source.strip_prefix(payload)?;
        let destination = private.join(relative);
        let mut candidates = Vec::new();
        if let Ok(image) = relative.strip_prefix("var/lib/droidloom/images/images")
            && ["system.img", "system_ext.img", "product.img"]
                .iter()
                .any(|name| image == Path::new(name))
        {
            candidates.push(Path::new("/var/lib/droidloom/images/current/images").join(image));
        }
        let expected = if candidates.is_empty() {
            String::new()
        } else {
            hash(&source)?
        };
        let mut selected = source.clone();
        for candidate in candidates {
            if candidate.is_file() && hash(&candidate)? == expected {
                selected = candidate;
                break;
            }
        }
        // Reflink matching pinned base images on the installation filesystem.
        // This saves space without sharing mutable inodes with an older release.
        copy(&selected, &destination)?;
        mode(
            &destination,
            fs::metadata(&source)?.permissions().mode() & 0o777,
        )?;
        for directory in destination
            .parent()
            .unwrap()
            .ancestors()
            .take_while(|p| *p != private)
        {
            mode(directory, 0o755)?;
        }
    }
    Ok(())
}
pub fn apply(payload: &Path, uid: u32, start_runtime: bool) -> Result<()> {
    refuse_package_installation()?;
    if unsafe { libc::geteuid() } != 0 {
        return fail("activation requires administrator authentication");
    }
    if uid == 0 {
        return fail("the desktop owner must be an unprivileged user");
    }
    if let Ok(invoker) = std::env::var("PKEXEC_UID").or_else(|_| std::env::var("SUDO_UID"))
        && invoker.parse::<u32>()? != uid
    {
        return fail("desktop owner differs from authenticated caller");
    }
    let base = Path::new(BASE);
    fs::create_dir_all(base)?;
    let _lock = Lock::acquire(&base.join("update.lock"))?;
    if base.join("transaction.json").exists() {
        let j: Journal = serde_json::from_slice(&fs::read(base.join("transaction.json"))?)?;
        eprintln!("Recovering interrupted Droidloom activation");
        restore(base, &j)?;
    }
    // Private root-owned copy closes the user-writable staging race. Verify only this copy.
    fs::create_dir_all(base.join("releases"))?;
    let private = tempfile::Builder::new()
        .prefix(".incoming-")
        .tempdir_in(base.join("releases"))?;
    stage_bundle(payload, private.path())?;
    let manifest = bundle::verify(private.path())?;
    if manifest.architecture != std::env::consts::ARCH {
        return fail("bundle architecture does not match this host");
    }
    let spec: serde_json::Value =
        serde_json::from_slice(&fs::read(private.path().join("etc/droidloom/cell.json"))?)?;
    if spec["host_uid"].as_u64() != Some(u64::from(uid)) {
        return fail("bundle desktop owner differs from authenticated update owner");
    }
    for (_, relative) in ALIASES {
        if !private.path().join(relative).exists() {
            return fail(format!("incomplete installation bundle: {relative}"));
        }
    }
    let id = manifest.build_id.clone();
    let release = base.join("releases").join(&id);
    if release.exists() {
        bundle::verify(&release)?;
    } else {
        sync_tree(private.path())?;
        fs::rename(private.path(), &release)?;
        fs::File::open(base.join("releases"))?.sync_all()?;
    }
    mode(&release, 0o755)?;
    fs::File::open(&release)?.sync_all()?;
    let data = PathBuf::from(
        spec["data_dir"]
            .as_str()
            .ok_or("bundle lacks data directory")?,
    );
    if !data.starts_with("/var/lib/droidloom")
        || data
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return fail("Android data must remain below /var/lib/droidloom");
    }
    fs::create_dir_all(&data)?;
    mode(&data, 0o700)?;
    droidloom_supervisor::host_storage::prepare(&data)?;
    let active = Command::new("systemctl")
        .args(["is-active", "--quiet", "droidloomd.service"])
        .status()?
        .success();
    let backup = base.join("transaction-backup");
    if backup.exists() {
        fs::remove_dir_all(&backup)?;
    }
    fs::create_dir(&backup)?;
    mode(&backup, 0o700)?;
    let mut j = Journal {
        uid,
        previous: fs::read_link(base.join("active")).ok(),
        was_active: active,
        aliases: Vec::new(),
    };
    for (index, (path, _)) in ALIASES.iter().enumerate() {
        let path = Path::new(path);
        let mut saved = Saved {
            path: path.to_str().unwrap().into(),
            link: None,
            file: None,
        };
        if let Ok(metadata) = fs::symlink_metadata(path) {
            if metadata.file_type().is_symlink() {
                saved.link = Some(fs::read_link(path)?);
            } else if metadata.is_file() {
                let name = index.to_string();
                copy(path, &backup.join(&name))?;
                saved.file = Some(name);
            } else {
                return fail(format!(
                    "refusing to replace non-file alias {}",
                    path.display()
                ));
            }
        }
        j.aliases.push(saved);
    }
    sync_tree(&backup)?;
    durable_write(
        &base.join("transaction.json"),
        serde_json::to_vec_pretty(&j)?,
    )?;
    let result = (|| -> Result<()> {
        stop(uid)?;
        atomic_link(&release, &base.join("active"))?;
        for (path, relative) in ALIASES {
            let path = Path::new(path);
            if *relative == "etc/droidloom/cell.json" {
                // The supervisor deliberately accepts specifications only below /etc/droidloom.
                durable_write(path, fs::read(release.join(relative))?)?;
                mode(path, 0o644)?;
            } else {
                atomic_link(&base.join("active").join(relative), path)?;
            }
        }
        reload(uid)?;
        run(Command::new("systemctl").args(["disable", "droidloomd.service"]))?;
        run(Command::new("systemctl").args(["--global", "disable", "droidloom.service"]))?;
        run(user_command(uid, "systemctl")?.args(["--user", "disable", "droidloom.service"]))?;
        if start_runtime {
            start(uid)?;
            let response = output(
                user_command(uid, "/usr/bin/droidloomctl")?.args(["--json", "applications"]),
            )?;
            let response: serde_json::Value = serde_json::from_str(&response)?;
            if response["ok"] != true
                || response["applications"]
                    .as_array()
                    .is_none_or(|a| a.is_empty())
            {
                return fail("Android readiness check failed");
            }
        }
        Ok(())
    })();
    if let Err(error) = result {
        eprintln!("Activation failed: {error}; restoring previous release");
        restore(base, &j)?;
        return Err(error);
    }
    fs::remove_file(base.join("transaction.json"))?;
    fs::File::open(base)?.sync_all()?;
    durable_write(
        &base.join("last-update.json"),
        serde_json::to_vec_pretty(
            &serde_json::json!({"release":id,"source":manifest.source_identity,"input_abi":manifest.input_abi,"readiness":if start_runtime {"catalog"} else {"not-started"},"autostart":false}),
        )?,
    )?;
    if start_runtime {
        println!("Droidloom updated and running. Automatic startup remains disabled.");
    } else {
        println!(
            "Droidloom installed and stopped. Runtime readiness has not been checked. Automatic startup remains disabled."
        );
    }
    Ok(())
}

fn refuse_package_installation() -> Result<()> {
    if Path::new("/usr/share/droidloom/package.json").exists() {
        return fail(
            "Droidloom is managed by pacman; use pacman -U to replace package-owned files. The development updater cannot activate or recover an installation over a pacman package.",
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn failed_first_activation_restores_legacy_files_and_links() {
        let d = tempfile::tempdir().unwrap();
        let base = d.path().join("base");
        let backup = base.join("transaction-backup");
        fs::create_dir_all(&backup).unwrap();
        let binary = d.path().join("binary");
        write(&backup.join("0"), b"old binary").unwrap();
        mode(&backup.join("0"), 0o755).unwrap();
        let link = d.path().join("runtime");
        let new = d.path().join("new-alias");
        let old = d.path().join("old-runtime");
        let j = Journal {
            uid: 1000,
            previous: None,
            was_active: true,
            aliases: vec![
                Saved {
                    path: binary.to_str().unwrap().into(),
                    link: None,
                    file: Some("0".into()),
                },
                Saved {
                    path: link.to_str().unwrap().into(),
                    link: Some(old.clone()),
                    file: None,
                },
                Saved {
                    path: new.to_str().unwrap().into(),
                    link: None,
                    file: None,
                },
            ],
        };
        atomic_link(&base.join("candidate"), &base.join("active")).unwrap();
        for p in [&binary, &link, &new] {
            atomic_link(&base.join("active"), p).unwrap();
        }
        restore_files(&base, &j).unwrap();
        assert_eq!(fs::read(binary).unwrap(), b"old binary");
        assert_eq!(fs::read_link(link).unwrap(), old);
        assert!(new.symlink_metadata().is_err());
        assert!(base.join("active").symlink_metadata().is_err());
        // Repeated recovery is safe after a crash halfway through the rollback.
        restore_files(&base, &j).unwrap();
    }
    #[test]
    fn failed_update_switches_complete_previous_release_back() {
        let d = tempfile::tempdir().unwrap();
        let old = d.path().join("old");
        let new = d.path().join("new");
        write(&old.join("host"), b"old-host").unwrap();
        write(&old.join("android"), b"old-android").unwrap();
        write(&new.join("host"), b"new-host").unwrap();
        write(&new.join("android"), b"new-android").unwrap();
        atomic_link(&new, &d.path().join("active")).unwrap();
        let j = Journal {
            uid: 1000,
            previous: Some(old),
            was_active: true,
            aliases: vec![],
        };
        restore_files(d.path(), &j).unwrap();
        assert_eq!(fs::read(d.path().join("active/host")).unwrap(), b"old-host");
        assert_eq!(
            fs::read(d.path().join("active/android")).unwrap(),
            b"old-android"
        );
    }
}

#[cfg(test)]
mod staging_tests {
    use super::*;
    #[test]
    fn all_shipped_host_programs_have_stable_entry_points() {
        for name in crate::assemble::HOST_BINARIES {
            assert!(
                ALIASES
                    .iter()
                    .any(|(path, relative)| *path == format!("/usr/bin/{name}")
                        && *relative == format!("usr/bin/{name}")),
                "{name}"
            );
        }
    }
    #[test]
    fn staged_component_does_not_share_mutable_inode() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let payload = d.path().join("payload");
        let stage = d.path().join("stage");
        fs::create_dir(&stage).unwrap();
        write(&payload.join("usr/bin/test"), b"same").unwrap();
        mode(&payload.join("usr/bin/test"), 0o755).unwrap();
        stage_bundle(&payload, &stage).unwrap();
        write(&payload.join("usr/bin/test"), b"changed-build-output").unwrap();
        assert_eq!(fs::read(stage.join("usr/bin/test")).unwrap(), b"same");
        assert_eq!(
            fs::metadata(stage.join("usr/bin/test"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
        assert_eq!(
            fs::metadata(stage.join("usr/bin"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
    }
}
