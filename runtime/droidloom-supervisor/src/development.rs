//! Minimal executable cell path for the first Android boot proof.
//!
//! This intentionally omits the production user-ID map, cgroup limits and
//! general seccomp policy. A mandatory inherited filter protects shared host
//! filesystems. It creates private mount, PID, cgroup, IPC and UTS
//! namespaces. A bounded veth/NAT path gives the private Android network
//! namespace outbound connectivity without exposing host network control.

#![deny(unsafe_op_in_unsafe_fn)]

use std::ffi::CString;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt, symlink};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ExitStatus};
use std::thread;
use std::time::{Duration, Instant};

use thiserror::Error;

use crate::adb::AdbConnection;
use crate::development_network::DevelopmentNetwork;
use crate::{CellSpec, SpecError};

const EROFS_MAGIC_OFFSET: u64 = 1024;
const EROFS_MAGIC: [u8; 4] = 0xe0f5_e1e2_u32.to_le_bytes();
const EXT4_MAGIC_OFFSET: u64 = 1024 + 56;
const EXT4_MAGIC: [u8; 2] = 0xef53_u16.to_le_bytes();
const ANDROID_SPARSE_MAGIC: [u8; 4] = 0xed26_ff3a_u32.to_le_bytes();
const DEVELOPMENT_LOOP_DEVICES: u32 = 256;
const ANDROID_SHARED_STORAGE_ROOT: &str = "data/media/0";
const ANDROID_MEDIA_RW_ID: u32 = 1023;
const MEDIA_PROVIDER_PACKAGE: &str = "com.android.providers.media.module";
const DEVELOPMENT_SYSTEM_LOWER: &str = "system-lower";
const BINDERFS_NAME_MAX: usize = 255;
const BINDERFS_DEVICE_NAME_BYTES: usize = BINDERFS_NAME_MAX + 1;
const ANDROID_DENIAL_SOCKET: &str = "dev/socket/droidloom/denial";
const ANDROID_RENDER_NODE: &str = "renderD128";
const ANDROID_CGROUPS_TARGET: &str = "system/etc/cgroups.json";
const DEVELOPMENT_CGROUPS: &str = include_str!("../assets/development-cgroups.json");
const ANDROID_INIT: &str = "system/bin/init";
const DROIDLOOM_CELL_MARKER_ENV: &str = "DROIDLOOM_CELL";
const ANDROID_SELINUX_COMPAT_SOURCE: &str = "vendor/lib64/libdroidloom_selinux_compat.so";
const ANDROID_SELINUX_COMPAT_TARGET: &str = "/vendor/lib64/libdroidloom_selinux_compat.so";
#[cfg(target_arch = "aarch64")]
const ANDROID_SELINUX_COMPAT_PRELOAD: &str =
    "/system/lib64/bootstrap/libclang_rt.hwasan-aarch64-android.so";
#[cfg(target_arch = "x86_64")]
const ANDROID_SELINUX_COMPAT_PRELOAD: &str = "/system/lib64/libclang_rt.asan-x86_64-android.so";
#[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
compile_error!("Droidloom supports only aarch64 and x86_64 hosts");
const DRM_DEVICE_MAJOR: u64 = 226;
const DRM_RENDER_MINOR_BASE: u64 = 128;
const MAX_RENDER_NODE_UEVENT_BYTES: usize = 64 * 1024;
const PRIVATE_SYSCTL_DIRECTORY: &str = "android-private-sysctls";
const PRIVATE_NET_CORE_DIRECTORY: &str = "net-core";
const PRIVATE_BPF_SYSCTLS: [(&str, &str); 2] =
    [("bpf_jit_enable", "1\n"), ("bpf_jit_kallsyms", "1\n")];
const POWER_SUPPLY_TYPES: [&str; 13] = [
    "Battery",
    "UPS",
    "Mains",
    "USB",
    "USB_DCP",
    "USB_HVDCP",
    "USB_CDP",
    "USB_ACA",
    "USB_C",
    "USB_PD",
    "USB_PD_DRP",
    "Wireless",
    "Dock",
];
const POWER_SUPPLY_ATTRIBUTES: [&str; 27] = [
    "type",
    "online",
    "is_dock",
    "status",
    "health",
    "present",
    "capacity",
    "capacity_level",
    "voltage_now",
    "voltage_max",
    "voltage_max_design",
    "voltage_min_design",
    "current_now",
    "current_avg",
    "current_max",
    "charge_counter",
    "charge_full",
    "charge_full_design",
    "cycle_count",
    "time_to_full_now",
    "temp",
    "technology",
    "state_of_health",
    "health_index",
    "health_status",
    "charging_state",
    "charging_policy",
];

/// Failure to validate or start the deliberately minimal first-boot cell.
#[derive(Debug, Error)]
pub enum DevelopmentError {
    /// The common cell contract is invalid.
    #[error(transparent)]
    InvalidSpec(#[from] SpecError),
    /// A filesystem operation failed.
    #[error("{context}: {source}")]
    Io {
        /// Operation being attempted.
        context: String,
        /// Underlying I/O error.
        source: io::Error,
    },
    /// An input is absent, has the wrong type, or has the wrong image format.
    #[error("invalid development boot input: {0}")]
    InvalidInput(String),
    /// A required host command failed.
    #[error("{program} failed ({status})")]
    Command {
        /// Executable name.
        program: String,
        /// Process exit rendering.
        status: ExitStatus,
    },
}

/// Validate all immutable inputs without changing the host.
///
/// # Errors
///
/// Rejects malformed specs, sparse/non-ext4 vendor images, unsupported base
/// image formats, non-device render paths, and non-socket Denial endpoints.
pub fn validate_development_inputs(spec: &CellSpec) -> Result<(), DevelopmentError> {
    spec.validate()?;
    for path in spec.graphics_backend.auxiliary_devices() {
        auxiliary_graphics_device_numbers(Path::new(path))?;
    }
    if effective_uid()? != 0 {
        return Err(DevelopmentError::InvalidInput(
            "development boot must be run as root".into(),
        ));
    }

    for name in ["system", "system_ext", "product"] {
        let image = crate::gapps::partition_image(spec, name);
        detect_read_only_filesystem(&image)?;
    }
    crate::gapps::validate(spec)
        .map_err(|error| DevelopmentError::InvalidInput(error.to_string()))?;
    if starts_with_magic(&spec.vendor_image, 0, &ANDROID_SPARSE_MAGIC)? {
        return Err(DevelopmentError::InvalidInput(format!(
            "{} is Android-sparse; convert it with simg2img before boot",
            spec.vendor_image.display()
        )));
    }
    verify_magic(
        &spec.vendor_image,
        EXT4_MAGIC_OFFSET,
        &EXT4_MAGIC,
        "raw ext4",
    )?;
    if let Some(android_init) = &spec.android_init {
        validate_android_init_override(android_init)?;
    }
    for file_override in &spec.android_file_overrides {
        validate_android_file_override_source(&file_override.source)?;
    }
    for directory in &spec.android_runtime_directories {
        validate_android_runtime_directory(&directory.source)?;
    }
    for directory in &spec.shared_storage() {
        validate_shared_storage_directory(&directory.source, (spec.host_uid, directory.host_gid))?;
    }
    crate::host_storage::validate(&spec.data_dir)
        .map_err(|error| io_error("validate direct host Android storage", error))?;

    let render =
        fs::metadata(&spec.render_node).map_err(|source| io_error("stat render node", source))?;
    if !render.file_type().is_char_device() {
        return Err(DevelopmentError::InvalidInput(format!(
            "{} is not a character device",
            spec.render_node.display()
        )));
    }
    let denial = fs::metadata(&spec.denial_socket)
        .map_err(|source| io_error("stat Denial socket", source))?;
    if !denial.file_type().is_socket() {
        return Err(DevelopmentError::InvalidInput(format!(
            "{} is not a Unix socket",
            spec.denial_socket.display()
        )));
    }
    Ok(())
}

fn validate_android_init_override(path: &Path) -> Result<(), DevelopmentError> {
    let metadata =
        fs::metadata(path).map_err(|source| io_error("stat Android init override", source))?;
    if !metadata.is_file() {
        return Err(DevelopmentError::InvalidInput(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    if metadata.permissions().mode() & 0o111 == 0 {
        return Err(DevelopmentError::InvalidInput(format!(
            "{} is not executable",
            path.display()
        )));
    }
    Ok(())
}

fn validate_android_file_override_source(path: &Path) -> Result<(), DevelopmentError> {
    let metadata = fs::metadata(path).map_err(|source| {
        io_error(
            &format!("stat Android file override {}", path.display()),
            source,
        )
    })?;
    if !metadata.is_file() && !metadata.is_dir() {
        return Err(DevelopmentError::InvalidInput(format!(
            "{} is neither a regular file nor a directory",
            path.display()
        )));
    }
    Ok(())
}

fn validate_android_runtime_directory(path: &Path) -> Result<(), DevelopmentError> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|source| io_error("stat Android runtime directory", source))?;
    if !metadata.is_dir() {
        return Err(DevelopmentError::InvalidInput(format!(
            "{} is not a directory",
            path.display()
        )));
    }
    Ok(())
}

fn validate_shared_storage_directory(
    path: &Path,
    expected_owner: (u32, u32),
) -> Result<(), DevelopmentError> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|source| io_error("stat shared-storage directory", source))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(DevelopmentError::InvalidInput(format!(
            "{} is not a real directory",
            path.display()
        )));
    }
    if (metadata.uid(), metadata.gid()) != expected_owner {
        return Err(DevelopmentError::InvalidInput(format!(
            "{} is owned by {}:{}, expected {}:{}",
            path.display(),
            metadata.uid(),
            metadata.gid(),
            expected_owner.0,
            expected_owner.1,
        )));
    }
    if metadata.mode() & 0o2000 == 0 {
        return Err(DevelopmentError::InvalidInput(format!(
            "{} needs setgid directory inheritance for Android shared-storage writes",
            path.display()
        )));
    }
    Ok(())
}

/// Start the development cell in private functional namespaces and wait for
/// Android init to exit.
///
/// The namespace child replaces itself with `/init`, so Android init is PID 1
/// inside the private PID namespace. Namespace teardown releases all mounts
/// and loop devices even when Android exits unsuccessfully.
///
/// # Errors
///
/// Fails before namespace creation when preflight validation or runtime path
/// preparation fails, or returns a failed `unshare` invocation as an error.
pub fn run_development_cell(spec_path: &Path, spec: &CellSpec) -> Result<(), DevelopmentError> {
    let executable = std::env::current_exe()
        .map_err(|source| io_error("resolve supervisor executable", source))?;
    let mut cell = start_development_cell(spec_path, spec, &executable)?;
    let status = cell.wait()?;
    if status.success() {
        Ok(())
    } else {
        Err(DevelopmentError::Command {
            program: "ip netns exec ... unshare".into(),
            status,
        })
    }
}

/// A running development Android cell whose namespace process, networking,
/// and ephemeral paths have one explicit owner.
pub struct DevelopmentCell {
    spec: CellSpec,
    child: Child,
    network: Option<DevelopmentNetwork>,
    adb: Option<AdbConnection>,
    cleaned: bool,
}

impl DevelopmentCell {
    /// Host PID of the exact `unshare` namespace owner.
    pub fn host_pid(&self) -> u32 {
        self.child.id()
    }

    /// Return the child exit status without blocking.
    ///
    /// Cleanup happens immediately once the child is observed to have exited.
    ///
    /// # Errors
    ///
    /// Returns process-wait or teardown failures.
    pub fn try_wait(&mut self) -> Result<Option<ExitStatus>, DevelopmentError> {
        let status = self
            .child
            .try_wait()
            .map_err(|source| io_error("poll Android namespace process", source))?;
        if status.is_some() {
            self.cleanup()?;
        }
        Ok(status)
    }

    /// Wait for Android init to exit and release every host resource.
    ///
    /// # Errors
    ///
    /// Returns process-wait or teardown failures.
    pub fn wait(&mut self) -> Result<ExitStatus, DevelopmentError> {
        let status = self
            .child
            .wait()
            .map_err(|source| io_error("wait for Android namespace process", source))?;
        self.cleanup()?;
        Ok(status)
    }

    /// Terminate the exact namespace owner, wait for its child-kill contract,
    /// and release all host resources.
    ///
    /// # Errors
    ///
    /// Returns signal, wait, or teardown failures.
    pub fn stop(&mut self) -> Result<(), DevelopmentError> {
        if self
            .child
            .try_wait()
            .map_err(|source| io_error("poll Android namespace process before stopping", source))?
            .is_none()
        {
            signal_process(self.child.id(), libc::SIGTERM)?;
            let deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < deadline {
                if self
                    .child
                    .try_wait()
                    .map_err(|source| io_error("wait for Android shutdown", source))?
                    .is_some()
                {
                    return self.cleanup();
                }
                thread::sleep(Duration::from_millis(50));
            }

            // `unshare --kill-child=TERM` binds Android PID 1 to this exact
            // outer process. Killing that owner cannot select an unrelated
            // process and still guarantees namespace teardown.
            self.child
                .kill()
                .map_err(|source| io_error("kill unresponsive Android namespace owner", source))?;
            self.child
                .wait()
                .map_err(|source| io_error("reap Android namespace owner", source))?;
        }
        self.cleanup()
    }

