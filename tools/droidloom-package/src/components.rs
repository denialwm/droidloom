//! Rebuild host components over a completed portable package baseline.
use super::{Architecture, Result, Version, run};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Clone, Debug, Deserialize, Serialize, ValueEnum)]
pub enum Component {
    #[value(name = "droidloom-supervisor", alias = "supervisor")]
    Supervisor,
    #[value(name = "droidloom-wayland", alias = "wayland")]
    Wayland,
    #[value(name = "droidloom-applications", alias = "applications")]
    Applications,
    #[value(name = "droidloom-audio", alias = "audio")]
    Audio,
    #[value(name = "droidloom-gpu-server", alias = "gpu")]
    Gpu,
    #[value(name = "droidloom-doctor", alias = "doctor")]
    Doctor,
    #[value(name = "droidloom-package-support", alias = "package-support")]
    PackageSupport,
}

impl Component {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Supervisor => "droidloom-supervisor",
            Self::Wayland => "droidloom-wayland",
            Self::Applications => "droidloom-applications",
            Self::Audio => "droidloom-audio",
            Self::Gpu => "droidloom-gpu-server",
            Self::Doctor => "droidloom-doctor",
            Self::PackageSupport => "droidloom-package-support",
        }
    }

    fn binaries(&self) -> &'static [(&'static str, &'static str)] {
        match self {
            Self::Supervisor => &[
                ("droidloom-supervisor", "usr/bin/droidloom-supervisor"),
                ("droidloomctl", "usr/bin/droidloomctl"),
                ("droidloomd", "usr/bin/droidloomd"),
            ],
            Self::Wayland => &[("droidloom-wayland", "usr/bin/droidloom-wayland")],
            Self::Applications => &[("droidloom-applications", "usr/bin/droidloom-applications")],
            Self::Audio => &[("droidloom-audio", "usr/bin/droidloom-audio")],
            Self::Gpu => &[("droidloom-gpu-server", "usr/bin/droidloom-gpu-server")],
            Self::Doctor => &[("droidloom-doctor", "usr/bin/droidloom-doctor")],
            Self::PackageSupport => &[(
                "droidloom-package-helper",
                "usr/lib/droidloom/droidloom-package-helper",
            )],
        }
    }
}

// A partial build must not silently reuse an Android image or dependency notices
// after changing pinned inputs, dependency resolution or the packaging recipe.
fn inputs(source: &Path) -> Result<BTreeMap<String, Vec<u8>>> {
    let mut result = BTreeMap::new();
    for path in ["Cargo.toml", "Cargo.lock", "rust-toolchain.toml"] {
        result.insert(path.into(), fs::read(source.join(path))?);
    }
    fn visit(source: &Path, relative: &Path, result: &mut BTreeMap<String, Vec<u8>>) -> Result<()> {
        for entry in fs::read_dir(source.join(relative))? {
            let entry = entry?;
            let path = relative.join(entry.file_name());
            if entry.file_type()?.is_dir() {
                visit(source, &path, result)?;
            } else if entry.file_type()?.is_file()
                && path != Path::new("packaging/arch/version.json")
                && path.extension().is_none_or(|ext| ext != "md")
            {
                result.insert(path.to_string_lossy().into_owned(), fs::read(entry.path())?);
            }
        }
        Ok(())
    }
    for directory in [
        "packaging",
        "android/manifest",
        "protocol",
        "runtime/droidloom-contracts/src",
        "tools/droidloom-update/src",
    ] {
        visit(source, Path::new(directory), &mut result)?;
    }
    Ok(result)
}

pub fn record_inputs(source: &Path, output: &Path) -> Result<()> {
    fs::write(
        output.join("component-inputs.json"),
        serde_json::to_vec(&inputs(source)?)?,
    )?;
    Ok(())
}

