//! Which host GPU renders for Android, and how.
//!
//! AMD, Intel and Qualcomm Adreno GPUs have Android drivers (Mesa), so
//! Android renders on them natively. NVIDIA's driver has no Android build: an NVIDIA GPU renders
//! through `droidloom-gpu-server`, and the cell's own render node stays on an
//! AMD or Intel GPU when the host has one. The choice is `auto`, a vendor or
//! a PCI address; GPUs are named by PCI address, or an Adreno, which sits on
//! the SoC, by its platform device, because render-node numbers follow driver
//! load order and can change between boots.

use std::fmt::{self, Display};
use std::path::Path;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::GraphicsBackend;

/// A GPU with a render node.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostGpu {
    /// `/dev/dri/renderD<number>`.
    pub render_node: String,
    /// PCI address, e.g. `0000:04:00.0`, or for an Adreno its platform
    /// device, e.g. `ae01000.display-controller`.
    pub device: String,
    /// Kernel driver, e.g. `amdgpu`.
    pub driver: String,
}

impl HostGpu {
    /// Every render node that `sys` (`/sys/class/drm`) lists with a bound
    /// driver, on a PCI device or, for an Adreno, a platform device, in node
    /// order.
    pub fn list(sys: &Path) -> std::io::Result<Vec<Self>> {
        let mut nodes = std::fs::read_dir(sys)?.collect::<std::io::Result<Vec<_>>>()?;
        nodes.sort_by_key(|n| n.file_name());
        let mut gpus = Vec::new();
        for node in nodes {
            let name = node.file_name().to_string_lossy().into_owned();
            if !name
                .strip_prefix("renderD")
                .is_some_and(|tail| !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit()))
            {
                continue;
            }
            let device = node.path().join("device");
            let (Ok(device), Ok(driver)) = (std::fs::canonicalize(&device), std::fs::canonicalize(device.join("driver"))) else {
                continue;
            };
            let (Some(device), Some(driver)) = (device.file_name().and_then(|p| p.to_str()), driver.file_name().and_then(|d| d.to_str())) else {
                continue;
            };
            let gpu = Self { render_node: format!("/dev/dri/{name}"), device: device.into(), driver: driver.into() };
            if is_pci_address(device) || gpu.vendor() == Some(Vendor::Qualcomm) {
                gpus.push(gpu);
            }
        }
        Ok(gpus)
    }

    /// The vendor Droidloom supports this GPU's driver for.
    pub fn vendor(&self) -> Option<Vendor> {
        match self.driver.as_str() {
            "amdgpu" => Some(Vendor::Amd),
            "i915" | "xe" => Some(Vendor::Intel),
            "nvidia" => Some(Vendor::Nvidia),
            "msm" | "msm_dpu" | "msm_mdp5" => Some(Vendor::Qualcomm),
            _ => None,
        }
    }

    fn native(&self) -> bool {
        matches!(self.vendor(), Some(Vendor::Amd | Vendor::Intel | Vendor::Qualcomm))
    }
}

impl Display for HostGpu {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let vendor = self.vendor().map_or("unsupported", Vendor::label);
        write!(f, "{vendor} GPU {} ({}, {})", self.device, self.driver, self.render_node)
    }
}

/// A GPU vendor Droidloom has a driver for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Vendor {
    /// `amdgpu`, rendering natively.
    Amd,
    /// `i915` or `xe`, rendering natively.
    Intel,
    /// `nvidia`, rendering through the GPU server.
    Nvidia,
    /// `msm`, an Adreno on a Qualcomm SoC, rendering natively.
    Qualcomm,
}

impl Vendor {
    fn label(self) -> &'static str {
        match self {
            Self::Amd => "AMD",
            Self::Intel => "Intel",
            Self::Nvidia => "NVIDIA",
            Self::Qualcomm => "Qualcomm",
        }
    }
}

/// The GPU Android should render on, as `/etc/droidloom/cell.json` names it.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum GpuChoice {
    /// NVIDIA when the host has one, else the first AMD, Intel or Qualcomm GPU.
    #[default]
    Auto,
    /// The first GPU of this vendor.
    Vendor(Vendor),
    /// One GPU by PCI address.
    Pci(String),
}

impl GpuChoice {
    /// Whether this is the default choice.
    pub fn is_auto(&self) -> bool {
        *self == Self::Auto
    }
}