    fn cleanup(&mut self) -> Result<(), DevelopmentError> {
        if self.cleaned {
            return Ok(());
        }
        let mut first_error = None;
        // Remove the transport before its route disappears. The user's ADB
        // server and connections to other devices remain independent.
        self.adb.take();
        if let Some(mut network) = self.network.take()
            && let Err(error) = network.teardown()
        {
            first_error = Some(error);
        }
        if let Err(error) = cleanup_runtime_directory(&self.spec)
            && first_error.is_none()
        {
            first_error = Some(error);
        }
        self.cleaned = true;
        first_error.map_or(Ok(()), Err)
    }
}

impl Drop for DevelopmentCell {
    fn drop(&mut self) {
        if self.cleaned {
            return;
        }
        if let Err(error) = self.stop() {
            eprintln!("droidloom-supervisor: best-effort cell teardown failed: {error}");
        }
    }
}

/// Start a development cell and return its exact lifecycle handle.
///
/// `entry_executable` must expose the supervisor's private
/// `development-enter --spec ...` command. Separating it from the caller lets
/// `droidloomd` own the cell while keeping namespace entry in the small
/// supervisor executable.
///
/// # Errors
///
/// Fails on invalid inputs or any host setup/spawn operation.
pub fn start_development_cell(
    spec_path: &Path,
    spec: &CellSpec,
    entry_executable: &Path,
) -> Result<DevelopmentCell, DevelopmentError> {
    crate::user_folders::prepare(spec)
        .map_err(|source| io_error("prepare the user's folders for Android", source))?;
    validate_development_inputs(spec)?;
    prepare_host_directories(spec)?;
    let mut network = match DevelopmentNetwork::create(spec) {
        Ok(network) => network,
        Err(error) => {
            cleanup_runtime_directory(spec)?;
            return Err(error);
        }
    };
    let adb_client = match AdbConnection::prepare(spec) {
        Ok(client) => client,
        Err(error) => {
            eprintln!("droidloom-supervisor: automatic ADB unavailable: {error}");
            None
        }
    };
    let parent_pid = unsafe { libc::getpid() };
    let mut command = droidloom_cpu_placement::command("ip");
    command
        .args(["netns", "exec", network.namespace(), "unshare"])
        .args([
            OsStr::new("--mount"),
            OsStr::new("--pid"),
            // Host-side setup helpers also need /proc to describe this PID
            // namespace (not only the procfs later exposed inside Android).
            OsStr::new("--mount-proc"),
            OsStr::new("--ipc"),
            OsStr::new("--uts"),
            OsStr::new("--fork"),
            OsStr::new("--kill-child=TERM"),
            OsStr::new("--forward-signals"),
            OsStr::new("--propagation=private"),
            entry_executable.as_os_str(),
            OsStr::new("development-enter"),
            OsStr::new("--spec"),
            spec_path.as_os_str(),
        ]);
    // If the lifecycle owner disappears even under SIGKILL or a crash,
    // terminate `unshare`; its own --kill-child contract then terminates
    // Android PID 1. Only deterministic network/path residue remains for the
    // next daemon start to recover.
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::getppid() != parent_pid {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "Droidloom lifecycle owner exited while spawning the cell",
                ));
            }
            Ok(())
        });
    }
    let child_result = command.spawn();
    match child_result {
        Ok(child) => {
            let adb = adb_client.and_then(|client| match AdbConnection::start(client) {
                Ok(connection) => Some(connection),
                Err(error) => {
                    eprintln!("droidloom-supervisor: automatic ADB registration failed: {error}");
                    None
                }
            });
            Ok(DevelopmentCell {
                spec: spec.clone(),
                child,
                network: Some(network),
                adb,
                cleaned: false,
            })
        }
        Err(source) => {
            let network_result = network.teardown();
            let runtime_result = cleanup_runtime_directory(spec);
            network_result?;
            runtime_result?;
            Err(io_error("execute networked unshare", source))
        }
    }
}

/// Remove deterministic host residue from a previously interrupted cell.
///
/// The namespace process has a parent-death signal tied to its lifecycle
/// owner, so recovery never selects processes by name. Only exact network
/// object names and the specification's exact ephemeral runtime path are
/// considered here; persistent Android data is never touched.
///
/// # Errors
///
/// Returns malformed-spec, network teardown, or runtime cleanup failures.
pub fn recover_development_cell(spec: &CellSpec) -> Result<(), DevelopmentError> {
    spec.validate()?;
    DevelopmentNetwork::recover(spec)?;
    cleanup_runtime_directory(spec)
}

fn signal_process(pid: u32, signal: libc::c_int) -> Result<(), DevelopmentError> {
    let pid = i32::try_from(pid).map_err(|_| {
        DevelopmentError::InvalidInput("namespace owner PID exceeds Linux pid_t".into())
    })?;
    let result = unsafe { libc::kill(pid, signal) };
    if result == 0 {
        Ok(())
    } else {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            Ok(())
        } else {
            Err(io_error("signal Android namespace owner", error))
        }
    }
}

fn start_idmap_namespace(
    spec: &CellSpec,
    shared: &[crate::SharedStorageDirectory],
    android_uid: u32,
    proc_root: &Path,
) -> Result<Option<Child>, DevelopmentError> {
    let Some(first) = shared.first() else {
        return Ok(None);
    };
    let uid_mapping = format!("{}:{android_uid}:1", spec.host_uid);
    let gid_mapping = format!("{}:{ANDROID_MEDIA_RW_ID}:1", first.host_gid);
    // Linux checks the creator's fsuid AND fsgid before applying setgid
    // inheritance. MediaProvider's primary GID must therefore be mapped too;
    // the collection's setgid bit still gives files the normal host group.
    let primary_gid_mapping = format!("{}:{android_uid}:1", spec.subordinate_gids.start);
    let parent_pid = unsafe { libc::getpid() };
    let mut command = droidloom_cpu_placement::command("unshare");
    command
        .args(["--user", "--map-users"])
        .arg(&uid_mapping)
        .arg("--map-groups")
        .arg(&gid_mapping)
        .arg("--map-groups")
        .arg(&primary_gid_mapping)
        .args(["sleep", "infinity"]);
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::getppid() != parent_pid {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "Droidloom lifecycle owner exited while creating storage idmap",
                ));
            }
            Ok(())
        });
    }
    let mut child = command
        .spawn()
        .map_err(|source| io_error("create shared-storage idmap namespace", source))?;
    if wait_for_idmap_namespace(
        &mut child,
        (spec.host_uid, first.host_gid),
        android_uid,
        spec.subordinate_gids.start,
        proc_root,
    )? {
        Ok(Some(child))
    } else {
        stop_idmap_namespace(&mut child)?;
        Err(DevelopmentError::InvalidInput(
            "shared-storage idmap namespace did not become ready".into(),
        ))
    }
}

fn wait_for_idmap_namespace(
    child: &mut Child,
    filesystem_owner: (u32, u32),
    android_uid: u32,
    primary_host_gid: u32,
    proc_root: &Path,
) -> Result<bool, DevelopmentError> {
    let proc = proc_root.join(child.id().to_string());
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if let Some(status) = child
            .try_wait()
            .map_err(|source| io_error("poll shared-storage idmap namespace", source))?
        {
            return Err(DevelopmentError::Command {
                program: "unshare --user for shared storage".into(),
                status,
            });
        }
        let user_ready = idmap_file_matches(&proc.join("uid_map"), filesystem_owner.0, android_uid);
        let group_ready =
            idmap_file_matches(
                &proc.join("gid_map"),
                filesystem_owner.1,
                ANDROID_MEDIA_RW_ID,
            ) && idmap_file_matches(&proc.join("gid_map"), primary_host_gid, android_uid);
        if user_ready && group_ready {
            return Ok(true);
        }
        thread::sleep(Duration::from_millis(10));
    }
    Ok(false)
}

fn idmap_file_matches(path: &Path, inside: u32, outside: u32) -> bool {
    fs::read_to_string(path).is_ok_and(|contents| {
        contents.lines().any(|line| {
            let ids = line
                .split_ascii_whitespace()
                .filter_map(|field| field.parse::<u32>().ok())
                .collect::<Vec<_>>();
            ids == [inside, outside, 1]
        })
    })
}

fn stop_idmap_namespace(child: &mut Child) -> Result<(), DevelopmentError> {
    if child
        .try_wait()
        .map_err(|source| {
            io_error(
                "poll shared-storage idmap namespace before stopping",
                source,
            )
        })?
        .is_none()
    {
        child
            .kill()
            .map_err(|source| io_error("stop shared-storage idmap namespace", source))?;
        child
            .wait()
            .map_err(|source| io_error("reap shared-storage idmap namespace", source))?;
    }
    Ok(())
}

fn cleanup_runtime_directory(spec: &CellSpec) -> Result<(), DevelopmentError> {
    crate::app_properties::cleanup(&spec.runtime_dir)
        .map_err(|source| io_error("remove Android application properties", source))?;
    crate::adb::cleanup(&spec.runtime_dir)
        .map_err(|source| io_error("remove private ADB configuration", source))?;
    for (path, context) in [
        (
            spec.runtime_dir.join("android-cmdline"),
            "remove Android development cmdline",
        ),
        (
            spec.runtime_dir.join("android-bootconfig"),
            "remove Android development bootconfig",
        ),
        (
            spec.runtime_dir.join("android-product.prop"),
            "remove Android product properties",
        ),
    ] {
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(source) => return Err(io_error(context, source)),
        }
    }
    let cgroup_policy = spec.runtime_dir.join("cgroup-policy");
    if cgroup_policy.exists() {
        for entry in
            fs::read_dir(&cgroup_policy).map_err(|e| io_error("read cgroup policy directory", e))?
        {
            let entry = entry.map_err(|e| io_error("read cgroup policy entry", e))?;
            fs::remove_file(entry.path())
                .map_err(|e| io_error("remove generated cgroup policy", e))?;
        }
        fs::remove_dir(cgroup_policy).map_err(|e| io_error("remove cgroup policy directory", e))?;
    }
    let private_sysctls = spec.runtime_dir.join(PRIVATE_SYSCTL_DIRECTORY);
    let private_net_core = private_sysctls.join(PRIVATE_NET_CORE_DIRECTORY);
    match fs::read_dir(&private_net_core) {
        Ok(entries) => {
            for entry in entries {
                let entry =
                    entry.map_err(|source| io_error("read private net.core entry", source))?;
                if !entry
                    .file_type()
                    .map_err(|source| io_error("stat private net.core entry", source))?
                    .is_file()
                {
                    return Err(DevelopmentError::InvalidInput(format!(
                        "unexpected non-file in private net.core directory: {}",
                        entry.path().display()
                    )));
                }
                fs::remove_file(entry.path())
                    .map_err(|source| io_error("remove private net.core entry", source))?;
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(source) => return Err(io_error("read private net.core directory", source)),
    }
    match fs::remove_file(private_sysctls.join("unprivileged_bpf_disabled")) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(source) => return Err(io_error("remove private unprivileged-BPF control", source)),
    }
    match fs::remove_dir(&private_net_core) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(source) => return Err(io_error("remove private net.core sysctl directory", source)),
    }
    match fs::remove_dir(&private_sysctls) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(source) => return Err(io_error("remove private Android sysctl directory", source)),
    }
    for (directory, context) in [
        ("root", "remove development runtime root"),
        (
            DEVELOPMENT_SYSTEM_LOWER,
            "remove development system lower directory",
        ),
    ] {
        match fs::remove_dir(spec.runtime_dir.join(directory)) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(source) => return Err(io_error(context, source)),
        }
    }
    match fs::remove_dir(&spec.runtime_dir) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(io_error("remove development runtime directory", source)),
    }
}

