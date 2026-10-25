//! Source-built ARM64 NativeBridge support for the x86_64 Android cell.
use crate::util::*;
use serde::Deserialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceLock {
    schema_version: u32,
    name: String,
    url: String,
    commit: String,
    license: String,
    path: String,
}

// These are already supplied by the checksum-pinned system image. Link only
// the translator's proxies against them; do not replace source-built platform
// modules or install these build references as a second runtime provider.
const HOST_API_LIBRARIES: &[&str] = &[
    "libandroid",
    "libandroid_runtime",
    "libaaudio",
    "libamidi",
    "libbinder_ndk",
    "libcamera2ndk",
    "libEGL",
    "libGLESv1_CM",
    "libGLESv2",
    "libGLESv3",
    "libjnigraphics",
    "libmediandk",
    "libnativewindow",
    "libnativehelper",
    "libneuralnetworks",
    "libOpenMAXAL",
    "libOpenSLES",
    "libwebviewchromium_plat_support",
    "libvulkan",
];

pub fn prepare_build(
    source: &Path,
    vendor: &Path,
    work: &Path,
    projection: &mut crate::android::Projection,
) -> Result<()> {
    let bridge = source.join("frameworks/libs/binary_translation");
    // Android 17 forbids global libnativehelper include paths. Export its
    // platform API declarations through a local header-only module instead.
    let nativehelper = source.join("libnativehelper/Android.bp");
    projection.put(
        &nativehelper,
        fs::read_to_string(&nativehelper)?
            + r#"
cc_library_headers {
    name: "droidloom_native_bridge_nativehelper_headers",
    export_include_dirs: ["include"],
    header_libs: ["jni_headers"],
    export_header_lib_headers: ["jni_headers"],
}
"#,
    )?;
    // Outside the mirrored android/native-bridge, which rsync --delete would
    // empty on every build. Unchanged bytes keep their timestamps, so Soong
    // need not analyze the tree again when nothing changed.
    let link = vendor.join("native-bridge-link");
    fs::create_dir_all(&link)?;
    let mut blueprint =
        String::from("package { default_applicable_licenses: [\"Android-Apache-2.0\"], }\n");
    for name in HOST_API_LIBRARIES {
        let library = work.join(format!("base-system/system/lib64/{name}.so"));
        let apex_name = match *name {
            "libneuralnetworks" => Some("com.android.neuralnetworks"),
            "libnativehelper" => Some("com.android.art"),
            _ => None,
        };
        if let Some(apex_name) = apex_name {
            let apex = work.join(format!("base-system/system/apex/{apex_name}.apex"));
            let scratch = tempfile::Builder::new()
                .prefix("native-bridge-apex-")
                .tempdir_in(work)?;
            let payload = scratch.path().join("payload.img");
            let extracted = scratch.path().join(format!("{name}.so"));
            run(Command::new("unzip")
                .args(["-p"])
                .arg(&apex)
                .arg("apex_payload.img")
                .stdout(std::process::Stdio::from(fs::File::create(&payload)?)))?;
            // This pinned Mainline APEX uses ext4, unlike the system EROFS.
            // debugfs opens it read-only and streams exactly the requested ELF.
            run(Command::new("debugfs")
                .arg("-R")
                .arg(format!("cat /lib64/{name}.so"))
                .arg(&payload)
                .stdout(std::process::Stdio::from(fs::File::create(&extracted)?)))?;
            write(&link.join(format!("{name}.so")), fs::read(&extracted)?)?;
        } else {
            write(&link.join(format!("{name}.so")), fs::read(&library)?)?;
        }
        let bytes = fs::read(link.join(format!("{name}.so")))?;
        if bytes.len() < 20
            || &bytes[..6] != b"\x7fELF\x02\x01"
            || u16::from_le_bytes([bytes[18], bytes[19]]) != 62
        {
            return fail(format!("invalid pinned x86_64 API library: {name}"));
        }
        blueprint.push_str(&format!(
            r#"
cc_prebuilt_library_shared {{
    name: "droidloom_native_bridge_host_{name}",
    stem: "{name}", srcs: ["{name}.so"],
    compile_multilib: "64", installable: false,
    system_shared_libs: [], shared_libs: [],
    header_libs: ["libnativewindow_headers", "libarect_headers", "libutils_headers", "droidloom_native_bridge_nativehelper_headers"],
    export_header_lib_headers: ["libnativewindow_headers", "libarect_headers", "libutils_headers", "droidloom_native_bridge_nativehelper_headers"],
    // Dependencies are provided by the pinned runtime, outside this build graph.
    check_elf_files: false, strip: {{ none: true }},
}}
"#
        ));
    }
    write(&link.join("Android.bp"), blueprint)?;
    // The engine itself uses libandroid for native activity integration.
    for relative in ["Android.bp", "native_activity/Android.bp"] {
        let root = bridge.join(relative);
        projection.put(
            &root,
            fs::read_to_string(&root)?.replace(
                "\"libandroid\"",
                "\"droidloom_native_bridge_host_libandroid\"",
            ),
        )?;
    }
    for entry in fs::read_dir(bridge.join("android_api"))? {
        let path = entry?.path().join("Android.bp");
        if !path.is_file() {
            continue;
        }
        let mut text = fs::read_to_string(&path)?;
        for name in HOST_API_LIBRARIES {
            text = text.replace(
                &format!("\"{name}\""),
                &format!("\"droidloom_native_bridge_host_{name}\""),
            );
        }
        projection.put(&path, text)?;
    }
    let defaults = bridge.join("android_api/Android.bp");
    let text = fs::read_to_string(&defaults)?.replace(
        "host_supported: false,",
        r#"host_supported: false,
    include_dirs: [
        "external/vulkan-headers/include",
        "frameworks/base/core/jni/include",
        "frameworks/wilhelm/include",
        "frameworks/av/media/ndk/include",
        "frameworks/av/media/libaaudio/include",
        "frameworks/base/media/native/midi/include",
        "frameworks/av/camera/ndk/include",
        "frameworks/native/libs/nativewindow/include",
        "frameworks/native/libs/binder/ndk/include_ndk",
    ],"#,
    );
    projection.put(&defaults, text)?;
    // This pinned Digitalis revision leaves the old atomic fault-check flag
    // unused after switching to fault-recoverable atomic builtins.
    let interpreter = bridge.join("interpreter/arm64/interpreter.h");
    projection.put(
        &interpreter,
        fs::read_to_string(&interpreter)?.replace(
            "    bool need_write = (args.op != Decoder::AtomicOp::kLdxr &&\n                       args.op != Decoder::AtomicOp::kLdar);\n",
            "",
        ),
    )?;
    // Android 17 moved these declarations; Digitalis still keeps them in
    // runtime_primitives. Adapt the header dependency and the three include paths.
    let support = source.join("frameworks/libs/native_bridge_support");
    let libc = support.join("android_api/libc/Android.bp");
    projection.put(
        &libc,
        fs::read_to_string(&libc)?
            .replace("        \"libberberis_runtime_library_headers\",\n", ""),
    )?;
    for relative in [
        "android_api/libEGL/proxy/egl_trampolines.cc",
        "android_api/libvulkan/proxy/vulkan_trampolines.cc",
        "android_api/libvulkan/proxy/vulkan_xml.h",
    ] {
        let path = support.join(relative);
        projection.put(
            &path,
            fs::read_to_string(&path)?.replace(
                "berberis/runtime_library/runtime_library.h",
                "berberis/runtime_primitives/runtime_library.h",
            ),
        )?;
    }
    Ok(())
}

