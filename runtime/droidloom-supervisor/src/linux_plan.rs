//! Auditable Linux operation plan for one Android cell.
//!
//! The plan is deliberately data, not a shell script. A future privileged
//! executor can map each closed operation variant to syscalls while preserving
//! the already-tested supervisor transaction and reverse ownership ordering.

use std::convert::Infallible;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::{CONSTRUCTION_ORDER, CellBackend, CellId, CellSpec, LifecycleStep, SpecError};

const CELL_ROOT: &str = "root";
const CGROUP_ROOT: &str = "/sys/fs/cgroup/droidloom";

/// Complete construction and reverse teardown plan.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LinuxCellPlan {
    /// Plan schema revision.
    pub schema_version: u32,
    /// Stable cell identity.
    pub cell: CellId,
    /// Construction operations in transaction order.
    pub construction: Vec<PlannedStep>,
    /// Teardown operations in reverse ownership order.
    pub teardown: Vec<PlannedStep>,
    /// Security properties proven structurally by plan construction.
    pub invariants: PlanInvariants,
}

/// Operations owned by one lifecycle stage.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PlannedStep {
    /// Supervisor transaction stage.
    pub step: LifecycleStep,
    /// Closed set of operations for this stage.
    pub operations: Vec<LinuxOperation>,
}

/// Namespace types assigned to the Android cell.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NamespaceKind {
    /// Dedicated UID/GID mapping.
    User,
    /// Android init becomes cell PID 1.
    Pid,
    /// Private mount tree.
    Mount,
    /// Private System V IPC/POSIX message namespace.
    Ipc,
    /// Private hostname/domain name.
    Uts,
    /// Private routes, firewall view, and sockets.
    Network,
    /// Private cgroup namespace rooted at the delegated subtree.
    Cgroup,
}

/// UID or GID mapping kind.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IdMapKind {
    /// User ID mapping.
    Uid,
    /// Group ID mapping.
    Gid,
}

/// Mount purpose and immutable flag contract.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MountKind {
    /// Android image whose EROFS or raw ext4 format is detected before mounting.
    ReadOnlyAndroidImage,
    /// Read-only EROFS Android partition image.
    ErofsImage,
    /// Read-only raw ext4 Android partition image.
    Ext4Image,
    /// Direct host directory containing Android-controlled state.
    HostStorage,
    /// Private tmpfs.
    Tmpfs,
    /// Private procfs.
    Proc,
    /// Minimal synthetic sysfs view.
    SyntheticSys,
    /// Private devpts instance.
    DevPts,
    /// Private binderfs instance.
    BinderFs,
    /// Single host path bind-mounted into the cell.
    FileBind,
    /// Package-owned directory bind-mounted below the cell-private runtime tree.
    DirectoryBind,
}

