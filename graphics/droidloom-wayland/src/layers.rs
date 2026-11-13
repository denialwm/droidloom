//! Direct Android layers on synchronized Wayland subsurfaces. No pixel copies
//! or rendering occur here. The SHM buffers represent constant colors.
use super::*;
use droidloom_denial_ipc::SeqPacket;
use droidloom_surface_bridge::layers::{self as wire, Buffer, Layer, Request};
use std::collections::BTreeSet;
use std::io::Write;
use wayland_client::protocol::{wl_shm, wl_shm_pool, wl_subcompositor, wl_subsurface};
use wayland_protocols::wp::alpha_modifier::v1::client::{
    wp_alpha_modifier_surface_v1::WpAlphaModifierSurfaceV1, wp_alpha_modifier_v1::WpAlphaModifierV1,
};

fn stage_batch(stream: &mut Stream, layers: Vec<wire::BatchLayer>, fds: Vec<OwnedFd>) {
    let mut descriptors: [Option<OwnedFd>; 4] = [const { None }; 4];
    for (slot, fd) in descriptors.iter_mut().zip(fds) {
        *slot = Some(fd);
    }
    for entry in layers {
        let fence = entry
            .fence_index
            .and_then(|index| descriptors[index].take());
        stream.staged.push(entry.layer, fence);
    }
}

pub(super) struct Globals {
    pub subcompositor: wl_subcompositor::WlSubcompositor,
    pub shm: wl_shm::WlShm,
    pub alpha: Option<WpAlphaModifierV1>,
}
struct Source {
    metadata: Buffer,
    buffer: Option<wl_buffer::WlBuffer>,
    lanes: Vec<Lane>,
    ticket: Option<u64>,
}

/// One timeline for reads of an allocation. Its points complete in order, so
/// a read starts on it only once its previous read ended. A compositor may
/// keep a detached read until its next frame; a new read of the unchanged
/// allocation then starts on another lane, and its scene brings that frame.
struct Lane {
    sync: BufferSync,
    availability_wakeup: crate::fence_wakeup::FenceWakeup,
}

/// Reads one allocation may have in flight before a new one waits.
const MAX_LANES: usize = 4;

impl Source {
    /// A lane with no read in flight, if any.
    fn free_lane(&self, id: u64, reads: &[Read]) -> Option<usize> {
        (0..self.lanes.len()).find(|&lane| !reads.iter().any(|r| r.buffer == id && r.lane == lane))
    }

    fn spare_timeline(&self, id: u64, reads: &[Read]) -> bool {
        self.lanes.len() < MAX_LANES || self.free_lane(id, reads).is_some()
    }
}

fn new_lane(
    syncobj: &SyncobjDevice,
    sync_manager: &WpLinuxDrmSyncobjManagerV1,
    qh: &QueueHandle<App>,
) -> Result<Lane, PresenterError> {
    let timeline = WaylandTimeline::create(syncobj)?;
    let fd = timeline.export_descriptor()?;
    let proxy = sync_manager.import_timeline(fd.as_fd(), qh, ());
    // SAFETY: eventfd returns a new owned descriptor without pointers.
    let event = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
    if event < 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(Lane {
        sync: BufferSync {
            timeline,
            proxy,
            next_point: 0,
            // SAFETY: ownership of the successful eventfd transfers once.
            release_wakeup: Some(unsafe { OwnedFd::from_raw_fd(event) }),
        },
        availability_wakeup: crate::fence_wakeup::FenceWakeup::default(),
    })
}
struct View {
    timeline: Option<frame_timeline::Surface>,
    surface: wl_surface::WlSurface,
    role: wl_subsurface::WlSubsurface,
    viewport: WpViewport,
    alpha: WpAlphaModifierSurfaceV1,
    sync: Option<WpLinuxDrmSyncobjSurfaceV1>,
    content: Option<Content>,
    solid: Option<(u32, wl_buffer::WlBuffer)>,
    geometry: Option<Geometry>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Content {
    Buffer { id: u64, ticket: u64 },
    Solid(u32),
}

impl Content {
    fn new(layer: &Layer) -> Self {
        if layer.buffer == 0 {
            Self::Solid(layer.color)
        } else {
            Self::Buffer {
                id: layer.buffer,
                ticket: layer.ticket,
            }
        }
    }
}

impl View {
    fn source(&self) -> Option<(u64, u64)> {
        match self.content {
            Some(Content::Buffer { id, ticket }) => Some((id, ticket)),
            _ => None,
        }
    }

    fn update(
        &mut self,
        geometry: Geometry,
        content: Content,
        apply: impl FnOnce(&mut Self, bool) -> Result<(), PresenterError>,
    ) -> Result<(), PresenterError> {
        let geometry_changed = self.geometry != Some(geometry);
        if !geometry_changed && self.content == Some(content) {
            // Keep the synchronized child's current state and GPU read. The
            // parent still commits the complete scene, including changed peers.
            return Ok(());
        }
        apply(self, geometry_changed)?;
        self.geometry = Some(geometry);
        self.content = Some(content);
        self.surface.damage_buffer(0, 0, i32::MAX, i32::MAX);
        self.surface.commit();
        Ok(())
    }

    /// Show `clear`, a transparent pixel, instead of unmapping: a compositor
    /// may show a newly mapped surface's first buffer a frame late, and an
    /// empty full-window layer reads as a black flash.
    fn hide(&mut self, clear: &wl_buffer::WlBuffer) {
        if self.content.take().is_some() {
            if let Some(sync) = self.sync.take() {
                sync.destroy();
            }
            // The pixel neither holds the old crop nor covers what lies
            // below, and it shrinks into the window's corner: an old size
            // or position would stretch the surface tree's bounds past a
            // resized window. Showing again resends all of them.
            self.viewport.set_source(-1.0, -1.0, -1.0, -1.0);
            self.viewport.set_destination(-1, -1);
            self.role.set_position(0, 0);
            self.surface.set_opaque_region(None);
            self.geometry = None;
            self.surface.attach(Some(clear), 0, 0);
            self.surface.damage_buffer(0, 0, i32::MAX, i32::MAX);
            self.surface.commit();
        }
    }
}

fn recycle_views(
    views: &mut BTreeMap<u64, View>,
    spare: &mut Vec<View>,
    ids: &BTreeSet<u64>,
    clear: &wl_buffer::WlBuffer,
) {
    let removed: Vec<_> = views
        .keys()
        .filter(|id| !ids.contains(id))
        .copied()
        .collect();
    for id in removed {
        let mut view = views.remove(&id).expect("removed view exists");
        view.hide(clear);
        // Subsurface destruction unmaps immediately, even while the parent's
        // replacement transaction is blocked on an acquire fence. Retain the
        // role and synchronize its hiding/reassignment with that transaction.
        // Reassignment also needs fresh stacking relative to the new peers.
        spare.push(view);
    }
}

fn view_for_layer<'a>(
    views: &'a mut BTreeMap<u64, View>,
    spare: &mut Vec<View>,
    id: u64,
    create: impl FnOnce() -> Result<View, PresenterError>,
) -> Result<&'a mut View, PresenterError> {
    use std::collections::btree_map::Entry;
    match views.entry(id) {
        Entry::Occupied(entry) => Ok(entry.into_mut()),
        Entry::Vacant(entry) => Ok(entry.insert(match spare.pop() {
            Some(view) => view,
            None => create()?,
        })),
    }
}

