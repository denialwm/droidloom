//! XDG application integration for launchable activities in one Droidloom cell.
//!
//! This crate is deliberately unprivileged. It reads Android metadata through
//! the authenticated Droidloom control socket and owns only files below the
//! calling user's XDG data and state directories.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use droidloom_contracts::applications::application_id;
use droidloom_supervisor::control::{
    AndroidApplication, ControlRequest, DEFAULT_CONTROL_SOCKET, parse_launch_resolution, request,
};
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;
use thiserror::Error;

const MANIFEST_SCHEMA: u32 = 1;
const ICON_SIZE: u32 = 128;
const MAX_ICON_BYTES: usize = 1024 * 1024;
const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";

/// User-owned paths used by the catalog reconciler.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CatalogPaths {
    /// Standard XDG application-entry directory.
    pub applications: PathBuf,
    /// Standard hicolor application-icon directory.
    pub icons: PathBuf,
    /// Last successfully reconciled catalog state.
    pub manifest: PathBuf,
}

impl CatalogPaths {
    /// Resolve paths according to the XDG base-directory contract.
    ///
    /// Relative XDG overrides are ignored, as required by the specification.
    ///
    /// # Errors
    ///
    /// Fails when neither an absolute XDG path nor an absolute home directory
    /// can supply the user-owned data and state roots.
    pub fn from_environment(
        data_home: Option<OsString>,
        state_home: Option<OsString>,
        home: Option<OsString>,
    ) -> Result<Self, CatalogError> {
        let home = home.map(PathBuf::from).filter(|path| path.is_absolute());
        let data_root = absolute_path(data_home)
            .or_else(|| home.as_ref().map(|path| path.join(".local/share")))
            .ok_or(CatalogError::MissingHome)?;
        let state_root = absolute_path(state_home)
            .or_else(|| home.as_ref().map(|path| path.join(".local/state")))
            .ok_or(CatalogError::MissingHome)?;
        Ok(Self {
            applications: data_root.join("applications"),
            icons: data_root.join("icons/hicolor/128x128/apps"),
            manifest: state_root.join("droidloom/application-catalog-v1.json"),
        })
    }

    /// Construct explicit paths for packaging and integration tests.
    pub fn new(applications: PathBuf, icons: PathBuf, manifest: PathBuf) -> Self {
        Self {
            applications,
            icons,
            manifest,
        }
    }
}

/// Source of Android launcher metadata and icons.
pub trait ApplicationSource {
    /// Android user represented by this source.
    fn user(&self) -> u32;

    /// Return every enabled MAIN/LAUNCHER activity.
    ///
    /// # Errors
    ///
    /// Returns a transport, runtime, or metadata error when Android cannot
    /// provide a trustworthy catalog snapshot.
    fn applications(&mut self) -> Result<Vec<AndroidApplication>, CatalogError>;

    /// Return a rendered PNG icon for one exact launcher component.
    ///
    /// # Errors
    ///
    /// Returns a transport, runtime, component, or image-encoding error.
    fn icon(&mut self, component: &str, size: u32) -> Result<Vec<u8>, CatalogError>;

    /// Render a bounded icon batch; omitted entries are retried after transient failures.
    fn icons(
        &mut self,
        components: &[String],
        size: u32,
    ) -> Result<BTreeMap<String, Vec<u8>>, CatalogError> {
        Ok(components
            .iter()
            .filter_map(|component| {
                self.icon(component, size)
                    .ok()
                    .map(|bytes| (component.clone(), bytes))
            })
            .collect())
    }
}

/// Catalog source backed by the local Droidloom lifecycle socket.
#[derive(Clone, Debug)]
pub struct ControlCatalog {
    socket: PathBuf,
    user: u32,
}

impl ControlCatalog {
    /// Create a source for one boot-managed Droidloom cell.
    pub fn new(socket: PathBuf, user: u32) -> Self {
        Self { socket, user }
    }
}

impl Default for ControlCatalog {
    fn default() -> Self {
        Self::new(DEFAULT_CONTROL_SOCKET.into(), 0)
    }
}

impl ApplicationSource for ControlCatalog {
    fn user(&self) -> u32 {
        self.user
    }

    fn applications(&mut self) -> Result<Vec<AndroidApplication>, CatalogError> {
        let response = request(
            &self.socket,
            &ControlRequest::ListApplications { user: self.user },
        )?;
        if !response.ok {
            return Err(CatalogError::Control(response.message));
        }
        response
            .applications
            .ok_or(CatalogError::MissingResponseData("application list"))
    }

