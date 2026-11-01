//! Record real client wire requests on a private socket pair, without a GPU or
//! desktop connection. The peer sends no events; registry names are fixture IDs.
use super::*;
use std::io::Read;
use std::os::unix::net::UnixStream;
use wayland_client::protocol::{wl_compositor, wl_registry};

struct Client;
wayland_client::delegate_noop!(Client: ignore wl_registry::WlRegistry);
wayland_client::delegate_noop!(Client: ignore wl_compositor::WlCompositor);
wayland_client::delegate_noop!(Client: ignore wl_surface::WlSurface);
wayland_client::delegate_noop!(Client: ignore wl_subcompositor::WlSubcompositor);
wayland_client::delegate_noop!(Client: ignore wl_subsurface::WlSubsurface);
wayland_client::delegate_noop!(Client: ignore WpViewporter);
wayland_client::delegate_noop!(Client: ignore WpViewport);
wayland_client::delegate_noop!(Client: ignore WpAlphaModifierV1);
wayland_client::delegate_noop!(Client: ignore WpAlphaModifierSurfaceV1);
wayland_client::delegate_noop!(Client: ignore wl_shm::WlShm);
wayland_client::delegate_noop!(Client: ignore wl_shm_pool::WlShmPool);
wayland_client::delegate_noop!(Client: ignore wl_buffer::WlBuffer);

struct Wire {
    conn: Connection,
    peer: UnixStream,
    _queue: EventQueue<Client>,
    qh: QueueHandle<Client>,
    compositor: wl_compositor::WlCompositor,
    subcompositor: wl_subcompositor::WlSubcompositor,
    viewporter: WpViewporter,
    alpha: WpAlphaModifierV1,
    buffer: wl_buffer::WlBuffer,
    root: wl_surface::WlSurface,
}

impl Wire {
    fn new() -> Self {
        let (client, peer) = UnixStream::pair().unwrap();
        peer.set_nonblocking(true).unwrap();
        let conn = Connection::from_socket(client).unwrap();
        let queue = conn.new_event_queue::<Client>();
        let qh = queue.handle();
        let registry = conn.display().get_registry(&qh, ());
        let compositor = registry.bind::<wl_compositor::WlCompositor, _, _>(1, 6, &qh, ());
        let subcompositor = registry.bind::<wl_subcompositor::WlSubcompositor, _, _>(2, 1, &qh, ());
        let viewporter = registry.bind::<WpViewporter, _, _>(3, 1, &qh, ());
        let alpha = registry.bind::<WpAlphaModifierV1, _, _>(4, 1, &qh, ());
        let shm = registry.bind::<wl_shm::WlShm, _, _>(5, 1, &qh, ());
        let file = tempfile::tempfile().unwrap();
        file.set_len(4).unwrap();
        let pool = shm.create_pool(file.as_fd(), 4, &qh, ());
        let buffer = pool.create_buffer(0, 1, 1, 4, wl_shm::Format::Argb8888, &qh, ());
        pool.destroy();
        let root = compositor.create_surface(&qh, ());
        Self {
            conn,
            peer,
            _queue: queue,
            qh,
            compositor,
            subcompositor,
            viewporter,
            alpha,
            buffer,
            root,
        }
    }

    fn view(&self) -> View {
        let surface = self.compositor.create_surface(&self.qh, ());
        let role = self
            .subcompositor
            .get_subsurface(&surface, &self.root, &self.qh, ());
        role.set_sync();
        let viewport = self.viewporter.get_viewport(&surface, &self.qh, ());
        let alpha = self.alpha.get_surface(&surface, &self.qh, ());
        View {
            timeline: None,
            surface,
            role,
            viewport,
            alpha,
            sync: None,
            content: None,
            solid: None,
            geometry: None,
        }
    }

    fn update(&self, view: &mut View, geometry: Geometry, content: Content) {
        view.update(geometry, content, |view, geometry_changed| {
            if geometry_changed {
                view.role
                    .set_position(geometry.destination[0], geometry.destination[1]);
            }
            view.surface.attach(Some(&self.buffer), 0, 0);
            Ok(())
        })
        .unwrap();
    }

    fn requests(&mut self) -> Vec<(u32, u16)> {
        self.conn.flush().unwrap();
        let mut bytes = Vec::new();
        let mut buffer = [0; 4096];
        loop {
            match self.peer.read(&mut buffer) {
                Ok(0) => panic!("private Wayland connection closed"),
                Ok(n) => bytes.extend_from_slice(&buffer[..n]),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => panic!("{error}"),
            }
        }
        let mut requests = Vec::new();
        let mut offset = 0;
        while offset < bytes.len() {
            let object = u32::from_ne_bytes(bytes[offset..offset + 4].try_into().unwrap());
            let header = u32::from_ne_bytes(bytes[offset + 4..offset + 8].try_into().unwrap());
            let size = (header >> 16) as usize;
            assert!(size >= 8 && offset + size <= bytes.len());
            requests.push((object, header as u16));
            offset += size;
        }
        requests
    }
}