/// Persistent Wayland state, independent of the producer's rotating buffers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Geometry {
    above: Option<u64>,
    destination: [i32; 4],
    source: [u32; 4],
    transform: u32,
    alpha: u32,
    opaque: bool,
}

impl Geometry {
    fn new(l: &Layer, above: Option<u64>, logical: (u32, u32), physical: (u32, u32)) -> Self {
        let [x, y, w, h] = l.destination;
        let edge = |v: i32, logical: u32, physical: u32| -> i32 {
            ((i64::from(v) * i64::from(logical) + i64::from(physical) / 2) / i64::from(physical))
                as i32
        };
        let dx = edge(x, logical.0, physical.0);
        let dy = edge(y, logical.1, physical.1);
        Self {
            above,
            destination: [
                dx,
                dy,
                (edge(x + w, logical.0, physical.0) - dx).max(1),
                (edge(y + h, logical.1, physical.1) - dy).max(1),
            ],
            source: if l.buffer == 0 {
                [0, 0, 256, 256]
            } else {
                l.source
            },
            transform: l.transform,
            alpha: l.alpha,
            opaque: l.opaque && l.alpha == u32::MAX,
        }
    }
}
impl Drop for View {
    fn drop(&mut self) {
        self.timeline = None;
        if let Some(sync) = self.sync.take() {
            sync.destroy();
        }
        self.alpha.destroy();
        self.viewport.destroy();
        self.role.destroy();
        self.surface.destroy();
        if let Some((_, buffer)) = self.solid.take() {
            buffer.destroy();
        }
    }
}
struct Read {
    buffer: u64,
    lane: usize,
    ticket: u64,
    point: u64,
    fence_sent: bool,
}
pub(super) struct Stream {
    requests: SeqPacket,
    events: SeqPacket,
    buffers: BTreeMap<u64, Source>,
    staged: wire::Staging<OwnedFd>,
    waiting: Option<PendingCommit>,
    parked: Option<Parked>,
    views: BTreeMap<u64, View>,
    spare_views: Vec<View>,
    reads: Vec<Read>,
    imports: imports::Imports<zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1>,
    /// The carrier's size and whether it is clear.
    root: Option<((u32, u32), bool, wl_buffer::WlBuffer)>,
    /// Transparent pixel hidden views show.
    clear: Option<wl_buffer::WlBuffer>,
    root_logical_size: Option<(u32, u32)>,
    active: bool,
    connected: bool,
    last_ticket: u64,
    last_scene: u64,
    last_import: u64,
    configuration: wire::ConfigPublisher,
}

#[derive(Clone, Copy)]
struct PendingCommit {
    scene: Option<u64>,
    serial: u32,
    width: u32,
    height: u32,
    count: u32,
}

/// Why the stream's scene waits, kept from the first park until the wait ends.
struct Parked {
    object: TaskObjectId,
    scene: Option<u64>,
    since: Instant,
    reason: &'static str,
    reported: bool,
}

/// A parked scene freezes its window. A wait this long is reported while it
/// lasts, with what holds it, and again when it ends.
const PARKED_SCENE_REPORT: Duration = Duration::from_millis(500);

impl Stream {
    pub(super) fn abort_timeline_wait(&mut self) -> Result<(), PresenterError> {
        if let Some(commit) = self.waiting.take() {
            self.staged.clear();
            if let Some(scene) = commit.scene {
                self.events
                    .send_record(&wire::commit_result(scene, wire::SCENE_RETRY), &[])?;
            } else {
                self.ack(wire::SCENE_RETRY)?;
            }
        }
        self.end_wait("aborted");
        Ok(())
    }

    fn park(&mut self, object: TaskObjectId, commit: PendingCommit, reason: &'static str) {
        let since = self.parked.as_ref().map_or_else(Instant::now, |p| p.since);
        let reported = self.parked.as_ref().is_some_and(|p| p.reported);
        self.parked = Some(Parked {
            object,
            scene: commit.scene,
            since,
            reason,
            reported,
        });
        self.waiting = Some(commit);
    }

    fn stall_deadline(&self) -> Option<Instant> {
        self.parked
            .as_ref()
            .filter(|p| !p.reported)
            .map(|p| p.since + PARKED_SCENE_REPORT)
    }

    /// Each staged read whose allocation still has an earlier read
    /// outstanding: where that read stands, and which layer still shows it.
    fn busy_detail(&self) -> String {
        use std::fmt::Write as _;
        let mut detail = String::new();
        for (l, _) in self.staged.layers() {
            let Some(b) = self.buffers.get(&l.buffer) else {
                continue;
            };
            let Some(held) = b.ticket.filter(|held| *held != l.ticket) else {
                continue;
            };
            let read = self
                .reads
                .iter()
                .find(|r| r.buffer == l.buffer && r.ticket == held)
                .map(|r| (r.lane, r.point, r.fence_sent));
            let signalled =
                read.and_then(|(lane, ..)| b.lanes[lane].sync.timeline.signalled_point().ok());
            let lanes = b.lanes.len();
            let shown_by = self
                .views
                .iter()
                .find(|(_, v)| v.source() == Some((l.buffer, held)))
                .map(|(id, _)| *id);
            let in_scene =
                shown_by.is_some_and(|id| self.staged.layers().iter().any(|(x, _)| x.id == id));
            let _ = write!(
                detail,
                "; layer {} wants buffer {} as ticket {} but ticket {held} is unreleased \
                 (read lane, point and fence sent {read:?}, signalled point {signalled:?}, \
                 lanes {lanes}, shown by layer {shown_by:?}, that layer in this scene: \
                 {in_scene})",
                l.id, l.buffer, l.ticket,
            );
        }
        detail
    }

