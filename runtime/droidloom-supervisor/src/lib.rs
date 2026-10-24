//! Fail-closed lifecycle transaction for one Droidloom Android cell.
//!
//! This crate contains the security-sensitive ordering and rollback logic. A
//! platform backend performs the Linux operations; the transaction never
//! publishes a cell until every prerequisite and Android init have started.

#![deny(unsafe_op_in_unsafe_fn)]

mod adb;
mod app_properties;
pub mod boot_notice;
pub mod control;
mod cpu_placement;
pub mod development;
mod development_network;
mod filesystem_guard;
pub mod gapps;
pub mod host_storage;
mod identity;
pub mod linux_plan;
mod package_cache;
pub mod storage_migration;
mod tracing;
mod user_folders;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{self, Display};
use std::path::{Component, Path, PathBuf};

use droidloom_contracts::MIN_SUBORDINATE_IDS;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Stable identifier for the single Android cell owned by a host user.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CellId(String);

impl CellId {
    /// Derive the v1 cell identifier from a numeric host UID.
    pub fn for_host_uid(host_uid: u32) -> Self {
        Self(format!("u{host_uid}"))
    }

    /// Return the serialized identifier.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Display for CellId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// One contiguous outer UID or GID allocation.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdRange {
    /// First host ID in the allocation.
    pub start: u32,
    /// Number of IDs in the allocation.
    pub count: u32,
}

/// One package-owned file or directory mounted over its counterpart in an
/// immutable Android partition.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AndroidFileOverride {
    /// Normalized absolute host path to the package-owned replacement.
    pub source: PathBuf,
    /// Normalized absolute path inside an immutable Android partition, of the
    /// same kind as `source`.
    pub target: PathBuf,
}

/// One package-owned directory projected read-only into Android's private
/// `/droidloom` runtime tree.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AndroidRuntimeDirectory {
    /// Normalized absolute host path to the package-owned directory.
    pub source: PathBuf,
    /// Unique normalized directory path below `/droidloom` inside the cell.
    pub target: PathBuf,
}

/// One host-owned directory exposed as a top-level Android shared-storage
/// collection through an idmapped bind mount.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SharedStorageDirectory {
    /// Existing normalized host directory owned by [`CellSpec::host_uid`].
    pub source: PathBuf,
    /// Single Android path component below `/data/media/0`.
    pub android_name: String,
    /// Host ownership group mapped to Android's `media_rw` group.
    pub host_gid: u32,
}

impl SharedStorageDirectory {
    /// Return the backing path consumed by Android's ordinary emulated-storage
    /// service. Applications continue to see `/storage/emulated/0/...`.
    pub fn android_backing_path(&self) -> PathBuf {
        Path::new("/data/media/0").join(&self.android_name)
    }
}

impl IdRange {
    fn end_exclusive(self) -> Option<u64> {
        u64::from(self.start).checked_add(u64::from(self.count))
    }

    fn contains(self, id: u32) -> bool {
        self.end_exclusive()
            .is_some_and(|end| u64::from(id) >= u64::from(self.start) && u64::from(id) < end)
    }
}

/// Explicit rendering/allocator pairing for the host's graphics architecture.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GraphicsBackend {
    /// Native DRM rendering and allocation; retains existing cell behavior.
    #[default]
    Drm,
    /// Native DRM rendering with portable linear system DMA-heap images.
    DrmDmaHeap,
    /// Turnip KGSL rendering, Zink GLES and linear system DMA-heap images.
    KgslDmaHeap,
}

/// System DMA heap that Codec2 allocates its linear media buffers from. Every
/// cell gets it when the host has it; without it the cell still starts, but
/// apps cannot decode audio or video.
pub const MEDIA_DMA_HEAP: &str = "/dev/dma_heap/system";

impl GraphicsBackend {
    /// Additional bounded device paths needed by this pairing.
    pub fn auxiliary_devices(self) -> &'static [&'static str] {
        match self {
            Self::Drm => &[],
            Self::DrmDmaHeap => &["/dev/dma_heap/system"],
            Self::KgslDmaHeap => &["/dev/kgsl-3d0", "/dev/dma_heap/system"],
        }
    }
}

