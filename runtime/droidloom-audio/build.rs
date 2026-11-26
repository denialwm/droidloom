//! Compile only the narrow PipeWire/SPA C ABI adapter; stream policy stays in Rust.
fn main() {
    let flags = std::process::Command::new("pkg-config")
        .args(["--cflags", "libpipewire-0.3"])
        .output()
        .expect("pkg-config is required for PipeWire headers");
    assert!(
        flags.status.success(),
        "libpipewire-0.3 development headers are required"
    );
    let mut build = cc::Build::new();
    build
        .file("src/pipewire.c")
        .file("src/volume.c")
        .flag("-std=c11")
        .flag("-D_GNU_SOURCE");
    for flag in String::from_utf8(flags.stdout).unwrap().split_whitespace() {
        build.flag(flag);
    }
    build.warnings(true).compile("droidloom_pipewire");
    println!("cargo:rustc-link-lib=pipewire-0.3");
    println!("cargo:rerun-if-changed=src/pipewire.c");
    println!("cargo:rerun-if-changed=src/volume.c");
}
