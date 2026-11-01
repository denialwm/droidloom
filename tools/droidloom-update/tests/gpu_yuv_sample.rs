//! Sample YUV AHardwareBuffers through the installed graphics stack in the
//! running cell, as SurfaceFlinger does with video. Opens no window.
use std::{
    fs,
    io::Write,
    path::Path,
    process::{Command, Stdio},
};

const DEV: &str = "/usr/local/bin/droidloom-dev";
/// Kept after the run, for checks against a driver build (docs/gpu-forwarding.md).
const CELL_BINARY: &str = "/data/local/tmp/droidloom-yuv-sample";

fn checked(command: &mut Command) {
    let result = command.output().expect("test build tool is installed");
    assert!(
        result.status.success(),
        "{}{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}

/// The clang Soong builds with, as `build/soong` names it.
fn clang(aosp: &Path) -> std::path::PathBuf {
    let global = fs::read_to_string(aosp.join("build/soong/cc/config/global.go")).unwrap();
    let version = global
        .lines()
        .find_map(|line| line.trim().strip_prefix("ClangDefaultVersion = "))
        .expect("Soong names its clang")
        .trim_matches('"');
    aosp.join("prebuilts/clang/host/linux-x86").join(version)
}

#[test]
#[ignore = "requires the pinned AOSP build outputs and a running forwarding cell"]
fn yuv_hardware_buffers_sample_through_angle() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let build = repo.join(".work/arch/build");
    let aosp = build.join("aosp-source");
    let soong = build.join("android-out/soong/.intermediates");
    let clang = clang(&aosp);
    let resource = fs::read_dir(clang.join("lib/clang"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let temp = tempfile::tempdir().unwrap();
    let object = temp.path().join("yuv_sample.o");
    let binary = temp.path().join("yuv_sample");

    let mut compile = Command::new(clang.join("bin/clang"));
    compile.args([
        "--target=x86_64-linux-android37",
        "-O2",
        "-fPIE",
        "-Wall",
        "-Werror",
        "-nostdinc",
    ]);
    for include in [
        resource.join("include"),
        aosp.join("bionic/libc/include"),
        aosp.join("bionic/libc/kernel/uapi"),
        aosp.join("bionic/libc/kernel/uapi/asm-x86"),
        aosp.join("bionic/libc/kernel/android/uapi"),
    ] {
        compile.arg("-isystem").arg(include);
    }
    checked(
        compile
            .arg("-c")
            .arg(repo.join("android/gpu/droidloom-vulkan/tests/yuv_sample.c"))
            .arg("-o")
            .arg(&object),
    );
    let shared = |path: &str, name: &str| soong.join(path).join("android_x86_64_shared").join(name);
    checked(
        Command::new(clang.join("bin/ld.lld"))
            .args([
                "-m",
                "elf_x86_64",
                "-pie",
                "-z",
                "max-page-size=16384",
                "--hash-style=gnu",
            ])
            .args(["-dynamic-linker", "/system/bin/linker64"])
            .arg(soong.join("bionic/libc/crtbegin_dynamic/android_x86_64/crtbegin_dynamic.o"))
            .arg(&object)
            .arg(shared(
                "frameworks/native/libs/nativewindow/libnativewindow",
                "libnativewindow.so",
            ))
            .arg(shared("bionic/libdl/libdl", "libdl.so"))
            .arg(shared("bionic/libc/libc", "libc.so"))
            .arg(resource.join("lib/linux/libclang_rt.builtins-x86_64-android.a"))
            .arg(soong.join("bionic/libc/crtend_android/android_x86_64/crtend_android.o"))
            .arg("-o")
            .arg(&binary),
    );

    let mut copy = Command::new("pkexec")
        .args([
            DEV,
            "shell",
            &format!("cat > {CELL_BINARY} && chmod 755 {CELL_BINARY}"),
        ])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    copy.stdin
        .take()
        .unwrap()
        .write_all(&fs::read(&binary).unwrap())
        .unwrap();
    assert!(copy.wait().unwrap().success());
    let run = Command::new("timeout")
        .args(["-s", "KILL", "60", "pkexec", DEV, "shell", CELL_BINARY])
        .output()
        .unwrap();
    let report = String::from_utf8_lossy(&run.stdout);
    println!("{report}");
    assert!(
        run.status.success(),
        "{report}{}",
        String::from_utf8_lossy(&run.stderr)
    );
}
