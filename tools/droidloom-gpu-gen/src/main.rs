//! Generate the GPU forwarding protocol from vk.xml and the allow-list.
//!
//! `droidloom-gpu-gen [--check] [--registry vk.xml]` writes (or, with
//! `--check`, compares) three files: the shared protocol, the Android-side
//! encoders and the host-side handlers. The registry defaults to the Vulkan
//! headers pinned by the AOSP source lock, the ones Android's loader uses.

mod client;
mod model;
mod protocol;
mod registry;
mod rust;
mod server;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

const ALLOWLIST: &str = "graphics/droidloom-gpu-protocol/api.txt";
const DEFAULT_REGISTRY: &str = ".work/arch/build/aosp-source/external/vulkan-headers/registry/vk.xml";
const OUTPUTS: [&str; 3] = [
    "graphics/droidloom-gpu-protocol/src/generated.rs",
    "android/gpu/droidloom-vulkan/src/generated.rs",
    "runtime/droidloom-gpu-server/src/generated.rs",
];

fn main() -> ExitCode {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut check = false;
    let mut registry_path = root.join(DEFAULT_REGISTRY);
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--check" => check = true,
            "--registry" => registry_path = PathBuf::from(arguments.next().expect("--registry needs a path")),
            other => {
                eprintln!("unknown argument {other}");
                return ExitCode::FAILURE;
            }
        }
    }
    let text = std::fs::read_to_string(&registry_path)
        .unwrap_or_else(|error| panic!("read {}: {error}", registry_path.display()));
    let allowlist = std::fs::read_to_string(root.join(ALLOWLIST)).expect("read the allow-list");
    let registry = registry::Registry::parse(&text);
    let api = model::Api::build(&registry, &allowlist);
    let outputs = [protocol::emit(&api), client::emit(&api), server::emit(&api)];

    let mut stale = false;
    for (path, content) in OUTPUTS.iter().zip(outputs) {
        let path = root.join(path);
        if check {
            if std::fs::read_to_string(&path).ok().as_deref() != Some(content.as_str()) {
                eprintln!("{} is stale; run droidloom-gpu-gen", path.display());
                stale = true;
            }
        } else {
            std::fs::create_dir_all(path.parent().expect("output directory")).expect("create output directory");
            std::fs::write(&path, content).unwrap_or_else(|error| panic!("write {}: {error}", path.display()));
        }
    }
    if stale { ExitCode::FAILURE } else { ExitCode::SUCCESS }
}
