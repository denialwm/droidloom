//! droidloom-base: the Android partitions that change only with the pinned
//! Android base, the ARM translator or the image policy. A release packs and
//! publishes it only when its images differ from the published ones; the
//! release's droidloom-image then depends on the base it was built against.
use super::{Result, install_tree, run};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::Command,
};

/// The partitions droidloom-base carries, beside droidloom-image's vendor.raw.img.
const IMAGES: [&str; 3] = ["product.img", "system.img", "system_ext.img"];
const IMAGE_DIR: &str = "usr/lib/droidloom/images/images";
/// Where the package repository publishes each architecture's base record.
pub(crate) const PAGES: &str = "https://denialwm.github.io/droidloom";

/// One architecture's base: its package version and its images' SHA-256.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Base {
    pub version: String,
    pub architecture: String,
    pub images: BTreeMap<String, String>,
}

/// What a build does with the base: pack a new one, or depend on `base`.
#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Decision {
    pub base: Base,
    pub packed: bool,
}

impl Decision {
    pub fn read(path: &Path) -> Result<Self> {
        Ok(serde_json::from_slice(&fs::read(path)?)?)
    }
    /// The package file a packed base is written to.
    pub fn file(&self) -> String {
        format!(
            "droidloom-base-{}-{}.pkg.tar.zst",
            self.base.version, self.base.architecture
        )
    }
}

/// Writes the published base record of `architecture` to `path`, or removes
/// `path` when none can be read: the build then packs a new base.
pub fn fetch_published(architecture: &str, path: &Path) -> Result<()> {
    let partial = path.with_extension("partial");
    let status = Command::new("curl")
        .args(["--fail", "--silent", "--show-error", "--location"])
        .args(["--max-time", "60", "--output"])
        .arg(&partial)
        .arg(format!("{PAGES}/{architecture}/base.json"))
        .status()?;
    if status.success() && serde_json::from_slice::<Base>(&fs::read(&partial)?).is_ok() {
        fs::rename(&partial, path)?;
        return Ok(());
    }
    let _ = fs::remove_file(&partial);
    match fs::remove_file(path) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error.into()),
        _ => {}
    }
    eprintln!("No published {architecture} base record; this build packs droidloom-base.");
    Ok(())
}

fn sha256(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; 1 << 20];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// A payload staged before the split carries the base images in its image
/// tree; move them, with a copy of the image's notices, into the base tree.
fn split(payload: &Path) -> Result<()> {
    let images = payload.join("image").join(IMAGE_DIR);
    if !IMAGES.iter().any(|name| images.join(name).exists()) {
        return Ok(());
    }
    let base = payload.join("base");
    fs::create_dir_all(base.join(IMAGE_DIR))?;
    for name in IMAGES {
        fs::rename(images.join(name), base.join(IMAGE_DIR).join(name))?;
    }
    let notices = base.join("usr/share/licenses/droidloom-base");
    if !notices.exists() {
        install_tree(
            &payload.join("image/usr/share/licenses/droidloom-image"),
            &notices,
        )?;
    }
    Ok(())
}

/// The payload's own release: version-release, with a phone candidate's number.
fn own_version(payload: &Path) -> Result<(String, String)> {
    let identity: serde_json::Value = serde_json::from_slice(&fs::read(
        payload.join("runtime/usr/share/droidloom/package.json"),
    )?)?;
    let version = identity["version"]
        .as_str()
        .ok_or("payload has no version")?;
    let release = identity["release"]
        .as_u64()
        .ok_or("payload has no release")?;
    let architecture = identity["architecture"]
        .as_str()
        .ok_or("payload has no architecture")?;
    let candidate = identity["candidate"]
        .as_u64()
        .map(|n| format!(".{n}"))
        .unwrap_or_default();
    Ok((
        format!("{version}-{release}{candidate}"),
        architecture.to_owned(),
    ))
}

