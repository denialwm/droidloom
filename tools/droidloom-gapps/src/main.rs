//! Offline optional-package preparation. Never installs or starts Droidloom.
mod archive;
mod erofs;
mod ext4;
mod import;
mod package;
mod util;
mod xml;

use clap::{Parser, Subcommand};
use droidloom_contracts::{
    Architecture,
    gapps::{ADDON_ROLES, BASE_ROLES, FileDigest, Manifest, SDK},
};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};
use util::*;

#[derive(Parser)]
#[command(
    version,
    about = "Build optional Droidloom Google-app images and packages without installation"
)]
struct Cli {
    #[command(subcommand)]
    action: Action,
}

#[derive(Subcommand)]
enum Action {
    /// Display archive identity and metadata without executing installer scripts.
    Inspect { archive: PathBuf },
    /// Import APKs and derive a verified add-on from ext4 or EROFS base images.
    Build {
        #[arg(long)]
        archive: PathBuf,
        /// Expected SHA-256 of the explicitly selected local archive.
        #[arg(long)]
        sha256: String,
        /// Base directory containing images/system.img, system_ext.img and product.img.
        #[arg(long)]
        base: PathBuf,
        /// Optional actual system_ext input when it was derived separately.
        #[arg(long)]
        system_ext: Option<PathBuf>,
        /// New output directory, published only after all image checks pass.
        #[arg(long)]
        output: PathBuf,
        #[arg(long, default_value = "aapt2")]
        aapt2: PathBuf,
        #[arg(long, default_value = "apksigner")]
        apksigner: PathBuf,
        /// Native Play Services APK, verified against the archive's signing certificate.
        #[arg(long)]
        play_services_apk: Option<PathBuf>,
        /// Native Play Store APK, verified against the archive's signing certificate.
        #[arg(long)]
        play_store_apk: Option<PathBuf>,
        /// Include Google Contacts and Calendar sync adapters.
        #[arg(long)]
        sync_adapters: bool,
    },
    /// Internal rootless EROFS assembly worker.
    #[command(hide = true)]
    DeriveErofs {
        source: PathBuf,
        destination: PathBuf,
        additions: PathBuf,
        tree: PathBuf,
    },
    /// Verify an add-on against the exact installed base without starting Android.
    Verify {
        #[arg(long)]
        addon: PathBuf,
        #[arg(long)]
        base: PathBuf,
    },
    /// Produce a pacman archive from a verified add-on; performs no installation.
    Package {
        #[arg(long)]
        addon: PathBuf,
        #[arg(long)]
        output: PathBuf,
        /// Package version, for example 4.9.20260513.
        #[arg(long)]
        version: String,
        /// Package data for a separately maintained ARM developer runtime.
        #[arg(long)]
        standalone: bool,
    },
}

fn main() {
    if let Err(error) = execute() {
        eprintln!("droidloom-gapps: {error}");
        std::process::exit(1);
    }
}

