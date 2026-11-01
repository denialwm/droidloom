//! Wayland side: one xdg toplevel fed DMA-BUFs with explicit-sync timelines.
//!
//! This is the path Droidloom's composer uses for Android frames: a
//! `zwp_linux_dmabuf_v1` buffer per image and one `wp_linux_drm_syncobj`
//! timeline per buffer, acquire at odd points and release at even ones.

use std::fs::File;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};

use droidloom_syncobj::{SyncobjDevice, WaylandTimeline};
use wayland_client::protocol::{wl_buffer, wl_callback, wl_compositor, wl_registry, wl_surface};
use wayland_client::{delegate_noop, Connection, Dispatch, EventQueue, QueueHandle};
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1, zwp_linux_dmabuf_v1,
};
use wayland_protocols::wp::linux_drm_syncobj::v1::client::{
    wp_linux_drm_syncobj_manager_v1, wp_linux_drm_syncobj_surface_v1,
    wp_linux_drm_syncobj_timeline_v1,
};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

use crate::gpu::{Target, MODIFIER_LINEAR};

/// `DRM_FORMAT_XRGB8888`, the byte order of `VK_FORMAT_B8G8R8A8_UNORM`.
const XRGB8888: u32 = u32::from_le_bytes(*b"XR24");

#[derive(Default)]
struct State {
    compositor: Option<wl_compositor::WlCompositor>,
    wm_base: Option<xdg_wm_base::XdgWmBase>,
    dmabuf: Option<zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1>,
    syncobj: Option<wp_linux_drm_syncobj_manager_v1::WpLinuxDrmSyncobjManagerV1>,
    configured: bool,
    frame_done: bool,
    closed: bool,
}

struct Slot {
    buffer: wl_buffer::WlBuffer,
    timeline: WaylandTimeline,
    wire_timeline: wp_linux_drm_syncobj_timeline_v1::WpLinuxDrmSyncobjTimelineV1,
    uses: u64,
}

pub struct Window {
    queue: EventQueue<State>,
    handle: QueueHandle<State>,
    state: State,
    surface: wl_surface::WlSurface,
    sync_surface: wp_linux_drm_syncobj_surface_v1::WpLinuxDrmSyncobjSurfaceV1,
    _xdg: (xdg_surface::XdgSurface, xdg_toplevel::XdgToplevel),
    slots: Vec<Slot>,
    width: i32,
    height: i32,
}

impl Window {
    pub fn open(
        title: &str,
        width: u32,
        height: u32,
        render_node: &str,
        targets: &[Target],
    ) -> Result<Self, String> {
        let connection = Connection::connect_to_env().map_err(|error| error.to_string())?;
        let mut queue = connection.new_event_queue();
        let handle = queue.handle();
        connection.display().get_registry(&handle, ());
        let mut state = State::default();
        queue.roundtrip(&mut state).map_err(|error| error.to_string())?;

        let compositor = state.compositor.clone().ok_or("compositor lacks wl_compositor")?;
        let wm_base = state.wm_base.clone().ok_or("compositor lacks xdg_wm_base")?;
        let dmabuf = state.dmabuf.clone().ok_or("compositor lacks zwp_linux_dmabuf_v1")?;
        let syncobj = state
            .syncobj
            .clone()
            .ok_or("compositor lacks wp_linux_drm_syncobj_manager_v1")?;

        let surface = compositor.create_surface(&handle, ());
        let xdg = wm_base.get_xdg_surface(&surface, &handle, ());
        let toplevel = xdg.get_toplevel(&handle, ());
        toplevel.set_title(title.to_owned());
        toplevel.set_app_id("dev.droidloom.gpu-triangle".to_owned());
        surface.commit();
        while !state.configured {
            queue.blocking_dispatch(&mut state).map_err(|error| error.to_string())?;
        }
        let sync_surface = syncobj.get_surface(&surface, &handle, ());

        let node = File::options()
            .read(true)
            .write(true)
            .open(render_node)
            .map_err(|error| format!("open {render_node}: {error}"))?;
        let device = SyncobjDevice::from_file(node);
        let mut slots = Vec::with_capacity(targets.len());
        for target in targets {
            let buffer = target.dmabuf.as_ref().ok_or("local targets cannot be shown")?;
            let params = dmabuf.create_params(&handle, ());
            params.add(
                buffer.as_fd(),
                0,
                target.offset,
                target.stride,
                (MODIFIER_LINEAR >> 32) as u32,
                MODIFIER_LINEAR as u32,
            );
            let buffer = params.create_immed(
                width as i32,
                height as i32,
                XRGB8888,
                zwp_linux_buffer_params_v1::Flags::empty(),
                &handle,
                (),
            );
            params.destroy();
            let timeline = WaylandTimeline::create(&device).map_err(|error| error.to_string())?;
            let descriptor = timeline.export_descriptor().map_err(|error| error.to_string())?;
            let wire_timeline = syncobj.import_timeline(descriptor.as_fd(), &handle, ());
            slots.push(Slot { buffer, timeline, wire_timeline, uses: 0 });
        }

        Ok(Self {
            queue,
            handle,
            state,
            surface,
            sync_surface,
            _xdg: (xdg, toplevel),
            slots,
            width: width as i32,
            height: height as i32,
        })
    }