/// One closed, auditable Linux mutation or teardown action.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum LinuxOperation {
    /// Install the inherited filesystem protection before Android can run.
    InstallSharedFilesystemGuard,
    /// Reserve the subordinate identities and runtime path for one cell.
    ReserveIdentity {
        /// Owning host user.
        host_uid: u32,
        /// First subordinate UID.
        uid_start: u32,
        /// First subordinate GID.
        gid_start: u32,
        /// Size of the UID mapping.
        uid_count: u32,
        /// Size of the GID mapping.
        gid_count: u32,
    },
    /// Create a directory with an explicit mode before namespace construction.
    CreateDirectory {
        /// Host path.
        path: PathBuf,
        /// Numeric Unix mode.
        mode: u32,
    },
    /// Spawn the stopped namespace leader and retain pidfd authority.
    CreateNamespaceLeader {
        /// Complete namespace set.
        namespaces: Vec<NamespaceKind>,
        /// Lifecycle uses a pidfd, never name/PID scanning.
        retain_pidfd: bool,
    },
    /// Deny setgroups before writing an unprivileged GID map.
    DenySetgroups,
    /// Write one exact outer ID map.
    WriteIdMap {
        /// UID or GID.
        kind: IdMapKind,
        /// First ID inside the cell.
        inside_start: u32,
        /// First host subordinate ID.
        outside_start: u32,
        /// Number of mapped IDs.
        count: u32,
    },
    /// Make the cell mount tree private so propagation cannot reach the host.
    MakeMountTreePrivate,
    /// Mount or bind one known filesystem object.
    Mount {
        /// Mount role.
        kind: MountKind,
        /// Optional host source.
        source: Option<PathBuf>,
        /// Absolute path inside the cell.
        target: PathBuf,
        /// Read-only after construction.
        read_only: bool,
        /// Reject device nodes from this mount.
        nodev: bool,
        /// Reject setuid/setgid execution.
        nosuid: bool,
        /// Reject executable mappings.
        noexec: bool,
    },
    /// Bind one explicitly configured host directory into Android shared
    /// storage with host ownership translated to Android's storage service.
    MountSharedStorage {
        /// Existing host-owned directory.
        source: PathBuf,
        /// Backing path below `/data/media/0` inside the cell.
        target: PathBuf,
        /// Package whose recorded UID owns files on the idmapped mount.
        android_uid_package: String,
        /// GID visible on the idmapped mount.
        android_gid: u32,
        /// Reserved host GID used to map the storage service's primary GID.
        primary_host_gid: u32,
        /// Owning host user translated by the mount.
        host_uid: u32,
        /// Owning host group translated by the mount.
        host_gid: u32,
    },
    /// Create one harmless character device in the private dev tmpfs.
    CreateBasicDevice {
        /// Cell path.
        path: PathBuf,
        /// Linux character-device major.
        major: u32,
        /// Linux character-device minor.
        minor: u32,
        /// Numeric Unix mode.
        mode: u32,
    },
    /// Recreate a selected graphics character device with a private inode.
    CreateGraphicsDevice {
        /// Validated host device whose major/minor are preserved.
        source: PathBuf,
        /// Cell-private node path.
        target: PathBuf,
        /// Cell permissions; host inode permissions remain unchanged.
        mode: u32,
    },
    /// Recreate the host's media DMA heap with a private inode when the host
    /// has one. Without it the cell starts and media decoding fails.
    CreateOptionalMediaDevice {
        /// Host device whose major/minor are preserved when it validates.
        source: PathBuf,
        /// Cell-private node path.
        target: PathBuf,
        /// Cell permissions; host inode permissions remain unchanged.
        mode: u32,
    },
    /// Add one private Binder context through binder-control.
    AddBinderDevice {
        /// Exact permitted Binder name.
        name: String,
        /// Cell-visible compatibility path.
        compatibility_path: PathBuf,
    },
    /// Allocate a host-owned network lease under a stable tag.
    AllocateNetworkLease {
        /// Stable teardown/firewall tag.
        tag: String,
    },
    /// Create the only veth pair crossing the network namespace.
    CreateVeth {
        /// Host-side interface.
        host_name: String,
        /// Cell-side interface.
        cell_name: String,
        /// Stable policy tag.
        tag: String,
    },
    /// Install anti-spoofing, forwarding, DNS, and NAT rules by stable tag.
    InstallNetworkPolicy {
        /// Stable policy tag.
        tag: String,
    },
    /// Create and configure the host-owned parent cgroup.
    ConfigureCgroup {
        /// Host cgroup path.
        path: PathBuf,
        /// Hard process-count limit.
        pids_max: u32,
        /// Hard memory limit in bytes.
        memory_max: u64,
    },
    /// Execute Android init as PID 1 inside the cell.
    StartAndroidInit {
        /// Cell path to init.
        executable: PathBuf,
        /// Fixed argv.
        arguments: Vec<String>,
        /// Whether the Linux no-new-privileges bit is proven compatible with Android init.
        no_new_privileges_before_exec: bool,
    },
    /// Publish the lifecycle endpoint only after init starts.
    PublishHandle {
        /// Host path to the authenticated control socket.
        path: PathBuf,
        /// Owning host user.
        owner_uid: u32,
        /// Numeric Unix mode.
        mode: u32,
    },
    /// Refuse new work and remove the published lifecycle endpoint.
    UnpublishHandle {
        /// Host path.
        path: PathBuf,
    },
    /// Terminate by retained pidfd, then kill the cgroup after a bound.
    StopAndroidInit {
        /// Grace period before cgroup.kill.
        grace_milliseconds: u32,
        /// Host cgroup path used for forced cleanup.
        cgroup: PathBuf,
    },
    /// Remove host-owned cgroup limits and the empty subtree.
    RemoveCgroup {
        /// Host cgroup path.
        path: PathBuf,
    },
    /// Remove all network policy objects selected by their stable tag.
    RemoveNetworkPolicy {
        /// Stable policy tag.
        tag: String,
    },
    /// Delete the tagged veth pair.
    RemoveVeth {
        /// Host-side interface.
        host_name: String,
        /// Stable policy tag.
        tag: String,
    },
    /// Unmount one exact cell path.
    Unmount {
        /// Absolute cell path.
        target: PathBuf,
    },
    /// Terminate and reap the namespace leader through its retained pidfd.
    DestroyNamespaceLeader,
    /// Release subordinate identity ownership and remove the empty runtime path.
    ReleaseIdentity {
        /// Stable cell.
        cell: CellId,
        /// Runtime path.
        runtime_dir: PathBuf,
    },
}