const PROPERTIES: &[(&str, &str)] = &[
    ("ro.dalvik.vm.native.bridge", "libberberis_arm64.so"),
    ("ro.dalvik.vm.isa.arm64", "x86_64"),
    ("ro.enable.native.bridge.exec", "0"),
    ("ro.berberis.flags", "android-mmap-noreserve"),
    ("ro.product.cpu.abilist", "x86_64,arm64-v8a"),
    ("ro.product.cpu.abilist64", "x86_64,arm64-v8a"),
    ("ro.product.cpu.abilist32", ""),
    ("ro.system.product.cpu.abilist", "x86_64,arm64-v8a"),
    ("ro.system.product.cpu.abilist64", "x86_64,arm64-v8a"),
    ("ro.system.product.cpu.abilist32", ""),
];

fn bridge_properties(original: &str) -> Result<String> {
    if !original
        .lines()
        .any(|line| line == "ro.build.version.sdk=37")
    {
        return fail("ARM64 translation requires the pinned Android 17 system image");
    }
    let mut result = String::new();
    for line in original.lines() {
        if !PROPERTIES.iter().any(|(key, _)| {
            line.split_once('=')
                .is_some_and(|(existing, _)| existing.trim() == *key)
        }) {
            result.push_str(line);
            result.push('\n');
        }
    }
    result.push_str("\n# Droidloom source-built ARM64 NativeBridge\n");
    for (key, value) in PROPERTIES {
        result.push_str(&format!("{key}={value}\n"));
    }
    Ok(result)
}

