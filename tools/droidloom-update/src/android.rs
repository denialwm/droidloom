use crate::util::*;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};
// Full runtime closure. Every update asks the build system for every target.
pub const TARGETS: &[&str] = &[
    "droidloom-native-bridge",
    "android.hardware.graphics.composer3-service.droidloom",
    "droidloom-framework-init",
    "DroidloomIME",
    "DroidloomHome",
    "DroidloomSystemUI",
    "droidloom-lmkd-compat",
    "droidloom-classpath-wrapper",
    "netbpfload",
    "init_second_stage",
    "netd",
    "vold",
    "libbinder",
    "servicemanager",
    "libmeminfo",
    "libinputflinger",
    "libaudiobase",
    "surfaceflinger",
    "libdroidloom_surface_bridge",
    "libdroidloom_task_control",
    "libdroidloom_selinux_compat",
    "libdroidloom_composer_aidl",
    "libdroidloom_composer",
    "libdroidloom_minigbm",
    "libdroidloom_syncobj",
    "libdroidloom_denial_ipc",
    "libdroidloom_denial_protocol",
    "libdroidloom_transport",
    "libdroidloom_audio_client",
    "android.hardware.graphics.allocator-service.minigbm",
    "mapper.minigbm",
    "libandroid_net_connectivity_com_android_net_module_util_jni",
    "libservice-connectivity",
    "libnetd_updatable",
    "services",
    "vendorimage",
    "dex2oat",
];
pub const AUDIO_TARGETS: &[&str] = &["com.android.hardware.audio.droidloom", "libaudiobase"];
pub const TASK_BACKEND_TARGETS: &[&str] = &[
    "services",
    "libinputflinger",
    "surfaceflinger",
    "DroidloomIME",
    "DroidloomSystemUI",
    "android.hardware.graphics.composer3-service.droidloom",
    "DroidloomSystemUIProbe",
    // Rust dylibs are separate installable outputs. Building only executables
    // can leave their stripped/staged dependencies at the previous ABI.
    "libdroidloom_task_control",
    "libdroidloom_surface_bridge",
    "libdroidloom_composer_aidl",
    "libdroidloom_syncobj",
    "libdroidloom_denial_ipc",
    "libdroidloom_composer",
    "libdroidloom_minigbm",
    "libdroidloom_denial_protocol",
    "libdroidloom_transport",
];
// These APEX components are installed by Droidloom's compatibility projection.
// Request their compiled outputs, without asking AOSP to install them into system.
pub fn apex_output(name: &str) -> Option<&'static str> {
    match name {
        "netbpfload" => Some(
            "soong/.intermediates/packages/modules/Connectivity/bpf/loader/netbpfload/android_x86_64/netbpfload",
        ),
        "libservice-connectivity" => Some(
            "soong/.intermediates/packages/modules/Connectivity/service/libservice-connectivity/android_x86_64_shared/libservice-connectivity.so",
        ),
        "libnetd_updatable" => Some(
            "soong/.intermediates/packages/modules/Connectivity/bpf/netd/libnetd_updatable/android_x86_64_shared_cfi/libnetd_updatable.so",
        ),
        _ => None,
    }
}
// The boot profile of services.jar. The system-server compile in bundle staging
// reads it beside the jar, so a jar is never staged with a stale profile.
const SERVICES_PROFILE: &str = "soong/.intermediates/frameworks/base/services/services/android_common/dexpreopt/services/profile.prof";
// The host compiler of the system-server jars, from the base image's source.
pub fn host_output(name: &str) -> Option<&'static str> {
    match name {
        "dex2oat" => Some("host/linux-x86/bin/dex2oat64"),
        _ => None,
    }
}
pub fn apex_output_for_product(name: &str, product: &str) -> Option<PathBuf> {
    let path = apex_output(name)?;
    match product {
        "droidloom_x86_64" => Some(PathBuf::from(path)),
        "droidloom_arm64" => Some(PathBuf::from(
            path.replace("android_x86_64", "android_arm64_armv8-a"),
        )),
        _ => None,
    }
}
const PATCHES: &[(&str, &str)] = &[
    (
        "build/make",
        "android/aosp-patches/0017-build-hwui-vulkan-unset.patch",
    ),
    (
        "external/libdrm",
        "android/aosp-patches/0015-libdrm-empty-version-strings.patch",
    ),
    (
        "frameworks/native",
        "android/surfaceflinger/0009-droidloom-cpu-placement.patch",
    ),
    (
        "external/minigbm",
        "android/aosp-patches/0010-minigbm-dma-heap-images.patch",
    ),
    (
        "packages/modules/Connectivity",
        "android/aosp-patches/0009-netd-bpf-pid-namespace.patch",
    ),
    (
        "packages/modules/Connectivity",
        "android/aosp-patches/0001-netbpfload-tolerate-shared-kernel-bpf-ids.patch",
    ),
    (
        "packages/modules/Connectivity",
        "android/aosp-patches/0003-connectivity-offline-rcu.patch",
    ),
    (
        "system/netd",
        "android/aosp-patches/0006-netd-cell-kernel-policy.patch",
    ),
    (
        "hardware/interfaces",
        "android/aosp-patches/0002-audio-default-only-service.patch",
    ),
    (
        "frameworks/av",
        "android/audio/0001-host-buffered-playback.patch",
    ),
    (
        "system/memory/libmeminfo",
        "android/aosp-patches/0004-libmeminfo-offline-gpu-accounting.patch",
    ),
    (
        "",
        "android/binder-compat/0001-droidloom-context-manager-without-kernel-sid.patch",
    ),
    (
        "",
        "android/binder-compat/0002-droidloom-servicemanager-pid-context-fallback.patch",
    ),
    (
        "",
        "android/binder-compat/0003-droidloom-cell-caller-context-fallback.patch",
    ),
    (
        "frameworks/native",
        "android/surfaceflinger/0001-droidloom-direct-denial-render-surface.patch",
    ),
    (
        "frameworks/native",
        "android/surfaceflinger/0002-droidloom-task-input-token.patch",
    ),
    (
        "frameworks/native",
        "android/surfaceflinger/0003-droidloom-headless-bootstrap.patch",
    ),
    (
        "frameworks/native",
        "android/surfaceflinger/0004-droidloom-opaque-content.patch",
    ),
    (
        "frameworks/native",
        "android/surfaceflinger/0005-droidloom-retained-task-composition.patch",
    ),
    (
        "frameworks/native",
        "android/surfaceflinger/0006-droidloom-late-task-registration.patch",
    ),
    (
        "frameworks/native",
        "android/surfaceflinger/0007-droidloom-wayland-layers.patch",
    ),
    (
        "frameworks/native",
        "android/surfaceflinger/0008-droidloom-release-fences.patch",
    ),
    (
        "frameworks/native",
        "android/surfaceflinger/0010-droidloom-task-backpressure-background.patch",
    ),
    (
        "frameworks/native",
        "android/surfaceflinger/0011-droidloom-async-layer-commits.patch",
    ),
    (
        "frameworks/native",
        "android/surfaceflinger/0012-droidloom-async-imports.patch",
    ),
    (
        "frameworks/native",
        "android/surfaceflinger/0013-droidloom-lazy-render-targets.patch",
    ),
    (
        "frameworks/native",
        "android/surfaceflinger/0014-droidloom-task-work-gate.patch",
    ),
    (
        "frameworks/native",
        "android/surfaceflinger/0015-droidloom-scene-batches.patch",
    ),
    (
        "frameworks/native",
        "android/surfaceflinger/0016-droidloom-frame-timing.patch",
    ),
    (
        "frameworks/native",
        "android/surfaceflinger/0017-droidloom-bufferless-bootstrap.patch",
    ),
    (
        "frameworks/native",
        "android/surfaceflinger/0018-droidloom-import-retention.patch",
    ),
    (
        "frameworks/native",
        "android/surfaceflinger/0019-droidloom-initial-task-geometry.patch",
    ),
    (
        "frameworks/native",
        "android/surfaceflinger/0020-droidloom-hidden-task-liveness.patch",
    ),
    (
        "frameworks/native",
        "android/surfaceflinger/0021-droidloom-release-backlog-diagnostics.patch",
    ),
    (
        "frameworks/native",
        "android/surfaceflinger/0022-droidloom-early-durations-follow-late.patch",
    ),
    (
        "frameworks/native",
        "android/surfaceflinger/0023-droidloom-read-switches-once.patch",
    ),
    (
        "frameworks/native",
        "android/inputflinger/0001-application-token-targeted-injection.patch",
    ),
    (
        "frameworks/native",
        "android/inputflinger/0002-droidloom-targeted-injection-binder.patch",
    ),
    (
        "frameworks/native",
        "android/inputflinger/0003-droidloom-window-policy.patch",
    ),
    (
        "frameworks/native",
        "android/inputflinger/0004-droidloom-native-input.patch",
    ),
    (
        "frameworks/native",
        "android/inputflinger/0005-droidloom-vulkan-probe.patch",
    ),
    (
        "",
        "android/init-compat/0001-droidloom-cell-namespace.patch",
    ),
    (
        "external/minigbm",
        "android/aosp-patches/0007-minigbm-amdgpu-platform.patch",
    ),
    (
        "system/vold",
        "android/aosp-patches/0008-vold-preserve-shared-storage-submounts.patch",
    ),
    (
        "frameworks/base",
        "android/framework/0001-droidloom-bottom-navigation-insets.patch",
    ),
    (
        "frameworks/base",
        "android/framework/0002-droidloom-routed-touch-configuration.patch",
    ),
    (
        "frameworks/base",
        "android/framework/0003-droidloom-launch-resolution.patch",
    ),
    (
        "frameworks/base",
        "android/framework/0004-droidloom-window-policy.patch",
    ),
    (
        "frameworks/base",
        "android/framework/0005-droidloom-authoritative-tasks.patch",
    ),
    (
        "frameworks/base",
        "android/framework/0006-droidloom-direct-host-channels.patch",
    ),
    (
        "frameworks/base",
        "android/framework/0007-droidloom-detected-vulkan-capabilities.patch",
    ),
];
#[derive(Serialize, Deserialize)]
struct Original {
    path: PathBuf,
    bytes: Option<Vec<u8>>,
    #[serde(default)]
    modified: Option<std::time::SystemTime>,
    // A directory keeps only its timestamp: creating or renaming files in it
    // changes that, and Mesa's Android build depends on its directories.
    #[serde(default)]
    directory: bool,
}
pub(crate) struct Projection {
    journal: PathBuf,
    originals: Vec<Original>,
}
impl Projection {
    fn new(journal: PathBuf) -> Result<Self> {
        if journal.exists() {
            return fail(format!(
                "interrupted source projection: {}; run recover-source before building",
                journal.display()
            ));
        }
        Ok(Self {
            journal,
            originals: Vec::new(),
        })
    }
    fn save(&mut self, path: &Path) -> Result<()> {
        let path = path.to_owned();
        if self.originals.iter().any(|o| o.path == path) {
            return Ok(());
        }
        if let Some(directory) = path.ancestors().skip(1).find(|a| a.is_dir()) {
            if !self.originals.iter().any(|o| o.path == directory) {
                self.originals.push(Original {
                    modified: Some(fs::metadata(directory)?.modified()?),
                    bytes: None,
                    path: directory.to_owned(),
                    directory: true,
                });
            }
        }
        self.originals.push(Original {
            modified: fs::metadata(&path).ok().and_then(|m| m.modified().ok()),
            bytes: match fs::read(&path) {
                Ok(b) => Some(b),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => return Err(e.into()),
            },
            path,
            directory: false,
        });
        durable_write(&self.journal, serde_json::to_vec(&self.originals)?)?;
        Ok(())
    }
    pub(crate) fn put(&mut self, path: &Path, bytes: impl AsRef<[u8]>) -> Result<()> {
        self.save(path)?;
        write(path, bytes)
    }
    fn stabilize_all(&self) -> Result<()> {
        // Wait until all patches are applied: several patches can touch one file.
        for original in self.originals.iter().filter(|o| !o.directory) {
            self.stabilize(&original.path)?;
        }
        self.restore_directory_times()
    }
    fn restore_directory_times(&self) -> Result<()> {
        for original in self.originals.iter().filter(|o| o.directory) {
            if let (Some(modified), true) = (original.modified, original.path.is_dir()) {
                fs::File::open(&original.path)?.set_modified(modified)?;
            }
        }
        Ok(())
    }
    fn stabilize(&self, path: &Path) -> Result<()> {
        if !path.is_file() {
            return Ok(());
        }
        let cache = self
            .journal
            .parent()
            .ok_or("journal lacks parent")?
            .join("projection-cache")
            .join(path.strip_prefix("/").unwrap_or(path));
        // Content comparison is for incremental builds, not artifact verification.
        // Reusing the cached timestamp prevents patch application from invalidating Ninja.
        write(&cache, fs::read(path)?)?;
        fs::File::open(path)?.set_modified(fs::metadata(cache)?.modified()?)?;
        Ok(())
    }
    fn patch(&mut self, source: &Path, cwd: &str, patch: &Path) -> Result<()> {
        let directory = source.join(cwd);
        run(Command::new("git")
            .env("GIT_CEILING_DIRECTORIES", source.parent().unwrap())
            .arg("-C")
            .arg(&directory)
            .args(["apply", "--check"])
            .arg(patch))?;
        let changed = output(
            Command::new("git")
                .env("GIT_CEILING_DIRECTORIES", source.parent().unwrap())
                .arg("-C")
                .arg(&directory)
                .args(["apply", "--numstat"])
                .arg(patch),
        )?;
        for line in changed.lines() {
            let path = line.split('\t').nth(2).ok_or("invalid patch numstat")?;
            if Path::new(path)
                .components()
                .any(|c| !matches!(c, std::path::Component::Normal(_)))
            {
                return fail("unsafe patch path");
            }
            self.save(&directory.join(path))?;
        }
        run(Command::new("git")
            .env("GIT_CEILING_DIRECTORIES", source.parent().unwrap())
            .arg("-C")
            .arg(&directory)
            .arg("apply")
            .arg(patch))?;
        Ok(())
    }
    fn restore(&mut self) -> Result<()> {
        for o in self.originals.iter().rev().filter(|o| !o.directory) {
            if let Some(bytes) = &o.bytes {
                write(&o.path, bytes)?;
                if let Some(modified) = o.modified {
                    fs::File::open(&o.path)?.set_modified(modified)?;
                }
            } else if o.path.exists() {
                fs::remove_file(&o.path)?;
            }
        }
        self.restore_directory_times()?;
        self.originals.clear();
        if self.journal.exists() {
            fs::remove_file(&self.journal)?;
        }
        Ok(())
    }
}
impl Drop for Projection {
    fn drop(&mut self) {
        if let Err(e) = self.restore() {
            eprintln!(
                "SOURCE RESTORE FAILED: {e}; retained {}",
                self.journal.display()
            );
        }
    }
}
pub fn recover(journal: &Path, source: &Path) -> Result<()> {
    let originals: Vec<Original> = serde_json::from_slice(&fs::read(journal)?)?;
    for o in &originals {
        if !o.path.starts_with(source)
            || o.path
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return fail("projection journal escapes source tree");
        }
    }
    Projection {
        journal: journal.to_owned(),
        originals,
    }
    .restore()
}
fn metadata_only(text: &str) -> Result<String> {
    let mut result = String::new();
    let mut taking = false;
    for line in text.lines() {
        if line.starts_with("package {") || line.starts_with("license {") {
            taking = true;
        }
        if taking {
            result.push_str(line);
            result.push('\n');
            if line == "}" {
                taking = false;
            }
        }
    }
    if result.is_empty() {
        return fail("missing Blueprint package metadata");
    }
    Ok(result)
}
fn project_blueprints(p: &mut Projection, source: &Path) -> Result<()> {
    for rel in [
        "frameworks/base/libs/WindowManager/Shell/Android.bp",
        "frameworks/base/packages/SystemUI/Android.bp",
        "packages/apps/Launcher3/Android.bp",
        "packages/apps/Settings/Android.bp",
        "frameworks/native/services/serialservice/Android.bp",
        "packages/modules/ConfigInfrastructure/aconfigd/Android.bp",
        "packages/modules/Permission/framework-s/Android.bp",
    ] {
        let path = source.join(rel);
        p.put(&path, metadata_only(&fs::read_to_string(&path)?)?)?;
    }
    let permission = source.join("packages/modules/Permission/framework-s/Android.bp");
    let mut text = fs::read_to_string(&permission)?;
    let mut names = Vec::new();
    for (scope, file) in [
        ("public", "current.txt"),
        ("system", "system-current.txt"),
        ("module-lib", "module-lib-current.txt"),
    ] {
        let name = format!("droidloom-permission-s-{scope}-api");
        names.push(format!("\"{name}\""));
        text += &format!(
            "java_api_contribution {{ name: \"{name}\", api_surface: \"{scope}\", api_file: \"api/{file}\", }}\n"
        );
    }
    text += &format!(
        "java_api_library {{ name: \"droidloom-permission-s-api-stubs\", api_surface: \"module-lib\", api_contributions: [{}], sdk_version: \"module_current\", libs: [\"stub-annotations\"], stubs_type: \"everything\", enable_validation: false, visibility: [\"//prebuilts/sdk:__pkg__\"], }}\n",
        names.join(",")
    );
    p.put(&permission, text)?;
    let sdk = source.join("prebuilts/sdk/Android.bp");
    let mut text = fs::read_to_string(&sdk)?;
    for (scope, suffix, version, combined) in [
        ("public", "", "current", "android_stubs_current"),
        (
            "system",
            ".system",
            "system_current",
            "android_system_stubs_current",
        ),
        (
            "module-lib",
            ".module_lib",
            "module_current",
            "android_module_lib_stubs_current",
        ),
        (
            "system-server",
            ".system_server",
            "system_server_current",
            "android_system_server_stubs_current",
        ),
    ] {
        let dir = source.join("prebuilts/sdk/current").join(scope);
        let mut jars: Vec<_> = fs::read_dir(&dir)?.collect::<std::io::Result<_>>()?;
        jars.sort_by_key(|e| e.file_name());
        for jar in jars {
            let path = jar.path();
            if path.extension().is_none_or(|e| e != "jar") {
                continue;
            }
            let stem = path.file_stem().unwrap().to_str().ok_or("bad SDK name")?;
            let names = if stem == "android" {
                vec![combined.to_string(), format!("{combined}_exportable")]
            } else if ["android-non-updatable", "android.net.ipsec.ike"].contains(&stem)
                || stem.starts_with("framework-")
                || stem.starts_with("service-")
            {
                vec![
                    format!("{stem}.stubs{suffix}"),
                    format!("{stem}.stubs.exportable{suffix}"),
                ]
            } else {
                continue;
            };
            for name in names {
                if stem == "framework-permission-s" && scope == "module-lib" {
                    text += &format!(
                        "\njava_library {{ name: \"{name}\", static_libs: [\"droidloom-permission-s-api-stubs\"], sdk_version: \"{version}\", is_stubs_module: true, installable: false, visibility: [\"//visibility:public\"], }}\n"
                    );
                } else {
                    text += &format!(
                        "\njava_import {{ name: \"{name}\", jars: [\"current/{scope}/{stem}.jar\"], sdk_version: \"{version}\", prefer: true, is_stubs_module: true, installable: false, visibility: [\"//visibility:public\"], }}\n"
                    );
                }
            }
        }
    }
    p.put(&sdk, text)
}
fn bootstrap(source: &Path, out: &Path) -> Result<()> {
    let goroot = source.join("prebuilts/go/linux-x86");
    let generated = out.join("microfactory-main.go");
    let code = fs::read_to_string(source.join("build/blueprint/microfactory/microfactory.go"))?
        .replace("package microfactory", "package main")
        + "\nfunc main() { Main() }\n";
    write(&generated, code)?;
    for (binary, package) in [
        ("soong_ui", "android/soong/cmd/soong_ui"),
        ("mk2rbc", "android/soong/mk2rbc/mk2rbc"),
        ("rbcrun", "rbcrun/rbcrun"),
        (
            "release-config",
            "android/soong/cmd/release_config/release_config",
        ),
    ] {
        let mut command = Command::new(goroot.join("bin/go"));
        command
            .current_dir(source)
            .env("GOROOT", &goroot)
            .env("GOCACHE", out.join("go-cache"))
            .env("GOWORK", "off")
            .env("GO111MODULE", "off")
            .arg("run")
            .arg(&generated)
            .arg("-b")
            .arg(out.join("microfactory_Linux"));
        for (name, path) in [
            ("github.com/google/blueprint", "build/blueprint"),
            ("android/soong", "build/soong"),
            (
                "prebuilts/bazel/common/proto",
                "prebuilts/bazel/common/proto",
            ),
            ("rbcrun", "build/make/tools/rbcrun"),
            ("google.golang.org/protobuf", "external/golang-protobuf"),
            ("go.starlark.net", "external/starlark-go"),
            ("github.com/akrylysov/pogreb", "external/pogreb"),
        ] {
            command
                .arg("-pkg-path")
                .arg(format!("{name}={}", source.join(path).display()));
        }
        command
            .arg("-trimpath")
            .arg(source)
            .arg("-o")
            .arg(out.join(binary))
            .arg(package);
        run_build(&mut command)?;
    }
    Ok(())
}
fn preserve_input_manager_layout(original: &str, patched: &str) -> Result<()> {
    // This existing Binder override replaces an inherited virtual method and
    // changes neither allocation size nor the inherited vtable slots.
    const BINDER_OVERRIDE: &str = "    status_t onTransact(uint32_t code, const Parcel& data, Parcel* reply,\n                        uint32_t flags) override;\n";
    if patched != original && patched.replacen(BINDER_OVERRIDE, "", 1) != original {
        return fail(
            "InputManager layout changed but prebuilt libandroid_servers is retained; preserve the allocation/layout ABI",
        );
    }
    Ok(())
}

