# Droidloom threat model v2

These are security design requirements, not a claim that the preview enforces
every control.

The current preview still needs production isolation hardening. In particular,
requirements below for subordinate UID mapping, seccomp and signed distribution
must not be read as evidence of enforcement.

## Security claim

The primary boundary is designed to contain an untrusted Android application
that has not escaped Android's application sandbox. Defense in depth should
also make a framework or cell compromise harder to turn into host compromise,
but version 2 does **not** claim VM-equivalent containment of a fully
compromised Android cell. The host kernel, selected DRM render driver, host compositor,
and `droidloomd` remain trusted computing base.

## Principals and trust

| Principal | Trust | Authority |
| --- | --- | --- |
| Android APK and native libraries | Untrusted | Android app UID, granted Android permissions |
| Android framework/cell | Partially trusted | Private namespaces, Binder contexts, delegated cgroup |
| Per-user presenter and catalog | User-trusted | Lifecycle requests and narrow host portals for that user |
| Per-user audio broker | User-trusted | PipeWire graph access and explicitly granted cell capture |
| `droidloomd` | Host-trusted, minimal | Namespace/mount/device/cgroup/network construction only |
| Host compositor | Host-trusted | Display, input, shell, task, capture, and power policy |
| Image/package manager | Host-trusted | Signed immutable system content and explicit activation |
| Linux kernel and selected GPU driver | Trusted computing base | Shared execution and render-node ABI |

## Protected assets

- host filesystem content outside explicit portals;
- host UID identities and credentials;
- host routes, firewall, sockets, and other users' traffic;
- physical display/KMS, input devices, camera, microphone, sensors, USB, modem,
  Bluetooth, and power controls;
- other users' Android cells and `/data`;
- The host compositor's private control channels and compositor-owned buffers;
- package-owned immutable Android system content.

## Mandatory controls

1. Map Android IDs into a dedicated, non-overlapping subordinate range. Never
   map Android root to host root or the interactive user's host UID.
2. Create private PID, mount, IPC, UTS, network, cgroup, and Binder namespaces.
3. Assemble mounts and devices in the supervisor. Android receives no general
   host mount authority and no broad `/dev` bind mount.
4. Expose only the selected DRM render node and the explicitly configured
   graphics backend's bounded auxiliary devices. `kgsl_dma_heap` adds only
   `/dev/kgsl-3d0` and `/dev/dma_heap/system`, while `drm_dma_heap` adds only
   `/dev/dma_heap/system`; their drivers become part of the
   trusted kernel interface. Other backends get `/dev/dma_heap/system` for
   Codec2 media buffers only when the host has it. Validate device identities and use private inodes
   so cell permissions cannot alter host nodes. DRM card/KMS and physical input
   nodes are forbidden.
5. Put Android networking behind a private veth and host-owned policy. Android
   `netd` never shares host route/firewall authority.
6. Authenticate the private compositor and lifecycle sockets with filesystem
   ownership plus peer credentials. Treat task/package identity sent by the
   Android side as untrusted until reconciled with the session service.
7. Validate and bound every message, string, object count, dimension, file
   descriptor, DMA-BUF plane, format/modifier, fence, and lifecycle transition.
8. Apply no-new-privileges and seccomp after construction; retain only the
   capabilities proven necessary for Android boot.
9. Keep system/vendor/product immutable and package signed. `/data` is the only
   normal mutable Android state.
10. Make teardown idempotent and host-owned. A dead or hostile cell must not be
    needed to release its cgroup, mounts, veth, Binder instance, or buffers.

## Boundary-specific attacks

- Binder: use a private binderfs instance and three private contexts; never
  expose host Binder devices.
- Denial protocol/DMA-BUF: reject truncated sequenced records, ancillary-data
  truncation, invalid plane counts, descriptor-role mismatches, overflows,
  unsupported format/modifier pairs, foreign devices, impossible dimensions,
  stale timeline points, and ambiguous ownership. Retain buffers until GPU/KMS
  completion.
- Portals: use typed, least-authority requests; pass individual descriptors
  instead of paths or host directory mounts.
- Framework launch/catalog: the existing lifecycle authority uses typed native
  RPC to system_server instead of privileged command children. Check socket
  ownership/mode and kernel peer credentials on both ends, bound frames and icon
  batches, and reject foreign package/component/user identities before side
  effects. Delegate catalog event descriptors only to the authorized cell owner;
  unexpected client writes end the subscription. No general Binder or command
  execution endpoint is exposed. Current UID 0/1000 checks require supervisor
  mapping when production subordinate IDs are enabled.
- Notifications: require a complete initial snapshot before deltas; bound the
  active set and match removal/action versions so stale records cannot affect a
  newer notification. PendingIntents remain inside Android.
- Audio: project only the private sequenced-packet endpoint, never the host
  PipeWire socket, session bus or ALSA device nodes. Authenticate the kernel
  peer UID; default capture to denied. Bound each connection to four dedicated
  size-sealed allocations, validate every slot/offset/frame credit, and keep
  graph metadata outside cell-writable memory. Capture descriptors are read-only
  and the host retains inode ownership. Broker restart ends capture and invalidates
  the generation; previously delivered samples cannot be revoked. Host policy
  currently grants the cell, not an individual Android app. The preview's UID
  1041 check needs supervisor mapping before production subordinate IDs.
- Image activation: validate schema/ABI and artifact hashes before an atomic
  version switch. Never execute hooks from an untrusted image.
- Network: host policy owns NAT/routing. Prevent spoofing outside the assigned
  veth and tear policy down by stable cell identity.
- Power supply: expose only a fixed allowlist of battery and charger status
  files through read-only mounts. Exclude identifiers, controls, device links,
  and unrecognized supply types.
- Resource exhaustion: cap processes, memory, CPU, file descriptors, Binder
  allocations, surfaces, buffers, and queued messages at the host cgroup and
  protocol layers.

## Residual risks and escalation

Kernel, Binder, GPU, and shared-filesystem vulnerabilities can cross the cell
boundary. Android SELinux cannot install a private policy into a conventional
shared host kernel. Protected media, hardware attestation, and Play Integrity
are not supported claims. Workloads requiring containment after total cell
compromise must use a separately designed hardened VM mode.

Any new device service, host portal, privileged capability, shared namespace,
or protected-buffer path changes this threat model and requires a new version.
