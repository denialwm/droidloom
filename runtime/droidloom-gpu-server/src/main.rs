//! Droidloom GPU server: runs Android's Vulkan on the host driver.
//!
//! The listener forks one server per connection, so each Android process
//! gets its own driver instance and a crash stays inside that process.

#![allow(non_snake_case)]

mod astc_luts;
mod compressed;
mod generated;
mod gpu;
mod manual;
mod probe;
mod rt;
mod shadow;

use std::ffi::{CStr, CString};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixListener;

use droidloom_gpu_protocol::generated::vk::PFN_vkGetInstanceProcAddr;
use droidloom_gpu_protocol::PROTOCOL;
use droidloom_gpu_protocol::layout::{Hello, Welcome, MAGIC, SHM_SIZE};
use droidloom_gpu_protocol::sys::{self, Drm, Shm};

struct Options {
    socket: String,
    /// PCI address.
    gpu: Option<String>,
    render_node: Option<String>,
}

fn options() -> Options {
    let mut options = Options {
        socket: std::env::var("XDG_RUNTIME_DIR").map_or_else(|_| "/tmp".into(), |dir| dir) + "/droidloom-gpu.sock",
        gpu: std::env::var("DROIDLOOM_GPU").ok().filter(|gpu| !gpu.is_empty()),
        render_node: None,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || args.next().unwrap_or_else(|| rt::fail(&format!("{arg} needs a value")));
        match arg.as_str() {
            "--socket" => options.socket = value(),
            "--gpu" => options.gpu = Some(value()),
            "--render-node" => options.render_node = Some(value()),
            _ => rt::fail(&format!("unknown argument {arg}; use --socket PATH, --gpu PCI-ADDRESS, --render-node PATH")),
        }
    }
    options
}

fn main() {
    let options = options();
    let chosen = options.gpu.is_some();
    let gpu = options.gpu.clone().or_else(gpu::first_nvidia).unwrap_or_else(|| rt::fail("no GPU chosen (DROIDLOOM_GPU or --gpu) and no NVIDIA GPU found"));
    let render_node = options
        .render_node
        .clone()
        .or_else(|| gpu::render_node(&gpu))
        .unwrap_or_else(|| rt::fail(&format!("the GPU at {gpu} has no render node; pass --render-node")));
    if gpu::driver(&gpu).as_deref() == Some("nvidia") && std::env::var_os("VK_LOADER_DRIVERS_SELECT").is_none() {
        // The loader would start every installed driver in every server.
        // SAFETY: single-threaded; nothing has read the environment yet.
        unsafe { std::env::set_var("VK_LOADER_DRIVERS_SELECT", "*nvidia*") };
    }
    let listener = listener(&options.socket);
    let (name, host_memory) = probe::host_gpu(&gpu);
    let source = if chosen { "chosen by Droidloom setup" } else { "the first NVIDIA GPU" };
    eprintln!(
        "droidloom-gpu-server: forwarding Android Vulkan to {name} at {gpu} ({source}) on {render_node}, shareable memory types {host_memory:x}, listening on {}",
        listener.local_addr().ok().and_then(|address| address.as_pathname().map(|path| path.display().to_string())).unwrap_or_else(|| "a socket from systemd".into())
    );
    // SAFETY: children are never waited for; the kernel reaps them.
    unsafe { libc::signal(libc::SIGCHLD, libc::SIG_IGN) };
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        // SAFETY: the listener is single-threaded and has loaded no driver.
        match unsafe { libc::fork() } {
            0 => {
                drop(listener);
                serve(OwnedFd::from(stream), &gpu, &render_node, vec![host_memory]);
            }
            -1 => eprintln!("droidloom-gpu-server: fork: {}", std::io::Error::last_os_error()),
            _ => {}
        }
    }
}

/// The socket systemd passes (`droidloom-gpu.socket`), whose inode the cell
/// keeps across server restarts, else a new one at `path`.
fn listener(path: &str) -> UnixListener {
    let activated = std::env::var("LISTEN_PID").is_ok_and(|pid| pid == std::process::id().to_string());
    if activated {
        if std::env::var("LISTEN_FDS").as_deref() != Ok("1") {
            rt::fail("systemd passed other than one socket");
        }
        // SAFETY: socket activation hands over descriptor 3, owned by no one else.
        return unsafe { UnixListener::from_raw_fd(3) };
    }
    let _ = std::fs::remove_file(path);
    UnixListener::bind(path).unwrap_or_else(|error| rt::fail(&format!("bind {path}: {error}")))
}