    fn end_wait(&mut self, outcome: &str) {
        if self.waiting.is_some() {
            return;
        }
        let Some(parked) = self.parked.take() else {
            return;
        };
        let waited = parked.since.elapsed();
        if parked.reported || waited >= PARKED_SCENE_REPORT {
            eprintln!(
                "Droidloom task {}: scene {} {outcome} after waiting {} ms ({})",
                parked.object.0,
                parked.scene.unwrap_or(0),
                waited.as_millis(),
                parked.reason,
            );
        }
    }
    pub fn busy(&self) -> bool {
        !self.reads.is_empty()
    }
    /// The window's surfaces show Android's buffers.
    pub fn shows_android(&self) -> bool {
        self.active
    }
    pub fn fds(&self) -> impl Iterator<Item = (i32, u8)> + '_ {
        (self.connected && !self.imports.blocked())
            .then_some((self.requests.as_fd().as_raw_fd(), readiness::REQUESTS))
            .into_iter()
            .chain(self.reads.iter().flat_map(|read| {
                let lane = self.buffers.get(&read.buffer).map(|b| &b.lanes[read.lane]);
                let available = lane
                    .and_then(|lane| lane.availability_wakeup.fd())
                    .filter(|_| self.connected && !read.fence_sent);
                let released = lane
                    .and_then(|lane| lane.sync.release_wakeup.as_ref())
                    .map(AsFd::as_fd);
                [available, released]
                    .into_iter()
                    .flatten()
                    .map(|event| (event.as_raw_fd(), readiness::RELEASES))
            }))
    }
    pub fn poll_fallback(&self) -> bool {
        self.reads.iter().any(|r| {
            let lane = self.buffers.get(&r.buffer).map(|b| &b.lanes[r.lane]);
            (self.connected
                && !r.fence_sent
                && lane.is_some_and(|lane| lane.availability_wakeup.fd().is_none()))
                || lane.is_some_and(|lane| lane.sync.release_wakeup.is_none())
        })
    }
    fn ack(&self, code: i32) -> Result<(), PresenterError> {
        self.requests
            .send_record(&wire::response(0x8001, &[code as u32]), &[])?;
        Ok(())
    }
    /// Detach children in the parent's pending transaction. Their original
    /// read records survive until the compositor releases each buffer.
    pub fn hide(&mut self) -> bool {
        if !self.active {
            return false;
        }
        let clear = self
            .clear
            .as_ref()
            .expect("an active stream has its clear pixel");
        for view in self.views.values_mut() {
            view.hide(clear);
        }
        self.active = false;
        true
    }
    fn release(&mut self) -> Result<(), PresenterError> {
        // Poll drains only signalled eventfds. Readiness selects the task;
        // the real timeline below still decides ownership and safe reuse.
        let mut index = 0;
        while index < self.reads.len() {
            let read = &self.reads[index];
            let (buffer, lane, ticket, point, fence_sent) = (
                read.buffer,
                read.lane,
                read.ticket,
                read.point,
                read.fence_sent,
            );
            let source = self.buffers.get_mut(&buffer).expect("read owns source");
            let timeline = &mut source.lanes[lane].sync.timeline;
            if self.connected && !fence_sent {
                if let Some(fence) = timeline.try_export_release_fence(point)? {
                    // A point exports once. Relabelled reads share it, so this
                    // fence serves every ticket still waiting for one there.
                    for read in self.reads.iter_mut().filter(|r| {
                        r.buffer == buffer && r.lane == lane && r.point == point && !r.fence_sent
                    }) {
                        self.events
                            .send_record(&wire::release_fence(read.ticket), &[fence.as_fd()])?;
                        read.fence_sent = true;
                    }
                }
            }
            if timeline.signalled_point()? < point {
                index += 1;
                continue;
            }
            // Completion comes from the actual Wayland buffer read. Neither
            // presentation timing nor socket acknowledgements release sources.
            let packet = wire::response(0x8003, &[ticket as u32, (ticket >> 32) as u32]);
            if self.connected {
                self.events.send_record(&packet, &[])?;
            } else {
                // Retiring a task closes only SF's request socket; its event
                // reader stays open for the reads it still tracks. Each app's
                // releases are ordered, so a read left unresolved here would
                // hold every later release of that app. A reader that is gone
                // too has nothing left to release.
                let _ = self.events.send_record(&packet, &[]);
            }
            // A relabelled read shares this point and completes in this pass.
            if source.ticket == Some(ticket) {
                source.ticket = None;
            }
            self.reads.swap_remove(index);
        }
        Ok(())
    }
}
impl Drop for Stream {
    fn drop(&mut self) {
        self.views.clear();
        self.spare_views.clear();
        for source in self.buffers.values_mut() {
            if let Some(b) = source.buffer.take() {
                b.destroy();
            }
        }
        if let Some((_, _, b)) = self.root.take() {
            b.destroy();
        }
        if let Some(b) = self.clear.take() {
            b.destroy();
        }
        for pending in self.imports.drain() {
            pending.params.destroy();
        }
    }
}

fn pixel(
    globals: &Globals,
    qh: &QueueHandle<App>,
    color: u32,
) -> Result<wl_buffer::WlBuffer, PresenterError> {
    let mut file = tempfile::tempfile()?;
    file.write_all(&color.to_ne_bytes())?;
    let pool = globals.shm.create_pool(file.as_fd(), 4, qh, ());
    let buffer = pool.create_buffer(0, 1, 1, 4, wl_shm::Format::Argb8888, qh, ());
    pool.destroy();
    Ok(buffer)
}

fn carrier_size(width: u32, height: u32) -> Result<(u32, u32), PresenterError> {
    if width == 0 || height == 0 {
        return Err(PresenterError::Configuration("empty layer task size"));
    }
    // Mobile shells can use the root buffer's aspect when fitting the whole
    // surface tree. Preserve that aspect for the constant backing.
    // Reduce the dimensions to avoid allocating a full task-sized blank image.
    let (mut a, mut b) = (width, height);
    while b != 0 {
        (a, b) = (b, a % b);
    }
    Ok((width / a, height / a))
}

fn covered_by_opaque_layer<'a>(
    mut layers: impl Iterator<Item = &'a Layer>,
    width: u32,
    height: u32,
) -> bool {
    let (Ok(width), Ok(height)) = (i32::try_from(width), i32::try_from(height)) else {
        return false;
    };
    width > 0
        && height > 0
        && layers.any(|layer| {
            layer.opaque
                && layer.alpha == u32::MAX
                && (layer.buffer != 0 || layer.color >> 24 == 0xff)
                && layer.destination == [0, 0, width, height]
        })
}

/// Dark gray #0d1117 beneath the layers. SurfaceFlinger composes tasks over
/// the same colour, so a gap reads as the window's background.
const BACKING: u32 = 0x000d_1117;

/// The root buffer beneath the layers: the backing, or clear for a task that
/// shows what it opened over.
fn carrier(
    globals: &Globals,
    qh: &QueueHandle<App>,
    (width, height): (u32, u32),
    clear: bool,
) -> Result<wl_buffer::WlBuffer, PresenterError> {
    let stride = width.checked_mul(4);
    let bytes = stride
        .and_then(|stride| stride.checked_mul(height))
        .and_then(|bytes| i32::try_from(bytes).ok())
        .ok_or(PresenterError::Configuration("layer root buffer too large"))?;
    let mut file = tempfile::tempfile()?;
    let (color, format) = if clear {
        (0, wl_shm::Format::Argb8888)
    } else {
        (BACKING, wl_shm::Format::Xrgb8888)
    };
    file.write_all(&color.to_ne_bytes().repeat(bytes as usize / 4))?;
    let pool = globals.shm.create_pool(file.as_fd(), bytes, qh, ());
    let buffer = pool.create_buffer(
        0,
        width as i32,
        height as i32,
        stride.unwrap() as i32,
        format,
        qh,
        (),
    );
    pool.destroy();
    Ok(buffer)
}

