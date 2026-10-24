use crate::util::*;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fs, os::unix::fs::PermissionsExt, path::Path, process::Command};
pub const MANIFEST: &str = "droidloom-update.json";
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub mode: u32,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub schema: u32,
    pub build_id: String,
    pub architecture: String,
    pub input_abi: u16,
    pub source_identity: String,
    pub files: BTreeMap<String, Artifact>,
}
pub fn input_abi(data: &[u8]) -> Result<u16> {
    let prefix = b"DROIDLOOM_INPUT_ABI=";
    let mut versions = std::collections::BTreeSet::new();
    for i in 0..data.len().saturating_sub(prefix.len()) {
        if data[i..].starts_with(prefix) {
            let tail = &data[i + prefix.len()..];
            let n = tail.iter().take_while(|b| b.is_ascii_digit()).count();
            if n > 0 && n <= 5 && tail.get(n) == Some(&b';') {
                versions.insert(std::str::from_utf8(&tail[..n])?.parse::<u16>()?);
            }
        }
    }
    if versions.len() != 1 {
        return fail(
            "missing or ambiguous compiled input ABI metadata; rebuild all Android components",
        );
    }
    Ok(*versions.first().unwrap())
}
pub fn dex(path: &Path) -> Result<Vec<u8>> {
    let entries = output(Command::new("unzip").args(["-Z1"]).arg(path))?;
    let mut result = Vec::new();
    for name in entries
        .lines()
        .filter(|n| n.starts_with("classes") && n.ends_with(".dex"))
    {
        let out = Command::new("unzip")
            .arg("-p")
            .arg(path)
            .arg(name)
            .output()?;
        if !out.status.success() {
            return fail("cannot extract compiled Android DEX");
        }
        validate_dex(&out.stdout).map_err(|error| format!("{}!{name}: {error}", path.display()))?;
        result.extend(out.stdout);
    }
    if result.is_empty() {
        return fail(format!(
            "{} is not a dexed Android runtime JAR",
            path.display()
        ));
    }
    Ok(result)
}
fn dex_checksum(data: &[u8]) -> u32 {
    // Adler-32, as used by ART over the bytes following checksum_ (offset 12).
    // 5552 bytes bound both accumulators below u32::MAX before reduction.
    let (mut a, mut b) = (1_u32, 0_u32);
    for chunk in data.chunks(5552) {
        for &byte in chunk {
            a += u32::from(byte);
            b += a;
        }
        a %= 65521;
        b %= 65521;
    }
    (b << 16) | a
}
fn validate_dex(data: &[u8]) -> Result<()> {
    if data.len() < 112 {
        return fail("truncated DEX header");
    }
    if &data[..4] != b"dex\n"
        || !matches!(
            &data[4..8],
            b"035\0" | b"037\0" | b"038\0" | b"039\0" | b"040\0"
        )
    {
        return fail("unsupported DEX magic/version; expected standard DEX 035–040");
    }
    let word = |offset| u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap());
    if word(32) as usize != data.len() || word(36) != 112 || word(40) != 0x12345678 {
        return fail("invalid DEX file size, header size or endian tag");
    }
    let actual = dex_checksum(&data[12..]);
    let expected = word(8);
    if actual != expected {
        return fail(format!(
            "bad DEX checksum ({actual:08x}, expected {expected:08x}); rebuild the Android artifact"
        ));
    }
    Ok(())
}
fn architecture(data: &[u8]) -> Result<&'static str> {
    if data.len() < 20 || &data[..4] != b"\x7fELF" || data[4] != 2 || data[5] != 1 {
        return fail("expected little-endian ELF artifact");
    }
    match u16::from_le_bytes([data[18], data[19]]) {
        62 => Ok("x86_64"),
        183 => Ok("aarch64"),
        _ => fail("unsupported ELF architecture"),
    }
}
fn compatibility(root: &Path) -> Result<(String, u16)> {
    let runtime = root.join("usr/lib/droidloom/runtime");
    let home = dex(&runtime.join("ime/DroidloomHome.apk"))?;
    if !home
        .windows(b"Lcom/android/droidloom/home/HomeActivity;".len())
        .any(|w| w == b"Lcom/android/droidloom/home/HomeActivity;")
    {
        return fail("DroidloomHome.apk lacks compiled HOME activity");
    }
    if fs::metadata(runtime.join("ime/home-setup"))?.len() == 0 {
        return fail("missing HOME provisioning script");
    }
    let systemui = dex(&runtime.join("systemui/SystemUI.apk"))?;
    if !systemui
        .windows(b"DROIDLOOM_SYSTEMUI_ABI=1;".len())
        .any(|w| w == b"DROIDLOOM_SYSTEMUI_ABI=1;")
    {
        return fail("SystemUI.apk is not the Droidloom minimal implementation");
    }
    if !systemui
        .windows(b"DROIDLOOM_CLIPBOARD_ABI=1;".len())
        .any(|w| w == b"DROIDLOOM_CLIPBOARD_ABI=1;")
    {
        return fail("SystemUI.apk lacks the clipboard adapter");
    }
    if !systemui
        .windows(b"DROIDLOOM_NOTIFICATIONS_ABI=3;".len())
        .any(|w| w == b"DROIDLOOM_NOTIFICATIONS_ABI=3;")
    {
        return fail("SystemUI.apk lacks the notification adapter");
    }
    let ime = dex(&runtime.join("ime/DroidloomIME.apk"))?;
    let composer =
        fs::read(runtime.join("bin/android.hardware.graphics.composer3-service.droidloom"))?;
    let arch = architecture(&composer)?.to_string();
    let version = input_abi(&composer)?;
    let direct_input_abi = b"DROIDLOOM_DIRECT_INPUT_ABI=1;";
    let presenter = fs::read(root.join("usr/bin/droidloom-wayland"))?;
    if !composer
        .windows(direct_input_abi.len())
        .any(|w| w == direct_input_abi)
        || !presenter
            .windows(direct_input_abi.len())
            .any(|w| w == direct_input_abi)
    {
        return fail("Composer and presenter lack paired direct input ABI 1");
    }
    let services = dex(&runtime.join("framework/services.jar"))?;
    let catalog_class = b"Lcom/android/server/wm/DroidloomApplicationCatalog;";
    if !services
        .windows(catalog_class.len())
        .any(|w| w == catalog_class)
    {
        return fail("in-process application catalog class is missing");
    }
    let policy_abi = b"DROIDLOOM_WINDOW_POLICY_ABI=1;";
    let policy_native = fs::read(runtime.join("lib64/libinputflinger.so"))?;
    let vulkan_abi = b"DROIDLOOM_VULKAN_CAPABILITIES_ABI=2;";
    if !services.windows(vulkan_abi.len()).any(|w| w == vulkan_abi)
        || !policy_native
            .windows(vulkan_abi.len())
            .any(|w| w == vulkan_abi)
    {
        return fail(
            "services.jar and libinputflinger.so lack paired Vulkan capability probe ABI 1",
        );
    }
    let framework_abi = b"DROIDLOOM_FRAMEWORK_CONTROL_ABI=1;";
    for (name, data) in [
        ("services.jar", services.clone()),
        ("libinputflinger.so", policy_native.clone()),
        ("droidloomd", fs::read(root.join("usr/bin/droidloomd"))?),
        (
            "droidloom-applications",
            fs::read(root.join("usr/bin/droidloom-applications"))?,
        ),
    ] {
        if !data
            .windows(framework_abi.len())
            .any(|w| w == framework_abi)
        {
            return fail(format!("{name} lacks direct framework control ABI 1"));
        }
    }
    let notifications_abi = b"DROIDLOOM_NOTIFICATIONS_ABI=3;";
    if !presenter
        .windows(notifications_abi.len())
        .any(|w| w == notifications_abi)
    {
        return fail("presenter lacks notification delta ABI 2");
    }
    if !services.windows(policy_abi.len()).any(|w| w == policy_abi)
        || !policy_native
            .windows(policy_abi.len())
            .any(|w| w == policy_abi)
    {
        return fail("services.jar and libinputflinger.so lack paired Rust window policy ABI 1");
    }
    if architecture(&policy_native)? != arch {
        return fail("wrong Rust window policy JNI architecture");
    }
    let task_abi = b"DROIDLOOM_TASK_BACKEND_ABI=2;";
    let channels_abi = b"DROIDLOOM_HOST_CHANNELS_ABI=1;";
    for (name, data) in [
        ("services.jar", services.as_slice()),
        ("SystemUI.apk", systemui.as_slice()),
        ("DroidloomIME.apk", ime.as_slice()),
        ("presenter", presenter.as_slice()),
    ] {
        if !data.windows(channels_abi.len()).any(|w| w == channels_abi) {
            return fail(format!("{name} lacks direct host channels ABI 1"));
        }
    }
    let native_input_abi = b"DROIDLOOM_NATIVE_INPUT_ABI=2;";
    for (name, data) in [
        ("services.jar", services.as_slice()),
        ("libinputflinger.so", policy_native.as_slice()),
        ("Composer", composer.as_slice()),
    ] {
        if !data
            .windows(native_input_abi.len())
            .any(|w| w == native_input_abi)
        {
            return fail(format!("{name} lacks native input ABI 2"));
        }
    }
    if input_abi(&policy_native)? != version {
        return fail("Composer and InputFlinger input protocols do not match");
    }
    for (name, data) in [
        ("services.jar", services.as_slice()),
        ("libinputflinger.so", policy_native.as_slice()),
        ("Composer", composer.as_slice()),
    ] {
        if !data.windows(task_abi.len()).any(|w| w == task_abi) {
            return fail(format!("{name} lacks framework task backend ABI 2"));
        }
    }
    if !services
        .windows(b"ro.vendor.droidloom.surfaceflinger_tasks".len())
        .any(|w| w == b"ro.vendor.droidloom.surfaceflinger_tasks")
    {
        return fail("services.jar lacks compiled Droidloom navigation policy");
    }
    for path in files(root)? {
        if path.file_name().is_some_and(|name| {
            matches!(
                name.to_str(),
                Some(
                    "droidloom-input-bridge"
                        | "droidloom-input-bridge.jar"
                        | "droidloom-input-bridge.rc"
                        | "droidloom-task-launcher"
                        | "libdroidloom_task_launcher.dylib.so"
                        | "droidloom-application-catalog.jar"
                )
            )
        }) {
            return fail("retired support artifact remains in payload");
        }
        let mut f = fs::File::open(&path)?;
        use std::io::Read;
        let mut header = [0; 20];
        if f.read(&mut header)? == 20
            && &header[..4] == b"\x7fELF"
            && architecture(&header)? != arch
        {
            return fail(format!("wrong ELF architecture: {}", path.display()));
        }
    }
    Ok((arch, version))
}
fn inventory(root: &Path) -> Result<BTreeMap<String, Artifact>> {
    let mut map = BTreeMap::new();
    for path in files(root)? {
        let relative = path
            .strip_prefix(root)?
            .to_str()
            .ok_or("non-UTF8 bundle path")?;
        if relative == MANIFEST {
            continue;
        }
        map.insert(
            relative.to_owned(),
            Artifact {
                mode: fs::metadata(path)?.permissions().mode() & 0o777,
            },
        );
    }
    Ok(map)
}
pub fn seal(root: &Path, source_identity: String) -> Result<String> {
    let (architecture, input_abi) = compatibility(root)?;
    let manifest = Manifest {
        schema: 2,
        build_id: format!(
            "{}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos(),
            std::process::id()
        ),
        architecture,
        input_abi,
        source_identity,
        files: inventory(root)?,
    };
    write(&root.join(MANIFEST), serde_json::to_vec_pretty(&manifest)?)?;
    Ok(manifest.build_id)
}
pub fn verify(root: &Path) -> Result<Manifest> {
    let m: Manifest = serde_json::from_slice(&fs::read(root.join(MANIFEST))?)?;
    if m.schema != 2
        || m.build_id.is_empty()
        || !m.build_id.bytes().all(|b| b.is_ascii_digit() || b == b'-')
    {
        return fail("invalid bundle build record");
    }
    for (relative, artifact) in &m.files {
        let path = Path::new(relative);
        if path
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
        {
            return fail("invalid component path");
        }
        let metadata = fs::metadata(root.join(path))?;
        if !metadata.is_file() || metadata.len() == 0 {
            return fail(format!("missing or empty component: {relative}"));
        }
        if artifact.mode & 0o111 != 0 && metadata.permissions().mode() & 0o111 == 0 {
            return fail(format!("component is not executable: {relative}"));
        }
    }
    let (arch, abi) = compatibility(root)?;
    if arch != m.architecture || abi != m.input_abi {
        return fail("manifest disagrees with compiled artifacts");
    }
    Ok(m)
}
pub fn source_identity(repo: &Path) -> Result<String> {
    let revision =
        output(
            Command::new("git")
                .current_dir(repo)
                .args(["rev-parse", "--short", "HEAD"]),
        )?;
    let dirty = !output(
        Command::new("git")
            .current_dir(repo)
            .args(["status", "--porcelain"]),
    )?
    .is_empty();
    Ok(format!("{revision}{}", if dirty { "+local" } else { "" }))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mismatched_metadata_is_not_guessed() {
        assert!(input_abi(b"old binary").is_err());
        assert!(input_abi(b"DROIDLOOM_INPUT_ABI=4;DROIDLOOM_INPUT_ABI=5;").is_err());
        assert_eq!(input_abi(b"DROIDLOOM_INPUT_ABI=00005;").unwrap(), 5);
    }
    #[test]
    fn inventory_detects_modes_and_files() {
        let d = tempfile::tempdir().unwrap();
        write(&d.path().join("a"), b"one").unwrap();
        let a = inventory(d.path()).unwrap();
        mode(&d.path().join("a"), 0o700).unwrap();
        assert_ne!(a, inventory(d.path()).unwrap());
        write(&d.path().join("extra"), b"x").unwrap();
        assert_eq!(inventory(d.path()).unwrap().len(), 2);
    }
}