fn count<P: Proxy>(requests: &[(u32, u16)], proxy: &P, name: &str) -> usize {
    requests
        .iter()
        .filter(|(object, opcode)| {
            *object == proxy.id().protocol_id()
                && P::interface().requests[usize::from(*opcode)].name == name
        })
        .count()
}

fn geometry() -> Geometry {
    Geometry {
        above: None,
        destination: [0, 0, 100, 100],
        source: [0, 0, 256, 256],
        transform: 0,
        alpha: u32::MAX,
        opaque: true,
    }
}

#[test]
fn animated_peer_sends_no_requests_for_unchanged_buffer_or_solid_children() {
    let mut wire = Wire::new();
    let mut buffered = wire.view();
    let mut solid = wire.view();
    let mut animated = wire.view();
    let held = Content::Buffer { id: 10, ticket: 1 };
    for (view, content) in [
        (&mut buffered, held),
        (&mut solid, Content::Solid(0xff112233)),
        (&mut animated, Content::Buffer { id: 11, ticket: 2 }),
    ] {
        wire.update(view, geometry(), content);
    }
    wire.root.commit();
    wire.requests();
    for ticket in 3..103 {
        wire.update(&mut buffered, geometry(), held);
        wire.update(&mut solid, geometry(), Content::Solid(0xff112233));
        wire.update(
            &mut animated,
            geometry(),
            Content::Buffer { id: 11, ticket },
        );
        wire.root.commit();
    }
    let requests = wire.requests();
    for unchanged in [&buffered, &solid] {
        assert!(
            !requests
                .iter()
                .any(|(object, _)| *object == unchanged.surface.id().protocol_id())
        );
    }
    assert_eq!(count(&requests, &animated.surface, "attach"), 100);
    assert_eq!(count(&requests, &animated.surface, "damage_buffer"), 100);
    assert_eq!(count(&requests, &animated.surface, "commit"), 100);
    assert_eq!(count(&requests, &wire.root, "commit"), 100);
    assert_eq!(buffered.source(), Some((10, 1)));
}

#[test]
fn reused_allocation_with_a_new_ticket_still_attaches_damages_and_commits() {
    let mut wire = Wire::new();
    let mut view = wire.view();
    wire.update(&mut view, geometry(), Content::Buffer { id: 10, ticket: 1 });
    wire.requests();
    let next = Content::Buffer { id: 10, ticket: 2 };
    wire.update(&mut view, geometry(), next);
    let requests = wire.requests();
    for name in ["attach", "damage_buffer", "commit"] {
        assert_eq!(count(&requests, &view.surface, name), 1);
    }
    assert_eq!(view.source(), Some((10, 2)));
    wire.update(&mut view, geometry(), next);
    assert!(wire.requests().is_empty());
}

#[test]
fn geometry_changes_commit_without_replacing_the_read_identity() {
    let mut wire = Wire::new();
    let mut view = wire.view();
    let content = Content::Buffer { id: 10, ticket: 1 };
    wire.update(&mut view, geometry(), content);
    wire.requests();
    for field in 0..6 {
        let mut changed = geometry();
        match field {
            0 => changed.above = Some(8),
            1 => changed.destination = [10, 20, 50, 60],
            2 => changed.source = [0, 0, 128, 128],
            3 => changed.transform = 2,
            4 => changed.alpha /= 2,
            _ => changed.opaque = false,
        }
        view.update(changed, content, |_, geometry_changed| {
            assert!(geometry_changed);
            Ok(())
        })
        .unwrap();
        let requests = wire.requests();
        assert_eq!(count(&requests, &view.surface, "attach"), 0);
        assert_eq!(count(&requests, &view.surface, "damage_buffer"), 1);
        assert_eq!(count(&requests, &view.surface, "commit"), 1);
        assert_eq!(view.source(), Some((10, 1)));
        view.update(changed, content, |_, _| {
            panic!("unchanged layer applied twice")
        })
        .unwrap();
        assert!(wire.requests().is_empty());
    }
}

#[test]
fn color_and_buffer_solid_transitions_commit_and_hiding_invalidates_both_kinds() {
    let mut wire = Wire::new();
    let mut view = wire.view();
    wire.update(&mut view, geometry(), Content::Buffer { id: 10, ticket: 1 });
    wire.requests();
    for content in [
        Content::Solid(0xff112233),
        Content::Solid(0xff334455),
        Content::Buffer { id: 10, ticket: 2 },
    ] {
        wire.update(&mut view, geometry(), content);
        let requests = wire.requests();
        assert_eq!(count(&requests, &view.surface, "commit"), 1);
        if matches!(content, Content::Solid(_)) {
            assert_eq!(view.source(), None);
        }
    }
    for content in [
        Content::Solid(0xff334455),
        Content::Buffer { id: 10, ticket: 3 },
    ] {
        wire.update(&mut view, geometry(), content);
        wire.requests();
        view.hide(&wire.buffer);
        let requests = wire.requests();
        assert_eq!(count(&requests, &view.surface, "attach"), 1);
        assert_eq!(count(&requests, &view.surface, "commit"), 1);
        assert_eq!(count(&requests, &view.surface, "damage_buffer"), 1);
        // The pixel keeps no old size or position that could stretch the
        // window's surface tree.
        assert_eq!(count(&requests, &view.viewport, "set_destination"), 1);
        assert_eq!(count(&requests, &view.role, "set_position"), 1);
        assert_eq!(view.geometry, None);
        assert_eq!(view.source(), None);
        view.hide(&wire.buffer);
        assert!(wire.requests().is_empty());
        wire.update(&mut view, geometry(), content);
        let requests = wire.requests();
        for name in ["attach", "damage_buffer", "commit"] {
            assert_eq!(
                count(&requests, &view.surface, name),
                1,
                "hidden content must reattach"
            );
        }
    }
}