    fn icon(&mut self, component: &str, size: u32) -> Result<Vec<u8>, CatalogError> {
        let response = request(
            &self.socket,
            &ControlRequest::ApplicationIcon {
                component: component.to_owned(),
                user: self.user,
                size,
            },
        )?;
        if !response.ok {
            return Err(CatalogError::Control(response.message));
        }
        let encoded = response
            .application_icon
            .ok_or(CatalogError::MissingResponseData("application icon"))?;
        decode_base64(&encoded)
    }

    fn icons(
        &mut self,
        components: &[String],
        size: u32,
    ) -> Result<BTreeMap<String, Vec<u8>>, CatalogError> {
        let response = request(
            &self.socket,
            &ControlRequest::ApplicationIcons {
                components: components.to_vec(),
                user: self.user,
                size,
            },
        )?;
        if !response.ok {
            return Err(CatalogError::Control(response.message));
        }
        response
            .application_icons
            .ok_or(CatalogError::MissingResponseData("application icon batch"))?
            .into_iter()
            .map(|(name, png)| decode_base64(&png).map(|bytes| (name, bytes)))
            .collect()
    }
}

/// Result of one catalog reconciliation.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ReconcileSummary {
    /// Retry this pass after a transport/rendering failure; normal idle needs no timer.
    pub retry_needed: bool,
    /// Number of exported Android launcher activities.
    pub applications: usize,
    /// Desktop files whose content changed.
    pub launchers_updated: usize,
    /// Icons fetched and atomically replaced.
    pub icons_updated: usize,
    /// Obsolete managed desktop files or icons removed.
    pub entries_removed: usize,
}

impl ReconcileSummary {
    /// Whether the visible XDG catalog changed.
    pub fn changed(self) -> bool {
        self.launchers_updated != 0 || self.icons_updated != 0 || self.entries_removed != 0
    }
}

