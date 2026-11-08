//! Consistency checks for checked-in, language-neutral contracts.

use std::fs;
use std::path::{Path, PathBuf};

use droidloom_contracts::{
    PINNED_AOSP_COMMIT, PINNED_AOSP_TAG, PINNED_MESA_SHA256, PINNED_MESA_VERSION,
};
use serde_json::Value;
use std::collections::BTreeSet;

fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn json(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

#[test]
fn shared_wayland_protocols_match_denial() {
    // CI supplies its independent Denial checkout. Ordinary source-only tests
    // do not require a second repository, but an explicit path must be valid.
    let Some(denial) = std::env::var_os("DROIDLOOM_DENIAL_SOURCE") else {
        return;
    };
    let root = repository_root();
    let denial = PathBuf::from(denial).join("compositor/protocol");
    for (local, name) in [
        (
            "graphics/droidloom-wayland/protocol/denial-insets-v1.xml",
            "denial-insets-v1.xml",
        ),
        (
            "graphics/droidloom-wayland/protocol/denial-text-input-panel-v1.xml",
            "denial-text-input-panel-v1.xml",
        ),
        (
            "protocol/denial-frame-timeline-v1.xml",
            "denial-frame-timeline-v1.xml",
        ),
        (
            "runtime/droidloom-devices/protocol/denial-fingerprint-verify-v1.xml",
            "denial-fingerprint-verify-v1.xml",
        ),
    ] {
        let local_bytes = fs::read(root.join(local)).unwrap();
        let upstream_bytes = fs::read(denial.join(name))
            .unwrap_or_else(|error| panic!("cannot read Denial {name}: {error}"));
        assert!(
            local_bytes == upstream_bytes,
            "{local} differs from Denial compositor/protocol/{name}; update both canonical copies together"
        );
    }
}

#[test]
fn checked_in_json_contracts_are_well_formed() {
    let root = repository_root();
    for relative in [
        "android/manifest/source-lock.json",
        "android/manifest/source-lock-x86_64.json",
        "android/manifest/m2-sparse-source-lock.json",
        "protocol/schemas/image-manifest-v1.schema.json",
        "protocol/schemas/doctor-report-v1.schema.json",
        "docs/examples/cell-spec-u1000.json",
        "android/hardware/composer/aidl-lock.json",
        "android/hardware/composer/service-contract.json",
        "android/device/droidloom-arm64-product.json",
        "android/device/droidloom-x86_64-product.json",
    ] {
        let value = json(&root.join(relative));
        assert!(value.is_object(), "{relative} must contain a JSON object");
    }
}

#[test]
fn rust_and_source_lock_pins_match() {
    let root = repository_root();
    let lock = json(&root.join("android/manifest/source-lock.json"));
    assert_eq!(lock["aosp"]["tag"], PINNED_AOSP_TAG);
    assert_eq!(lock["aosp"]["commit"], PINNED_AOSP_COMMIT);
    assert_eq!(lock["mesa"]["version"], PINNED_MESA_VERSION);
    assert_eq!(lock["mesa"]["sha256"], PINNED_MESA_SHA256);
    assert_eq!(
        lock["aosp_ci_base"]["framework_res"]["path"],
        "system/framework/framework-res.apk"
    );
    assert_eq!(
        lock["aosp_ci_base"]["framework_res"]["sha256"]
            .as_str()
            .unwrap()
            .len(),
        64
    );

    let x86 = json(&root.join("android/manifest/source-lock-x86_64.json"));
    assert_eq!(x86["architecture"], "x86_64");
    assert_eq!(x86["aosp"]["tag"], PINNED_AOSP_TAG);
    assert_eq!(x86["aosp"]["commit"], PINNED_AOSP_COMMIT);
    assert_eq!(x86["mesa"]["version"], PINNED_MESA_VERSION);
    assert_eq!(x86["mesa"]["sha256"], PINNED_MESA_SHA256);
    assert_eq!(x86["aosp_ci_base"]["cpu_abi"], "x86_64");
    assert_eq!(
        x86["aosp_ci_base"]["framework_res"],
        lock["aosp_ci_base"]["framework_res"]
    );
}

#[test]
fn sparse_aosp_lock_is_exact_and_bounded() {
    let lock = json(&repository_root().join("android/manifest/m2-sparse-source-lock.json"));
    assert_eq!(lock["policy"]["allow_unrestricted_repo_sync"], false);
    assert_eq!(lock["policy"]["require_exact_commits"], true);

    let projects = lock["projects"].as_array().unwrap();
    assert!(!projects.is_empty());
    let mut paths = BTreeSet::new();
    let mut names = BTreeSet::new();
    for project in projects {
        let path = project["path"].as_str().unwrap();
        let name = project["name"].as_str().unwrap();
        let commit = project["commit"].as_str().unwrap();
        assert!(
            !path.starts_with('/') && !path.split('/').any(|part| part.is_empty() || part == ".."),
            "invalid sparse project path {path}"
        );
        assert!(
            name.starts_with("platform/")
                || name.starts_with("kernel/")
                || name.starts_with("toolchain/")
                || name == "tools/platform-compat",
            "project {name} is outside the bounded official namespaces"
        );
        assert!(
            commit.len() == 40 && commit.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "project {path} must use an exact commit"
        );
        assert!(paths.insert(path), "duplicate sparse path {path}");
        assert!(names.insert(name), "duplicate sparse project {name}");
    }
}

#[test]
fn android_graphics_abi_lock_comes_from_the_sparse_source_commit() {
    let root = repository_root();
    let sparse = json(&root.join("android/manifest/m2-sparse-source-lock.json"));
    let aidl = json(&root.join("android/hardware/composer/aidl-lock.json"));
    let hardware_interfaces = sparse["projects"]
        .as_array()
        .unwrap()
        .iter()
        .find(|project| project["name"] == "platform/hardware/interfaces")
        .unwrap();
    assert_eq!(aidl["source"]["commit"], hardware_interfaces["commit"]);
    assert_eq!(aidl["composer"]["version"], 5);
    assert_eq!(aidl["composer"]["backend"], "rust");
    assert_eq!(aidl["allocator"]["version"], 3);
    for hash in [&aidl["composer"]["hash"], &aidl["allocator"]["hash"]] {
        let hash = hash.as_str().unwrap();
        assert!(
            hash.len() == 40 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "frozen AIDL hashes must be exact SHA-1 values"
        );
    }
}

#[test]
fn android_tasks_are_native_windows_not_one_emulator_window() {
    let contract = json(&repository_root().join("android/hardware/composer/service-contract.json"));
    assert_eq!(
        contract["composition"]["display_model"],
        "one_host_managed_android_logical_display_per_top_level_task"
    );
    assert_eq!(
        contract["composition"]["combined_android_desktop_window"],
        false
    );
    assert_eq!(contract["composition"]["maximum_task_displays"], 4096);
}

#[test]
fn host_presentation_is_ordinary_wayland_without_plugin_artifacts() {
    let root = repository_root();
    let workspace = fs::read_to_string(root.join("Cargo.toml")).expect("read workspace manifest");
    assert!(workspace.contains("graphics/droidloom-wayland"));
    assert!(!workspace.contains("graphics/droidloom-denial-plugin"));
    assert!(!root.join("graphics/droidloom-denial-plugin").exists());
    assert!(
        !root
            .join("packaging/systemd/denial.service.d/50-droidloom-native-app-plugin.conf")
            .exists()
    );

    let presenter = fs::read_to_string(root.join("graphics/droidloom-wayland/src/main.rs"))
        .expect("read Wayland presenter");
    for required in [
        "create_window",
        "set_app_id",
        "get_default_feedback",
        "set_acquire_point",
        "set_release_point",
        "KeyboardHandler",
        "PointerHandler",
        "TouchHandler",
    ] {
        assert!(
            presenter.contains(required),
            "Wayland presenter is missing {required}"
        );
    }

    let service = fs::read_to_string(root.join("packaging/systemd/user/droidloom.service"))
        .expect("read user service");
    assert!(service.contains("Type=notify"));
    assert!(service.contains("ExecStart=/usr/bin/droidloom-wayland"));
    assert!(service.contains("ExecStartPost=/usr/bin/droidloomctl start"));
    assert!(service.contains("ExecStopPost=/usr/bin/systemctl stop droidloomd.service"));
    assert!(!service.contains("ExecStopPost=/usr/bin/droidloomctl stop"));
    assert!(service.contains("Wants=droidloom-applications.service"));

    let cell_service = fs::read_to_string(root.join("packaging/systemd/system/droidloomd.service"))
        .expect("read cell service");
    assert!(cell_service.contains("OOMPolicy=continue"));

    let catalog_service =
        fs::read_to_string(root.join("packaging/systemd/user/droidloom-applications.service"))
            .expect("read application catalog service");
    assert!(catalog_service.contains("ExecStart=/usr/bin/droidloom-applications"));

    let catalog = fs::read_to_string(root.join("runtime/droidloom-applications/src/lib.rs"))
        .expect("read XDG application catalog");
    assert!(catalog.contains("/usr/bin/droidloomctl launch"));
    assert!(!catalog.contains("/usr/bin/droidloomctl start"));

    let android_catalog = fs::read_to_string(root.join(
        "android/framework/droidloom-window-policy/java/com/android/server/wm/DroidloomApplicationCatalog.java",
    ))
    .expect("read Android application catalog adapter");
    assert!(android_catalog.contains("Intent.CATEGORY_LAUNCHER"));
    assert!(android_catalog.contains("queryIntentActivities"));
    assert!(android_catalog.contains("Bitmap.CompressFormat.PNG"));
}

#[test]
fn shared_kernel_vintf_reports_the_real_kernel_policy_version() {
    let root = repository_root();
    let matrix =
        fs::read_to_string(root.join("android/vintf-compat/compatibility_matrix.device.xml"))
            .expect("read shared-kernel VINTF matrix");
    assert!(matrix.contains("<kernel-sepolicy-version>0</kernel-sepolicy-version>"));

    let spec = json(&root.join("packaging/cell-spec-x86_64-u1000.json"));
    let overrides = spec["android_file_overrides"].as_array().unwrap();
    assert!(overrides.iter().any(|entry| {
        entry["source"] == "/usr/lib/droidloom/current/compat/vintf/compatibility_matrix.device.xml"
            && entry["target"] == "/system/etc/vintf/compatibility_matrix.device.xml"
    }));
}

#[test]
fn android_vendor_build_uses_the_safe_rust_composer_core() {
    let android_bp =
        fs::read_to_string(repository_root().join("Android.bp")).expect("read Android.bp");
    assert!(android_bp.contains("name: \"libdroidloom_transport\""));
    assert!(android_bp.contains("name: \"libdroidloom_denial_protocol\""));
    assert!(android_bp.contains("name: \"libdroidloom_denial_ipc\""));
    assert!(android_bp.contains("name: \"libdroidloom_syncobj\""));
    assert!(android_bp.contains("name: \"libdroidloom_composer\""));
    assert!(android_bp.contains("name: \"libdroidloom_minigbm\""));
    assert!(android_bp.contains("name: \"libdroidloom_composer_aidl\""));
    assert!(android_bp.contains("name: \"libdroidloom_task_control\""));
    assert!(!android_bp.contains("name: \"libdroidloom_task_launcher\""));
    assert!(!android_bp.contains("name: \"droidloom-task-launcher\""));
    assert!(android_bp.contains("name: \"droidloom-classpath-wrapper\""));
    assert!(!android_bp.contains("name: \"droidloom-application-catalog\""));
    assert!(android_bp.contains("name: \"android.hardware.graphics.composer3-service.droidloom\""));
    assert!(android_bp.contains("crate_root: \"graphics/droidloom-composer/src/lib.rs\""));
    assert!(android_bp.contains("crate_root: \"graphics/droidloom-minigbm/src/lib.rs\""));
    assert!(android_bp.contains("crate_root: \"graphics/droidloom-composer-aidl/src/lib.rs\""));
    assert!(android_bp.contains("\"android.hardware.graphics.composer3-V5-rust\""));
    assert!(android_bp.contains("crate_root: \"graphics/droidloom-denial-protocol/src/lib.rs\""));
    assert!(android_bp.contains("crate_root: \"graphics/droidloom-denial-ipc/src/lib.rs\""));
    assert!(android_bp.contains("name: \"android.hardware.power-service.droidloom\""));
    assert!(android_bp.contains("crate_root: \"android/power/droidloom-power/src/lib.rs\""));
    assert!(android_bp.contains("\"android.hardware.power-V7-rust\""));
    assert!(android_bp.contains("name: \"vulkan.droidloom\""));
    assert!(android_bp.contains("crate_root: \"android/gpu/droidloom-vulkan/src/lib.rs\""));
    // Five shared transport/codec crates also build platform variants for WMS
    // and its native input receiver. The haptics, camera and authentication
    // HALs add the device protocol, their libraries and their services.
    assert_eq!(android_bp.matches("vendor: true").count(), 24);

    let product = fs::read_to_string(
        repository_root().join("android/device/droidloom_arm64/droidloom_arm64.mk"),
    )
    .expect("read Droidloom product");
    assert!(product.contains("PRODUCT_BUILD_VENDOR_IMAGE := true"));
    assert!(product.contains("android.hardware.graphics.allocator-service.minigbm"));
    assert!(product.contains("mapper.minigbm"));
    assert!(product.contains("droidloom-classpath-wrapper"));
    // Hint sessions only help when HWUI opens them and the HAL acts on them.
    assert!(product.contains("android.hardware.power-service.droidloom"));
    assert!(!product.contains("android.hardware.power-service.example"));
    assert!(product.contains("debug.hwui.use_hint_manager=true"));
    assert!(product.contains("DroidloomConnectivityOverlay"));
    assert!(product.contains("android.hardware.ethernet.prebuilt.xml"));
    for forbidden in [
        "PRODUCT_BUILD_SYSTEM_IMAGE := true",
        "PRODUCT_BUILD_BOOT_IMAGE := true",
        "PRODUCT_BUILD_VENDOR_BOOT_IMAGE := true",
    ] {
        assert!(
            !product.contains(forbidden),
            "forbidden product output {forbidden}"
        );
    }
}

#[test]
fn native_targeted_input_keeps_the_boot_framework_abi_untouched() {
    let root = repository_root();
    let bridge = fs::read_to_string(root.join(
        "android/framework/droidloom-window-policy/java/com/android/server/policy/DroidloomHostChannels.java",
    ))
    .expect("read Droidloom input bridge");
    let surfaceflinger_patch = fs::read_to_string(
        root.join("android/surfaceflinger/0002-droidloom-task-input-token.patch"),
    )
    .expect("read SurfaceFlinger task-token patch");
    let inputflinger_patch = fs::read_to_string(
        root.join("android/inputflinger/0002-droidloom-targeted-injection-binder.patch"),
    )
    .expect("read InputFlinger targeted-injection patch");
    let init_patch = fs::read_to_string(
        root.join("android/apex-compat/0001-droidloom-classpath-projection.patch"),
    )
    .expect("read Android cell init patch");

    let native_input = fs::read_to_string(
        root.join("android/framework/droidloom-window-policy/jni/DroidloomInput.cpp"),
    )
    .expect("read native input adapter");
    let native_patch =
        fs::read_to_string(root.join("android/inputflinger/0004-droidloom-native-input.patch"))
            .expect("read native input patch");
    assert!(bridge.contains("0x00444c03"));
    assert!(native_input.contains("0x00444c03"));
    assert!(native_patch.contains("input->transact"));
    // Prebuilt libandroid_servers allocates InputManager with its original
    // sizeof. Native input must preserve that header's allocation/layout ABI.
    assert!(!native_patch.contains("diff --git a/services/inputflinger/InputManager.h"));
    assert!(
        !root
            .join("android/framework/droidloom-input-bridge")
            .exists()
    );
    assert!(surfaceflinger_patch.contains("0x00444c01"));
    assert!(inputflinger_patch.contains("0x00444c02"));
    assert!(!bridge.contains("MotionEvent"));
    assert!(!bridge.contains("getTaskBounds"));
    assert!(native_input.contains("injectInputEventToApplication"));
    assert!(bridge.contains("inputflinger"));
    assert!(init_patch.contains("setprop debug.wm.disable_deprecated_target_sdk_dialog true"));
    for forbidden in [
        "core/java/android/app/IActivityTaskManager.aidl",
        "core/java/android/hardware/input/IInputManager.aidl",
        "core/java/android/hardware/input/InputManagerGlobal.java",
    ] {
        assert!(
            !surfaceflinger_patch.contains(forbidden)
                && !inputflinger_patch.contains(forbidden)
                && !native_patch.contains(forbidden),
            "native task-targeted input must not change boot framework ABI through {forbidden}"
        );
    }
}

#[test]
fn host_display_power_reaches_android_power_manager_through_versioned_protocols() {
    let root = repository_root();
    let presenter = fs::read_to_string(root.join("graphics/droidloom-wayland/src/main.rs"))
        .expect("read Wayland presenter");
    let denial_protocol =
        fs::read_to_string(root.join("graphics/droidloom-denial-protocol/src/lib.rs"))
            .expect("read Denial protocol");
    let input_protocol =
        fs::read_to_string(root.join("graphics/droidloom-input-protocol/src/lib.rs"))
            .expect("read input protocol");
    let bridge = fs::read_to_string(root.join(
        "android/framework/droidloom-window-policy/java/com/android/server/policy/DroidloomHostChannels.java",
    ))
    .expect("read Droidloom input bridge");

    assert!(presenter.contains("ZwlrOutputPowerManagerV1"));
    assert!(presenter.contains("aggregate_display_power"));
    assert!(denial_protocol.contains("DisplayPower"));
    assert!(input_protocol.contains("encode_display_power"));
    assert!(bridge.contains("manager.goToSleep"));
    assert!(bridge.contains("manager.wakeUp"));
    assert!(bridge.contains("policyHandler.post"));
}

#[test]
fn android_task_geometry_does_not_conflate_density_with_wayland_scale() {
    let root = repository_root();
    let backend = fs::read_to_string(root.join(
        "android/framework/droidloom-window-policy/java/com/android/server/wm/DroidloomTaskBackend.java",
    ))
    .expect("read Droidloom framework task backend");
    assert!(
        backend.contains("mService.resizeTask(task.mTaskId, bounds, 0 /* RESIZE_MODE_SYSTEM */)")
    );
    assert!(!backend.contains("setDensityDpi"));
    assert!(!backend.contains("scaledDensity"));

    let surfaceflinger = fs::read_to_string(
        root.join("android/surfaceflinger/0001-droidloom-direct-denial-render-surface.patch"),
    )
    .expect("read Droidloom SurfaceFlinger patch");
    assert!(surfaceflinger.contains(".sourceCrop = compositionCrop"));
    assert!(!surfaceflinger.contains(".sourceCrop = frame.sourceCrop"));
}

#[test]
fn x86_64_product_uses_native_amd_and_intel_dmabufs_and_drivers() {
    let root = repository_root();
    let board = fs::read_to_string(root.join("android/device/droidloom_x86_64/BoardConfig.mk"))
        .expect("read x86_64 BoardConfig");
    assert!(board.contains("TARGET_ARCH := x86_64"));
    assert!(board.contains("SOONG_CONFIG_minigbm_platform := droidloom_x86"));
    assert!(board.contains("BOARD_MESA3D_GALLIUM_DRIVERS := radeonsi iris"));
    assert!(board.contains("BOARD_MESA3D_VULKAN_DRIVERS := amd"));
    assert!(board.contains("BOARD_MESA3D_MESON_ARGS := -Damd-use-llvm=false"));
    for forbidden in ["gfxstream", "virgl", "llvmpipe"] {
        assert!(
            !board
                .lines()
                .any(|line| { !line.trim_start().starts_with('#') && line.contains(forbidden) }),
            "x86_64 BoardConfig enables forbidden fallback {forbidden}"
        );
    }

    let product =
        fs::read_to_string(root.join("android/device/droidloom_x86_64/droidloom_x86_64.mk"))
            .expect("read x86_64 product");
    assert!(product.contains("PRODUCT_BUILD_VENDOR_IMAGE := true"));
    assert!(product.contains("android.hardware.graphics.allocator-service.minigbm"));
    assert!(product.contains("vulkan.radeon"));
    assert!(product.contains("ro.hardware.vulkan=radeon"));
    // The cell's `forwarding` graphics pairing selects it over radeon.
    assert!(product.contains("vulkan.droidloom"));
}

#[test]
fn android_task_displays_are_permanently_own_content_displays() {
    let root = repository_root();
    let product =
        fs::read_to_string(root.join("android/device/droidloom_arm64/droidloom_arm64.mk"))
            .expect("read Droidloom product");
    assert!(product.contains("DroidloomFrameworkDisplayOverlay"));

    let overlay_root = root.join("android/device/droidloom_arm64/framework-display-overlay");
    let blueprint = fs::read_to_string(overlay_root.join("Android.bp"))
        .expect("read Droidloom framework display overlay blueprint");
    assert!(blueprint.contains("name: \"DroidloomFrameworkDisplayOverlay\""));
    assert!(blueprint.contains("vendor: true"));

    let manifest = fs::read_to_string(overlay_root.join("AndroidManifest.xml"))
        .expect("read Droidloom framework display overlay manifest");
    assert!(manifest.contains("android:targetPackage=\"android\""));
    assert!(manifest.contains("android:isStatic=\"true\""));

    let resources = fs::read_to_string(overlay_root.join("res/values/config.xml"))
        .expect("read Droidloom framework display overlay resources");
    assert!(resources.contains("<bool name=\"config_localDisplaysMirrorContent\">false</bool>"));
}

#[test]
fn private_host_protocol_has_the_v1_contract() {
    let specification =
        fs::read_to_string(repository_root().join("protocol/droidloom-host-v1.md")).unwrap();
    assert!(specification.contains("SOCK_SEQPACKET"));
    assert!(specification.contains("SCM_RIGHTS"));
    assert!(specification.contains("one ordinary Wayland toplevel"));
    assert!(specification.contains("descriptor-free"));
    assert!(
        !repository_root()
            .join("protocol/droidloom-shell-v1.xml")
            .exists()
    );
}

#[test]
fn packaged_artifact_compatibility_regressions() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let output = std::process::Command::new("python3")
        .args(["-m", "unittest", "discover", "-s", "packaging/tests"])
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .current_dir(root)
        .output()
        .expect("python3 is required for package contract tests");
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
