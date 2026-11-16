//! Exercises the pinned upstream libdrm against synthetic DRM kernel replies.
use std::{fs, path::Path, process::Command};

#[test]
#[ignore = "requires the pinned .work/arch/build/aosp-source libdrm checkout and host C compiler"]
fn drm_version_accepts_empty_kernel_strings() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let source = repo.join(".work/arch/build/aosp-source/external/libdrm");
    let temp = tempfile::tempdir().unwrap();
    // The checkout's committed tree, upgraded and patched as the build does.
    let tree = temp.path().join("libdrm");
    fs::create_dir(&tree).unwrap();
    let archive = temp.path().join("libdrm.tar");
    let pristine = Command::new("git")
        .current_dir(&source)
        .args(["archive", "--output"])
        .arg(&archive)
        .arg("HEAD")
        .output()
        .unwrap();
    assert!(pristine.status.success());
    let unpack = Command::new("tar")
        .arg("-xf")
        .arg(&archive)
        .arg("-C")
        .arg(&tree)
        .output()
        .unwrap();
    assert!(unpack.status.success());
    for patch in [
        "0036-libdrm-2.4.134.patch",
        "0015-libdrm-empty-version-strings.patch",
    ] {
        let apply = Command::new("git")
            .current_dir(&tree)
            .arg("apply")
            .arg(repo.join("android/aosp-patches").join(patch))
            .output()
            .unwrap();
        assert!(
            apply.status.success(),
            "{patch}: {}",
            String::from_utf8_lossy(&apply.stderr)
        );
    }
    let binary = temp.path().join("version-test");
    let compile = Command::new("cc")
        .args([
            "-std=gnu11",
            "-DMAJOR_IN_SYSMACROS",
            "-ffunction-sections",
            "-fdata-sections",
            "-Wl,--gc-sections",
            "-Wl,--wrap=ioctl",
            "-I",
        ])
        .arg(&tree)
        .arg("-I")
        .arg(tree.join("include/drm"))
        .arg(tree.join("xf86drm.c"))
        .arg(repo.join("android/mesa/tests/libdrm_version.c"))
        .arg("-o")
        .arg(&binary)
        .output()
        .unwrap();
    assert!(
        compile.status.success(),
        "{}",
        String::from_utf8_lossy(&compile.stderr)
    );
    assert!(Command::new(binary).status().unwrap().success());
}