/// Structural security properties of the generated plan.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "evidence keeps each security invariant independently machine-readable"
)]
pub struct PlanInvariants {
    /// Exactly one DRM render node is exposed.
    pub render_nodes: Vec<PathBuf>,
    /// Explicit auxiliary rendering/allocator devices, absent for native DRM.
    pub auxiliary_graphics_devices: Vec<PathBuf>,
    /// Always false by construction.
    pub drm_card_nodes: bool,
    /// Always false by construction.
    pub physical_input_nodes: bool,
    /// Always true by construction.
    pub private_binderfs: bool,
    /// Always true by construction.
    pub private_network_namespace: bool,
    /// Always true by construction.
    pub immutable_android_partitions: bool,
    /// Always true: Android writable state uses the host filesystem.
    pub android_writable_host_filesystems: bool,
    /// Mandatory inherited filter prevents filesystem-wide shutdown, freeze,
    /// trim, quota and superblock reconfiguration by Android.
    pub shared_filesystem_guard: bool,
    /// Always true by construction.
    pub pidfd_lifecycle_authority: bool,
}

/// Recorder implementing the real supervisor backend interface without mutation.
#[derive(Clone, Debug, Default)]
pub struct PlanningBackend {
    construction: Vec<PlannedStep>,
    teardown: Vec<PlannedStep>,
}

impl PlanningBackend {
    /// Borrow recorded construction operations.
    pub fn construction(&self) -> &[PlannedStep] {
        &self.construction
    }

    /// Borrow recorded teardown operations.
    pub fn teardown(&self) -> &[PlannedStep] {
        &self.teardown
    }
}

impl CellBackend for PlanningBackend {
    type Error = Infallible;

    fn apply(&mut self, spec: &CellSpec, step: LifecycleStep) -> Result<(), Self::Error> {
        self.construction.push(PlannedStep {
            step,
            operations: construction_operations(spec, step),
        });
        Ok(())
    }

    fn rollback(&mut self, spec: &CellSpec, step: LifecycleStep) -> Result<(), Self::Error> {
        self.teardown.push(PlannedStep {
            step,
            operations: teardown_operations(spec, step),
        });
        Ok(())
    }
}

/// Build a complete plan by exercising the same transaction used by an
/// executing backend.
///
/// # Errors
///
/// Rejects an invalid cell specification before recording any operation.
pub fn build_linux_plan(spec: &CellSpec) -> Result<LinuxCellPlan, SpecError> {
    spec.validate()?;
    let mut backend = PlanningBackend::default();
    for step in CONSTRUCTION_ORDER {
        match backend.apply(spec, step) {
            Ok(()) => {}
            Err(never) => match never {},
        }
    }
    for step in CONSTRUCTION_ORDER.into_iter().rev() {
        match backend.rollback(spec, step) {
            Ok(()) => {}
            Err(never) => match never {},
        }
    }
    Ok(LinuxCellPlan {
        schema_version: 3,
        cell: spec.id(),
        construction: backend.construction.clone(),
        teardown: backend.teardown.clone(),
        invariants: PlanInvariants {
            render_nodes: vec![spec.render_node.clone()],
            auxiliary_graphics_devices: spec
                .graphics_backend
                .auxiliary_devices()
                .iter()
                .map(PathBuf::from)
                .collect(),
            drm_card_nodes: false,
            physical_input_nodes: false,
            private_binderfs: true,
            private_network_namespace: true,
            immutable_android_partitions: true,
            android_writable_host_filesystems: true,
            shared_filesystem_guard: true,
            pidfd_lifecycle_authority: true,
        },
    })
}