/// Assemble the already-private namespace and replace the current process
/// with Android init.
///
/// This is public only so the CLI can expose a hidden re-entry subcommand. It
/// must be invoked by [`run_development_cell`], not directly.
///
/// # Errors
///
/// Returns when a required mount/device operation or final `chroot` exec
/// fails. Successful execution never returns.
pub fn enter_development_cell(spec: &CellSpec) -> Result<(), DevelopmentError> {
    validate_development_inputs(spec)?;
    make_mounts_private()?;
    let root = spec.runtime_dir.join("root");
    let system_lower = spec.runtime_dir.join(DEVELOPMENT_SYSTEM_LOWER);
    mount_read_only_image(&spec.image_dir.join("images/system.img"), &system_lower)?;
    mount_synthetic_system_root(&system_lower, &root)?;
    mount_read_only_image(
        &crate::gapps::partition_image(spec, "system_ext"),
        &root.join("system_ext"),
    )?;
    mount_read_only_image(
        &crate::gapps::partition_image(spec, "product"),
        &root.join("product"),
    )?;
    mount_image("ext4", &spec.vendor_image, &root.join("vendor"), true)?;

    // Android init and its services require SELinux context operations even
    // though the shared host kernel deliberately has no Android policy. Keep
    // the development-only bridge at a system-visible preload point until the
    // base image owns a dedicated system-library pathname for it.
    let packaged_selinux_compat = spec
        .android_file_overrides
        .iter()
        .find(|file_override| file_override.target == Path::new(ANDROID_SELINUX_COMPAT_TARGET))
        .map(|file_override| file_override.source.as_path());
    let image_selinux_compat = root.join(ANDROID_SELINUX_COMPAT_SOURCE);
    bind_mount(
        packaged_selinux_compat.unwrap_or(&image_selinux_compat),
        &root.join(ANDROID_SELINUX_COMPAT_PRELOAD.trim_start_matches('/')),
        false,
    )?;

    // An optional package-owned init carries the minimal shared-kernel
    // namespace adaptation without modifying the pinned system image. Either
    // that override mount or the fallback self-bind also gives init a stable
    // cell-owned mount identity before pivot_root.
    let init = root.join(ANDROID_INIT);
    if let Some(android_init) = &spec.android_init {
        bind_mount_read_only_executable(android_init, &init)?;
    } else {
        bind_mount(&init, &init, false)?;
    }
    for file_override in &spec.android_file_overrides {
        let relative_target = file_override.target.strip_prefix("/").map_err(|_| {
            DevelopmentError::InvalidInput(format!(
                "{} is not an absolute Android override target",
                file_override.target.display()
            ))
        })?;
        let target = root.join(relative_target);
        let metadata = fs::symlink_metadata(&target).map_err(|source| {
            io_error(
                &format!(
                    "stat Android file override target {}",
                    file_override.target.display()
                ),
                source,
            )
        })?;
        let directory = fs::metadata(&file_override.source)
            .map_err(|source| {
                io_error(
                    &format!(
                        "stat Android file override {}",
                        file_override.source.display()
                    ),
                    source,
                )
            })?
            .is_dir();
        if directory != metadata.is_dir() || !(metadata.is_file() || metadata.is_dir()) {
            return Err(DevelopmentError::InvalidInput(format!(
                "{} is not an existing Android partition {}",
                file_override.target.display(),
                if directory {
                    "directory"
                } else {
                    "regular file"
                }
            )));
        }
        bind_mount_read_only_executable(&file_override.source, &target)?;
    }

    for directory in &spec.android_runtime_directories {
        let relative_target = directory.target.strip_prefix("/").map_err(|_| {
            DevelopmentError::InvalidInput(format!(
                "{} is not an absolute Android runtime target",
                directory.target.display()
            ))
        })?;
        let target = root.join(relative_target);
        fs::create_dir_all(&target)
            .map_err(|source| io_error("create Android runtime directory target", source))?;
        bind_mount_read_only_executable(&directory.source, &target)?;
    }

    mount_host_storage(&spec.data_dir.join("data"), &root.join("data"))?;
    crate::gapps::prepare_data(spec, &root)
        .map_err(|error| DevelopmentError::InvalidInput(error.to_string()))?;
    crate::package_cache::invalidate_systemui_cache(&root)
        .map_err(|source| io_error("invalidate overlaid SystemUI package cache", source))?;
    mount_host_storage(&spec.data_dir.join("metadata"), &root.join("metadata"))?;
    mount_proc_and_sys(spec, &root)?;
    create_private_dev(spec, &root)?;
    crate::authentication::prepare(spec, &root)
        .map_err(|source| io_error("prepare Android authentication secrets", source))?;
    mount_first_stage_runtime_filesystems(&root)?;
    bind_boot_parameters(spec, &root)?;
    let controllers = crate::cpu_placement::prepare(&root, spec.id().as_str())
        .map_err(|source| io_error("prepare private Android CPU placement hierarchy", source))?;
    bind_development_cgroups(spec, &root, &controllers)?;
    if let Some(policy) = &controllers.cpu_policy {
        bind_cpu_policy(spec, &root, policy)?;
    }
    for (source, target) in crate::app_properties::prepare(&root, &spec.runtime_dir)
        .map_err(|source| io_error("prepare Android application properties", source))?
    {
        bind_mount(&source, &target, true)?;
    }
    bind_adb_configuration(spec, &root)?;
    bind_product_properties(spec, &root)?;
    bind_root_servicemanager(spec, &root)?;

    // The reusable base is a system-as-root image, but Droidloom has already
    // supplied the mounts that Android first-stage init normally constructs.
    // Entering first stage here would try to mount /dev, /proc, /sys and the
    // partitions a second time, which Android treats as fatal.
    arm_android_init_parent_death()?;
    pivot_into_cell_root(&root)?;
    let mut init = droidloom_cpu_placement::command("/init");
    // Do not inherit a host renderer override accidentally. The explicit
    // pairing applies to all Android children, including mapper clients.
    init.env_remove("MESA_LOADER_DRIVER_OVERRIDE")
        .env_remove("vendor.minigbm.allocator")
        .env_remove("vendor.minigbm.vram");
    if spec.graphics_backend != crate::GraphicsBackend::Drm {
        init.env("vendor.minigbm.allocator", "dma_heap_images");
    }
    // The forwarded driver lists the host GPU's layouts the compositor
    // imports; buffers nobody maps then lie in VRAM in one of them.
    if spec.graphics_backend == crate::GraphicsBackend::Forwarding {
        init.env("vendor.minigbm.vram", "vulkan");
    }
    if spec.graphics_backend == crate::GraphicsBackend::KgslDmaHeap {
        init.env("MESA_LOADER_DRIVER_OVERRIDE", "zink");
    }
    crate::filesystem_guard::install()
        .map_err(|error| io_error("install shared-filesystem protection", error))?;
    let error = std::os::unix::process::CommandExt::exec(
        init.arg("second_stage")
            // Android's mount-namespace decision runs before ro.boot.*
            // properties are guaranteed to exist. Give the package-owned
            // init adaptation an unambiguous early-boot marker instead.
            .env(DROIDLOOM_CELL_MARKER_ENV, "1")
            .env("PATH", "/system/bin:/system/xbin:/vendor/bin:/vendor/xbin")
            .env("ANDROID_ROOT", "/system")
            .env("ANDROID_DATA", "/data")
            .env("ANDROID_STORAGE", "/storage")
            .env("ASEC_MOUNTPOINT", "/mnt/asec")
            .env("LOOP_MOUNTPOINT", "/mnt/obb")
            .env("LD_PRELOAD", ANDROID_SELINUX_COMPAT_PRELOAD),
    );
    Err(io_error("exec Android init", error))
}

fn arm_android_init_parent_death() -> Result<(), DevelopmentError> {
    // `unshare --kill-child` cannot run its exit handler after SIGKILL. Keep a
    // kernel-enforced link from Android PID 1 to that exact namespace owner so
    // the forced-stop path cannot orphan a cell. The setting survives exec of
    // the non-set-ID Android init binary. The namespace child cannot compare
    // its parent PID before and after this call: Linux deliberately reports
    // PPID 0 when that parent is outside the child's PID namespace.
    let result = unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) };
    if result != 0 {
        return Err(io_error(
            "arm Android init parent-death signal",
            io::Error::last_os_error(),
        ));
    }
    Ok(())
}

fn pivot_into_cell_root(root: &Path) -> Result<(), DevelopmentError> {
    // chroot leaves the executable's kernel path anchored in the host mount
    // tree, which makes Android init's /proc/self/exe self-reexec escape the
    // chroot path and fail. Use a conventional, distinct put_old directory so
    // the new root remains valid when Android clones and switches between its
    // bootstrap and default APEX mount namespaces.
    fs::create_dir(root.join(".oldroot"))
        .map_err(|source| io_error("create old-root pivot directory", source))?;
    std::env::set_current_dir(root)
        .map_err(|source| io_error("enter Android root mount", source))?;
    let dot = CString::new(".").expect("static path has no NUL");
    let old_root = CString::new(".oldroot").expect("static path has no NUL");
    let pivot_result = unsafe {
        libc::syscall(
            libc::SYS_pivot_root,
            dot.as_ptr().cast::<libc::c_char>(),
            old_root.as_ptr().cast::<libc::c_char>(),
        )
    };
    if pivot_result != 0 {
        return Err(io_error(
            "pivot into Android root",
            io::Error::last_os_error(),
        ));
    }
    std::env::set_current_dir("/")
        .map_err(|source| io_error("enter pivoted Android root", source))?;
    let old_root = CString::new("/.oldroot").expect("static path has no NUL");
    let unmount_result = unsafe { libc::umount2(old_root.as_ptr(), libc::MNT_DETACH) };
    if unmount_result != 0 {
        return Err(io_error("detach old host root", io::Error::last_os_error()));
    }
    fs::remove_dir("/.oldroot")
        .map_err(|source| io_error("remove old-root pivot directory", source))
}

fn prepare_host_directories(spec: &CellSpec) -> Result<(), DevelopmentError> {
    if spec.runtime_dir.exists() {
        return Err(DevelopmentError::InvalidInput(format!(
            "runtime directory {} already exists; choose a fresh path",
            spec.runtime_dir.display()
        )));
    }
    for (directory, context) in [
        ("root", "create development runtime root"),
        (
            DEVELOPMENT_SYSTEM_LOWER,
            "create development system lower directory",
        ),
    ] {
        fs::create_dir_all(spec.runtime_dir.join(directory))
            .map_err(|source| io_error(context, source))?;
    }
    set_mode(&spec.runtime_dir, 0o700)?;
    let data = fs::metadata(&spec.data_dir)
        .map_err(|source| io_error("stat Android storage parent", source))?;
    if !data.is_dir() {
        return Err(DevelopmentError::InvalidInput(format!(
            "{} is not a directory",
            spec.data_dir.display()
        )));
    }
    set_mode(&spec.data_dir, 0o700)
}

fn make_mounts_private() -> Result<(), DevelopmentError> {
    mount_syscall(None, Path::new("/"), None, "rprivate")
}

fn mount_image(
    filesystem: &str,
    image: &Path,
    target: &Path,
    read_only: bool,
) -> Result<(), DevelopmentError> {
    let access = if read_only { "ro" } else { "rw" };
    run_os(
        "mount",
        [
            OsString::from("-t"),
            OsString::from(filesystem),
            OsString::from("-o"),
            OsString::from(format!("loop,{access},nodev,nosuid")),
            image.as_os_str().to_owned(),
            target.as_os_str().to_owned(),
        ],
    )
}

fn mount_read_only_image(image: &Path, target: &Path) -> Result<(), DevelopmentError> {
    mount_image(
        detect_read_only_filesystem(image)?.mount_name(),
        image,
        target,
        true,
    )
}

fn mount_host_storage(source: &Path, target: &Path) -> Result<(), DevelopmentError> {
    bind_mount(source, target, false)?;
    remount(target, "bind,rw,nodev,nosuid")
}

fn mount_synthetic_system_root(lower: &Path, target: &Path) -> Result<(), DevelopmentError> {
    // Keep the official system image immutable while giving the cell a
    // writable root directory. Every official top-level entry is either
    // recreated as its original symlink or exposed through a read-only
    // superblock bind mount; new compatibility names live only on tmpfs.
    mount_tmpfs(target, "mode=0755,uid=0,gid=0,nosuid,nodev")?;
    let mut entries = fs::read_dir(lower)
        .map_err(|source| io_error("list development system root", source))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| io_error("read development system root entry", source))?;
    entries.sort_by_key(std::fs::DirEntry::file_name);

    for entry in entries {
        let source = entry.path();
        let destination = target.join(entry.file_name());
        let metadata = fs::symlink_metadata(&source)
            .map_err(|source| io_error("stat development system root entry", source))?;
        let file_type = metadata.file_type();
        if file_type.is_symlink() {
            let link = fs::read_link(&source)
                .map_err(|source| io_error("read development system root link", source))?;
            symlink(link, destination)
                .map_err(|source| io_error("recreate development system root link", source))?;
        } else if file_type.is_dir() {
            fs::create_dir(&destination)
                .map_err(|source| io_error("create development system root directory", source))?;
            bind_mount(&source, &destination, false)?;
        } else if file_type.is_file() {
            create_mount_target(&destination)?;
            bind_mount(&source, &destination, false)?;
        } else {
            return Err(DevelopmentError::InvalidInput(format!(
                "unsupported system root entry {}",
                source.display()
            )));
        }
    }

    // Aston's host boot root is named /newroot, and procfs retains that name
    // when rendering a self-bound executable. Resolve it wholly inside the
    // cell instead of teaching Android about a host-specific runtime path.
    symlink("/", target.join("newroot"))
        .map_err(|source| io_error("create cell boot-root alias", source))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReadOnlyFilesystem {
    Erofs,
    Ext4,
}

impl ReadOnlyFilesystem {
    const fn mount_name(self) -> &'static str {
        match self {
            Self::Erofs => "erofs",
            Self::Ext4 => "ext4",
        }
    }
}

fn detect_read_only_filesystem(path: &Path) -> Result<ReadOnlyFilesystem, DevelopmentError> {
    if starts_with_magic(path, EROFS_MAGIC_OFFSET, &EROFS_MAGIC)? {
        Ok(ReadOnlyFilesystem::Erofs)
    } else if starts_with_magic(path, EXT4_MAGIC_OFFSET, &EXT4_MAGIC)? {
        Ok(ReadOnlyFilesystem::Ext4)
    } else {
        Err(DevelopmentError::InvalidInput(format!(
            "{} is neither an EROFS nor raw ext4 image",
            path.display()
        )))
    }
}

fn bind_mount(source: &Path, target: &Path, read_only: bool) -> Result<(), DevelopmentError> {
    mount_syscall(Some(source.as_os_str()), target, None, "bind")?;
    if read_only {
        remount(target, "bind,ro,nosuid,nodev,noexec")?;
    }
    Ok(())
}

fn bind_mount_read_only_executable(source: &Path, target: &Path) -> Result<(), DevelopmentError> {
    bind_mount(source, target, false)?;
    remount(target, "bind,ro,nosuid,nodev")
}