fn execute() -> Result<()> {
    match Cli::parse().action {
        Action::Inspect { archive } => {
            if fs::metadata(&archive)?.len() > archive::MAX_ARCHIVE {
                return Err("archive exceeds 512 MiB".into());
            }
            let digest = FileDigest::read(&archive)?;
            if digest.size > archive::MAX_ARCHIVE {
                return Err("archive exceeds 512 MiB".into());
            }
            let members = archive::list(&archive)?;
            if members.get("module.prop") != Some(&false) {
                return Err("missing module.prop".into());
            }
            let props = archive::properties(&String::from_utf8(archive::member(
                &archive,
                "module.prop",
                16_384,
            )?)?)?;
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &serde_json::json!({"archive": digest, "module": props})
                )?
            );
        }
        Action::Build {
            archive,
            sha256,
            base,
            system_ext,
            output,
            aapt2,
            apksigner,
            play_services_apk,
            play_store_apk,
            sync_adapters,
        } => {
            build(
                &archive,
                &sha256,
                &base,
                system_ext.as_deref(),
                &output,
                &aapt2,
                &apksigner,
                play_services_apk.as_deref(),
                play_store_apk.as_deref(),
                sync_adapters,
            )?;
        }
        Action::DeriveErofs {
            source,
            destination,
            additions,
            tree,
        } => {
            erofs::derive_worker(
                &source,
                &destination,
                &serde_json::from_slice(&fs::read(additions)?)?,
                &tree,
            )?;
        }
        Action::Verify { addon, base } => {
            let manifest = Manifest::load(&addon.join("manifest.json"))?;
            manifest.verify_images(&base, &addon, manifest.architecture)?;
            println!(
                "Verified {:?} API {} add-on against {}",
                manifest.architecture,
                manifest.sdk,
                base.display()
            );
        }
        Action::Package {
            addon,
            output,
            version,
            standalone,
        } => {
            package::build(&addon, &output, &version, standalone)?;
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn build(
    archive: &Path,
    sha256: &str,
    base: &Path,
    system_ext: Option<&Path>,
    output: &Path,
    aapt2: &Path,
    apksigner: &Path,
    play_services_apk: Option<&Path>,
    play_store_apk: Option<&Path>,
    sync_adapters: bool,
) -> Result<()> {
    new_output(output)?;
    let work = tempfile::Builder::new()
        .prefix("gapps-build-")
        .tempdir_in(output.parent().ok_or("output has no parent")?)?;
    let inputs: BTreeMap<String, PathBuf> = BASE_ROLES
        .into_iter()
        .map(|role| {
            let path = if role == "system_ext" {
                system_ext.map(Path::to_owned)
            } else {
                None
            };
            (
                role.into(),
                path.unwrap_or_else(|| base.join("images").join(format!("{role}.img"))),
            )
        })
        .collect();
    for image in inputs.values() {
        if !ext4::is_ext4(image)? && !erofs::is_erofs(image)? {
            return Err(format!(
                "{} is not a supported raw ext4 or EROFS image",
                image.display()
            )
            .into());
        }
    }
    let props = archive::properties(&String::from_utf8(read_image_file(
        &inputs["system"],
        "/system/build.prop",
        65_536,
    )?)?)?;
    if props.get("ro.build.version.sdk").map(String::as_str) != Some("37") {
        return Err("base system must be Android 17 / API 37".into());
    }
    let architecture = match props.get("ro.product.cpu.abi").map(String::as_str) {
        Some("arm64-v8a") => Architecture::Aarch64,
        Some("x86_64") => Architecture::X86_64,
        _ => return Err("base system has an unsupported native ABI".into()),
    };
    let build_id = props
        .get("ro.build.id")
        .ok_or("base system has no build ID")?;
    let incremental = props
        .get("ro.build.version.incremental")
        .ok_or("base system has no build revision")?;
    for role in ADDON_ROLES {
        let partition = archive::properties(&String::from_utf8(read_image_file(
            &inputs[role],
            "/etc/build.prop",
            65_536,
        )?)?)?;
        if partition
            .get(&format!("ro.{role}.build.version.sdk"))
            .map(String::as_str)
            != Some("37")
            || partition.get(&format!("ro.{role}.build.id")) != Some(build_id)
            || partition.get(&format!("ro.{role}.build.version.incremental")) != Some(incremental)
            || partition
                .get("ro.product.cpu.abi")
                .is_some_and(|abi| Some(abi) != props.get("ro.product.cpu.abi"))
        {
            return Err(format!("{role} does not match the base Android build/SDK/ABI").into());
        }
    }
    let imported = import::prepare(
        archive,
        sha256,
        work.path(),
        aapt2,
        apksigner,
        sync_adapters,
        architecture,
        play_services_apk,
        play_store_apk,
    )?;
    if imported.architecture != architecture {
        return Err("LiteGapps and base image architectures differ".into());
    }
    let stage = work.path().join("addon");
    fs::create_dir_all(stage.join("images"))?;
    let base_images = inputs
        .iter()
        .map(|(role, image)| Ok((role.clone(), FileDigest::read(image)?)))
        .collect::<Result<_>>()?;
    let mut images = BTreeMap::new();
    for role in ADDON_ROLES {
        let prefix = format!("{role}/");
        let files = imported
            .files
            .iter()
            .filter_map(|(path, digest)| {
                path.strip_prefix(&prefix)
                    .map(|path| (path.to_owned(), digest.clone()))
            })
            .collect();
        let destination = stage.join("images").join(format!("{role}.img"));
        let derive = if erofs::is_erofs(&inputs[role])? {
            erofs::derive
        } else {
            ext4::derive
        };
        derive(
            &inputs[role],
            &destination,
            &files,
            &imported.tree.join(role),
        )?;
        images.insert(role.into(), FileDigest::read(&destination)?);
    }
    let manifest = Manifest {
        schema_version: 1,
        architecture,
        sdk: SDK,
        upstream_version: imported.version,
        archive: imported.archive,
        base_images,
        images,
        apks: imported.apks,
        sync_adapters,
    };
    manifest.validate()?;
    for (role, image) in &inputs {
        if FileDigest::read(image)? != manifest.base_images[role] {
            return Err("base inputs changed during build".into());
        }
    }
    write(
        &stage.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )?;
    write(
        &stage.join("import-report.json"),
        serde_json::to_vec_pretty(
            &serde_json::json!({"files": imported.files, "excluded": imported.excluded, "native_apk_sources": imported.native_apk_sources}),
        )?,
    )?;
    write(&stage.join("LICENSE.LiteGapps"), imported.license)?;
    write(
        &stage.join("NOTICE"),
        "Google APKs remain under their own terms. The LiteGapps installer license does not license Google's APKs. This locally imported add-on provides no Google certification or Play Integrity guarantee.\n",
    )?;
    fs::rename(&stage, output)?;
    println!("Verified optional add-on: {}", output.display());
    println!(
        "Activation requires the updated supervisor and fresh Android data. Nothing was installed or started."
    );
    Ok(())
}

fn read_image_file(image: &Path, path: &str, maximum: u64) -> Result<Vec<u8>> {
    if erofs::is_erofs(image)? {
        capture(
            std::process::Command::new("dump.erofs")
                .arg("--cat")
                .arg(format!("--path={path}"))
                .arg(image),
            maximum,
        )
    } else {
        ext4::read_file(image, path, maximum)
    }
}