/// Immutable inputs required to construct one cell.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CellSpec {
    /// Host user who owns this cell.
    pub host_uid: u32,
    /// Dedicated host UID range mapped to Android IDs starting at zero.
    pub subordinate_uids: IdRange,
    /// Dedicated host GID range mapped to Android IDs starting at zero.
    pub subordinate_gids: IdRange,
    /// Activated, verified image directory.
    pub image_dir: PathBuf,
    /// Explicit optional Google-app image pair. Omission uses the base images.
    /// First activation requires fresh Android data; package installation alone
    /// never enables Google services.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gapps_dir: Option<PathBuf>,
    /// Raw ext4 Droidloom vendor image compatible with the activated base.
    pub vendor_image: PathBuf,
    /// Optional package-owned Android init binary mounted over the immutable
    /// base image's `/system/bin/init`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub android_init: Option<PathBuf>,
    /// Package-owned compatibility files and directories mounted read-only
    /// over their immutable Android partition counterparts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub android_file_overrides: Vec<AndroidFileOverride>,
    /// Package-owned compatibility trees projected read-only below the
    /// cell-private `/droidloom` directory.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub android_runtime_directories: Vec<AndroidRuntimeDirectory>,
    /// Explicit host directories shared with Android's normal emulated
    /// storage. Omission keeps every host directory private.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shared_storage_directories: Vec<SharedStorageDirectory>,
    /// Also share the desktop user's Downloads, Pictures, Documents, Music
    /// and Videos as Android's own folders, found at each start.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub share_user_folders: bool,
    /// Private host parent containing direct `data` and `metadata` directories.
    /// Android shares their host filesystem capacity; no writable disk images.
    pub data_dir: PathBuf,
    /// Ephemeral cell directory below `/run/droidloom/cells`.
    pub runtime_dir: PathBuf,
    /// The only DRM device exposed to Android.
    pub render_node: PathBuf,
    /// Host-validated graphics pairing. Omission uses native DRM.
    #[serde(default)]
    pub graphics_backend: GraphicsBackend,
    /// Authenticated Denial endpoint exposed at the fixed Android path.
    pub denial_socket: PathBuf,
}

impl CellSpec {
    /// Return the stable v1 identifier for this specification.
    pub fn id(&self) -> CellId {
        CellId::for_host_uid(self.host_uid)
    }

    /// The host directories Android shows in its shared storage: the explicit
    /// ones, then the user's own folders that can be shared now.
    pub fn shared_storage(&self) -> Vec<SharedStorageDirectory> {
        let mut directories = self.shared_storage_directories.clone();
        directories.extend(user_folders::resolve(self));
        directories
    }