// A cell start makes a few hundred mounts and device nodes. Spawning mount(8)
// or mknod(1) for each cost about 2 ms, over a second before Android started,
// so these call the kernel directly with mount(8)-style option strings. Loop
// images and idmapped binds still use mount(8), which sets those up.
fn mount_syscall(
    source: Option<&OsStr>,
    target: &Path,
    filesystem: Option<&str>,
    options: &str,
) -> Result<(), DevelopmentError> {
    let mut flags: libc::c_ulong = 0;
    let mut data = Vec::new();
    for option in options.split(',').filter(|option| !option.is_empty()) {
        match option {
            "ro" => flags |= libc::MS_RDONLY,
            "rw" => {}
            "nosuid" => flags |= libc::MS_NOSUID,
            "nodev" => flags |= libc::MS_NODEV,
            "noexec" => flags |= libc::MS_NOEXEC,
            "bind" => flags |= libc::MS_BIND,
            "remount" => flags |= libc::MS_REMOUNT,
            "rprivate" => flags |= libc::MS_REC | libc::MS_PRIVATE,
            _ => data.push(option),
        }
    }
    let source = source
        .map(|source| c_string(source.as_bytes()))
        .transpose()?;
    let target_c = c_string(target.as_os_str().as_bytes())?;
    let filesystem = filesystem
        .map(|name| c_string(name.as_bytes()))
        .transpose()?;
    let data = (!data.is_empty())
        .then(|| c_string(data.join(",").as_bytes()))
        .transpose()?;
    // SAFETY: every pointer is null or a NUL-terminated string that outlives
    // the call; the kernel copies them.
    let result = unsafe {
        libc::mount(
            source
                .as_ref()
                .map_or(std::ptr::null(), |value| value.as_ptr()),
            target_c.as_ptr(),
            filesystem
                .as_ref()
                .map_or(std::ptr::null(), |value| value.as_ptr()),
            flags,
            data.as_ref()
                .map_or(std::ptr::null(), |value| value.as_ptr().cast()),
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io_error(
            &format!("mount {} ({options})", target.display()),
            io::Error::last_os_error(),
        ))
    }
}

fn mount_filesystem(
    filesystem: &str,
    target: &Path,
    options: &str,
) -> Result<(), DevelopmentError> {
    mount_syscall(
        Some(OsStr::new(filesystem)),
        target,
        Some(filesystem),
        options,
    )
}

fn remount(target: &Path, options: &str) -> Result<(), DevelopmentError> {
    mount_syscall(None, target, None, &format!("remount,{options}"))
}

fn c_string(bytes: &[u8]) -> Result<CString, DevelopmentError> {
    CString::new(bytes).map_err(|_| {
        DevelopmentError::InvalidInput(format!(
            "{} contains a NUL byte",
            String::from_utf8_lossy(bytes)
        ))
    })
}

/// Shares `spec`'s folders into the running cell whose Android init is host
/// process `init`, below Android's `/data/media/0`.
///
/// The folders are idmapped to `MediaProvider`'s UID, which Android records
/// on its first start, so they join the cell while it runs: the mounts are
/// made detached here and attached in Android init's mount namespace.
/// Android shares `/data`, so a mount attached after vold's lower view or an
/// app's namespace exists still reaches them. `setns` needs a
/// single-threaded caller, so droidloomd runs this as its own process.
///
/// # Errors
///
/// Fails before Android has written its package list and made user 0's
/// storage, and when a folder cannot be mounted.
pub fn share_storage_into_cell(spec: &CellSpec, init: u32) -> Result<(), DevelopmentError> {
    // Resolved once: the user's folders are found again at each start.
    let shared = spec.shared_storage();
    if shared.is_empty() {
        return Ok(());
    }
    let android_uid = installed_media_provider_uid(&spec.data_dir)?.ok_or_else(|| {
        DevelopmentError::InvalidInput("Android has not written its package list yet".into())
    })?;
    let storage = fs::metadata(spec.data_dir.join(ANDROID_SHARED_STORAGE_ROOT))
        .map_err(|source| io_error("stat Android shared storage on the host", source))?;

    let proc_root = Path::new("/proc");
    let Some(mut namespace) = start_idmap_namespace(spec, &shared, android_uid, proc_root)? else {
        return Ok(());
    };
    let trees = File::open(proc_root.join(namespace.id().to_string()).join("ns/user"))
        .map_err(|source| io_error("open shared-storage idmap", source))
        .and_then(|userns| {
            shared
                .iter()
                .map(|directory| idmapped_tree(&directory.source, &userns))
                .collect::<Result<Vec<_>, _>>()
        });
    // The detached mounts keep the mapping once its namespace is gone.
    let cleanup = stop_idmap_namespace(&mut namespace);
    let trees = trees?;
    cleanup?;

    let namespace = File::open(format!("/proc/{init}/ns/mnt"))
        .map_err(|source| io_error("open Android's mount namespace", source))?;
    if unsafe { libc::setns(namespace.as_raw_fd(), libc::CLONE_NEWNS) } != 0 {
        return Err(io_error(
            "enter Android's mount namespace",
            io::Error::last_os_error(),
        ));
    }
    // Only Android's own /data reaches this exact folder; before Android init
    // has pivoted, the same path would name a host folder.
    let media_root = Path::new("/").join(ANDROID_SHARED_STORAGE_ROOT);
    let target = fs::metadata(&media_root)
        .map_err(|source| io_error("stat Android shared storage in the cell", source))?;
    if (target.dev(), target.ino()) != (storage.dev(), storage.ino()) {
        return Err(DevelopmentError::InvalidInput(
            "Android's /data/media/0 is not this cell's storage".into(),
        ));
    }
    for (directory, tree) in shared.iter().zip(&trees) {
        let target = media_root.join(&directory.android_name);
        fs::create_dir_all(&target)
            .map_err(|source| io_error("create shared-storage mountpoint", source))?;
        set_mode(&target, 0o770)?;
        set_owner(&target, ANDROID_MEDIA_RW_ID, ANDROID_MEDIA_RW_ID)?;
        attach_tree(tree, &target)?;
    }
    Ok(())
}

/// `MediaProvider`'s UID from Android's package list; none before Android's
/// first start writes the list.
fn installed_media_provider_uid(root: &Path) -> Result<Option<u32>, DevelopmentError> {
    match fs::read_to_string(root.join("data/system/packages.list")) {
        Ok(packages) => media_provider_uid(&packages).map(Some),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(io_error(
            "read initialized Android package ownership for shared storage",
            source,
        )),
    }
}

fn media_provider_uid(packages: &str) -> Result<u32, DevelopmentError> {
    for line in packages.lines() {
        let mut fields = line.split_ascii_whitespace();
        if fields.next() == Some(MEDIA_PROVIDER_PACKAGE) {
            return fields
                .next()
                .and_then(|uid| uid.parse::<u32>().ok())
                .filter(|uid| (10_000..100_000).contains(uid))
                .ok_or_else(|| {
                    DevelopmentError::InvalidInput("invalid MediaProvider package UID".into())
                });
        }
    }
    Err(DevelopmentError::InvalidInput(
        "shared storage requires an initialized MediaProvider package in Android data".into(),
    ))
}

/// A detached bind of `source` whose host owner and group read as Android's
/// through `userns`, without set-ID files, devices or programs.
fn idmapped_tree(source: &Path, userns: &File) -> Result<OwnedFd, DevelopmentError> {
    let path = c_string(source.as_os_str().as_bytes())?;
    let flags = libc::OPEN_TREE_CLONE | libc::OPEN_TREE_CLOEXEC | libc::AT_SYMLINK_NOFOLLOW as u32;
    let fd = unsafe { libc::syscall(libc::SYS_open_tree, libc::AT_FDCWD, path.as_ptr(), flags) };
    let fd = i32::try_from(fd)
        .ok()
        .filter(|fd| *fd >= 0)
        .ok_or_else(|| io_error("clone shared folder", io::Error::last_os_error()))?;
    // SAFETY: a fresh descriptor returned by the kernel, owned from here on.
    let tree = unsafe { OwnedFd::from_raw_fd(fd) };
    let mut attributes = libc::mount_attr {
        attr_set: libc::MOUNT_ATTR_IDMAP
            | libc::MOUNT_ATTR_NOSUID
            | libc::MOUNT_ATTR_NODEV
            | libc::MOUNT_ATTR_NOEXEC,
        attr_clr: libc::MOUNT_ATTR_RDONLY,
        propagation: 0,
        userns_fd: u64::try_from(userns.as_raw_fd()).unwrap_or(u64::MAX),
    };
    let result = unsafe {
        libc::syscall(
            libc::SYS_mount_setattr,
            tree.as_raw_fd(),
            c"".as_ptr(),
            libc::AT_EMPTY_PATH,
            &raw mut attributes,
            std::mem::size_of::<libc::mount_attr>(),
        )
    };
    if result < 0 {
        return Err(io_error("idmap shared folder", io::Error::last_os_error()));
    }
    Ok(tree)
}

fn attach_tree(tree: &OwnedFd, target: &Path) -> Result<(), DevelopmentError> {
    let target = c_string(target.as_os_str().as_bytes())?;
    let result = unsafe {
        libc::syscall(
            libc::SYS_move_mount,
            tree.as_raw_fd(),
            c"".as_ptr(),
            libc::AT_FDCWD,
            target.as_ptr(),
            libc::MOVE_MOUNT_F_EMPTY_PATH,
        )
    };
    if result < 0 {
        return Err(io_error("attach shared folder", io::Error::last_os_error()));
    }
    Ok(())
}

fn mount_proc_and_sys(spec: &CellSpec, root: &Path) -> Result<(), DevelopmentError> {
    let proc = root.join("proc");
    mount_filesystem("proc", &proc, "nosuid,nodev,noexec")?;
    // Keep procfs itself writable, but make the root view read-only with bind
    // mount flags. A plain `remount,ro` would make the proc superblock
    // read-only, preventing any writable child view from accepting sysctl
    // writes. Android's IpClient must tune the network-namespace-local
    // controls before it provisions an address.
    bind_mount(&proc, &proc, false)?;
    remount(&proc, "bind,ro,nosuid,nodev,noexec")?;

    // Linux isolates /proc/sys/net through the cell's network namespace, so
    // expose just that subtree as writable. Global controls and sysrq-trigger
    // remain behind the read-only parent proc mount.
    let network_sysctls = proc.join("sys/net");
    bind_mount(&network_sysctls, &network_sysctls, false)?;
    remount(&network_sysctls, "bind,rw,nosuid,nodev,noexec")?;
    project_private_bpf_sysctls(spec, root)?;

    // Do not expose the host sysfs device tree. ueventd coldboot would
    // otherwise recreate nodes for every host disk in Android's private /dev.
    // Project only the immutable identification metadata libdrm needs for the
    // one render node already selected by the cell contract. A private cgroup
    // namespace lets Android mount a namespaced cgroup2 view below this
    // synthetic tree without access to ancestor host cgroups.
    let sys = root.join("sys");
    mount_tmpfs(&sys, "mode=0755,nosuid,nodev,noexec")?;
    fs::create_dir_all(sys.join("fs/cgroup"))
        .map_err(|source| io_error("create private cgroup mount point", source))?;
    // Android normally inherits this mount point from the real sysfs tree and
    // mounts bpffs onto it from init.rc. Droidloom exposes a synthetic sysfs,
    // so create only the otherwise-missing mount point and let stock Android
    // own the bpffs mount and BPF program loading sequence.
    fs::create_dir_all(sys.join("fs/bpf"))
        .map_err(|source| io_error("create Android BPF mount point", source))?;
    // vold mounts fusectl here before constructing Android's emulated-storage
    // FUSE view. The host sysfs normally supplies this directory, but the cell
    // intentionally receives a synthetic sysfs with only explicit kernel
    // interfaces.
    fs::create_dir_all(sys.join("fs/fuse/connections"))
        .map_err(|source| io_error("create Android fusectl mount point", source))?;
    project_loop_sysfs(&sys)?;
    project_power_supply_sysfs(&sys)?;
    project_cpu_sysfs(&sys)?;
    let render_metadata = read_render_node_sysfs_metadata(&spec.render_node)?;
    publish_render_node_sysfs_metadata(&sys, &render_metadata)?;
    remount(&sys, "ro,nosuid,nodev,noexec")
}

// Bionic reads the CPU masks for get_nprocs()/sysconf; Unity also classifies
// workers using capacity and maximum frequency. Missing masks silently make
// Android report one CPU. Bind only these discovery attributes read-only:
// neither hotplug/frequency controls nor uevent files belong in the cell.
fn discover_cpu_files(root: &Path) -> Result<Vec<(PathBuf, PathBuf)>, DevelopmentError> {
    let mut files = Vec::new();
    for name in ["possible", "present", "online"] {
        let source = root.join(name);
        if !fs::metadata(&source)
            .map_err(|error| io_error("read required CPU discovery attribute", error))?
            .is_file()
        {
            return Err(DevelopmentError::InvalidInput(format!(
                "CPU discovery attribute {} is not a file",
                source.display()
            )));
        }
        files.push((source, PathBuf::from(name)));
    }
    for entry in fs::read_dir(root).map_err(|error| io_error("read host CPU sysfs", error))? {
        let entry = entry.map_err(|error| io_error("read host CPU entry", error))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(index) = name.strip_prefix("cpu") else {
            continue;
        };
        if index.is_empty() || !index.bytes().all(|byte| byte.is_ascii_digit()) {
            continue;
        }
        // Optional on homogeneous hosts or kernels without cpufreq/capacity.
        for attribute in ["cpu_capacity", "cpufreq/cpuinfo_max_freq"] {
            let source = entry.path().join(attribute);
            match fs::metadata(&source) {
                Ok(metadata) if metadata.is_file() => {
                    files.push((source, PathBuf::from(name).join(attribute)));
                }
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(io_error("read CPU discovery attribute", error)),
            }
        }
    }
    files.sort_by(|left, right| left.1.cmp(&right.1));
    Ok(files)
}

