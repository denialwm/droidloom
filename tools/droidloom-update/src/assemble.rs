use crate::util::*;
use droidloom_supervisor::host_gpu::{GpuChoice, Graphics, HostGpu};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};
pub const HOST_PACKAGES: &[&str] = &[
    "droidloom-supervisor",
    "droidloom-wayland",
    "droidloom-applications",
    "droidloom-audio",
    "droidloom-gpu-server",
    "droidloom-doctor",
    "droidloom-update",
    "droidloom-package-support",
];
pub const HOST_BINARIES: &[&str] = &[
    "droidloomd",
    "droidloomctl",
    "droidloom-supervisor",
    "droidloom-wayland",
    "droidloom-applications",
    "droidloom-audio",
    "droidloom-gpu-server",
    "droidloom-doctor",
    "droidloom-update",
];
fn json(path: &Path) -> Result<Value> {
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}
pub(crate) fn download(url: &str, path: &Path, expected: &str) -> Result<()> {
    if path.exists() && hash(path)? == expected {
        return Ok(());
    }
    let scratch = path.with_extension("download");
    run(Command::new("curl")
        .args(["--fail", "--location", "--retry", "3", "--output"])
        .arg(&scratch)
        .arg(url))?;
    if hash(&scratch)? != expected {
        return fail(format!("download checksum mismatch: {}", path.display()));
    }
    fs::rename(scratch, path)?;
    Ok(())
}
fn extract(image: &Path, destination: &Path) -> Result<()> {
    fs::create_dir_all(destination)?;
    run(Command::new("fsck.erofs")
        .arg(format!("--extract={}", destination.display()))
        .arg(image))
}
fn unzip_file(archive: &Path, name: &str, destination: &Path) -> Result<()> {
    fs::create_dir_all(destination.parent().unwrap())?;
    let output = fs::File::create(destination)?;
    run(Command::new("unzip")
        .args(["-p"])
        .arg(archive)
        .arg(name)
        .stdout(Stdio::from(output)))
}
pub fn prepare_inputs(repo: &Path, work: &Path, arch: &str) -> Result<PathBuf> {
    prepare_inputs_from(repo, work, arch, true)
}
pub fn prepare_package_inputs(repo: &Path, work: &Path, arch: &str) -> Result<PathBuf> {
    prepare_inputs_from(repo, work, arch, false)
}
fn prepare_inputs_from(
    repo: &Path,
    work: &Path,
    arch: &str,
    reuse_installed: bool,
) -> Result<PathBuf> {
    // ARM64 kept the lock's original name.
    let lock_path = repo.join(if arch == "aarch64" {
        "android/manifest/source-lock.json".into()
    } else {
        format!("android/manifest/source-lock-{arch}.json")
    });
    let lock = json(&lock_path)?;
    let typed = droidloom_image::load_source_lock(&lock_path)?;
    let base = if reuse_installed {
        work.join("base")
    } else {
        work.join(format!("base-{}-{}", arch, typed.aosp_ci_base.build_number))
    };
    // Cached and installed partitions are interchangeable only by their pinned content hash.
    let mut candidates = vec![base.join("images")];
    if reuse_installed {
        candidates.push(PathBuf::from("/var/lib/droidloom/images/current/images"));
    }
    let mut found = None;
    for candidate in candidates {
        let mut valid = true;
        for name in ["system", "system_ext", "product"] {
            let path = candidate.join(format!("{name}.img"));
            let expected = typed
                .aosp_ci_base
                .partitions
                .get(&format!("{name}_a"))
                .ok_or("partition absent from lock")?;
            if !path.is_file() || (reuse_installed && hash(&path)? != *expected) {
                valid = false;
                break;
            }
        }
        if valid {
            found = Some(candidate);
            break;
        }
    }
    let images = if let Some(path) = found {
        path
    } else {
        let archive = repo.join(".work").join(&typed.aosp_ci_base.artifact_name);
        // Public redirect endpoint used by Android's kleaf kernel_prebuilt_repo.bzl.
        let url = format!(
            "https://androidbuildinternal.googleapis.com/android/internal/build/v3/builds/{}/{}/attempts/latest/artifacts/{}/url?redirect=true",
            typed.aosp_ci_base.build_number,
            typed.aosp_ci_base.target,
            typed.aosp_ci_base.artifact_name
        );
        download(&url, &archive, &typed.aosp_ci_base.artifact_sha256)?;
        if base.exists() {
            fs::remove_dir_all(&base)?;
        }
        if reuse_installed {
            droidloom_image::prepare_base(&archive, &base, &typed.aosp_ci_base)?;
        } else {
            extract_package_base(&archive, &base)?;
        }
        base.join("images")
    };
    // Reuse extraction for this selected base; generated files do not need byte-for-byte policing.
    let extracted = work.join("base-system");
    let marker = work.join("base-system.json");
    let system_hash = typed
        .aosp_ci_base
        .partitions
        .get("system_a")
        .ok_or("missing system base identity")?;
    let valid = if marker.exists() && extracted.exists() {
        let m = json(&marker)?;
        m["image"] == *system_hash
            && extracted
                .join(&typed.aosp_ci_base.framework_res.path)
                .is_file()
    } else {
        false
    };
    if !valid {
        if extracted.exists() {
            fs::remove_dir_all(&extracted)?;
        }
        extract(&images.join("system.img"), &extracted)?;
        write(&marker, serde_json::to_vec(&json!({"image":system_hash}))?)?;
    }
    let framework = extracted.join(&typed.aosp_ci_base.framework_res.path);
    if reuse_installed && fs::metadata(&framework)?.len() != typed.aosp_ci_base.framework_res.size {
        return fail("framework-res differs from pinned base");
    }
    copy(&framework, &work.join("framework-res.apk"))?;
    let mesa = &lock["mesa"];
    let version = mesa["version"].as_str().ok_or("Mesa version absent")?;
    let archive = repo.join(format!(".work/mesa-{version}.tar.xz"));
    download(
        mesa["url"].as_str().ok_or("Mesa URL absent")?,
        &archive,
        mesa["sha256"].as_str().ok_or("Mesa digest absent")?,
    )?;
    let mesa_source = work.join("mesa-source");
    if let Err(error) = droidloom_mesa::prepare(
        &lock_path,
        &archive,
        &repo.join("android/mesa/patches"),
        &mesa_source,
    ) {
        if !mesa_source.exists() {
            return Err(error.into());
        }
        eprintln!("Regenerating Mesa source cache: {error}");
        fs::remove_dir_all(&mesa_source)?;
        droidloom_mesa::prepare(
            &lock_path,
            &archive,
            &repo.join("android/mesa/patches"),
            &mesa_source,
        )?;
    }
    // The APEX is taken from the verified system partition, never from a previous installation.
    let apex = extracted.join("system/apex/com.android.virt.apex");
    let compressed = extracted.join("system/apex/com.android.virt.capex");
    let apex = if apex.exists() {
        apex
    } else {
        let a = work.join("virt.apex");
        unzip_file(&compressed, "original_apex", &a)?;
        a
    };
    let payload = work.join("virt-payload.img");
    unzip_file(&apex, "apex_payload.img", &payload)?;
    let virt = work.join("virt");
    if virt.exists() {
        fs::remove_dir_all(&virt)?;
    }
    extract(&payload, &virt)?;
    Ok(images)
}

