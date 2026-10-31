//! One complete, verified Droidloom build and activation transaction.
mod android;
mod assemble;
mod bundle;
mod dependencies;
mod image_policy;
mod install;
mod licenses;
mod native_bridge;
mod package;
mod python;
mod system_server;
mod util;
use clap::{Parser, Subcommand};
use std::{fs, path::PathBuf, process::Command};
use util::*;
#[derive(Parser)]
#[command(
    version,
    about = "Build, verify, install and restart all Droidloom components"
)]
struct Args {
    /// Droidloom checkout (remembered automatically after installation).
    #[arg(long, global = true)]
    source: Option<PathBuf>,
    /// Delete this updater's build outputs before compiling.
    #[arg(long)]
    clean: bool,
    #[arg(long, hide = true)]
    bootstrapped: bool,
    /// Build and verify a complete bundle without installing it.
    #[arg(long)]
    build_only: bool,
    /// Maximum parallel jobs, capped at available CPUs minus two.
    #[arg(short = 'j', long)]
    jobs: Option<usize>,
    #[command(subcommand)]
    command: Option<Action>,
}
#[derive(Subcommand)]
enum Action {
    /// Seal already cross-built outputs for a remote host, without activation.
    BundleStage {
        #[arg(long)]
        work: PathBuf,
        #[arg(long)]
        host: PathBuf,
        #[arg(long)]
        base: PathBuf,
        #[arg(long, value_parser = ["droidloom_x86_64", "droidloom_arm64"])]
        product: String,
        /// Tracked profile in packaging/devices; supplies the cell and user.
        #[arg(long, conflicts_with_all = ["cell_spec", "user"], required_unless_present_all = ["cell_spec", "user"])]
        device: Option<String>,
        #[arg(long, requires = "user")]
        cell_spec: Option<PathBuf>,
        #[arg(long, requires = "cell_spec")]
        user: Option<String>,
        #[arg(long)]
        destination: PathBuf,
    },
    /// Cross-compile the Linux host programs for an ARM64 device.
    HostBuild {
        #[arg(long)]
        work: PathBuf,
        /// ARM64 root filesystem providing the target's native libraries.
        #[arg(long)]
        sysroot: PathBuf,
    },
    /// Cross-build the complete Android userspace closure, including the vendor image.
    AndroidBuild {
        #[arg(long)]
        work: PathBuf,
        #[arg(long, default_value = "droidloom_x86_64", value_parser = ["droidloom_x86_64", "droidloom_arm64"])]
        product: String,
    },
    /// Compile system_server and its Rust policy in a prepared Android workspace.
    FrameworkBuild {
        #[arg(long)]
        work: Option<PathBuf>,
        #[arg(long, default_value = "droidloom_x86_64", value_parser = ["droidloom_x86_64", "droidloom_arm64"])]
        product: String,
        /// Rebuild task, graphics and input peers alongside the framework pair.
        #[arg(long, conflicts_with = "surfaceflinger_only")]
        tasks: bool,
        /// Rebuild only SurfaceFlinger and its native dependency closure.
        #[arg(long)]
        surfaceflinger_only: bool,
    },
    /// Compile the audio HAL APEX and client library in a prepared Android workspace.
    AudioBuild {
        #[arg(long)]
        work: Option<PathBuf>,
        #[arg(long, default_value = "droidloom_x86_64", value_parser = ["droidloom_x86_64", "droidloom_arm64"])]
        product: String,
    },
    #[command(hide = true)]
    NativeBridgeImage {
        #[arg(long)]
        image: PathBuf,
        #[arg(long)]
        destination: PathBuf,
        #[arg(long)]
        product: PathBuf,
        #[arg(long)]
        source_lock: PathBuf,
    },
    /// Rebuild the ARM64 translator in an already prepared Android workspace.
    NativeBridgeBuild {
        /// Use an existing package build workspace (containing aosp-source).
        #[arg(long)]
        work: Option<PathBuf>,
        /// Also build Teto's ARM64 host regression suite.
        #[arg(long)]
        tests: bool,
    },
    #[command(hide = true)]
    PruneDesktopImage {
        #[arg(long)]
        image: PathBuf,
        #[arg(long)]
        destination: PathBuf,
        #[arg(long)]
        vendor_properties: PathBuf,
    },
    /// Compile and stage a portable pacman payload; does not install or start services.
    PackageStage {
        #[arg(long)]
        work: PathBuf,
        #[arg(long)]
        destination: PathBuf,
        #[arg(long)]
        clean: bool,
        #[arg(long)]
        jobs: usize,
        #[arg(long, default_value = "droidloom_x86_64", value_parser = ["droidloom_x86_64", "droidloom_arm64"])]
        product: String,
        /// ARM64 root filesystem the cross-compiled host programs link against.
        #[arg(long, required_if_eq("product", "droidloom_arm64"))]
        sysroot: Option<PathBuf>,
    },
    /// Show the complete build plan without changing anything.
    Plan,
    /// Verify an already built bundle.
    Verify { payload: PathBuf },
    /// Privileged activation worker; normally invoked automatically through polkit.
    #[command(hide = true)]
    Apply {
        payload: PathBuf,
        #[arg(long)]
        uid: u32,
        /// Install the verified release while leaving Android stopped.
        #[arg(long)]
        no_start: bool,
    },
    #[command(hide = true)]
    Recover {
        #[arg(long)]
        uid: u32,
    },
}
fn repository(explicit: Option<PathBuf>) -> Result<PathBuf> {
    let path = if let Some(p) = explicit {
        p
    } else {
        let current = std::env::current_dir()?;
        if let Some(p) = current
            .ancestors()
            .find(|p| p.join("tools/droidloom-update/Cargo.toml").exists())
        {
            p.to_owned()
        } else {
            let config: serde_json::Value = serde_json::from_slice(
                &fs::read("/usr/lib/droidloom/active/etc/droidloom/update.json")
                    .map_err(|_| "run the initial update inside the Droidloom checkout")?,
            )?;
            PathBuf::from(
                config["source"]
                    .as_str()
                    .ok_or("invalid updater configuration")?,
            )
        }
    }
    .canonicalize()?;
    if !path.join("tools/droidloom-update/Cargo.toml").is_file() {
        return fail("not a Droidloom checkout");
    }
    Ok(path)
}
fn execute(args: Args) -> Result<()> {
    match &args.command {
        Some(Action::BundleStage {
            work,
            host,
            base,
            product,
            device,
            cell_spec,
            user,
            destination,
        }) => {
            let repo = repository(args.source.clone())?;
            let (cell, user) = match (device, cell_spec, user) {
                (Some(device), _, _) => assemble::device_cell(&repo, product, device)?,
                (None, Some(spec), Some(user)) => {
                    (serde_json::from_slice(&fs::read(spec)?)?, user.clone())
                }
                _ => return fail("bundle-stage needs --device, or --cell-spec with --user"),
            };
            return assemble::stage_cross(
                &repo,
                work,
                host,
                base,
                assemble::CrossTarget {
                    product,
                    cell: &cell,
                    user: &user,
                },
                destination,
            );
        }
        Some(Action::HostBuild { work, sysroot }) => {
            let capacity = std::thread::available_parallelism()
                .map(|n| n.get().saturating_sub(2).max(1))
                .unwrap_or(1);
            let jobs = args.jobs.unwrap_or(capacity).min(capacity);
            assemble::host_build(&repository(args.source.clone())?, work, sysroot, jobs)?;
            return Ok(());
        }
        Some(Action::NativeBridgeImage {
            image,
            destination,
            product,
            source_lock,
        }) => {
            return native_bridge::derive_image(image, destination, product, source_lock);
        }
        Some(Action::PruneDesktopImage {
            image,
            destination,
            vendor_properties,
        }) => {
            return image_policy::prune(image, destination, vendor_properties);
        }
        Some(Action::PackageStage {
            work,
            destination,
            clean,
            jobs,
            product,
            sysroot,
        }) => {
            return package::stage(
                &repository(args.source.clone())?,
                work,
                destination,
                *clean,
                *jobs,
                product,
                sysroot.as_deref(),
            );
        }
        Some(Action::Recover { uid }) => return install::recover(*uid),
        Some(Action::Apply {
            payload,
            uid,
            no_start,
        }) => {
            return install::apply(&payload.canonicalize()?, *uid, !no_start);
        }
        Some(Action::Verify { payload }) => {
            let m = bundle::verify(payload)?;
            println!(
                "Verified {} bundle, input ABI {}, source {}",
                m.architecture, m.input_abi, m.source_identity
            );
            return Ok(());
        }
        _ => {}
    }
    if args.command.is_none()
        && !args.build_only
        && std::path::Path::new("/usr/share/droidloom/package.json").exists()
    {
        return fail(
            "Droidloom is managed by pacman. Build packages with cargo run --locked -j 1 -p droidloom-package -- build, then install the printed package pair with sudo pacman -U. Sudo is needed to replace package-owned system files and update pacman's database.",
        );
    }
    let repo = repository(args.source)?;
    let arch = std::env::consts::ARCH;
    if arch != "x86_64" {
        return fail("the source builder currently requires an x86_64 Linux build host");
    }
    let product = match &args.command {
        Some(Action::AndroidBuild { product, .. })
        | Some(Action::AudioBuild { product, .. })
        | Some(Action::FrameworkBuild { product, .. }) => product.as_str(),
        _ => "droidloom_x86_64",
    };
    let prepared_work = match &args.command {
        Some(Action::AndroidBuild { work, .. }) => Some(work.canonicalize()?),
        Some(Action::NativeBridgeBuild {
            work: Some(work), ..
        })
        | Some(Action::AudioBuild {
            work: Some(work), ..
        })
        | Some(Action::FrameworkBuild {
            work: Some(work), ..
        }) => Some(work.canonicalize()?),
        _ => None,
    };
    // Keep every local build mode in the package builder's persistent cache.
    // A caller may still select an explicitly prepared workspace for a focused
    // native-bridge build (for example, /build inside the package container).
    let work = prepared_work
        .clone()
        .unwrap_or_else(|| repo.join(".work/arch/build"));
    let source = work.join("aosp-source");
    let out = work.join("android-out");
    let cargo = work.join("cargo");
    if matches!(args.command, Some(Action::AudioBuild { .. })) && !source.is_dir() {
        return fail("audio-build requires an already prepared workspace containing aosp-source");
    }
    if matches!(args.command, Some(Action::FrameworkBuild { .. })) && !source.is_dir() {
        return fail(
            "framework-build requires an already prepared workspace containing aosp-source",
        );
    }
    if matches!(args.command, Some(Action::Plan)) {
        println!(
            "Source: {}\nCache: {}\nAndroid product: {product}\nHost: supervisor, ctl, daemon, Wayland, applications, doctor, updater\nAndroid: {}\nPinned inputs: system, system_ext, product, Mesa\nActivation: verify -> stage -> stop Droidloom -> switch complete release -> start -> check Android catalog\nFailure: restore previous release\nBoot autostart: disabled",
            repo.display(),
            work.display(),
            android::TARGETS.join(", ")
        );
        return Ok(());
    }
    let uid = unsafe { libc::getuid() };
    if uid == 0 {
        return fail(
            "run as the desktop user; administrator authentication is requested only for installation",
        );
    }
    let capacity = std::thread::available_parallelism()
        .map(|n| n.get().saturating_sub(2).max(1))
        .unwrap_or(1);
    let jobs = args.jobs.unwrap_or(capacity).min(capacity);
    if jobs == 0 {
        return fail("jobs must be positive");
    }
    let _lock = Lock::acquire(&work.join("package-build.lock"))?;
    let _source_lock = Lock::acquire(&work.join("aosp-source-update.lock"))?;
    // A prepared package workspace runs inside the rootless builder, which
    // supplies build tools and does not run host systemd or install services.
    if prepared_work.is_none()
        && !matches!(
            args.command,
            Some(Action::AudioBuild { .. } | Action::FrameworkBuild { .. })
        )
    {
        dependencies::ensure()?;
    }
    if work.join("source-projection.json").exists() {
        android::recover(&work.join("source-projection.json"), &source)?;
    }
    if matches!(args.command, Some(Action::AndroidBuild { .. })) {
        if product == "droidloom_arm64" {
            assemble::mesa_tools(&work, jobs, true)?;
        }
        return android::build(&repo, &source, &out, &work, product, jobs);
    }
    if matches!(args.command, Some(Action::FrameworkBuild { .. })) {
        let targets: &[&str] = if matches!(
            args.command,
            Some(Action::FrameworkBuild {
                surfaceflinger_only: true,
                ..
            })
        ) {
            &["surfaceflinger"]
        } else if matches!(
            args.command,
            Some(Action::FrameworkBuild { tasks: true, .. })
        ) {
            android::TASK_BACKEND_TARGETS
        } else {
            &["services", "libinputflinger"]
        };
        return android::build_targets(&repo, &source, &out, &work, product, jobs, targets);
    }
    if matches!(args.command, Some(Action::AudioBuild { .. })) {
        return android::build_targets(
            &repo,
            &source,
            &out,
            &work,
            product,
            jobs,
            android::AUDIO_TARGETS,
        );
    }
    if let Some(Action::NativeBridgeBuild { tests, .. }) = args.command {
        return android::build_targets(
            &repo,
            &source,
            &out,
            &work,
            product,
            jobs,
            if tests {
                &["droidloom-native-bridge", "berberis_arm64_host_tests"]
            } else {
                &["droidloom-native-bridge"]
            },
        );
    }
    if args.clean {
        for path in [&out, &cargo, &work.join("mesa-tools"), &work.join("ccache")] {
            if path.exists() {
                fs::remove_dir_all(path)?;
            }
        }
    }
    let identity = bundle::source_identity(&repo)?;
    eprintln!("Building complete Droidloom release from {identity}");
    eprintln!("Compiler budget: {jobs} parallel jobs; two logical CPUs reserved");
    let mut host_build = Command::new("cargo");
    host_build
        .current_dir(&repo)
        .env("CARGO_TARGET_DIR", &cargo)
        .args(["build", "--locked", "--release"])
        .arg(format!("-j{jobs}"));
    for package in assemble::HOST_PACKAGES {
        host_build.arg("-p").arg(package);
    }
    run_build(&mut host_build)?;
    let fresh = cargo.join("release/droidloom-update");
    if !args.bootstrapped {
        // The updater is part of the build too. Use its new orchestration in this update.
        // Release locks before exec so the replacement can acquire them normally.
        drop(_source_lock);
        drop(_lock);
        use std::os::unix::process::CommandExt;
        let mut command = Command::new(fresh);
        command
            .arg("--bootstrapped")
            .arg("--source")
            .arg(&repo)
            .arg("--jobs")
            .arg(jobs.to_string());
        if args.build_only {
            command.arg("--build-only");
        }
        // --clean was already applied; repeating it would cause an endless bootstrap.
        return Err(command.exec().into());
    }
    if !args.build_only && std::path::Path::new("/usr/lib/droidloom/transaction.json").exists() {
        run(Command::new("pkexec")
            .arg(&fresh)
            .arg("recover")
            .arg("--uid")
            .arg(uid.to_string()))?;
    }
    let base = assemble::prepare_inputs(&repo, &work, arch)?;
    python::prepare(&repo, &work)?;
    assemble::mesa_tools(&work, jobs, false)?;
    android::build(&repo, &source, &out, &work, product, jobs)?;
    let staging = tempfile::Builder::new()
        .prefix("bundle-")
        .tempdir_in(&work)?;
    assemble::assemble(
        &repo,
        &work,
        &base,
        &out.join("target/product").join(product),
        &cargo.join("release"),
        staging.path(),
        uid,
    )?;
    let id = bundle::seal(staging.path(), identity)?;
    bundle::verify(staging.path())?;
    let payload = work.join(format!("release-{id}"));
    if payload.exists() {
        bundle::verify(&payload)?;
    } else {
        fs::rename(staging.path(), &payload)?;
    }
    println!("Verified bundle: {}", payload.display());
    if !args.build_only {
        run(Command::new("pkexec")
            .arg(payload.join("usr/bin/droidloom-update"))
            .arg("apply")
            .arg(&payload)
            .arg("--uid")
            .arg(uid.to_string()))?;
    }
    Ok(())
}
fn main() {
    if let Err(error) = execute(Args::parse()) {
        eprintln!("Droidloom update failed: {error}");
        std::process::exit(1);
    }
}