/// Application catalog or filesystem failure.
#[derive(Debug, Error)]
pub enum CatalogError {
    /// No absolute user data directory can be established.
    #[error("HOME and the required XDG base directories are unavailable or relative")]
    MissingHome,
    /// Local lifecycle transport failed.
    #[error(transparent)]
    Lifecycle(#[from] droidloom_supervisor::control::ControlError),
    /// Droidloom rejected a catalog operation.
    #[error("Droidloom catalog request failed: {0}")]
    Control(String),
    /// A successful response omitted its typed payload.
    #[error("Droidloom response omitted {0}")]
    MissingResponseData(&'static str),
    /// Android supplied unsafe or contradictory application metadata.
    #[error("invalid Android application metadata: {0}")]
    InvalidApplication(String),
    /// Android supplied an invalid rendered icon.
    #[error("invalid Android application icon: {0}")]
    InvalidIcon(String),
    /// A user-owned filesystem operation failed.
    #[error("{context}: {source}")]
    Io {
        /// Operation being attempted.
        context: &'static str,
        /// Underlying failure.
        source: io::Error,
    },
    /// The persistent manifest was not serializable.
    #[error("serialize application catalog manifest: {0}")]
    Manifest(#[from] serde_json::Error),
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema: u32,
    android_user: u32,
    applications: BTreeMap<String, ManifestApplication>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ManifestApplication {
    component: String,
    icon_key: String,
    has_icon: bool,
}

/// Atomically reconcile all managed XDG application entries and icons.
///
/// The last good catalog is preserved if Android is temporarily unavailable.
/// Icon failures degrade only that application's icon and are retried on the
/// next pass; they never suppress a valid launcher.
///
/// # Errors
///
/// Fails for an unavailable application list, invalid metadata, or a local
/// filesystem transaction failure.
pub fn reconcile(
    paths: &CatalogPaths,
    source: &mut impl ApplicationSource,
) -> Result<ReconcileSummary, CatalogError> {
    let mut applications = source.applications()?;
    if applications.is_empty() {
        return Err(CatalogError::InvalidApplication(
            "Android returned no launchable activities; preserving the last good catalog".into(),
        ));
    }
    applications.sort_by(|left, right| {
        left.name
            .to_lowercase()
            .cmp(&right.name.to_lowercase())
            .then_with(|| left.component.cmp(&right.component))
    });
    validate_applications(&applications)?;

    create_directory(&paths.applications)?;
    create_directory(&paths.icons)?;
    if let Some(parent) = paths.manifest.parent() {
        create_directory(parent)?;
    }

    let previous = read_manifest(&paths.manifest)?;
    let mut current = Manifest {
        schema: MANIFEST_SCHEMA,
        android_user: source.user(),
        applications: BTreeMap::new(),
    };
    let mut summary = ReconcileSummary {
        applications: applications.len(),
        ..ReconcileSummary::default()
    };
    let mut live_desktops = BTreeSet::new();
    let mut live_icons = BTreeSet::new();

    let needed: Vec<String> = applications
        .iter()
        .filter(|application| {
            let id = application_id(source.user(), &application.component);
            let path = paths.icons.join(format!("droidloom-{id}.png"));
            !previous.applications.get(&id).is_some_and(|entry| {
                entry.component == application.component
                    && entry.has_icon
                    && entry.icon_key == application.icon_key
                    && path.is_file()
            })
        })
        .map(|application| application.component.clone())
        .collect();
    let mut icons = BTreeMap::new();
    for components in needed.chunks(32) {
        match source.icons(components, ICON_SIZE) {
            Ok(batch) => icons.extend(batch),
            Err(error) => {
                summary.retry_needed = true;
                eprintln!("droidloom-applications: icon batch deferred: {error}");
            }
        }
    }

    for application in &applications {
        let id = application_id(source.user(), &application.component);
        let desktop_name = format!("droidloom-{id}.desktop");
        let icon_name = format!("droidloom-{id}");
        let icon_file = format!("{icon_name}.png");
        let icon_path = paths.icons.join(&icon_file);
        let old = previous.applications.get(&id);
        let cached_icon = old.is_some_and(|entry| {
            entry.component == application.component && entry.has_icon && icon_path.is_file()
        });
        let mut has_icon =
            cached_icon && old.is_some_and(|entry| entry.icon_key == application.icon_key);
        let mut stored_icon_key = application.icon_key.clone();

        if !has_icon {
            match icons.remove(&application.component) {
                Some(icon) => {
                    validate_icon(&icon)?;
                    if write_if_changed(&icon_path, &icon)? {
                        summary.icons_updated += 1;
                    }
                    has_icon = true;
                }
                None => {
                    summary.retry_needed = true;
                    eprintln!(
                        "droidloom-applications: icon for {} deferred",
                        application.component
                    );
                    // Keep presenting a previous valid icon, but retain its old
                    // key in the manifest so the next pass retries the update.
                    if let Some(old) = old.filter(|_| cached_icon) {
                        has_icon = true;
                        stored_icon_key.clone_from(&old.icon_key);
                    }
                }
            }
        }

        let desktop_path = paths.applications.join(&desktop_name);
        let resolution = preserved_resolution(&desktop_path)?;
        let mut desktop = desktop_entry(
            application,
            source.user(),
            has_icon.then_some(icon_name.as_str()),
        );
        if let Some(resolution) = resolution {
            desktop = with_launch_resolution(desktop, &resolution);
        }
        if write_if_changed(&desktop_path, desktop.as_bytes())? {
            summary.launchers_updated += 1;
        }
        live_desktops.insert(desktop_name);
        if has_icon {
            live_icons.insert(icon_file);
        }
        current.applications.insert(
            id,
            ManifestApplication {
                component: application.component.clone(),
                icon_key: stored_icon_key,
                has_icon,
            },
        );
    }

    summary.entries_removed += remove_stale(&paths.applications, "desktop", &live_desktops)?;
    summary.entries_removed += remove_legacy_application_entries(&paths.applications)?;
    summary.entries_removed += remove_stale(&paths.icons, "png", &live_icons)?;
    let encoded = serde_json::to_vec_pretty(&current)?;
    let _ = write_if_changed(&paths.manifest, &encoded)?;
    Ok(summary)
}

/// Remove only launcher/icon files recorded by this user's last catalog.
///
/// # Errors
/// Returns an error for malformed catalog identities or filesystem failures.
pub fn remove_managed(paths: &CatalogPaths) -> Result<usize, CatalogError> {
    let manifest = read_manifest(&paths.manifest)?;
    for (id, entry) in &manifest.applications {
        if *id != application_id(manifest.android_user, &entry.component) {
            return Err(CatalogError::InvalidApplication(
                "catalog entry has an invalid managed identity".into(),
            ));
        }
    }
    let mut removed = 0;
    for (id, entry) in &manifest.applications {
        let mut files = vec![paths.applications.join(format!("droidloom-{id}.desktop"))];
        if entry.has_icon {
            files.push(paths.icons.join(format!("droidloom-{id}.png")));
        }
        for path in files {
            match fs::remove_file(&path) {
                Ok(()) => removed += 1,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(io_error("remove managed catalog entry", error)),
            }
        }
    }
    match fs::remove_file(&paths.manifest) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(io_error("remove catalog manifest", error)),
    }
    Ok(removed)
}

fn absolute_path(value: Option<OsString>) -> Option<PathBuf> {
    value.map(PathBuf::from).filter(|path| path.is_absolute())
}

fn decode_base64(encoded: &str) -> Result<Vec<u8>, CatalogError> {
    if encoded.len() % 4 != 0 {
        return Err(CatalogError::InvalidIcon(
            "base64 payload has an invalid length".into(),
        ));
    }
    let mut decoded = Vec::with_capacity(encoded.len() / 4 * 3);
    let chunks = encoded.len() / 4;
    for (index, chunk) in encoded.as_bytes().chunks_exact(4).enumerate() {
        let final_chunk = index + 1 == chunks;
        if !final_chunk && (chunk[2] == b'=' || chunk[3] == b'=') {
            return Err(CatalogError::InvalidIcon(
                "base64 padding appears before the final block".into(),
            ));
        }
        let a = base64_value(chunk[0])?;
        let b = base64_value(chunk[1])?;
        let c = if chunk[2] == b'=' {
            0
        } else {
            base64_value(chunk[2])?
        };
        let d = if chunk[3] == b'=' {
            0
        } else {
            base64_value(chunk[3])?
        };
        if chunk[2] == b'=' && chunk[3] != b'=' {
            return Err(CatalogError::InvalidIcon(
                "base64 payload has invalid padding".into(),
            ));
        }
        decoded.push((a << 2) | (b >> 4));
        if chunk[2] != b'=' {
            decoded.push((b << 4) | (c >> 2));
        }
        if chunk[3] != b'=' {
            decoded.push((c << 6) | d);
        }
    }
    Ok(decoded)
}

fn base64_value(byte: u8) -> Result<u8, CatalogError> {
    match byte {
        b'A'..=b'Z' => Ok(byte - b'A'),
        b'a'..=b'z' => Ok(byte - b'a' + 26),
        b'0'..=b'9' => Ok(byte - b'0' + 52),
        b'+' => Ok(62),
        b'/' => Ok(63),
        _ => Err(CatalogError::InvalidIcon(
            "base64 payload contains an invalid character".into(),
        )),
    }
}

fn validate_applications(applications: &[AndroidApplication]) -> Result<(), CatalogError> {
    let mut components = BTreeSet::new();
    for application in applications {
        if application.name.is_empty()
            || application.name.len() > 1024
            || application.name.contains('\0')
        {
            return Err(CatalogError::InvalidApplication(format!(
                "{} has an invalid display name",
                application.component
            )));
        }
        if !valid_package(&application.package)
            || !valid_component(&application.component, &application.package)
        {
            return Err(CatalogError::InvalidApplication(format!(
                "invalid component {} for package {}",
                application.component, application.package
            )));
        }
        if application.icon_key.is_empty()
            || application.icon_key.len() > 128
            || !application.icon_key.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b':')
            })
        {
            return Err(CatalogError::InvalidApplication(format!(
                "{} has an invalid icon identity",
                application.component
            )));
        }
        if !components.insert(&application.component) {
            return Err(CatalogError::InvalidApplication(format!(
                "duplicate launcher component {}",
                application.component
            )));
        }
    }
    Ok(())
}