// The package route consumes the pinned upstream archive directly. Derived
// partitions are not subject to the developer installation's binary matching.
// Publish the cache only after all extraction commands succeed.
fn extract_package_base(archive: &Path, base: &Path) -> Result<()> {
    let temporary = tempfile::Builder::new()
        .prefix("package-base-")
        .tempdir_in(base.parent().ok_or("base cache has no parent")?)?;
    let sparse = temporary.path().join("super.img");
    let raw = temporary.path().join("super.raw.img");
    let images = temporary.path().join("images");
    fs::create_dir(&images)?;
    unzip_file(archive, "super.img", &sparse)?;
    run(Command::new("simg2img").arg(&sparse).arg(&raw))?;
    run(Command::new("lpunpack")
        .args(["-p", "system_a", "-p", "system_ext_a", "-p", "product_a"])
        .arg(&raw)
        .arg(&images))?;
    for name in ["system", "system_ext", "product"] {
        fs::rename(
            images.join(format!("{name}_a.img")),
            images.join(format!("{name}.img")),
        )?;
    }
    fs::remove_file(sparse)?;
    fs::remove_file(raw)?;
    fs::rename(temporary.path(), base)?;
    Ok(())
}
pub fn mesa_tools(work: &Path, jobs: usize, panfrost: bool) -> Result<()> {
    run(Command::new("pkg-config").args(["--exists", "LLVMSPIRVLib"]))?;
    // The package tool cache can retain absolute /build container paths.
    // Keep the workstation's ARM shader compiler cache separate from it.
    let build = work.join(if panfrost {
        "mesa-panfrost-tools"
    } else {
        "mesa-tools"
    });
    let mut configure = Command::new("meson");
    configure
        .env("CCACHE_DIR", work.join("ccache"))
        .env("CCACHE_TEMPDIR", work.join("ccache-tmp"));
    configure
        .env("PYTHONPATH", work.join("python"))
        .env("PYTHONDONTWRITEBYTECODE", "1");
    configure.arg("setup");
    if build.join("build.ninja").exists() {
        // Container/toolchain upgrades can retire a previously detected LLVM
        // soname. Re-probe dependencies while retaining Ninja objects/ccache.
        configure.args(["--reconfigure", "--clearcache"]);
    }
    configure.arg(&build).arg(work.join("mesa-source")).args([
        "-Dplatforms=",
        "-Dgallium-drivers=",
        "-Dvulkan-drivers=",
        "-Dllvm=enabled",
        "-Dshared-llvm=enabled",
        "-Dmesa-clc=enabled",
        "-Dinstall-mesa-clc=true",
        "-Dprecomp-compiler=enabled",
        "-Dopengl=false",
        "-Degl=disabled",
        "-Dgbm=disabled",
        "-Dglx=disabled",
        "-Dgles1=disabled",
        "-Dgles2=disabled",
        "-Dvideo-codecs=",
        "-Dbuild-tests=false",
        if panfrost {
            "-Dtools=panfrost"
        } else {
            "-Dtools="
        },
        "-Dvalgrind=disabled",
        "-Dlibunwind=disabled",
    ]);
    run_build(&mut configure)?;
    let mut ninja = Command::new("ninja");
    ninja
        .arg("-C")
        .arg(&build)
        .arg(format!("-j{jobs}"))
        .args([
            "src/compiler/clc/mesa_clc",
            "src/compiler/spirv/vtn_bindgen2",
        ])
        .env("PYTHONPATH", work.join("python"))
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .env("CCACHE_DIR", work.join("ccache"))
        .env("CCACHE_TEMPDIR", work.join("ccache-tmp"));
    if panfrost {
        ninja.arg("src/panfrost/clc/panfrost_compile");
    }
    run_build(&mut ninja)
}
pub fn assemble(
    repo: &Path,
    work: &Path,
    base: &Path,
    product: &Path,
    host: &Path,
    payload: &Path,
    uid: u32,
) -> Result<()> {
    assemble_artifacts(repo, work, base, product, host, payload)?;
    let notices = payload.join("usr/share/licenses/droidloom");
    crate::licenses::project(repo, &notices)?;
    crate::licenses::rust(repo, &notices.join("rust"))?;
    configure_local(repo, payload, uid)
}