    /// Validate paths, ID mappings, and render-node policy before mutation.
    ///
    /// # Errors
    ///
    /// Returns every discovered contract violation.
    pub fn validate(&self) -> Result<(), SpecError> {
        let mut problems = Vec::new();

        for (name, range) in [
            ("subordinate_uids", self.subordinate_uids),
            ("subordinate_gids", self.subordinate_gids),
        ] {
            if u64::from(range.count) < MIN_SUBORDINATE_IDS {
                problems.push(format!(
                    "{name} has {} IDs; at least {MIN_SUBORDINATE_IDS} are required",
                    range.count
                ));
            }
            if range
                .end_exclusive()
                .is_none_or(|end| end > u64::from(u32::MAX) + 1)
            {
                problems.push(format!("{name} overflows the 32-bit ID space"));
            }
        }

        if self.subordinate_uids.contains(self.host_uid) {
            problems.push("the interactive host UID must not be inside the Android UID map".into());
        }

        for (name, path) in [
            ("image_dir", self.image_dir.as_path()),
            ("vendor_image", self.vendor_image.as_path()),
            ("data_dir", self.data_dir.as_path()),
            ("runtime_dir", self.runtime_dir.as_path()),
            ("render_node", self.render_node.as_path()),
            ("denial_socket", self.denial_socket.as_path()),
        ] {
            if !is_normal_absolute(path) {
                problems.push(format!("{name} must be a normalized absolute path"));
            }
        }
        if self
            .gapps_dir
            .as_ref()
            .is_some_and(|path| !is_normal_absolute(path))
        {
            problems.push("gapps_dir must be a normalized absolute path".into());
        }

        if self
            .android_init
            .as_deref()
            .is_some_and(|path| !is_normal_absolute(path))
        {
            problems.push("android_init must be a normalized absolute path".into());
        }

        let mut override_targets = BTreeSet::new();
        for (index, file_override) in self.android_file_overrides.iter().enumerate() {
            if !is_normal_absolute(&file_override.source) {
                problems.push(format!(
                    "android_file_overrides[{index}].source must be a normalized absolute path"
                ));
            }
            if !is_android_file_override_target(&file_override.target) {
                problems.push(format!(
                    "android_file_overrides[{index}].target must be a normalized path below /system, /system_ext, /product, or /vendor"
                ));
            }
            if file_override.target == Path::new("/system/bin/init") {
                problems.push(format!(
                    "android_file_overrides[{index}].target must use android_init for /system/bin/init"
                ));
            }
            if !override_targets.insert(file_override.target.clone()) {
                problems.push(format!(
                    "android_file_overrides[{index}].target duplicates {}",
                    file_override.target.display()
                ));
            }
        }

        let mut runtime_targets = BTreeSet::new();
        for (index, directory) in self.android_runtime_directories.iter().enumerate() {
            if !is_normal_absolute(&directory.source) {
                problems.push(format!(
                    "android_runtime_directories[{index}].source must be a normalized absolute path"
                ));
            }
            if !is_droidloom_runtime_target(&directory.target) {
                problems.push(format!(
                    "android_runtime_directories[{index}].target must be a normalized directory below /droidloom"
                ));
            }
            if runtime_targets.iter().any(|target: &PathBuf| {
                target.starts_with(&directory.target) || directory.target.starts_with(target)
            }) {
                problems.push(format!(
                    "android_runtime_directories[{index}].target overlaps {}",
                    directory.target.display()
                ));
            }
            runtime_targets.insert(directory.target.clone());
        }

        validate_shared_storage_specs(&self.shared_storage_directories, &mut problems);

        let render_name = self.render_node.file_name().and_then(|name| name.to_str());
        let render_number = render_name
            .and_then(|name| name.strip_prefix("renderD"))
            .filter(|suffix| !suffix.is_empty())
            .filter(|suffix| suffix.bytes().all(|byte| byte.is_ascii_digit()));
        if self.render_node.parent() != Some(Path::new("/dev/dri")) || render_number.is_none() {
            problems.push("render_node must name exactly one /dev/dri/renderD<number> node".into());
        }

        if problems.is_empty() {
            Ok(())
        } else {
            Err(SpecError { problems })
        }
    }
}

fn is_normal_absolute(path: &Path) -> bool {
    path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::RootDir | Component::Normal(_)))
}

fn is_android_file_override_target(path: &Path) -> bool {
    is_normal_absolute(path)
        && ["/system", "/system_ext", "/product", "/vendor"]
            .into_iter()
            .map(Path::new)
            .any(|root| path.starts_with(root) && path != root)
}

fn is_droidloom_runtime_target(path: &Path) -> bool {
    is_normal_absolute(path) && path.starts_with("/droidloom") && path != Path::new("/droidloom")
}

fn is_android_shared_storage_name(name: &str) -> bool {
    if name.is_empty()
        || name.len() > 255
        || name.as_bytes().contains(&0)
        || name.eq_ignore_ascii_case("android")
    {
        return false;
    }
    let mut components = Path::new(name).components();
    matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none()
}

fn validate_shared_storage_specs(
    directories: &[SharedStorageDirectory],
    problems: &mut Vec<String>,
) {
    let mut names = BTreeSet::new();
    let common_gid = directories.first().map(|directory| directory.host_gid);
    for (index, directory) in directories.iter().enumerate() {
        if !is_normal_absolute(&directory.source) {
            problems.push(format!(
                "shared_storage_directories[{index}].source must be a normalized absolute path"
            ));
        }
        if !is_android_shared_storage_name(&directory.android_name) {
            problems.push(format!(
                "shared_storage_directories[{index}].android_name must be one non-reserved Android path component"
            ));
        } else if !names.insert(directory.android_name.clone()) {
            problems.push(format!(
                "shared_storage_directories[{index}].android_name duplicates {}",
                directory.android_name
            ));
        }
        if Some(directory.host_gid) != common_gid {
            problems.push(format!(
                "shared_storage_directories[{index}].host_gid must match the other shared directories"
            ));
        }
    }
}