pub fn build(
    repo: &Path,
    source: &Path,
    out: &Path,
    work: &Path,
    product: &str,
    jobs: usize,
) -> Result<()> {
    let targets = full_targets(product);
    build_targets(repo, source, out, work, product, jobs, &targets)
}
fn full_targets(product: &str) -> Vec<&'static str> {
    TARGETS
        .iter()
        .copied()
        .filter(|name| product != "droidloom_arm64" || *name != "droidloom-native-bridge")
        .collect()
}
pub fn build_targets(
    repo: &Path,
    source: &Path,
    out: &Path,
    work: &Path,
    product: &str,
    jobs: usize,
    targets: &[&str],
) -> Result<()> {
    let lock = repo.join("android/manifest/m2-sparse-source-lock.json");
    let source_lock = droidloom_source::load_lock(&lock)?;
    if !source.exists() {
        droidloom_source::materialize(&lock, &source_lock, source)?;
    } else {
        droidloom_source::reconcile(&lock, &source_lock, source)?;
    }
    droidloom_source::verify_materialized(&lock, &source_lock, source)?;
    // Focused audio/framework builds neither rebuild nor record a translator
    // artifact. Keep existing translator development provenance intact.
    let framework_only = targets == ["services", "libinputflinger"]
        || targets == ["surfaceflinger"]
        || targets == TASK_BACKEND_TARGETS;
    let audio_only = targets == AUDIO_TARGETS;
    if !audio_only {
        remove_retired_vendor_modules(&out.join("target/product").join(product))?;
    }
    let native_bridge_source = if audio_only || framework_only || product == "droidloom_arm64" {
        None
    } else {
        Some(crate::native_bridge::prepare_source(repo, source)?)
    };
    for (aidl, adapter) in [
        ("statusbar/IStatusBar.aidl", "EmptyStatusBar"),
        ("policy/IKeyguardService.aidl", "EmptyKeyguard"),
    ] {
        let digest = hash(
            &source
                .join("frameworks/base/core/java/com/android/internal")
                .join(aidl),
        )?;
        let generated = fs::read_to_string(repo.join(format!(
            "android/framework/droidloom-systemui/src/com/android/systemui/compat/{adapter}.java"
        )))?;
        if !generated
            .lines()
            .any(|line| line == format!("// AIDL SHA-256: {digest}"))
        {
            return fail(
                "SystemUI Binder adapters are stale; run tools/droidloom-systemui-stubs for the pinned source",
            );
        }
    }
    let mut p = Projection::new(work.join("source-projection.json"))?;
    // The prebuilt system JNI library allocates InputManager itself. Preserve
    // its C++ layout unless that consumer is rebuilt and shipped together.
    let input_manager_header =
        source.join("frameworks/native/services/inputflinger/InputManager.h");
    let input_manager_layout = fs::read_to_string(&input_manager_header)?;
    for (cwd, patch) in PATCHES {
        p.patch(source, cwd, &repo.join(patch))?;
    }
    preserve_input_manager_layout(
        &input_manager_layout,
        &fs::read_to_string(&input_manager_header)?,
    )?;
    project_blueprints(&mut p, source)?;
    for relative in [
        "frameworks/base/tools/aapt2/integration-tests",
        "frameworks/base/core/tests",
        "frameworks/base/media/tests",
        "art/test",
        "art/libartservice/service",
        "packages/modules/Virtualization/guest/trusty/test_vm",
        "packages/modules/Virtualization/guest/trusty/test_vm_os",
        "packages/modules/Connectivity/framework",
        "packages/modules/Connectivity/Tethering/common/TetheringLib",
        "frameworks/base/services/tests",
        "frameworks/base/services/robotests",
        // Vendor metadata enumerates HIDL interfaces globally. These upstream
        // test fixtures reference test-only HALs omitted from the sparse tree.
        "system/tools/hidl/test",
        "system/tools/hidl/hidl2aidl/test",
    ] {
        p.put(&source.join(relative).join(".find-ignore"), b"")?;
    }
    // Mirror current sources on every invocation; deletion is propagated too.
    let vendor = source.join("vendor/droidloom");
    fs::create_dir_all(&vendor)?;
    for relative in [
        "Android.bp",
        "LICENSE",
        "LICENSES",
        "graphics",
        "runtime/droidloom-cpu-placement",
        "runtime/droidloom-timing",
        "runtime/droidloom-audio-protocol",
        "android/audio",
        "android/framework",
        "android/power",
        "android/lmkd-compat",
        "android/runtime",
        "android/native-bridge",
        "android/selinux-compat",
        "android/device",
        "device",
        "android/mesa",
    ] {
        let destination = vendor.join(relative);
        if repo.join(relative).is_dir() {
            mirror_directory(&repo.join(relative), &destination)?;
        } else {
            write(&destination, fs::read(repo.join(relative))?)?;
        }
    }
    crate::native_bridge::prepare_build(source, &vendor, work, &mut p)?;
    let mesa = vendor.join("mesa3d");
    fs::create_dir_all(&mesa)?;
    run(Command::new("rsync")
        .args(["-a", "--delete"])
        .arg(format!("{}/", work.join("mesa-source").display()))
        .arg(&mesa))?;
    p.patch(
        source,
        "vendor/droidloom/mesa3d",
        &repo.join("android/aosp-patches/0011-mesa-zink-kgsl.patch"),
    )?;
    p.patch(
        source,
        "vendor/droidloom/mesa3d",
        &repo.join("android/aosp-patches/0012-mesa-adreno722.patch"),
    )?;
    p.patch(
        source,
        "vendor/droidloom/mesa3d",
        &repo.join("android/aosp-patches/0013-mesa-texture-upload-span.patch"),
    )?;
    p.patch(
        source,
        "vendor/droidloom/mesa3d",
        &repo.join("android/aosp-patches/0014-mesa-background-cpu-placement.patch"),
    )?;
    p.patch(
        source,
        "vendor/droidloom/mesa3d",
        &repo.join("android/aosp-patches/0016-mesa-turnip-hwui-queues.patch"),
    )?;
    p.patch(
        source,
        "vendor/droidloom/mesa3d",
        &repo.join("android/aosp-patches/0018-mesa-ir3-alias-array-sources.patch"),
    )?;
    p.patch(
        source,
        "vendor/droidloom/mesa3d",
        &repo.join("android/aosp-patches/0019-mesa-turnip-a722-no-bv.patch"),
    )?;
    let cross = mesa.join("android/mesa3d_cross.mk");
    let cross_text = fs::read_to_string(&cross)?;
    let python_assignment = "MESA3D_PYTHONPATH := $(AOSP_ABSOLUTE_PATH)/external/python/mako";
    if !cross_text.contains(python_assignment) {
        return fail("Mesa Python build projection no longer matches its pinned source");
    }
    p.put(
        &cross,
        cross_text.replace(
            python_assignment,
            &format!("MESA3D_PYTHONPATH := {}", work.join("python").display()),
        ),
    )?;
    p.put(
        &vendor.join("android/prebuilts/framework-res.apk"),
        fs::read(work.join("framework-res.apk"))?,
    )?;
    let board = vendor.join(format!("android/device/{product}/BoardConfig.mk"));
    let native = work.join("mesa-native.ini");
    let mesa_tools = work.join(if product == "droidloom_arm64" {
        "mesa-panfrost-tools"
    } else {
        "mesa-tools"
    });
    let mut native_tools = format!(
        "[binaries]\nmesa_clc = {}\nvtn_bindgen2 = {}\n",
        meson_string(&mesa_tools.join("src/compiler/clc/mesa_clc"))?,
        meson_string(&mesa_tools.join("src/compiler/spirv/vtn_bindgen2"))?
    );
    if product == "droidloom_arm64" {
        native_tools += &format!(
            "panfrost_compile = {}\n",
            meson_string(&mesa_tools.join("src/panfrost/clc/panfrost_compile"))?
        );
    }
    write(&native, native_tools)?;
    p.put(
        &board,
        fs::read_to_string(&board)?
            + &format!(
                "\nBOARD_MESA3D_MESON_ARGS += -Dmesa-clc=system {}--native-file {}\n",
                if product == "droidloom_arm64" {
                    "-Dprecomp-compiler=system "
                } else {
                    ""
                },
                native.display()
            ),
    )?;
    // Generate the init projection from the pinned source with the same checked patch.
    let init = tempfile::Builder::new().prefix("init-").tempdir_in(work)?;
    copy(
        &source.join("system/core/rootdir/init.rc"),
        &init.path().join("rootdir/init.rc"),
    )?;
    run(Command::new("git")
        .env("GIT_CEILING_DIRECTORIES", work)
        .current_dir(init.path())
        .arg("apply")
        .arg(repo.join("android/apex-compat/0001-droidloom-classpath-projection.patch")))?;
    copy(&init.path().join("rootdir/init.rc"), &work.join("init.rc"))?;
    p.stabilize_all()?;
    bootstrap(source, out)?;
    copy(
        &source.join("prebuilts/build-tools/common/framework/turbine.jar"),
        &out.join("host/linux-x86/framework/turbine.jar"),
    )?;
    let mut cmd = Command::new(out.join("soong_ui"));
    cmd.current_dir(source)
        .env("SOONG_NINJA", "ninja")
        .env("TOP", source)
        .env("OUT_DIR", out)
        .env("ORIGINAL_PWD", source)
        .env(
            "PATH",
            format!(
                "{}:{}:{}",
                mesa_tools.join("src/compiler/clc").display(),
                mesa_tools.join("src/compiler/spirv").display(),
                std::env::var("PATH")?
            ),
        )
        .env("CCACHE_DIR", work.join("ccache"))
        .env("CCACHE_TEMPDIR", work.join("ccache-tmp"))
        .env("TARGET_PRODUCT", product)
        .env("TARGET_RELEASE", "cp2a")
        .env("TARGET_BUILD_VARIANT", "userdebug")
        .env("JAVA_HOME", source.join("prebuilts/jdk/jdk21/linux-x86"))
        .env("ALLOW_MISSING_DEPENDENCIES", "true")
        .env("SOONG_INCREMENTAL_ANALYSIS", "false")
        .arg("--make-mode")
        .arg(format!("-j{jobs}"))
        .args(targets.iter().flat_map(|name| {
            if framework_only {
                let mut outputs = vec![
                    out.join(framework_output(name, product).expect("known framework target")),
                ];
                if *name == "services" {
                    outputs.push(out.join(SERVICES_PROFILE));
                }
                outputs
            } else {
                vec![
                    host_output(name)
                        .map(PathBuf::from)
                        .or_else(|| apex_output_for_product(name, product))
                        .map_or_else(|| PathBuf::from(name), |path| out.join(path)),
                ]
            }
        }));
    if framework_only {
        cmd.env("SOONG_ONLY", "true");
    }
    let result = run_build(&mut cmd);
    p.restore()?;
    result?;
    if framework_only {
        for name in targets {
            let destination = match *name {
                name if name.starts_with("libdroidloom_") => {
                    format!("vendor/lib64/{name}.dylib.so")
                }
                name => match name {
                    "services" => "system/framework/services.jar",
                    "libinputflinger" => "system/lib64/libinputflinger.so",
                    "surfaceflinger" => "system/bin/surfaceflinger",
                    "DroidloomIME" => "system/app/DroidloomIME/DroidloomIME.apk",
                    "DroidloomSystemUIProbe" => "system/framework/DroidloomSystemUIProbe.jar",
                    "DroidloomSystemUI" => {
                        "system/system_ext/priv-app/DroidloomSystemUI/DroidloomSystemUI.apk"
                    }
                    "android.hardware.graphics.composer3-service.droidloom" => {
                        "vendor/bin/hw/android.hardware.graphics.composer3-service.droidloom"
                    }
                    _ => unreachable!(),
                }
                .to_owned(),
            };
            copy(
                &out.join(framework_output(name, product).expect("known framework target")),
                &out.join("target/product").join(product).join(destination),
            )?;
            if *name == "services" {
                copy(
                    &out.join(SERVICES_PROFILE),
                    &out.join("target/product")
                        .join(product)
                        .join("system/framework/services.jar.prof"),
                )?;
            }
        }
    }
    if let Some(native_bridge_source) = native_bridge_source {
        write(
            &out.join("target/product")
                .join(product)
                .join("droidloom-native-bridge-source.json"),
            serde_json::to_vec_pretty(&native_bridge_source)?,
        )?;
    }
    Ok(())
}
// Compile outputs directly for the focused framework check, avoiding Make's
// unrelated installation closure and retaining the normal product staging paths.
fn framework_output(name: &str, product: &str) -> Option<PathBuf> {
    match name {
        name if name.starts_with("libdroidloom_") && TASK_BACKEND_TARGETS.contains(&name) => {
            let arch = match product {
                "droidloom_x86_64" => "x86_64",
                "droidloom_arm64" => "arm64_armv8-a",
                _ => return None,
            };
            Some(PathBuf::from(format!(
                "soong/.intermediates/vendor/droidloom/{name}/android_vendor_{arch}_dylib/{name}.dylib.so"
            )))
        }
        "services" => Some(PathBuf::from(
            "soong/.intermediates/frameworks/base/services/services/android_common/aligned/services.jar",
        )),
        "surfaceflinger" => {
            let arch = match product {
                "droidloom_x86_64" => "x86_64",
                "droidloom_arm64" => "arm64_armv8-a",
                _ => return None,
            };
            Some(PathBuf::from(format!(
                "soong/.intermediates/frameworks/native/services/surfaceflinger/surfaceflinger/android_{arch}/surfaceflinger"
            )))
        }
        "libinputflinger" => {
            let variant = match product {
                "droidloom_x86_64" => "android_x86_64_shared",
                "droidloom_arm64" => "android_arm64_armv8-a_shared",
                _ => return None,
            };
            Some(PathBuf::from(format!(
                "soong/.intermediates/frameworks/native/services/inputflinger/libinputflinger/{variant}/libinputflinger.so"
            )))
        }
        "DroidloomIME" | "DroidloomSystemUI" => Some(PathBuf::from(format!(
            "soong/.intermediates/vendor/droidloom/{name}/android_common/{name}.apk"
        ))),
        "DroidloomSystemUIProbe" => Some(PathBuf::from(
            "soong/.intermediates/vendor/droidloom/DroidloomSystemUIProbe/android_common/dex/DroidloomSystemUIProbe.jar",
        )),
        "android.hardware.graphics.composer3-service.droidloom" => {
            let arch = match product {
                "droidloom_x86_64" => "x86_64",
                "droidloom_arm64" => "arm64_armv8-a",
                _ => return None,
            };
            Some(PathBuf::from(format!(
                "soong/.intermediates/vendor/droidloom/{name}/android_vendor_{arch}/{name}"
            )))
        }
        _ => None,
    }
}

