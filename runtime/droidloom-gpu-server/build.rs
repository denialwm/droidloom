//! Compile the texture decoders to SPIR-V: one ETC2 module per variant
//! (shaders/etc2.comp) and the ASTC one.

use std::path::Path;
use std::process::Command;

fn glslang(source: &Path, out: &Path, defines: &[String]) {
    let status = Command::new("glslangValidator")
        .args(["-V", "--target-env", "vulkan1.1", "-S", "comp", "--quiet"])
        .args(defines)
        .arg(source)
        .arg("-o")
        .arg(out)
        .status()
        .unwrap_or_else(|error| panic!("glslangValidator (from glslang) is needed to build the texture decoders: {error}"));
    assert!(status.success(), "glslangValidator failed on {}", source.display());
}

fn main() {
    let out = std::env::var("OUT_DIR").unwrap();
    let out = Path::new(&out);
    let shaders = Path::new("shaders");
    for variant in 0..7 {
        glslang(&shaders.join("etc2.comp"), &out.join(format!("etc2-{variant}.spv")), &[format!("-DVARIANT={variant}")]);
    }
    glslang(&shaders.join("astc.comp"), &out.join("astc.spv"), &[]);
    println!("cargo:rerun-if-changed=shaders");
}
