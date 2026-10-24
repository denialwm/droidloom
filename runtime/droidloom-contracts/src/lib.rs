//! Versioned compatibility contracts shared by Droidloom host components.
//!
//! The data structures in this crate describe artifacts. They do not create
//! namespaces, mount filesystems, or start processes.

#![forbid(unsafe_code)]

pub mod applications;
pub mod gapps;

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::fs;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Image manifest schema accepted by this runtime.
pub const IMAGE_MANIFEST_SCHEMA: u32 = 1;
/// Runtime ABI implemented by this source tree.
pub const RUNTIME_ABI: u32 = 1;
/// Denial-native protocol required by the first graphics milestone.
pub const DENIAL_PROTOCOL: &str = "denial_droidloom_native";
/// Denial-native protocol version required by this runtime.
pub const DENIAL_PROTOCOL_VERSION: u32 = 1;
/// Native CPU architecture selected when this runtime was compiled.
#[cfg(target_arch = "aarch64")]
pub const TARGET_ARCHITECTURE: Architecture = Architecture::Aarch64;
/// Native CPU architecture selected when this runtime was compiled.
#[cfg(target_arch = "x86_64")]
pub const TARGET_ARCHITECTURE: Architecture = Architecture::X86_64;
#[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
compile_error!("Droidloom supports only aarch64 and x86_64 hosts");
/// Contiguous subordinate IDs reserved for one Android user-0 cell.
pub const MIN_SUBORDINATE_IDS: u64 = 100_000;
/// Immutable AOSP release selected for the first Droidloom product.
pub const PINNED_AOSP_TAG: &str = "android-17.0.0_r1";
/// Commit referenced by the selected annotated AOSP manifest tag.
pub const PINNED_AOSP_COMMIT: &str = "5bc9a7ce1cd78dd53613bbfd0ebf506e1e4adb0f";
/// Stable Mesa release selected for the Android/Bionic graphics stack.
pub const PINNED_MESA_VERSION: &str = "26.1.7";
/// SHA-256 of the selected upstream Mesa release archive.
pub const PINNED_MESA_SHA256: &str =
    "25e0a669e6638c3563e7be32a0a09f1888317e6eed0d047dc41d49dc8de26c7d";

/// CPU architecture encoded in an image manifest.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Architecture {
    /// 64-bit Arm.
    Aarch64,
    /// 64-bit x86.
    X86_64,
}

impl Architecture {
    /// Return the Rust/Linux spelling for this architecture.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Aarch64 => "aarch64",
            Self::X86_64 => "x86_64",
        }
    }
}

/// A reproducibly identified upstream Git input.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitSource {
    /// Canonical HTTPS repository URL.
    pub url: String,
    /// Immutable tag used for human-facing provenance.
    pub tag: String,
    /// Full 40-character commit object ID resolved from `tag`.
    pub commit: String,
}

/// A reproducibly identified release archive.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchiveSource {
    /// Canonical HTTPS archive URL.
    pub url: String,
    /// Upstream release version.
    pub version: String,
    /// Lower-case SHA-256 digest of the archive.
    pub sha256: String,
}

/// One immutable or mutable image artifact.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageArtifact {
    /// Stable role such as `system`, `vendor`, or `product`.
    pub role: String,
    /// Package-relative artifact path.
    pub path: PathBuf,
    /// Lower-case SHA-256 digest of the artifact.
    pub sha256: String,
    /// Whether the runtime must expose the artifact read-only.
    pub read_only: bool,
}

/// Features promised by an Android image.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageFeatures {
    /// Image provides a current AIDL Composer service.
    pub composer_aidl: bool,
    /// Image provides a GBM/DMA-BUF allocator and mapper.
    pub dmabuf_allocator: bool,
    /// Image contains Android/Bionic Freedreno GLES.
    pub freedreno_gles: bool,
    /// Image contains Android/Bionic Turnip Vulkan.
    pub turnip_vulkan: bool,
    /// Image contains Android/Bionic `RadeonSI` GLES.
    #[serde(default)]
    pub radeonsi_gles: bool,
    /// Image contains Android/Bionic RADV Vulkan.
    #[serde(default)]
    pub radv_vulkan: bool,
    /// Image uses flattened APEX packages for this non-phone boot model.
    pub flattened_apex: bool,
}

