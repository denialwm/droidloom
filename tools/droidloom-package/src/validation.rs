//! Exercise real pacman transactions without exposing host devices or services.
use super::{Result, podman, repository, require_user, run};
use std::{
    fs,
    io::Read,
    os::unix::fs::symlink,
    path::{Path, PathBuf},
    process::Command,
};

fn exec(work: &Path, name: &str, args: &[&str]) -> Result<()> {
    run(podman(work).args(["exec", name]).args(args))
}

fn output(work: &Path, name: &str, args: &[&str]) -> Result<String> {
    let result = podman(work).args(["exec", name]).args(args).output()?;
    if !result.status.success() {
        return Err(format!(
            "container check failed: {args:?}: {}",
            String::from_utf8_lossy(&result.stderr)
        )
        .into());
    }
    Ok(String::from_utf8(result.stdout)?)
}

fn archives(packages: &Path, mount: &str) -> Result<Vec<String>> {
    let mut archives = Vec::new();
    for prefix in ["droidloom-runtime-", "droidloom-image-"] {
        let matches = fs::read_dir(&packages)?
            .collect::<std::io::Result<Vec<_>>>()?
            .into_iter()
            .filter(|e| {
                let name = e.file_name();
                let name = name.to_string_lossy();
                name.starts_with(prefix) && name.ends_with(".pkg.tar.zst")
            })
            .collect::<Vec<_>>();
        if matches.len() != 1 {
            return Err(format!(
                "expected exactly one {prefix} package in {}",
                packages.display()
            )
            .into());
        }
        archives.push(format!(
            "{mount}/{}",
            matches[0].file_name().to_string_lossy()
        ));
    }
    Ok(archives)
}

fn pkginfo(archive: &Path) -> Result<String> {
    let output = Command::new("bsdtar")
        .arg("-xOf")
        .arg(archive)
        .arg(".PKGINFO")
        .output()?;
    if !output.status.success() {
        return Err(format!("{} has no package metadata", archive.display()).into());
    }
    Ok(String::from_utf8(output.stdout)?)
}

fn architecture(info: &str) -> Result<&str> {
    info.lines()
        .find_map(|line| line.strip_prefix("arch = "))
        .ok_or_else(|| "package metadata names no architecture".into())
}

/// ELF machine of a file, or None when it is not ELF.
fn elf_machine(path: &Path) -> Result<Option<u16>> {
    let mut header = [0_u8; 20];
    let mut file = fs::File::open(path)?;
    if file.read(&mut header)? < header.len() || header[..4] != *b"\x7fELF" {
        return Ok(None);
    }
    // e_machine follows the identification bytes, e_type, in the file's byte order.
    let machine = [header[18], header[19]];
    Ok(Some(match header[5] {
        1 => u16::from_le_bytes(machine),
        _ => u16::from_be_bytes(machine),
    }))
}

/// Packages for another architecture cannot be installed here. Check that
/// they claim it and that the host programs were compiled for it.
fn check_foreign(packages: &Path, architecture: &str) -> Result<()> {
    const EM_AARCH64: u16 = 183;
    if architecture != "aarch64" {
        return Err(format!("no check for {architecture} packages").into());
    }
    let root = packages.to_str().ok_or("non-UTF8 package path")?;
    let archives: Vec<PathBuf> = archives(packages, root)?
        .into_iter()
        .map(PathBuf::from)
        .collect();
    for archive in &archives {
        if self::architecture(&pkginfo(archive)?)? != architecture {
            return Err(format!("{} is not an {architecture} package", archive.display()).into());
        }
    }
    let tree = tempfile::tempdir()?;
    run(Command::new("bsdtar")
        .arg("-xpf")
        .arg(&archives[0])
        .arg("--no-same-owner")
        .arg("-C")
        .arg(tree.path())
        .args(["usr/bin", "usr/lib/droidloom/droidloom-package-helper"]))?;
    let mut programs = Vec::new();
    for directory in ["usr/bin", "usr/lib/droidloom"] {
        for entry in fs::read_dir(tree.path().join(directory))? {
            let path = entry?.path();
            if path.is_file() && !path.is_symlink() {
                programs.push(path);
            }
        }
    }
    let mut compiled = 0;
    for program in &programs {
        match elf_machine(program)? {
            Some(EM_AARCH64) => compiled += 1,
            Some(machine) => {
                return Err(
                    format!("{} is built for ELF machine {machine}", program.display()).into(),
                );
            }
            None => {}
        }
    }
    if compiled == 0 {
        return Err("the runtime package carries no host programs".into());
    }
    println!(
        "{} {architecture} packages claim their architecture; {compiled} host programs are AArch64. Installation was not exercised on this host.",
        archives.len()
    );
    Ok(())
}