fn project_cpu_sysfs(sys: &Path) -> Result<(), DevelopmentError> {
    let target_root = sys.join("devices/system/cpu");
    fs::create_dir_all(&target_root)
        .map_err(|error| io_error("create synthetic CPU sysfs", error))?;
    for (source, relative) in discover_cpu_files(Path::new("/sys/devices/system/cpu"))? {
        let target = target_root.join(relative);
        fs::create_dir_all(target.parent().expect("CPU attribute has a parent"))
            .map_err(|error| io_error("create CPU discovery directory", error))?;
        create_mount_target(&target)?;
        bind_mount(&source, &target, true)?;
    }
    Ok(())
}

fn discover_power_supply_files(root: &Path) -> Result<Vec<(PathBuf, PathBuf)>, DevelopmentError> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(io_error("read host power-supply sysfs", error)),
    };
    let mut files = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| io_error("read host power-supply entry", error))?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let source = entry.path();
        let Ok(kind) = fs::read_to_string(source.join("type")) else {
            continue;
        };
        if !POWER_SUPPLY_TYPES.contains(&kind.trim()) {
            continue;
        }
        for attribute in POWER_SUPPLY_ATTRIBUTES {
            let source_file = source.join(attribute);
            if fs::metadata(&source_file).is_ok_and(|metadata| metadata.is_file()) {
                files.push((source_file, PathBuf::from(&name).join(attribute)));
            }
        }
    }
    files.sort_by(|left, right| left.1.cmp(&right.1));
    Ok(files)
}

fn project_power_supply_sysfs(sys: &Path) -> Result<(), DevelopmentError> {
    let target_root = sys.join("class/power_supply");
    fs::create_dir_all(&target_root)
        .map_err(|error| io_error("create synthetic power-supply sysfs", error))?;
    for (source, relative) in discover_power_supply_files(Path::new("/sys/class/power_supply"))? {
        let target = target_root.join(relative);
        let directory = target
            .parent()
            .expect("power-supply attribute has a parent");
        fs::create_dir_all(directory)
            .map_err(|error| io_error("create projected power-supply directory", error))?;
        create_mount_target(&target)?;
        bind_mount(&source, &target, true)?;
    }
    Ok(())
}

fn project_private_bpf_sysctls(spec: &CellSpec, root: &Path) -> Result<(), DevelopmentError> {
    // These BPF loader controls are global in Linux rather than network-
    // namespaced. Android insists on writing its expected values at boot, but
    // a cell must never mutate the host's copies. Bind ephemeral files over
    // only those three procfs nodes so Android observes its own successful
    // writes while the host controls remain unchanged.
    let source_directory = spec.runtime_dir.join(PRIVATE_SYSCTL_DIRECTORY);
    fs::create_dir(&source_directory)
        .map_err(|source| io_error("create private Android sysctl directory", source))?;

    let unprivileged_bpf = source_directory.join("unprivileged_bpf_disabled");
    fs::write(&unprivileged_bpf, "0\n")
        .map_err(|error| io_error("create private unprivileged-BPF control", error))?;
    set_mode(&unprivileged_bpf, 0o600)?;
    bind_mount(
        &unprivileged_bpf,
        &root.join("proc/sys/kernel/unprivileged_bpf_disabled"),
        false,
    )?;

    // The kernel exposes these global JIT controls only in its initial network
    // namespace, so the target files do not exist in the cell's procfs. Mount
    // a minimal private net.core directory rather than exposing init_net.
    let net_core_source = source_directory.join(PRIVATE_NET_CORE_DIRECTORY);
    fs::create_dir(&net_core_source)
        .map_err(|error| io_error("create private net.core sysctl directory", error))?;
    for (name, initial_value) in PRIVATE_BPF_SYSCTLS {
        let source = net_core_source.join(name);
        fs::write(&source, initial_value)
            .map_err(|error| io_error("create private Android sysctl", error))?;
        set_mode(&source, 0o600)?;
    }
    bind_mount(&net_core_source, &root.join("proc/sys/net/core"), false)?;
    Ok(())
}

fn project_loop_sysfs(sys: &Path) -> Result<(), DevelopmentError> {
    // Bootstrap apexd mounts ext4 payloads through the private loop nodes in
    // /dev, then the normal apexd process reconstructs its active-package
    // database from /proc/mounts. That reconstruction resolves each loop's
    // backing file through /sys/block/loopN/loop/backing_file. Without this
    // metadata the APEX contents are mounted but Package Manager never scans
    // embedded APKs such as PermissionController.
    //
    // Project only Linux's virtual block subtree. Physical devices remain
    // absent, while the directory bind reflects loop devices allocated after
    // the cell enters Android. Stable synthetic /sys/block links expose only
    // the loop minors already admitted by the private /dev contract.
    let virtual_block = sys.join("devices/virtual/block");
    fs::create_dir_all(&virtual_block)
        .map_err(|source| io_error("create virtual block sysfs projection", source))?;
    bind_mount(
        Path::new("/sys/devices/virtual/block"),
        &virtual_block,
        true,
    )?;

    let block = sys.join("block");
    fs::create_dir(&block)
        .map_err(|source| io_error("create synthetic block sysfs directory", source))?;
    for minor in 0..DEVELOPMENT_LOOP_DEVICES {
        symlink(
            format!("../devices/virtual/block/loop{minor}"),
            block.join(format!("loop{minor}")),
        )
        .map_err(|source| io_error("create loop-device sysfs link", source))?;
    }
    Ok(())
}

#[derive(Debug, Eq, PartialEq)]
struct RenderNodeSysfsMetadata {
    major: u64,
    minor: u64,
    node_name: String,
    subsystem: String,
    uevent: String,
    pci_identity: Option<PciIdentity>,
}

#[derive(Debug, Eq, PartialEq)]
struct PciIdentity {
    revision: String,
    vendor: String,
    device: String,
    subsystem_vendor: String,
    subsystem_device: String,
}

fn read_render_node_sysfs_metadata(
    render_node: &Path,
) -> Result<RenderNodeSysfsMetadata, DevelopmentError> {
    let metadata = fs::metadata(render_node)
        .map_err(|source| io_error("stat render node for sysfs projection", source))?;
    if !metadata.file_type().is_char_device() {
        return Err(DevelopmentError::InvalidInput(format!(
            "{} is not a character device",
            render_node.display()
        )));
    }

    let (major, minor) = linux_device_numbers(metadata.rdev());
    if major != DRM_DEVICE_MAJOR || minor < DRM_RENDER_MINOR_BASE {
        return Err(DevelopmentError::InvalidInput(format!(
            "{} is not a DRM render node (device {major}:{minor})",
            render_node.display()
        )));
    }
    let expected_node_name = format!("renderD{minor}");
    if render_node.file_name().and_then(OsStr::to_str) != Some(&expected_node_name) {
        return Err(DevelopmentError::InvalidInput(format!(
            "{} does not match DRM device {major}:{minor} ({expected_node_name})",
            render_node.display()
        )));
    }

    let device = PathBuf::from(format!("/sys/dev/char/{major}:{minor}/device"));
    let subsystem_link = fs::read_link(device.join("subsystem"))
        .map_err(|source| io_error("read render-node sysfs subsystem", source))?;
    let subsystem = subsystem_link
        .file_name()
        .and_then(OsStr::to_str)
        .filter(|name| matches!(*name, "platform" | "spi" | "host1x" | "pci"))
        .ok_or_else(|| {
            DevelopmentError::InvalidInput(format!(
                "unsupported render-node sysfs subsystem {}",
                subsystem_link.display()
            ))
        })?
        .to_owned();

    let uevent = fs::read_to_string(device.join("uevent"))
        .map_err(|source| io_error("read render-node sysfs uevent", source))?;
    if uevent.len() > MAX_RENDER_NODE_UEVENT_BYTES || uevent.as_bytes().contains(&0) {
        return Err(DevelopmentError::InvalidInput(format!(
            "render-node sysfs uevent is not bounded text ({} bytes)",
            uevent.len()
        )));
    }
    let pci_identity = if subsystem == "pci" {
        Some(PciIdentity {
            revision: read_pci_identity(&device, "revision")?,
            vendor: read_pci_identity(&device, "vendor")?,
            device: read_pci_identity(&device, "device")?,
            subsystem_vendor: read_pci_identity(&device, "subsystem_vendor")?,
            subsystem_device: read_pci_identity(&device, "subsystem_device")?,
        })
    } else {
        None
    };

    Ok(RenderNodeSysfsMetadata {
        major,
        minor,
        node_name: expected_node_name,
        subsystem,
        uevent,
        pci_identity,
    })
}

fn read_pci_identity(device: &Path, name: &str) -> Result<String, DevelopmentError> {
    let value = fs::read_to_string(device.join(name))
        .map_err(|source| io_error(&format!("read render-node PCI {name}"), source))?;
    let hex = value.trim().strip_prefix("0x").ok_or_else(|| {
        DevelopmentError::InvalidInput(format!(
            "render-node PCI {name} is not a hexadecimal sysfs value"
        ))
    })?;
    if hex.is_empty() || hex.len() > 8 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(DevelopmentError::InvalidInput(format!(
            "render-node PCI {name} is not a bounded hexadecimal sysfs value"
        )));
    }
    Ok(format!("0x{}\n", hex.to_ascii_lowercase()))
}

fn publish_render_node_sysfs_metadata(
    sys: &Path,
    metadata: &RenderNodeSysfsMetadata,
) -> Result<(), DevelopmentError> {
    let device = sys
        .join("dev/char")
        .join(format!("{}:{}", metadata.major, metadata.minor))
        .join("device");
    fs::create_dir_all(&device)
        .map_err(|source| io_error("create render-node sysfs projection", source))?;
    fs::create_dir_all(device.join("drm").join(&metadata.node_name))
        .map_err(|source| io_error("create selected DRM sysfs node", source))?;
    fs::create_dir_all(sys.join("bus").join(&metadata.subsystem))
        .map_err(|source| io_error("create render-node sysfs bus", source))?;
    symlink(
        Path::new("/sys/bus").join(&metadata.subsystem),
        device.join("subsystem"),
    )
    .map_err(|source| io_error("create render-node sysfs subsystem link", source))?;
    let uevent = device.join("uevent");
    fs::write(&uevent, &metadata.uevent)
        .map_err(|source| io_error("write render-node sysfs uevent", source))?;
    set_mode(&uevent, 0o444)?;
    if let Some(identity) = &metadata.pci_identity {
        for (name, value) in [
            ("revision", identity.revision.as_str()),
            ("vendor", identity.vendor.as_str()),
            ("device", identity.device.as_str()),
            ("subsystem_vendor", identity.subsystem_vendor.as_str()),
            ("subsystem_device", identity.subsystem_device.as_str()),
        ] {
            let attribute = device.join(name);
            fs::write(&attribute, value)
                .map_err(|source| io_error(&format!("write render-node PCI {name}"), source))?;
            set_mode(&attribute, 0o444)?;
        }
    }
    Ok(())
}

const fn linux_device_numbers(device: u64) -> (u64, u64) {
    let major = ((device >> 8) & 0x0fff) | ((device >> 32) & 0xffff_f000);
    let minor = (device & 0x00ff) | ((device >> 12) & 0xffff_ff00);
    (major, minor)
}

fn mount_first_stage_runtime_filesystems(root: &Path) -> Result<(), DevelopmentError> {
    // These tmpfs mounts are normally created by Android first-stage init.
    // Droidloom enters second stage after assembling the immutable partitions,
    // so reproduce only the writable runtime mount points that second stage
    // relies on rather than running Android's partition mounter again.
    mount_tmpfs(
        &root.join("mnt"),
        "mode=0755,uid=0,gid=1000,nosuid,nodev,noexec",
    )?;
    for directory in ["vendor", "product"] {
        fs::create_dir(root.join("mnt").join(directory))
            .map_err(|source| io_error("create Android mount staging directory", source))?;
    }
    for directory in ["debug_ramdisk", "second_stage_resources"] {
        mount_tmpfs(
            &root.join(directory),
            "mode=0755,uid=0,gid=0,nosuid,nodev,noexec",
        )?;
    }
    Ok(())
}

fn mount_tmpfs(target: &Path, options: &str) -> Result<(), DevelopmentError> {
    mount_filesystem("tmpfs", target, options)
}

// Linux reserves a power-of-two minor-number range for every loop device
// when loop.max_part is enabled. For example loop6 is 7:48 with max_part=7;
// 7:6 would address a partition of loop0 instead of the requested whole disk.
fn loop_device_minor(index: u32, max_part: u32) -> Result<u32, DevelopmentError> {
    max_part
        .checked_add(1)
        .and_then(u32::checked_next_power_of_two)
        .and_then(|stride| index.checked_mul(stride))
        .filter(|minor| *minor < (1 << 20))
        .ok_or_else(|| {
            DevelopmentError::InvalidInput(format!(
                "loop device {index} with max_part={max_part} exceeds Linux minor-number range"
            ))
        })
}