/// Invalid cell specification.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("invalid cell specification: {0}", .problems.join("; "))]
pub struct SpecError {
    problems: Vec<String>,
}

impl SpecError {
    /// Individual validation failures.
    pub fn problems(&self) -> &[String] {
        &self.problems
    }
}

/// Ordered construction stages and their reverse teardown ownership.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleStep {
    /// Reserve the ID allocation, runtime path, and parent cgroup.
    ReserveIdentity,
    /// Create private user, PID, mount, IPC, UTS, network, and cgroup namespaces.
    CreateNamespaces,
    /// Populate the dedicated UID and GID maps.
    PopulateIdMaps,
    /// Assemble immutable images, mutable state, and the strict `/dev` tree.
    AssembleMountsAndDevices,
    /// Create the private Binder filesystem and three Binder contexts.
    CreateBinderContexts,
    /// Create tagged veth and host network policy.
    CreateNetworkPolicy,
    /// Apply the host-owned parent cgroup limits.
    ApplyResourceLimits,
    /// Start Android init and retain its lifecycle authority.
    StartAndroidInit,
    /// Publish the per-user lifecycle handle only after init starts.
    PublishHandle,
}

/// The complete, fixed v1 construction order.
pub const CONSTRUCTION_ORDER: [LifecycleStep; 9] = [
    LifecycleStep::ReserveIdentity,
    LifecycleStep::CreateNamespaces,
    LifecycleStep::PopulateIdMaps,
    LifecycleStep::AssembleMountsAndDevices,
    LifecycleStep::CreateBinderContexts,
    LifecycleStep::CreateNetworkPolicy,
    LifecycleStep::ApplyResourceLimits,
    LifecycleStep::StartAndroidInit,
    LifecycleStep::PublishHandle,
];

/// Linux-specific operations behind the deterministic lifecycle transaction.
pub trait CellBackend {
    /// Backend error with actionable context.
    type Error: std::error::Error;

    /// Apply one construction step.
    ///
    /// # Errors
    ///
    /// Must fail without claiming ownership when the operation did not finish.
    fn apply(&mut self, spec: &CellSpec, step: LifecycleStep) -> Result<(), Self::Error>;

    /// Reverse one successfully applied construction step.
    ///
    /// # Errors
    ///
    /// Implementations must make absence a success so teardown is idempotent.
    fn rollback(&mut self, spec: &CellSpec, step: LifecycleStep) -> Result<(), Self::Error>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CellRecord {
    spec: CellSpec,
    completed: Vec<LifecycleStep>,
}

/// A successfully published Android cell.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActiveCell {
    /// Stable cell identifier.
    pub id: CellId,
    /// Host user who owns the cell.
    pub host_uid: u32,
}

/// Start failure with the failing stage and any rollback residue.
#[derive(Debug, Error)]
pub enum StartError<E: std::error::Error> {
    /// The specification failed before any mutation.
    #[error(transparent)]
    InvalidSpec(#[from] SpecError),
    /// A cell for this user already exists or has retained rollback residue.
    #[error("cell {0} already exists or requires teardown")]
    AlreadyExists(CellId),
    /// A backend stage failed; rollback failures retain a teardown record.
    #[error("cell {cell} failed at {step:?}: {source}; rollback failures: {rollback_failures:?}")]
    Backend {
        /// Cell being constructed.
        cell: CellId,
        /// Stage that did not complete.
        step: LifecycleStep,
        /// Backend failure.
        source: E,
        /// Stages that could not be reversed, with rendered errors.
        rollback_failures: Vec<(LifecycleStep, String)>,
    },
}

/// Teardown failure. Successfully reversed stages are not retried.
#[derive(Debug, Error)]
#[error("cell {cell} teardown retained stages: {failures:?}")]
pub struct TeardownError {
    /// Cell whose teardown was incomplete.
    pub cell: CellId,
    /// Failed stages and rendered backend errors.
    pub failures: Vec<(LifecycleStep, String)>,
}

/// Result of an idempotent successful teardown.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TeardownReport {
    /// True when there was no active or residual record to tear down.
    pub already_absent: bool,
    /// Number of owned stages successfully reversed.
    pub reversed_steps: usize,
}