/// Versioned compatibility manifest shipped with each Droidloom image.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageManifest {
    /// Manifest schema revision.
    pub schema_version: u32,
    /// Unique package/image identity.
    pub image_id: String,
    /// Package version of this image.
    pub image_version: String,
    /// Host ABI required by this image.
    pub runtime_abi: u32,
    /// Native CPU architecture.
    pub architecture: Architecture,
    /// Pinned AOSP manifest source.
    pub aosp: GitSource,
    /// Pinned Mesa release source.
    pub mesa: ArchiveSource,
    /// Required Denial protocol global.
    pub denial_protocol: String,
    /// Required Denial protocol version.
    pub denial_protocol_version: u32,
    /// Packaged filesystem artifacts.
    pub artifacts: Vec<ImageArtifact>,
    /// Feature promises used to reject incomplete images before activation.
    pub features: ImageFeatures,
}

/// Failure to read, parse, or validate a manifest.
#[derive(Debug, Error)]
pub enum ManifestError {
    /// The manifest could not be read.
    #[error("cannot read manifest {path}: {source}")]
    Read {
        /// Path that failed.
        path: PathBuf,
        /// Underlying I/O error.
        source: std::io::Error,
    },
    /// JSON syntax or shape was invalid.
    #[error("invalid JSON manifest {path}: {source}")]
    Json {
        /// Path that failed.
        path: PathBuf,
        /// Underlying JSON error.
        source: serde_json::Error,
    },
    /// The document was syntactically valid but violated the contract.
    #[error("image manifest is incompatible:\n{0}")]
    Invalid(String),
}

