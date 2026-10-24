//! Names Droidloom gives Android launcher activities on the host.

use sha2::{Digest, Sha256};

/// The ID in `droidloom-<id>.desktop` and the `droidloom-<id>` icon for one
/// launcher activity (`package/class`) of one Android user. Desktop files,
/// icons and notifications name the app by it.
#[must_use]
pub fn application_id(user: u32, component: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(user.to_le_bytes());
    digest.update([0]);
    digest.update(component.as_bytes());
    hex::encode(digest.finalize())[..32].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_stay_stable_for_installed_desktop_files() {
        assert_eq!(
            application_id(0, "org.mozilla.firefox/org.mozilla.fenix.HomeActivity"),
            "9051d914308e4ce692296d67fbb969a2"
        );
    }
}