pub fn check(packages: &Path, previous: Option<&Path>) -> Result<()> {
    require_user()?;
    let packages = packages.canonicalize()?;
    let runtime = PathBuf::from(
        archives(&packages, packages.to_str().ok_or("non-UTF8 package path")?)?.remove(0),
    );
    let info = pkginfo(&runtime)?;
    let target = architecture(&info)?;
    if target != std::env::consts::ARCH {
        if previous.is_some() {
            return Err("upgrade transactions need packages for this host's architecture".into());
        }
        return check_foreign(&packages, target);
    }
    let repo = repository(None)?;
    let work = super::build_workspace(&repo)?;
    fs::create_dir_all(&work)?;
    let current = archives(&packages, "/packages")?;
    let previous = previous.map(Path::canonicalize).transpose()?;
    let old = previous
        .as_ref()
        .map(|p| archives(p, "/previous"))
        .transpose()?;
    // A filesystem fixture exercises GPU selection. No DRM device is passed in.
    let fixture = tempfile::Builder::new()
        .prefix("package-check-")
        .tempdir_in(&work)?;
    // As in sysfs, the render node's device is a PCI device with a driver link.
    let drm = fixture.path().join("drm");
    fs::create_dir_all(drm.join("renderD129"))?;
    fs::create_dir_all(drm.join("devices/0000:0a:00.0"))?;
    fs::create_dir_all(drm.join("drivers/amdgpu"))?;
    symlink("../devices/0000:0a:00.0", drm.join("renderD129/device"))?;
    symlink("../../drivers/amdgpu", drm.join("devices/0000:0a:00.0/driver"))?;
    let name = format!("droidloom-package-check-{}", std::process::id());
    eprintln!(
        "Testing actual pacman transactions in disposable rootless Arch. No host sudo, devices or graphical services are used."
    );
    let mut container = podman(&work);
    container
        .args(["run", "--detach", "--init", "--name", &name])
        .arg("--volume")
        .arg(format!("{}:/packages:ro", packages.display()))
        .arg("--volume")
        .arg(format!("{}:/sys/class/drm:ro", drm.display()));
    if let Some(previous) = previous {
        container
            .arg("--volume")
            .arg(format!("{}:/previous:ro", previous.display()));
    }
    run(container.args([
        "docker.io/library/archlinux:base",
        "/usr/bin/sleep",
        "infinity",
    ]))?;
    let result = transactions(&work, &name, &current, old.as_deref());
    let cleanup = run(podman(&work).args(["rm", "--force", &name]));
    result?;
    cleanup?;
    println!(
        "Package checks passed: dependency installation, pacman ownership, UID 1234 setup, repeated setup, reinstall, removal and retained Android data. Desktop runtime validation is separate."
    );
    if old.is_some() {
        println!("Upgrade and downgrade transactions also preserved Android data.");
    }
    Ok(())
}