fn valid_package(package: &str) -> bool {
    !package.is_empty()
        && package.len() <= 255
        && package.split('.').count() >= 2
        && package
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_'))
}

fn valid_component(component: &str, package: &str) -> bool {
    component.len() <= 511
        && component.split_once('/').is_some_and(|(owner, activity)| {
            owner == package
                && !activity.is_empty()
                && activity
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'$'))
        })
}

fn validate_icon(icon: &[u8]) -> Result<(), CatalogError> {
    if icon.len() > MAX_ICON_BYTES {
        return Err(CatalogError::InvalidIcon(format!(
            "PNG is {} bytes; maximum is {MAX_ICON_BYTES}",
            icon.len()
        )));
    }
    if !icon.starts_with(PNG_SIGNATURE) {
        return Err(CatalogError::InvalidIcon(
            "payload is not a PNG image".into(),
        ));
    }
    Ok(())
}

fn preserved_resolution(path: &Path) -> Result<Option<String>, CatalogError> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io_error("inspect desktop resolution", error)),
        Ok(metadata) if !metadata.is_file() || metadata.len() > 1024 * 1024 => {
            return Err(CatalogError::InvalidApplication(
                "unsafe desktop entry for launch resolution".into(),
            ));
        }
        Ok(_) => {}
    }
    let content =
        fs::read_to_string(path).map_err(|error| io_error("read desktop resolution", error))?;
    let mut in_entry = false;
    let mut resolution = None;
    for line in content.lines() {
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
        }
        if in_entry {
            if let Some(value) = line.strip_prefix("X-Droidloom-Resolution=") {
                if resolution.is_some() {
                    return Err(CatalogError::InvalidApplication(
                        "duplicate desktop launch resolution".into(),
                    ));
                }
                resolution = Some(
                    parse_launch_resolution(value)
                        .map_err(|error| CatalogError::InvalidApplication(error.to_string()))?,
                );
            }
        }
    }
    Ok(resolution)
}

