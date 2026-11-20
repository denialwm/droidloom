use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    os::unix::fs::{PermissionsExt, symlink},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};
pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
pub fn fail<T>(message: impl Into<String>) -> Result<T> {
    Err(message.into().into())
}
pub fn run(command: &mut Command) -> Result<()> {
    eprintln!("+ {command:?}");
    let status = command.stdin(Stdio::null()).status()?;
    if !status.success() {
        return fail(format!("command failed ({status}): {command:?}"));
    }
    Ok(())
}
/// Bound the complete compiler process tree, including nested Ninja and JVM workers.
pub fn run_build(command: &mut Command) -> Result<()> {
    if std::env::var_os("DROIDLOOM_COMPILER_CPUS_RESERVED").as_deref()
        == Some(std::ffi::OsStr::new("1"))
    {
        return run(command);
    }
    use std::os::unix::process::CommandExt;
    // SAFETY: cpu_set_t is an integer bitset; sched_getaffinity initializes it.
    let mut allowed: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    if unsafe { libc::sched_getaffinity(0, std::mem::size_of_val(&allowed), &mut allowed) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let cpus: Vec<_> = (0..libc::CPU_SETSIZE as usize)
        .filter(|&cpu| unsafe { libc::CPU_ISSET(cpu, &allowed) })
        .collect();
    for &cpu in cpus.iter().rev().take(cpus.len().saturating_sub(1).min(2)) {
        unsafe { libc::CPU_CLR(cpu, &mut allowed) };
    }
    command.env("DROIDLOOM_COMPILER_CPUS_RESERVED", "1");
    // SAFETY: the callback performs only the async-signal-safe affinity syscall;
    // all allocation and mask construction happened before fork.
    unsafe {
        command.pre_exec(move || {
            if libc::sched_setaffinity(0, std::mem::size_of_val(&allowed), &allowed) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    run(command)
}
pub fn output(command: &mut Command) -> Result<String> {
    let out = command.stdin(Stdio::null()).output()?;
    if !out.status.success() {
        return fail(format!(
            "command failed: {command:?}\n{}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(String::from_utf8(out.stdout)?.trim().to_owned())
}
pub fn hash(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0; 131072];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    Ok(hex::encode(hash.finalize()))
}
pub fn sha256_hex(bytes: impl AsRef<[u8]>) -> String {
    hex::encode(Sha256::digest(bytes))
}
pub fn write(path: &Path, bytes: impl AsRef<[u8]>) -> Result<()> {
    fs::create_dir_all(path.parent().ok_or("path has no parent")?)?;
    if fs::read(path).ok().as_deref() != Some(bytes.as_ref()) {
        fs::write(path, bytes)?;
    }
    Ok(())
}
pub fn durable_write(path: &Path, bytes: impl AsRef<[u8]>) -> Result<()> {
    let parent = path.parent().ok_or("path has no parent")?;
    fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(bytes.as_ref())?;
    temporary.as_file().sync_all()?;
    temporary.persist(path)?;
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}
pub fn copy(source: &Path, destination: &Path) -> Result<()> {
    if !source.is_file() {
        return fail(format!(
            "required build output is missing: {}",
            source.display()
        ));
    }
    fs::create_dir_all(destination.parent().ok_or("path has no parent")?)?;
    // GNU cp preserves holes/reflinks in Android images; no shell is invoked.
    run(Command::new("cp")
        .args([
            "--reflink=auto",
            "--sparse=always",
            "--preserve=mode,timestamps",
            "--",
        ])
        .arg(source)
        .arg(destination))
}
pub fn files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut result = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            return fail(format!(
                "bundle symlinks are forbidden: {}",
                entry.path().display()
            ));
        }
        if kind.is_dir() {
            result.extend(files(&entry.path())?);
        } else if kind.is_file() {
            result.push(entry.path());
        } else {
            return fail("non-regular bundle input");
        }
    }
    result.sort();
    Ok(result)
}
pub fn sync_tree(root: &Path) -> Result<()> {
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            sync_tree(&entry.path())?;
        } else {
            fs::File::open(entry.path())?.sync_all()?;
        }
    }
    fs::File::open(root)?.sync_all()?;
    Ok(())
}
pub fn atomic_link(target: &Path, path: &Path) -> Result<()> {
    fs::create_dir_all(path.parent().ok_or("link has no parent")?)?;
    let directory = tempfile::Builder::new()
        .prefix(".link-")
        .tempdir_in(path.parent().unwrap())?;
    let temporary = directory.path().join("link");
    symlink(target, &temporary)?;
    fs::rename(&temporary, path)?;
    fs::File::open(path.parent().unwrap())?.sync_all()?;
    Ok(())
}
pub fn mode(path: &Path, mode: u32) -> Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    Ok(())
}
pub struct Lock {
    _file: fs::File,
}
impl Lock {
    pub fn acquire(path: &Path) -> Result<Self> {
        Self::take(path, None)
    }
    /// Waits for the holder to finish instead of failing, naming `holder`
    /// once while it waits.
    pub fn wait(path: &Path, holder: &str) -> Result<Self> {
        Self::take(path, Some(holder))
    }
    fn take(path: &Path, waiting_for: Option<&str>) -> Result<Self> {
        fs::create_dir_all(path.parent().unwrap())?;
        let mut file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;
        use std::os::fd::AsRawFd;
        // SAFETY: the owned file descriptor stays open for the lock lifetime.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let Some(holder) = waiting_for else {
                return fail("another Droidloom update is running");
            };
            eprintln!("Waiting for {holder}");
            // SAFETY: as above.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
        }
        // Keep exclusion alive in build children if the updater is killed. Recovery
        // cannot restore source files while an orphaned compiler is reading them.
        if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFD, 0) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        file.set_len(0)?;
        writeln!(file, "{}", std::process::id())?;
        Ok(Self { _file: file })
    }
}
// Closing the final inherited descriptor releases flock, including after SIGKILL.

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn child_holds_build_lock_after_parent_handle_is_dropped() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("lock");
        let lock = Lock::acquire(&path).unwrap();
        let mut child = Command::new("sleep").arg("0.1").spawn().unwrap();
        drop(lock);
        assert!(Lock::acquire(&path).is_err());
        child.wait().unwrap();
        assert!(Lock::acquire(&path).is_ok());
    }
}

#[cfg(test)]
mod affinity_tests {
    use super::*;
    #[test]
    fn compilation_child_leaves_two_available_cpus_outside_its_affinity() {
        let parent = std::thread::available_parallelism().unwrap().get();
        let d = tempfile::tempdir().unwrap();
        let file = fs::File::create(d.path().join("cpus")).unwrap();
        run_build(Command::new("nproc").stdout(Stdio::from(file))).unwrap();
        let child: usize = fs::read_to_string(d.path().join("cpus"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_eq!(child, parent.saturating_sub(2).max(1));
        assert_eq!(std::thread::available_parallelism().unwrap().get(), parent);
    }
}