/// Stage the complete artifact recipe without reading any build-host configuration.
pub fn assemble_artifacts(
    repo: &Path,
    work: &Path,
    base: &Path,
    product: &Path,
    host: &Path,
    payload: &Path,
) -> Result<()> {
    let runtime = payload.join("usr/lib/droidloom/runtime");
    let product_name = product
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("Android product directory has no name")?;
    let arm64 = match product_name {
        "droidloom_arm64" => true,
        "droidloom_x86_64" => false,
        _ => return fail("unsupported Android product directory"),
    };
    for name in HOST_BINARIES {
        let dest = payload.join("usr/bin").join(name);
        copy(&host.join(name), &dest)?;
        mode(&dest, 0o755)?;
    }
    for (src, dst) in [
        ("system/bin/init", "bin/droidloom-android-init"),
        (
            "vendor/bin/droidloom-lmkd-compat",
            "bin/droidloom-lmkd-compat",
        ),
        ("system/bin/netd", "bin/netd-cell-policy"),
        ("system/bin/servicemanager", "bin/servicemanager"),
        ("system/bin/surfaceflinger", "bin/surfaceflinger"),
        (
            "vendor/bin/hw/android.hardware.graphics.composer3-service.droidloom",
            "bin/android.hardware.graphics.composer3-service.droidloom",
        ),
        ("system/framework/services.jar", "framework/services.jar"),
        (
            "system/app/DroidloomIME/DroidloomIME.apk",
            "ime/DroidloomIME.apk",
        ),
        (
            "system/app/DroidloomHome/DroidloomHome.apk",
            "ime/DroidloomHome.apk",
        ),
        (
            "system/system_ext/priv-app/DroidloomSystemUI/DroidloomSystemUI.apk",
            "systemui/SystemUI.apk",
        ),
        ("vendor/build.prop", "overrides/vendor/build.prop"),
        (
            "vendor/bin/droidloom-classpath-wrapper",
            "compat/classpath-compat/bin/droidloom-classpath-wrapper",
        ),
        (
            "system/lib64/libandroid_net_connectivity_com_android_net_module_util_jni.so",
            "compat/classpath-compat/lib64/libandroid_net_connectivity_com_android_net_module_util_jni.so",
        ),
    ] {
        copy(&product.join(src), &runtime.join(dst))?;
    }
    let scratch = tempfile::Builder::new()
        .prefix("system-server-")
        .tempdir_in(work)?;
    crate::system_server::stage(
        base,
        &product.join("system/framework/services.jar"),
        &work.join("android-out/host/linux-x86/bin"),
        scratch.path(),
        &runtime.join("framework/oat"),
    )?;
    for (name, destination) in [
        ("netbpfload", "compat/classpath-compat/bin/netbpfload"),
        (
            "libnetd_updatable",
            "compat/classpath-compat/lib64/libnetd_updatable.so",
        ),
        (
            "libservice-connectivity",
            "compat/classpath-compat/lib64/libservice-connectivity.so",
        ),
    ] {
        copy(
            &work
                .join("android-out")
                .join(crate::android::apex_output_for_product(name, product_name).unwrap()),
            &runtime.join(destination),
        )?;
    }
    copy(
        &product.join("system/bin/vold"),
        &payload.join("usr/lib/droidloom/storage/vold"),
    )?;
    for name in [
        "libbinder.so",
        "libmeminfo.so",
        "libinputflinger.so",
        "libaudiobase.so",
    ] {
        copy(
            &product.join("system/lib64").join(name),
            &runtime.join("lib64").join(name),
        )?;
    }
    for name in [
        "libdroidloom_surface_bridge.dylib.so",
        "libdroidloom_task_control.dylib.so",
        "libdroidloom_composer_aidl.dylib.so",
        "libdroidloom_syncobj.dylib.so",
        "libdroidloom_denial_ipc.dylib.so",
        "libdroidloom_composer.dylib.so",
        "libdroidloom_minigbm.dylib.so",
        "libdroidloom_denial_protocol.dylib.so",
        "libdroidloom_transport.dylib.so",
        "libdroidloom_selinux_compat.so",
        "libminigbm_gralloc.so",
        "libgallium_dri.so",
    ] {
        copy(
            &product.join("vendor/lib64").join(name),
            &runtime.join("lib64").join(name),
        )?;
    }
    if !arm64 {
        copy(
            &product.join("vendor/lib64/libdrm_amdgpu.so"),
            &runtime.join("lib64/libdrm_amdgpu.so"),
        )?;
    }
    for name in [
        "libEGL_mesa.so",
        "libGLESv1_CM_mesa.so",
        "libGLESv2_mesa.so",
    ] {
        copy(
            &product.join("vendor/lib64/egl").join(name),
            &runtime.join("egl").join(name),
        )?;
    }
    for (src, dst) in [
        ("packaging/compat/lmkd.rc", "compat/lmkd-first-pixels.rc"),
        ("packaging/compat/netd.rc", "compat/netd-mainline-bpf.rc"),
        (
            "android/vintf-compat/compatibility_matrix.device.xml",
            "compat/vintf/compatibility_matrix.device.xml",
        ),
        (
            "android/device/droidloom_arm64/droidloom-composer.rc",
            "etc/init/droidloom-composer.rc",
        ),
        ("packaging/ime/setup", "ime/setup"),
        ("packaging/home/setup", "ime/home-setup"),
        (
            "android/framework/droidloom-home/droidloom-home.rc",
            "ime/droidloom-home.rc",
        ),
        (
            "android/framework/droidloom-window-policy/droidloom-framework.rc",
            "ime/droidloom-framework.rc",
        ),
    ] {
        copy(&repo.join(src), &runtime.join(dst))?;
    }
    copy(
        &work.join("init.rc"),
        &runtime.join("compat/init-classpath-first-pixels.rc"),
    )?;
    // Mounted over the base image's apps that Denial replaces or the cell
    // cannot use (no camera): Android finds no APK there and drops them.
    // Releases install files, so the folder holds one that is not an APK.
    write(
        &runtime.join("compat/removed-app/README"),
        "Droidloom leaves this app out of Android.\n",
    )?;
    // Preserve both Java classpaths advertised by the base APEX manifest.
    for name in [
        "etc/classpaths/bootclasspath.pb",
        "etc/classpaths/systemserverclasspath.pb",
        "javalib/framework-virtualization.jar",
        "javalib/service-virtualization.jar",
    ] {
        copy(
            &work.join("virt").join(name),
            &runtime
                .join("compat/classpath-compat/apex/com.android.virt")
                .join(name),
        )?;
    }
    let images = payload.join("var/lib/droidloom/images/images");
    copy(&base.join("product.img"), &images.join("product.img"))?;
    if arm64 {
        // Native ARM apps need neither an ISA translator nor the desktop-only
        // system_ext projection. Keep both pinned input partitions intact.
        for name in ["system.img", "system_ext.img"] {
            copy(&base.join(name), &images.join(name))?;
        }
    } else {
        crate::native_bridge::stage_image(
            repo,
            &base.join("system.img"),
            &images.join("system.img"),
            product,
        )?;
        crate::image_policy::stage(
            &base.join("system_ext.img"),
            &images.join("system_ext.img"),
            &product.join("vendor/build.prop"),
        )?;
    }
    let vendor = product.join("vendor.img");
    let mut magic = [0; 4];
    use std::io::Read;
    fs::File::open(&vendor)?.read_exact(&mut magic)?;
    if magic == [0x3a, 0xff, 0x26, 0xed] {
        run(Command::new("simg2img")
            .arg(&vendor)
            .arg(images.join("vendor.raw.img")))?;
    } else {
        copy(&vendor, &images.join("vendor.raw.img"))?;
    }
    for name in [
        "system/droidloomd.service",
        "user/droidloom.service",
        "user/droidloom-applications.service",
        "user/droidloom-recovery.service",
        "user/droidloom-audio.service",
        "user/droidloom-audio.socket",
        "user/droidloom-gpu.service",
        "user/droidloom-gpu.socket",
    ] {
        copy(
            &repo.join("packaging/systemd").join(name),
            &payload.join("usr/lib/systemd").join(name),
        )?;
    }
    Ok(())
}

