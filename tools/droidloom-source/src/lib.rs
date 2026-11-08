//! Exact-commit sparse AOSP source materialization.
//!
//! This tool intentionally does not invoke an unrestricted repo sync. It
//! accepts only a bounded lock whose projects are pinned to Git commits from
//! one AOSP superproject, fetches those commits into a staging directory,
//! verifies every resulting HEAD, creates the manifest-declared root links,
//! and then publishes the source tree atomically.

#![forbid(unsafe_code)]

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs;
use std::io::{self, BufReader, Write as _};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::Builder;
use thiserror::Error;

const MAX_LOCK_BYTES: u64 = 1024 * 1024;
const MAX_PROJECTS: usize = 256;
const MAX_LINKS: usize = 64;
const MAX_SPARSE_PATHS: usize = 160;
const OFFICIAL_REMOTE: &str = "https://android.googlesource.com/";

/// Exact sparse-source lock selected for one milestone.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SparseSourceLock {
    /// Lock schema revision.
    pub schema_version: u32,
    /// Human-readable selection date.
    pub selected_at: String,
    /// Closure maturity; a seed must not be represented as build-proven.
    pub status: String,
    /// Upstream URL prefix applied to project names.
    pub remote_base: String,
    /// AOSP superproject that authenticates every project Gitlink.
    pub superproject: SuperprojectLock,
    /// Fail-closed source expansion policy.
    pub policy: SparsePolicy,
    /// Root links otherwise supplied by the AOSP repo manifest.
    #[serde(default)]
    pub links: Vec<RootLink>,
    /// Exact projects in the current closure.
    pub projects: Vec<SparseProject>,
}

/// Immutable AOSP superproject identity.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SuperprojectLock {
    /// Android Git project name.
    pub name: String,
    /// Auditable source ref used when the lock was generated.
    #[serde(rename = "ref")]
    pub source_ref: String,
    /// Exact superproject commit.
    pub commit: String,
}

/// Policy preventing accidental expansion into a full AOSP checkout.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "the lock keeps each fail-closed source policy independently auditable"
)]
pub struct SparsePolicy {
    /// Must remain false.
    pub allow_unrestricted_repo_sync: bool,
    /// Must remain true.
    pub require_exact_commits: bool,
    /// Must remain true.
    pub add_dependencies_only_from_superproject: bool,
    /// Must remain true while reusing the verified CI base.
    pub base_partitions_come_from_aosp_ci_artifact: bool,
}

/// One exact upstream project.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SparseProject {
    /// Checkout-relative path.
    pub path: PathBuf,
    /// Android Git project name below the official remote.
    pub name: String,
    /// Exact 40-digit lowercase Git commit.
    pub commit: String,
    /// Why this project belongs in the closure.
    pub purpose: String,
    /// Optional project-relative directories checked out in Git cone mode.
    #[serde(default)]
    pub sparse_paths: Vec<PathBuf>,
}

/// One root link copied from the pinned repo manifest.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RootLink {
    /// Source-tree-relative existing path.
    pub source: PathBuf,
    /// Source-tree-relative link to create.
    pub target: PathBuf,
}

/// Network-free exact materialization plan.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MaterializationPlan {
    /// Schema revision.
    pub schema_version: u32,
    /// Exact AOSP superproject commit.
    pub superproject_commit: String,
    /// Ordered project fetches.
    pub projects: Vec<PlannedProject>,
    /// Ordered root links created after checkout.
    pub links: Vec<RootLink>,
    /// Explicit proof that this is not an unrestricted repo checkout.
    pub unrestricted_repo_sync: bool,
}

/// One immutable project fetch.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PlannedProject {
    /// Checkout-relative path.
    pub path: PathBuf,
    /// Exact Android Git URL.
    pub url: String,
    /// Exact commit to fetch and verify.
    pub commit: String,
    /// Auditable inclusion reason.
    pub purpose: String,
    /// Optional project-relative directories checked out in Git cone mode.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sparse_paths: Vec<PathBuf>,
}

/// Manifest written beside a successfully materialized source tree.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MaterializedManifest {
    /// Manifest schema revision.
    pub schema_version: u32,
    /// SHA-256 of the source lock that drove materialization.
    pub source_lock_sha256: String,
    /// Fully validated immutable plan.
    pub plan: MaterializationPlan,
}