/// Deterministic owner of active and incompletely rolled-back cells.
#[derive(Debug)]
pub struct Supervisor<B> {
    backend: B,
    cells: BTreeMap<CellId, CellRecord>,
}

impl<B: CellBackend> Supervisor<B> {
    /// Construct an empty supervisor using `backend`.
    pub fn new(backend: B) -> Self {
        Self {
            backend,
            cells: BTreeMap::new(),
        }
    }

    /// Apply all v1 construction stages and publish the lifecycle handle.
    ///
    /// # Errors
    ///
    /// Invalid input causes no mutation. A backend failure triggers immediate
    /// reverse-order rollback; unreversed stages remain visible to `teardown`.
    pub fn start(&mut self, spec: CellSpec) -> Result<ActiveCell, StartError<B::Error>> {
        spec.validate()?;
        let id = spec.id();
        let host_uid = spec.host_uid;
        if self.cells.contains_key(&id) {
            return Err(StartError::AlreadyExists(id));
        }

        let mut completed = Vec::new();
        for step in CONSTRUCTION_ORDER {
            if let Err(source) = self.backend.apply(&spec, step) {
                let (retained, rollback_failures) = self.rollback_steps(&spec, completed);
                if !retained.is_empty() {
                    self.cells.insert(
                        id.clone(),
                        CellRecord {
                            spec,
                            completed: retained,
                        },
                    );
                }
                return Err(StartError::Backend {
                    cell: id,
                    step,
                    source,
                    rollback_failures,
                });
            }
            completed.push(step);
        }

        self.cells
            .insert(id.clone(), CellRecord { spec, completed });
        Ok(ActiveCell { id, host_uid })
    }

    /// Reverse every owned stage. Calling this repeatedly is safe.
    ///
    /// # Errors
    ///
    /// Failed stages remain recorded and may be retried by a later call.
    pub fn teardown(&mut self, id: &CellId) -> Result<TeardownReport, TeardownError> {
        let Some(record) = self.cells.get(id) else {
            return Ok(TeardownReport {
                already_absent: true,
                reversed_steps: 0,
            });
        };
        let before = record.completed.len();
        let failures = self.rollback_record(id);
        let retained = self
            .cells
            .get(id)
            .map_or(0, |record| record.completed.len());
        if failures.is_empty() {
            Ok(TeardownReport {
                already_absent: false,
                reversed_steps: before,
            })
        } else {
            debug_assert!(retained > 0);
            Err(TeardownError {
                cell: id.clone(),
                failures,
            })
        }
    }

    /// True for both published cells and cells retaining failed rollback state.
    pub fn owns(&self, id: &CellId) -> bool {
        self.cells.contains_key(id)
    }

    /// Access the backend, primarily for diagnostics and evidence collection.
    pub fn backend(&self) -> &B {
        &self.backend
    }

    fn rollback_record(&mut self, id: &CellId) -> Vec<(LifecycleStep, String)> {
        let Some(record) = self.cells.get(id) else {
            return Vec::new();
        };
        let spec = record.spec.clone();
        let completed = record.completed.clone();
        let (retained, failures) = self.rollback_steps(&spec, completed);

        if retained.is_empty() {
            self.cells.remove(id);
        } else if let Some(record) = self.cells.get_mut(id) {
            record.completed = retained;
        }
        failures
    }