fn construction_operations(spec: &CellSpec, step: LifecycleStep) -> Vec<LinuxOperation> {
    match step {
        LifecycleStep::ReserveIdentity => vec![
            LinuxOperation::ReserveIdentity {
                host_uid: spec.host_uid,
                uid_start: spec.subordinate_uids.start,
                gid_start: spec.subordinate_gids.start,
                uid_count: spec.subordinate_uids.count,
                gid_count: spec.subordinate_gids.count,
            },
            LinuxOperation::CreateDirectory {
                path: spec.runtime_dir.clone(),
                mode: 0o700,
            },
            LinuxOperation::CreateDirectory {
                path: spec.data_dir.clone(),
                mode: 0o700,
            },
            LinuxOperation::CreateDirectory {
                path: spec.runtime_dir.join(CELL_ROOT),
                mode: 0o700,
            },
        ],
        LifecycleStep::CreateNamespaces => vec![LinuxOperation::CreateNamespaceLeader {
            namespaces: vec![
                NamespaceKind::User,
                NamespaceKind::Pid,
                NamespaceKind::Mount,
                NamespaceKind::Ipc,
                NamespaceKind::Uts,
                NamespaceKind::Network,
                NamespaceKind::Cgroup,
            ],
            retain_pidfd: true,
        }],
        LifecycleStep::PopulateIdMaps => vec![
            LinuxOperation::WriteIdMap {
                kind: IdMapKind::Uid,
                inside_start: 0,
                outside_start: spec.subordinate_uids.start,
                count: spec.subordinate_uids.count,
            },
            LinuxOperation::DenySetgroups,
            LinuxOperation::WriteIdMap {
                kind: IdMapKind::Gid,
                inside_start: 0,
                outside_start: spec.subordinate_gids.start,
                count: spec.subordinate_gids.count,
            },
        ],
        LifecycleStep::AssembleMountsAndDevices => mount_operations(spec),
        LifecycleStep::CreateBinderContexts => binder_operations(),
        LifecycleStep::CreateNetworkPolicy => {
            let names = network_names(spec);
            vec![
                LinuxOperation::AllocateNetworkLease {
                    tag: names.tag.clone(),
                },
                LinuxOperation::CreateVeth {
                    host_name: names.host,
                    cell_name: names.cell,
                    tag: names.tag.clone(),
                },
                LinuxOperation::InstallNetworkPolicy { tag: names.tag },
            ]
        }
        LifecycleStep::ApplyResourceLimits => vec![LinuxOperation::ConfigureCgroup {
            path: cgroup_path(spec),
            pids_max: 32_768,
            memory_max: 4 * 1024 * 1024 * 1024,
        }],
        LifecycleStep::StartAndroidInit => vec![
            LinuxOperation::InstallSharedFilesystemGuard,
            LinuxOperation::StartAndroidInit {
                executable: "/system/bin/init".into(),
                arguments: vec!["/system/bin/init".into()],
                no_new_privileges_before_exec: false,
            },
        ],
        LifecycleStep::PublishHandle => vec![LinuxOperation::PublishHandle {
            path: spec.runtime_dir.join("control.sock"),
            owner_uid: spec.host_uid,
            mode: 0o600,
        }],
    }
}

fn mount_operations(spec: &CellSpec) -> Vec<LinuxOperation> {
    let mut operations = vec![LinuxOperation::MakeMountTreePrivate];
    operations.extend(partition_mounts(spec));
    if let Some(android_init) = &spec.android_init {
        operations.push(mount(
            MountKind::FileBind,
            Some(android_init.clone()),
            Path::new("/system/bin/init"),
            true,
            true,
            true,
            false,
        ));
    }
    operations.extend(spec.android_file_overrides.iter().map(|file_override| {
        mount(
            MountKind::FileBind,
            Some(file_override.source.clone()),
            &file_override.target,
            true,
            true,
            true,
            false,
        )
    }));
    operations.extend(spec.android_runtime_directories.iter().map(|directory| {
        mount(
            MountKind::DirectoryBind,
            Some(directory.source.clone()),
            &directory.target,
            true,
            true,
            true,
            false,
        )
    }));
    operations.extend(private_filesystem_mounts(spec));
    operations.extend(spec.shared_storage().iter().map(|directory| {
        LinuxOperation::MountSharedStorage {
            source: directory.source.clone(),
            target: directory.android_backing_path(),
            android_uid_package: "com.android.providers.media.module".into(),
            android_gid: 1023,
            primary_host_gid: spec.subordinate_gids.start,
            host_uid: spec.host_uid,
            host_gid: directory.host_gid,
        }
    }));
    operations.extend(basic_device_operations());
    operations.push(LinuxOperation::CreateGraphicsDevice {
        source: spec.render_node.clone(),
        target: "/dev/dri/renderD128".into(),
        mode: 0o666,
    });
    let auxiliary = spec.graphics_backend.auxiliary_devices();
    operations.extend(
        auxiliary
            .iter()
            .map(|path| LinuxOperation::CreateGraphicsDevice {
                source: path.into(),
                target: path.into(),
                mode: 0o666,
            }),
    );
    if !auxiliary.contains(&crate::MEDIA_DMA_HEAP) {
        operations.push(LinuxOperation::CreateOptionalMediaDevice {
            source: crate::MEDIA_DMA_HEAP.into(),
            target: crate::MEDIA_DMA_HEAP.into(),
            mode: 0o666,
        });
    }
    operations.push(mount(
        MountKind::FileBind,
        Some(spec.denial_socket.clone()),
        Path::new("/dev/socket/droidloom/denial"),
        false,
        true,
        true,
        true,
    ));
    operations
}