fn transactions(
    work: &Path,
    name: &str,
    archives: &[String],
    previous: Option<&[String]>,
) -> Result<()> {
    let helper = "/usr/lib/droidloom/droidloom-package-helper";
    exec(work, name, &["systemd-machine-id-setup"])?;
    exec(work, name, &["pacman-key", "--init"])?;
    exec(work, name, &["pacman-key", "--populate", "archlinux"])?;
    exec(work, name, &["pacman", "-Syu", "--noconfirm"])?;
    if let Some(previous) = previous {
        let current = output(work, name, &["pacman", "-Qp", &archives[0]])?;
        let old = output(work, name, &["pacman", "-Qp", &previous[0]])?;
        let current = current
            .split_whitespace()
            .nth(1)
            .ok_or("current package has no version")?;
        let old = old
            .split_whitespace()
            .nth(1)
            .ok_or("previous package has no version")?;
        if output(work, name, &["vercmp", current, old])?
            .trim()
            .parse::<i32>()?
            <= 0
        {
            return Err("--previous must contain an older package revision to exercise upgrade and downgrade".into());
        }
    }
    let install = ["pacman", "-U", "--noconfirm", &archives[0], &archives[1]];
    exec(work, name, &install)?;
    for path in [
        "/usr/bin/droidloomctl",
        helper,
        "/usr/lib/droidloom/images/images/system.img",
    ] {
        exec(work, name, &["pacman", "-Qo", path])?;
    }
    exec(work, name, &["/usr/bin/droidloomctl", "--help"])?;
    exec(
        work,
        name,
        &["useradd", "--create-home", "--uid", "1234", "preview"],
    )?;
    exec(work, name, &[helper, "setup", "--uid", "1234"])?;
    exec(
        work,
        name,
        &["stat", "--format=%a %U:%G %n", "/etc/polkit-1/rules.d"],
    )?;
    let spec: serde_json::Value =
        serde_json::from_str(&output(work, name, &["cat", "/etc/droidloom/cell.json"])?)?;
    if spec["host_uid"] != 1234
        || spec["render_node"] != "/dev/dri/renderD129"
        || spec["image_dir"] != "/usr/lib/droidloom/images"
    {
        return Err(
            "setup did not use the target account, fixture GPU and package image paths".into(),
        );
    }
    let data = "/var/lib/droidloom/users/1234/data/data";
    // Existing Android directory identity must survive package reinstalls.
    let data_identity = output(work, name, &["stat", "--format=%i:%s", data])?;
    exec(
        work,
        name,
        &["runuser", "-u", "preview", "--", helper, "prepare"],
    )?;
    exec(work, name, &[helper, "setup", "--uid", "1234"])?;
    exec(work, name, &install)?;
    if output(work, name, &["stat", "--format=%i:%s", data])? != data_identity {
        return Err("repeated setup or package reinstall replaced existing Android data".into());
    }
    if let Some(previous) = previous {
        exec(
            work,
            name,
            &["pacman", "-U", "--noconfirm", &previous[0], &previous[1]],
        )?;
        exec(
            work,
            name,
            &["runuser", "-u", "preview", "--", helper, "prepare"],
        )?;
        if output(work, name, &["stat", "--format=%i:%s", data])? != data_identity {
            return Err("downgrade replaced existing Android data".into());
        }
        exec(work, name, &install)?;
        exec(
            work,
            name,
            &["runuser", "-u", "preview", "--", helper, "prepare"],
        )?;
        if output(work, name, &["stat", "--format=%i:%s", data])? != data_identity {
            return Err("upgrade replaced existing Android data".into());
        }
    }
    exec(
        work,
        name,
        &[
            "pacman",
            "-R",
            "--noconfirm",
            "droidloom-runtime",
            "droidloom-image",
        ],
    )?;
    if output(work, name, &["stat", "--format=%i:%s", data])? != data_identity {
        return Err("package removal did not preserve Android data".into());
    }
    exec(work, name, &["test", "-f", "/etc/droidloom/cell.json"])?;
    for path in [
        "/etc/droidloom/runtime.env",
        "/etc/droidloom/package-state.json",
        "/etc/polkit-1/rules.d/49-droidloom.rules",
        "/usr/bin/droidloomctl",
    ] {
        exec(work, name, &["test", "!", "-e", path])?;
    }
    exec(work, name, &install)?;
    exec(
        work,
        name,
        &["runuser", "-u", "preview", "--", helper, "prepare"],
    )?;
    if output(work, name, &["stat", "--format=%i:%s", data])? != data_identity {
        return Err("reinstall after removal replaced existing Android data".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elf_machine_reads_either_byte_order() {
        let dir = tempfile::tempdir().unwrap();
        let mut little = *b"\x7fELF\x02\x01\x01\0\0\0\0\0\0\0\0\0\x02\0\xb7\0";
        let path = dir.path().join("program");
        fs::write(&path, little).unwrap();
        assert_eq!(elf_machine(&path).unwrap(), Some(183));
        little[5] = 2;
        little[18..20].copy_from_slice(&183_u16.to_be_bytes());
        fs::write(&path, little).unwrap();
        assert_eq!(elf_machine(&path).unwrap(), Some(183));
        fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
        assert_eq!(elf_machine(&path).unwrap(), None);
    }

    #[test]
    fn architecture_comes_from_package_metadata() {
        let info = "pkgname = droidloom-runtime\npkgver = 0.1.0-27\narch = aarch64\n";
        assert_eq!(architecture(info).unwrap(), "aarch64");
        assert!(architecture("pkgname = droidloom-runtime\n").is_err());
    }
}