// Uevent multicast reaches the cell even though its sysfs is synthetic.
// Without a sealed input directory, ueventd recreates Bluetooth/USB evdev
// nodes on hotplug and InputReader bypasses host focus (and doubles forwarded
// keys). All physical input belongs to the host compositor; the synthetic
// Host Keyboard and targeted injection need no device node here.
fn mount_private_input(dev: &Path) -> Result<(), DevelopmentError> {
    let input = dev.join("input");
    fs::create_dir_all(&input)
        .map_err(|source| io_error("create sealed Android input directory", source))?;
    mount_tmpfs(
        &input,
        "ro,mode=0755,nosuid,nodev,noexec,size=4k,nr_inodes=1",
    )
}

#[allow(clippy::too_many_lines)]
fn create_private_dev(spec: &CellSpec, root: &Path) -> Result<(), DevelopmentError> {
    let dev = root.join("dev");
    mount_tmpfs(&dev, "mode=0755,nosuid")?;
    mount_private_input(&dev)?;
    for directory in ["pts", "binderfs", "block", "dri", "socket/droidloom"] {
        fs::create_dir_all(dev.join(directory))
            .map_err(|source| io_error("create private device directory", source))?;
    }
    for (name, major, minor, mode) in [
        ("null", 1, 3, "666"),
        ("zero", 1, 5, "666"),
        ("full", 1, 7, "666"),
        ("random", 1, 8, "666"),
        ("urandom", 1, 9, "666"),
        ("kmsg", 1, 11, "600"),
        ("kmsg_debug", 1, 11, "622"),
        ("console", 5, 1, "600"),
        ("tty", 5, 0, "666"),
        // vold opens this node to construct /storage/emulated for the Android
        // user. The node references the shared kernel's FUSE driver while the
        // mounted filesystem and daemon remain inside the cell namespaces.
        ("fuse", 10, 229, "666"),
    ] {
        make_device_node(&dev.join(name), "c", major, minor, mode)?;
    }

    // The pinned CI base contains ext4-payload APEX packages. apexd obtains
    // unused loop minors through loop-control; provide only loop devices, not
    // the host block-device tree that ueventd would otherwise recreate.
    make_device_node(&dev.join("loop-control"), "c", 10, 237, "600")?;
    let loop_max_part = fs::read_to_string("/sys/module/loop/parameters/max_part")
        .map_err(|error| io_error("read host loop partition geometry", error))?
        .trim()
        .parse::<u32>()
        .map_err(|error| {
            DevelopmentError::InvalidInput(format!("invalid host loop max_part: {error}"))
        })?;
    for index in 0..DEVELOPMENT_LOOP_DEVICES {
        make_device_node(
            &dev.join("block").join(format!("loop{index}")),
            "b",
            7,
            loop_device_minor(index, loop_max_part)?,
            "600",
        )?;
    }
    mount_filesystem(
        "devpts",
        &dev.join("pts"),
        "newinstance,ptmxmode=0666,mode=0620,nosuid,noexec",
    )?;
    symlink("pts/ptmx", dev.join("ptmx")).map_err(|source| io_error("create ptmx link", source))?;
    for (link, target) in [
        ("fd", "/proc/self/fd"),
        ("stdin", "/proc/self/fd/0"),
        ("stdout", "/proc/self/fd/1"),
        ("stderr", "/proc/self/fd/2"),
    ] {
        symlink(target, dev.join(link))
            .map_err(|source| io_error("create standard device link", source))?;
    }

    mount_filesystem("binder", &dev.join("binderfs"), "")?;
    let control = dev.join("binderfs/binder-control");
    for name in ["binder", "hwbinder", "vndbinder"] {
        let binder = dev.join("binderfs").join(name);
        match fs::symlink_metadata(&binder) {
            Ok(metadata) if metadata.file_type().is_char_device() => {}
            Ok(_) => {
                return Err(DevelopmentError::InvalidInput(format!(
                    "{} exists but is not a Binder character device",
                    binder.display()
                )));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                add_binder_device(&control, name)?;
            }
            Err(source) => return Err(io_error("stat Binder device", source)),
        }
        set_mode(&binder, 0o666)?;
        symlink(Path::new("binderfs").join(name), dev.join(name))
            .map_err(|source| io_error("create Binder compatibility link", source))?;
    }

    // The host may enumerate the selected GPU as any render minor (for
    // example renderD129 when another GPU owns renderD128). Android sees one
    // private GPU, so give it the canonical first-render-node name with the
    // selected device's real major/minor. Use a private inode: host render-group
    // IDs do not match Android's users, and chmod on a bind mount would change
    // the host node. A render node has no display-master/modesetting authority.
    let render_target = dev.join("dri").join(ANDROID_RENDER_NODE);
    let render = read_render_node_sysfs_metadata(&spec.render_node)?;
    match spec.graphics_backend {
        crate::GraphicsBackend::Forwarding => eprintln!(
            "droidloom-supervisor: graphics: GPU forwarding (GPU choice {}); Android Vulkan and GLES run through droidloom-gpu-server, the cell's render node is {}",
            spec.gpu,
            spec.render_node.display()
        ),
        backend => eprintln!(
            "droidloom-supervisor: graphics: native {backend:?} on {} (GPU choice {}); GPU forwarding is off",
            spec.render_node.display(),
            spec.gpu
        ),
    }
    make_device_node(
        &render_target,
        "c",
        u32::try_from(render.major).expect("validated Linux device major"),
        u32::try_from(render.minor).expect("validated Linux device minor"),
        "666",
    )?;
    let host_node_name = spec.render_node.file_name().ok_or_else(|| {
        DevelopmentError::InvalidInput(format!(
            "{} has no render-node filename",
            spec.render_node.display()
        ))
    })?;
    if host_node_name != OsStr::new(ANDROID_RENDER_NODE) {
        // Older libdrm derives the node type from the real DRM minor and then
        // checks that the matching pathname exists.  Keep Android's stable
        // renderD128 path while giving that check a relative alias to the
        // exact same selected character device.  No second GPU is exposed.
        symlink(ANDROID_RENDER_NODE, dev.join("dri").join(host_node_name))
            .map_err(|source| io_error("create render-node minor compatibility alias", source))?;
    }

    let auxiliary = spec.graphics_backend.auxiliary_devices();
    for path in auxiliary {
        create_auxiliary_device(root, Path::new(path))?;
    }
    if !auxiliary.contains(&crate::MEDIA_DMA_HEAP) {
        // Media decoding needs the heap, the cell does not: start without it.
        if let Err(error) = create_auxiliary_device(root, Path::new(crate::MEDIA_DMA_HEAP)) {
            eprintln!("droidloom-supervisor: no media DMA heap, apps cannot decode media: {error}");
        }
    }

    let denial_target = root.join(ANDROID_DENIAL_SOCKET);
    create_mount_target(&denial_target)?;
    bind_mount(&spec.denial_socket, &denial_target, false)?;
    // Framework-only host channels. The host listener is reachable only below
    // its owner's 0700 directory. This guest mount directory restricts access
    // to root/system even when the host owner is not Android's UID 1000.
    let framework_host = root.join("dev/socket/droidloom-framework-host");
    fs::create_dir_all(&framework_host)
        .map_err(|source| io_error("create framework host channel directory", source))?;
    set_owner(&framework_host, 1000, 1000)?;
    set_mode(&framework_host, 0o700)?;
    let text_socket = spec.denial_socket.with_file_name("text-input.sock");
    if text_socket.exists() {
        if !fs::metadata(&text_socket)
            .map_err(|source| io_error("stat text-input socket", source))?
            .file_type()
            .is_socket()
        {
            return Err(DevelopmentError::InvalidInput(
                "text-input endpoint is not a socket".into(),
            ));
        }
        let target = framework_host.join("text-input");
        create_mount_target(&target)?;
        bind_mount(&text_socket, &target, false)?;
    }
    // Audio, the GPU server and the device broker (haptics, camera,
    // fingerprint) are separate user brokers. Their systemd-owned sockets
    // survive broker restarts, preserving the inodes bind-mounted into the
    // Android cell.
    for name in [
        "clipboard",
        "notifications",
        "audio",
        "gpu",
        "haptics",
        "camera",
        "fingerprint",
    ] {
        let socket = spec.denial_socket.with_file_name(format!("{name}.sock"));
        if socket.exists() {
            if !fs::symlink_metadata(&socket)
                .map_err(|source| io_error("stat desktop integration socket", source))?
                .file_type()
                .is_socket()
            {
                return Err(DevelopmentError::InvalidInput(format!(
                    "{name} endpoint is not a socket"
                )));
            }
            let target = if matches!(name, "audio" | "gpu" | "haptics" | "camera" | "fingerprint") {
                root.join("dev/socket/droidloom").join(name)
            } else {
                framework_host.join(name)
            };
            create_mount_target(&target)?;
            bind_mount(&socket, &target, false)?;
        }
    }
    Ok(())
}

fn create_auxiliary_device(root: &Path, path: &Path) -> Result<(), DevelopmentError> {
    let (major, minor) = auxiliary_graphics_device_numbers(path)?;
    let target = root.join(path.strip_prefix("/").expect("fixed absolute device path"));
    fs::create_dir_all(target.parent().expect("device parent"))
        .map_err(|source| io_error("create private graphics device directory", source))?;
    make_device_node(&target, "c", major, minor, "666")
}

fn auxiliary_graphics_device_numbers(path: &Path) -> Result<(u32, u32), DevelopmentError> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|source| io_error("stat auxiliary graphics device", source))?;
    if !metadata.file_type().is_char_device() {
        return Err(DevelopmentError::InvalidInput(format!(
            "{} must be a character device, not a symlink",
            path.display()
        )));
    }
    let (major, minor) = linux_device_numbers(metadata.rdev());
    let uevent = fs::read_to_string(format!("/sys/dev/char/{major}:{minor}/uevent"))
        .map_err(|source| io_error("read auxiliary graphics device identity", source))?;
    let expected = format!(
        "DEVNAME={}",
        path.strip_prefix("/dev")
            .expect("fixed device path")
            .display()
    );
    if !uevent.lines().any(|line| line == expected) {
        return Err(DevelopmentError::InvalidInput(format!(
            "{} does not match its kernel device identity",
            path.display()
        )));
    }
    Ok((
        u32::try_from(major).expect("Linux device major"),
        u32::try_from(minor).expect("Linux device minor"),
    ))
}

fn bind_boot_parameters(spec: &CellSpec, root: &Path) -> Result<(), DevelopmentError> {
    let cmdline = spec.runtime_dir.join("android-cmdline");
    fs::write(
        &cmdline,
        concat!(
            "androidboot.hardware=droidloom ",
            "androidboot.serialno=droidloom ",
            "androidboot.selinux=permissive ",
            "androidboot.debuggable=0 ",
            "androidboot.veritymode=enforcing ",
            "androidboot.verifiedbootstate=green\n"
        ),
    )
    .map_err(|source| io_error("write Android development cmdline", source))?;
    bind_mount(&cmdline, &root.join("proc/cmdline"), true)?;

    // A host booted by an Android bootloader with a bootconfig carries its own
    // androidboot values (hardware, serial number, verified boot state).
    // Android init prefers them to the cmdline, so they would replace the
    // cell's identity and the KeyMint root of trust: every existing key blob
    // would then be rejected, and system_server would die unlocking user 0.
    let bootconfig = root.join("proc/bootconfig");
    if !bootconfig.exists() {
        return Ok(());
    }
    let empty = spec.runtime_dir.join("android-bootconfig");
    fs::write(&empty, b"")
        .map_err(|source| io_error("write Android development bootconfig", source))?;
    bind_mount(&empty, &bootconfig, true)
}

fn bind_development_cgroups(
    spec: &CellSpec,
    root: &Path,
    controllers: &crate::cpu_placement::Controllers,
) -> Result<(), DevelopmentError> {
    // Android mounts or activates every controller it is told about, and each
    // one it cannot use fails again at every profile application. Describe
    // only the controllers the cell provides, and fit the profiles to them.
    let directory = spec.runtime_dir.join("cgroup-policy");
    fs::create_dir(&directory).map_err(|e| io_error("create cgroup policy directory", e))?;
    let mut config: serde_json::Value =
        serde_json::from_str(DEVELOPMENT_CGROUPS).expect("embedded cgroup configuration is valid");
    config["Cgroups"]
        .as_array_mut()
        .expect("controller list")
        .retain(|controller| {
            controller["Controller"]
                .as_str()
                .is_some_and(|name| controllers.provides(name))
        });
    let source = directory.join("cgroups.json");
    fs::write(
        &source,
        serde_json::to_vec_pretty(&config).expect("JSON value serializes"),
    )
    .map_err(|source| io_error("write Android development cgroup configuration", source))?;
    bind_mount(&source, &root.join(ANDROID_CGROUPS_TARGET), true)?;

    let mut targets = vec![root.join("system/etc/task_profiles.json")];
    // API-specific profiles load between system and vendor.
    let api_directory = root.join("system/etc/task_profiles");
    if api_directory.is_dir() {
        for entry in
            fs::read_dir(api_directory).map_err(|e| io_error("read API task profiles", e))?
        {
            let entry = entry.map_err(|e| io_error("read API task profile entry", e))?;
            if entry
                .file_name()
                .to_str()
                .is_some_and(|s| s.starts_with("task_profiles_") && s.ends_with(".json"))
            {
                targets.push(entry.path());
            }
        }
    }
    for relative in [
        "vendor/etc/task_profiles.json",
        "system_ext/etc/task_profiles.json",
    ] {
        let target = root.join(relative);
        if target.exists() {
            targets.push(target);
        }
    }
    let sources = targets
        .iter()
        .map(fs::read_to_string)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| io_error("read Android task profiles", e))?;
    let fitted = crate::cpu_placement::task_profiles(&sources, controllers)
        .map_err(|e| io_error("fit Android task profiles to the cell", e))?;
    for (index, (target, profiles)) in targets.iter().zip(fitted).enumerate() {
        let source = directory.join(format!("profiles-{index}.json"));
        fs::write(&source, profiles).map_err(|e| io_error("write Android task profiles", e))?;
        bind_mount(&source, target, true)?;
    }
    Ok(())
}