// Files of modules removed from PRODUCT_PACKAGES.
const RETIRED_VENDOR_FILES: &[&str] = &[
    "vendor/bin/droidloom-input-bridge",
    "vendor/framework/droidloom-input-bridge.jar",
    "vendor/etc/init/droidloom-input-bridge.rc",
    "vendor/bin/droidloom-task-launcher",
    "vendor/lib64/libdroidloom_task_launcher.dylib.so",
    "vendor/framework/droidloom-application-catalog.jar",
    // Replaced by android.hardware.power-service.droidloom, which registers
    // the same IPower/default instance.
    "vendor/bin/hw/android.hardware.power-service.example",
    "vendor/etc/init/power-default.rc",
    "vendor/etc/vintf/manifest/power-default.xml",
];

fn remove_retired_vendor_modules(product: &Path) -> Result<()> {
    // Incremental product trees can retain files from a module removed from
    // PRODUCT_PACKAGES. Never let its old init service enter a new vendor image.
    for relative in RETIRED_VENDOR_FILES {
        match fs::remove_file(product.join(relative)) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn mirror_directory(source: &Path, destination: &Path) -> Result<()> {
    fs::create_dir_all(destination)?;
    // A source can change while a preceding build compiles its older mirror.
    // Retaining that source's earlier mtime makes Ninja miss the new contents.
    // Compare contents and timestamp changed copies at projection time instead.
    run(Command::new("rsync")
        .args(["-a", "--checksum", "--no-times", "--delete"])
        .arg(format!("{}/", source.display()))
        .arg(destination))
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_arm_build_omits_the_x86_isa_translator() {
        let arm = full_targets("droidloom_arm64");
        assert!(!arm.contains(&"droidloom-native-bridge"));
        assert!(arm.contains(&"vendorimage"));
        assert!(arm.contains(&"surfaceflinger"));
        assert!(full_targets("droidloom_x86_64").contains(&"droidloom-native-bridge"));
        for target in ["netbpfload", "libservice-connectivity", "libnetd_updatable"] {
            let path = apex_output_for_product(target, "droidloom_arm64").unwrap();
            assert!(path.to_str().unwrap().contains("android_arm64_armv8-a"));
            assert!(!path.to_str().unwrap().contains("x86_64"));
            assert_eq!(
                apex_output_for_product(target, "droidloom_x86_64").unwrap(),
                PathBuf::from(apex_output(target).unwrap())
            );
        }
        assert!(apex_output_for_product("netbpfload", "unknown").is_none());
    }

    #[test]
    fn retired_service_is_pruned_from_incremental_products_without_removing_neighbors() {
        let root = tempfile::tempdir().unwrap();
        let retired = RETIRED_VENDOR_FILES;
        for relative in retired {
            write(&root.path().join(relative), b"retired").unwrap();
        }
        let neighbor = root.path().join("vendor/etc/init/droidloom-composer.rc");
        write(&neighbor, b"preserve").unwrap();
        remove_retired_vendor_modules(root.path()).unwrap();
        remove_retired_vendor_modules(root.path()).unwrap();
        assert!(retired.iter().all(|path| !root.path().join(path).exists()));
        assert!(
            !root
                .path()
                .join("vendor/etc/init/droidloom-input-bridge.rc")
                .exists()
        );
        assert!(
            !root
                .path()
                .join("vendor/bin/droidloom-input-bridge")
                .exists()
        );
        assert!(
            !root
                .path()
                .join("vendor/framework/droidloom-input-bridge.jar")
                .exists()
        );
        assert_eq!(fs::read(neighbor).unwrap(), b"preserve");
    }

    #[test]
    fn focused_task_targets_have_outputs_for_each_product() {
        for product in ["droidloom_x86_64", "droidloom_arm64"] {
            for target in TASK_BACKEND_TARGETS {
                assert!(
                    framework_output(target, product).is_some(),
                    "{product}: {target}"
                );
            }
        }
        assert!(TASK_BACKEND_TARGETS.contains(&"surfaceflinger"));
        assert!(framework_output("surfaceflinger", "unknown").is_none());
    }

    #[test]
    fn prebuilt_input_manager_allocator_rejects_added_fields_and_virtual_slots() {
        let original = "class InputManager {\nprivate:\n    void* reader;\n};\n";
        assert!(super::preserve_input_manager_layout(original, original).is_ok());
        let overridden = original.replace("private:", "    status_t onTransact(uint32_t code, const Parcel& data, Parcel* reply,\n                        uint32_t flags) override;\nprivate:");
        assert!(super::preserve_input_manager_layout(original, &overridden).is_ok());
        for added in ["    void* nativeInput;\n", "    virtual void newSlot();\n"] {
            let incompatible = overridden.replace("private:", &format!("{added}private:"));
            assert!(super::preserve_input_manager_layout(original, &incompatible).is_err());
        }
    }

    #[test]
    fn mirrored_edits_older_than_build_outputs_still_invalidate_ninja_inputs() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source");
        let destination = directory.path().join("mirror");
        fs::create_dir(&source).unwrap();
        let input = source.join("input.rs");
        fs::write(&input, b"old contents").unwrap();
        mirror_directory(&source, &destination).unwrap();
        let mirrored = destination.join("input.rs");
        let previous = fs::metadata(&mirrored).unwrap().modified().unwrap();
        fs::write(&input, b"new contents").unwrap();
        fs::File::open(&input)
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH)
            .unwrap();
        mirror_directory(&source, &destination).unwrap();
        assert_eq!(fs::read(&mirrored).unwrap(), b"new contents");
        let updated = fs::metadata(&mirrored).unwrap().modified().unwrap();
        assert!(updated > previous);
        mirror_directory(&source, &destination).unwrap();
        assert_eq!(
            fs::metadata(&mirrored).unwrap().modified().unwrap(),
            updated
        );
        fs::remove_file(input).unwrap();
        mirror_directory(&source, &destination).unwrap();
        assert!(!mirrored.exists());
    }
    #[test]
    fn repeated_projection_keeps_ninja_outputs_until_content_changes() {
        let d = tempfile::tempdir().unwrap();
        let input = d.path().join("input");
        write(&input, b"original").unwrap();
        let original_time = fs::metadata(&input).unwrap().modified().unwrap();
        write(
            &d.path().join("build.ninja"),
            b"rule compile\n  command = cp $in $out\nbuild result: compile input\n",
        )
        .unwrap();
        let ninja = || output(Command::new("ninja").current_dir(d.path())).unwrap();
        let journal = d.path().join("journal.json");
        let mut p = Projection::new(journal.clone()).unwrap();
        p.put(&input, b"intermediate patch").unwrap();
        p.put(&input, b"patched").unwrap();
        p.stabilize_all().unwrap();
        ninja();
        let result_time = fs::metadata(d.path().join("result"))
            .unwrap()
            .modified()
            .unwrap();
        p.restore().unwrap();
        assert_eq!(
            fs::metadata(&input).unwrap().modified().unwrap(),
            original_time
        );
        let mut p = Projection::new(journal).unwrap();
        p.put(&input, b"intermediate patch").unwrap();
        p.put(&input, b"patched").unwrap();
        p.stabilize_all().unwrap();
        assert!(ninja().contains("no work to do"));
        assert_eq!(
            fs::metadata(d.path().join("result"))
                .unwrap()
                .modified()
                .unwrap(),
            result_time
        );
        p.put(&input, b"changed patch").unwrap();
        p.stabilize_all().unwrap();
        assert!(!ninja().contains("no work to do"));
        assert_eq!(fs::read(d.path().join("result")).unwrap(), b"changed patch");
    }
    #[test]
    fn patched_directories_keep_their_timestamps() {
        let d = tempfile::tempdir().unwrap();
        let source = d.path().join("source");
        fs::create_dir_all(&source).unwrap();
        write(&source.join("input"), b"before\n").unwrap();
        let patch = d.path().join("change.patch");
        write(
            &patch,
            b"--- a/input\n+++ b/input\n@@ -1 +1 @@\n-before\n+after\n\
              --- /dev/null\n+++ b/created\n@@ -0,0 +1 @@\n+new\n",
        )
        .unwrap();
        let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        fs::File::open(&source).unwrap().set_modified(old).unwrap();
        let time = || fs::metadata(&source).unwrap().modified().unwrap();
        let mut p = Projection::new(d.path().join("journal")).unwrap();
        p.patch(&source, "", &patch).unwrap();
        p.stabilize_all().unwrap();
        assert_eq!(fs::read(source.join("created")).unwrap(), b"new\n");
        assert_eq!(time(), old);
        p.restore().unwrap();
        assert!(!source.join("created").exists());
        assert_eq!(time(), old);
    }
    #[test]
    fn restoration_preserves_original_and_removes_created_file() {
        let d = tempfile::tempdir().unwrap();
        let original = d.path().join("original");
        write(&original, b"before").unwrap();
        {
            let mut p = Projection::new(d.path().join("journal")).unwrap();
            p.put(&original, b"after").unwrap();
            p.put(&d.path().join("created"), b"x").unwrap();
        }
        assert_eq!(fs::read(original).unwrap(), b"before");
        assert!(!d.path().join("created").exists());
    }
    #[test]
    fn complete_target_set_contains_both_wire_peers_and_framework() {
        for name in [
            "android.hardware.graphics.composer3-service.droidloom",
            "droidloom-framework-init",
            "surfaceflinger",
            "libinputflinger",
            "services",
            "vendorimage",
        ] {
            assert!(TARGETS.contains(&name));
        }
    }
}