#[cfg(test)]
mod artifact_tests {
    use super::*;
    fn dex_fixture(payload: &[u8]) -> Vec<u8> {
        let mut data = vec![0; 112];
        data[..8].copy_from_slice(b"dex\n039\0");
        data[36..40].copy_from_slice(&112_u32.to_le_bytes());
        data[40..44].copy_from_slice(&0x12345678_u32.to_le_bytes());
        data.extend_from_slice(payload);
        refresh_dex_fixture(&mut data);
        data
    }
    fn refresh_dex_fixture(data: &mut [u8]) {
        let size = u32::try_from(data.len()).unwrap();
        data[32..36].copy_from_slice(&size.to_le_bytes());
        let checksum = dex_checksum(&data[12..]);
        data[8..12].copy_from_slice(&checksum.to_le_bytes());
    }
    fn fixture(root: &Path, composer: u16, java: u16) {
        let mut elf = vec![0; 64];
        elf[..4].copy_from_slice(b"\x7fELF");
        elf[4] = 2;
        elf[5] = 1;
        elf[18] = 62;
        elf.extend(format!("DROIDLOOM_FRAMEWORK_CONTROL_ABI=1;DROIDLOOM_NOTIFICATIONS_ABI=3;DROIDLOOM_INPUT_ABI={composer};DROIDLOOM_TASK_BACKEND_ABI=2;DROIDLOOM_DIRECT_INPUT_ABI=1;DROIDLOOM_NATIVE_INPUT_ABI=2;DROIDLOOM_HOST_CHANNELS_ABI=1;").bytes());
        write(&root.join("usr/bin/droidloom-wayland"), &elf).unwrap();
        write(&root.join("usr/lib/droidloom/runtime/bin/android.hardware.graphics.composer3-service.droidloom"), &elf).unwrap();
        elf.extend(format!("DROIDLOOM_WINDOW_POLICY_ABI=1;DROIDLOOM_VULKAN_CAPABILITIES_ABI=2;DROIDLOOM_TASK_BACKEND_ABI=2;DROIDLOOM_NATIVE_INPUT_ABI=2;DROIDLOOM_INPUT_ABI={java};").bytes());
        write(
            &root.join("usr/lib/droidloom/runtime/lib64/libinputflinger.so"),
            elf,
        )
        .unwrap();
        for name in ["droidloomd", "droidloom-applications"] {
            write(
                &root.join("usr/bin").join(name),
                b"DROIDLOOM_FRAMEWORK_CONTROL_ABI=1;",
            )
            .unwrap();
        }
        let d = tempfile::tempdir().unwrap();
        let payload = format!(
            "DROIDLOOM_FRAMEWORK_CONTROL_ABI=1;DROIDLOOM_INPUT_ABI={java};DROIDLOOM_SYSTEMUI_ABI=1;DROIDLOOM_CLIPBOARD_ABI=1;DROIDLOOM_HOST_CHANNELS_ABI=1;DROIDLOOM_NOTIFICATIONS_ABI=3;DROIDLOOM_WINDOW_POLICY_ABI=1;DROIDLOOM_VULKAN_CAPABILITIES_ABI=2;DROIDLOOM_TASK_BACKEND_ABI=2;DROIDLOOM_NATIVE_INPUT_ABI=2;Lcom/android/server/wm/DroidloomApplicationCatalog;Lcom/android/droidloom/home/HomeActivity;ro.vendor.droidloom.surfaceflinger_tasks"
        );
        write(
            &d.path().join("classes.dex"),
            dex_fixture(payload.as_bytes()),
        )
        .unwrap();
        let jar = root.join("usr/lib/droidloom/runtime/framework/services.jar");
        fs::create_dir_all(jar.parent().unwrap()).unwrap();
        run(Command::new("zip")
            .current_dir(d.path())
            .arg("-q")
            .arg(&jar)
            .arg("classes.dex"))
        .unwrap();
        copy(
            &jar,
            &root.join("usr/lib/droidloom/runtime/ime/DroidloomIME.apk"),
        )
        .unwrap();

        copy(
            &jar,
            &root.join("usr/lib/droidloom/runtime/ime/DroidloomHome.apk"),
        )
        .unwrap();
        copy(
            &jar,
            &root.join("usr/lib/droidloom/runtime/systemui/SystemUI.apk"),
        )
        .unwrap();
        write(
            &root.join("usr/lib/droidloom/runtime/ime/home-setup"),
            b"#!/system/bin/sh\n",
        )
        .unwrap();
    }
    #[test]
    fn refuses_bundle_without_minimal_system_apps() {
        for relative in [
            "ime/DroidloomHome.apk",
            "ime/home-setup",
            "systemui/SystemUI.apk",
            "lib64/libinputflinger.so",
        ] {
            let d = tempfile::tempdir().unwrap();
            fixture(d.path(), 5, 5);
            fs::remove_file(d.path().join("usr/lib/droidloom/runtime").join(relative)).unwrap();
            assert!(seal(d.path(), "fixture".into()).is_err());
        }
    }
    #[test]
    fn refuses_stock_or_mismatched_window_policy_jni() {
        let d = tempfile::tempdir().unwrap();
        fixture(d.path(), 5, 5);
        let path = d
            .path()
            .join("usr/lib/droidloom/runtime/lib64/libinputflinger.so");
        let native = fs::read(&path).unwrap();
        for marker in [
            b"DROIDLOOM_WINDOW_POLICY_ABI=0;",
            b"DROIDLOOM_WINDOW_POLICY_ABI=2;",
        ] {
            let mut wrong = native[..64].to_vec();
            wrong.extend_from_slice(marker);
            write(&path, wrong).unwrap();
            assert!(seal(d.path(), "fixture".into()).is_err());
        }
        let mut wrong_arch = native;
        wrong_arch[18..20].copy_from_slice(&183_u16.to_le_bytes());
        write(&path, wrong_arch).unwrap();
        assert!(seal(d.path(), "fixture".into()).is_err());
    }
    #[test]
    fn refuses_retired_input_support_process() {
        let d = tempfile::tempdir().unwrap();
        fixture(d.path(), 5, 5);
        seal(d.path(), "fixture".into()).unwrap();
        write(
            &d.path()
                .join("usr/lib/droidloom/runtime/ime/droidloom-input-bridge.rc"),
            b"service droidloom-input-bridge /vendor/bin/droidloom-input-bridge\n",
        )
        .unwrap();
        assert!(verify(d.path()).is_err());
        assert!(seal(d.path(), "fixture".into()).is_err());
    }
    #[test]
    fn direct_channels_and_owned_input_require_every_matching_peer() {
        for (relative, marker) in [
            (
                "framework/services.jar",
                b"DROIDLOOM_VULKAN_CAPABILITIES_ABI=2;".as_slice(),
            ),
            (
                "lib64/libinputflinger.so",
                b"DROIDLOOM_VULKAN_CAPABILITIES_ABI=2;".as_slice(),
            ),
            (
                "framework/services.jar",
                b"DROIDLOOM_HOST_CHANNELS_ABI=1;".as_slice(),
            ),
            (
                "systemui/SystemUI.apk",
                b"DROIDLOOM_HOST_CHANNELS_ABI=1;".as_slice(),
            ),
            (
                "ime/DroidloomIME.apk",
                b"DROIDLOOM_HOST_CHANNELS_ABI=1;".as_slice(),
            ),
            (
                "../../../bin/droidloom-wayland",
                b"DROIDLOOM_HOST_CHANNELS_ABI=1;".as_slice(),
            ),
            (
                "framework/services.jar",
                b"DROIDLOOM_NATIVE_INPUT_ABI=2;".as_slice(),
            ),
            (
                "lib64/libinputflinger.so",
                b"DROIDLOOM_NATIVE_INPUT_ABI=2;".as_slice(),
            ),
            (
                "bin/android.hardware.graphics.composer3-service.droidloom",
                b"DROIDLOOM_NATIVE_INPUT_ABI=2;".as_slice(),
            ),
        ] {
            let root = tempfile::tempdir().unwrap();
            fixture(root.path(), 7, 7);
            let path = root.path().join("usr/lib/droidloom/runtime").join(relative);
            let archive = matches!(
                path.extension().and_then(|s| s.to_str()),
                Some("jar" | "apk")
            );
            let mut data = if archive {
                dex(&path).unwrap()
            } else {
                fs::read(&path).unwrap()
            };
            while let Some(start) = data.windows(marker.len()).position(|w| w == marker) {
                data.drain(start..start + marker.len());
            }
            if archive {
                refresh_dex_fixture(&mut data);
                let temporary = tempfile::tempdir().unwrap();
                write(&temporary.path().join("classes.dex"), &data).unwrap();
                run(Command::new("zip")
                    .current_dir(temporary.path())
                    .args(["-q"])
                    .arg(&path)
                    .arg("classes.dex"))
                .unwrap();
            } else {
                write(&path, data).unwrap();
            }
            let error = seal(root.path(), "mismatched".into())
                .unwrap_err()
                .to_string();
            assert!(error.contains("ABI"), "{relative}: {error}");
        }
    }
    #[test]
    fn dex_checksums_match_adler32_reference_vectors() {
        assert_eq!(dex_checksum(b""), 1);
        assert_eq!(dex_checksum(b"Wikipedia"), 0x11e60398);
    }
    #[test]
    fn rejects_damaged_dex_headers_and_payloads() {
        let good = dex_fixture(b"compiled bytecode");
        validate_dex(&good).unwrap();
        for offset in [0, 8, 12, 32, 36, 40, 112] {
            let mut damaged = good.clone();
            damaged[offset] ^= 1;
            assert!(validate_dex(&damaged).is_err(), "offset {offset}");
        }
        for length in [0, 7, 111, good.len() - 1] {
            assert!(validate_dex(&good[..length]).is_err(), "length {length}");
        }
    }
    #[test]
    fn rejects_corrupt_secondary_dex_even_with_valid_zip_crc() {
        let d = tempfile::tempdir().unwrap();
        fixture(d.path(), 7, 7);
        let jar = d
            .path()
            .join("usr/lib/droidloom/runtime/framework/services.jar");
        let temporary = tempfile::tempdir().unwrap();
        let mut secondary = dex_fixture(b"secondary bytecode");
        write(&temporary.path().join("classes2.dex"), &secondary).unwrap();
        let append = || {
            run(Command::new("zip")
                .current_dir(temporary.path())
                .arg("-q")
                .arg(&jar)
                .arg("classes2.dex"))
            .unwrap();
        };
        append();
        seal(d.path(), "valid-multidex".into()).unwrap();
        // Repack a corrupted DEX, giving the ZIP member a correct CRC. This
        // reproduces a damaged compiler output that transfer hashing accepts.
        secondary[112] ^= 1;
        write(&temporary.path().join("classes2.dex"), secondary).unwrap();
        append();
        for error in [
            seal(d.path(), "bad-multidex".into()).unwrap_err(),
            verify(d.path()).unwrap_err(),
        ] {
            let message = error.to_string();
            assert!(message.contains("services.jar!classes2.dex"), "{message}");
            assert!(message.contains("bad DEX checksum"), "{message}");
        }
    }
    #[test]
    fn refuses_mixed_native_java_bundle() {
        let d = tempfile::tempdir().unwrap();
        fixture(d.path(), 4, 5);
        assert!(seal(d.path(), "fixture".into()).is_err());
        assert!(!d.path().join(MANIFEST).exists());
    }
    #[test]
    fn checks_required_components_without_requiring_identical_bytes() {
        let d = tempfile::tempdir().unwrap();
        fixture(d.path(), 5, 5);
        seal(d.path(), "fixture".into()).unwrap();
        verify(d.path()).unwrap();
        let file = d.path().join(
            "usr/lib/droidloom/runtime/bin/android.hardware.graphics.composer3-service.droidloom",
        );
        let original = fs::read(&file).unwrap();
        write(&file, b"corrupt").unwrap();
        assert!(verify(d.path()).is_err());
        let mut compatible = original.clone();
        compatible.extend_from_slice(b"different build metadata");
        write(&file, &compatible).unwrap();
        verify(d.path()).unwrap();
        fs::remove_file(&file).unwrap();
        assert!(verify(d.path()).is_err());
        write(&file, &original).unwrap();
        write(&d.path().join("unexpected"), b"old component").unwrap();
        verify(d.path()).unwrap();
    }
    #[test]
    fn rejects_foreign_architecture_in_other_component() {
        let d = tempfile::tempdir().unwrap();
        fixture(d.path(), 5, 5);
        let mut elf = vec![0; 20];
        elf[..4].copy_from_slice(b"\x7fELF");
        elf[4] = 2;
        elf[5] = 1;
        elf[18] = 183;
        write(&d.path().join("wrong-library.so"), elf).unwrap();
        assert!(seal(d.path(), "fixture".into()).is_err());
    }
}
