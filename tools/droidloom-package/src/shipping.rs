//! Small release interface shared by local runs and GitHub Actions.
use super::{
    Architecture, Result, Version,
    base::{Decision, PAGES},
    repository, run,
};
use clap::Subcommand;
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

const REPOSITORY: &str = "denialwm/droidloom";
const RELEASE: &str = "packages";

#[derive(Subcommand)]
pub enum Action {
    /// Write the Pages databases and installer from the current package pairs.
    Prepare,
    /// Upload immutable package archives before deploying the Pages database.
    Publish,
}

/// One architecture's packages: runtime and image, then the base when this
/// release packed one, with the build's base decision.
struct Release {
    packages: Vec<PathBuf>,
    decision: Decision,
}

fn checked(path: PathBuf) -> Result<PathBuf> {
    if !path.is_file() {
        return Err(format!("missing {}", path.display()).into());
    }
    if fs::metadata(&path)?.len() >= 2 * 1024 * 1024 * 1024 {
        return Err("GitHub Release assets must be smaller than 2 GiB".into());
    }
    Ok(path)
}

fn packages(repo: &Path, version: &Version) -> Result<Release> {
    let output = version.output(repo);
    let mut packages = ["runtime", "image"]
        .iter()
        .map(|component| {
            checked(output.join(format!(
                "droidloom-{component}-{}-{}.pkg.tar.zst",
                version.directory(),
                version.architecture
            )))
        })
        .collect::<Result<Vec<_>>>()?;
    let decision = Decision::read(&output.join("base.json"))?;
    if decision.base.architecture != version.architecture {
        return Err("the base decision names another architecture".into());
    }
    if decision.packed {
        packages.push(checked(output.join(decision.file()))?);
    }
    Ok(Release { packages, decision })
}

/// Starts a database from the published one when this release reuses the
/// published base: its entry stays, and repo-add replaces runtime and image.
fn published_database(database: &Path, decision: &Decision) -> Result<()> {
    for name in ["droidloom.db.tar.gz", "droidloom.files.tar.gz"] {
        run(Command::new("curl")
            .args(["--fail", "--silent", "--show-error", "--location"])
            .args(["--max-time", "60", "--output"])
            .arg(database.join(name))
            .arg(format!("{PAGES}/{}/{name}", decision.base.architecture)))?;
    }
    let entries = Command::new("bsdtar")
        .arg("-tf")
        .arg(database.join("droidloom.db.tar.gz"))
        .output()?;
    let entry = format!("droidloom-base-{}/", decision.base.version);
    if !entries.status.success()
        || !String::from_utf8(entries.stdout)?
            .lines()
            .any(|line| line == entry)
    {
        return Err(format!(
            "the published {} database does not list droidloom-base {}",
            decision.base.architecture, decision.base.version
        )
        .into());
    }
    Ok(())
}

fn gh() -> Command {
    let mut command = Command::new("gh");
    command.env("GH_REPO", REPOSITORY);
    command
}

