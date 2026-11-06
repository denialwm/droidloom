//! Compile one portable package payload without activating a local installation.
use crate::{android, assemble, licenses, python, util::*};
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::fs::symlink,
    path::{Path, PathBuf},
    process::Command,
};

/// Pacman architecture and cell recipe of each Android product.
fn product_target(product: &str) -> Result<(&'static str, &'static str)> {
    match product {
        "droidloom_x86_64" => Ok(("x86_64", "cell-spec-x86_64-u1000.json")),
        "droidloom_arm64" => Ok(("aarch64", "cell-spec-arm64.json")),
        _ => fail("unsupported Android product"),
    }
}

pub fn stage(
    repo: &Path,
    work: &Path,
    destination: &Path,
    clean: bool,
    jobs: usize,
    product: &str,
    sysroot: Option<&Path>,
) -> Result<()> {
    let (arch, recipe) = product_target(product)?;
    if (arch == "aarch64") != sysroot.is_some() {
        return fail("only the ARM64 product cross-compiles against a sysroot");
    }
    if unsafe { libc::geteuid() } == 0 {
        return fail("package compilation must run as an ordinary build user, without sudo");
    }
    let available = std::thread::available_parallelism()?.get();
    let reserved = std::env::var("DROIDLOOM_COMPILER_CPUS_RESERVED").as_deref() == Ok("1");
    let capacity = if reserved {
        available
    } else {
        available.saturating_sub(2)
    };
    if jobs == 0 || jobs > capacity {
        return fail("package jobs must leave two logical CPUs available");
    }
    fs::create_dir_all(work)?;
    let work = work.canonicalize()?;
    if !destination.is_absolute()
        || !destination.starts_with(&work)
        || destination == work
        || destination
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return fail("package destination must be a child of its private work directory");
    }
    let _lock = Lock::acquire(&work.join("package-build.lock"))?;
    let source = work.join("aosp-source");
    let out = work.join("android-out");
    let cargo = work.join("cargo");
    if work.join("source-projection.json").exists() {
        android::recover(&work.join("source-projection.json"), &source)?;
    }
    let source_ready = work.join("package-source-ready");
    if source.exists() && !source_ready.exists() {
        normalize_source_cache(&source)?;
        write(
            &source_ready,
            b"Private package source cache initialized.\n",
        )?;
    }
    if clean {
        for path in [
            &out,
            &cargo,
            &work.join(format!("cargo-{}", assemble::CROSS_TARGET)),
            &work.join("mesa-tools"),
            &work.join("mesa-panfrost-tools"),
            &work.join("ccache"),
        ] {
            if path.exists() {
                fs::remove_dir_all(path)?;
            }
        }
    }
    let host: PathBuf = match sysroot {
        Some(sysroot) => assemble::host_build(repo, &work, sysroot, jobs)?,
        None => {
            let mut host = Command::new("cargo");
            host.current_dir(repo)
                .env("CARGO_TARGET_DIR", &cargo)
                .args(["build", "--locked", "--release", "--jobs"])
                .arg(jobs.to_string());
            for package in assemble::HOST_PACKAGES {
                host.arg("-p").arg(package);
            }
            run_build(&mut host)?;
            cargo.join("release")
        }
    };
    let base = assemble::prepare_package_inputs(repo, &work, arch)?;
    python::prepare(repo, &work)?;
    // As android-build: the ARM64 product also compiles Panfrost's shaders.
    assemble::mesa_tools(&work, jobs, arch == "aarch64")?;
    android::build(repo, &source, &out, &work, product, jobs)?;
    if !source_ready.exists() {
        write(
            &source_ready,
            b"Private package source cache initialized.\n",
        )?;
    }
    let stage = tempfile::Builder::new()
        .prefix("package-stage-")
        .tempdir_in(&work)?;
    assemble::assemble_artifacts(
        repo,
        &work,
        &base,
        &out.join("target/product").join(product),
        &host,
        stage.path(),
    )?;
    package_layout(repo, &host, stage.path(), arch, recipe, None)?;
    package_notices(repo, &work, stage.path())?;
    if destination.exists() {
        fs::remove_dir_all(destination)?;
    }
    fs::create_dir_all(
        destination
            .parent()
            .ok_or("package destination has no parent")?,
    )?;
    fs::rename(stage.path(), destination)?;
    println!("Portable package payload: {}", destination.display());
    Ok(())
}