/// Invalid input, local I/O, or exact Git operation failure.
#[derive(Debug, Error)]
pub enum SourceError {
    /// Filesystem operation failed.
    #[error("{context}: {source}")]
    Io {
        /// Operation context.
        context: String,
        /// Underlying I/O error.
        source: io::Error,
    },
    /// Lock JSON was malformed.
    #[error("invalid sparse source lock {path}: {source}")]
    Json {
        /// Lock path.
        path: PathBuf,
        /// JSON parser failure.
        source: serde_json::Error,
    },
    /// One or more lock invariants failed.
    #[error("invalid sparse source lock: {}", .0.join("; "))]
    Invalid(Vec<String>),
    /// An exact Git operation failed.
    #[error("command {program} failed ({status}): {stderr}")]
    Command {
        /// Program name.
        program: String,
        /// Exit status rendering.
        status: String,
        /// Bounded stderr.
        stderr: String,
    },
    /// A checked-out HEAD did not match its lock.
    #[error("project {path} resolved to {actual}, expected {expected}")]
    RevisionMismatch {
        /// Project checkout path.
        path: PathBuf,
        /// Expected exact commit.
        expected: String,
        /// Actual HEAD.
        actual: String,
    },
}

/// Read a bounded sparse lock.
///
/// # Errors
///
/// Rejects oversized, unreadable, or malformed JSON input.
pub fn load_lock(path: &Path) -> Result<SparseSourceLock, SourceError> {
    let metadata = fs::metadata(path).map_err(|source| io_error("stat source lock", source))?;
    if metadata.len() > MAX_LOCK_BYTES {
        return Err(SourceError::Invalid(vec![format!(
            "lock is {} bytes; maximum is {MAX_LOCK_BYTES}",
            metadata.len()
        )]));
    }
    let bytes = fs::read(path).map_err(|source| io_error("read source lock", source))?;
    serde_json::from_slice(&bytes).map_err(|source| SourceError::Json {
        path: path.to_path_buf(),
        source,
    })
}

/// Validate the sparse policy and return a deterministic, network-free plan.
///
/// # Errors
///
/// Returns every structural or source-expansion policy violation discovered.
pub fn build_plan(lock: &SparseSourceLock) -> Result<MaterializationPlan, SourceError> {
    let mut problems = Vec::new();
    validate_header(lock, &mut problems);
    validate_projects(lock, &mut problems);
    validate_links(lock, &mut problems);
    if !problems.is_empty() {
        return Err(SourceError::Invalid(problems));
    }

    Ok(MaterializationPlan {
        schema_version: 1,
        superproject_commit: lock.superproject.commit.clone(),
        projects: lock
            .projects
            .iter()
            .map(|project| PlannedProject {
                path: project.path.clone(),
                url: format!("{}{}", lock.remote_base, project.name),
                commit: project.commit.clone(),
                purpose: project.purpose.clone(),
                sparse_paths: project.sparse_paths.clone(),
            })
            .collect(),
        links: lock.links.clone(),
        unrestricted_repo_sync: false,
    })
}

fn validate_header(lock: &SparseSourceLock, problems: &mut Vec<String>) {
    if lock.schema_version != 1 {
        problems.push(format!(
            "unsupported schema version {}",
            lock.schema_version
        ));
    }
    if lock.selected_at.trim().is_empty() {
        problems.push("selected_at must not be empty".into());
    }
    if lock.status != "seed_not_yet_build_proven" && lock.status != "build_proven" {
        problems.push(format!("unknown closure status {:?}", lock.status));
    }
    if lock.remote_base != OFFICIAL_REMOTE {
        problems.push(format!("remote_base must be {OFFICIAL_REMOTE}"));
    }
    if lock.superproject.name != "platform/superproject" {
        problems.push("superproject must be platform/superproject".into());
    }
    if !lock
        .superproject
        .source_ref
        .starts_with("refs/heads/android-")
    {
        problems.push("superproject ref must be a named Android release ref".into());
    }
    if !is_commit(&lock.superproject.commit) {
        problems.push("superproject commit must be exact lowercase SHA-1".into());
    }
    if lock.policy.allow_unrestricted_repo_sync {
        problems.push("unrestricted repo sync must remain disabled".into());
    }
    if !lock.policy.require_exact_commits
        || !lock.policy.add_dependencies_only_from_superproject
        || !lock.policy.base_partitions_come_from_aosp_ci_artifact
    {
        problems.push("all fail-closed sparse-source policy flags must remain enabled".into());
    }
}