fn configure_local(repo: &Path, payload: &Path, uid: u32) -> Result<()> {
    let mut spec = json(&repo.join("packaging/cell-spec-x86_64-u1000.json"))?;
    spec["host_uid"] = json!(uid);
    spec["data_dir"] = json!(format!("/var/lib/droidloom/users/{uid}/data"));
    spec["runtime_dir"] = json!(format!("/run/droidloom/cells/u{uid}"));
    spec["denial_socket"] = json!(format!("/run/user/{uid}/droidloom/native-bridge.sock"));
    spec["shared_storage_directories"] = json!([]);
    if Path::new("/etc/droidloom/cell.json").exists() {
        let existing = json(Path::new("/etc/droidloom/cell.json"))?;
        preserve_settings(&mut spec, &existing, uid)?;
    }
    let choice: GpuChoice = match spec.get("gpu") {
        Some(value) => serde_json::from_value(value.clone())?,
        None => GpuChoice::Auto,
    };
    let graphics = Graphics::resolve(&choice, &HostGpu::list(Path::new("/sys/class/drm"))?)
        .map_err(|error| format!("this x86_64 product needs an AMD, Intel or NVIDIA GPU: {error}"))?;
    println!("droidloom-update: GPU choice {choice}: {graphics}.");
    spec["render_node"] = json!(graphics.render_node);
    spec["graphics_backend"] = json!(graphics.backend);
    let record = output(Command::new("getent").args(["passwd", &uid.to_string()]))?;
    configure_target(
        repo,
        payload,
        &spec,
        record.split(':').next().ok_or("user absent")?,
        graphics.forwarded(),
    )
}