/// A test package payload for the phones, from the Android and host builds a
/// phone build has already made: the published release's layout, named as
/// candidate `candidate` of the release after it. Nothing is compiled here.
pub fn stage_candidate(
    repo: &Path,
    work: &Path,
    host: &Path,
    base: &Path,
    product: &str,
    candidate: u32,
    destination: &Path,
) -> Result<()> {
    let (arch, recipe) = product_target(product)?;
    let work = work.canonicalize()?;
    let _lock = Lock::acquire(&work.join("package-build.lock"))?;
    let _source_lock = Lock::acquire(&work.join("aosp-source-update.lock"))?;
    assemble::check_base(repo, base, product)?;
    let parent = destination
        .parent()
        .ok_or("candidate destination has no parent")?;
    fs::create_dir_all(parent)?;
    let stage = tempfile::Builder::new()
        .prefix("candidate-stage-")
        .tempdir_in(parent)?;
    assemble::assemble_artifacts(
        repo,
        &work,
        base,
        &work.join("android-out/target/product").join(product),
        host,
        stage.path(),
    )?;
    package_layout(repo, host, stage.path(), arch, recipe, Some(candidate))?;
    package_notices(repo, &work, stage.path())?;
    if destination.exists() {
        fs::remove_dir_all(destination)?;
    }
    fs::rename(stage.path(), destination)?;
    println!("Candidate package payload: {}", destination.display());
    Ok(())
}

fn normalize_source_cache(source: &Path) -> Result<()> {
    // Seeding reuses Git objects, not prior developer modifications. Only the
    // package workflow's private copy is changed; the seed checkout is untouched.
    eprintln!("Restoring pinned worktrees in the private package source cache.");
    let manifest: Value =
        serde_json::from_slice(&fs::read(source.join(".droidloom-source-manifest.json"))?)?;
    for project in manifest["plan"]["projects"]
        .as_array()
        .ok_or("source manifest has no projects")?
    {
        let path = source
            .join(
                project["path"]
                    .as_str()
                    .ok_or("source project has no path")?,
            )
            .canonicalize()?;
        if !path.starts_with(source) {
            return fail("source project escapes the private package cache");
        }
        run(Command::new("git")
            .arg("-C")
            .arg(&path)
            .args(["reset", "--hard", "HEAD"]))?;
        run(Command::new("git")
            .arg("-C")
            .arg(&path)
            .args(["clean", "-fd"]))?;
    }
    Ok(())
}

fn package_layout(
    repo: &Path,
    host: &Path,
    stage: &Path,
    arch: &str,
    recipe: &str,
    candidate: Option<u32>,
) -> Result<()> {
    let runtime = stage.join("runtime");
    let image = stage.join("image");
    fs::create_dir_all(image.join("usr/lib/droidloom"))?;
    fs::rename(
        stage.join("usr/lib/droidloom/runtime"),
        image.join("usr/lib/droidloom/runtime"),
    )?;
    fs::rename(
        stage.join("usr/lib/droidloom/storage"),
        image.join("usr/lib/droidloom/storage"),
    )?;
    fs::rename(
        stage.join("var/lib/droidloom/images"),
        image.join("usr/lib/droidloom/images"),
    )?;
    symlink("runtime", image.join("usr/lib/droidloom/current"))?;
    fs::remove_dir_all(stage.join("var"))?;
    fs::create_dir_all(&runtime)?;
    fs::rename(stage.join("usr"), runtime.join("usr"))?;
    // A package installation is maintained through pacman. The source updater remains
    // available in the checkout but is not installed as a second system-file owner.
    fs::remove_file(runtime.join("usr/bin/droidloom-update"))?;
    copy(
        &host.join("droidloom-package-helper"),
        &runtime.join("usr/lib/droidloom/droidloom-package-helper"),
    )?;
    mode(
        &runtime.join("usr/lib/droidloom/droidloom-package-helper"),
        0o755,
    )?;
    let mut template: Value =
        serde_json::from_slice(&fs::read(repo.join("packaging").join(recipe))?)?;
    portable_template(&mut template);
    write(
        &runtime.join("usr/share/droidloom/cell-template.json"),
        serde_json::to_vec_pretty(&template)?,
    )?;
    write(
        &runtime.join("usr/share/droidloom/package.json"),
        serde_json::to_vec_pretty(&package_identity(repo, arch, candidate)?)?,
    )?;
    for (from, to) in [
        (
            "droidloom.service",
            "usr/lib/systemd/user/droidloom.service",
        ),
        (
            "org.droidloom.setup.policy",
            "usr/share/polkit-1/actions/org.droidloom.setup.policy",
        ),
        (
            "60-droidloom-upgrade.hook",
            "usr/share/libalpm/hooks/60-droidloom-upgrade.hook",
        ),
        (
            "60-droidloom-remove.hook",
            "usr/share/libalpm/hooks/60-droidloom-remove.hook",
        ),
        (
            "90-droidloom-installed.hook",
            "usr/share/libalpm/hooks/90-droidloom-installed.hook",
        ),
        ("README.md", "usr/share/doc/droidloom/README.md"),
    ] {
        copy(&repo.join("packaging/arch").join(from), &runtime.join(to))?;
    }
    Ok(())
}