impl App {
    fn layer_config(&self, object: TaskObjectId) -> Option<[u32; 6]> {
        let task = self.tasks.get(&object)?;
        let enabled =
            self.layer_globals.alpha.is_some() && task.accepts_present() && !task.suspended;
        // The grid submit_layer_scene checks edges against.
        let (logical_width, logical_height) = task.logical_size.unwrap_or_default();
        Some([
            if enabled {
                wire::SUPPORTED
            } else {
                wire::CONFIG_EVENTS
            },
            task.configure_serial,
            task.buffer_size.0,
            task.buffer_size.1,
            logical_width,
            logical_height,
        ])
    }
    pub(super) fn bind_layers(
        &mut self,
        object: TaskObjectId,
        sockets: Vec<OwnedFd>,
    ) -> Result<(), PresenterError> {
        let task = self
            .tasks
            .get_mut(&object)
            .ok_or(PresenterError::UnknownTask(object))?;
        if task.layers.is_some() || sockets.len() != 2 || task.headless() {
            return Err(PresenterError::Configuration(
                "invalid layer stream binding",
            ));
        }
        let mut sockets = sockets.into_iter();
        let requests = SeqPacket::from_owned_fd(sockets.next().unwrap())?;
        let events = SeqPacket::from_owned_fd(sockets.next().unwrap())?;
        requests.set_nonblocking(true)?;
        // Releases are bounded by MAX_BUFFERS, far below the socket send queue.
        events.set_nonblocking(true)?;
        task.layers = Some(Stream {
            requests,
            events,
            buffers: BTreeMap::new(),
            staged: wire::Staging::default(),
            waiting: None,
            parked: None,
            views: BTreeMap::new(),
            spare_views: Vec::new(),
            clear: None,
            reads: Vec::new(),
            imports: imports::Imports::default(),
            root: None,
            root_logical_size: None,
            active: false,
            connected: true,
            last_ticket: 0,
            last_scene: 0,
            last_import: 0,
            configuration: wire::ConfigPublisher::default(),
        });
        self.task_work.streams.insert(object);
        self.task_work
            .mark(object, readiness::REQUESTS | readiness::CONFIG);
        Ok(())
    }
    pub(super) fn stalled_scene_deadline(&self) -> Option<Instant> {
        self.tasks
            .values()
            .filter_map(|task| task.layers.as_ref()?.stall_deadline())
            .min()
    }
    /// Report each scene parked for PARKED_SCENE_REPORT once, while its
    /// window is still frozen, with the state that decides how it can resume.
    pub(super) fn report_stalled_scenes(&mut self, now: Instant) {
        for task in self.tasks.values_mut() {
            let Some(stream) = task.layers.as_mut() else {
                continue;
            };
            if stream.stall_deadline().is_none_or(|due| now < due) {
                continue;
            }
            let parked = stream.parked.as_ref().expect("a stall deadline is parked");
            let line = format!(
                "Droidloom task {}: scene {} still waiting after {} ms ({}){}; direct={}, \
                 queued composed frame={}, scene feedback={}, presentation feedback={}, \
                 configure={}, timeline: {}",
                parked.object.0,
                parked.scene.unwrap_or(0),
                now.saturating_duration_since(parked.since).as_millis(),
                parked.reason,
                stream.busy_detail(),
                stream.active,
                task.timeline.queued.is_some(),
                task.scene_feedback.len(),
                task.presentation_feedback.len(),
                task.configure_serial,
                task.timeline.describe(),
            );
            stream.parked.as_mut().unwrap().reported = true;
            eprintln!("{line}");
        }
    }
    pub(super) fn drain_layers(
        &mut self,
        qh: &QueueHandle<Self>,
        objects: &mut Vec<TaskObjectId>,
    ) -> Result<(), PresenterError> {
        let mut receive_buffer = [0; droidloom_surface_bridge::layers::MAX_LAYER_PACKET];
        while let Some(object) = self.task_work.pop(readiness::RELEASES) {
            if let Some(mut stream) = self.tasks.get_mut(&object).and_then(|t| t.layers.take()) {
                stream.release()?;
                let result = if let Some(commit) = stream.waiting.take() {
                    self.submit_layer_scene(qh, object, &mut stream, commit)
                } else {
                    Ok(())
                };
                stream.end_wait("retried");
                self.task_work
                    .layer_fallback(object, stream.poll_fallback());
                self.tasks.get_mut(&object).unwrap().layers = Some(stream);
                result?;
                if self.tasks[&object].closing {
                    self.task_work.mark(object, readiness::CLEANUP);
                }
            }
        }
        self.task_work.take(readiness::REQUESTS, objects);
        for object in objects.iter().copied() {
            for _ in 0..128 {
                let Some(stream) = self.tasks.get_mut(&object).and_then(|t| t.layers.as_mut())
                else {
                    break;
                };
                if !stream.connected || stream.imports.blocked() {
                    break;
                }
                let record = match stream.requests.receive_record_into(&mut receive_buffer) {
                    Ok(r) => r,
                    Err(IpcError::Io(e)) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(IpcError::PeerClosed) => {
                        stream.connected = false;
                        stream.waiting = None;
                        stream.end_wait("dropped at disconnect");
                        stream.staged.clear();
                        stream.hide();
                        if let Some(surface) = self.tasks[&object].surface() {
                            surface.commit();
                        }
                        break;
                    }
                    Err(e) => return Err(e.into()),
                };
                let request = wire::decode(record.bytes, record.descriptors.len())
                    .map_err(|_| PresenterError::Configuration("invalid layer stream record"))?;
                if matches!(
                    request,
                    Request::Commit { .. }
                        | Request::CommitAsync { .. }
                        | Request::CommitBatch { .. }
                        | Request::Config
                        | Request::SubscribeConfig { .. }
                ) {
                    // A legacy target is sent on Composer's socket before SF
                    // receives its ack and can send this request. Drain that
                    // socket AFTER receiving the barrier, while task.layers
                    // is still attached, so fallback/direct mode cannot reorder
                    // across the two file descriptors.
                    self.drain_endpoint(qh)?;
                }
                let Some(mut stream) = self.tasks.get_mut(&object).and_then(|t| t.layers.take())
                else {
                    break;
                };
                let result =
                    self.layer_request(qh, object, &mut stream, request, record.descriptors);
                self.tasks.get_mut(&object).unwrap().layers = Some(stream);
                result?;
            }
            if let Some(stream) = self.tasks.get(&object).and_then(|t| t.layers.as_ref()) {
                self.task_work
                    .layer_fallback(object, stream.poll_fallback());
            }
        }
        // A barrier above may drain endpoint changes for any task. Publish
        // after all barriers so no configuration waits for a future wakeup.
        while let Some(object) = self.task_work.pop(readiness::CONFIG) {
            let Some(config) = self.layer_config(object) else {
                continue;
            };
            if let Some(stream) = self.tasks.get_mut(&object).and_then(|t| t.layers.as_mut())
                && stream.connected
            {
                if stream.waiting.is_some() {
                    // Revalidate stale geometry or a closing window without
                    // waiting for an unrelated GPU release.
                    self.task_work.mark(object, readiness::RELEASES);
                }
                let update = stream.configuration.update(config).map_err(|_| {
                    PresenterError::Configuration("layer configuration revision exhausted")
                })?;
                if let Some(words) = update {
                    stream
                        .events
                        .send_record(&wire::response(0x8005, &words), &[])?;
                }
            }
        }
        Ok(())
    }
    fn layer_request(
        &mut self,
        qh: &QueueHandle<Self>,
        object: TaskObjectId,
        stream: &mut Stream,
        request: Request,
        fds: Vec<OwnedFd>,
    ) -> Result<(), PresenterError> {
        // A waiting commit owns the bounded staging area. Continue observing
        // disconnects and servicing configuration/cancellation on this socket;
        // a peer cannot overwrite its pending scene or queue a second one.
        if stream.waiting.is_some()
            && !matches!(
                request,
                Request::Config | Request::SubscribeConfig { .. } | Request::Abort
            )
        {
            return Err(PresenterError::Configuration("layer scene already waiting"));
        }
        let async_scene = if let Request::CommitAsync { scene, .. }
        | Request::CommitBatch { scene, .. } = &request
        {
            if *scene <= stream.last_scene {
                return Err(PresenterError::Configuration("layer scene identity reused"));
            }
            stream.last_scene = *scene;
            Some(*scene)
        } else {
            None
        };
        let import_attempt = if let Request::RegisterAsync { attempt, .. } = &request {
            if *attempt <= stream.last_import {
                return Err(PresenterError::Configuration(
                    "layer import identity reused",
                ));
            }
            stream.last_import = *attempt;
            Some(*attempt)
        } else {
            None
        };
        // Chunks stay invisible in the same bounded staging area as legacy
        // records. Move each FD once; decoding proved unique, exhaustive indices.
        let (request, fds) = match request {
            Request::StageBatch(layers) => {
                stage_batch(stream, layers, fds);
                return Ok(());
            }
            Request::CommitBatch {
                scene,
                serial,
                width,
                height,
                count,
                layers,
            } => {
                stage_batch(stream, layers, fds);
                (
                    Request::CommitAsync {
                        scene,
                        serial,
                        width,
                        height,
                        count,
                    },
                    Vec::new(),
                )
            }
            request => (request, fds),
        };
        match request {
            Request::StageBatch(_) | Request::CommitBatch { .. } => {
                unreachable!("normalized batch")
            }
            Request::SubscribeConfig { placement } => {
                let words = stream
                    .configuration
                    .subscribe(
                        self.layer_config(object)
                            .ok_or(PresenterError::UnknownTask(object))?,
                        placement,
                    )
                    .map_err(|_| {
                        PresenterError::Configuration("layer configuration revision exhausted")
                    })?;
                stream
                    .requests
                    .send_record(&wire::response(0x8005, &words), &[])?;
            }
            Request::Config => {
                let config = self
                    .layer_config(object)
                    .ok_or(PresenterError::UnknownTask(object))?;
                stream
                    .requests
                    .send_record(&wire::response(0x8002, &config[..4]), &[])?;
            }
            Request::Register(metadata)
            | Request::RegisterAsync {
                buffer: metadata, ..
            } => {
                let supported = self.feedback.as_ref().is_some_and(|f| {
                    f.tranches()
                        .iter()
                        .flat_map(|t| t.formats.iter())
                        .filter_map(|i| f.format_table().get(*i as usize))
                        .any(|f| f.format == metadata.fourcc && f.modifier == metadata.modifier)
                });
                if !supported
                    || stream.buffers.len() >= wire::MAX_BUFFERS
                    || stream.buffers.contains_key(&metadata.id)
                    || stream.imports.full()
                {
                    if let Some(attempt) = import_attempt {
                        stream
                            .events
                            .send_record(&wire::import_result(attempt, metadata.id, -95), &[])?;
                        return Ok(());
                    }
                    return stream.ack(-95);
                }
                let params = self.dmabuf_state.create_params(qh)?;
                for (i, (p, fd)) in metadata.planes.iter().zip(&fds).enumerate() {
                    params.add(fd.as_fd(), i as u32, p.offset, p.stride, metadata.modifier);
                }
                let lane = new_lane(&self.syncobj, &self.sync_manager, qh)?;
                let id = metadata.id;
                let params = params.create(
                    metadata.width as i32,
                    metadata.height as i32,
                    metadata.fourcc,
                    zwp_linux_buffer_params_v1::Flags::empty(),
                );
                stream.buffers.insert(
                    id,
                    Source {
                        metadata,
                        buffer: None,
                        lanes: vec![lane],
                        ticket: None,
                    },
                );
                assert!(stream.imports.insert(imports::Pending {
                    buffer: id,
                    attempt: import_attempt,
                    params
                }));
                // Ack only after the compositor confirms import. Failure is a
                // normal capability fallback, never a fatal Wayland error.
            }
            Request::Unregister(id) => {
                // Any read in flight keeps the allocation, not only the newest.
                if stream.imports.contains(id)
                    || !stream.buffers.contains_key(&id)
                    || stream.reads.iter().any(|r| r.buffer == id)
                {
                    return stream.ack(-16);
                }
                if let Some(mut source) = stream.buffers.remove(&id) {
                    if let Some(b) = source.buffer.take() {
                        b.destroy();
                    }
                }
                stream.ack(0)?;
            }
            Request::Stage(layer) => {
                let accepted = stream.staged.push(layer, fds.into_iter().next());
                stream.ack(if accepted { 0 } else { -7 })?;
            }
            Request::StageAsync(layer) => {
                // No per-layer reply. The failure remains sticky until the
                // Commit/Abort barrier; an overflowing prefix cannot be shown.
                stream.staged.push(layer, fds.into_iter().next());
            }
            Request::Abort => {
                stream.staged.clear();
                if let Some(scene) = stream.waiting.take().and_then(|commit| commit.scene) {
                    stream
                        .events
                        .send_record(&wire::commit_result(scene, wire::SCENE_RETRY), &[])?;
                }
                stream.end_wait("aborted by SurfaceFlinger");
                stream.ack(0)?;
            }
            Request::Commit {
                serial,
                width,
                height,
                count,
            }
            | Request::CommitAsync {
                serial,
                width,
                height,
                count,
                ..
            } => {
                self.submit_layer_scene(
                    qh,
                    object,
                    stream,
                    PendingCommit {
                        scene: async_scene,
                        serial,
                        width,
                        height,
                        count,
                    },
                )?;
            }
        }
        Ok(())
    }

