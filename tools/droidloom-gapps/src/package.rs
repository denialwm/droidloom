use crate::util::*;
use droidloom_contracts::gapps::{FileDigest, Manifest};
use std::{fs, os::unix::fs::MetadataExt, path::Path, process::Command};

fn version(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 80
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._+-".contains(&b))
}

pub fn build(
    addon: &Path,
    output: &Path,
    package_version: &str,
    standalone: bool,
) -> Result<()> {
    if !version(package_version) || package_version.contains('-') {
        return Err(
            "the package version is not a plain version without a release".into(),
        );
    }
    new_output(output)?;
    let manifest = Manifest::load(&addon.join("manifest.json"))?;
    for (role, expected) in &manifest.images {
        if FileDigest::read(&addon.join("images").join(format!("{role}.img")))? != *expected {
            return Err("add-on image changed before packaging".into());
        }
    }
    let work = tempfile::Builder::new()
        .prefix("gapps-package-")
        .tempdir_in(output.parent().ok_or("output has no parent")?)?;
    let tree = work.path().join("payload");
    let destination = tree.join("usr/lib/droidloom/addons/gapps");
    fs::create_dir_all(destination.join("images"))?;
    for name in [
        "manifest.json",
        "import-report.json",
        "LICENSE.LiteGapps",
        "NOTICE",
        "images/product.img",
        "images/system_ext.img",
    ] {
        run(Command::new("cp")
            .args(["--reflink=auto", "--sparse=always", "--"])
            .arg(addon.join(name))
            .arg(destination.join(name)))?;
    }
    let license = tree.join("usr/share/licenses/droidloom-gapps");
    write(
        &license.join("LICENSE.LiteGapps"),
        fs::read(addon.join("LICENSE.LiteGapps"))?,
    )?;
    write(&license.join("NOTICE"), fs::read(addon.join("NOTICE"))?)?;
    let guide = include_str!("../../../docs/BUILDING.md")
        .split_once("## Optional Google apps")
        .ok_or("missing optional package guide")?
        .1;
    write(
        &tree.join("usr/share/doc/droidloom-gapps/README.md"),
        format!("# Optional Google apps\n{guide}"),
    )?;
    // The managed package uses the existing runtime helper. Developer runtimes
    // are stopped explicitly before installing/replacing/removing this data package.
    if !standalone {
        write(
            &tree.join("usr/share/libalpm/hooks/60-droidloom-gapps.hook"),
            include_str!("../../../packaging/arch/60-droidloom-gapps.hook"),
        )?;
    }
    // Any release: the add-on is bound to the pinned Android base images, which
    // the supervisor checks by hash at each start, not to a Droidloom release.
    let dependencies = if standalone {
        ""
    } else {
        "depends=(droidloom-runtime droidloom-image)\n"
    };
    // Only validated scalar values enter PKGBUILD. Paths are supplied via the
    // process working directory, never interpolated into shell source.
    let recipe = format!(
        "pkgname=droidloom-gapps\npkgver={package_version}\npkgrel=1\narch=({})\npkgdesc='Optional locally imported Google apps for Droidloom Android 17'\nlicense=(LicenseRef-Android LicenseRef-Google MIT)\noptions=(!strip !debug !purge)\n{dependencies}package() {{\n  cp -a \"$startdir/payload/usr\" \"$pkgdir/\"\n  chown -R 0:0 \"$pkgdir/usr\"\n  chmod -R u=rwX,go=rX \"$pkgdir/usr\"\n}}\n",
        manifest.architecture.as_str()
    );
    write(&work.path().join("PKGBUILD"), recipe)?;
    write(
        &work.path().join("makepkg.conf"),
        format!(
            "source /etc/makepkg.conf\nCARCH='{}'\nCOMPRESSZST=(zstd -c -z -q -T1 -)\nPKGEXT='.pkg.tar.zst'\n",
            manifest.architecture.as_str()
        ),
    )?;
    let cwd = std::env::current_dir()?;
    let repo = cwd
        .ancestors()
        .find(|p| p.join("packaging/arch/Containerfile").is_file())
        .ok_or("run package inside the Droidloom checkout")?;
    let storage = repo.join(".work/arch");
    fs::create_dir_all(&storage)?;
    let podman = || {
        let mut command = Command::new("podman");
        command
            .arg("--root")
            .arg(storage.join("containers"))
            .arg("--runroot")
            .arg(storage.join("container-run"));
        command
    };
    let identity = fs::metadata("/proc/self")?;
    if identity.uid() == 0 {
        return Err("package builds use rootless Podman; run as the ordinary build user".into());
    }
    let image = "localhost/droidloom-arch-builder";
    match podman().args(["image", "exists", image]).status()?.code() {
        Some(0) => {}
        Some(1) => run(podman()
            .args(["build", "--tag", image, "--file"])
            .arg(repo.join("packaging/arch/Containerfile"))
            .arg(repo.join("packaging/arch")))?,
        _ => return Err("cannot access rootless Podman package storage".into()),
    }
    run(podman()
        .args([
            "run",
            "--rm",
            "--network=none",
            "--userns=keep-id",
            "--user",
        ])
        .arg(format!("{}:{}", identity.uid(), identity.gid()))
        .args(["--workdir", "/build", "--volume"])
        .arg(format!(
            "{}:/build:rw",
            work.path().canonicalize()?.display()
        ))
        .args([
            "--env",
            "PKGDEST=/build",
            "--env",
            "SOURCE_DATE_EPOCH=1230768000",
            image,
            "makepkg",
            "--nodeps",
            "--force",
            "--config",
            "/build/makepkg.conf",
        ]))?;
    let archives: Vec<_> = fs::read_dir(work.path())?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().ends_with(".pkg.tar.zst"))
        .collect();
    if archives.len() != 1 {
        return Err("makepkg did not produce exactly one package".into());
    }
    let info = text(
        Command::new("bsdtar")
            .args(["-xOf"])
            .arg(archives[0].path())
            .arg(".PKGINFO"),
    )?;
    if !info
        .lines()
        .any(|line| line == format!("arch = {}", manifest.architecture.as_str()))
        || !info.lines().any(|line| line == "pkgname = droidloom-gapps")
    {
        return Err("makepkg produced incorrect package identity or guest architecture".into());
    }
    fs::rename(archives[0].path(), output)?;
    println!("Optional package: {}", output.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_shell_syntax_in_package_versions() {
        for input in ["", "1;id", "$(id)", "`id`", "1\narch=(any)", "'quoted'"] {
            assert!(!version(input));
        }
        assert!(version("4.9.20260513"));
        assert!(version("0.1.0-14"));
    }
}