fn with_launch_resolution(desktop: String, resolution: &str) -> String {
    let mut result = String::new();
    for line in desktop.lines() {
        result.push_str(line);
        if line.starts_with("Exec=") {
            result.push_str(" --resolution ");
            result.push_str(resolution);
        }
        result.push('\n');
    }
    result.push_str(&format!("X-Droidloom-Resolution={resolution}\n"));
    result
}

fn desktop_entry(application: &AndroidApplication, user: u32, icon: Option<&str>) -> String {
    let icon = icon.unwrap_or("application-x-executable");
    format!(
        "[Desktop Entry]\nType=Application\nVersion=1.0\nName={}\nComment=Android application via Droidloom\nTryExec=/usr/bin/droidloomctl\nExec=/usr/bin/droidloomctl launch {} --component {} --user {}\nIcon={}\nTerminal=false\nStartupNotify=true\nStartupWMClass={}\nX-Droidloom-Managed=true\nX-Droidloom-Package={}\nX-Droidloom-Component={}\nX-Droidloom-User={}\n",
        escape_desktop_value(&application.name),
        application.package,
        application.component,
        user,
        icon,
        application.package,
        application.package,
        application.component,
        user
    )
}

fn escape_desktop_value(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            character => escaped.push(character),
        }
    }
    escaped
}

fn read_manifest(path: &Path) -> Result<Manifest, CatalogError> {
    let encoded = match fs::read(path) {
        Ok(encoded) => encoded,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Manifest::default()),
        Err(source) => return Err(io_error("read application catalog", source)),
    };
    let manifest: Manifest = match serde_json::from_slice(&encoded) {
        Ok(manifest) => manifest,
        Err(error) => {
            eprintln!(
                "droidloom-applications: replacing invalid catalog manifest {}: {error}",
                path.display()
            );
            return Ok(Manifest::default());
        }
    };
    if manifest.schema != MANIFEST_SCHEMA {
        return Ok(Manifest::default());
    }
    Ok(manifest)
}

fn create_directory(path: &Path) -> Result<(), CatalogError> {
    fs::create_dir_all(path).map_err(|source| io_error("create catalog directory", source))
}

fn write_if_changed(path: &Path, contents: &[u8]) -> Result<bool, CatalogError> {
    match fs::read(path) {
        Ok(existing) if existing == contents => return Ok(false),
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(source) => return Err(io_error("read managed catalog file", source)),
    }
    let parent = path.parent().ok_or_else(|| {
        io_error(
            "resolve managed catalog parent",
            io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"),
        )
    })?;
    create_directory(parent)?;
    let mut temporary = NamedTempFile::new_in(parent)
        .map_err(|source| io_error("create temporary catalog file", source))?;
    temporary
        .write_all(contents)
        .and_then(|()| temporary.as_file().sync_all())
        .map_err(|source| io_error("write temporary catalog file", source))?;
    temporary
        .persist(path)
        .map_err(|error| io_error("replace managed catalog file", error.error))?;
    Ok(true)
}