    fn submit_layer_scene(
        &mut self,
        qh: &QueueHandle<Self>,
        object: TaskObjectId,
        stream: &mut Stream,
        commit: PendingCommit,
    ) -> Result<(), PresenterError> {
        let PendingCommit {
            scene,
            serial,
            width,
            height,
            count,
        } = commit;
        let task = &self.tasks[&object];
        let exact_geometry = stream.staged.layers().iter().all(|(l, _)| {
            let Some((lw, lh)) = task.logical_size else {
                return false;
            };
            let [x, y, w, h] = l.destination;
            [
                (i64::from(x), lw, width),
                (i64::from(y), lh, height),
                (i64::from(x) + i64::from(w), lw, width),
                (i64::from(y) + i64::from(h), lh, height),
            ]
            .iter()
            .all(|&(edge, logical, physical)| {
                physical > 0 && edge * i64::from(logical) % i64::from(physical) == 0
            })
        });
        let valid = !stream.staged.failed()
            && self.layer_globals.alpha.is_some()
            && task.accepts_present()
            && !task.suspended
            && task.configure_serial == serial
            && task.buffer_size == (width, height)
            && exact_geometry;
        let status = if valid {
            wire::assess_scene(
                stream
                    .staged
                    .layers()
                    .iter()
                    .map(|(l, fd)| (l, fd.is_some())),
                count,
                width,
                height,
                stream.last_ticket,
                |id| {
                    stream.buffers.get(&id).map(|b| wire::SourceState {
                        buffer: &b.metadata,
                        imported: b.buffer.is_some(),
                        ticket: b.ticket,
                        spare_timeline: b.spare_timeline(id, &stream.reads),
                    })
                },
                |id| stream.views.get(&id).and_then(View::source),
            )
        } else {
            wire::SceneStatus::Invalid
        };
        if status == wire::SceneStatus::Busy && scene.is_some() {
            // Keep the current scene visible and retain this scene's
            // metadata/acquire FDs. SF's one outstanding Commit bounds
            // admission and pins producer reads until actual attachment
            // or explicit rejection. Real release eventfds retry it;
            // no wait, polling spin, or fallback render is introduced.
            stream.park(object, commit, "busy: a reused buffer is not released yet");
            return Ok(());
        }
        if status != wire::SceneStatus::Ready {
            stream.staged.clear();
            // Off-grid edges or a missing alpha global refuse the arrangement
            // itself; anything else may pass with the next scene.
            let code = if !exact_geometry || self.layer_globals.alpha.is_none() {
                wire::SCENE_UNPLACEABLE
            } else {
                wire::SCENE_RETRY
            };
            if let Some(scene) = scene {
                stream
                    .events
                    .send_record(&wire::commit_result(scene, code), &[])?;
                return Ok(());
            }
            return stream.ack(code);
        }
        let task = self.tasks.get_mut(&object).unwrap();
        // A newer accepted direct scene supersedes a composed frame which has
        // not yet consumed a grant; never display that old fallback afterward.
        if let Some(queued) = task.timeline.queued.take() {
            self.endpoint
                .as_mut()
                .ok_or(PresenterError::Configuration("endpoint disappeared"))?
                .discard_frame(object, queued.frame.frame)?;
        }
        if task.timeline.exact && task.scene_feedback.len() >= MAX_SCENE_FEEDBACK {
            stream.park(object, commit, "presentation feedback limit");
            return Ok(());
        }
        let now = monotonic_timestamp_nanos()?;
        let Some(target) = task.timeline.reserve(now) else {
            stream.park(object, commit, "no frame grant");
            return Ok(());
        };
        self.android_frame(now, target.target.as_ref());
        self.commit_layers(
            qh,
            object,
            stream,
            (width, height),
            scene,
            target.target.as_ref(),
        )?;
        if let Some(scene) = scene {
            stream
                .events
                .send_record(&wire::commit_result(scene, 0), &[])?;
        } else {
            stream.ack(0)?;
        }
        Ok(())
    }
    fn commit_layers(
        &mut self,
        qh: &QueueHandle<Self>,
        object: TaskObjectId,
        stream: &mut Stream,
        size: (u32, u32),
        scene: Option<u64>,
        target_grant: Option<&frame_timeline::Target>,
    ) -> Result<(), PresenterError> {
        let (width, height) = size;
        let timing = droidloom_timing::Span::new(
            "timing_presenter_commit",
            object.0,
            stream.last_scene,
            stream.staged.layers().len() as u64,
        );
        let task = self.tasks.get_mut(&object).unwrap();
        let root = task.surface().unwrap().clone();
        let (lw, lh) = task.logical_size.ok_or(PresenterError::Configuration(
            "layer task has no logical size",
        ))?;
        // Retain the last composed image when direct children fully cover it.
        // Some compositors publish root textures and child layouts separately;
        // replacing the root with the backing can expose it during that handoff.
        // Partial/transparent scenes still require the backing, and an initial direct
        // scene needs a carrier to map its XDG root. No target is copied or
        // released here: normal compositor release fences retain ownership.
        let preserve_composed = task.composed_root
            && covered_by_opaque_layer(
                stream.staged.layers().iter().map(|(layer, _)| layer),
                width,
                height,
            );
        let backing = if preserve_composed {
            None
        } else {
            let size = carrier_size(lw, lh)?;
            let clear = task.see_through;
            let changed = stream
                .root
                .as_ref()
                .is_none_or(|(old, was_clear, _)| (*old, *was_clear) != (size, clear));
            if changed {
                let buffer = carrier(&self.layer_globals, qh, size, clear)?;
                if let Some((_, _, old)) = stream.root.replace((size, clear, buffer)) {
                    old.destroy();
                }
            }
            Some((size, changed || !stream.active || task.composed_root))
        };
        let mut staged = stream.staged.take();
        // Observe one changed child in the synchronized scene. The carrier root
        // may be fully occluded, so asking for its feedback could be discarded
        // even while the game is displayed. Never force unchanged children to
        // commit solely for timing, and bound callbacks independently of credits.
        let feedback_layer = if target_grant.is_some()
            || scene.is_some()
                && task.scene_feedback.len() < MAX_SCENE_FEEDBACK
                && self
                    .endpoint
                    .as_ref()
                    .is_some_and(|e| e.scene_presentation())
        {
            staged
                .iter()
                .rev()
                .find(|(layer, _)| {
                    layer.alpha != 0
                        && stream
                            .views
                            .get(&layer.id)
                            .is_none_or(|view| view.content != Some(Content::new(layer)))
                })
                .map(|(layer, _)| layer.id)
        } else {
            None
        };
        let ids: BTreeSet<_> = staged.iter().map(|(l, _)| l.id).collect();
        if stream.clear.is_none() {
            stream.clear = Some(pixel(&self.layer_globals, qh, 0)?);
        }
        recycle_views(
            &mut stream.views,
            &mut stream.spare_views,
            &ids,
            stream.clear.as_ref().unwrap(),
        );
        let mut previous = root.clone();
        let mut previous_id = None;
        for (l, acquire) in staged.drain(..) {
            let v = view_for_layer(&mut stream.views, &mut stream.spare_views, l.id, || {
                let surface = self.compositor.create_surface(qh);
                let role = self
                    .layer_globals
                    .subcompositor
                    .get_subsurface(&surface, &root, qh, ());
                role.set_sync();
                let viewport = self.viewporter.get_viewport(&surface, qh, ());
                let alpha =
                    self.layer_globals
                        .alpha
                        .as_ref()
                        .unwrap()
                        .get_surface(&surface, qh, ());
                let sync = None;
                let timeline = self
                    .frame_timeline_manager
                    .as_ref()
                    .map(|m| frame_timeline::Surface::new(m, &surface, qh));
                // Route input to the root, retaining task-relative Android coordinates.
                let empty = Region::new(&self.compositor)?;
                surface.set_input_region(Some(empty.wl_region()));
                Ok(View {
                    timeline,
                    surface,
                    role,
                    viewport,
                    alpha,
                    sync,
                    content: None,
                    solid: None,
                    geometry: None,
                })
            })?;
            let geometry = Geometry::new(&l, previous_id, (lw, lh), (width, height));
            v.update(geometry, Content::new(&l), |v, geometry_changed| {
                if feedback_layer == Some(l.id) {
                    let proxy = self.presentation_time.feedback(&v.surface, qh)?;
                    task.scene_feedback.push(SceneFeedback {
                        grant: target_grant.map(|g| g.identity),
                        proxy,
                        scene: scene.unwrap_or(0),
                        configure_serial: task.configure_serial,
                    });
                }
                // A new ticket for the read this view shows relabels it. Any
                // other new ticket is a new read of the allocation.
                let held = stream
                    .buffers
                    .get(&l.buffer)
                    .and_then(|b| b.ticket)
                    .filter(|held| *held != l.ticket);
                let relabel = held.filter(|held| v.source() == Some((l.buffer, *held)));
                let new_read = l.buffer != 0
                    && relabel.is_none()
                    && stream
                        .buffers
                        .get(&l.buffer)
                        .is_some_and(|b| b.ticket != Some(l.ticket));
                if (l.buffer == 0 || new_read)
                    && let (Some(timeline), Some(grant)) = (&v.timeline, target_grant)
                {
                    timeline.target(grant);
                }
                if geometry_changed {
                    v.role.place_above(&previous);
                    let [dx, dy, dw, dh] = geometry.destination;
                    v.role.set_position(dx, dy);
                    v.viewport.set_destination(dw, dh);
                    v.alpha.set_multiplier(geometry.alpha);
                    if geometry.opaque {
                        let r = Region::new(&self.compositor)?;
                        r.add(0, 0, dw, dh);
                        v.surface.set_opaque_region(Some(r.wl_region()));
                    } else {
                        v.surface.set_opaque_region(None);
                    }
                    v.surface.set_buffer_transform(
                        wl_output::Transform::try_from(geometry.transform)
                            .map_err(|_| PresenterError::Configuration("layer transform"))?,
                    );
                    v.viewport.set_source(
                        f64::from(geometry.source[0]) / 256.0,
                        f64::from(geometry.source[1]) / 256.0,
                        f64::from(geometry.source[2]) / 256.0,
                        f64::from(geometry.source[3]) / 256.0,
                    );
                }
                if l.buffer == 0 {
                    if let Some(sync) = v.sync.take() {
                        sync.destroy();
                    }
                    if v.solid.as_ref().is_none_or(|(color, _)| *color != l.color) {
                        let b = pixel(&self.layer_globals, qh, l.color)?;
                        if let Some((_, old)) = v.solid.replace((l.color, b)) {
                            old.destroy();
                        }
                    }
                    v.surface.attach(v.solid.as_ref().map(|(_, b)| b), 0, 0);
                } else {
                    let b = stream.buffers.get_mut(&l.buffer).unwrap();
                    if new_read {
                        // A read still in flight keeps its lane, such as one
                        // whose view the compositor has not released since
                        // composition hid it. assess_scene admitted this read
                        // only with a free lane or room for one.
                        let lane = match b.free_lane(l.buffer, &stream.reads) {
                            Some(lane) => lane,
                            None => {
                                b.lanes
                                    .push(new_lane(&self.syncobj, &self.sync_manager, qh)?);
                                b.lanes.len() - 1
                            }
                        };
                        let s = &mut b.lanes[lane];
                        let acquire_point = s
                            .sync
                            .next_point
                            .checked_add(1)
                            .ok_or(PresenterError::Configuration("layer timeline exhausted"))?;
                        let release = acquire_point
                            .checked_add(1)
                            .ok_or(PresenterError::Configuration("layer timeline exhausted"))?;
                        if let Some(fd) = acquire {
                            s.sync
                                .timeline
                                .import_acquire_fence(acquire_point, fd.as_fd())?;
                        } else {
                            s.sync.timeline.signal_acquire(acquire_point)?;
                        }
                        s.sync.next_point = release;
                        s.sync.arm_release_wakeup(release)?;
                        // Fence availability unblocks Android callbacks. Completion
                        // separately permits this allocation/timeline to be reused.
                        s.availability_wakeup.arm(&s.sync.timeline, release)?;
                        let sync = v.sync.get_or_insert_with(|| {
                            self.sync_manager.get_surface(&v.surface, qh, ())
                        });
                        let (hi, lo) = split_point(acquire_point);
                        sync.set_acquire_point(&s.sync.proxy, hi, lo);
                        let (hi, lo) = split_point(release);
                        sync.set_release_point(&s.sync.proxy, hi, lo);
                        v.surface.attach(b.buffer.as_ref(), 0, 0);
                        b.ticket = Some(l.ticket);
                        stream.last_ticket = stream.last_ticket.max(l.ticket);
                        stream.reads.push(Read {
                            buffer: l.buffer,
                            lane,
                            ticket: l.ticket,
                            point: release,
                            fence_sent: false,
                        });
                    } else if let Some(held) = relabel {
                        // SurfaceFlinger forgets its attachments when it falls
                        // back to composition and re-sends the read this view
                        // shows under a new ticket. Keep the attachment: a new
                        // acquire point on this lane would sit behind the
                        // unsignalled release point it replaces. The new
                        // ticket ends with the read it relabels.
                        let (lane, point, fence_sent) = stream
                            .reads
                            .iter()
                            .find(|r| r.buffer == l.buffer && r.ticket == held)
                            .map(|r| (r.lane, r.point, r.fence_sent))
                            .ok_or(PresenterError::Configuration(
                                "relabelled layer read is not outstanding",
                            ))?;
                        b.ticket = Some(l.ticket);
                        stream.last_ticket = stream.last_ticket.max(l.ticket);
                        // A point exports one fence. If the relabelled read's
                        // went out already, this ticket ends by completion.
                        stream.reads.push(Read {
                            buffer: l.buffer,
                            lane,
                            ticket: l.ticket,
                            point,
                            fence_sent,
                        });
                    }
                }
                Ok(())
            })?;
            previous = v.surface.clone();
            previous_id = Some(l.id);
        }
        // Retain the bounded layer capacity across buffer-only frames.
        stream.staged.recycle(staged);
        // Hidden roles remain reusable until the task retires. Their total is
        // bounded by the largest accepted scene (MAX_LAYERS), not frame count.
        if !stream.active
            || backing.is_some_and(|(_, attach)| attach)
            || stream.root_logical_size != Some((lw, lh))
        {
            if task.see_through {
                root.set_opaque_region(None);
            } else {
                let region = Region::new(&self.compositor)?;
                region.add(0, 0, lw as i32, lh as i32);
                root.set_opaque_region(Some(region.wl_region()));
            }
            root.set_buffer_transform(wl_output::Transform::Normal);
            task.viewport
                .as_ref()
                .unwrap()
                .set_destination(lw as i32, lh as i32);
            stream.root_logical_size = Some((lw, lh));
        }
        if let Some((size, true)) = backing {
            if let Some(sync) = task.sync_surface.take() {
                sync.destroy();
            }
            task.viewport.as_ref().unwrap().set_source(
                0.0,
                0.0,
                f64::from(size.0),
                f64::from(size.1),
            );
            root.attach(stream.root.as_ref().map(|(_, _, buffer)| buffer), 0, 0);
            root.damage_buffer(0, 0, size.0 as i32, size.1 as i32);
            if let (Some(timeline), Some(grant)) = (&task.timeline.surface, target_grant) {
                timeline.target(grant);
            }
            task.composed_root = false;
        }
        if let Some(grant) = target_grant {
            if feedback_layer.is_none() {
                if backing.is_some_and(|(_, attach)| attach) {
                    task.scene_feedback.push(SceneFeedback {
                        proxy: self.presentation_time.feedback(&root, qh)?,
                        scene: scene.unwrap_or(0),
                        configure_serial: task.configure_serial,
                        grant: Some(grant.identity),
                    });
                } else {
                    task.timeline
                        .finish(Some(grant.identity), frame_timeline::Terminal::Skipped);
                }
            }
        }
        root.commit();
        self.activation.mapped(object, &root);
        if !stream.active && droidloom_timing::traced(droidloom_timing::Diagnostic::Audit) {
            eprintln!(
                "Droidloom task {}: direct Wayland composition, {} layers, backing={}",
                object.0,
                ids.len(),
                if preserve_composed {
                    "composed"
                } else {
                    "carrier"
                },
            );
        }
        task.applied_opaque = Some(true);
        stream.active = true;
        timing.finish(1);
        Ok(())
    }
    pub(super) fn layer_imported(
        &mut self,
        params: &zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1,
        buffer: Option<wl_buffer::WlBuffer>,
    ) -> bool {
        for (object, task) in &mut self.tasks {
            let Some(s) = task.layers.as_mut() else {
                continue;
            };
            let Some(pending) = s.imports.finish(params) else {
                continue;
            };
            let id = pending.buffer;
            pending.params.destroy();
            let code = if let Some(buffer) = buffer {
                s.buffers.get_mut(&id).unwrap().buffer = Some(buffer);
                0
            } else {
                s.buffers.remove(&id);
                -95
            };
            if s.connected {
                let result = if let Some(attempt) = pending.attempt {
                    s.events
                        .send_record(&wire::import_result(attempt, id, code), &[])
                        .map_err(PresenterError::from)
                } else {
                    s.ack(code)
                };
                if let Err(e) = result {
                    self.fatal = Some(e.to_string());
                }
            }
            // Only legacy imports pause request polling. Resume any commands
            // already queued without relying on another peer write.
            if pending.attempt.is_none() {
                self.task_work.mark(*object, readiness::REQUESTS);
            }
            return true;
        }
        // A window can retire while compositor import callbacks are queued.
        // Such callbacks carry no read ownership and must not revive the task.
        if let Some(buffer) = buffer {
            buffer.destroy();
        }
        false
    }
}
wayland_client::delegate_noop!(App: ignore wl_subcompositor::WlSubcompositor);
wayland_client::delegate_noop!(App: ignore wl_subsurface::WlSubsurface);
wayland_client::delegate_noop!(App: ignore wl_shm::WlShm);
wayland_client::delegate_noop!(App: ignore wl_shm_pool::WlShmPool);
wayland_client::delegate_noop!(App: ignore wl_buffer::WlBuffer);
wayland_client::delegate_noop!(App: ignore WpAlphaModifierV1);
wayland_client::delegate_noop!(App: ignore WpAlphaModifierSurfaceV1);