    fn rollback_steps(
        &mut self,
        spec: &CellSpec,
        completed: Vec<LifecycleStep>,
    ) -> (Vec<LifecycleStep>, Vec<(LifecycleStep, String)>) {
        let mut failures = Vec::new();
        let mut retained = Vec::new();
        for step in completed.into_iter().rev() {
            if let Err(error) = self.backend.rollback(spec, step) {
                failures.push((step, error.to_string()));
                retained.push(step);
            }
        }
        retained.reverse();
        (retained, failures)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::io;

    use super::*;

    #[derive(Debug, Default)]
    struct FakeBackend {
        events: Vec<(bool, LifecycleStep)>,
        fail_apply: Option<LifecycleStep>,
        fail_rollback_once: BTreeSet<LifecycleStep>,
    }

    impl CellBackend for FakeBackend {
        type Error = io::Error;

        fn apply(&mut self, _spec: &CellSpec, step: LifecycleStep) -> Result<(), Self::Error> {
            self.events.push((true, step));
            if self.fail_apply == Some(step) {
                Err(io::Error::other("injected apply failure"))
            } else {
                Ok(())
            }
        }

        fn rollback(&mut self, _spec: &CellSpec, step: LifecycleStep) -> Result<(), Self::Error> {
            self.events.push((false, step));
            if self.fail_rollback_once.remove(&step) {
                Err(io::Error::other("injected rollback failure"))
            } else {
                Ok(())
            }
        }
    }

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
            android_init: None,
            android_file_overrides: Vec::new(),
            android_runtime_directories: Vec::new(),
            shared_storage_directories: Vec::new(),
            share_user_folders: false,
            data_dir: "/var/lib/droidloom/users/1000/data".into(),
            runtime_dir: "/run/droidloom/cells/u1000".into(),
            render_node: "/dev/dri/renderD128".into(),
            graphics_backend: GraphicsBackend::default(),
            denial_socket: "/run/user/1000/denial/native-bridge.sock".into(),
        }
    }

    #[test]
    fn graphics_backend_is_explicit_and_bounded() {
        let original = spec();
        let mut json = serde_json::to_value(&original).unwrap();
        json.as_object_mut().unwrap().remove("graphics_backend");
        let decoded: CellSpec = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(decoded.graphics_backend, GraphicsBackend::Drm);
        assert!(decoded.graphics_backend.auxiliary_devices().is_empty());
        json["graphics_backend"] = serde_json::json!("drm_dma_heap");
        let decoded: CellSpec = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(decoded.graphics_backend, GraphicsBackend::DrmDmaHeap);
        assert_eq!(
            decoded.graphics_backend.auxiliary_devices(),
            &["/dev/dma_heap/system"]
        );
        json["graphics_backend"] = serde_json::json!("kgsl_dma_heap");
        let decoded: CellSpec = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(
            decoded.graphics_backend.auxiliary_devices(),
            &["/dev/kgsl-3d0", "/dev/dma_heap/system"]
        );
        json["graphics_backend"] = serde_json::json!("/dev/dri/card0");
        assert!(serde_json::from_value::<CellSpec>(json).is_err());
    }

    #[test]
    fn complete_start_and_teardown_are_strictly_ordered() {
        let mut supervisor = Supervisor::new(FakeBackend::default());
        let cell = supervisor.start(spec()).unwrap();
        assert_eq!(cell.id, CellId::for_host_uid(1000));

        let report = supervisor.teardown(&cell.id).unwrap();
        assert_eq!(report.reversed_steps, CONSTRUCTION_ORDER.len());
        assert!(!report.already_absent);

        let events = &supervisor.backend().events;
        assert_eq!(
            &events[..CONSTRUCTION_ORDER.len()],
            &CONSTRUCTION_ORDER.map(|step| (true, step))
        );
        let expected_reverse: Vec<_> = CONSTRUCTION_ORDER
            .into_iter()
            .rev()
            .map(|step| (false, step))
            .collect();
        assert_eq!(&events[CONSTRUCTION_ORDER.len()..], expected_reverse);
    }

    #[test]
    fn every_start_failure_rolls_back_completed_stages() {
        for failed in CONSTRUCTION_ORDER {
            let mut supervisor = Supervisor::new(FakeBackend {
                fail_apply: Some(failed),
                ..FakeBackend::default()
            });
            let error = supervisor.start(spec()).unwrap_err();
            assert!(matches!(error, StartError::Backend { step, .. } if step == failed));
            assert!(!supervisor.owns(&CellId::for_host_uid(1000)));

            let applied: Vec<_> = CONSTRUCTION_ORDER
                .into_iter()
                .take_while(|step| *step != failed)
                .collect();
            let expected: Vec<_> = CONSTRUCTION_ORDER
                .into_iter()
                .take(applied.len() + 1)
                .map(|step| (true, step))
                .chain(applied.into_iter().rev().map(|step| (false, step)))
                .collect();
            assert_eq!(supervisor.backend().events, expected);
        }
    }

    #[test]
    fn rollback_residue_remains_owned_and_is_retryable() {
        let mut fail_rollback_once = BTreeSet::new();
        fail_rollback_once.insert(LifecycleStep::AssembleMountsAndDevices);
        let mut supervisor = Supervisor::new(FakeBackend {
            fail_apply: Some(LifecycleStep::CreateNetworkPolicy),
            fail_rollback_once,
            ..FakeBackend::default()
        });
        let error = supervisor.start(spec()).unwrap_err();
        assert!(matches!(
            error,
            StartError::Backend {
                rollback_failures,
                ..
            } if rollback_failures.len() == 1
        ));
        let id = CellId::for_host_uid(1000);
        assert!(supervisor.owns(&id));
        assert_eq!(supervisor.teardown(&id).unwrap().reversed_steps, 1);
        assert!(!supervisor.owns(&id));
    }

    #[test]
    fn teardown_of_absent_cell_is_successful_and_explicit() {
        let mut supervisor = Supervisor::new(FakeBackend::default());
        let report = supervisor.teardown(&CellId::for_host_uid(1000)).unwrap();
        assert!(report.already_absent);
        assert_eq!(report.reversed_steps, 0);
    }

    #[test]
    fn spec_rejects_card_nodes_and_host_uid_aliasing() {
        let mut invalid = spec();
        invalid.render_node = "/dev/dri/card0".into();
        invalid.subordinate_uids.start = 0;
        invalid.android_init = Some("relative/init".into());
        let error = invalid.validate().unwrap_err();
        assert_eq!(error.problems().len(), 3);
    }

    #[test]
    fn spec_rejects_unsafe_or_duplicate_android_file_overrides() {
        let mut invalid = spec();
        invalid.android_file_overrides = vec![
            AndroidFileOverride {
                source: "relative/libbinder.so".into(),
                target: "/system/lib64/libbinder.so".into(),
            },
            AndroidFileOverride {
                source: "/usr/lib/droidloom/android/init".into(),
                target: "/system/bin/init".into(),
            },
            AndroidFileOverride {
                source: "/usr/lib/droidloom/android/libbinder-again.so".into(),
                target: "/system/lib64/libbinder.so".into(),
            },
            AndroidFileOverride {
                source: "/usr/lib/droidloom/android/outside".into(),
                target: "/data/local/tmp/outside".into(),
            },
        ];
        let error = invalid.validate().unwrap_err();
        assert_eq!(error.problems().len(), 4);
    }

    #[test]
    fn spec_rejects_unsafe_or_overlapping_runtime_directories() {
        let mut invalid = spec();
        invalid.android_runtime_directories = vec![
            AndroidRuntimeDirectory {
                source: "relative/source".into(),
                target: "/droidloom/compat".into(),
            },
            AndroidRuntimeDirectory {
                source: "/usr/lib/droidloom/other".into(),
                target: "/droidloom/compat/nested".into(),
            },
            AndroidRuntimeDirectory {
                source: "/usr/lib/droidloom/outside".into(),
                target: "/vendor/droidloom".into(),
            },
        ];
        let error = invalid.validate().unwrap_err();
        assert_eq!(error.problems().len(), 3);
    }

    #[test]
    fn spec_rejects_unsafe_and_duplicate_shared_storage_names() {
        let mut invalid = spec();
        invalid.shared_storage_directories = vec![
            SharedStorageDirectory {
                source: "relative/source".into(),
                android_name: "Download".into(),
                host_gid: 1000,
            },
            SharedStorageDirectory {
                source: "/home/user/other".into(),
                android_name: "Download".into(),
                host_gid: 1000,
            },
            SharedStorageDirectory {
                source: "/home/user/android".into(),
                android_name: "Android".into(),
                host_gid: 1000,
            },
        ];
        let error = invalid.validate().unwrap_err();
        assert_eq!(error.problems().len(), 3);
    }
}