impl ImageManifest {
    /// Read and validate a manifest from disk.
    ///
    /// # Errors
    ///
    /// Returns [`ManifestError`] when the file cannot be read, its JSON shape
    /// is invalid, or its values are incompatible with this runtime.
    pub fn load(path: &Path) -> Result<Self, ManifestError> {
        let bytes = fs::read(path).map_err(|source| ManifestError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        let manifest =
            serde_json::from_slice::<Self>(&bytes).map_err(|source| ManifestError::Json {
                path: path.to_path_buf(),
                source,
            })?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Validate schema, ABI, provenance, artifact paths, and feature promises.
    ///
    /// # Errors
    ///
    /// Returns [`ManifestError::Invalid`] with every discovered contract
    /// violation rather than stopping at the first one.
    pub fn validate(&self) -> Result<(), ManifestError> {
        let mut problems = Vec::new();

        if self.schema_version != IMAGE_MANIFEST_SCHEMA {
            problems.push(format!(
                "schema_version {} is not supported (expected {IMAGE_MANIFEST_SCHEMA})",
                self.schema_version
            ));
        }
        if self.runtime_abi != RUNTIME_ABI {
            problems.push(format!(
                "runtime_abi {} is not supported (expected {RUNTIME_ABI})",
                self.runtime_abi
            ));
        }
        if self.architecture != TARGET_ARCHITECTURE {
            problems.push(format!(
                "architecture {} is not supported (expected {})",
                self.architecture.as_str(),
                TARGET_ARCHITECTURE.as_str()
            ));
        }
        if self.image_id.trim().is_empty() || self.image_version.trim().is_empty() {
            problems.push("image_id and image_version must be non-empty".to_owned());
        }
        validate_git_source("aosp", &self.aosp, &mut problems);
        validate_archive_source("mesa", &self.mesa, &mut problems);

        if self.denial_protocol != DENIAL_PROTOCOL
            || self.denial_protocol_version != DENIAL_PROTOCOL_VERSION
        {
            problems.push(format!(
                "Denial protocol must be {DENIAL_PROTOCOL}@{DENIAL_PROTOCOL_VERSION}"
            ));
        }

        if self.artifacts.is_empty() {
            problems.push("at least one packaged image artifact is required".to_owned());
        }
        let mut roles = BTreeSet::new();
        for artifact in &self.artifacts {
            if !roles.insert(artifact.role.as_str()) {
                problems.push(format!("duplicate artifact role {:?}", artifact.role));
            }
            if !is_safe_relative_path(&artifact.path) {
                problems.push(format!(
                    "artifact {:?} path {} must be a normalized package-relative path",
                    artifact.role,
                    artifact.path.display()
                ));
            }
            if !is_sha256(&artifact.sha256) {
                problems.push(format!(
                    "artifact {:?} has an invalid SHA-256 digest",
                    artifact.role
                ));
            }
            if matches!(artifact.role.as_str(), "system" | "vendor" | "product")
                && !artifact.read_only
            {
                problems.push(format!(
                    "artifact {:?} must be mounted read-only",
                    artifact.role
                ));
            }
        }

        let required_roles = ["system", "vendor", "product"];
        for role in required_roles {
            if !roles.contains(role) {
                problems.push(format!("required artifact role {role:?} is absent"));
            }
        }

        let features = &self.features;
        if !(features.composer_aidl && features.dmabuf_allocator && features.flattened_apex) {
            problems.push(
                "images must promise Composer AIDL, DMA-BUF allocation, and flattened APEX"
                    .to_owned(),
            );
        }
        let native_graphics = match self.architecture {
            Architecture::Aarch64 => features.freedreno_gles && features.turnip_vulkan,
            Architecture::X86_64 => features.radeonsi_gles && features.radv_vulkan,
        };
        if !native_graphics {
            problems.push(format!(
                "{} images must promise their native Mesa GLES and Vulkan drivers",
                self.architecture.as_str()
            ));
        }

        if problems.is_empty() {
            Ok(())
        } else {
            let mut rendered = String::new();
            for problem in problems {
                let _ = writeln!(rendered, "- {problem}");
            }
            Err(ManifestError::Invalid(rendered.trim_end().to_owned()))
        }
    }
}

fn validate_git_source(name: &str, source: &GitSource, problems: &mut Vec<String>) {
    if !source.url.starts_with("https://") {
        problems.push(format!("{name}.url must use HTTPS"));
    }
    if source.tag.trim().is_empty() {
        problems.push(format!("{name}.tag must be non-empty"));
    }
    if !is_hex(&source.commit, 40) {
        problems.push(format!("{name}.commit must be a full 40-character Git ID"));
    }
}

fn validate_archive_source(name: &str, source: &ArchiveSource, problems: &mut Vec<String>) {
    if !source.url.starts_with("https://") {
        problems.push(format!("{name}.url must use HTTPS"));
    }
    if source.version.trim().is_empty() {
        problems.push(format!("{name}.version must be non-empty"));
    }
    if !is_sha256(&source.sha256) {
        problems.push(format!("{name}.sha256 must be a lower-case SHA-256 digest"));
    }
}

fn is_hex(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn is_sha256(value: &str) -> bool {
    is_hex(value, 64)
}

fn is_safe_relative_path(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && path
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_manifest() -> ImageManifest {
        ImageManifest {
            schema_version: IMAGE_MANIFEST_SCHEMA,
            image_id: "org.droidloom.aosp".to_owned(),
            image_version: "17.0.0-r1".to_owned(),
            runtime_abi: RUNTIME_ABI,
            architecture: TARGET_ARCHITECTURE,
            aosp: GitSource {
                url: "https://android.googlesource.com/platform/manifest".to_owned(),
                tag: PINNED_AOSP_TAG.to_owned(),
                commit: PINNED_AOSP_COMMIT.to_owned(),
            },
            mesa: ArchiveSource {
                url: "https://archive.mesa3d.org/mesa-26.1.7.tar.xz".to_owned(),
                version: PINNED_MESA_VERSION.to_owned(),
                sha256: PINNED_MESA_SHA256.to_owned(),
            },
            denial_protocol: DENIAL_PROTOCOL.to_owned(),
            denial_protocol_version: DENIAL_PROTOCOL_VERSION,
            artifacts: ["system", "vendor", "product"]
                .into_iter()
                .map(|role| ImageArtifact {
                    role: role.to_owned(),
                    path: PathBuf::from(format!("images/{role}.img")),
                    sha256: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                        .to_owned(),
                    read_only: true,
                })
                .collect(),
            features: ImageFeatures {
                composer_aidl: true,
                dmabuf_allocator: true,
                freedreno_gles: true,
                turnip_vulkan: true,
                radeonsi_gles: true,
                radv_vulkan: true,
                flattened_apex: true,
            },
        }
    }

    #[test]
    fn accepts_a_complete_manifest() {
        valid_manifest().validate().unwrap();
    }

    #[test]
    fn rejects_path_traversal_and_mutable_system() {
        let mut manifest = valid_manifest();
        manifest.artifacts[0].path = PathBuf::from("../system.img");
        manifest.artifacts[0].read_only = false;

        let error = manifest.validate().unwrap_err().to_string();
        assert!(error.contains("package-relative"));
        assert!(error.contains("mounted read-only"));
    }

    #[test]
    fn rejects_unknown_json_fields() {
        let mut value = serde_json::to_value(valid_manifest()).unwrap();
        value["surprise"] = serde_json::json!(true);
        assert!(serde_json::from_value::<ImageManifest>(value).is_err());
    }
}