/// Decides whether the payload's base is packed and writes the decision to
/// `payload/base.json`. A payload without base images (a component build)
/// reuses the base of `reused`, its baseline's decision.
pub fn decide(payload: &Path, published: Option<&Path>, reused: Option<&Path>) -> Result<()> {
    split(payload)?;
    let (version, architecture) = own_version(payload)?;
    let tree = payload.join("base").join(IMAGE_DIR);
    let decision = if tree.is_dir() {
        let mut images = BTreeMap::new();
        for name in IMAGES {
            images.insert(name.to_owned(), sha256(&tree.join(name))?);
        }
        let published = published
            .filter(|path| path.is_file())
            .map(|path| -> Result<Base> { Ok(serde_json::from_slice(&fs::read(path)?)?) })
            .transpose()?;
        match published {
            Some(base) if base.architecture == architecture && base.images == images => {
                println!(
                    "Android base images match published droidloom-base {}; not packing them.",
                    base.version
                );
                Decision {
                    base,
                    packed: false,
                }
            }
            _ => {
                println!("Android base images changed; packing droidloom-base {version}.");
                Decision {
                    base: Base {
                        version,
                        architecture,
                        images,
                    },
                    packed: true,
                }
            }
        }
    } else {
        let reused = reused.ok_or("payload has no base images and no base to reuse")?;
        let Decision { base, .. } = Decision::read(reused)?;
        if base.architecture != architecture {
            return Err("the reused base is for another architecture".into());
        }
        Decision {
            base,
            packed: false,
        }
    };
    fs::write(
        payload.join("base.json"),
        serde_json::to_vec_pretty(&decision)?,
    )?;
    Ok(())
}

/// The droidloom-base version the payload's image package depends on.
pub fn version(payload: &Path) -> Result<()> {
    println!(
        "{}",
        Decision::read(&payload.join("base.json"))?.base.version
    );
    Ok(())
}

/// Installs the base tree into droidloom-base's package directory, or nothing
/// when the build depends on a published base: that package is discarded.
pub fn install(payload: &Path, destination: &Path) -> Result<()> {
    if Decision::read(&payload.join("base.json"))?.packed {
        install_tree(&payload.join("base"), destination)?;
    }
    Ok(())
}

