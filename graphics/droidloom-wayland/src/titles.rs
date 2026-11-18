//! Window titles. A window is titled with its app's name, the `Name=` of the
//! launcher entry the application catalog wrote for its package
//! (`$XDG_DATA_HOME/applications/droidloom-*.desktop`), and with the package
//! name when no entry names it. The app id stays the package: window rules
//! and `StartupWMClass` match it.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

pub struct Titles {
    applications: Option<PathBuf>,
}

impl Titles {
    pub fn new(data_home: Option<OsString>, home: Option<OsString>) -> Self {
        let root = data_home
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .or_else(|| {
                home.map(PathBuf::from)
                    .filter(|path| path.is_absolute())
                    .map(|home| home.join(".local/share"))
            });
        Self {
            applications: root.map(|root| root.join("applications")),
        }
    }

    /// The app name for `package`, or the package itself.
    pub fn title(&self, package: &str) -> String {
        self.applications
            .as_deref()
            .and_then(|folder| label(folder, package))
            .unwrap_or_else(|| package.to_owned())
    }
}

/// The name of the first managed entry for `package`, by file name.
fn label(folder: &Path, package: &str) -> Option<String> {
    let mut entries: Vec<PathBuf> = fs::read_dir(folder)
        .ok()?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("droidloom-") && name.ends_with(".desktop"))
        })
        .collect();
    entries.sort();
    entries
        .iter()
        .find_map(|path| entry_name(&fs::read_to_string(path).ok()?, package))
}

fn entry_name(entry: &str, package: &str) -> Option<String> {
    let mut name = None;
    let mut managed = false;
    let mut matches = false;
    for line in entry.lines() {
        if let Some(value) = line.strip_prefix("Name=") {
            name = Some(unescape(value));
        } else if line == "X-Droidloom-Managed=true" {
            managed = true;
        } else if let Some(value) = line.strip_prefix("X-Droidloom-Package=") {
            matches = value == package;
        }
    }
    name.filter(|name| managed && matches && !name.trim().is_empty())
}

/// Undoes the catalog's escaping of `\\`, `\n`, `\r` and `\t`; a title is
/// one line, so the line breaks become spaces.
fn unescape(value: &str) -> String {
    let mut result = String::with_capacity(value.len());
    let mut characters = value.chars();
    while let Some(character) = characters.next() {
        if character != '\\' {
            result.push(character);
            continue;
        }
        match characters.next() {
            Some('\\') => result.push('\\'),
            Some('n' | 'r' | 't') => result.push(' '),
            Some(other) => {
                result.push('\\');
                result.push(other);
            }
            None => result.push('\\'),
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, package: &str) -> String {
        format!(
            "[Desktop Entry]\nType=Application\nName={name}\nStartupWMClass={package}\nX-Droidloom-Managed=true\nX-Droidloom-Package={package}\n"
        )
    }

    #[test]
    fn titles_come_from_the_catalog_entry_of_the_package() {
        let home = tempfile::tempdir().unwrap();
        let folder = home.path().join(".local/share/applications");
        fs::create_dir_all(&folder).unwrap();
        fs::write(
            folder.join("droidloom-0-settings.desktop"),
            entry("Settings", "com.android.settings"),
        )
        .unwrap();
        fs::write(
            folder.join("droidloom-0-firefox.desktop"),
            entry("Firefox\\nNightly", "org.mozilla.firefox"),
        )
        .unwrap();
        fs::write(
            folder.join("droidloom-0-plain.desktop"),
            "[Desktop Entry]\nName=Unmanaged\nX-Droidloom-Package=org.example.plain\n",
        )
        .unwrap();
        let titles = Titles::new(None, Some(home.path().into()));
        assert_eq!(titles.title("com.android.settings"), "Settings");
        assert_eq!(titles.title("org.mozilla.firefox"), "Firefox Nightly");
        assert_eq!(titles.title("org.example.plain"), "org.example.plain");
        assert_eq!(titles.title("org.example.missing"), "org.example.missing");
    }

    #[test]
    fn relative_or_missing_homes_fall_back_to_the_package() {
        let titles = Titles::new(Some("relative".into()), None);
        assert_eq!(titles.title("com.android.settings"), "com.android.settings");
    }
}