fn bind_adb_configuration(spec: &CellSpec, root: &Path) -> Result<(), DevelopmentError> {
    let directory = spec.runtime_dir.join(crate::adb::DIRECTORY);
    let keys = directory.join(crate::adb::KEYS_FILE);
    if !keys.exists() {
        return Ok(());
    }
    set_mode(&keys, 0o644)?;
    let target = root.join("adb_keys");
    match fs::symlink_metadata(&target) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            // This link was copied into the synthetic root. Replace it there,
            // rather than following an image's absolute link on the host.
            fs::remove_file(&target)
                .map_err(|source| io_error("replace Android ADB key link", source))?;
            create_mount_target(&target)?;
        }
        Ok(metadata) if metadata.is_file() => {}
        Ok(_) => {
            return Err(DevelopmentError::InvalidInput(
                "Android ADB key target is not a regular file".into(),
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => create_mount_target(&target)?,
        Err(source) => return Err(io_error("stat Android ADB key target", source)),
    }
    bind_mount(&keys, &target, true)?;

    // Product properties are read last by Android's boot property loader.
    // Override the pinned development image's authentication default while
    // retaining every unrelated property and immutable partition byte.
    let target = ["product/etc/build.prop", "product/build.prop"]
        .into_iter()
        .map(|path| root.join(path))
        .find(|path| path.is_file())
        .ok_or_else(|| {
            DevelopmentError::InvalidInput("Android product build properties are missing".into())
        })?;
    let properties = fs::read_to_string(&target)
        .map_err(|source| io_error("read Android product properties", source))?;
    let source = directory.join("build.prop");
    fs::write(&source, crate::adb::authenticated_properties(&properties))
        .map_err(|source| io_error("write authenticated ADB properties", source))?;
    set_mode(&source, 0o644)?;
    bind_mount(&source, &target, true)?;

    // Append after the classpath and CPU adaptations already mounted here.
    let target = root.join("system/etc/init/hw/init.rc");
    let mut init = fs::read_to_string(&target)
        .map_err(|source| io_error("read Android init for ADB", source))?;
    init.push_str(crate::adb::INIT_POLICY);
    let source = directory.join("init.rc");
    fs::write(&source, init).map_err(|source| io_error("write Android ADB init policy", source))?;
    set_mode(&source, 0o644)?;
    bind_mount(&source, &target, true)
}

// Application properties, tracing and host identity apply even without ADB
// authentication keys. Product defaults load after all other partitions.
fn bind_product_properties(spec: &CellSpec, root: &Path) -> Result<(), DevelopmentError> {
    let target = ["product/etc/build.prop", "product/build.prop"]
        .into_iter()
        .map(|path| root.join(path))
        .find(|path| path.is_file())
        .ok_or_else(|| {
            DevelopmentError::InvalidInput("Android product build properties are missing".into())
        })?;
    let original = fs::read_to_string(&target)
        .map_err(|source| io_error("read Android product properties", source))?;
    let properties = crate::tracing::container_properties(&original);
    let properties = crate::identity::product_properties(&properties, &crate::identity::host());
    let properties = crate::app_properties::product_properties(&properties);
    let properties = spec.graphics_backend.product_properties(&properties);
    let source = spec.runtime_dir.join("android-product.prop");
    fs::write(&source, properties)
        .map_err(|source| io_error("write Android product properties", source))?;
    set_mode(&source, 0o644)?;
    bind_mount(&source, &target, true)
}

/// Android's system user is the host user in a cell. servicemanager, which
/// tells every process where each service is, runs as root so that user can
/// neither change its answers nor kill it; its patch keeps the authentication
/// services' names for root.
fn bind_root_servicemanager(spec: &CellSpec, root: &Path) -> Result<(), DevelopmentError> {
    let target = root.join("system/etc/init/servicemanager.rc");
    let original =
        fs::read_to_string(&target).map_err(|e| io_error("read servicemanager service", e))?;
    let service = root_servicemanager(&original).ok_or_else(|| {
        DevelopmentError::InvalidInput("servicemanager.rc has no `user system` line".into())
    })?;
    let source = spec.runtime_dir.join("android-servicemanager.rc");
    fs::write(&source, service).map_err(|e| io_error("write servicemanager service", e))?;
    set_mode(&source, 0o644)?;
    bind_mount(&source, &target, true)
}

fn root_servicemanager(original: &str) -> Option<String> {
    const SYSTEM: &str = "\n    user system\n";
    original
        .contains(SYSTEM)
        .then(|| original.replacen(SYSTEM, "\n    user root\n", 1))
}

fn bind_cpu_policy(spec: &CellSpec, root: &Path, policy: &str) -> Result<(), DevelopmentError> {
    let directory = spec.runtime_dir.join("cgroup-policy");
    // Preserve the already projected init file, including the classpath and
    // shared-kernel adaptations. The appended on-init action follows AOSP's
    // cpuset defaults and completes before services are started.
    let target = root.join("system/etc/init/hw/init.rc");
    let mut init =
        fs::read_to_string(&target).map_err(|e| io_error("read Android init policy", e))?;
    init.push_str(policy);
    let source = directory.join("init.rc");
    fs::write(&source, init).map_err(|e| io_error("write Android CPU init policy", e))?;
    bind_mount(&source, &target, true)?;

    let target = root.join("system/etc/init/surfaceflinger.rc");
    let original =
        fs::read_to_string(&target).map_err(|e| io_error("read SurfaceFlinger service", e))?;
    let service = crate::cpu_placement::graphics_service(&original)
        .map_err(|e| io_error("set SurfaceFlinger graphics role", e))?;
    let source = directory.join("surfaceflinger.rc");
    fs::write(&source, service).map_err(|e| io_error("write SurfaceFlinger CPU policy", e))?;
    bind_mount(&source, &target, true)?;

    Ok(())
}

fn create_mount_target(path: &Path) -> Result<(), DevelopmentError> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map(drop)
        .map_err(|source| io_error("create bind-mount target", source))
}

fn make_device_node(
    path: &Path,
    kind: &str,
    major: u32,
    minor: u32,
    mode: &str,
) -> Result<(), DevelopmentError> {
    let kind = match kind {
        "c" => libc::S_IFCHR,
        "b" => libc::S_IFBLK,
        _ => {
            return Err(DevelopmentError::InvalidInput(format!(
                "unknown device node kind {kind:?}"
            )));
        }
    };
    let mode = u32::from_str_radix(mode, 8)
        .map_err(|_| DevelopmentError::InvalidInput(format!("invalid device mode {mode:?}")))?;
    let path_c = c_string(path.as_os_str().as_bytes())?;
    // SAFETY: the path is NUL terminated and outlives the call.
    if unsafe { libc::mknod(path_c.as_ptr(), kind | 0o600, libc::makedev(major, minor)) } != 0 {
        return Err(io_error(
            &format!("create device node {}", path.display()),
            io::Error::last_os_error(),
        ));
    }
    // mknod applies the umask; set the exact mode as mknod -m did.
    set_mode(path, mode)
}

#[repr(C)]
struct BinderfsDevice {
    name: [u8; BINDERFS_DEVICE_NAME_BYTES],
    major: u32,
    minor: u32,
}

fn add_binder_device(control: &Path, name: &str) -> Result<(), DevelopmentError> {
    if name.len() > BINDERFS_NAME_MAX || name.as_bytes().contains(&0) {
        return Err(DevelopmentError::InvalidInput(format!(
            "invalid binderfs device name {name:?}"
        )));
    }
    let mut request = BinderfsDevice {
        name: [0; BINDERFS_DEVICE_NAME_BYTES],
        major: 0,
        minor: 0,
    };
    request.name[..name.len()].copy_from_slice(name.as_bytes());
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(control)
        .map_err(|source| io_error("open binder-control", source))?;

    // BINDER_CTL_ADD is _IOWR('b', 1, struct binderfs_device). The request
    // structure above exactly mirrors linux/android/binderfs.h. The kernel
    // reads and writes the live structure only for this synchronous call.
    let result = unsafe {
        libc::ioctl(
            file.as_raw_fd(),
            binder_ctl_add_opcode(),
            std::ptr::from_mut(&mut request),
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io_error(
            &format!("create binderfs device {name}"),
            io::Error::last_os_error(),
        ))
    }
}

const fn binder_ctl_add_opcode() -> libc::c_ulong {
    const IOC_WRITE: u64 = 1;
    const IOC_READ: u64 = 2;
    const IOC_DIRSHIFT: u64 = 30;
    const IOC_SIZESHIFT: u64 = 16;
    const IOC_TYPESHIFT: u64 = 8;
    let size = std::mem::size_of::<BinderfsDevice>() as u64;
    (((IOC_READ | IOC_WRITE) << IOC_DIRSHIFT)
        | (size << IOC_SIZESHIFT)
        | ((b'b' as u64) << IOC_TYPESHIFT)
        | 1) as libc::c_ulong
}

fn effective_uid() -> Result<u32, DevelopmentError> {
    let status = fs::read_to_string("/proc/self/status")
        .map_err(|source| io_error("read process credentials", source))?;
    status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))
        .and_then(|uids| uids.split_whitespace().nth(1))
        .and_then(|uid| uid.parse().ok())
        .ok_or_else(|| DevelopmentError::InvalidInput("cannot parse effective UID".into()))
}

fn verify_magic(
    path: &Path,
    offset: u64,
    expected: &[u8],
    label: &str,
) -> Result<(), DevelopmentError> {
    if starts_with_magic(path, offset, expected)? {
        Ok(())
    } else {
        Err(DevelopmentError::InvalidInput(format!(
            "{} is not a {label} image",
            path.display()
        )))
    }
}

fn starts_with_magic(path: &Path, offset: u64, expected: &[u8]) -> Result<bool, DevelopmentError> {
    let mut file = File::open(path).map_err(|source| io_error("open image", source))?;
    file.seek(SeekFrom::Start(offset))
        .map_err(|source| io_error("seek image magic", source))?;
    let mut actual = vec![0; expected.len()];
    file.read_exact(&mut actual)
        .map_err(|source| io_error("read image magic", source))?;
    Ok(actual == expected)
}

fn set_mode(path: &Path, mode: u32) -> Result<(), DevelopmentError> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .map_err(|source| io_error("set path mode", source))
}

fn set_owner(path: &Path, uid: u32, gid: u32) -> Result<(), DevelopmentError> {
    let path_bytes = CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        DevelopmentError::InvalidInput(format!("{} contains a NUL byte", path.display()))
    })?;
    let result = unsafe { libc::chown(path_bytes.as_ptr(), uid, gid) };
    if result == 0 {
        Ok(())
    } else {
        Err(io_error("set path owner", io::Error::last_os_error()))
    }
}

fn run_os<I>(program: &str, args: I) -> Result<(), DevelopmentError>
where
    I: IntoIterator<Item = OsString>,
{
    let status = droidloom_cpu_placement::command(program)
        .args(args)
        .status()
        .map_err(|source| io_error(&format!("execute {program}"), source))?;
    if status.success() {
        Ok(())
    } else {
        Err(DevelopmentError::Command {
            program: program.into(),
            status,
        })
    }
}