#[cfg(test)]
#[path = "layer_updates_test.rs"]
mod update_tests;

#[cfg(test)]
mod tests {
    use super::{Geometry, Layer, carrier_size, covered_by_opaque_layer};

    fn fixture() -> Layer {
        Layer {
            id: 1,
            buffer: 10,
            ticket: 1,
            destination: [0, 0, 800, 600],
            source: [0, 0, 800 * 256, 600 * 256],
            transform: 0,
            alpha: u32::MAX,
            opaque: true,
            color: 0,
        }
    }

    #[test]
    fn composed_backing_requires_proven_opaque_coverage_of_the_entire_task() {
        let layer = fixture();
        let covered = |layer: &Layer| covered_by_opaque_layer([layer].into_iter(), 800, 600);
        assert!(covered(&layer));
        let mut translucent_peer = layer.clone();
        translucent_peer.id = 2;
        translucent_peer.opaque = false;
        translucent_peer.destination = [10, 10, 20, 20];
        assert!(covered_by_opaque_layer(
            [&layer, &translucent_peer].into_iter(),
            800,
            600
        ));
        for change in 0..5 {
            let mut changed = layer.clone();
            match change {
                0 => changed.opaque = false,
                1 => changed.alpha -= 1,
                2 => changed.destination[0] = 1,
                3 => changed.destination[2] -= 1,
                _ => changed.destination[3] -= 1,
            }
            assert!(!covered(&changed));
        }
        let mut solid = layer.clone();
        solid.buffer = 0;
        solid.color = 0xff00ff00;
        assert!(covered(&solid));
        solid.color = 0xfe00ff00;
        assert!(!covered(&solid));
        assert!(!covered_by_opaque_layer([&layer].into_iter(), 801, 600));
        assert!(!covered_by_opaque_layer([&layer].into_iter(), 0, 600));
        assert!(!covered_by_opaque_layer(
            [&layer].into_iter(),
            u32::MAX,
            600
        ));
        assert!(!covered_by_opaque_layer(std::iter::empty(), 800, 600));
    }