fn remove_stale(
    directory: &Path,
    extension: &str,
    live: &BTreeSet<String>,
) -> Result<usize, CatalogError> {
    let mut removed = 0;
    for entry in fs::read_dir(directory).map_err(|source| io_error("scan catalog", source))? {
        let entry = entry.map_err(|source| io_error("read catalog entry", source))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.starts_with("droidloom-")
            || Path::new(name).extension().and_then(|value| value.to_str()) != Some(extension)
            || live.contains(name)
        {
            continue;
        }
        let metadata = entry
            .metadata()
            .map_err(|source| io_error("inspect stale catalog entry", source))?;
        if metadata.is_file() {
            fs::remove_file(entry.path())
                .map_err(|source| io_error("remove stale catalog entry", source))?;
            removed += 1;
        }
    }
    Ok(removed)
}

fn remove_legacy_application_entries(applications: &Path) -> Result<usize, CatalogError> {
    let legacy = applications.join("droidloom");
    let entries = match fs::read_dir(&legacy) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(source) => return Err(io_error("scan legacy application catalog", source)),
    };
    let mut removed = 0;
    for entry in entries {
        let entry = entry.map_err(|source| io_error("read legacy catalog entry", source))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.starts_with("droidloom-")
            || Path::new(name).extension().and_then(|value| value.to_str()) != Some("desktop")
        {
            continue;
        }
        let file_type = entry
            .file_type()
            .map_err(|source| io_error("inspect legacy catalog entry", source))?;
        if file_type.is_file() {
            fs::remove_file(entry.path())
                .map_err(|source| io_error("remove legacy catalog entry", source))?;
            removed += 1;
        }
    }
    match fs::remove_dir(legacy) {
        Ok(()) => {}
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::DirectoryNotEmpty
            ) => {}
        Err(source) => return Err(io_error("remove legacy catalog directory", source)),
    }
    Ok(removed)
}