pub fn baseline(repo: &Path, current: &Version) -> Result<PathBuf> {
    let mut candidates = Vec::new();
    if !repo.join("dist/arch").is_dir() {
        return Err(
            "component builds require a completed package baseline; run a full build first".into(),
        );
    }
    for entry in fs::read_dir(repo.join("dist/arch"))? {
        let path = entry?.path().join(&current.architecture);
        let Ok(summary) = fs::read(path.join("build-summary.json")) else {
            continue;
        };
        let summary: serde_json::Value = serde_json::from_slice(&summary)?;
        let version: Version = serde_json::from_value(summary["version"].clone())?;
        if version.version == current.version
            && version.architecture == current.architecture
            && version.release < current.release
            && archives(&path, &version).iter().all(|p| p.is_file())
        {
            candidates.push((version.release, path));
        }
    }
    candidates.sort_by_key(|(release, _)| *release);
    let (_, path) = candidates.pop().ok_or("component builds need a completed older package pair of the same version; run a full build first and increase the package release")?;
    let provenance = path.join("component-inputs.json");
    if !provenance.exists() {
        return Err("baseline has no recorded component inputs; run a full build first".into());
    }
    let previous: BTreeMap<String, Vec<u8>> = serde_json::from_slice(&fs::read(provenance)?)?;
    if previous != inputs(repo)? {
        return Err("pinned inputs, Cargo dependencies, runtime contracts or packaging changed since the baseline; omit --component for a full build".into());
    }
    Ok(path)
}

fn archives(path: &Path, version: &Version) -> [PathBuf; 2] {
    ["runtime", "image"].map(|kind| {
        path.join(format!(
            "droidloom-{kind}-{}-{}-{}.pkg.tar.zst",
            version.version, version.release, version.architecture
        ))
    })
}