/// The release from packaging/arch/version.json and the architecture built;
/// a phone test build also names its candidate number.
fn package_identity(repo: &Path, arch: &str, candidate: Option<u32>) -> Result<Value> {
    let mut identity: Value =
        serde_json::from_slice(&fs::read(repo.join("packaging/arch/version.json"))?)?;
    let fields = identity
        .as_object_mut()
        .ok_or("package version is not an object")?;
    fields.insert("architecture".into(), json!(arch));
    if let Some(candidate) = candidate {
        fields.insert("candidate".into(), json!(candidate));
    }
    Ok(identity)
}

fn portable_template(template: &mut Value) {
    template["image_dir"] = json!("/usr/lib/droidloom/images");
    template["vendor_image"] = json!("/usr/lib/droidloom/images/images/vendor.raw.img");
    for key in [
        "host_uid",
        "render_node",
        "data_dir",
        "runtime_dir",
        "denial_socket",
    ] {
        template.as_object_mut().unwrap().remove(key);
    }
    template["shared_storage_directories"] = json!([]);
}

fn package_notices(repo: &Path, work: &Path, stage: &Path) -> Result<()> {
    let runtime_notices = stage.join("runtime/usr/share/licenses/droidloom-runtime");
    licenses::project(repo, &runtime_notices)?;
    licenses::rust(repo, &runtime_notices.join("rust"))?;
    let notices = stage.join("image/usr/share/licenses/droidloom-image");
    licenses::project(repo, &notices)?;
    copy(
        &work.join("base-system/system/etc/NOTICE.xml.gz"),
        &notices.join("Android-NOTICE.xml.gz"),
    )?;
    copy(
        &work.join("mesa-source/docs/license.rst"),
        &notices.join("Mesa-license.rst"),
    )?;
    let mesa_licenses = work.join("mesa-source/licenses");
    for path in files(&mesa_licenses)? {
        copy(
            &path,
            &notices
                .join("mesa")
                .join(path.strip_prefix(&mesa_licenses)?),
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn portable_recipe_has_no_builder_identity_or_local_storage() {
        let mut template: Value = serde_json::from_str(include_str!(
            "../../../packaging/cell-spec-x86_64-u1000.json"
        ))
        .unwrap();
        portable_template(&mut template);
        for key in [
            "host_uid",
            "render_node",
            "data_dir",
            "runtime_dir",
            "denial_socket",
        ] {
            assert!(template.get(key).is_none());
        }
        assert_eq!(template["image_dir"], "/usr/lib/droidloom/images");
        assert_eq!(template["shared_storage_directories"], json!([]));
        assert!(
            !template["android_file_overrides"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }
    #[test]
    fn package_identity_names_the_architecture_built() {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let version: Value =
            serde_json::from_slice(&fs::read(repo.join("packaging/arch/version.json")).unwrap())
                .unwrap();
        for product in ["droidloom_x86_64", "droidloom_arm64"] {
            let (arch, recipe) = product_target(product).unwrap();
            let identity = package_identity(&repo, arch, None).unwrap();
            assert_eq!(identity["architecture"], arch);
            assert_eq!(identity["version"], version["version"]);
            assert_eq!(identity["release"], version["release"]);
            assert!(repo.join("packaging").join(recipe).is_file());
        }
        assert!(product_target("droidloom_riscv64").is_err());
    }
}
