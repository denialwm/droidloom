//! Read-only host capability discovery for Droidloom.
//!
//! Probes are intentionally implemented with filesystem metadata and file
//! reads. This crate never mounts, creates a namespace or device, loads a
//! module, changes a cgroup, or opens a DRM node.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{self, Read};
use std::os::unix::fs::FileTypeExt as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use droidloom_contracts::{ImageManifest, TARGET_ARCHITECTURE};
use flate2::read::GzDecoder;
use serde::{Deserialize, Serialize};

/// Machine-readable report schema emitted by this crate.
pub const REPORT_SCHEMA_VERSION: u32 = 1;
const MAX_PROBE_FILE_BYTES: u64 = 8 * 1024 * 1024;

/// Probe result status.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    /// The capability was positively observed.
    Pass,
    /// The capability is usable but a hardening or future-milestone concern exists.
    Warning,
    /// A required capability was proven absent.
    Fail,
    /// Available read-only evidence could not prove presence or absence.
    Unknown,
}

/// One independently actionable capability result.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Check {
    /// Stable machine identifier.
    pub id: String,
    /// Short human-facing name.
    pub title: String,
    /// Observed status.
    pub status: CheckStatus,
    /// Whether this result prevents the next boot/graphics experiments.
    pub blocking: bool,
    /// Concrete observations behind the result.
    pub evidence: Vec<String>,
    /// Actionable follow-up when the check is not a pass.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remediation: Option<String>,
}

/// Basic immutable host identity included in a report.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostInfo {
    /// Kernel operating system name.
    pub os: String,
    /// Kernel release string.
    pub kernel_release: String,
    /// Architecture being inspected.
    pub architecture: String,
    /// User running the probe.
    pub user: String,
    /// Root prefix used for all probe reads.
    pub probe_root: PathBuf,
}

/// Complete deterministic capability report, apart from its generation time.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Report {
    /// Report document schema.
    pub schema_version: u32,
    /// Seconds since the Unix epoch when discovery began.
    pub generated_at_unix_seconds: u64,
    /// Host identity.
    pub host: HostInfo,
    /// True only when no blocking result is failed or unknown.
    pub ready: bool,
    /// Number of blocking failures/unknowns.
    pub blocker_count: usize,
    /// Ordered capability results.
    pub checks: Vec<Check>,
}

impl Report {
    /// Render a stable, terminal-friendly report without ANSI control codes.
    pub fn render_human(&self) -> String {
        let mut output = format!(
            "Droidloom host capability report\nHost: {} {} ({})\nUser: {}\nResult: {} ({} blocker{})\n",
            self.host.os,
            self.host.kernel_release,
            self.host.architecture,
            self.host.user,
            if self.ready { "READY" } else { "BLOCKED" },
            self.blocker_count,
            if self.blocker_count == 1 { "" } else { "s" }
        );

        for check in &self.checks {
            let label = match check.status {
                CheckStatus::Pass => "PASS",
                CheckStatus::Warning => "WARN",
                CheckStatus::Fail => "FAIL",
                CheckStatus::Unknown => "UNKNOWN",
            };
            let blocker = if check.blocking { " [required]" } else { "" };
            let _ = writeln!(output, "\n[{label}] {}{blocker}", check.title);
            for evidence in &check.evidence {
                let _ = writeln!(output, "  - {evidence}");
            }
            if let Some(remediation) = &check.remediation {
                let _ = writeln!(output, "  Remedy: {remediation}");
            }
        }
        output
    }
}

/// Inputs controlling a probe. The alternate root is intended for tests and
/// offline root filesystem inspection.
#[derive(Clone, Debug)]
pub struct ProbeOptions {
    /// Prefix corresponding to `/` on the inspected host.
    pub root: PathBuf,
    /// Optional architecture override for offline inspection.
    pub architecture: Option<String>,
    /// Optional Droidloom image manifest to validate with host checks.
    pub image_manifest: Option<PathBuf>,
}

impl Default for ProbeOptions {
    fn default() -> Self {
        Self {
            root: PathBuf::from("/"),
            architecture: None,
            image_manifest: None,
        }
    }
}