pub fn execute(action: Action) -> Result<()> {
    let repo = repository(None)?;
    // Every architecture ships the same release, or none does.
    let mut releases = Vec::new();
    for architecture in Architecture::ALL {
        let version = Version::read(&repo, architecture)?;
        releases.push((packages(&repo, &version)?, version));
    }
    let version = &releases[0].1;
    let packages: Vec<_> = releases
        .iter()
        .flat_map(|(release, _)| &release.packages)
        .collect();
    let site = repo.join("dist/pages");
    match action {
        Action::Prepare => {
            if site.exists() {
                fs::remove_dir_all(&site)?;
            }
            for (release, version) in &releases {
                let database = site.join(&version.architecture);
                fs::create_dir_all(&database)?;
                if !release.decision.packed {
                    published_database(&database, &release.decision)?;
                }
                run(Command::new("repo-add")
                    .arg(database.join("droidloom.db.tar.gz"))
                    .args(&release.packages))?;
                // The next release compares its base images with this record.
                fs::write(
                    database.join("base.json"),
                    serde_json::to_vec_pretty(&release.decision.base)?,
                )?;
                // Pages artifacts cannot contain symlinks. Publish the actual bytes.
                for name in ["droidloom.db", "droidloom.files"] {
                    fs::remove_file(database.join(name))?;
                    fs::copy(database.join(format!("{name}.tar.gz")), database.join(name))?;
                }
            }
            fs::copy(repo.join("install.sh"), site.join("install.sh"))?;
            fs::write(site.join(".nojekyll"), "")?;
            fs::write(
                site.join("index.html"),
                format!(
                    "<!doctype html><html lang=\"en\"><meta charset=\"utf-8\"><title>Droidloom packages</title>\n<h1>Droidloom {}</h1><p>Arch Linux on x86_64, Arch Linux ARM on aarch64. NVIDIA graphics are not supported yet; support is planned and under development, and <a href=\"https://denialwm.org/support.html\">supporting the project</a> helps it move forward.</p>\n<p>Download and review <a href=\"install.sh\">install.sh</a>, then run <code>sh install.sh</code> to install or update.</p>\n<p>Packages are unsigned and delivered over HTTPS. The installer configures this policy only for the Droidloom repository.</p>\n<p><a href=\"https://github.com/denialwm/droidloom/blob/main/docs/INSTALL.md\">Installation guide</a> · <a href=\"https://github.com/denialwm/droidloom/releases/latest\">Packages and source</a></p></html>\n",
                    version.directory()
                ),
            )?;
        }
        Action::Publish => {
            let metadata = gh()
                .args(["api", "repos/denialwm/droidloom/releases/tags/packages"])
                .output()?;
            let release: serde_json::Value = if metadata.status.success() {
                serde_json::from_slice(&metadata.stdout)?
            } else {
                // A failed create remains an error (including authentication failures).
                run(gh().args(["release", "create", RELEASE, "--title", "Droidloom pacman packages", "--notes", "Pacman package archives and matching source snapshots. Install using https://denialwm.github.io/droidloom/install.sh. Older archives are retained for clients with cached databases."]))?;
                serde_json::json!({"assets": []})
            };
            let source = repo
                .join("dist")
                .join(format!("droidloom-source-{}.tar.gz", version.directory()));
            run(Command::new("git")
                .current_dir(&repo)
                .args(["archive", "--format=tar.gz", "--prefix=droidloom/"])
                .arg("-o")
                .arg(&source)
                .arg("HEAD"))?;
            // Preflight every asset before any upload. Same-version rebuilds may
            // resume only when their bytes match; never replace published files.
            let mut pending = Vec::new();
            for path in packages.into_iter().chain(std::iter::once(&source)) {
                let name = path
                    .file_name()
                    .unwrap()
                    .to_str()
                    .ok_or("non-UTF8 filename")?;
                if release["assets"]
                    .as_array()
                    .is_some_and(|assets| assets.iter().any(|a| a["name"].as_str() == Some(name)))
                {
                    let temporary = tempfile::tempdir()?;
                    run(gh()
                        .args(["release", "download", RELEASE, "--pattern", name, "--dir"])
                        .arg(temporary.path()))?;
                    let status = Command::new("cmp")
                        .arg("--silent")
                        .arg(path)
                        .arg(temporary.path().join(name))
                        .status()?;
                    if !status.success() {
                        return Err(format!("{name} is already published with different bytes; increase packaging/arch/version.json release").into());
                    }
                } else {
                    pending.push(path);
                }
            }
            for path in pending {
                run(gh().args(["release", "upload", RELEASE]).arg(path))?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn publication_requires_runtime_image_and_a_packed_base_and_excludes_addons() {
        let temp = tempfile::tempdir().unwrap();
        let version = Version {
            version: "1.2.3".into(),
            release: 4,
            architecture: "x86_64".into(),
        };
        let output = temp.path().join("dist/arch/1.2.3-4/x86_64");
        fs::create_dir_all(&output).unwrap();
        let decision = |packed: bool| Decision {
            base: crate::base::Base {
                version: if packed { "1.2.3-4" } else { "1.2.3-2" }.into(),
                architecture: "x86_64".into(),
                images: Default::default(),
            },
            packed,
        };
        fs::write(
            output.join("base.json"),
            serde_json::to_vec(&decision(false)).unwrap(),
        )
        .unwrap();
        fs::write(
            output.join("droidloom-runtime-1.2.3-4-x86_64.pkg.tar.zst"),
            "runtime",
        )
        .unwrap();
        assert!(packages(temp.path(), &version).is_err());
        fs::write(
            output.join("droidloom-image-1.2.3-4-x86_64.pkg.tar.zst"),
            "image",
        )
        .unwrap();
        fs::write(output.join("droidloom-gapps-1-x86_64.pkg.tar.zst"), "addon").unwrap();
        assert_eq!(packages(temp.path(), &version).unwrap().packages.len(), 2);
        fs::write(
            output.join("base.json"),
            serde_json::to_vec(&decision(true)).unwrap(),
        )
        .unwrap();
        assert!(packages(temp.path(), &version).is_err());
        fs::write(
            output.join("droidloom-base-1.2.3-4-x86_64.pkg.tar.zst"),
            "base",
        )
        .unwrap();
        assert_eq!(packages(temp.path(), &version).unwrap().packages.len(), 3);
    }
}
