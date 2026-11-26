//! Android names the device after its host.
//!
//! The model comes from the device tree, as the host's own About page shows
//! it. The device name is the host's pretty hostname, else its hostname;
//! Android's system server copies it into `Settings.Global.DEVICE_NAME`.

use std::fs;

const DEVICE_TREE_MODEL: &str = "/sys/firmware/devicetree/base/model";
const MACHINE_INFO: &str = "/etc/machine-info";
const HOSTNAME: &str = "/proc/sys/kernel/hostname";
const DEFAULT_MODEL: &str = "Droidloom";
/// Android's limit for a property value, without the terminating NUL.
const VALUE_BYTES: usize = 91;
const MODEL_PROPERTIES: [&str; 2] = ["ro.product.model", "ro.product.product.model"];
pub(crate) const DEVICE_NAME_PROPERTY: &str = "ro.droidloom.device_name";

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Identity {
    pub model: String,
    pub name: Option<String>,
}

pub(crate) fn host() -> Identity {
    let read = |path| fs::read_to_string(path).ok();
    let model = read(DEVICE_TREE_MODEL)
        .and_then(|model| value(&model))
        .unwrap_or_else(|| DEFAULT_MODEL.to_owned());
    let name = read(MACHINE_INFO)
        .and_then(|info| pretty_hostname(&info))
        .or_else(|| read(HOSTNAME).and_then(|name| value(&name)));
    Identity { model, name }
}

/// Replaces the image's model and device name in product properties, which
/// Android loads last.
pub(crate) fn product_properties(original: &str, identity: &Identity) -> String {
    let mut output = String::with_capacity(original.len() + 160);
    for line in original.lines() {
        if line.split_once('=').is_some_and(|(key, _)| {
            let key = key.trim();
            MODEL_PROPERTIES.contains(&key) || key == DEVICE_NAME_PROPERTY
        }) {
            continue;
        }
        output.push_str(line);
        output.push('\n');
    }
    for key in MODEL_PROPERTIES {
        output.push_str(&format!("{key}={}\n", identity.model));
    }
    if let Some(name) = &identity.name {
        output.push_str(&format!("{DEVICE_NAME_PROPERTY}={name}\n"));
    }
    output
}

/// `PRETTY_HOSTNAME` from machine-info(5), which is environment-file syntax.
fn pretty_hostname(info: &str) -> Option<String> {
    let raw = info.lines().find_map(|line| {
        let (key, raw) = line.trim().split_once('=')?;
        (key.trim() == "PRETTY_HOSTNAME").then_some(raw.trim())
    })?;
    let unquoted = match raw.as_bytes() {
        [b'"', .., b'"'] if raw.len() >= 2 => {
            let mut text = String::new();
            let mut escaped = false;
            for c in raw[1..raw.len() - 1].chars() {
                if escaped || c != '\\' {
                    text.push(c);
                    escaped = false;
                } else {
                    escaped = true;
                }
            }
            text
        }
        [b'\'', .., b'\''] if raw.len() >= 2 => raw[1..raw.len() - 1].to_owned(),
        _ => raw.to_owned(),
    };
    value(&unquoted)
}

/// One property line: no control characters, at most Android's length.
fn value(text: &str) -> Option<String> {
    let clean: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let mut clean = clean.trim();
    while clean.len() > VALUE_BYTES {
        let mut end = VALUE_BYTES;
        while !clean.is_char_boundary(end) {
            end -= 1;
        }
        clean = clean[..end].trim_end();
    }
    (!clean.is_empty()).then(|| clean.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_tree_model_is_cleaned() {
        assert_eq!(value("OnePlus 12R\0").as_deref(), Some("OnePlus 12R"));
        assert_eq!(value("\0\n"), None);
        let long = "é".repeat(60);
        assert!(value(&long).unwrap().len() <= VALUE_BYTES);
    }

    #[test]
    fn pretty_hostname_reads_quoted_and_plain_values() {
        let info = "CHASSIS=handset\nPRETTY_HOSTNAME=\"Kai's \\\"phone\\\"\"\n";
        assert_eq!(pretty_hostname(info).as_deref(), Some("Kai's \"phone\""));
        assert_eq!(
            pretty_hostname("PRETTY_HOSTNAME=Aston").as_deref(),
            Some("Aston")
        );
        assert_eq!(pretty_hostname("PRETTY_HOSTNAME=''"), None);
        assert_eq!(pretty_hostname("CHASSIS=handset"), None);
    }

    #[test]
    fn replaces_the_image_identity_and_keeps_everything_else() {
        let identity = Identity {
            model: "OnePlus 12R".into(),
            name: Some("AstonDenial".into()),
        };
        let input = "# image\nro.product.product.model=Cuttlefish arm64 phone\nro.product.product.name=aosp_cf\n";
        let result = product_properties(input, &identity);
        assert_eq!(
            result,
            "# image\nro.product.product.name=aosp_cf\nro.product.model=OnePlus 12R\nro.product.product.model=OnePlus 12R\nro.droidloom.device_name=AstonDenial\n"
        );
        assert_eq!(product_properties(&result, &identity), result);
        let unnamed = Identity {
            model: DEFAULT_MODEL.into(),
            name: None,
        };
        assert!(!product_properties(input, &unnamed).contains(DEVICE_NAME_PROPERTY));
    }
}