pub fn compile(
    source: &Path,
    work: &Path,
    destination: &Path,
    jobs: usize,
    selected: &[Component],
) -> Result<()> {
    let baseline = Path::new("/baseline");
    let summary: serde_json::Value =
        serde_json::from_slice(&fs::read(baseline.join("build-summary.json"))?)?;
    let previous: Version = serde_json::from_value(summary["version"].clone())?;
    let current = Version::read(source, Architecture::X86_64)?;
    if previous.version != current.version
        || previous.architecture != current.architecture
        || previous.release >= current.release
    {
        return Err("component baseline must be an older matching package pair".into());
    }
    let provenance: BTreeMap<String, Vec<u8>> =
        serde_json::from_slice(&fs::read(baseline.join("component-inputs.json"))?)?;
    if provenance != inputs(source)? {
        return Err("component baseline inputs do not match; perform a full build".into());
    }
    let mut command = Command::new("cargo");
    command
        .current_dir(source)
        .env("CARGO_TARGET_DIR", work.join("cargo"))
        .args(["build", "--locked", "--release", "--jobs"])
        .arg(jobs.to_string());
    for component in selected {
        command.arg("-p").arg(component.name());
    }
    run(&mut command)?;
    fs::create_dir_all(destination)?;
    for (kind, archive) in ["runtime", "image"]
        .into_iter()
        .zip(archives(baseline, &previous))
    {
        let metadata = Command::new("bsdtar")
            .args(["-xOf"])
            .arg(&archive)
            .arg(".PKGINFO")
            .output()?;
        let text = String::from_utf8(metadata.stdout)?;
        if !metadata.status.success()
            || !text
                .lines()
                .any(|s| s == format!("pkgname = droidloom-{kind}"))
            || !text
                .lines()
                .any(|s| s == format!("pkgver = {}-{}", previous.version, previous.release))
            || !text
                .lines()
                .any(|s| s == format!("arch = {}", previous.architecture))
        {
            return Err("component baseline archive metadata does not match its summary".into());
        }
        let tree = destination.join(kind);
        fs::create_dir_all(&tree)?;
        run(Command::new("bsdtar")
            .args(["-xpf"])
            .arg(archive)
            .arg("--no-same-owner")
            .arg("-C")
            .arg(tree)
            .arg("usr"))?;
    }
    for component in selected {
        for (binary, relative) in component.binaries() {
            let target = destination.join("runtime").join(relative);
            let metadata = fs::symlink_metadata(&target)?;
            if !metadata.is_file() {
                return Err(format!(
                    "baseline binary is not a regular file: {}",
                    target.display()
                )
                .into());
            }
            fs::copy(work.join("cargo/release").join(binary), &target)?;
            fs::set_permissions(target, fs::Permissions::from_mode(0o755))?;
        }
    }
    fs::write(
        destination.join("runtime/usr/share/droidloom/package.json"),
        serde_json::to_vec_pretty(&current)?,
    )?;
    println!(
        "Component payload ready; Android and unselected host files were reused without compilation."
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(root: &Path, release: u32) -> PathBuf {
        for directory in [
            "packaging/arch",
            "android/manifest",
            "protocol",
            "runtime/droidloom-contracts/src",
            "tools/droidloom-update/src",
        ] {
            fs::create_dir_all(root.join(directory)).unwrap();
        }
        for path in [
            "Cargo.toml",
            "Cargo.lock",
            "rust-toolchain.toml",
            "packaging/arch/PKGBUILD.in",
        ] {
            fs::write(root.join(path), b"fixture").unwrap();
        }
        let version = Version {
            version: "0.1.0".into(),
            release,
            architecture: "x86_64".into(),
        };
        let output = version.output(root);
        fs::create_dir_all(&output).unwrap();
        fs::write(
            output.join("build-summary.json"),
            serde_json::to_vec(&serde_json::json!({"version": version})).unwrap(),
        )
        .unwrap();
        for archive in archives(&output, &version) {
            fs::write(archive, b"archive fixture").unwrap();
        }
        record_inputs(root, &output).unwrap();
        output
    }

    #[test]
    fn baseline_requires_complete_pair_and_unchanged_inputs() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let current = Version {
            version: "0.1.0".into(),
            release: 3,
            architecture: "x86_64".into(),
        };
        assert!(baseline(root, &current).is_err());
        let old = fixture(root, 1);
        let latest = fixture(root, 2);
        assert_eq!(baseline(root, &current).unwrap(), latest);
        // Version and documentation changes do not invalidate compiled inputs.
        fs::write(root.join("packaging/arch/version.json"), b"new release").unwrap();
        fs::write(root.join("packaging/arch/README.md"), b"new documentation").unwrap();
        assert_eq!(baseline(root, &current).unwrap(), latest);
        fs::write(root.join("Cargo.lock"), b"changed dependencies").unwrap();
        assert!(
            baseline(root, &current)
                .unwrap_err()
                .to_string()
                .contains("full build")
        );
        fs::write(root.join("Cargo.lock"), b"fixture").unwrap();
        fs::remove_file(latest.join("droidloom-image-0.1.0-2-x86_64.pkg.tar.zst")).unwrap();
        assert_eq!(baseline(root, &current).unwrap(), old);
        fs::remove_file(old.join("component-inputs.json")).unwrap();
        assert!(
            baseline(root, &current)
                .unwrap_err()
                .to_string()
                .contains("full build")
        );
    }
    #[test]
    fn supervisor_rebuild_updates_both_protocol_peers() {
        let binaries = Component::Supervisor.binaries();
        assert!(binaries.contains(&("droidloomctl", "usr/bin/droidloomctl")));
        assert!(binaries.contains(&("droidloomd", "usr/bin/droidloomd")));
        assert_eq!(binaries.len(), 3);
    }

    #[test]
    fn selection_is_optional_repeatable_and_rejects_unknown_components() {
        use clap::Parser;
        assert!(super::super::Cli::try_parse_from(["package", "build"]).is_ok());
        assert!(
            super::super::Cli::try_parse_from([
                "package",
                "build",
                "--component",
                "supervisor",
                "--component",
                "wayland"
            ])
            .is_ok()
        );
        assert!(
            super::super::Cli::try_parse_from(["package", "build", "--component", "typo"]).is_err()
        );
        assert!(
            super::super::Cli::try_parse_from([
                "package",
                "build",
                "--component",
                "supervisor",
                "--clean"
            ])
            .is_err()
        );
    }
}