#[test]
fn unsuccessful_update_does_not_cache_uncommitted_geometry_or_content() {
    let mut wire = Wire::new();
    let mut view = wire.view();
    wire.update(&mut view, geometry(), Content::Buffer { id: 10, ticket: 1 });
    wire.requests();
    let mut changed = geometry();
    changed.alpha /= 2;
    let content = Content::Solid(0xffaabbcc);
    assert!(
        view.update(changed, content, |_, _| {
            Err(PresenterError::Configuration("test attachment failure"))
        })
        .is_err()
    );
    assert!(wire.requests().is_empty());
    assert_eq!(view.geometry, Some(geometry()));
    assert_eq!(view.source(), Some((10, 1)));
    wire.update(&mut view, changed, content);
    assert_eq!(count(&wire.requests(), &view.surface, "commit"), 1);
}

#[test]
fn replacement_scene_keeps_old_roles_alive_until_synchronized_state_applies() {
    let mut wire = Wire::new();
    let mut views = BTreeMap::new();
    let mut spare = Vec::new();
    let view = view_for_layer(&mut views, &mut spare, 1, || Ok(wire.view())).unwrap();
    wire.update(view, geometry(), Content::Buffer { id: 10, ticket: 1 });
    let surface = view.surface.clone();
    let role = view.role.clone();
    wire.root.commit();
    wire.requests();

    recycle_views(&mut views, &mut spare, &BTreeSet::from([2]), &wire.buffer);
    assert_eq!(spare.len(), 1);
    assert_eq!(spare[0].geometry, None);
    let view = view_for_layer(&mut views, &mut spare, 2, || {
        panic!("replacement must reuse the synchronized role")
    })
    .unwrap();
    assert_eq!(view.surface, surface);
    assert_eq!(view.role, role);
    wire.update(view, geometry(), Content::Buffer { id: 11, ticket: 2 });
    wire.root.commit();
    // The peer sends no events: no replacement frame has completed. A destroy
    // here would unmap the preceding frame immediately despite its parent's
    // GPU acquire blocker; the clear pixel and the new buffer remain synchronized.
    let requests = wire.requests();
    assert_eq!(count(&requests, &surface, "destroy"), 0);
    assert!(!requests.iter().any(|(object, opcode)| {
        *object == role.id().protocol_id()
            && wl_subsurface::WlSubsurface::interface().requests[usize::from(*opcode)].name
                == "destroy"
    }));
    assert_eq!(count(&requests, &surface, "attach"), 2);
    assert_eq!(count(&requests, &surface, "commit"), 2);
    assert_eq!(count(&requests, &wire.root, "commit"), 1);
}

#[test]
fn changing_layer_ids_reuses_a_bounded_pool_and_hidden_views_do_not_resurrect() {
    let mut wire = Wire::new();
    let mut views = BTreeMap::new();
    let mut spare = Vec::new();
    let mut allocations = 0;
    for scene in 0..100_u64 {
        let ids: BTreeSet<_> = (0..1 + scene % 3).map(|id| scene * 3 + id).collect();
        recycle_views(&mut views, &mut spare, &ids, &wire.buffer);
        for id in &ids {
            let view = view_for_layer(&mut views, &mut spare, *id, || {
                allocations += 1;
                Ok(wire.view())
            })
            .unwrap();
            wire.update(view, geometry(), Content::Solid(0xff112233));
        }
        wire.root.commit();
        assert!(views.len() + spare.len() <= 3);
        assert!(spare.iter().all(|view| view.content.is_none()));
        let requests = wire.requests();
        for hidden in &spare {
            assert_eq!(count(&requests, &hidden.surface, "destroy"), 0);
            // A role detached in an earlier scene must produce no requests.
            if count(&requests, &hidden.surface, "attach") == 0 {
                assert_eq!(count(&requests, &hidden.surface, "commit"), 0);
            }
        }
    }
    assert_eq!(allocations, 3);
    recycle_views(&mut views, &mut spare, &BTreeSet::new(), &wire.buffer);
    wire.root.commit();
    assert!(views.is_empty());
    assert_eq!(spare.len(), 3);
    assert!(spare.iter().all(|view| view.content.is_none()));
}