fn partition_mounts(spec: &CellSpec) -> Vec<LinuxOperation> {
    vec![
        mount(
            MountKind::ReadOnlyAndroidImage,
            Some(spec.image_dir.join("images/system.img")),
            Path::new("/"),
            true,
            true,
            true,
            false,
        ),
        mount(
            MountKind::ReadOnlyAndroidImage,
            Some(crate::gapps::partition_image(spec, "system_ext")),
            Path::new("/system_ext"),
            true,
            true,
            true,
            false,
        ),
        mount(
            MountKind::ReadOnlyAndroidImage,
            Some(crate::gapps::partition_image(spec, "product")),
            Path::new("/product"),
            true,
            true,
            true,
            false,
        ),
        mount(
            MountKind::Ext4Image,
            Some(spec.vendor_image.clone()),
            Path::new("/vendor"),
            true,
            true,
            true,
            false,
        ),
    ]
}

fn private_filesystem_mounts(spec: &CellSpec) -> Vec<LinuxOperation> {
    vec![
        mount(
            MountKind::HostStorage,
            Some(spec.data_dir.join("data")),
            Path::new("/data"),
            false,
            true,
            true,
            false,
        ),
        mount(
            MountKind::HostStorage,
            Some(spec.data_dir.join("metadata")),
            Path::new("/metadata"),
            false,
            true,
            true,
            false,
        ),
        mount(
            MountKind::Proc,
            None,
            Path::new("/proc"),
            false,
            true,
            true,
            true,
        ),
        mount(
            MountKind::SyntheticSys,
            None,
            Path::new("/sys"),
            true,
            true,
            true,
            true,
        ),
        mount(
            MountKind::Tmpfs,
            None,
            Path::new("/dev"),
            false,
            false,
            true,
            true,
        ),
        mount(
            MountKind::Tmpfs,
            None,
            Path::new("/dev/input"),
            true,
            true,
            true,
            true,
        ),
        mount(
            MountKind::DevPts,
            None,
            Path::new("/dev/pts"),
            false,
            true,
            true,
            true,
        ),
    ]
}

fn basic_device_operations() -> Vec<LinuxOperation> {
    vec![
        LinuxOperation::CreateBasicDevice {
            path: "/dev/null".into(),
            major: 1,
            minor: 3,
            mode: 0o666,
        },
        LinuxOperation::CreateBasicDevice {
            path: "/dev/zero".into(),
            major: 1,
            minor: 5,
            mode: 0o666,
        },
        LinuxOperation::CreateBasicDevice {
            path: "/dev/full".into(),
            major: 1,
            minor: 7,
            mode: 0o666,
        },
        LinuxOperation::CreateBasicDevice {
            path: "/dev/random".into(),
            major: 1,
            minor: 8,
            mode: 0o666,
        },
        LinuxOperation::CreateBasicDevice {
            path: "/dev/urandom".into(),
            major: 1,
            minor: 9,
            mode: 0o666,
        },
        LinuxOperation::CreateBasicDevice {
            path: "/dev/fuse".into(),
            major: 10,
            minor: 229,
            mode: 0o666,
        },
    ]
}

fn binder_operations() -> Vec<LinuxOperation> {
    let mut operations = vec![mount(
        MountKind::BinderFs,
        None,
        Path::new("/dev/binderfs"),
        false,
        false,
        true,
        true,
    )];
    for name in ["binder", "hwbinder", "vndbinder"] {
        operations.push(LinuxOperation::AddBinderDevice {
            name: name.into(),
            compatibility_path: Path::new("/dev").join(name),
        });
    }
    operations
}

#[allow(
    clippy::fn_params_excessive_bools,
    reason = "the four independent mount security flags mirror the kernel contract"
)]
fn mount(
    kind: MountKind,
    source: Option<PathBuf>,
    target: &Path,
    read_only: bool,
    nodev: bool,
    nosuid: bool,
    noexec: bool,
) -> LinuxOperation {
    LinuxOperation::Mount {
        kind,
        source,
        target: target.to_path_buf(),
        read_only,
        nodev,
        nosuid,
        noexec,
    }
}

