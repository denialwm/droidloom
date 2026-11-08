//! Preserve project and dependency notices in binary distributions.
use crate::util::*;
use serde_json::{Value, json};
use std::{collections::BTreeSet, path::Path, process::Command};

pub fn project(repo: &Path, destination: &Path) -> Result<()> {
    for name in ["LICENSE", "THIRD_PARTY.md"] {
        copy(&repo.join(name), &destination.join(name))?;
    }
    for path in files(&repo.join("LICENSES"))? {
        copy(&path, &destination.join(path.strip_prefix(repo)?))?;
    }
    for name in [
        "android/native-bridge/NOTICE.txt",
        "android/manifest/native-bridge-lock.json",
        "protocol/denial-frame-timeline-v1.xml",
        "graphics/droidloom-wayland/protocol/denial-text-input-panel-v1.xml",
        "graphics/droidloom-wayland/protocol/denial-insets-v1.xml",
        "runtime/droidloom-devices/protocol/denial-fingerprint-verify-v1.xml",
        "android/device/droidloom_arm64/android.software.activities_on_secondary_displays.xml",
    ] {
        copy(&repo.join(name), &destination.join("upstream").join(name))?;
    }
    Ok(())
}

pub fn rust(repo: &Path, destination: &Path) -> Result<()> {
    let metadata: Value =
        serde_json::from_str(&output(Command::new("cargo").current_dir(repo).args([
            "metadata",
            "--locked",
            "--format-version",
            "1",
        ]))?)?;
    rust_from_metadata(
        &metadata,
        &repo.join("packaging/licenses/cargo"),
        destination,
    )
}

fn is_notice_path(path: &Path) -> bool {
    path.components().any(|part| {
        let name = part.as_os_str().to_string_lossy().to_ascii_lowercase();
        [
            "license",
            "licence",
            "copying",
            "notice",
            "copyright",
            "authors",
        ]
        .iter()
        .any(|prefix| name.starts_with(prefix))
    })
}