fn validate_projects(lock: &SparseSourceLock, problems: &mut Vec<String>) {
    if lock.projects.is_empty() || lock.projects.len() > MAX_PROJECTS {
        problems.push(format!(
            "projects must contain 1 through {MAX_PROJECTS} entries"
        ));
    }
    let mut paths = BTreeSet::new();
    let mut names = BTreeSet::new();
    for project in &lock.projects {
        if !is_normal_relative(&project.path) {
            problems.push(format!(
                "project path {} is not normalized and relative",
                project.path.display()
            ));
        }
        if !paths.insert(project.path.clone()) {
            problems.push(format!("duplicate project path {}", project.path.display()));
        }
        if !is_project_name(&project.name) {
            problems.push(format!("invalid Android project name {:?}", project.name));
        }
        if !names.insert(project.name.clone()) {
            problems.push(format!("duplicate Android project {:?}", project.name));
        }
        if !is_commit(&project.commit) {
            problems.push(format!(
                "project {} does not use an exact lowercase SHA-1",
                project.path.display()
            ));
        }
        if project.purpose.trim().is_empty() || project.purpose.len() > 255 {
            problems.push(format!(
                "project {} has an empty or oversized purpose",
                project.path.display()
            ));
        }
        if project.sparse_paths.len() > MAX_SPARSE_PATHS {
            problems.push(format!(
                "project {} has more than {MAX_SPARSE_PATHS} sparse paths",
                project.path.display()
            ));
        }
        let mut sparse_paths = BTreeSet::new();
        for sparse_path in &project.sparse_paths {
            if !is_normal_relative(sparse_path) {
                problems.push(format!(
                    "project {} sparse path {} is not normalized and relative",
                    project.path.display(),
                    sparse_path.display()
                ));
            }
            if !sparse_paths.insert(sparse_path) {
                problems.push(format!(
                    "project {} repeats sparse path {}",
                    project.path.display(),
                    sparse_path.display()
                ));
            }
        }
    }
}

fn validate_links(lock: &SparseSourceLock, problems: &mut Vec<String>) {
    if lock.links.len() > MAX_LINKS {
        problems.push(format!("links exceed the {MAX_LINKS}-entry bound"));
    }
    let project_paths: Vec<&Path> = lock
        .projects
        .iter()
        .map(|project| project.path.as_path())
        .collect();
    let mut targets = BTreeSet::new();
    for link in &lock.links {
        if !is_normal_relative(&link.source) || !is_normal_relative(&link.target) {
            problems.push("root link paths must be normalized and relative".into());
        }
        if !project_paths
            .iter()
            .any(|project| link.source.starts_with(project))
        {
            problems.push(format!(
                "link source {} is not inside a selected project",
                link.source.display()
            ));
        }
        if project_paths
            .iter()
            .any(|project| link.target == *project || link.target.starts_with(project))
        {
            problems.push(format!(
                "link target {} overlaps a selected project",
                link.target.display()
            ));
        }
        if !targets.insert(link.target.clone()) {
            problems.push(format!("duplicate root link {}", link.target.display()));
        }
    }
}

/// Fetch, verify, link, and atomically publish one sparse source tree.
///
/// The output must not exist. A failure before publication removes the staging
/// directory and never exposes a partial source tree at the destination.
///
/// # Errors
///
/// Returns for invalid locks, existing output, Git failures, revision mismatch,
/// missing link sources, or local I/O errors.
pub fn materialize(
    lock_path: &Path,
    lock: &SparseSourceLock,
    destination: &Path,
) -> Result<MaterializedManifest, SourceError> {
    let plan = build_plan(lock)?;
    if destination.exists() {
        return Err(SourceError::Invalid(vec![format!(
            "destination {} already exists",
            destination.display()
        )]));
    }
    let parent = destination
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|source| io_error("create output parent", source))?;
    let staging = Builder::new()
        .prefix(".droidloom-source-")
        .tempdir_in(parent)
        .map_err(|source| io_error("create source staging directory", source))?;

    for project in &plan.projects {
        materialize_project(staging.path(), project)?;
    }
    for link in &plan.links {
        create_root_link(staging.path(), link)?;
    }

    let manifest = MaterializedManifest {
        schema_version: 1,
        source_lock_sha256: sha256_file(lock_path)?,
        plan,
    };
    let json = serde_json::to_vec_pretty(&manifest)
        .map_err(|error| SourceError::Invalid(vec![format!("serialize manifest: {error}")]))?;
    fs::write(staging.path().join(".droidloom-source-manifest.json"), json)
        .map_err(|source| io_error("write materialized manifest", source))?;
    fs::rename(staging.path(), destination)
        .map_err(|source| io_error("publish sparse source tree atomically", source))?;
    Ok(manifest)
}