    pub fn closed(&self) -> bool {
        self.state.closed
    }

    /// The compositor's release fence for the last commit of `index`, once it
    /// has one. The GPU waits on it; the CPU only waits for it to exist.
    pub fn release_fence(&mut self, index: usize) -> Result<Option<OwnedFd>, String> {
        let slot = &mut self.slots[index];
        if slot.uses == 0 {
            return Ok(None);
        }
        let point = slot.uses * 2;
        if let Some(fence) = slot.timeline.try_export_release_fence(point).map_err(|e| e.to_string())? {
            return Ok(Some(fence));
        }
        // SAFETY: eventfd returns a new descriptor or -1.
        let raw = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC) };
        if raw < 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        // SAFETY: `raw` is a fresh descriptor owned here.
        let event = unsafe { OwnedFd::from_raw_fd(raw) };
        slot.timeline
            .notify_when_available(point, event.as_fd())
            .map_err(|error| error.to_string())?;
        let mut poll = libc::pollfd { fd: event.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        // SAFETY: one valid pollfd for the duration of the call.
        while unsafe { libc::poll(&raw mut poll, 1, -1) } < 0 {}
        slot.timeline
            .try_export_release_fence(point)
            .map_err(|error| error.to_string())?
            .map(Some)
            .ok_or_else(|| "release fence vanished after availability".to_owned())
    }

    /// Commit image `index`, whose rendering completes with `rendered`, and
    /// block until the compositor asks for the next frame.
    pub fn present(&mut self, index: usize, rendered: &OwnedFd) -> Result<(), String> {
        let slot = &mut self.slots[index];
        let acquire = slot.uses * 2 + 1;
        let release = acquire + 1;
        slot.timeline
            .import_acquire_fence(acquire, rendered.as_fd())
            .map_err(|error| error.to_string())?;
        self.sync_surface
            .set_acquire_point(&slot.wire_timeline, (acquire >> 32) as u32, acquire as u32);
        self.sync_surface
            .set_release_point(&slot.wire_timeline, (release >> 32) as u32, release as u32);
        slot.uses += 1;
        self.surface.attach(Some(&slot.buffer), 0, 0);
        self.surface.damage_buffer(0, 0, self.width, self.height);
        self.surface.frame(&self.handle, ());
        self.surface.commit();
        self.state.frame_done = false;
        while !self.state.frame_done && !self.state.closed {
            self.queue
                .blocking_dispatch(&mut self.state)
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        (): &(),
        _: &Connection,
        handle: &QueueHandle<Self>,
    ) {
        let wl_registry::Event::Global { name, interface, version } = event else { return };
        match interface.as_str() {
            "wl_compositor" => state.compositor = Some(registry.bind(name, version.min(6), handle, ())),
            "xdg_wm_base" => state.wm_base = Some(registry.bind(name, version.min(5), handle, ())),
            "zwp_linux_dmabuf_v1" if version >= 3 => {
                state.dmabuf = Some(registry.bind(name, version.min(4), handle, ()));
            }
            "wp_linux_drm_syncobj_manager_v1" => {
                state.syncobj = Some(registry.bind(name, 1, handle, ()));
            }
            _ => {}
        }
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for State {
    fn event(
        _: &mut Self,
        wm_base: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            wm_base.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for State {
    fn event(
        state: &mut Self,
        surface: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            surface.ack_configure(serial);
            state.configured = true;
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for State {
    fn event(
        state: &mut Self,
        _: &xdg_toplevel::XdgToplevel,
        event: xdg_toplevel::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_toplevel::Event::Close = event {
            state.closed = true;
        }
    }
}

impl Dispatch<wl_callback::WlCallback, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_callback::WlCallback,
        event: wl_callback::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = event {
            state.frame_done = true;
        }
    }
}

delegate_noop!(State: wl_compositor::WlCompositor);
delegate_noop!(State: ignore wl_surface::WlSurface);
delegate_noop!(State: ignore wl_buffer::WlBuffer);
delegate_noop!(State: ignore zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1);
delegate_noop!(State: ignore zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1);
delegate_noop!(State: wp_linux_drm_syncobj_manager_v1::WpLinuxDrmSyncobjManagerV1);
delegate_noop!(State: wp_linux_drm_syncobj_surface_v1::WpLinuxDrmSyncobjSurfaceV1);
delegate_noop!(State: wp_linux_drm_syncobj_timeline_v1::WpLinuxDrmSyncobjTimelineV1);