    #[test]
    fn rotating_producer_buffers_preserves_wayland_geometry() {
        let mut layer = fixture();
        let original = Geometry::new(&layer, None, (400, 300), (800, 600));
        assert_eq!(original.destination, [0, 0, 400, 300]);
        for frame in 2..100 {
            layer.buffer = 10 + frame % 3;
            layer.ticket = frame;
            assert_eq!(
                Geometry::new(&layer, None, (400, 300), (800, 600)),
                original
            );
        }
        assert_ne!(
            Geometry::new(&layer, None, (800, 600), (800, 600)),
            original
        );
        assert_ne!(
            Geometry::new(&layer, Some(2), (400, 300), (800, 600)),
            original
        );
        for change in 0..5 {
            let mut changed = layer.clone();
            match change {
                0 => changed.opaque = false,
                1 => changed.alpha /= 2,
                2 => changed.transform = 2,
                3 => changed.source[2] /= 2,
                _ => changed.destination[2] /= 2,
            }
            assert_ne!(
                Geometry::new(&changed, None, (400, 300), (800, 600)),
                original
            );
        }
    }

    #[test]
    fn solid_buffer_transitions_restore_the_correct_viewport_source() {
        let mut layer = fixture();
        let buffered = Geometry::new(&layer, None, (400, 300), (800, 600));
        layer.buffer = 0;
        let solid = Geometry::new(&layer, None, (400, 300), (800, 600));
        assert_eq!(solid.source, [0, 0, 256, 256]);
        assert_ne!(solid, buffered);
        layer.color = 0xffaabbcc;
        assert_eq!(Geometry::new(&layer, None, (400, 300), (800, 600)), solid);
        layer.buffer = 11;
        assert_eq!(
            Geometry::new(&layer, None, (400, 300), (800, 600)),
            buffered
        );
    }

    #[test]
    fn root_backing_preserves_task_aspect_through_resize_and_rotation() {
        // The Moto task is 610 x 1356 logical pixels. A 1 x 1 carrier made
        // the mobile shell fit its portrait surface tree into a square first.
        for (width, height) in [(610, 1356), (1356, 610), (611, 1357), (800, 600)] {
            let (root_width, root_height) = carrier_size(width, height).unwrap();
            assert!(root_width > 0 && root_height > 0);
            assert_eq!(
                u64::from(root_width) * u64::from(height),
                u64::from(root_height) * u64::from(width),
                "root aspect changed for {width} x {height}",
            );
            assert!(root_width <= width && root_height <= height);
        }
        assert!(carrier_size(0, 1356).is_err());
        assert!(carrier_size(610, 0).is_err());
    }
}