impl FromStr for GpuChoice {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, String> {
        Ok(match value.to_ascii_lowercase().as_str() {
            "auto" => Self::Auto,
            "amd" => Self::Vendor(Vendor::Amd),
            "intel" => Self::Vendor(Vendor::Intel),
            "nvidia" => Self::Vendor(Vendor::Nvidia),
            "qualcomm" => Self::Vendor(Vendor::Qualcomm),
            pci if is_pci_address(pci) => Self::Pci(pci.into()),
            // `lspci` leaves out the PCI domain.
            pci if is_pci_address(&format!("0000:{pci}")) => Self::Pci(format!("0000:{pci}")),
            _ => return Err(format!("GPU must be auto, amd, intel, nvidia, qualcomm or a PCI address such as 0000:04:00.0, not {value:?}")),
        })
    }
}

impl TryFrom<String> for GpuChoice {
    type Error = String;

    fn try_from(value: String) -> Result<Self, String> {
        value.parse()
    }
}

impl From<GpuChoice> for String {
    fn from(choice: GpuChoice) -> Self {
        choice.to_string()
    }
}

impl Display for GpuChoice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Auto => f.write_str("auto"),
            Self::Vendor(vendor) => f.write_str(&vendor.label().to_ascii_lowercase()),
            Self::Pci(pci) => f.write_str(pci),
        }
    }
}