/// Reconcile a verified materialization with an expanded exact source lock.
///
/// Existing projects must be clean and still resolve to the same URL and
/// commit recorded by the new plan. The new plan must be a strict extension of
/// the published manifest: reconciliation never changes or removes an input.
/// A sparse project may add explicitly locked paths at the same commit; it may
/// not drop a previously materialized sparse path.
/// Each missing project is staged and renamed independently, so interruption
/// leaves either a complete exact checkout or no checkout at that path. The
/// manifest is replaced atomically only after the complete tree verifies.
///
/// # Errors
///
/// Returns for an unrecognized destination, a non-monotonic plan change,
/// dirty/mismatched existing projects, Git failures, or invalid root links.
pub fn reconcile(
    lock_path: &Path,
    lock: &SparseSourceLock,
    destination: &Path,
) -> Result<MaterializedManifest, SourceError> {
    let plan = build_plan(lock)?;
    let old_manifest_path = destination.join(".droidloom-source-manifest.json");
    let old_bytes = fs::read(&old_manifest_path)
        .map_err(|source| io_error("read existing materialized manifest", source))?;
    let old = serde_json::from_slice::<MaterializedManifest>(&old_bytes).map_err(|source| {
        SourceError::Json {
            path: old_manifest_path,
            source,
        }
    })?;
    validate_plan_extension(&old.plan, &plan)?;

    for project in &plan.projects {
        let checkout = destination.join(&project.path);
        if checkout.exists() {
            verify_project(&checkout, project)?;
            apply_sparse_checkout(&checkout, project)?;
            verify_project(&checkout, project)?;
        } else {
            install_missing_project(destination, project)?;
            verify_project(&checkout, project)?;
        }
    }
    for link in &plan.links {
        ensure_root_link(destination, link)?;
    }

    let manifest = MaterializedManifest {
        schema_version: 1,
        source_lock_sha256: sha256_file(lock_path)?,
        plan,
    };
    write_manifest_atomically(destination, &manifest)?;
    Ok(manifest)
}

/// Verify a published source tree against the current exact lock without
/// fetching or modifying it.
///
/// # Errors
///
/// Returns when the manifest does not exactly match the lock, a project is
/// dirty or has the wrong origin/commit, or a declared root link is missing or
/// points somewhere else.
pub fn verify_materialized(
    lock_path: &Path,
    lock: &SparseSourceLock,
    destination: &Path,
) -> Result<MaterializedManifest, SourceError> {
    let expected_plan = build_plan(lock)?;
    let manifest_path = destination.join(".droidloom-source-manifest.json");
    let bytes = fs::read(&manifest_path)
        .map_err(|source| io_error("read materialized manifest", source))?;
    let manifest = serde_json::from_slice::<MaterializedManifest>(&bytes).map_err(|source| {
        SourceError::Json {
            path: manifest_path,
            source,
        }
    })?;

    let mut problems = Vec::new();
    if manifest.schema_version != 1 {
        problems.push("materialized manifest is not schema version 1".into());
    }
    if manifest.source_lock_sha256 != sha256_file(lock_path)? {
        problems.push("materialized manifest source-lock hash does not match".into());
    }
    if manifest.plan != expected_plan {
        problems.push("materialized manifest plan does not exactly match the lock".into());
    }
    if !problems.is_empty() {
        return Err(SourceError::Invalid(problems));
    }

    for project in &manifest.plan.projects {
        verify_project(&destination.join(&project.path), project)?;
    }
    for link in &manifest.plan.links {
        verify_root_link(destination, link)?;
    }
    Ok(manifest)
}

