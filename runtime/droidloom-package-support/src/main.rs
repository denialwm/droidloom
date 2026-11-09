//! Package setup and pacman lifecycle integration. No shell interpreter is used.
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use droidloom_supervisor::host_gpu::{GpuChoice, Graphics, HostGpu};
use std::{
    fs,
    io::Write,
    os::fd::AsRawFd,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Component, Path, PathBuf},
    process::Command,
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
const BASE: &str = "/etc/droidloom";
const SPEC: &str = "/etc/droidloom/cell.json";
const STATE: &str = "/etc/droidloom/package-state.json";
const ENVIRONMENT: &str = "/etc/droidloom/runtime.env";
const RULE: &str = "/etc/polkit-1/rules.d/49-droidloom.rules";
const HELPER: &str = "/usr/lib/droidloom/droidloom-package-helper";
const RECIPE: &str = "/usr/share/droidloom/cell-template.json";
const VERSION: &str = "/usr/share/droidloom/package.json";
const GAPPS: &str = "/usr/lib/droidloom/addons/gapps";

#[derive(Parser)]
#[command(
    version,
    about = "Configure and maintain a pacman-managed Droidloom installation"
)]
struct Cli {
    #[command(subcommand)]
    action: Action,
}
#[derive(Subcommand)]
enum Action {
    /// Perform missing setup as the calling desktop user, explaining authentication.
    Prepare,
    /// List the host's GPUs, or choose the one Android renders on.
    Gpu {
        /// auto, amd, intel, nvidia, qualcomm, mali or a PCI address.
        choice: Option<GpuChoice>,
    },
    #[command(hide = true)]
    Setup {
        #[arg(long)]
        uid: u32,
        #[arg(long)]
        gpu: Option<GpuChoice>,
        #[arg(long)]
        data_home: Option<PathBuf>,
        #[arg(long)]
        state_home: Option<PathBuf>,
    },
    #[command(hide = true)]
    BeforeUpgrade,
    #[command(hide = true)]
    BeforeRemove,
    #[command(hide = true)]
    AfterInstall,
}
#[derive(Clone, Deserialize, Serialize)]
struct SetupState {
    uid: u32,
    version: Value,
    data_home: PathBuf,
    state_home: PathBuf,
}
struct User {
    name: String,
    home: PathBuf,
}
fn read_json(path: &Path) -> Result<Value> {
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}
fn optional_json(path: &Path) -> Result<Option<Value>> {
    match fs::read(path) {
        Ok(data) => Ok(Some(serde_json::from_slice(&data)?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}
fn run(command: &mut Command) -> Result<()> {
    let status = command.status()?;
    if !status.success() {
        return Err(format!("{command:?} failed: {status}").into());
    }
    Ok(())
}
fn user(uid: u32) -> Result<User> {
    if uid == 0 {
        return Err("Droidloom's desktop owner must be an ordinary user".into());
    }
    let record = Command::new("/usr/bin/getent")
        .args(["passwd", &uid.to_string()])
        .output()?;
    if !record.status.success() {
        return Err(format!("no desktop user exists for UID {uid}").into());
    }
    let record = String::from_utf8(record.stdout)?;
    let fields: Vec<_> = record.trim_end().split(':').collect();
    if fields.len() != 7 || fields[2].parse::<u32>()? != uid || !Path::new(fields[5]).is_absolute()
    {
        return Err("invalid desktop user record".into());
    }
    Ok(User {
        name: fields[0].into(),
        home: fields[5].into(),
    })
}
fn require_root() -> Result<()> {
    if unsafe { libc::geteuid() } != 0 {
        return Err("this operation writes root-owned Droidloom configuration or controls its system service; run through pacman or the authenticated setup operation".into());
    }
    Ok(())
}
fn root_directory(path: &Path, mode: u32) -> Result<()> {
    if path == Path::new("/") {
        return Ok(());
    }
    if !path.is_absolute() || path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(
            "administrator-owned directory must be an absolute path without parent traversal"
                .into(),
        );
    }
    if let Some(parent) = path.parent() {
        root_directory(parent, 0o755)?;
    }
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() && m.uid() == 0 && m.mode() & 0o022 == 0 => Ok(()),
        Ok(_) => Err(format!(
            "refusing non-directory, symlink, or writable/non-root-owned setup path {}",
            path.display()
        )
        .into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(path)?;
            fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}
fn atomic_write(path: &Path, value: &[u8], mode: u32) -> Result<()> {
    let parent = path.parent().ok_or("configuration path has no parent")?;
    root_directory(parent, 0o755)?;
    if let Ok(metadata) = fs::symlink_metadata(path) {
        if !metadata.is_file() || metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
            return Err(format!(
                "refusing to replace unsafe configuration {}",
                path.display()
            )
            .into());
        }
    }
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(value)?;
    temporary
        .as_file()
        .set_permissions(fs::Permissions::from_mode(mode))?;
    temporary.as_file().sync_all()?;
    temporary.persist(path)?;
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}
/// The GPU choice in a cell specification; none means auto.
fn chosen_gpu(spec: Option<&Value>) -> Result<GpuChoice> {
    Ok(match spec.and_then(|s| s.get("gpu")) {
        Some(value) => serde_json::from_value(value.clone())?,
        None => GpuChoice::Auto,
    })
}
/// The rendering `choice` names among the GPUs `sys` lists.
fn graphics(sys: &Path, choice: &GpuChoice) -> Result<Graphics> {
    Graphics::resolve(choice, &HostGpu::list(sys)?).map_err(|error| {
        format!("Droidloom needs an AMD, Intel, NVIDIA, Qualcomm Adreno or Arm Mali GPU: {error}. Configuration was not changed.").into()
    })
}
/// What the GPU server reads: the forwarded GPU, if any.
fn environment(graphics: &Graphics) -> String {
    let mut text = format!(
        "DROIDLOOM_HOST_PEER_PID=0\nDROIDLOOM_HOST_PEER_UID=1000\nDROIDLOOM_HOST_PEER_GID=1000\nDROIDLOOM_RENDER_NODE={}\n",
        graphics.render_node
    );
    if let Some(pci) = graphics.forwarded() {
        text.push_str(&format!("DROIDLOOM_GPU={pci}\n"));
    }
    text
}
/// Google apps start with Android's storage and cannot join it later, so the
/// first setup selects an installed add-on while Android has never booted on
/// the storage.
fn selects_gapps(previous: Option<&Value>, addon: &Path, data: &Path) -> bool {
    previous.is_none()
        && addon.is_dir()
        && fs::read_dir(data.join("data")).map_or(true, |mut entries| entries.next().is_none())
}
fn configuration(
    mut recipe: Value,
    previous: Option<&Value>,
    uid: u32,
    choice: &GpuChoice,
    graphics: &Graphics,
) -> Result<Value> {
    if uid == 0 {
        return Err("desktop UID must be nonzero".into());
    }
    recipe["host_uid"] = json!(uid);
    if !choice.is_auto() {
        recipe["gpu"] = json!(choice);
    }
    recipe["render_node"] = json!(graphics.render_node);
    recipe["graphics_backend"] = json!(graphics.backend);
    recipe["data_dir"] = json!(format!("/var/lib/droidloom/users/{uid}/data"));
    recipe["runtime_dir"] = json!(format!("/run/droidloom/cells/u{uid}"));
    recipe["denial_socket"] = json!(format!("/run/user/{uid}/droidloom/native-bridge.sock"));
    if let Some(previous) = previous {
        if previous["host_uid"].as_u64() != Some(u64::from(uid)) {
            return Err("this Droidloom installation belongs to another desktop user; existing data and configuration were preserved".into());
        }
        for key in [
            "data_dir",
            "gapps_dir",
            "shared_storage_directories",
            "subordinate_uids",
            "subordinate_gids",
        ] {
            if let Some(value) = previous.get(key) {
                recipe[key] = value.clone();
            }
        }
    }
    let data = Path::new(
        recipe["data_dir"]
            .as_str()
            .ok_or("missing Android data path")?,
    );
    if !data.starts_with("/var/lib/droidloom")
        || data.components().any(|c| matches!(c, Component::ParentDir))
    {
        return Err("Android data must remain below /var/lib/droidloom".into());
    }
    let spec: droidloom_supervisor::CellSpec = serde_json::from_value(recipe.clone())?;
    spec.validate()?;
    Ok(recipe)
}
fn prepare() -> Result<()> {
    let uid = unsafe { libc::getuid() };
    let account = user(uid)?;
    if unsafe { libc::geteuid() } == 0 {
        return Err("run droidloomctl start as your desktop user, without sudo".into());
    }
    let previous = optional_json(Path::new(SPEC))?;
    if previous
        .as_ref()
        .is_some_and(|s| s["host_uid"].as_u64() != Some(u64::from(uid)))
    {
        return Err("Droidloom is configured for a different desktop user".into());
    }
    let version = read_json(Path::new(VERSION))?;
    let state: Option<SetupState> = optional_json(Path::new(STATE))?
        .map(serde_json::from_value)
        .transpose()?;
    let wanted = graphics(Path::new("/sys/class/drm"), &chosen_gpu(previous.as_ref())?)?;
    let configured = fs::read_to_string(ENVIRONMENT).ok();
    let current = configured.as_deref() == Some(environment(&wanted).as_str())
        && previous.as_ref().is_some_and(|s| {
            s["render_node"] == json!(wanted.render_node) && s["graphics_backend"] == json!(wanted.backend)
        });
    if state
        .as_ref()
        .is_some_and(|s| s.uid == uid && s.version == version)
        && current
    {
        // Polkit's rules directory is intentionally not traversable by an
        // ordinary user. The root-owned state is published after the rule.
        eprintln!("Droidloom graphics: {wanted}.");
        return Ok(());
    }
    if previous.is_some() && configured.is_some() && !current {
        eprintln!("The host's GPUs changed since Droidloom was set up. Now: {wanted}.");
    }
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| account.home.join(".local/share"));
    let state_home = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| account.home.join(".local/state"));
    let data = previous
        .as_ref()
        .and_then(|s| s["data_dir"].as_str())
        .map(str::to_owned)
        .unwrap_or_else(|| format!("/var/lib/droidloom/users/{uid}/data"));
    eprintln!(
        "Administrator authentication is needed to configure Droidloom for {} (UID {uid}).",
        account.name
    );
    eprintln!(
        "The installed helper will write {SPEC}, {ENVIRONMENT}, {STATE}, and {RULE}. These files are administrator-owned."
    );
    eprintln!(
        "It will create missing Android data and metadata directories in {data}, use host filesystem capacity directly, and preserve existing Android data. Legacy images require explicit offline migration. It authorizes only start/stop of droidloomd.service for this user."
    );
    eprintln!(
        "That system service needs root to create Android namespaces, mount the image files, and configure Droidloom's network connection."
    );
    run(Command::new("/usr/bin/pkexec")
        .arg(HELPER)
        .args(["setup", "--uid"])
        .arg(uid.to_string())
        .arg("--data-home")
        .arg(data_home)
        .arg("--state-home")
        .arg(state_home))
}
fn setup(
    uid: u32,
    gpu: Option<GpuChoice>,
    data_home: Option<PathBuf>,
    state_home: Option<PathBuf>,
    hook: bool,
) -> Result<()> {
    require_root()?;
    if !hook {
        if let Ok(invoker) = std::env::var("PKEXEC_UID").or_else(|_| std::env::var("SUDO_UID")) {
            if invoker.parse::<u32>()? != uid {
                return Err("setup owner differs from the authenticated caller".into());
            }
        }
    }
    let account = user(uid)?;
    root_directory(Path::new(BASE), 0o755)?;
    let lock = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(Path::new(BASE).join("setup.lock"))?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let previous = optional_json(Path::new(SPEC))?;
    let choice = match gpu {
        Some(choice) => choice,
        None => chosen_gpu(previous.as_ref())?,
    };
    let graphics = graphics(Path::new("/sys/class/drm"), &choice)?;
    let mut spec = configuration(
        read_json(Path::new(RECIPE))?,
        previous.as_ref(),
        uid,
        &choice,
        &graphics,
    )?;
    let data = PathBuf::from(spec["data_dir"].as_str().unwrap());
    if selects_gapps(previous.as_ref(), Path::new(GAPPS), &data) {
        spec["gapps_dir"] = json!(GAPPS);
    }
    let data = data.as_path();
    root_directory(data, 0o700)?;
    droidloom_supervisor::host_storage::prepare(data)?;
    let old_state: Option<SetupState> = optional_json(Path::new(STATE))?
        .map(serde_json::from_value)
        .transpose()?;
    let state = SetupState {
        uid,
        version: read_json(Path::new(VERSION))?,
        data_home: data_home
            .or_else(|| old_state.as_ref().map(|s| s.data_home.clone()))
            .unwrap_or_else(|| account.home.join(".local/share")),
        state_home: state_home
            .or_else(|| old_state.as_ref().map(|s| s.state_home.clone()))
            .unwrap_or_else(|| account.home.join(".local/state")),
    };
    if !state.data_home.is_absolute() || !state.state_home.is_absolute() {
        return Err("XDG paths must be absolute".into());
    }
    let username = serde_json::to_string(&account.name)?;
    let rule = format!(
        "// Generated for Droidloom's configured desktop user.\npolkit.addRule(function(action, subject) {{\n  if (action.id === \"org.freedesktop.systemd1.manage-units\" && subject.user === {username} && action.lookup(\"unit\") === \"droidloomd.service\" && (action.lookup(\"verb\") === \"start\" || action.lookup(\"verb\") === \"stop\")) return polkit.Result.YES;\n}});\n"
    );
    atomic_write(Path::new(SPEC), &serde_json::to_vec_pretty(&spec)?, 0o644)?;
    atomic_write(Path::new(ENVIRONMENT), environment(&graphics).as_bytes(), 0o644)?;
    atomic_write(Path::new(RULE), rule.as_bytes(), 0o644)?;
    atomic_write(Path::new(STATE), &serde_json::to_vec_pretty(&state)?, 0o644)?;
    println!(
        "Droidloom is configured for {}. Automatic startup remains disabled.",
        account.name
    );
    println!("Droidloom graphics (GPU choice {choice}): {graphics}.");
    Ok(())
}
/// List the GPUs, or set the choice through setup.
fn gpu(choice: Option<GpuChoice>) -> Result<()> {
    let sys = Path::new("/sys/class/drm");
    let spec = optional_json(Path::new(SPEC))?;
    let current = chosen_gpu(spec.as_ref())?;
    let Some(choice) = choice else {
        for gpu in HostGpu::list(sys)? {
            println!("{gpu}");
        }
        match graphics(sys, &current) {
            Ok(graphics) => println!("GPU choice {current}: {graphics}."),
            Err(error) => println!("GPU choice {current}: {error}"),
        }
        println!("Choose with: droidloomctl gpu auto|amd|intel|nvidia|qualcomm|mali|PCI-ADDRESS");
        return Ok(());
    };
    let graphics = graphics(sys, &choice)?;
    let uid = unsafe { libc::getuid() };
    if spec.as_ref().is_some_and(|s| s["host_uid"].as_u64() != Some(u64::from(uid))) {
        return Err("Droidloom is configured for a different desktop user".into());
    }
    eprintln!("GPU choice {choice}: {graphics}.");
    eprintln!("Administrator authentication is needed to write {SPEC} and {ENVIRONMENT}.");
    run(Command::new("/usr/bin/pkexec")
        .arg(HELPER)
        .args(["setup", "--uid"])
        .arg(uid.to_string())
        .arg("--gpu")
        .arg(choice.to_string()))?;
    println!("Restart Droidloom to use it: droidloomctl restart");
    Ok(())
}
fn user_command(uid: u32, executable: &str) -> Result<Command> {
    let account = user(uid)?;
    let mut command = Command::new("/usr/bin/runuser");
    command
        .args(["-u", &account.name, "--", "/usr/bin/env"])
        .arg(format!("XDG_RUNTIME_DIR=/run/user/{uid}"))
        .arg(format!(
            "DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/{uid}/bus"
        ))
        .arg(executable);
    Ok(command)
}
fn stop_runtime(spec: Option<&Value>) -> Result<()> {
    if !Path::new("/run/systemd/system").exists() {
        return Ok(());
    }
    if let Some(uid) = spec
        .and_then(|s| s["host_uid"].as_u64())
        .and_then(|v| u32::try_from(v).ok())
    {
        if Path::new(&format!("/run/user/{uid}/bus")).exists() {
            run(user_command(uid, "/usr/bin/systemctl")?.args([
                "--user",
                "stop",
                "droidloom.service",
                "droidloom-applications.service",
            ]))?;
        }
    }
    run(Command::new("/usr/bin/systemctl").args(["stop", "droidloomd.service"]))?;
    let result = Command::new("/usr/bin/systemctl")
        .args([
            "show",
            "--property=ActiveState",
            "--value",
            "droidloomd.service",
        ])
        .output()?;
    let state = String::from_utf8(result.stdout)?;
    if !result.status.success() || !matches!(state.trim(), "inactive" | "failed") {
        return Err("Droidloom has not stopped; refusing to replace mounted Android images".into());
    }
    Ok(())
}
fn before(remove: bool) -> Result<()> {
    require_root()?;
    let spec = optional_json(Path::new(SPEC))?;
    stop_runtime(spec.as_ref())?;
    if remove {
        if let Some(value) = optional_json(Path::new(STATE))? {
            let state: SetupState = serde_json::from_value(value)?;
            let account = user(state.uid)?;
            run(Command::new("/usr/bin/runuser")
                .args(["-u", &account.name, "--", "/usr/bin/env"])
                .arg(format!("XDG_DATA_HOME={}", state.data_home.display()))
                .arg(format!("XDG_STATE_HOME={}", state.state_home.display()))
                .args(["/usr/bin/droidloom-applications", "--remove-managed"]))?;
        }
        for path in [RULE, ENVIRONMENT, STATE] {
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        println!(
            "Droidloom stopped. Android application data and personal configuration were retained."
        );
    }
    Ok(())
}
fn after_install() -> Result<()> {
    require_root()?;
    if let Some(spec) = optional_json(Path::new(SPEC))? {
        let uid = u32::try_from(
            spec["host_uid"]
                .as_u64()
                .ok_or("invalid existing desktop owner")?,
        )?;
        setup(uid, None, None, None, true)?;
    }
    println!("Droidloom is installed and stopped. Run droidloomctl start as your desktop user.");
    println!(
        "First start may request administrator authentication to create /etc/droidloom configuration, the service authorization rule, and Android data files under /var/lib/droidloom. Builds and subsequent starts do not need sudo."
    );
    Ok(())
}
fn execute() -> Result<()> {
    match Cli::parse().action {
        Action::Prepare => prepare(),
        Action::Gpu { choice } => gpu(choice),
        Action::Setup {
            uid,
            gpu,
            data_home,
            state_home,
        } => setup(uid, gpu, data_home, state_home, false),
        Action::BeforeUpgrade => before(false),
        Action::BeforeRemove => before(true),
        Action::AfterInstall => after_install(),
    }
}
fn main() {
    if let Err(error) = execute() {
        eprintln!("Droidloom package setup failed: {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn recipe() -> Value {
        let mut recipe: Value = serde_json::from_str(include_str!(
            "../../../packaging/cell-spec-x86_64-u1000.json"
        ))
        .unwrap();
        recipe["image_dir"] = json!("/usr/lib/droidloom/images");
        recipe["vendor_image"] = json!("/usr/lib/droidloom/images/images/vendor.raw.img");
        recipe["shared_storage_directories"] = json!([]);
        recipe
    }
    fn drm(node: u32) -> Graphics {
        let gpu = HostGpu { render_node: format!("/dev/dri/renderD{node}"), device: "0000:0a:00.0".into(), driver: "amdgpu".into() };
        Graphics::resolve(&GpuChoice::Auto, &[gpu]).unwrap()
    }
    #[test]
    fn setup_uses_target_user_and_retains_personal_state_during_upgrade() {
        let mut previous = configuration(recipe(), None, 1234, &GpuChoice::Auto, &drm(129)).unwrap();
        assert_eq!(
            previous["denial_socket"],
            "/run/user/1234/droidloom/native-bridge.sock"
        );
        assert_eq!(previous["runtime_dir"], "/run/droidloom/cells/u1234");
        assert_eq!(previous["data_dir"], "/var/lib/droidloom/users/1234/data");
        previous["data_dir"] = json!("/var/lib/droidloom/preserved-data");
        previous["gapps_dir"] = json!("/usr/lib/droidloom/addons/gapps");
        previous["android_file_overrides"] = json!([]);
        let updated =
            configuration(recipe(), Some(&previous), 1234, &GpuChoice::Auto, &drm(128)).unwrap();
        assert_eq!(updated["data_dir"], previous["data_dir"]);
        assert_eq!(updated["gapps_dir"], previous["gapps_dir"]);
        assert!(
            !updated["android_file_overrides"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert!(configuration(recipe(), Some(&previous), 1000, &GpuChoice::Auto, &drm(128)).is_err());
    }
    #[test]
    fn first_setup_selects_installed_gapps_for_fresh_storage_only() {
        let root = tempfile::tempdir().unwrap();
        let (addon, data) = (root.path().join("gapps"), root.path().join("storage"));
        assert!(!selects_gapps(None, &addon, &data));
        fs::create_dir(&addon).unwrap();
        assert!(selects_gapps(None, &addon, &data));
        fs::create_dir_all(data.join("data")).unwrap();
        assert!(selects_gapps(None, &addon, &data));
        assert!(!selects_gapps(Some(&json!({})), &addon, &data));
        fs::create_dir(data.join("data/system")).unwrap();
        assert!(!selects_gapps(None, &addon, &data));
    }
    #[test]
    fn gpu_choice_reaches_the_spec_and_the_server() {
        let sys = tempfile::tempdir().unwrap();
        let node = |name: &str, pci: &str, driver: &str| {
            let device = sys.path().join("devices").join(pci);
            fs::create_dir_all(&device).unwrap();
            fs::create_dir_all(sys.path().join("drivers").join(driver)).unwrap();
            std::os::unix::fs::symlink(sys.path().join("drivers").join(driver), device.join("driver")).unwrap();
            fs::create_dir_all(sys.path().join(name)).unwrap();
            std::os::unix::fs::symlink(&device, sys.path().join(name).join("device")).unwrap();
        };
        assert!(graphics(sys.path(), &GpuChoice::Auto).is_err());
        node("renderD128", "0000:04:00.0", "nvidia");
        node("renderD129", "0000:0a:00.0", "amdgpu");
        let auto = graphics(sys.path(), &GpuChoice::Auto).unwrap();
        assert!(environment(&auto).ends_with("DROIDLOOM_RENDER_NODE=/dev/dri/renderD129\nDROIDLOOM_GPU=0000:04:00.0\n"));
        let spec = configuration(recipe(), None, 1000, &GpuChoice::Auto, &auto).unwrap();
        assert_eq!((&spec["graphics_backend"], spec.get("gpu")), (&json!("forwarding"), None));
        let amd: GpuChoice = "amd".parse().unwrap();
        let native = graphics(sys.path(), &amd).unwrap();
        assert!(!environment(&native).contains("DROIDLOOM_GPU"));
        let spec = configuration(recipe(), Some(&spec), 1000, &amd, &native).unwrap();
        assert_eq!((&spec["graphics_backend"], &spec["gpu"]), (&json!("drm"), &json!("amd")));
        assert_eq!(chosen_gpu(Some(&spec)).unwrap(), amd);
        assert_eq!(chosen_gpu(None).unwrap(), GpuChoice::Auto);
    }
    #[test]
    fn setup_rejects_root_and_data_path_escape() {
        assert!(configuration(recipe(), None, 0, &GpuChoice::Auto, &drm(128)).is_err());
        let mut previous = configuration(recipe(), None, 1000, &GpuChoice::Auto, &drm(128)).unwrap();
        previous["data_dir"] = json!("/var/lib/droidloom/../../../home/test");
        assert!(configuration(recipe(), Some(&previous), 1000, &GpuChoice::Auto, &drm(128)).is_err());
    }
}
