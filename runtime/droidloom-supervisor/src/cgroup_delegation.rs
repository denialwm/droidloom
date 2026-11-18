//! The cell's memory and process accounting under a delegated unit.
//!
//! The cell's cgroup root is the group droidloomd runs in. The unit runs it in
//! an empty subgroup (`DelegateSubgroup=`), where Android can attach its BPF
//! programs whatever the host put on the unit's own group. systemd leaves the
//! controllers inside a delegated group to the program, so droidloomd turns
//! on the ones the cell had before, which its Settings statistics read.

use std::ffi::CString;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

const CONTROLLERS: [&str; 2] = ["memory", "pids"];

/// Turns the cell's controllers on in droidloomd's parent group when systemd
/// delegated that group; does nothing anywhere else.
pub fn enable_delegated_controllers() {
    let result = fs::read_to_string("/proc/self/cgroup")
        .and_then(|membership| enable(Path::new("/sys/fs/cgroup"), &membership));
    match result {
        Ok(Some(request)) => eprintln!("droidloomd: delegated cgroup controllers {request}"),
        Ok(None) => {}
        Err(error) => {
            eprintln!("droidloomd: cell memory and process accounting unavailable: {error}")
        }
    }
}

fn enable(root: &Path, membership: &str) -> io::Result<Option<String>> {
    let Some(own) = membership.lines().find_map(|line| line.strip_prefix("0::")) else {
        return Ok(None);
    };
    let Some(parent) = Path::new(own.trim()).parent() else {
        return Ok(None);
    };
    let parent = root.join(parent.strip_prefix("/").unwrap_or(parent));
    if parent == root || !delegated(&parent) {
        return Ok(None);
    }
    let available = fs::read_to_string(parent.join("cgroup.controllers"))?;
    let enabled = fs::read_to_string(parent.join("cgroup.subtree_control"))?;
    let listed =
        |list: &str, controller: &str| list.split_whitespace().any(|name| name == controller);
    let request: Vec<String> = CONTROLLERS
        .iter()
        .filter(|controller| listed(&available, controller) && !listed(&enabled, controller))
        .map(|controller| format!("+{controller}"))
        .collect();
    if request.is_empty() {
        return Ok(None);
    }
    let request = request.join(" ");
    fs::write(parent.join("cgroup.subtree_control"), &request)?;
    Ok(Some(request))
}

/// systemd marks a group it delegated with `trusted.delegate=1`, and with
/// `user.delegate=1` where trusted attributes are unavailable.
fn delegated(group: &Path) -> bool {
    ["trusted.delegate", "user.delegate"]
        .iter()
        .any(|name| attribute(group, name).is_some_and(|value| value == b"1"))
}

fn attribute(path: &Path, name: &str) -> Option<Vec<u8>> {
    let path = CString::new(path.as_os_str().as_bytes()).ok()?;
    let name = CString::new(name).ok()?;
    let mut value = [0_u8; 8];
    // SAFETY: both strings are NUL-terminated and `value` is a live buffer of
    // the length passed.
    let length = unsafe {
        libc::getxattr(
            path.as_ptr(),
            name.as_ptr(),
            value.as_mut_ptr().cast(),
            value.len(),
        )
    };
    usize::try_from(length)
        .ok()
        .map(|length| value[..length].to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group(root: &Path, path: &str, available: &str, enabled: &str) -> std::path::PathBuf {
        let group = root.join(path);
        fs::create_dir_all(&group).unwrap();
        fs::write(group.join("cgroup.controllers"), available).unwrap();
        fs::write(group.join("cgroup.subtree_control"), enabled).unwrap();
        group
    }

    fn mark_delegated(group: &Path) {
        let path = CString::new(group.as_os_str().as_bytes()).unwrap();
        // SAFETY: NUL-terminated strings and a live one-byte value.
        let status = unsafe {
            libc::setxattr(
                path.as_ptr(),
                c"user.delegate".as_ptr(),
                b"1".as_ptr().cast(),
                1,
                0,
            )
        };
        assert_eq!(status, 0, "{}", io::Error::last_os_error());
    }

    #[test]
    fn a_delegated_parent_gives_the_cell_memory_and_pids() {
        let root = tempfile::tempdir().unwrap();
        let unit = group(
            root.path(),
            "system.slice/droidloomd.service",
            "cpu memory pids\n",
            "pids\n",
        );
        mark_delegated(&unit);
        let membership = "0::/system.slice/droidloomd.service/supervisor\n";
        assert_eq!(
            enable(root.path(), membership).unwrap().as_deref(),
            Some("+memory")
        );
        assert_eq!(
            fs::read_to_string(unit.join("cgroup.subtree_control")).unwrap(),
            "+memory"
        );
    }

    #[test]
    fn groups_systemd_did_not_delegate_stay_untouched() {
        let root = tempfile::tempdir().unwrap();
        let slice = group(root.path(), "system.slice", "memory pids\n", "\n");
        // The unit without delegation: droidloomd's parent is the host's slice.
        assert_eq!(
            enable(root.path(), "0::/system.slice/droidloomd.service\n").unwrap(),
            None
        );
        assert_eq!(
            fs::read_to_string(slice.join("cgroup.subtree_control")).unwrap(),
            "\n"
        );
        assert_eq!(enable(root.path(), "0::/\n").unwrap(), None);
        assert_eq!(enable(root.path(), "1:name=systemd:/\n").unwrap(), None);
    }
}