fn io_error(context: &'static str, source: io::Error) -> CatalogError {
    CatalogError::Io { context, source }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\nfixture";

    struct FixtureSource {
        user: u32,
        applications: Vec<AndroidApplication>,
        icon_requests: usize,
    }

    impl ApplicationSource for FixtureSource {
        fn user(&self) -> u32 {
            self.user
        }

        fn applications(&mut self) -> Result<Vec<AndroidApplication>, CatalogError> {
            Ok(self.applications.clone())
        }

        fn icon(&mut self, _component: &str, _size: u32) -> Result<Vec<u8>, CatalogError> {
            self.icon_requests += 1;
            Ok(PNG.to_vec())
        }
    }

    fn application(
        name: &str,
        package: &str,
        component: &str,
        icon_key: &str,
    ) -> AndroidApplication {
        AndroidApplication {
            name: name.into(),
            package: package.into(),
            component: component.into(),
            icon_key: icon_key.into(),
        }
    }

    fn fixture_paths(root: &Path) -> CatalogPaths {
        CatalogPaths::new(
            root.join("data/applications"),
            root.join("data/icons/hicolor/128x128/apps"),
            root.join("state/droidloom/application-catalog-v1.json"),
        )
    }

    #[test]
    fn icon_batches_are_bounded_cached_and_retry_only_missing_entries() {
        struct BatchSource {
            applications: Vec<AndroidApplication>,
            batches: Vec<Vec<String>>,
            missing: bool,
        }
        impl ApplicationSource for BatchSource {
            fn user(&self) -> u32 {
                0
            }
            fn applications(&mut self) -> Result<Vec<AndroidApplication>, CatalogError> {
                Ok(self.applications.clone())
            }
            fn icon(&mut self, _: &str, _: u32) -> Result<Vec<u8>, CatalogError> {
                panic!("batch path must be used")
            }
            fn icons(
                &mut self,
                components: &[String],
                size: u32,
            ) -> Result<BTreeMap<String, Vec<u8>>, CatalogError> {
                assert_eq!(size, ICON_SIZE);
                self.batches.push(components.to_vec());
                Ok(components
                    .iter()
                    .filter(|component| !self.missing || *component != "org.example.app0/.Main")
                    .map(|component| (component.clone(), PNG.to_vec()))
                    .collect())
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let paths = fixture_paths(dir.path());
        let mut source = BatchSource {
            applications: (0..40)
                .map(|n| {
                    application(
                        &format!("App {n:02}"),
                        &format!("org.example.app{n}"),
                        &format!("org.example.app{n}/.Main"),
                        "one",
                    )
                })
                .collect(),
            batches: Vec::new(),
            missing: true,
        };
        let first = reconcile(&paths, &mut source).unwrap();
        assert_eq!(first.launchers_updated, 40);
        assert_eq!(first.icons_updated, 39);
        assert!(first.retry_needed);
        assert_eq!(
            source.batches.iter().map(Vec::len).collect::<Vec<_>>(),
            vec![32, 8]
        );
        source.batches.clear();
        source.missing = false;
        let next = reconcile(&paths, &mut source).unwrap();
        assert!(!next.retry_needed);
        assert_eq!(
            source.batches,
            vec![vec!["org.example.app0/.Main".to_owned()]]
        );
        source.batches.clear();
        assert!(!reconcile(&paths, &mut source).unwrap().changed());
        assert!(source.batches.is_empty());
        // An update that races removal keeps the previous valid icon and its old key.
        source.applications[0].icon_key = "two".into();
        source.missing = true;
        assert!(reconcile(&paths, &mut source).unwrap().retry_needed);
        let old = read_manifest(&paths.manifest).unwrap();
        let entry = old
            .applications
            .get(&application_id(0, "org.example.app0/.Main"))
            .unwrap();
        assert!(entry.has_icon);
        assert_eq!(entry.icon_key, "one");
        source.missing = false;
        assert!(!reconcile(&paths, &mut source).unwrap().retry_needed);
        let next = read_manifest(&paths.manifest).unwrap();
        assert_eq!(
            next.applications
                .get(&application_id(0, "org.example.app0/.Main"))
                .unwrap()
                .icon_key,
            "two"
        );
    }

    #[test]
    fn package_removal_uses_the_catalog_and_retains_unrelated_desktop_files() {
        let temp = tempfile::tempdir().unwrap();
        let paths = fixture_paths(temp.path());
        let mut source = FixtureSource {
            user: 0,
            applications: vec![application(
                "Example",
                "org.example.app",
                "org.example.app/.Main",
                "one",
            )],
            icon_requests: 0,
        };
        reconcile(&paths, &mut source).unwrap();
        let unrelated = paths.applications.join("droidloom-personal.desktop");
        fs::write(&unrelated, b"user-created shortcut").unwrap();
        assert_eq!(remove_managed(&paths).unwrap(), 2);
        assert!(unrelated.is_file());
        assert!(!paths.manifest.exists());
        assert_eq!(remove_managed(&paths).unwrap(), 0);
    }

    #[test]
    fn package_removal_rejects_catalog_path_traversal_before_deleting_anything() {
        let temp = tempfile::tempdir().unwrap();
        let paths = fixture_paths(temp.path());
        let mut source = FixtureSource {
            user: 0,
            applications: vec![application(
                "Example",
                "org.example.app",
                "org.example.app/.Main",
                "one",
            )],
            icon_requests: 0,
        };
        reconcile(&paths, &mut source).unwrap();
        let mut manifest = read_manifest(&paths.manifest).unwrap();
        let entry = manifest.applications.values().next().unwrap().clone();
        manifest
            .applications
            .insert("../../unrelated".into(), entry);
        fs::write(&paths.manifest, serde_json::to_vec(&manifest).unwrap()).unwrap();
        assert!(remove_managed(&paths).is_err());
        assert_eq!(fs::read_dir(&paths.applications).unwrap().count(), 1);
        assert_eq!(fs::read_dir(&paths.icons).unwrap().count(), 1);
    }

    #[test]
    fn xdg_paths_never_accept_relative_overrides() {
        let paths = CatalogPaths::from_environment(
            Some("relative".into()),
            None,
            Some("/home/tester".into()),
        )
        .unwrap();
        assert_eq!(
            paths.applications,
            Path::new("/home/tester/.local/share/applications")
        );
    }

    #[test]
    fn icon_transport_base64_is_strict() {
        assert_eq!(decode_base64("iVBORw==").unwrap(), b"\x89PNG");
        assert!(decode_base64("iV=ORw==").is_err());
        assert!(decode_base64("not base64").is_err());
    }

    #[test]
    fn reconciliation_is_atomic_idempotent_and_removes_stale_entries() {
        let temporary = tempfile::tempdir().unwrap();
        let paths = fixture_paths(temporary.path());
        let mut source = FixtureSource {
            user: 0,
            applications: vec![application(
                "Firefox",
                "org.mozilla.firefox",
                "org.mozilla.firefox/.App",
                "1:one",
            )],
            icon_requests: 0,
        };

        let first = reconcile(&paths, &mut source).unwrap();
        assert_eq!(first.applications, 1);
        assert_eq!(first.launchers_updated, 1);
        assert_eq!(first.icons_updated, 1);
        assert_eq!(source.icon_requests, 1);

        let second = reconcile(&paths, &mut source).unwrap();
        assert!(!second.changed());
        assert_eq!(source.icon_requests, 1);

        source.applications = vec![application(
            "Settings",
            "com.android.settings",
            "com.android.settings/.Settings",
            "2:two",
        )];
        let third = reconcile(&paths, &mut source).unwrap();
        assert_eq!(third.entries_removed, 2);
        assert_eq!(fs::read_dir(&paths.applications).unwrap().count(), 1);
    }

    #[test]
    fn reconciliation_preserves_validated_launch_resolution() {
        let temporary = tempfile::tempdir().unwrap();
        let paths = fixture_paths(temporary.path());
        let mut source = FixtureSource {
            user: 0,
            applications: vec![application(
                "Game",
                "org.example.game",
                "org.example.game/.Main",
                "one",
            )],
            icon_requests: 0,
        };
        reconcile(&paths, &mut source).unwrap();
        let path = paths.applications.join(format!(
            "droidloom-{}.desktop",
            application_id(0, "org.example.game/.Main")
        ));
        let mut desktop = fs::read_to_string(&path).unwrap();
        desktop.push_str("X-Droidloom-Resolution=2560x1440\n");
        fs::write(&path, desktop).unwrap();
        source.applications[0].name = "Renamed Game".into();
        reconcile(&paths, &mut source).unwrap();
        let desktop = fs::read_to_string(&path).unwrap();
        assert!(desktop.contains("Name=Renamed Game\n"));
        assert!(desktop.contains("--user 0 --resolution 2560x1440\n"));
        assert_eq!(desktop.matches("X-Droidloom-Resolution=").count(), 1);
        assert_eq!(reconcile(&paths, &mut source).unwrap().launchers_updated, 0);
        fs::write(
            &path,
            "[Desktop Entry]\nX-Droidloom-Resolution=2560x1440 --other\n",
        )
        .unwrap();
        assert!(preserved_resolution(&path).is_err());
    }

    #[test]
    fn desktop_entry_uses_only_launch_and_escapes_labels() {
        let application = application(
            "Firefox\nAndroid",
            "org.mozilla.firefox",
            "org.mozilla.firefox/.App",
            "one",
        );
        let entry = desktop_entry(&application, 0, Some("droidloom-icon"));
        assert!(entry.contains("Name=Firefox\\nAndroid"));
        assert!(entry.contains(
            "Exec=/usr/bin/droidloomctl launch org.mozilla.firefox --component org.mozilla.firefox/.App --user 0"
        ));
        assert!(!entry.contains("droidloomctl start"));
    }

    #[test]
    fn empty_query_preserves_the_previous_catalog() {
        let temporary = tempfile::tempdir().unwrap();
        let paths = fixture_paths(temporary.path());
        fs::create_dir_all(&paths.applications).unwrap();
        let existing = paths.applications.join("droidloom-existing.desktop");
        fs::write(&existing, "last good").unwrap();
        let mut source = FixtureSource {
            user: 0,
            applications: Vec::new(),
            icon_requests: 0,
        };
        assert!(reconcile(&paths, &mut source).is_err());
        assert_eq!(fs::read_to_string(existing).unwrap(), "last good");
    }

    #[test]
    fn reconciliation_migrates_only_managed_legacy_entries() {
        let temporary = tempfile::tempdir().unwrap();
        let paths = fixture_paths(temporary.path());
        let legacy = paths.applications.join("droidloom");
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join("droidloom-old.desktop"), "managed").unwrap();
        fs::write(legacy.join("keep.desktop"), "unmanaged").unwrap();
        let mut source = FixtureSource {
            user: 0,
            applications: vec![application(
                "Settings",
                "com.android.settings",
                "com.android.settings/.Settings",
                "one",
            )],
            icon_requests: 0,
        };

        let summary = reconcile(&paths, &mut source).unwrap();
        assert_eq!(summary.entries_removed, 1);
        assert!(!legacy.join("droidloom-old.desktop").exists());
        assert!(legacy.join("keep.desktop").exists());
        assert_eq!(
            fs::read_dir(&paths.applications)
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| entry
                    .path()
                    .extension()
                    .is_some_and(|value| value == "desktop"))
                .count(),
            1
        );
    }
}