fn serve(socket: OwnedFd, gpu: &str, render_node: &str, host_memory: Vec<u32>) -> ! {
    let mut hello = [0u8; size_of::<Hello>()];
    let size = sys::recv(socket.as_fd(), &mut hello).map_or(0, |(size, _)| size);
    // SAFETY: plain data of the checked size.
    let hello: Hello = unsafe { hello.as_ptr().cast::<Hello>().read_unaligned() };
    let mut welcome = Welcome { magic: MAGIC, protocol: PROTOCOL, shm_size: SHM_SIZE as u64, render_node: [0; 64] };
    welcome.render_node[..render_node.len()].copy_from_slice(render_node.as_bytes());
    // SAFETY: `Welcome` is plain data.
    let bytes = unsafe { core::slice::from_raw_parts((&raw const welcome).cast::<u8>(), size_of::<Welcome>()) };
    if size != size_of::<Hello>() || hello.magic != MAGIC || hello.protocol != PROTOCOL {
        // The client reports the mismatch from our protocol value.
        let _ = sys::send(socket.as_fd(), bytes, None);
        rt::fail("client from another build; refused");
    }
    let (shm, memory) = Shm::create(SHM_SIZE).unwrap_or_else(|error| rt::fail(&format!("create shared memory: {error}")));
    sys::send(socket.as_fd(), bytes, Some(memory.as_raw_fd())).unwrap_or_else(|_| rt::fail("client went away"));
    drop(memory);
    let client = client(socket.as_fd());
    let client = format!("{} {client}", if hello.android != 0 { "Android" } else { "desktop" });
    eprintln!("droidloom-gpu-server: serving {client}");
    let node = CString::new(render_node).unwrap_or_else(|_| rt::fail("bad render node"));
    let drm = Drm::open(&node).unwrap_or_else(|error| rt::fail(&format!("open {render_node}: {error}")));
    let gipa = load_vulkan();
    watch(socket.as_raw_fd(), client);
    rt::Server::new(shm, socket, drm, gipa, gpu.into(), host_memory, hello.android != 0).run()
}

/// The host's Vulkan loader.
fn load_vulkan() -> PFN_vkGetInstanceProcAddr {
    // SAFETY: dlopen/dlsym with NUL-terminated names; the symbol has this type.
    unsafe {
        let library = libc::dlopen(c"libvulkan.so.1".as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
        if library.is_null() {
            rt::fail(&format!("load libvulkan.so.1: {}", CStr::from_ptr(libc::dlerror()).to_string_lossy()));
        }
        let symbol = libc::dlsym(library, c"vkGetInstanceProcAddr".as_ptr());
        if symbol.is_null() {
            rt::fail("libvulkan.so.1 has no vkGetInstanceProcAddr");
        }
        core::mem::transmute::<*mut libc::c_void, PFN_vkGetInstanceProcAddr>(symbol)
    }
}

/// The process at the other end of `socket`, for the log.
fn client(socket: std::os::fd::BorrowedFd<'_>) -> String {
    let mut credentials = libc::ucred { pid: 0, uid: 0, gid: 0 };
    let mut size = size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: SO_PEERCRED fills one ucred of the given size.
    let found = unsafe {
        libc::getsockopt(socket.as_raw_fd(), libc::SOL_SOCKET, libc::SO_PEERCRED, (&raw mut credentials).cast(), &raw mut size)
    } == 0;
    if !found {
        return "an unknown client".into();
    }
    let pid = credentials.pid;
    let command = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
    let command = String::from_utf8_lossy(command.split(|&b| b == 0).next().unwrap_or_default()).into_owned();
    format!("pid {pid} ({})", if command.is_empty() { "?" } else { &command })
}

/// Exit when the client hangs up; the ring loop never reads the socket
/// unless a record carries a descriptor.
fn watch(socket: libc::c_int, client: String) {
    std::thread::spawn(move || {
        let mut poll = libc::pollfd { fd: socket, events: libc::POLLRDHUP, revents: 0 };
        loop {
            // SAFETY: one pollfd on a live descriptor.
            let ready = unsafe { libc::poll(&raw mut poll, 1, -1) };
            if ready > 0 && poll.revents & (libc::POLLRDHUP | libc::POLLHUP | libc::POLLERR) != 0 {
                eprintln!("droidloom-gpu-server: {client} disconnected");
                // No atexit handlers: the driver's destructors would run while
                // the replay thread is still inside the driver.
                // SAFETY: ends the process; the kernel releases everything.
                unsafe { libc::_exit(0) };
            }
        }
    });
}