/// `forwarded` names the GPU `droidloom-gpu-server` renders on.
fn configure_target(repo: &Path, payload: &Path, spec: &Value, user: &str, forwarded: Option<&str>) -> Result<()> {
    let runtime = payload.join("usr/lib/droidloom/runtime");
    let uid = spec["host_uid"]
        .as_u64()
        .filter(|uid| *uid > 0 && *uid <= u64::from(u32::MAX))
        .ok_or("cell requires an unprivileged desktop UID")?;
    let render = spec["render_node"]
        .as_str()
        .filter(|path| path.starts_with("/dev/dri/") && !path.contains(['\n', '\r']))
        .ok_or("cell requires a DRM render node")?;
    for entry in spec["android_file_overrides"]
        .as_array()
        .ok_or("missing overrides")?
    {
        let src = entry["source"].as_str().ok_or("bad override")?;
        let actual = if let Some(rel) = src.strip_prefix("/usr/lib/droidloom/current/") {
            runtime.join(rel)
        } else {
            payload.join(src.trim_start_matches('/'))
        };
        if !actual.is_file() && !actual.is_dir() {
            return fail(format!("missing runtime override: {src}"));
        }
    }
    write(
        &payload.join("etc/droidloom/cell.json"),
        serde_json::to_vec_pretty(&spec)?,
    )?;
    write(
        &payload.join("etc/droidloom/update.json"),
        serde_json::to_vec_pretty(&json!({"source":repo,"uid":uid}))?,
    )?;
    write(
        &payload.join("usr/lib/environment.d/60-droidloom.conf"),
        format!(
            "DROIDLOOM_HOST_PEER_PID=0\nDROIDLOOM_HOST_PEER_UID=1000\nDROIDLOOM_HOST_PEER_GID=1000\nDROIDLOOM_RENDER_NODE={render}\n{}",
            forwarded.map_or_else(String::new, |pci| format!("DROIDLOOM_GPU={pci}\n"))
        ),
    )?;
    let user = serde_json::to_string(user)?;
    write(
        &payload.join("etc/polkit-1/rules.d/49-droidloom.rules"),
        format!(
            "polkit.addRule(function(action, subject) {{\n if (action.id === \"org.freedesktop.systemd1.manage-units\" && subject.user === {user} && action.lookup(\"unit\") === \"droidloomd.service\" && (action.lookup(\"verb\") === \"start\" || action.lookup(\"verb\") === \"stop\")) return polkit.Result.YES;\n}});\n"
        ),
    )?;
    Ok(())
}