fn rust_from_metadata(metadata: &Value, supplements: &Path, destination: &Path) -> Result<()> {
    let workspace: BTreeSet<_> = metadata["workspace_members"]
        .as_array()
        .ok_or("Cargo metadata has no workspace members")?
        .iter()
        .filter_map(Value::as_str)
        .collect();
    let mut inventory = Vec::new();
    for package in metadata["packages"]
        .as_array()
        .ok_or("Cargo metadata has no packages")?
    {
        let id = package["id"]
            .as_str()
            .ok_or("Cargo package has no identity")?;
        if workspace.contains(id) {
            continue;
        }
        let name = package["name"]
            .as_str()
            .ok_or("Cargo package has no name")?;
        let version = package["version"]
            .as_str()
            .ok_or("Cargo package has no version")?;
        let root = Path::new(
            package["manifest_path"]
                .as_str()
                .ok_or("Cargo package has no manifest")?,
        )
        .parent()
        .ok_or("Cargo manifest has no parent")?;
        let mut notices = BTreeSet::new();
        for path in files(root)? {
            let relative = path.strip_prefix(root)?;
            if is_notice_path(relative) {
                notices.insert(relative.to_owned());
            }
        }
        if let Some(path) = package["license_file"].as_str() {
            // Cargo resolves license-file relative to the package manifest.
            let path = root.join(path).canonicalize()?;
            notices.insert(path.strip_prefix(root.canonicalize()?)?.to_owned());
        }
        let directory = format!("{name}-{version}");
        let supplement = supplements.join(&directory);
        let supplemental = if supplement.is_dir() {
            files(&supplement)?
        } else {
            Vec::new()
        };
        if notices.is_empty() && supplemental.is_empty() {
            return fail(format!(
                "no upstream license text found for Cargo dependency {name} {version}"
            ));
        }
        for relative in &notices {
            copy(
                &root.join(relative),
                &destination.join(&directory).join(relative),
            )?;
        }
        for path in supplemental {
            let relative = Path::new("supplemental").join(path.strip_prefix(&supplement)?);
            copy(&path, &destination.join(&directory).join(&relative))?;
            notices.insert(relative);
        }
        inventory.push(json!({
            "name": name,
            "version": version,
            "license": package["license"],
            "source": package["source"],
            "repository": package["repository"],
            "directory": directory,
            "notices": notices,
        }));
    }
    inventory.sort_by_key(|entry| {
        (
            entry["name"].as_str().unwrap().to_owned(),
            entry["version"].as_str().unwrap().to_owned(),
        )
    });
    write(
        &destination.join("index.json"),
        serde_json::to_vec_pretty(&inventory)?,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn preserves_nested_notices_and_explicit_license_file_without_shipping_sources() {
        let tree = tempfile::tempdir().unwrap();
        let upstream = tree.path().join("dependency");
        for (path, text) in [
            ("Cargo.toml", "[package]"),
            ("LEGAL.txt", "custom upstream terms"),
            (
                "third-party/licenses/attribution.txt",
                "third-party attribution",
            ),
            ("src/lib.rs", "implementation"),
        ] {
            write(&upstream.join(path), text).unwrap();
        }
        let metadata = json!({
            "workspace_members": ["local"],
            "packages": [
                {"id": "local"},
                {
                    "id": "dependency", "name": "dependency", "version": "1.0.0",
                    "manifest_path": upstream.join("Cargo.toml"),
                    "license_file": "LEGAL.txt", "license": null,
                    "source": "registry+example"
                }
            ]
        });
        let output = tree.path().join("notices");
        rust_from_metadata(&metadata, &tree.path().join("supplements"), &output).unwrap();
        assert_eq!(
            fs::read_to_string(output.join("dependency-1.0.0/LEGAL.txt")).unwrap(),
            "custom upstream terms"
        );
        assert!(
            output
                .join("dependency-1.0.0/third-party/licenses/attribution.txt")
                .is_file()
        );
        assert!(!output.join("dependency-1.0.0/src").exists());
        let index: Value =
            serde_json::from_slice(&fs::read(output.join("index.json")).unwrap()).unwrap();
        assert_eq!(index.as_array().unwrap().len(), 1);
        assert_eq!(index[0]["license"], Value::Null);
    }

    #[test]
    fn supplements_a_published_crate_that_omits_its_license_text() {
        let tree = tempfile::tempdir().unwrap();
        let upstream = tree.path().join("dependency");
        write(&upstream.join("Cargo.toml"), "").unwrap();
        let supplements = tree.path().join("supplements");
        write(
            &supplements.join("dependency-1.0.0/LICENSE"),
            "upstream MIT terms",
        )
        .unwrap();
        write(
            &supplements.join("dependency-1.0.0/NOTICE.md"),
            "upstream provenance",
        )
        .unwrap();
        let metadata = json!({
            "workspace_members": [],
            "packages": [{
                "id": "dependency", "name": "dependency", "version": "1.0.0",
                "manifest_path": upstream.join("Cargo.toml"), "license": "MIT"
            }]
        });
        let output = tree.path().join("out");
        rust_from_metadata(&metadata, &supplements, &output).unwrap();
        assert_eq!(
            fs::read_to_string(output.join("dependency-1.0.0/supplemental/LICENSE")).unwrap(),
            "upstream MIT terms"
        );
        assert!(!output.join("dependency-1.0.0/Cargo.toml").exists());
    }

    #[test]
    fn missing_dependency_notice_is_reported_instead_of_silently_omitted() {
        let tree = tempfile::tempdir().unwrap();
        write(&tree.path().join("Cargo.toml"), "").unwrap();
        let metadata = json!({
            "workspace_members": [],
            "packages": [{
                "id": "missing", "name": "missing", "version": "1.0.0",
                "manifest_path": tree.path().join("Cargo.toml"), "license": "MIT"
            }]
        });
        assert!(
            rust_from_metadata(
                &metadata,
                &tree.path().join("supplements"),
                &tree.path().join("out")
            )
            .unwrap_err()
            .to_string()
            .contains("missing 1.0.0")
        );
    }
}