pub fn stage_image(repo: &Path, image: &Path, destination: &Path, product: &Path) -> Result<()> {
    fs::create_dir_all(destination.parent().ok_or("image has no parent")?)?;
    let worker = tempfile::Builder::new()
        .prefix("native-bridge-worker-")
        .tempdir_in(destination.parent().unwrap())?;
    let executable = worker.path().join("droidloom-update");
    fs::copy("/proc/self/exe", &executable)?;
    mode(&executable, 0o755)?;
    run(Command::new("fakeroot")
        .arg("--")
        .arg(executable)
        .arg("native-bridge-image")
        .arg("--image")
        .arg(image)
        .arg("--destination")
        .arg(destination)
        .arg("--product")
        .arg(product)
        .arg("--source-lock")
        .arg(repo.join("android/manifest/native-bridge-lock.json")))
}

fn guest_files(root: &Path, paths: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            guest_files(&entry.path(), paths)?;
        } else {
            paths.push(entry.path());
        }
    }
    Ok(())
}

/// Rebuild the system EROFS in one fakeroot session, retaining its metadata.
pub fn derive_image(
    image: &Path,
    destination: &Path,
    product: &Path,
    source_lock: &Path,
) -> Result<()> {
    if std::env::var_os("FAKEROOTKEY").is_none() {
        return fail("NativeBridge image assembly requires fakeroot metadata preservation");
    }
    if destination.exists() {
        return fail("NativeBridge image destination already exists");
    }
    let work = tempfile::Builder::new()
        .prefix("native-bridge-image-")
        .tempdir_in(destination.parent().ok_or("image has no parent")?)?;
    let tree = work.path().join("tree");
    crate::image_policy::extract(image, &tree)?;
    let properties = tree.join("system/build.prop");
    let updated = bridge_properties(&fs::read_to_string(&properties)?)?;
    fs::write(&properties, updated)?;

    let mut paths = Vec::new();
    for entry in fs::read_dir(product.join("system/lib64"))? {
        let entry = entry?;
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with("libberberis_")
            && entry.file_name().to_string_lossy().ends_with(".so")
        {
            paths.push(entry.path());
        }
    }
    guest_files(&product.join("system/lib64/arm64"), &mut paths)?;
    guest_files(&product.join("system/bin/arm64"), &mut paths)?;
    paths.push(product.join("system/etc/ld.config.arm64.txt"));
    paths.sort();
    for required in [
        "system/lib64/libberberis_arm64.so",
        "system/lib64/libberberis_exec_region.so",
        "system/lib64/libberberis_proxy_libc.so",
        "system/lib64/libberberis_proxy_libEGL.so",
        "system/lib64/libberberis_proxy_libGLESv2.so",
        "system/lib64/libberberis_proxy_libvulkan.so",
        "system/lib64/arm64/libc.so",
        "system/lib64/arm64/libnative_bridge_vdso.so",
        "system/bin/arm64/linker64",
    ] {
        if !paths.contains(&product.join(required)) {
            return fail(format!("missing required NativeBridge output: {required}"));
        }
    }
    let mut inventory = BTreeMap::new();
    let mut directories = BTreeSet::new();
    for path in paths {
        let relative = path.strip_prefix(product)?;
        let destination = tree.join(relative);
        let metadata = fs::symlink_metadata(&path)?;
        fs::create_dir_all(destination.parent().unwrap())?;
        for parent in relative.ancestors().skip(1) {
            if parent.starts_with("system/lib64/arm64") || parent.starts_with("system/bin/arm64") {
                directories.insert(tree.join(parent));
            }
        }
        if metadata.file_type().is_symlink() {
            let target = fs::read_link(&path)?;
            // Preserve relative links only when their target stays in the product.
            if target.is_absolute() || !path.canonicalize()?.starts_with(product.canonicalize()?) {
                return fail(format!(
                    "unsupported guest runtime symlink: {}",
                    path.display()
                ));
            }
            inventory.insert(
                relative.to_string_lossy().into_owned(),
                format!("link:{}", target.display()),
            );
        } else if metadata.is_file() {
            let bytes = fs::read(&path)?;
            if bytes.starts_with(b"\x7fELF") {
                let guest = relative.starts_with("system/lib64/arm64")
                    || relative.starts_with("system/bin/arm64");
                let machine = if guest { 183u16 } else { 62u16 };
                if bytes.len() < 20
                    || bytes[4] != 2
                    || bytes[5] != 1
                    || u16::from_le_bytes([bytes[18], bytes[19]]) != machine
                {
                    return fail(format!(
                        "wrong NativeBridge ELF architecture: {}",
                        path.display()
                    ));
                }
            } else if relative != Path::new("system/etc/ld.config.arm64.txt") {
                return fail(format!(
                    "non-ELF NativeBridge runtime output: {}",
                    path.display()
                ));
            }
            inventory.insert(relative.to_string_lossy().into_owned(), hash(&path)?);
        } else {
            return fail("unsupported NativeBridge runtime object");
        }
        run(Command::new("cp")
            .args(["-a", "--remove-destination", "--"])
            .arg(&path)
            .arg(&destination))?;
        run(Command::new("chown").args(["-h", "0:0"]).arg(&destination))?;
        let label = if relative.starts_with("system/lib64") {
            "u:object_r:system_lib_file:s0"
        } else {
            "u:object_r:system_file:s0"
        };
        run(Command::new("setfattr")
            .args(["-h", "-n", "security.selinux", "-v", label])
            .arg(&destination))?;
    }
    for directory in directories {
        run(Command::new("chown").arg("0:0").arg(&directory))?;
        mode(&directory, 0o755)?;
        run(Command::new("setfattr")
            .args([
                "-n",
                "security.selinux",
                "-v",
                if directory.starts_with(tree.join("system/lib64")) {
                    "u:object_r:system_lib_file:s0"
                } else {
                    "u:object_r:system_file:s0"
                },
            ])
            .arg(&directory))?;
    }
    let manifest = serde_json::json!({
        "schema_version": 1,
        "source": serde_json::from_slice::<serde_json::Value>(&fs::read(source_lock)?)?,
        "build_source": serde_json::from_slice::<serde_json::Value>(&fs::read(
            product.join("droidloom-native-bridge-source.json"))?)?,
        "guest_abi": "arm64-v8a", "host_abi": "x86_64",
        "native_executables": false, "files": inventory,
    });
    let manifest_path = tree.join("system/etc/droidloom-native-bridge.json");
    fs::write(&manifest_path, serde_json::to_vec_pretty(&manifest)?)?;
    run(Command::new("chown").arg("0:0").arg(&manifest_path))?;
    run(Command::new("setfattr")
        .args(["-n", "security.selinux", "-v", "u:object_r:system_file:s0"])
        .arg(&manifest_path))?;
    let expected = crate::image_policy::inventory(&tree)?;
    let rebuilt = work.path().join("system.img");
    run(Command::new("mkfs.erofs")
        // makepkg exports SOURCE_DATE_EPOCH, which otherwise silently clamps
        // newer mtimes despite --preserve-mtime. Preserve the staged metadata
        // exactly; the filesystem creation timestamp is explicitly pinned below.
        .env_remove("SOURCE_DATE_EPOCH")
        .args([
            "-zlz4hc,level=9",
            "--workers=2",
            "-T1230768000",
            "--mkfs-time",
            "--preserve-mtime",
            "-U166f4792-a27c-43a9-a1b7-e5d5c6e8ac5b",
        ])
        .arg(&rebuilt)
        .arg(&tree))?;
    let checked = work.path().join("checked");
    crate::image_policy::extract(&rebuilt, &checked)?;
    let actual = crate::image_policy::inventory(&checked)?;
    if actual != expected {
        for path in expected
            .keys()
            .chain(actual.keys())
            .collect::<BTreeSet<_>>()
        {
            if expected.get(path) != actual.get(path) {
                eprintln!(
                    "NativeBridge image mismatch at {}: expected {:?}, got {:?}",
                    path.display(),
                    expected.get(path),
                    actual.get(path)
                );
            }
        }
        return fail("NativeBridge image verification found changed contents or metadata");
    }
    fs::rename(&rebuilt, destination)?;
    eprintln!(
        "Verified ARM64 NativeBridge system image: {}",
        destination.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOCK: &str = include_str!("../../../android/manifest/native-bridge-lock.json");

    fn git(dir: &Path, args: &[&str]) -> String {
        output(
            Command::new("git")
                .arg("-C")
                .arg(dir)
                .args([
                    "-c",
                    "user.name=Droidloom Test",
                    "-c",
                    "user.email=test@example.invalid",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args),
        )
        .unwrap()
    }

    fn source_fixture() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        let source = root.path().join("source");
        let checkout = source.join("frameworks/libs/binary_translation");
        fs::create_dir_all(repo.join("android/manifest")).unwrap();
        fs::create_dir_all(&checkout).unwrap();
        git(&checkout, &["init", "--quiet"]);
        let mut lock: serde_json::Value = serde_json::from_str(LOCK).unwrap();
        git(
            &checkout,
            &["remote", "add", "origin", lock["url"].as_str().unwrap()],
        );
        fs::write(checkout.join("README.md"), "fixture\n").unwrap();
        git(&checkout, &["add", "README.md"]);
        git(&checkout, &["commit", "--quiet", "-m", "fixture"]);
        lock["commit"] = git(&checkout, &["rev-parse", "HEAD"]).into();
        fs::write(
            repo.join("android/manifest/native-bridge-lock.json"),
            serde_json::to_vec(&lock).unwrap(),
        )
        .unwrap();
        (root, repo, source, checkout)
    }

    #[test]
    fn source_lock_accepts_only_teto_and_valid_contract() {
        let original: serde_json::Value = serde_json::from_str(LOCK).unwrap();
        validate_source_lock(&serde_json::from_value(original.clone()).unwrap()).unwrap();
        for (key, value) in [
            (
                "url",
                serde_json::json!(
                    "https://github.com/DigitalisX64/platform_frameworks_libs_binary_translation.git"
                ),
            ),
            (
                "url",
                serde_json::json!(
                    "https://github.com/denialwm/platform_frameworks_libs_binary_translation.git"
                ),
            ),
            (
                "url",
                serde_json::json!("https://example.invalid/translator.git"),
            ),
            ("commit", serde_json::json!("main")),
            ("commit", serde_json::json!("Z".repeat(40))),
            ("path", serde_json::json!("../outside")),
            ("license", serde_json::json!("unknown")),
            ("schema_version", serde_json::json!(2)),
        ] {
            let mut lock = original.clone();
            lock[key] = value;
            assert!(
                validate_source_lock(&serde_json::from_value(lock).unwrap()).is_err(),
                "{key}"
            );
        }
    }

    #[test]
    fn locked_source_rejects_dirty_wrong_origin_and_wrong_revision() {
        let (_root, repo, source, checkout) = source_fixture();
        let result = prepare_source_with_local(&repo, &source, None).unwrap();
        assert_eq!(result["working_tree"], false);
        fs::write(checkout.join("README.md"), "modified\n").unwrap();
        assert!(prepare_source_with_local(&repo, &source, None).is_err());
        fs::write(checkout.join("README.md"), "fixture\n").unwrap();
        fs::write(checkout.join("untracked"), "local\n").unwrap();
        assert!(prepare_source_with_local(&repo, &source, None).is_err());
        fs::remove_file(checkout.join("untracked")).unwrap();
        let origin = git(&checkout, &["remote", "get-url", "origin"]);
        git(
            &checkout,
            &[
                "remote",
                "set-url",
                "origin",
                "https://example.invalid/other.git",
            ],
        );
        assert!(prepare_source_with_local(&repo, &source, None).is_err());
        git(&checkout, &["remote", "set-url", "origin", &origin]);
        git(
            &checkout,
            &[
                "commit",
                "--allow-empty",
                "--quiet",
                "-m",
                "different revision",
            ],
        );
        assert!(prepare_source_with_local(&repo, &source, None).is_err());
    }

    #[test]
    fn development_source_requires_fork_ancestry_and_records_working_files() {
        let (root, repo, source, checkout) = source_fixture();
        assert!(prepare_source_with_local(&repo, &source, Some(checkout.clone())).is_err());
        let local = root.path().join("development");
        fs::rename(checkout, &local).unwrap();
        git(
            &local,
            &["commit", "--allow-empty", "--quiet", "-m", "development"],
        );
        fs::write(local.join("README.md"), "uncommitted development\n").unwrap();
        let result = prepare_source_with_local(&repo, &source, Some(local.clone())).unwrap();
        assert_eq!(result["working_tree"], true);
        assert_eq!(result["commit"], git(&local, &["rev-parse", "HEAD"]));
        assert_ne!(result["commit"], result["base_commit"]);
        assert_eq!(
            result["files"]["README.md"],
            hash(&local.join("README.md")).unwrap()
        );
        let lock_path = repo.join("android/manifest/native-bridge-lock.json");
        let original = fs::read(&lock_path).unwrap();
        let mut lock: serde_json::Value = serde_json::from_slice(&original).unwrap();
        lock["commit"] = "0".repeat(40).into();
        fs::write(&lock_path, serde_json::to_vec(&lock).unwrap()).unwrap();
        assert!(prepare_source_with_local(&repo, &source, Some(local.clone())).is_err());
        fs::write(&lock_path, original).unwrap();
        git(
            &local,
            &[
                "remote",
                "set-url",
                "origin",
                "https://example.invalid/other.git",
            ],
        );
        assert!(prepare_source_with_local(&repo, &source, Some(local)).is_err());
    }

    #[test]
    #[ignore = "requires network access to fetch the published source pin"]
    fn published_source_pin_fetches_and_verifies() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("android/manifest")).unwrap();
        fs::write(
            root.path().join("android/manifest/native-bridge-lock.json"),
            LOCK,
        )
        .unwrap();
        let source = root.path().join("source");
        let result = prepare_source_with_local(root.path(), &source, None).unwrap();
        let lock: SourceLock = serde_json::from_str(LOCK).unwrap();
        assert_eq!(result["commit"], lock.commit);
        assert_eq!(result["working_tree"], false);
        assert_eq!(
            prepare_source_with_local(root.path(), &source, None).unwrap(),
            result
        );
    }

    #[test]
    fn bridge_properties_replace_conflicts_and_preserve_system_identity() {
        let original = "ro.build.version.sdk=37\nro.product.cpu.abi=x86_64\nro.dalvik.vm.native.bridge=0\nro.product.cpu.abilist=x86_64\n";
        let changed = bridge_properties(original).unwrap();
        assert!(changed.contains("ro.product.cpu.abi=x86_64\n"));
        assert!(!changed.contains("ro.dalvik.vm.native.bridge=0\n"));
        for (key, value) in PROPERTIES {
            assert_eq!(
                changed
                    .lines()
                    .filter(|line| line.starts_with(&format!("{key}=")))
                    .collect::<Vec<_>>(),
                vec![format!("{key}={value}")]
            );
        }
        assert!(bridge_properties("ro.build.version.sdk=36\n").is_err());
    }
}