/// A stand-in droidloom-base of `base`'s version for install checks of a
/// release that did not pack one: the same package name, version and image
/// paths, with empty images. The published base was checked when it shipped.
pub fn stand_in(base: &Base, directory: &Path) -> Result<PathBuf> {
    let root = tempfile::tempdir()?;
    let images = root.path().join(IMAGE_DIR);
    fs::create_dir_all(&images)?;
    for name in IMAGES {
        fs::write(images.join(name), "")?;
    }
    fs::write(
        root.path().join(".PKGINFO"),
        format!(
            "pkgname = droidloom-base\npkgbase = droidloom\npkgver = {}\npkgdesc = Install-check stand-in for the published Android base\nurl = https://github.com/denialwm/droidloom\nbuilddate = 0\npackager = Droidloom package checks\nsize = 0\narch = {}\nlicense = GPL-3.0-or-later\n",
            base.version, base.architecture
        ),
    )?;
    let archive = directory.join(format!(
        "droidloom-base-{}-{}.pkg.tar.zst",
        base.version, base.architecture
    ));
    run(Command::new("bsdtar")
        .args(["--zstd", "--uid", "0", "--gid", "0", "-cf"])
        .arg(&archive)
        .arg("-C")
        .arg(root.path())
        .args([".PKGINFO", "usr"]))?;
    Ok(archive)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(root: &Path, images: &[(&str, &str)]) -> PathBuf {
        let payload = root.join("payload");
        fs::create_dir_all(payload.join("runtime/usr/share/droidloom")).unwrap();
        fs::write(
            payload.join("runtime/usr/share/droidloom/package.json"),
            r#"{"version":"0.1.0","release":49,"architecture":"x86_64"}"#,
        )
        .unwrap();
        let tree = payload.join("base").join(IMAGE_DIR);
        fs::create_dir_all(&tree).unwrap();
        for (name, bytes) in images {
            fs::write(tree.join(name), bytes).unwrap();
        }
        payload
    }

    fn published(root: &Path, version: &str, payload: &Path) -> PathBuf {
        let tree = payload.join("base").join(IMAGE_DIR);
        let base = Base {
            version: version.into(),
            architecture: "x86_64".into(),
            images: IMAGES
                .iter()
                .map(|name| (name.to_string(), sha256(&tree.join(name)).unwrap()))
                .collect(),
        };
        let path = root.join("published.json");
        fs::write(&path, serde_json::to_vec(&base).unwrap()).unwrap();
        path
    }

    const SAME: [(&str, &str); 3] = [
        ("product.img", "p"),
        ("system.img", "s"),
        ("system_ext.img", "e"),
    ];

    #[test]
    fn unchanged_images_depend_on_the_published_base() {
        let root = tempfile::tempdir().unwrap();
        let payload = payload(root.path(), &SAME);
        let published = published(root.path(), "0.1.0-48", &payload);
        decide(&payload, Some(&published), None).unwrap();
        let decision = Decision::read(&payload.join("base.json")).unwrap();
        assert!(!decision.packed);
        assert_eq!(decision.base.version, "0.1.0-48");
        let destination = root.path().join("package");
        fs::create_dir(&destination).unwrap();
        install(&payload, &destination).unwrap();
        assert_eq!(fs::read_dir(&destination).unwrap().count(), 0);
    }

    #[test]
    fn changed_or_unpublished_images_pack_a_base_of_this_release() {
        let root = tempfile::tempdir().unwrap();
        let payload = payload(root.path(), &SAME);
        let published = published(root.path(), "0.1.0-48", &payload);
        fs::write(payload.join("base").join(IMAGE_DIR).join("system.img"), "t").unwrap();
        decide(&payload, Some(&published), None).unwrap();
        let decision = Decision::read(&payload.join("base.json")).unwrap();
        assert!(decision.packed);
        assert_eq!(decision.base.version, "0.1.0-49");
        assert_eq!(
            decision.file(),
            "droidloom-base-0.1.0-49-x86_64.pkg.tar.zst"
        );
        decide(&payload, None, None).unwrap();
        assert!(Decision::read(&payload.join("base.json")).unwrap().packed);
        let destination = root.path().join("package");
        install(&payload, &destination).unwrap();
        assert!(destination.join(IMAGE_DIR).join("system.img").is_file());
    }

    #[test]
    fn a_payload_from_before_the_split_moves_its_images_into_the_base() {
        let root = tempfile::tempdir().unwrap();
        let payload = payload(root.path(), &[]);
        fs::remove_dir_all(payload.join("base")).unwrap();
        let images = payload.join("image").join(IMAGE_DIR);
        fs::create_dir_all(&images).unwrap();
        for (name, bytes) in SAME.iter().chain(&[("vendor.raw.img", "v")]) {
            fs::write(images.join(name), bytes).unwrap();
        }
        let notices = payload.join("image/usr/share/licenses/droidloom-image");
        fs::create_dir_all(&notices).unwrap();
        fs::write(notices.join("LICENSE"), "notice").unwrap();
        decide(&payload, None, None).unwrap();
        assert!(images.join("vendor.raw.img").is_file());
        assert!(!images.join("system.img").exists());
        assert!(
            payload
                .join("base")
                .join(IMAGE_DIR)
                .join("system.img")
                .is_file()
        );
        assert!(
            payload
                .join("base/usr/share/licenses/droidloom-base/LICENSE")
                .is_file()
        );
    }

    #[test]
    fn a_component_build_reuses_its_baseline_base() {
        let root = tempfile::tempdir().unwrap();
        let payload = payload(root.path(), &[]);
        fs::remove_dir_all(payload.join("base")).unwrap();
        assert!(decide(&payload, None, None).is_err());
        let reused = root.path().join("baseline.json");
        let decision = Decision {
            base: Base {
                version: "0.1.0-47".into(),
                architecture: "x86_64".into(),
                images: BTreeMap::new(),
            },
            packed: true,
        };
        fs::write(&reused, serde_json::to_vec(&decision).unwrap()).unwrap();
        decide(&payload, None, Some(&reused)).unwrap();
        let decided = Decision::read(&payload.join("base.json")).unwrap();
        assert!(!decided.packed);
        assert_eq!(decided.base.version, "0.1.0-47");
    }
}