fn io_error(context: &str, source: io::Error) -> DevelopmentError {
    DevelopmentError::Io {
        context: context.into(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn servicemanager_runs_as_root_and_keeps_its_groups() {
        let original = "service servicemanager /system/bin/servicemanager\n    class core animation\n    user system\n    group system readproc\n    critical\n";
        let service = root_servicemanager(original).unwrap();
        assert!(service.contains("\n    user root\n    group system readproc\n"));
        assert!(!service.contains("user system"));
        assert_eq!(root_servicemanager("service servicemanager x\n    user root\n"), None);
    }

    #[test]
    fn physical_input_hotplug_cannot_create_nodes() {
        const CHILD: &str = "DROIDLOOM_TEST_INPUT_ISOLATION";
        if std::env::var_os(CHILD).is_none() {
            assert!(
                std::process::Command::new("unshare")
                    .args(["--user", "--map-root-user", "--mount"])
                    .arg(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "development::tests::physical_input_hotplug_cannot_create_nodes"
                    ])
                    .env(CHILD, "1")
                    .status()
                    .unwrap()
                    .success()
            );
            return;
        }
        let dev = tempfile::tempdir().unwrap();
        mount_private_input(dev.path()).unwrap();
        let input = dev.path().join("input");
        assert_eq!(fs::read_dir(&input).unwrap().count(), 0);
        let node =
            std::ffi::CString::new(input.join("event0").as_os_str().as_encoded_bytes()).unwrap();
        // Even root in this namespace cannot recreate a host input node.
        // The temporary node is never opened; successful creation is a failure.
        assert_eq!(
            unsafe { libc::mknod(node.as_ptr(), libc::S_IFCHR | 0o600, libc::makedev(13, 64)) },
            -1
        );
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::EROFS));
        assert_eq!(
            fs::write(input.join("ordinary-file"), b"x")
                .unwrap_err()
                .raw_os_error(),
            Some(libc::EROFS)
        );
        fs::write(dev.path().join("other-device-state"), b"unchanged").unwrap();
        assert!(
            std::process::Command::new("umount")
                .arg(&input)
                .status()
                .unwrap()
                .success()
        );
    }

    #[test]
    fn loop_device_minors_follow_host_partition_geometry() {
        assert_eq!(loop_device_minor(0, 7).unwrap(), 0);
        assert_eq!(loop_device_minor(6, 0).unwrap(), 6);
        assert_eq!(loop_device_minor(6, 7).unwrap(), 48);
        assert_eq!(loop_device_minor(6, 15).unwrap(), 96);
        assert_eq!(loop_device_minor(255, 7).unwrap(), 2040);
        assert_eq!(loop_device_minor(6, 5).unwrap(), 48);
    }

    #[test]
    fn loop_device_minors_reject_overflow_and_out_of_range_devices() {
        assert!(loop_device_minor(1, u32::MAX).is_err());
        assert!(loop_device_minor(u32::MAX, 7).is_err());
        assert!(loop_device_minor(1 << 17, 7).is_err());
    }

    #[test]
    fn storage_identity_uses_installed_media_provider_not_media_rw_or_other_apps() {
        let packages = "com.android.providers.media 10014 0 /data/user/0/legacy\n\
                        com.android.providers.media.module 10101 0 /data/user/0/provider\n";
        assert_eq!(media_provider_uid(packages).unwrap(), 10101);
        assert_eq!(
            media_provider_uid(&packages.replace("10101", "10542")).unwrap(),
            10542
        );
        for invalid in [
            "",
            "com.android.providers.media.module",
            "com.android.providers.media.module 0",
            "com.android.providers.media.module 1023",
            "com.android.providers.media.module invalid",
        ] {
            assert!(media_provider_uid(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn fresh_storage_has_no_media_provider_uid_until_android_writes_packages() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(installed_media_provider_uid(root.path()).unwrap(), None);

        let system = root.path().join("data/system");
        fs::create_dir_all(&system).unwrap();
        assert_eq!(installed_media_provider_uid(root.path()).unwrap(), None);

        fs::write(
            system.join("packages.list"),
            "com.android.providers.media.module 10101 0 /data/user/0/provider\n",
        )
        .unwrap();
        assert_eq!(
            installed_media_provider_uid(root.path()).unwrap(),
            Some(10101)
        );

        // A written list without MediaProvider is broken storage, not new storage.
        fs::write(system.join("packages.list"), "").unwrap();
        assert!(installed_media_provider_uid(root.path()).is_err());
    }

    fn image_with_magic(offset: u64, magic: &[u8]) -> tempfile::NamedTempFile {
        let mut image = tempfile::NamedTempFile::new().unwrap();
        image.as_file_mut().seek(SeekFrom::Start(offset)).unwrap();
        image.as_file_mut().write_all(magic).unwrap();
        image
    }

    #[test]
    fn binder_ioctl_matches_linux_uapi() {
        assert_eq!(std::mem::size_of::<BinderfsDevice>(), 264);
        assert_eq!(binder_ctl_add_opcode(), 0xc108_6201);
    }

    #[test]
    fn android_sparse_magic_is_little_endian_uapi_value() {
        assert_eq!(ANDROID_SPARSE_MAGIC, [0x3a, 0xff, 0x26, 0xed]);
    }

    #[test]
    fn development_cgroups_keep_unavailable_v1_controllers_optional() {
        let document: serde_json::Value = serde_json::from_str(DEVELOPMENT_CGROUPS).unwrap();
        let legacy = document["Cgroups"].as_array().unwrap();
        assert!(!legacy.is_empty());
        assert!(
            legacy
                .iter()
                .all(|controller| controller["Optional"] == true)
        );
        assert_eq!(document["Cgroups2"]["Path"], "/sys/fs/cgroup");
    }

    #[test]
    fn development_base_accepts_erofs_and_raw_ext4() {
        let erofs = image_with_magic(EROFS_MAGIC_OFFSET, &EROFS_MAGIC);
        assert_eq!(
            detect_read_only_filesystem(erofs.path()).unwrap(),
            ReadOnlyFilesystem::Erofs
        );

        let ext4 = image_with_magic(EXT4_MAGIC_OFFSET, &EXT4_MAGIC);
        assert_eq!(
            detect_read_only_filesystem(ext4.path()).unwrap(),
            ReadOnlyFilesystem::Ext4
        );
    }

    #[test]
    fn development_base_rejects_unknown_images() {
        let image = image_with_magic(EXT4_MAGIC_OFFSET, b"no");
        let error = detect_read_only_filesystem(image.path()).unwrap_err();
        assert!(error.to_string().contains("neither an EROFS nor raw ext4"));
    }

    #[test]
    fn android_init_override_must_be_a_regular_executable_file() {
        let init = tempfile::NamedTempFile::new().unwrap();
        set_mode(init.path(), 0o644).unwrap();
        let error = validate_android_init_override(init.path()).unwrap_err();
        assert!(error.to_string().contains("is not executable"));

        set_mode(init.path(), 0o755).unwrap();
        validate_android_init_override(init.path()).unwrap();

        let directory = tempfile::tempdir().unwrap();
        let error = validate_android_init_override(directory.path()).unwrap_err();
        assert!(error.to_string().contains("is not a regular file"));
    }

    #[test]
    fn android_file_override_source_is_a_regular_file_or_directory() {
        let file = tempfile::NamedTempFile::new().unwrap();
        validate_android_file_override_source(file.path()).unwrap();

        let directory = tempfile::tempdir().unwrap();
        validate_android_file_override_source(directory.path()).unwrap();

        let socket = directory.path().join("socket");
        let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let error = validate_android_file_override_source(&socket).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("neither a regular file nor a directory")
        );
    }

    #[test]
    fn linux_device_numbers_decode_render_node() {
        let encoded = ((DRM_DEVICE_MAJOR & 0x0fff) << 8)
            | ((DRM_DEVICE_MAJOR & !0x0fff) << 32)
            | (DRM_RENDER_MINOR_BASE & 0x00ff)
            | ((DRM_RENDER_MINOR_BASE & !0x00ff) << 12);
        assert_eq!(
            linux_device_numbers(encoded),
            (DRM_DEVICE_MAJOR, DRM_RENDER_MINOR_BASE)
        );
    }

    #[test]
    fn render_node_sysfs_projection_contains_only_selected_metadata() {
        let root = tempfile::tempdir().unwrap();
        let metadata = RenderNodeSysfsMetadata {
            major: DRM_DEVICE_MAJOR,
            minor: DRM_RENDER_MINOR_BASE + 1,
            node_name: "renderD129".into(),
            subsystem: "platform".into(),
            uevent: "OF_FULLNAME=/soc/gpu\nOF_COMPATIBLE_N=1\nOF_COMPATIBLE_0=qcom,gpu\n".into(),
            pci_identity: None,
        };

        publish_render_node_sysfs_metadata(root.path(), &metadata).unwrap();

        let device = root.path().join("dev/char/226:129/device");
        assert_eq!(
            fs::read_link(device.join("subsystem")).unwrap(),
            PathBuf::from("/sys/bus/platform")
        );
        assert_eq!(
            fs::read_to_string(device.join("uevent")).unwrap(),
            metadata.uevent
        );
        assert!(device.join("drm/renderD129").is_dir());
        assert!(!device.join("drm/card0").exists());
        assert!(!device.join("vendor").exists());
        assert_eq!(
            fs::metadata(device.join("uevent"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o444
        );
        assert_eq!(
            fs::read_dir(root.path().join("dev/char")).unwrap().count(),
            1
        );
        assert_eq!(fs::read_dir(root.path().join("bus")).unwrap().count(), 1);
    }

    #[test]
    fn cpu_projection_requires_discovery_masks_and_excludes_controls() {
        let root = tempfile::tempdir().unwrap();
        assert!(discover_cpu_files(root.path()).is_err());
        for name in ["possible", "present", "online"] {
            fs::write(root.path().join(name), "0-7\n").unwrap();
        }
        for name in [
            "cpu0/cpu_capacity",
            "cpu0/cpufreq/cpuinfo_max_freq",
            "cpu0/online",
            "cpu0/uevent",
            "cpu0/cpufreq/scaling_max_freq",
            "cpu0/cpufreq/scaling_governor",
            "cpu0extra/cpu_capacity",
            "cpufreq/policy0/scaling_setspeed",
            "smt/control",
        ] {
            let p = root.path().join(name);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, "1024\n").unwrap();
        }
        let selected: Vec<_> = discover_cpu_files(root.path())
            .unwrap()
            .into_iter()
            .map(|(_, relative)| relative)
            .collect();
        assert_eq!(
            selected,
            vec![
                PathBuf::from("cpu0/cpu_capacity"),
                PathBuf::from("cpu0/cpufreq/cpuinfo_max_freq"),
                PathBuf::from("online"),
                PathBuf::from("possible"),
                PathBuf::from("present"),
            ]
        );
    }

    #[test]
    fn cpu_projection_allows_absent_optional_topology_and_policy_symlink() {
        let root = tempfile::tempdir().unwrap();
        for name in ["possible", "present", "online"] {
            fs::write(root.path().join(name), "0-1\n").unwrap();
        }
        fs::create_dir(root.path().join("cpu0")).unwrap();
        fs::create_dir(root.path().join("cpu1")).unwrap();
        fs::create_dir_all(root.path().join("cpufreq/policy0")).unwrap();
        fs::write(
            root.path().join("cpufreq/policy0/cpuinfo_max_freq"),
            "4000000\n",
        )
        .unwrap();
        symlink("../cpufreq/policy0", root.path().join("cpu0/cpufreq")).unwrap();
        let files = discover_cpu_files(root.path()).unwrap();
        assert_eq!(files.len(), 4);
        assert!(files.iter().any(|(source, relative)| {
            relative == Path::new("cpu0/cpufreq/cpuinfo_max_freq")
                && fs::read_to_string(source).unwrap() == "4000000\n"
        }));
    }

    #[test]
    fn power_supply_projection_selects_only_health_state() {
        let root = tempfile::tempdir().unwrap();
        let battery = root.path().join("battery");
        let usb = root.path().join("usb");
        let ignored = root.path().join("charge_pump");
        for directory in [&battery, &usb, &ignored] {
            fs::create_dir(directory).unwrap();
        }
        fs::write(battery.join("type"), "Battery\n").unwrap();
        fs::write(battery.join("status"), "Discharging\n").unwrap();
        fs::write(battery.join("capacity"), "42\n").unwrap();
        fs::write(battery.join("serial_number"), "private\n").unwrap();
        fs::write(usb.join("type"), "USB\n").unwrap();
        fs::write(usb.join("online"), "0\n").unwrap();
        fs::write(ignored.join("type"), "Unknown\n").unwrap();
        fs::write(ignored.join("online"), "1\n").unwrap();

        let selected = discover_power_supply_files(root.path())
            .unwrap()
            .into_iter()
            .map(|(_, relative)| relative)
            .collect::<Vec<_>>();
        assert_eq!(
            selected,
            [
                PathBuf::from("battery/capacity"),
                PathBuf::from("battery/status"),
                PathBuf::from("battery/type"),
                PathBuf::from("usb/online"),
                PathBuf::from("usb/type"),
            ]
        );
    }

    #[test]
    fn pci_render_node_projection_includes_bounded_libdrm_identity() {
        let root = tempfile::tempdir().unwrap();
        let metadata = RenderNodeSysfsMetadata {
            major: DRM_DEVICE_MAJOR,
            minor: DRM_RENDER_MINOR_BASE,
            node_name: "renderD128".into(),
            subsystem: "pci".into(),
            uevent: "PCI_SLOT_NAME=0000:00:02.0\n".into(),
            pci_identity: Some(PciIdentity {
                revision: "0x02\n".into(),
                vendor: "0x8086\n".into(),
                device: "0x9bc4\n".into(),
                subsystem_vendor: "0x17aa\n".into(),
                subsystem_device: "0x22c7\n".into(),
            }),
        };

        publish_render_node_sysfs_metadata(root.path(), &metadata).unwrap();

        let device = root.path().join("dev/char/226:128/device");
        for (name, expected) in [
            ("revision", "0x02\n"),
            ("vendor", "0x8086\n"),
            ("device", "0x9bc4\n"),
            ("subsystem_vendor", "0x17aa\n"),
            ("subsystem_device", "0x22c7\n"),
        ] {
            assert_eq!(fs::read_to_string(device.join(name)).unwrap(), expected);
            assert_eq!(
                fs::metadata(device.join(name))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o444
            );
        }
    }
}