fn validate_source_lock(lock: &SourceLock) -> Result<()> {
    if lock.schema_version != 1
        || lock.url != "https://github.com/denialwm/teto.git"
        || lock.path != "frameworks/libs/binary_translation"
        || lock.license != "Apache-2.0"
        || lock.commit.len() != 40
        || !lock
            .commit
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return fail("invalid Teto source lock");
    }
    Ok(())
}

pub fn prepare_source(repo: &Path, source: &Path) -> Result<serde_json::Value> {
    prepare_source_with_local(
        repo,
        source,
        std::env::var_os("DROIDLOOM_NATIVE_BRIDGE_SOURCE").map(PathBuf::from),
    )
}

fn prepare_source_with_local(
    repo: &Path,
    source: &Path,
    local: Option<PathBuf>,
) -> Result<serde_json::Value> {
    let lock: SourceLock = serde_json::from_slice(&fs::read(
        repo.join("android/manifest/native-bridge-lock.json"),
    )?)?;
    validate_source_lock(&lock)?;
    let checkout = source.join(&lock.path);
    if let Some(local) = local {
        let local = local.canonicalize()?;
        if local.starts_with(source.canonicalize()?) {
            return fail(
                "Teto development checkout must be outside the generated AOSP source tree",
            );
        }
        if output(
            Command::new("git")
                .arg("-C")
                .arg(&local)
                .args(["remote", "get-url", "origin"]),
        )? != lock.url
        {
            return fail("local Teto checkout must use the locked origin");
        }
        // A development branch can commit fixes on top of the published Teto pin.
        // Keep the base check while recording the actual revision being built.
        run(Command::new("git").arg("-C").arg(&local).args([
            "merge-base",
            "--is-ancestor",
            &lock.commit,
            "HEAD",
        ]))?;
        let head = output(
            Command::new("git")
                .arg("-C")
                .arg(&local)
                .args(["rev-parse", "HEAD"]),
        )?;
        let files = source_inventory(&local)?;
        fs::create_dir_all(&checkout)?;
        // Snapshot the actual working files. Never apply functional patches to
        // Digitalis or mutate the developer's checkout during a build.
        run(Command::new("rsync")
            .args(["-a", "--delete", "--exclude", ".git"])
            .arg(format!("{}/", local.display()))
            .arg(&checkout))?;
        if source_inventory(&checkout)? != files || source_inventory(&local)? != files {
            return fail("Digitalis working files changed while snapshotting; retry the build");
        }
        eprintln!("Building Teto working files based on {}", lock.commit);
        return Ok(
            serde_json::json!({"commit": head, "base_commit": lock.commit,
                "working_tree": true, "files": files}),
        );
    }
    if !checkout.exists() {
        let parent = checkout.parent().ok_or("source checkout has no parent")?;
        fs::create_dir_all(parent)?;
        let temporary = tempfile::Builder::new()
            .prefix(".teto-")
            .tempdir_in(parent)?;
        run(Command::new("git")
            .arg("init")
            .arg("--quiet")
            .arg(temporary.path()))?;
        run(Command::new("git")
            .arg("-C")
            .arg(temporary.path())
            .args(["remote", "add", "origin", &lock.url]))?;
        run(Command::new("git").arg("-C").arg(temporary.path()).args([
            "fetch",
            "--depth",
            "1",
            "origin",
            &lock.commit,
        ]))?;
        run(Command::new("git").arg("-C").arg(temporary.path()).args([
            "checkout",
            "--detach",
            &lock.commit,
        ]))?;
        fs::rename(temporary.path(), &checkout)?;
    }
    for (args, expected) in [
        (vec!["rev-parse", "HEAD"], lock.commit.as_str()),
        (vec!["remote", "get-url", "origin"], lock.url.as_str()),
        (vec!["status", "--porcelain", "--untracked-files=all"], ""),
    ] {
        if output(Command::new("git").arg("-C").arg(&checkout).args(args))? != expected {
            return fail("Teto checkout differs from its source lock or has local changes");
        }
    }
    eprintln!("Verified {} at {}", lock.name, lock.commit);
    Ok(serde_json::json!({"commit": lock.commit, "working_tree": false}))
}

fn source_inventory(root: &Path) -> Result<BTreeMap<String, String>> {
    fn visit(root: &Path, dir: &Path, result: &mut BTreeMap<String, String>) -> Result<()> {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            if entry.file_name() == ".git" {
                continue;
            }
            let path = entry.path();
            let kind = entry.file_type()?;
            let relative = path.strip_prefix(root)?.to_string_lossy().into_owned();
            if kind.is_dir() {
                visit(root, &path, result)?;
            } else if kind.is_symlink() {
                if !path.canonicalize()?.starts_with(root) {
                    return fail("Teto source symlink escapes its checkout");
                }
                result.insert(relative, format!("link:{}", fs::read_link(path)?.display()));
            } else if kind.is_file() {
                result.insert(relative, hash(&path)?);
            } else {
                return fail("unsupported object in Teto checkout");
            }
        }
        Ok(())
    }
    let mut result = BTreeMap::new();
    visit(root, root, &mut result)?;
    Ok(result)
}