/// Linux target of the ARM64 device programs.
pub const CROSS_TARGET: &str = "aarch64-unknown-linux-gnu";

/// Cross-compile the host programs for an ARM64 device. The Cargo output is a
/// persistent cache below `work`, so later builds compile only what changed.
pub fn host_build(repo: &Path, work: &Path, sysroot: &Path, jobs: usize) -> Result<PathBuf> {
    let sysroot = sysroot.canonicalize()?;
    let lib = sysroot.join("usr/lib");
    if !lib.join("pkgconfig").is_dir() {
        return fail("the ARM64 sysroot has no usr/lib/pkgconfig");
    }
    let root = sysroot
        .to_str()
        .filter(|root| !root.contains(char::is_whitespace));
    let root = root.ok_or("the ARM64 sysroot path must be UTF-8 without whitespace")?;
    let target = work.join(format!("cargo-{CROSS_TARGET}"));
    let _lock = Lock::acquire(&work.join("host-cross-build.lock"))?;
    let mut build = Command::new("cargo");
    // Flags in the environment describe the build machine: makepkg exports
    // Arch's x86_64 CFLAGS, and RUSTFLAGS would replace the target's rustflags.
    for flags in [
        "CFLAGS",
        "CXXFLAGS",
        "CPPFLAGS",
        "LDFLAGS",
        "RUSTFLAGS",
        "CARGO_ENCODED_RUSTFLAGS",
    ] {
        build.env_remove(flags);
    }
    build
        .current_dir(repo)
        .env("CARGO_TARGET_DIR", &target)
        .env(
            "CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER",
            "aarch64-linux-gnu-gcc",
        )
        .env(
            "CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUSTFLAGS",
            format!(
                "-C link-arg=--sysroot={root} -C link-arg=-L{root}/usr/lib -C link-arg=-Wl,-rpath-link,{root}/usr/lib"
            ),
        )
        .env("CC_aarch64_unknown_linux_gnu", "aarch64-linux-gnu-gcc")
        .env("CFLAGS_aarch64_unknown_linux_gnu", format!("--sysroot={root}"))
        .env("PKG_CONFIG_ALLOW_CROSS", "1")
        .env("PKG_CONFIG_SYSROOT_DIR_aarch64_unknown_linux_gnu", root)
        .env(
            "PKG_CONFIG_LIBDIR_aarch64_unknown_linux_gnu",
            format!("{root}/usr/lib/pkgconfig:{root}/usr/share/pkgconfig"),
        )
        .args(["build", "--locked", "--release", "--target", CROSS_TARGET])
        .arg(format!("-j{jobs}"));
    for package in HOST_PACKAGES {
        build.arg("-p").arg(package);
    }
    run_build(&mut build)?;
    let release = target.join(CROSS_TARGET).join("release");
    for name in HOST_BINARIES {
        if !release.join(name).is_file() {
            return fail(format!("cross build did not produce {name}"));
        }
    }
    println!("ARM64 host programs: {}", release.display());
    Ok(release)
}