/// Execute all read-only probes.
pub fn probe(options: &ProbeOptions) -> Report {
    let view = HostView::new(options.root.clone());
    let os = view
        .read_trimmed("/proc/sys/kernel/ostype")
        .unwrap_or_else(|| "unknown".to_owned());
    let release = view
        .read_trimmed("/proc/sys/kernel/osrelease")
        .unwrap_or_else(|| "unknown".to_owned());
    let architecture = options
        .architecture
        .clone()
        .unwrap_or_else(|| std::env::consts::ARCH.to_owned());
    let user = resolve_user(&view);
    let kernel = KernelConfig::discover(&view, &release);

    let mut checks = vec![
        check_linux(&os),
        check_architecture(&architecture),
        check_namespaces(&view),
        check_binderfs(&view, &kernel),
    ];
    checks.extend(check_cgroups(&view));
    checks.push(check_seccomp(&view, &kernel));
    checks.push(check_network_isolation(&view, &kernel));
    checks.extend(check_graphics(&view, &kernel));
    checks.push(check_mount_filesystems(&view));
    checks.push(check_lsm(&view));
    if let Some(path) = &options.image_manifest {
        checks.push(check_image_manifest(path));
    }

    let blocker_count = checks
        .iter()
        .filter(|check| {
            check.blocking && matches!(check.status, CheckStatus::Fail | CheckStatus::Unknown)
        })
        .count();

    Report {
        schema_version: REPORT_SCHEMA_VERSION,
        generated_at_unix_seconds: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_secs()),
        host: HostInfo {
            os,
            kernel_release: release,
            architecture,
            user,
            probe_root: options.root.clone(),
        },
        ready: blocker_count == 0,
        blocker_count,
        checks,
    }
}

#[derive(Clone, Debug)]
struct HostView {
    root: PathBuf,
}

impl HostView {
    fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn path(&self, absolute: &str) -> PathBuf {
        self.root.join(absolute.trim_start_matches('/'))
    }

    fn exists(&self, absolute: &str) -> bool {
        fs::symlink_metadata(self.path(absolute)).is_ok()
    }

    fn read(&self, absolute: &str) -> Option<String> {
        read_bounded(&self.path(absolute), MAX_PROBE_FILE_BYTES).ok()
    }

    fn read_trimmed(&self, absolute: &str) -> Option<String> {
        self.read(absolute)
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
    }

    fn is_real_root(&self) -> bool {
        self.root == Path::new("/")
    }
}

fn resolve_user(view: &HostView) -> String {
    let uid = view.read("/proc/self/status").and_then(|status| {
        parse_status_value(&status, "Uid")?
            .split_whitespace()
            .next()?
            .parse::<u64>()
            .ok()
    });
    uid.and_then(|wanted| {
        view.read("/etc/passwd")?.lines().find_map(|line| {
            let mut fields = line.split(':');
            let name = fields.next()?;
            let _password = fields.next()?;
            let candidate = fields.next()?.parse::<u64>().ok()?;
            (candidate == wanted).then(|| name.to_owned())
        })
    })
    .unwrap_or_else(|| "unknown".to_owned())
}