/// `dddd:bb:dd.f` in lowercase hexadecimal.
fn is_pci_address(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 12
        && [4, 7].iter().all(|&i| bytes[i] == b':')
        && bytes[10] == b'.'
        && bytes.iter().enumerate().all(|(i, b)| matches!(i, 4 | 7 | 10) || matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// How the cell renders: what setup writes into `cell.json` and
/// `runtime.env`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Graphics {
    /// The GPU Android's rendering runs on.
    pub renderer: HostGpu,
    /// The cell's only render node: the renderer's own, or for forwarding an
    /// AMD or Intel one when the host has it.
    pub render_node: String,
    /// The rendering and allocation pairing.
    pub backend: GraphicsBackend,
}

impl Graphics {
    /// The rendering `choice` names among `gpus`.
    pub fn resolve(choice: &GpuChoice, gpus: &[HostGpu]) -> Result<Self, String> {
        let supported = || gpus.iter().filter(|g| g.vendor().is_some());
        let renderer = match choice {
            GpuChoice::Auto => supported().find(|g| g.vendor() == Some(Vendor::Nvidia)).or_else(|| supported().next()),
            GpuChoice::Vendor(vendor) => supported().find(|g| g.vendor() == Some(*vendor)),
            GpuChoice::Pci(pci) => gpus.iter().find(|g| g.device == *pci),
        };
        let found = || {
            if gpus.is_empty() {
                "none".to_owned()
            } else {
                gpus.iter().map(ToString::to_string).collect::<Vec<_>>().join("; ")
            }
        };
        let renderer = renderer.ok_or_else(|| format!("no GPU matches {choice}; found: {}", found()))?;
        if renderer.vendor().is_none() {
            return Err(format!("{renderer} has no Droidloom driver; AMD (amdgpu), Intel (i915, xe), NVIDIA (nvidia) and Qualcomm Adreno (msm) are supported"));
        }
        Ok(if renderer.native() {
            Self { renderer: renderer.clone(), render_node: renderer.render_node.clone(), backend: GraphicsBackend::Drm }
        } else {
            let node = supported().find(|g| g.native()).unwrap_or(renderer);
            Self { renderer: renderer.clone(), render_node: node.render_node.clone(), backend: GraphicsBackend::Forwarding }
        })
    }

    /// The forwarded GPU's PCI address, for `droidloom-gpu-server`.
    pub fn forwarded(&self) -> Option<&str> {
        (self.backend == GraphicsBackend::Forwarding).then_some(self.renderer.device.as_str())
    }
}

impl Display for Graphics {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.backend {
            GraphicsBackend::Forwarding => write!(
                f,
                "Android renders on the {} through droidloom-gpu-server (GPU forwarding); the cell's render node is {}",
                self.renderer, self.render_node
            ),
            _ => write!(f, "Android renders natively on the {} (Mesa); GPU forwarding is off", self.renderer),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gpu(node: u32, pci: &str, driver: &str) -> HostGpu {
        HostGpu { render_node: format!("/dev/dri/renderD{node}"), device: pci.into(), driver: driver.into() }
    }

    #[test]
    fn sysfs_names_gpus_by_pci_address() {
        let sys = tempfile::tempdir().unwrap();
        let root = sys.path();
        let node = |name: &str, pci: &str, driver: &str| {
            std::fs::create_dir_all(root.join("drivers").join(driver)).unwrap();
            std::fs::create_dir_all(root.join("pci").join(pci)).unwrap();
            std::os::unix::fs::symlink(root.join("drivers").join(driver), root.join("pci").join(pci).join("driver")).unwrap();
            std::fs::create_dir_all(root.join("drm").join(name)).unwrap();
            std::os::unix::fs::symlink(root.join("pci").join(pci), root.join("drm").join(name).join("device")).unwrap();
        };
        std::fs::create_dir_all(root.join("drm/card0")).unwrap();
        node("renderD129", "0000:0a:00.0", "amdgpu");
        node("renderD128", "0000:04:00.0", "nvidia");
        std::fs::create_dir_all(root.join("drm/renderD130/device")).unwrap();
        node("renderD131", "ae01000.display-controller", "msm_dpu");
        node("renderD132", "1c00000.gpu", "vc4");
        assert_eq!(
            HostGpu::list(&root.join("drm")).unwrap(),
            [
                gpu(128, "0000:04:00.0", "nvidia"),
                gpu(129, "0000:0a:00.0", "amdgpu"),
                gpu(131, "ae01000.display-controller", "msm_dpu")
            ]
        );
    }

    #[test]
    fn an_adreno_renders_natively() {
        let phone = [gpu(128, "ae01000.display-controller", "msm_dpu")];
        let auto = Graphics::resolve(&GpuChoice::Auto, &phone).unwrap();
        assert_eq!((auto.render_node.as_str(), auto.backend, auto.forwarded()), ("/dev/dri/renderD128", GraphicsBackend::Drm, None));
        assert_eq!(Graphics::resolve(&"qualcomm".parse().unwrap(), &phone).unwrap(), auto);
    }

    #[test]
    fn nvidia_forwards_and_keeps_a_native_render_node() {
        let both = [gpu(128, "0000:04:00.0", "nvidia"), gpu(129, "0000:0a:00.0", "amdgpu")];
        let auto = Graphics::resolve(&GpuChoice::Auto, &both).unwrap();
        assert_eq!((auto.render_node.as_str(), auto.backend), ("/dev/dri/renderD129", GraphicsBackend::Forwarding));
        assert_eq!(auto.forwarded(), Some("0000:04:00.0"));
        let amd = Graphics::resolve(&"amd".parse().unwrap(), &both).unwrap();
        assert_eq!((amd.render_node.as_str(), amd.backend, amd.forwarded()), ("/dev/dri/renderD129", GraphicsBackend::Drm, None));
        let by_address = Graphics::resolve(&"04:00.0".parse().unwrap(), &both).unwrap();
        assert_eq!(by_address, auto);
        let alone = Graphics::resolve(&GpuChoice::Auto, &both[..1]).unwrap();
        assert_eq!((alone.render_node.as_str(), alone.backend), ("/dev/dri/renderD128", GraphicsBackend::Forwarding));
    }

    #[test]
    fn missing_or_unsupported_gpus_are_named() {
        let host = [gpu(128, "0000:01:00.0", "nouveau"), gpu(129, "0000:00:02.0", "i915")];
        assert_eq!(Graphics::resolve(&GpuChoice::Auto, &host).unwrap().renderer, host[1]);
        let nouveau = Graphics::resolve(&"0000:01:00.0".parse().unwrap(), &host).unwrap_err();
        assert!(nouveau.contains("unsupported GPU 0000:01:00.0 (nouveau"), "{nouveau}");
        let none = Graphics::resolve(&"nvidia".parse().unwrap(), &host).unwrap_err();
        assert!(none.starts_with("no GPU matches nvidia; found: unsupported GPU 0000:01:00.0"), "{none}");
        assert!(Graphics::resolve(&GpuChoice::Auto, &[]).unwrap_err().ends_with("found: none"));
    }

    #[test]
    fn choices_round_trip_and_reject_anything_else() {
        for text in ["auto", "amd", "intel", "nvidia", "qualcomm", "0000:04:00.0"] {
            assert_eq!(text.parse::<GpuChoice>().unwrap().to_string(), text);
        }
        assert_eq!("NVIDIA".parse::<GpuChoice>().unwrap(), GpuChoice::Vendor(Vendor::Nvidia));
        for text in ["", "geforce", "0000:04:00", "0000:04:00.0\n", "/dev/dri/renderD128"] {
            assert!(text.parse::<GpuChoice>().is_err(), "{text:?}");
        }
    }
}