/// The base partitions are the ones the product's source lock pins.
pub fn check_base(repo: &Path, base: &Path, product: &str) -> Result<()> {
    let lock = match product {
        "droidloom_arm64" => "android/manifest/source-lock.json",
        "droidloom_x86_64" => "android/manifest/source-lock-x86_64.json",
        _ => return fail("unsupported Android product"),
    };
    let lock = json(&repo.join(lock))?;
    for name in ["system", "system_ext", "product"] {
        let expected = lock["aosp_ci_base"]["partitions"][format!("{name}_a")]
            .as_str()
            .ok_or("base image identity is missing")?;
        if hash(&base.join(format!("{name}.img")))? != expected {
            return fail(format!("{name} differs from the pinned {product} base"));
        }
    }
    Ok(())
}

fn preserve_settings(spec: &mut Value, existing: &Value, uid: u32) -> Result<()> {
    if existing["host_uid"].as_u64() != Some(u64::from(uid)) {
        return fail("the existing Droidloom installation belongs to another desktop user");
    }
    // Runtime mappings come from the new recipe. Personal data and sharing do not.
    for field in [
        "data_dir",
        "gpu",
        "subordinate_uids",
        "subordinate_gids",
        "shared_storage_directories",
    ] {
        if let Some(value) = existing.get(field) {
            spec[field] = value.clone();
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn update_keeps_personal_state_but_replaces_runtime_mappings() {
        let existing = json!({"host_uid":1000,"data_dir":"/var/lib/droidloom/custom-data","shared_storage_directories":[{"source":"/home/test/Downloads"}],"android_file_overrides":["old"]});
        let mut candidate = json!({"android_file_overrides":["new"]});
        preserve_settings(&mut candidate, &existing, 1000).unwrap();
        assert_eq!(candidate["data_dir"], existing["data_dir"]);
        assert_eq!(
            candidate["shared_storage_directories"],
            existing["shared_storage_directories"]
        );
        assert_eq!(candidate["android_file_overrides"], json!(["new"]));
        assert!(preserve_settings(&mut candidate, &existing, 1001).is_err());
    }
    #[test]
    fn arm64_recipe_maps_only_release_files() {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let cell = json(&repo.join("packaging/cell-spec-arm64.json")).unwrap();
        for entry in cell["android_file_overrides"].as_array().unwrap() {
            let source = entry["source"].as_str().unwrap();
            assert!(
                source.starts_with("/usr/lib/droidloom/current/")
                    || source == "/usr/lib/droidloom/storage/vold",
                "{source} is not part of the release"
            );
        }
    }
}