fn parse_status_value<'a>(status: &'a str, key: &str) -> Option<&'a str> {
    status.lines().find_map(|line| {
        let (candidate, value) = line.split_once(':')?;
        (candidate == key).then(|| value.trim())
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ConfigValue {
    BuiltIn,
    Module,
    Disabled,
    Unknown,
}

#[derive(Debug, Default)]
struct KernelConfig {
    values: BTreeMap<String, String>,
    source: Option<String>,
}

impl KernelConfig {
    fn discover(view: &HostView, release: &str) -> Self {
        let candidates = [format!("/boot/config-{release}"), "/proc/config".to_owned()];
        for candidate in candidates {
            if let Some(contents) = view.read(&candidate) {
                return Self::parse(&contents, candidate);
            }
        }

        let compressed = view.path("/proc/config.gz");
        if let Ok(file) = File::open(&compressed) {
            let mut contents = String::new();
            if GzDecoder::new(file)
                .take(MAX_PROBE_FILE_BYTES.saturating_add(1))
                .read_to_string(&mut contents)
                .is_ok()
                && u64::try_from(contents.len()).unwrap_or(u64::MAX) <= MAX_PROBE_FILE_BYTES
            {
                return Self::parse(&contents, "/proc/config.gz".to_owned());
            }
        }
        Self::default()
    }

    fn parse(contents: &str, source: String) -> Self {
        let mut values = BTreeMap::new();
        for line in contents.lines() {
            if let Some((name, value)) = line.split_once('=') {
                if name.starts_with("CONFIG_") {
                    values.insert(name.to_owned(), value.trim().to_owned());
                }
            } else if let Some(name) = line
                .strip_prefix("# ")
                .and_then(|line| line.strip_suffix(" is not set"))
            {
                values.insert(name.to_owned(), "n".to_owned());
            }
        }
        Self {
            values,
            source: Some(source),
        }
    }

    fn get(&self, name: &str) -> ConfigValue {
        match self.values.get(name).map(String::as_str) {
            Some("m") => ConfigValue::Module,
            Some("n") => ConfigValue::Disabled,
            Some(_) => ConfigValue::BuiltIn,
            None => ConfigValue::Unknown,
        }
    }

    fn evidence(&self, name: &str) -> String {
        let value = match self.get(name) {
            ConfigValue::BuiltIn => "enabled",
            ConfigValue::Module => "module",
            ConfigValue::Disabled => "disabled",
            ConfigValue::Unknown => "not observable",
        };
        match &self.source {
            Some(source) => format!("{name} is {value} in {source}"),
            None => format!("{name} is {value}; no readable kernel config was found"),
        }
    }
}

fn check_linux(os: &str) -> Check {
    let pass = os == "Linux";
    check(
        "host.linux",
        "Linux shared-kernel host",
        if pass {
            CheckStatus::Pass
        } else {
            CheckStatus::Fail
        },
        true,
        vec![format!("kernel reports {os:?}")],
        (!pass)
            .then(|| "Run Droidloom on Linux; no guest-kernel fallback is implemented.".to_owned()),
    )
}

fn check_architecture(architecture: &str) -> Check {
    let normalized = match architecture {
        "arm64" => "aarch64",
        other => other,
    };
    let pass = normalized == TARGET_ARCHITECTURE.as_str();
    check(
        "cpu.architecture",
        "Architecture-matched native execution",
        if pass {
            CheckStatus::Pass
        } else {
            CheckStatus::Fail
        },
        true,
        vec![format!(
            "host is {architecture}; native image contract is {}",
            TARGET_ARCHITECTURE.as_str()
        )],
        (!pass).then(|| {
            format!(
                "Use a {} host and image pair. Droidloom does not provide CPU instruction translation.",
                TARGET_ARCHITECTURE.as_str()
            )
        }),
    )
}

fn check_namespaces(view: &HostView) -> Check {
    let required = ["user", "pid", "mnt", "ipc", "uts", "net", "cgroup"];
    let missing: Vec<_> = required
        .iter()
        .filter(|namespace| !view.exists(&format!("/proc/self/ns/{namespace}")))
        .copied()
        .collect();
    let pass = missing.is_empty();
    let evidence = if pass {
        vec![format!(
            "observed namespace handles: {}",
            required.join(", ")
        )]
    } else {
        vec![format!("missing namespace handles: {}", missing.join(", "))]
    };
    check(
        "kernel.namespaces",
        "Required Linux namespaces",
        if pass {
            CheckStatus::Pass
        } else {
            CheckStatus::Fail
        },
        true,
        evidence,
        (!pass).then(|| {
            "Enable user, PID, mount, IPC, UTS, network, and cgroup namespaces.".to_owned()
        }),
    )
}

fn check_binderfs(view: &HostView, kernel: &KernelConfig) -> Check {
    let listed = filesystem_is_listed(view, "binder");
    let configured = matches!(
        kernel.get("CONFIG_ANDROID_BINDERFS"),
        ConfigValue::BuiltIn | ConfigValue::Module
    );
    let pass = listed || configured;
    let mut evidence = vec![kernel.evidence("CONFIG_ANDROID_BINDERFS")];
    evidence.push(format!(
        "binderfs is {} in /proc/filesystems",
        if listed { "listed" } else { "not listed" }
    ));
    check(
        "kernel.binderfs",
        "Private Binder filesystem",
        if pass { CheckStatus::Pass } else { CheckStatus::Fail },
        true,
        evidence,
        (!pass).then(|| {
            "Enable CONFIG_ANDROID_BINDER_IPC and CONFIG_ANDROID_BINDERFS; the runtime will create private binder, hwbinder, and vndbinder devices."
                .to_owned()
        }),
    )
}

fn check_cgroups(view: &HostView) -> Vec<Check> {
    let controllers = view.read_trimmed("/sys/fs/cgroup/cgroup.controllers");
    let required = ["cpu", "cpuset", "memory", "pids"];
    let present: Vec<&str> = controllers
        .as_deref()
        .map_or_else(Vec::new, |line| line.split_whitespace().collect());
    let missing: Vec<_> = required
        .iter()
        .filter(|controller| !present.contains(controller))
        .copied()
        .collect();
    let v2_pass = controllers.is_some() && missing.is_empty();
    let v2 = check(
        "kernel.cgroup_v2",
        "Delegated cgroup v2 resource control",
        if v2_pass { CheckStatus::Pass } else { CheckStatus::Fail },
        true,
        vec![match controllers {
            Some(line) => format!("available controllers: {line}"),
            None => "cgroup.controllers is absent; cgroup v2 was not observed".to_owned(),
        }, format!(
            "required controllers missing: {}",
            if missing.is_empty() { "none".to_owned() } else { missing.join(", ") }
        )],
        (!v2_pass).then(|| {
            "Mount cgroup v2 and enable cpu, cpuset, memory, and pids controllers for the Droidloom service subtree."
                .to_owned()
        }),
    );

    let freezer_path = find_direct_child_file(view, "/sys/fs/cgroup", "cgroup.freeze");
    let freezer_pass = freezer_path.is_some();
    let freezer = check(
        "kernel.cgroup_freezer",
        "Cgroup v2 freezer",
        if freezer_pass { CheckStatus::Pass } else { CheckStatus::Warning },
        false,
        vec![freezer_path.map_or_else(
            || "no readable cgroup.freeze file was observed in an existing child cgroup".to_owned(),
            |path| format!("observed {}", path.display()),
        )],
        (!freezer_pass).then(|| {
            "Recheck cgroup.freeze when a delegated Droidloom cgroup is created in Milestone 1; safe whole-cell freezing is a Milestone 4 feature."
                .to_owned()
        }),
    );
    vec![v2, freezer]
}

fn check_seccomp(view: &HostView, kernel: &KernelConfig) -> Check {
    let actions = view.read_trimmed("/proc/sys/kernel/seccomp/actions_avail");
    let config = kernel.get("CONFIG_SECCOMP_FILTER");
    let pass = actions.is_some() || matches!(config, ConfigValue::BuiltIn | ConfigValue::Module);
    let mut evidence = vec![kernel.evidence("CONFIG_SECCOMP_FILTER")];
    evidence.push(actions.map_or_else(
        || "seccomp actions are not exposed".to_owned(),
        |value| format!("available actions: {value}"),
    ));
    check(
        "kernel.seccomp",
        "Seccomp filtering",
        if pass {
            CheckStatus::Pass
        } else {
            CheckStatus::Fail
        },
        true,
        evidence,
        (!pass).then(|| "Enable CONFIG_SECCOMP and CONFIG_SECCOMP_FILTER.".to_owned()),
    )
}

fn check_network_isolation(view: &HostView, kernel: &KernelConfig) -> Check {
    let veth = kernel_or_module(view, kernel, "CONFIG_VETH", "veth");
    let nft = kernel_or_module(view, kernel, "CONFIG_NF_TABLES", "nf_tables");
    let pass = veth && nft;
    check(
        "kernel.network_isolation",
        "Private veth and nftables networking",
        if pass { CheckStatus::Pass } else { CheckStatus::Fail },
        true,
        vec![
            kernel.evidence("CONFIG_VETH"),
            kernel.evidence("CONFIG_NF_TABLES"),
            format!("veth runtime/module evidence: {veth}"),
            format!("nf_tables runtime/module evidence: {nft}"),
        ],
        (!pass).then(|| {
            "Provide veth and nftables support. Sharing Android netd with the host network namespace is forbidden."
                .to_owned()
        }),
    )
}

fn check_graphics(view: &HostView, kernel: &KernelConfig) -> Vec<Check> {
    let render_nodes = render_nodes(view);
    let render_pass = !render_nodes.is_empty();
    let render = check(
        "graphics.render_node",
        "DRM render-only GPU access",
        if render_pass {
            CheckStatus::Pass
        } else {
            CheckStatus::Fail
        },
        true,
        if render_pass {
            render_nodes
        } else {
            vec!["no /dev/dri/renderD* node was observed".to_owned()]
        },
        (!render_pass).then(|| {
            "Expose the selected GPU render node. Do not grant Android a DRM card/KMS node."
                .to_owned()
        }),
    );

    let dma_shared = matches!(
        kernel.get("CONFIG_DMA_SHARED_BUFFER"),
        ConfigValue::BuiltIn | ConfigValue::Module
    ) || view.exists("/sys/kernel/dmabuf");
    let sync_file = matches!(
        kernel.get("CONFIG_SYNC_FILE"),
        ConfigValue::BuiltIn | ConfigValue::Module
    ) || view.exists("/sys/kernel/debug/sync");
    let primitives_pass = dma_shared && sync_file;
    let primitives = check(
        "graphics.buffer_sync",
        "DMA-BUF and sync_file primitives",
        if primitives_pass { CheckStatus::Pass } else { CheckStatus::Fail },
        true,
        vec![
            kernel.evidence("CONFIG_DMA_SHARED_BUFFER"),
            kernel.evidence("CONFIG_SYNC_FILE"),
            format!("DMA-BUF runtime evidence: {dma_shared}"),
            format!("sync_file runtime/config evidence: {sync_file}"),
        ],
        (!primitives_pass).then(|| {
            "Enable DMA_SHARED_BUFFER and SYNC_FILE; CPU framebuffer copies are not an accepted graphics path."
                .to_owned()
        }),
    );

    let heaps = matches!(
        kernel.get("CONFIG_DMABUF_HEAPS"),
        ConfigValue::BuiltIn | ConfigValue::Module
    ) || view.exists("/dev/dma_heap");
    let heap_check = check(
        "graphics.dmabuf_heaps",
        "DMA-BUF heaps",
        if heaps { CheckStatus::Pass } else { CheckStatus::Warning },
        false,
        vec![
            kernel.evidence("CONFIG_DMABUF_HEAPS"),
            format!("/dev/dma_heap exists: {}", view.exists("/dev/dma_heap")),
        ],
        (!heaps).then(|| {
            "Confirm that GBM allocations alone satisfy the selected allocator, or enable DMA-BUF heaps before image integration."
                .to_owned()
        }),
    );
    vec![render, primitives, heap_check]
}

fn render_nodes(view: &HostView) -> Vec<String> {
    let directory = view.path("/dev/dri");
    let Ok(entries) = fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut nodes = Vec::new();
    for entry in entries.flatten().take(128) {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.strip_prefix("renderD").is_some_and(|suffix| {
            !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
        }) {
            continue;
        }
        let valid_type = !view.is_real_root()
            || entry
                .metadata()
                .is_ok_and(|metadata| metadata.file_type().is_char_device());
        if !valid_type {
            continue;
        }
        let driver = graphics_driver(view, &name);
        nodes.push(format!(
            "/dev/dri/{name}{}",
            driver.map_or_else(String::new, |driver| format!(" (driver {driver})"))
        ));
    }
    nodes.sort();
    nodes
}

fn graphics_driver(view: &HostView, node: &str) -> Option<String> {
    let link = view.path(&format!("/sys/class/drm/{node}/device/driver"));
    fs::read_link(link)
        .ok()
        .and_then(|target| {
            target
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .or_else(|| {
            view.read(&format!("/sys/class/drm/{node}/device/uevent"))?
                .lines()
                .find_map(|line| line.strip_prefix("DRIVER=").map(str::to_owned))
        })
}

fn check_mount_filesystems(view: &HostView) -> Check {
    let required = ["proc", "sysfs", "tmpfs"];
    let missing: Vec<_> = required
        .iter()
        .filter(|filesystem| !filesystem_is_listed(view, filesystem))
        .copied()
        .collect();
    let pass = missing.is_empty();
    check(
        "kernel.mount_filesystems",
        "Synthetic cell filesystem support",
        if pass {
            CheckStatus::Pass
        } else {
            CheckStatus::Fail
        },
        true,
        vec![format!(
            "required filesystems missing: {}",
            if missing.is_empty() {
                "none".to_owned()
            } else {
                missing.join(", ")
            }
        )],
        (!pass).then(|| "Enable procfs, sysfs, and tmpfs support.".to_owned()),
    )
}

fn check_lsm(view: &HostView) -> Check {
    let lsm = view
        .read_trimmed("/sys/kernel/security/lsm")
        .or_else(|| view.read_trimmed("/proc/sys/kernel/lsm"));
    let hardened = lsm.as_deref().is_some_and(|value| {
        ["apparmor", "selinux", "landlock", "bpf"]
            .iter()
            .any(|candidate| value.split(',').any(|active| active == *candidate))
    });
    check(
        "security.host_lsm",
        "Host LSM hardening",
        if hardened { CheckStatus::Pass } else { CheckStatus::Warning },
        false,
        vec![lsm.map_or_else(
            || "active LSM list is not readable".to_owned(),
            |value| format!("active LSMs: {value}"),
        )],
        (!hardened).then(|| {
            "Treat compromise of the full Android cell as outside the primary boundary until a host LSM policy or hardened VM fallback is available."
                .to_owned()
        }),
    )
}

fn check_image_manifest(path: &Path) -> Check {
    match ImageManifest::load(path) {
        Ok(manifest) => check(
            "image.manifest",
            "Android image compatibility manifest",
            CheckStatus::Pass,
            true,
            vec![
                format!(
                    "validated {} version {}",
                    manifest.image_id, manifest.image_version
                ),
                format!("AOSP tag {} at {}", manifest.aosp.tag, manifest.aosp.commit),
                format!("Mesa {}", manifest.mesa.version),
            ],
            None,
        ),
        Err(error) => check(
            "image.manifest",
            "Android image compatibility manifest",
            CheckStatus::Fail,
            true,
            vec![error.to_string()],
            Some("Install an image whose signed manifest satisfies image-manifest-v1.".to_owned()),
        ),
    }
}

fn filesystem_is_listed(view: &HostView, name: &str) -> bool {
    view.read("/proc/filesystems").is_some_and(|contents| {
        contents
            .lines()
            .filter_map(|line| line.split_whitespace().last())
            .any(|candidate| candidate == name)
    })
}

fn kernel_or_module(view: &HostView, kernel: &KernelConfig, flag: &str, module: &str) -> bool {
    if matches!(kernel.get(flag), ConfigValue::BuiltIn | ConfigValue::Module)
        || view.exists(&format!("/sys/module/{module}"))
    {
        return true;
    }
    let release = view.read_trimmed("/proc/sys/kernel/osrelease");
    release.is_some_and(|release| {
        ["modules.builtin", "modules.dep"].iter().any(|index| {
            view.read(&format!("/lib/modules/{release}/{index}"))
                .is_some_and(|contents| {
                    contents.lines().any(|line| {
                        line.contains(&format!("/{module}.ko"))
                            || line.contains(&format!("/{module}.ko:"))
                    })
                })
        })
    })
}

fn find_direct_child_file(view: &HostView, parent: &str, filename: &str) -> Option<PathBuf> {
    let parent_path = view.path(parent);
    let entries = fs::read_dir(parent_path).ok()?;
    for entry in entries.flatten().take(256) {
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            let candidate = entry.path().join(filename);
            if candidate.exists() {
                return candidate
                    .strip_prefix(&view.root)
                    .ok()
                    .map(|relative| Path::new("/").join(relative));
            }
        }
    }
    None
}

fn check(
    id: &str,
    title: &str,
    status: CheckStatus,
    blocking: bool,
    evidence: Vec<String>,
    remediation: Option<String>,
) -> Check {
    Check {
        id: id.to_owned(),
        title: title.to_owned(),
        status,
        blocking,
        evidence,
        remediation,
    }
}

/// Serialize a report as compact or pretty JSON.
///
/// # Errors
///
/// Returns a Serde error if a future report field cannot be represented as
/// JSON.
pub fn report_json(report: &Report, pretty: bool) -> Result<String, serde_json::Error> {
    if pretty {
        serde_json::to_string_pretty(report)
    } else {
        serde_json::to_string(report)
    }
}

/// Read a UTF-8 file through the same bounded interface used by callers.
/// Exposed only to make the probe's failure mode easy to test.
///
/// # Errors
///
/// Returns an I/O error when the file cannot be read, exceeds `maximum`, or is
/// not valid UTF-8.
pub fn read_bounded(path: &Path, maximum: u64) -> io::Result<String> {
    let file = File::open(path)?;
    let mut bytes = Vec::new();
    file.take(maximum.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > maximum {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "file exceeds probe limit",
        ));
    }
    String::from_utf8(bytes).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use tempfile::TempDir;

    use super::*;

    fn put(root: &Path, path: &str, contents: &str) {
        let target = root.join(path.trim_start_matches('/'));
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(target, contents).unwrap();
    }

    fn ready_fixture() -> TempDir {
        let root = tempfile::tempdir().unwrap();
        put(root.path(), "/proc/sys/kernel/ostype", "Linux\n");
        put(root.path(), "/proc/sys/kernel/osrelease", "7.0-test\n");
        put(
            root.path(),
            "/proc/sys/kernel/seccomp/actions_avail",
            "kill_process kill_thread trap errno user_notif trace log allow\n",
        );
        put(
            root.path(),
            "/proc/self/status",
            "Uid:\t1000 1000 1000 1000\n",
        );
        for namespace in ["user", "pid", "mnt", "ipc", "uts", "net", "cgroup"] {
            put(
                root.path(),
                &format!("/proc/self/ns/{namespace}"),
                "fixture",
            );
        }
        put(
            root.path(),
            "/proc/filesystems",
            "nodev\tproc\nnodev\tsysfs\nnodev\ttmpfs\nnodev\tbinder\n",
        );
        put(
            root.path(),
            "/proc/config",
            "CONFIG_ANDROID_BINDERFS=y\nCONFIG_SECCOMP_FILTER=y\nCONFIG_VETH=y\nCONFIG_NF_TABLES=y\nCONFIG_DMA_SHARED_BUFFER=y\nCONFIG_SYNC_FILE=y\nCONFIG_DMABUF_HEAPS=y\n",
        );
        put(
            root.path(),
            "/etc/passwd",
            "tester:x:1000:1000::/home/tester:/bin/sh\n",
        );
        put(
            root.path(),
            "/sys/fs/cgroup/cgroup.controllers",
            "cpu cpuset io memory pids\n",
        );
        put(
            root.path(),
            "/sys/fs/cgroup/user.slice/cgroup.freeze",
            "0\n",
        );
        put(root.path(), "/dev/dri/renderD128", "fixture node\n");
        put(root.path(), "/sys/kernel/dmabuf/buffers/.keep", "");
        put(root.path(), "/dev/dma_heap/.keep", "");
        put(
            root.path(),
            "/sys/kernel/security/lsm",
            "lockdown,capability,landlock,bpf\n",
        );
        root
    }

    #[test]
    fn complete_fixture_is_ready() {
        let root = ready_fixture();
        let report = probe(&ProbeOptions {
            root: root.path().to_path_buf(),
            architecture: Some(TARGET_ARCHITECTURE.as_str().to_owned()),
            ..ProbeOptions::default()
        });
        assert!(report.ready, "{}", report.render_human());
        assert_eq!(report.blocker_count, 0);
    }

    #[test]
    fn missing_binder_and_wrong_architecture_are_blockers() {
        let root = ready_fixture();
        put(
            root.path(),
            "/proc/filesystems",
            "nodev\tproc\nnodev\tsysfs\nnodev\ttmpfs\n",
        );
        put(root.path(), "/proc/config", "CONFIG_ANDROID_BINDERFS=n\n");
        let report = probe(&ProbeOptions {
            root: root.path().to_path_buf(),
            architecture: Some(
                if TARGET_ARCHITECTURE.as_str() == "x86_64" {
                    "aarch64"
                } else {
                    "x86_64"
                }
                .to_owned(),
            ),
            ..ProbeOptions::default()
        });
        assert!(!report.ready);
        assert!(report.blocker_count >= 2);
        assert!(
            report.checks.iter().any(|check| {
                check.id == "kernel.binderfs" && check.status == CheckStatus::Fail
            })
        );
    }

    #[test]
    fn bounded_read_rejects_large_input() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("large");
        fs::write(&path, b"12345").unwrap();
        assert_eq!(
            read_bounded(&path, 4).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }
}
