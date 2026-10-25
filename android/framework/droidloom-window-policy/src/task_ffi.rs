//! Narrow C ABI for the JNI adapter. Framework strings are copied before I/O.

use crate::tasks::{Backend, Binding, CommandBinding, CommandStream, Task, CONTROL_SOCKET};
use std::sync::{Mutex, OnceLock};

static BACKEND: OnceLock<Mutex<Backend>> = OnceLock::new();
static COMMANDS: OnceLock<Mutex<CommandStream>> = OnceLock::new();

#[unsafe(no_mangle)]
pub static DROIDLOOM_TASK_BACKEND_ABI: [u8; 29] = *b"DROIDLOOM_TASK_BACKEND_ABI=3;";

fn with_backend<T>(f: impl FnOnce(&mut Backend) -> T) -> T {
    let mut backend = BACKEND
        .get_or_init(|| Mutex::new(Backend::new(CONTROL_SOCKET)))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    f(&mut backend)
}

#[unsafe(no_mangle)]
pub extern "C" fn droidloom_task_version() -> i32 {
    3
}

fn with_commands<T>(f: impl FnOnce(&mut CommandStream) -> T) -> T {
    let mut commands = COMMANDS
        .get_or_init(|| Mutex::new(CommandStream::new(CONTROL_SOCKET)))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    f(&mut commands)
}

#[unsafe(no_mangle)]
/// # Safety
/// `out` must be writable and aligned for one `CommandBinding` for this call.
pub unsafe extern "C" fn droidloom_task_next_command(out: *mut CommandBinding) -> bool {
    if out.is_null() {
        return false;
    }
    with_commands(|stream| loop {
        let (serial, command) = match stream.next() {
            Ok(command) => command,
            Err(error) => {
                eprintln!("Droidloom task command stream: {error}");
                return false;
            }
        };
        if let Some(binding) = with_backend(|backend| backend.resolve_command(serial, command)) {
            // SAFETY: JNI supplies a live, writable, aligned stack CommandBinding.
            unsafe { out.write(binding) };
            return true;
        }
        // A stale object must not block newer commands; consume it without effects.
        if stream.acknowledge(serial).is_err() {
            return false;
        }
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn droidloom_task_acknowledge(serial: i64) -> bool {
    let Ok(serial) = u64::try_from(serial) else {
        return false;
    };
    serial != 0 && with_commands(|stream| stream.acknowledge(serial).is_ok())
}

#[unsafe(no_mangle)]
pub extern "C" fn droidloom_task_epoch() -> i64 {
    with_backend(|b| {
        b.epoch()
            .ok()
            .and_then(|e| i64::try_from(e).ok())
            .unwrap_or(0)
    })
}

#[unsafe(no_mangle)]
/// # Safety
/// `package` must point to `length` initialized bytes; `out` must be writable
/// and aligned for one Binding. Both allocations must survive this call.
pub unsafe extern "C" fn droidloom_task_register(
    id: i32,
    incarnation: i64,
    user: i32,
    display: i32,
    package: *const u8,
    length: usize,
    out: *mut Binding,
    activate: bool,
    keep_screen_on: bool,
) -> bool {
    if id <= 0
        || incarnation <= 0
        || user < 0
        || display < 0
        || package.is_null()
        || out.is_null()
        || length == 0
        || length > 255
    {
        return false;
    }
    // SAFETY: JNI adapter supplies a live GetStringUTFChars buffer and its exact
    // byte length, and a writable, aligned stack Binding. No pointer is retained.
    let bytes = unsafe { std::slice::from_raw_parts(package, length) };
    let Ok(package) = std::str::from_utf8(bytes) else {
        return false;
    };
    let (Ok(id), Ok(incarnation), Ok(user), Ok(display)) = (
        u64::try_from(id),
        u64::try_from(incarnation),
        u32::try_from(user),
        u32::try_from(display),
    ) else {
        return false;
    };
    let task = Task {
        id,
        incarnation,
        user,
        display,
        package: package.to_owned(),
        keep_screen_on,
    };
    with_backend(|b| {
        match if activate {
            b.activate(task)
        } else {
            b.register(task)
        } {
            Ok(binding) => {
                unsafe { out.write(binding) };
                true
            }
            Err(error) => {
                eprintln!("Droidloom task registration: {error}");
                false
            }
        }
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn droidloom_task_retire(id: i32, incarnation: i64) -> bool {
    if id <= 0 || incarnation <= 0 {
        return false;
    }
    let (Ok(id), Ok(incarnation)) = (u64::try_from(id), u64::try_from(incarnation)) else {
        return false;
    };
    with_backend(|b| match b.retire(id, incarnation) {
        Ok(()) => true,
        Err(error) => {
            eprintln!("Droidloom task retirement: {error}");
            false
        }
    })
}