fn teardown_operations(spec: &CellSpec, step: LifecycleStep) -> Vec<LinuxOperation> {
    match step {
        LifecycleStep::PublishHandle => vec![LinuxOperation::UnpublishHandle {
            path: spec.runtime_dir.join("control.sock"),
        }],
        LifecycleStep::StartAndroidInit => vec![LinuxOperation::StopAndroidInit {
            grace_milliseconds: 5_000,
            cgroup: cgroup_path(spec),
        }],
        LifecycleStep::ApplyResourceLimits => vec![LinuxOperation::RemoveCgroup {
            path: cgroup_path(spec),
        }],
        LifecycleStep::CreateNetworkPolicy => {
            let names = network_names(spec);
            vec![
                LinuxOperation::RemoveNetworkPolicy {
                    tag: names.tag.clone(),
                },
                LinuxOperation::RemoveVeth {
                    host_name: names.host,
                    tag: names.tag,
                },
            ]
        }
        LifecycleStep::CreateBinderContexts => vec![LinuxOperation::Unmount {
            target: "/dev/binderfs".into(),
        }],
        LifecycleStep::AssembleMountsAndDevices => {
            let mut targets: Vec<_> = spec
                .android_runtime_directories
                .iter()
                .rev()
                .map(|directory| directory.target.clone())
                .collect();
            targets.extend(
                spec.android_file_overrides
                    .iter()
                    .rev()
                    .map(|file_override| file_override.target.clone()),
            );
            if spec.android_init.is_some() {
                targets.push(PathBuf::from("/system/bin/init"));
            }
            targets.extend([PathBuf::from("/dev/socket/droidloom/denial")]);
            targets.extend(
                spec.shared_storage()
                    .iter()
                    .rev()
                    .map(crate::SharedStorageDirectory::android_backing_path),
            );
            targets.extend(
                [
                    "/dev/pts",
                    "/dev/input",
                    "/dev",
                    "/sys",
                    "/proc",
                    "/metadata",
                    "/data",
                    "/vendor",
                    "/product",
                    "/system_ext",
                    "/",
                ]
                .into_iter()
                .map(PathBuf::from),
            );
            targets
                .into_iter()
                .map(|target| LinuxOperation::Unmount { target })
                .collect()
        }
        LifecycleStep::PopulateIdMaps => Vec::new(),
        LifecycleStep::CreateNamespaces => vec![LinuxOperation::DestroyNamespaceLeader],
        LifecycleStep::ReserveIdentity => vec![LinuxOperation::ReleaseIdentity {
            cell: spec.id(),
            runtime_dir: spec.runtime_dir.clone(),
        }],
    }
}

fn cgroup_path(spec: &CellSpec) -> PathBuf {
    Path::new(CGROUP_ROOT).join(spec.id().as_str())
}

struct NetworkNames {
    host: String,
    cell: String,
    tag: String,
}