#[cfg(test)]
mod patch_tests {
    use super::*;
    #[test]
    fn patch_in_source_without_git_root_is_restored_inside_enclosing_checkout() {
        let d = tempfile::tempdir().unwrap();
        run(Command::new("git").arg("init").arg("--quiet").arg(d.path())).unwrap();
        let source = d.path().join("work/source");
        fs::create_dir_all(&source).unwrap();
        write(&source.join("input"), b"before\n").unwrap();
        let patch = d.path().join("change.patch");
        write(
            &patch,
            b"--- a/input\n+++ b/input\n@@ -1 +1 @@\n-before\n+after\n",
        )
        .unwrap();
        {
            let mut p = Projection::new(d.path().join("journal")).unwrap();
            p.patch(&source, "", &patch).unwrap();
            assert_eq!(fs::read(source.join("input")).unwrap(), b"after\n");
        }
        assert_eq!(fs::read(source.join("input")).unwrap(), b"before\n");
    }
    #[test]
    fn interrupted_projection_recovers_before_next_build() {
        let d = tempfile::tempdir().unwrap();
        let source = d.path().join("source");
        fs::create_dir(&source).unwrap();
        let path = source.join("input");
        write(&path, b"original").unwrap();
        let journal = d.path().join("journal");
        let mut p = Projection::new(journal.clone()).unwrap();
        p.put(&path, b"modified").unwrap();
        std::mem::forget(p);
        recover(&journal, &source).unwrap();
        assert_eq!(fs::read(path).unwrap(), b"original");
        assert!(!journal.exists());
    }
}

fn meson_string(path: &Path) -> Result<String> {
    Ok(format!(
        "'{}'",
        path.to_str()
            .ok_or("non-UTF8 tool path")?
            .replace('\\', "\\\\")
            .replace('\'', "\\'")
    ))
}