fn validate_plan_extension(
    old: &MaterializationPlan,
    new: &MaterializationPlan,
) -> Result<(), SourceError> {
    let mut problems = Vec::new();
    if old.schema_version != 1 || old.unrestricted_repo_sync {
        problems.push("existing manifest is not a bounded schema-v1 plan".into());
    }
    if old.superproject_commit != new.superproject_commit {
        problems.push("reconciliation cannot change the AOSP superproject commit".into());
    }
    for project in &old.projects {
        let replacement = new.projects.iter().find(|candidate| {
            candidate.path == project.path
                && candidate.url == project.url
                && candidate.commit == project.commit
                && candidate.purpose == project.purpose
        });
        let compatible = replacement.is_some_and(|candidate| {
            candidate.sparse_paths == project.sparse_paths
                || project.sparse_paths.is_empty()
                || project
                    .sparse_paths
                    .iter()
                    .all(|path| candidate.sparse_paths.contains(path))
        });
        if !compatible {
            problems.push(format!(
                "reconciliation cannot change or remove project {} or its existing sparse paths",
                project.path.display()
            ));
        }
    }
    for link in &old.links {
        if !new.links.contains(link) {
            problems.push(format!(
                "reconciliation cannot change or remove root link {}",
                link.target.display()
            ));
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(SourceError::Invalid(problems))
    }
}

fn verify_project(checkout: &Path, project: &PlannedProject) -> Result<(), SourceError> {
    let head = run_git_output(checkout, &[OsStr::new("rev-parse"), OsStr::new("HEAD")])?;
    let actual = String::from_utf8_lossy(&head.stdout).trim().to_owned();
    if actual != project.commit {
        return Err(SourceError::RevisionMismatch {
            path: project.path.clone(),
            expected: project.commit.clone(),
            actual,
        });
    }
    let remote = run_git_output(
        checkout,
        &[
            OsStr::new("remote"),
            OsStr::new("get-url"),
            OsStr::new("origin"),
        ],
    )?;
    if String::from_utf8_lossy(&remote.stdout).trim() != project.url {
        return Err(SourceError::Invalid(vec![format!(
            "project {} origin does not match {}",
            project.path.display(),
            project.url
        )]));
    }
    let status = run_git_output(checkout, &[OsStr::new("status"), OsStr::new("--porcelain")])?;
    if !status.stdout.is_empty() {
        return Err(SourceError::Invalid(vec![format!(
            "project {} has local changes",
            project.path.display()
        )]));
    }
    Ok(())
}

fn install_missing_project(root: &Path, project: &PlannedProject) -> Result<(), SourceError> {
    let parent = root
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let staging = Builder::new()
        .prefix(".droidloom-project-")
        .tempdir_in(parent)
        .map_err(|source| io_error("create project staging directory", source))?;
    materialize_project(staging.path(), project)?;
    let checkout = root.join(&project.path);
    let checkout_parent = checkout
        .parent()
        .ok_or_else(|| SourceError::Invalid(vec!["project path has no parent".into()]))?;
    fs::create_dir_all(checkout_parent)
        .map_err(|source| io_error("create missing project parent", source))?;
    fs::rename(staging.path().join(&project.path), &checkout)
        .map_err(|source| io_error("publish exact project atomically", source))?;
    Ok(())
}

fn ensure_root_link(root: &Path, link: &RootLink) -> Result<(), SourceError> {
    let target = root.join(&link.target);
    match fs::symlink_metadata(&target) {
        Ok(_) => verify_root_link(root, link),
        Err(error) if error.kind() == io::ErrorKind::NotFound => create_root_link(root, link),
        Err(error) => Err(io_error("stat existing root link", error)),
    }
}

fn verify_root_link(root: &Path, link: &RootLink) -> Result<(), SourceError> {
    fs::symlink_metadata(root.join(&link.source))
        .map_err(|source| io_error("stat root link source", source))?;
    let target = root.join(&link.target);
    let metadata = fs::symlink_metadata(&target)
        .map_err(|source| io_error("stat root link target", source))?;
    if !metadata.file_type().is_symlink() {
        return Err(SourceError::Invalid(vec![format!(
            "root link target {} exists and is not a symlink",
            link.target.display()
        )]));
    }
    let expected = relative_path(
        link.target.parent().unwrap_or_else(|| Path::new("")),
        &link.source,
    );
    let actual =
        fs::read_link(&target).map_err(|source| io_error("read existing root link", source))?;
    if actual != expected {
        return Err(SourceError::Invalid(vec![format!(
            "root link {} resolves through {}, expected {}",
            link.target.display(),
            actual.display(),
            expected.display()
        )]));
    }
    Ok(())
}

fn write_manifest_atomically(
    root: &Path,
    manifest: &MaterializedManifest,
) -> Result<(), SourceError> {
    let json = serde_json::to_vec_pretty(manifest)
        .map_err(|error| SourceError::Invalid(vec![format!("serialize manifest: {error}")]))?;
    let mut temporary = Builder::new()
        .prefix(".droidloom-manifest-")
        .tempfile_in(root)
        .map_err(|source| io_error("create temporary materialized manifest", source))?;
    temporary
        .write_all(&json)
        .and_then(|()| temporary.as_file().sync_all())
        .map_err(|source| io_error("write temporary materialized manifest", source))?;
    temporary
        .persist(root.join(".droidloom-source-manifest.json"))
        .map_err(|error| io_error("publish materialized manifest atomically", error.error))?;
    Ok(())
}

fn materialize_project(root: &Path, project: &PlannedProject) -> Result<(), SourceError> {
    let checkout = root.join(&project.path);
    let parent = checkout
        .parent()
        .ok_or_else(|| SourceError::Invalid(vec!["project path has no parent".into()]))?;
    fs::create_dir_all(parent).map_err(|source| io_error("create project parent", source))?;
    run(
        "git",
        &[
            OsStr::new("init"),
            OsStr::new("--quiet"),
            OsStr::new("--initial-branch=droidloom"),
            checkout.as_os_str(),
        ],
    )?;
    run_git(
        &checkout,
        &[
            OsStr::new("remote"),
            OsStr::new("add"),
            OsStr::new("origin"),
            OsStr::new(&project.url),
        ],
    )?;
    apply_sparse_checkout(&checkout, project)?;
    run_git(
        &checkout,
        &[
            OsStr::new("fetch"),
            OsStr::new("--quiet"),
            OsStr::new("--depth=1"),
            OsStr::new("--filter=blob:none"),
            OsStr::new("origin"),
            OsStr::new(&project.commit),
        ],
    )?;
    run_git(
        &checkout,
        &[
            OsStr::new("checkout"),
            OsStr::new("--quiet"),
            OsStr::new("--detach"),
            OsStr::new("FETCH_HEAD"),
        ],
    )?;
    let head = run_git_output(&checkout, &[OsStr::new("rev-parse"), OsStr::new("HEAD")])?;
    let actual = String::from_utf8_lossy(&head.stdout).trim().to_owned();
    if actual != project.commit {
        return Err(SourceError::RevisionMismatch {
            path: project.path.clone(),
            expected: project.commit.clone(),
            actual,
        });
    }
    Ok(())
}

fn apply_sparse_checkout(checkout: &Path, project: &PlannedProject) -> Result<(), SourceError> {
    if project.sparse_paths.is_empty() {
        return Ok(());
    }
    run_git(
        checkout,
        &[
            OsStr::new("sparse-checkout"),
            OsStr::new("init"),
            OsStr::new("--cone"),
        ],
    )?;
    let mut arguments = vec![
        OsStr::new("sparse-checkout"),
        OsStr::new("set"),
        OsStr::new("--cone"),
        // Droidloom's bounded plans may name an individual pinned prebuilt.
        // Git's cone implementation supports that pattern but requires this
        // explicit acknowledgement because the path is not a directory.
        OsStr::new("--skip-checks"),
        OsStr::new("--"),
    ];
    arguments.extend(project.sparse_paths.iter().map(|path| path.as_os_str()));
    run_git(checkout, &arguments)
}

fn create_root_link(root: &Path, link: &RootLink) -> Result<(), SourceError> {
    let source = root.join(&link.source);
    fs::symlink_metadata(&source).map_err(|error| {
        io_error(
            &format!("stat root link source {}", link.source.display()),
            error,
        )
    })?;
    let target = root.join(&link.target);
    let target_parent = target
        .parent()
        .ok_or_else(|| SourceError::Invalid(vec!["root link target has no parent".into()]))?;
    fs::create_dir_all(target_parent).map_err(|source| io_error("create link parent", source))?;
    let relative_source = relative_path(
        link.target.parent().unwrap_or_else(|| Path::new("")),
        &link.source,
    );
    std::os::unix::fs::symlink(relative_source, &target)
        .map_err(|source| io_error("create AOSP root link", source))
}

fn relative_path(from: &Path, to: &Path) -> PathBuf {
    let from_parts: Vec<_> = from.components().collect();
    let to_parts: Vec<_> = to.components().collect();
    let common = from_parts
        .iter()
        .zip(&to_parts)
        .take_while(|(left, right)| left == right)
        .count();
    let mut result = PathBuf::new();
    for _ in common..from_parts.len() {
        result.push("..");
    }
    for component in &to_parts[common..] {
        result.push(component.as_os_str());
    }
    result
}

fn is_normal_relative(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && !path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

fn is_project_name(name: &str) -> bool {
    (name.starts_with("platform/")
        || name.starts_with("kernel/")
        || name.starts_with("toolchain/")
        || name == "tools/platform-compat")
        && name.len() <= 255
        && name
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

fn is_commit(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn run_git(checkout: &Path, args: &[&OsStr]) -> Result<(), SourceError> {
    run_git_output(checkout, args).map(drop)
}

fn run_git_output(checkout: &Path, args: &[&OsStr]) -> Result<Output, SourceError> {
    let mut all_args = vec![OsStr::new("-C"), checkout.as_os_str()];
    all_args.extend_from_slice(args);
    run_output("git", &all_args)
}

fn run(program: &str, args: &[&OsStr]) -> Result<(), SourceError> {
    run_output(program, args).map(drop)
}

fn run_output(program: &str, args: &[&OsStr]) -> Result<Output, SourceError> {
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|source| io_error(&format!("execute {program}"), source))?;
    if output.status.success() {
        Ok(output)
    } else {
        Err(SourceError::Command {
            program: program.to_owned(),
            status: output.status.to_string(),
            stderr: String::from_utf8_lossy(&output.stderr)
                .chars()
                .take(4096)
                .collect(),
        })
    }
}

fn sha256_file(path: &Path) -> Result<String, SourceError> {
    let file = fs::File::open(path).map_err(|source| io_error("open lock for SHA-256", source))?;
    let mut reader = BufReader::new(file);
    let mut hash = Sha256::new();
    io::copy(&mut reader, &mut hash).map_err(|source| io_error("read lock for SHA-256", source))?;
    Ok(hex::encode(hash.finalize()))
}

fn io_error(context: &str, source: io::Error) -> SourceError {
    SourceError::Io {
        context: context.to_owned(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_lock() -> SparseSourceLock {
        SparseSourceLock {
            schema_version: 1,
            selected_at: "2026-08-14".into(),
            status: "seed_not_yet_build_proven".into(),
            remote_base: OFFICIAL_REMOTE.into(),
            superproject: SuperprojectLock {
                name: "platform/superproject".into(),
                source_ref: "refs/heads/android-17.0.0_r1".into(),
                commit: "28f0bbddc24d56c8ec5a8df5342ac7a292184039".into(),
            },
            policy: SparsePolicy {
                allow_unrestricted_repo_sync: false,
                require_exact_commits: true,
                add_dependencies_only_from_superproject: true,
                base_partitions_come_from_aosp_ci_artifact: true,
            },
            links: vec![RootLink {
                source: "build/make/core".into(),
                target: "build/core".into(),
            }],
            projects: vec![SparseProject {
                path: "build/make".into(),
                name: "platform/build".into(),
                commit: "5ce6f787337d0223710bf7d4a16dbe6d2a35f777".into(),
                purpose: "product rules".into(),
                sparse_paths: Vec::new(),
            }],
        }
    }

    #[test]
    fn plan_uses_only_exact_official_projects() {
        let plan = build_plan(&valid_lock()).unwrap();
        assert!(!plan.unrestricted_repo_sync);
        assert_eq!(plan.projects.len(), 1);
        assert_eq!(
            plan.projects[0].url,
            "https://android.googlesource.com/platform/build"
        );
        assert_eq!(
            plan.projects[0].commit,
            "5ce6f787337d0223710bf7d4a16dbe6d2a35f777"
        );
    }

    #[test]
    fn platform_compat_uses_its_official_tools_namespace() {
        let mut lock = valid_lock();
        lock.projects[0].name = "tools/platform-compat".into();
        assert_eq!(
            build_plan(&lock).unwrap().projects[0].url,
            "https://android.googlesource.com/tools/platform-compat"
        );
        for invalid in ["tools/other", "tools/platform-compat/../other"] {
            lock.projects[0].name = invalid.into();
            assert!(build_plan(&lock).is_err());
        }
    }

    #[test]
    fn traversal_and_full_sync_policy_are_rejected_together() {
        let mut lock = valid_lock();
        lock.projects[0].path = "../outside".into();
        lock.policy.allow_unrestricted_repo_sync = true;
        let SourceError::Invalid(problems) = build_plan(&lock).unwrap_err() else {
            panic!("expected validation failures");
        };
        assert!(
            problems
                .iter()
                .any(|problem| problem.contains("normalized"))
        );
        assert!(problems.iter().any(|problem| problem.contains("repo sync")));
    }

    #[test]
    fn duplicate_projects_and_moving_refs_are_rejected() {
        let mut lock = valid_lock();
        let mut duplicate = lock.projects[0].clone();
        duplicate.commit = "refs/heads/main".into();
        lock.projects.push(duplicate);
        let SourceError::Invalid(problems) = build_plan(&lock).unwrap_err() else {
            panic!("expected validation failures");
        };
        assert!(problems.iter().any(|problem| problem.contains("duplicate")));
        assert!(problems.iter().any(|problem| problem.contains("exact")));
    }

    #[test]
    fn root_links_are_relative_to_their_parent() {
        assert_eq!(
            relative_path(Path::new("build"), Path::new("build/make/core")),
            Path::new("make/core")
        );
        assert_eq!(
            relative_path(Path::new(""), Path::new("build/soong/root.bp")),
            Path::new("build/soong/root.bp")
        );
    }

    #[test]
    fn exact_local_git_fetch_and_manifest_link_are_verified() {
        let sandbox = tempfile::tempdir().unwrap();
        let origin = sandbox.path().join("origin");
        run(
            "git",
            &[
                OsStr::new("init"),
                OsStr::new("--quiet"),
                OsStr::new("--initial-branch=main"),
                origin.as_os_str(),
            ],
        )
        .unwrap();
        run_git(
            &origin,
            &[
                OsStr::new("config"),
                OsStr::new("user.email"),
                OsStr::new("test@droidloom.invalid"),
            ],
        )
        .unwrap();
        run_git(
            &origin,
            &[
                OsStr::new("config"),
                OsStr::new("user.name"),
                OsStr::new("Droidloom Test"),
            ],
        )
        .unwrap();
        fs::create_dir(origin.join("core")).unwrap();
        fs::write(origin.join("core/marker"), b"exact commit").unwrap();
        run_git(&origin, &[OsStr::new("add"), OsStr::new("core/marker")]).unwrap();
        run_git(
            &origin,
            &[
                OsStr::new("commit"),
                OsStr::new("--quiet"),
                OsStr::new("-m"),
                OsStr::new("fixture"),
            ],
        )
        .unwrap();
        let output =
            run_git_output(&origin, &[OsStr::new("rev-parse"), OsStr::new("HEAD")]).unwrap();
        let commit = String::from_utf8(output.stdout).unwrap().trim().to_owned();

        let root = sandbox.path().join("checkout");
        fs::create_dir(&root).unwrap();
        materialize_project(
            &root,
            &PlannedProject {
                path: "build/make".into(),
                url: origin.to_string_lossy().into_owned(),
                commit,
                purpose: "test fixture".into(),
                sparse_paths: vec!["core".into()],
            },
        )
        .unwrap();
        create_root_link(
            &root,
            &RootLink {
                source: "build/make/core".into(),
                target: "build/core".into(),
            },
        )
        .unwrap();
        assert_eq!(
            fs::read(root.join("build/core/marker")).unwrap(),
            b"exact commit"
        );
        assert!(
            fs::symlink_metadata(root.join("build/core"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn reconciliation_accepts_only_monotonic_plan_expansion() {
        let initial = build_plan(&valid_lock()).unwrap();
        let mut expanded = initial.clone();
        expanded.projects.push(PlannedProject {
            path: "external/rust/android-crates-io".into(),
            url: "https://android.googlesource.com/platform/external/rust/android-crates-io".into(),
            commit: "1f5e95cd6e996995023c9c0f7df0f15a8a43269e".into(),
            purpose: "Rust crates".into(),
            sparse_paths: Vec::new(),
        });
        validate_plan_extension(&initial, &expanded).unwrap();

        let mut changed = expanded;
        changed.projects[0].commit = "0000000000000000000000000000000000000000".into();
        assert!(validate_plan_extension(&initial, &changed).is_err());
    }

    #[test]
    fn reconciliation_accepts_bounded_sparse_path_expansion() {
        let mut initial = build_plan(&valid_lock()).unwrap();
        initial.projects[0].sparse_paths = vec!["core".into()];

        let mut expanded = initial.clone();
        expanded.projects[0].sparse_paths.push("tools".into());
        validate_plan_extension(&initial, &expanded).unwrap();

        let mut contracted = expanded;
        contracted.projects[0].sparse_paths = vec!["tools".into()];
        assert!(validate_plan_extension(&initial, &contracted).is_err());
    }

    #[test]
    fn materialized_verification_rejects_dirty_projects() {
        let sandbox = tempfile::tempdir().unwrap();
        let root = sandbox.path().join("checkout");
        let checkout = root.join("build/make");
        fs::create_dir_all(checkout.join("core")).unwrap();
        run(
            "git",
            &[
                OsStr::new("init"),
                OsStr::new("--quiet"),
                OsStr::new("--initial-branch=main"),
                checkout.as_os_str(),
            ],
        )
        .unwrap();
        run_git(
            &checkout,
            &[
                OsStr::new("config"),
                OsStr::new("user.email"),
                OsStr::new("test@droidloom.invalid"),
            ],
        )
        .unwrap();
        run_git(
            &checkout,
            &[
                OsStr::new("config"),
                OsStr::new("user.name"),
                OsStr::new("Droidloom Test"),
            ],
        )
        .unwrap();
        run_git(
            &checkout,
            &[
                OsStr::new("remote"),
                OsStr::new("add"),
                OsStr::new("origin"),
                OsStr::new("https://android.googlesource.com/platform/build"),
            ],
        )
        .unwrap();
        fs::write(checkout.join("core/marker"), b"clean").unwrap();
        run_git(&checkout, &[OsStr::new("add"), OsStr::new("core/marker")]).unwrap();
        run_git(
            &checkout,
            &[
                OsStr::new("commit"),
                OsStr::new("--quiet"),
                OsStr::new("-m"),
                OsStr::new("fixture"),
            ],
        )
        .unwrap();

        let output =
            run_git_output(&checkout, &[OsStr::new("rev-parse"), OsStr::new("HEAD")]).unwrap();
        let mut lock = valid_lock();
        lock.projects[0].commit = String::from_utf8(output.stdout).unwrap().trim().to_owned();
        let lock_path = sandbox.path().join("lock.json");
        fs::write(&lock_path, b"exact test lock").unwrap();
        create_root_link(&root, &lock.links[0]).unwrap();
        let manifest = MaterializedManifest {
            schema_version: 1,
            source_lock_sha256: sha256_file(&lock_path).unwrap(),
            plan: build_plan(&lock).unwrap(),
        };
        fs::write(
            root.join(".droidloom-source-manifest.json"),
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();

        verify_materialized(&lock_path, &lock, &root).unwrap();
        fs::write(checkout.join("core/marker"), b"dirty").unwrap();
        let error = verify_materialized(&lock_path, &lock, &root).unwrap_err();
        assert!(error.to_string().contains("local changes"));
    }
}