fn network_names(spec: &CellSpec) -> NetworkNames {
    let suffix = format!("{:08x}", spec.host_uid);
    NetworkNames {
        host: format!("dlh{suffix}"),
        cell: format!("dlc{suffix}"),
        tag: format!("droidloom-{}", spec.id()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::IdRange;

    fn spec() -> CellSpec {
        CellSpec {
            host_uid: 1000,
            subordinate_uids: IdRange {
                start: 200_000,
                count: 100_000,
            },
            subordinate_gids: IdRange {
                start: 300_000,
                count: 100_000,
            },
            image_dir: "/var/lib/droidloom/images/current".into(),
            gapps_dir: None,
            vendor_image: "/var/lib/droidloom/images/current/images/vendor.raw.img".into(),
            android_init: Some("/usr/lib/droidloom/android/init".into()),
            android_file_overrides: vec![crate::AndroidFileOverride {
                source: "/usr/lib/droidloom/android/lib64/libbinder.so".into(),
                target: "/system/lib64/libbinder.so".into(),
            }],
            android_runtime_directories: vec![crate::AndroidRuntimeDirectory {
                source: "/usr/lib/droidloom/android/runtime".into(),
                target: "/droidloom/runtime".into(),
            }],
            shared_storage_directories: Vec::new(),
            share_user_folders: false,
            data_dir: "/var/lib/droidloom/users/1000/data".into(),
            runtime_dir: "/run/droidloom/cells/u1000".into(),
            gpu: Default::default(),
            render_node: "/dev/dri/renderD128".into(),
            graphics_backend: crate::GraphicsBackend::default(),
            denial_socket: "/run/user/1000/denial/native-bridge.sock".into(),
        }
    }

    #[test]
    fn gapps_selects_only_product_and_system_ext_before_boot() {
        let mut cell = spec();
        let base = partition_mounts(&cell);
        cell.gapps_dir = Some("/usr/lib/droidloom/addons/gapps".into());
        let addon = partition_mounts(&cell);
        assert_eq!(base[0], addon[0]);
        assert_eq!(base[3], addon[3]);
        assert_ne!(base[1], addon[1]);
        assert_ne!(base[2], addon[2]);
        for operation in &addon[1..3] {
            assert!(matches!(
                operation,
                LinuxOperation::Mount {
                    kind: MountKind::ReadOnlyAndroidImage,
                    ..
                }
            ));
        }
        let encoded = serde_json::to_string(&addon).unwrap();
        assert!(encoded.contains("/usr/lib/droidloom/addons/gapps/images/system_ext.img"));
        assert!(encoded.contains("/usr/lib/droidloom/addons/gapps/images/product.img"));
        cell.gapps_dir = Some("/usr/lib/droidloom/../escape".into());
        assert!(cell.validate().is_err());
    }

    #[test]
    fn plan_uses_transaction_order_and_reverse_teardown() {
        let plan = build_linux_plan(&spec()).unwrap();
        let construction: Vec<_> = plan.construction.iter().map(|entry| entry.step).collect();
        assert_eq!(construction, CONSTRUCTION_ORDER);
        let teardown: Vec<_> = plan.teardown.iter().map(|entry| entry.step).collect();
        assert_eq!(
            teardown,
            CONSTRUCTION_ORDER.into_iter().rev().collect::<Vec<_>>()
        );
    }

    #[test]
    fn device_plan_contains_one_render_node_and_no_host_device_tree() {
        let plan = build_linux_plan(&spec()).unwrap();
        assert_eq!(
            plan.invariants.render_nodes,
            [PathBuf::from("/dev/dri/renderD128")]
        );
        assert!(!plan.invariants.drm_card_nodes);
        assert!(!plan.invariants.physical_input_nodes);
        assert!(
            plan.construction
                .iter()
                .flat_map(|step| &step.operations)
                .any(|op| {
                    matches!(op, LinuxOperation::Mount {
                kind: MountKind::Tmpfs, source: None, target,
                read_only: true, nodev: true, nosuid: true, noexec: true,
            } if target == Path::new("/dev/input"))
                })
        );

        let sources: Vec<_> = plan
            .construction
            .iter()
            .flat_map(|step| &step.operations)
            .filter_map(|operation| match operation {
                LinuxOperation::Mount {
                    kind: MountKind::FileBind,
                    source,
                    ..
                } => source.as_ref(),
                _ => None,
            })
            .collect();
        assert_eq!(
            sources,
            [
                &PathBuf::from("/usr/lib/droidloom/android/init"),
                &PathBuf::from("/usr/lib/droidloom/android/lib64/libbinder.so"),
                &PathBuf::from("/run/user/1000/denial/native-bridge.sock")
            ]
        );

        let basic_devices = plan
            .construction
            .iter()
            .flat_map(|step| &step.operations)
            .filter_map(|operation| match operation {
                LinuxOperation::CreateBasicDevice {
                    path,
                    major,
                    minor,
                    mode,
                } => Some((path.as_path(), *major, *minor, *mode)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(basic_devices.contains(&(Path::new("/dev/fuse"), 10, 229, 0o666)));
    }

    #[test]
    fn media_heap_is_optional_unless_the_backend_allocates_from_it() {
        let media_device = |spec: &CellSpec| {
            build_linux_plan(spec)
                .unwrap()
                .construction
                .iter()
                .flat_map(|step| &step.operations)
                .any(|op| matches!(op, LinuxOperation::CreateOptionalMediaDevice { .. }))
        };
        let mut spec = spec();
        assert!(media_device(&spec));
        spec.graphics_backend = crate::GraphicsBackend::DrmDmaHeap;
        assert!(!media_device(&spec));
    }

    #[test]
    fn android_writable_storage_uses_host_directories() {
        let spec = spec();
        let plan = build_linux_plan(&spec).unwrap();
        assert!(plan.invariants.android_writable_host_filesystems);
        assert!(plan.invariants.shared_filesystem_guard);

        let data_mounts: Vec<_> = plan
            .construction
            .iter()
            .flat_map(|step| &step.operations)
            .filter_map(|operation| match operation {
                LinuxOperation::Mount {
                    kind: MountKind::HostStorage,
                    source: Some(source),
                    target,
                    ..
                } => Some((source.clone(), target.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(
            data_mounts,
            [
                (spec.data_dir.join("data"), PathBuf::from("/data")),
                (spec.data_dir.join("metadata"), PathBuf::from("/metadata"))
            ]
        );
    }

    #[test]
    fn explicit_shared_storage_is_idmapped_and_unmounted_before_data() {
        let mut spec = spec();
        spec.shared_storage_directories = vec![crate::SharedStorageDirectory {
            source: "/home/user/Downloads".into(),
            android_name: "Download".into(),
            host_gid: 1000,
        }];
        let plan = build_linux_plan(&spec).unwrap();
        assert!(plan.invariants.android_writable_host_filesystems);
        assert!(plan.construction.iter().any(|step| {
            step.operations.iter().any(|operation| {
                operation
                    == &LinuxOperation::MountSharedStorage {
                        source: "/home/user/Downloads".into(),
                        target: "/data/media/0/Download".into(),
                        android_uid_package: "com.android.providers.media.module".into(),
                        android_gid: 1023,
                        primary_host_gid: spec.subordinate_gids.start,
                        host_uid: 1000,
                        host_gid: 1000,
                    }
            })
        }));

        let teardown = plan
            .teardown
            .iter()
            .find(|step| step.step == LifecycleStep::AssembleMountsAndDevices)
            .unwrap();
        let shared = teardown
            .operations
            .iter()
            .position(|operation| {
                operation
                    == &LinuxOperation::Unmount {
                        target: "/data/media/0/Download".into(),
                    }
            })
            .unwrap();
        let data = teardown
            .operations
            .iter()
            .position(|operation| {
                operation
                    == &LinuxOperation::Unmount {
                        target: "/data".into(),
                    }
            })
            .unwrap();
        assert!(shared < data);
    }

    #[test]
    fn android_init_override_is_read_only_but_executable() {
        let plan = build_linux_plan(&spec()).unwrap();
        let init_mount = plan
            .construction
            .iter()
            .flat_map(|step| &step.operations)
            .find(|operation| {
                matches!(
                    operation,
                    LinuxOperation::Mount { target, .. }
                        if target == Path::new("/system/bin/init")
                )
            })
            .unwrap();
        assert_eq!(
            init_mount,
            &LinuxOperation::Mount {
                kind: MountKind::FileBind,
                source: Some(PathBuf::from("/usr/lib/droidloom/android/init")),
                target: PathBuf::from("/system/bin/init"),
                read_only: true,
                nodev: true,
                nosuid: true,
                noexec: false,
            }
        );
    }

    #[test]
    fn android_file_overrides_are_read_only_but_mappable() {
        let plan = build_linux_plan(&spec()).unwrap();
        let library_mount = plan
            .construction
            .iter()
            .flat_map(|step| &step.operations)
            .find(|operation| {
                matches!(
                    operation,
                    LinuxOperation::Mount { target, .. }
                        if target == Path::new("/system/lib64/libbinder.so")
                )
            })
            .unwrap();
        assert_eq!(
            library_mount,
            &LinuxOperation::Mount {
                kind: MountKind::FileBind,
                source: Some(PathBuf::from(
                    "/usr/lib/droidloom/android/lib64/libbinder.so"
                )),
                target: PathBuf::from("/system/lib64/libbinder.so"),
                read_only: true,
                nodev: true,
                nosuid: true,
                noexec: false,
            }
        );
    }

    #[test]
    fn android_runtime_directories_are_private_read_only_projections() {
        let plan = build_linux_plan(&spec()).unwrap();
        let runtime_mount = plan
            .construction
            .iter()
            .flat_map(|step| &step.operations)
            .find(|operation| {
                matches!(
                    operation,
                    LinuxOperation::Mount { target, .. }
                        if target == Path::new("/droidloom/runtime")
                )
            })
            .unwrap();
        assert_eq!(
            runtime_mount,
            &LinuxOperation::Mount {
                kind: MountKind::DirectoryBind,
                source: Some(PathBuf::from("/usr/lib/droidloom/android/runtime")),
                target: PathBuf::from("/droidloom/runtime"),
                read_only: true,
                nodev: true,
                nosuid: true,
                noexec: false,
            }
        );
    }

    #[test]
    fn binder_and_partition_allowlists_are_exact() {
        let plan = build_linux_plan(&spec()).unwrap();
        let operations: Vec<_> = plan
            .construction
            .iter()
            .flat_map(|step| &step.operations)
            .collect();
        let binder_names: Vec<_> = operations
            .iter()
            .filter_map(|operation| match operation {
                LinuxOperation::AddBinderDevice { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(binder_names, ["binder", "hwbinder", "vndbinder"]);

        let partitions: Vec<_> =
            operations
                .iter()
                .filter_map(|operation| match operation {
                    LinuxOperation::Mount {
                        kind:
                            MountKind::ErofsImage
                            | MountKind::Ext4Image
                            | MountKind::ReadOnlyAndroidImage,
                        target,
                        read_only,
                        ..
                    } => {
                        assert!(*read_only);
                        Some(target.as_path())
                    }
                    _ => None,
                })
                .collect();
        assert_eq!(
            partitions,
            [
                Path::new("/"),
                Path::new("/system_ext"),
                Path::new("/product"),
                Path::new("/vendor")
            ]
        );
    }

    #[test]
    fn network_objects_are_bounded_and_tagged() {
        let names = network_names(&spec());
        assert!(names.host.len() <= 15);
        assert!(names.cell.len() <= 15);
        assert_eq!(names.tag, "droidloom-u1000");
    }
}
