# Runtime and namespace layout v1

This is the construction contract for the Rust `droidloomd`. The supervisor
implements the fail-closed transaction, emits the complete typed Linux
construction/reverse-teardown plan, and provides the executable Android
cell path. `droidloomd` retains the exact namespace child handle while
`droidloomctl` supplies idempotent lifecycle operations. Production user-ID
mapping, cgroup limits, general seccomp sandboxing, and complete packaging hardening remain later
work; the executable path does not claim those unfinished isolation layers.

## Intended production identity

The mapping below is a design requirement, not an implemented preview guarantee.

One cell exists for one logged-in host UID. Its stable ID is `u<host-uid>`.
Android UID/GID `0..99999` maps one-to-one onto a dedicated contiguous
subordinate host range of at least 100,000 IDs. Android root is therefore an
unprivileged subordinate host ID, never host root or the interactive host UID.
Ranges must not overlap another allocation or cell.

## Namespaces

The cell receives new user, PID, mount, IPC, UTS, network, and cgroup
namespaces. Android `init` becomes PID 1 only inside the PID namespace. The
supervisor keeps a pidfd and host-side cgroup handle; process names and PID
scans are never lifecycle authority.

## Mount tree

| Cell path | Source/ownership | Required flags |
| --- | --- | --- |
| `/system` | activated package artifact | read-only, nodev |
| `/system/bin/init` | optional package-owned compatibility binary over the activated base | read-only, nosuid, nodev, executable |
| immutable partition file override | optional package-owned compatibility file at its original Android path | read-only, nosuid, nodev, executable mappings allowed |
| `/vendor` | activated package artifact | read-only, nodev |
| `/product` | activated package artifact | read-only, nodev |
| `/data` | direct host `data_dir/data` directory | read-write, nodev, nosuid |
| `/metadata` | direct host `data_dir/metadata` directory | read-write, nodev, nosuid |
| `/data/media/0/<collection>` | explicitly configured host-user directory | read-write idmapped bind, nodev, nosuid, noexec |
| `/proc` | private procfs | nosuid, nodev, noexec, hide host processes |
| `/sys` | minimal read-only projection/synthetic tree | read-only, nosuid, nodev, noexec |
| `/dev` | private tmpfs | nosuid, strict modes |
| `/dev/input` | empty private tmpfs; host owns physical input | read-only, nosuid, nodev, noexec |
| `/dev/pts` | private devpts | nosuid, noexec, newinstance |
| `/dev/binderfs` | private binderfs | three named devices only |

Host hotplug uevents can reach Android even with synthetic sysfs. The sealed
`/dev/input` mount prevents ueventd from recreating physical keyboard/mouse nodes
that would bypass Wayland focus and duplicate forwarded input. The synthetic
Host Keyboard and targeted input protocol do not use evdev nodes.

The Droidloom system-server policy is a paired file override:
`/system/framework/services.jar` supplies the Java `WindowManagerPolicy` adapter,
and `/system/lib64/libinputflinger.so` includes its in-process Rust engine alongside
the existing Droidloom native input integration.
Both replace existing regular partition files through the normal override
mechanism. No new file is created in the immutable system image.
Each base odex records the checksums of the jars loaded before it, so ART would
reject the base's precompiled code for services.jar and every system-server jar
after it. Bundle staging compiles those jars again with a host `dex2oat` built
from the base's own source, against the base boot image and with the context,
compiler filter, profile and app image the base used for each. The result, with
the base's other oat files unchanged, replaces `/system/framework/oat/<isa>`. The package
must contain window-policy ABI 1 metadata in both compiled artifacts; native
architecture must match the cell, and JNI checks the ABI again before WMS starts.
The framework task backend additionally requires task-backend ABI 3 metadata
in services, InputFlinger and Composer. Native input ABI 2
pairs services, InputFlinger and Composer; host channels ABI 1 pairs services,
SystemUI, IME and the presenter. The standalone input support process and its
artifacts are retired. The catalog adapter lives in services.jar; the separate
task launcher, its shared library and application-catalog JAR are also retired.
Task discovery and host focus/resize/close run inside
ActivityTaskManager/WindowManager. A dedicated command subscription reuses the
existing privileged Composer endpoint; object and task-incarnation validation
prevents stale commands from affecting a reused task ID.

The audio client override at `/system/lib64/libaudiobase.so` keeps the minimum
playback queue at two mixer periods when `ro.hardware=droidloom`. PipeWire owns
device buffering; duplicating its latency in each app queue adds avoidable delay.
This changes no native ABI, output-latency reports or presentation timestamps.
Explicitly requested larger app buffers are preserved.

An optional `gapps_dir` in the root-owned cell specification selects a derived
`product`/`system_ext` image pair before Android init starts. `system` and `vendor`
retain their normal sources. The add-on manifest binds all three base partitions
and both derived outputs to exact hashes, Android SDK and native architecture.
Its files and ancestors must be root-owned, without symlinks or group/other write
access. Missing or mismatched selected images fail startup. Omission uses the
base images; installing the optional package does not edit the specification.
Read-only Android image mounts detect raw ext4 or EROFS from the filesystem
magic, including the selected add-on images.

The first GApps activation refuses already initialized Android data. A marker
inside the cell-owned data directory records selected packages, signers and manifest
identity. Changing signers/selection or disabling the add-on requires fresh data
or a suitable backup; images alone cannot roll back Google updates in `/data`.
Manifest changes invalidate only the selected Google applications' parser cache.
The runtime never deletes accounts or installed app updates as a side effect.

The supervisor builds the complete tree before exec. Writable Android state is
bind-mounted from root-private host directories, sharing host capacity without
loop images. A mandatory inherited seccomp filter blocks filesystem-wide
shutdown/freeze/trim, quota operations, ext4 resize/tuning, ordinary superblock
remounts and fsconfig reconfiguration. Bind remounts and per-file operations
remain available. It covers native and compat ABIs and fails startup if it
cannot be installed. The preview still retains capabilities needed by Android
APEX/FUSE setup; this targeted guard is not a complete privileged-container
sandbox. Explicit shared-storage collections remain idmapped host directories.
They map the host UID to MediaProvider's recorded app UID and the host
GID to Android `media_rw`, and are mounted
`nodev,nosuid,noexec`, and placed behind Android's ordinary emulated-storage
service rather than exposed as Android system state.

The host kernel must provide FUSE. The packaged `vold` recursively binds its
MediaProvider lower view so configured collection submounts remain visible;
both volume teardown paths detach the whole lower tree. A non-recursive bind
exposes the underlying empty mountpoint instead of the host collection.
The supervisor reads MediaProvider's UID from the data directory's
`/data/system/packages.list`; it never guesses an app UID. Android writes that
list on its first start, so the collections join the running cell: once the list
exists and Android has made `/data/media/0`, `droidloomd` runs the supervisor's
private `development-share` step. It makes each idmapped mount detached on the
host, enters Android init's mount namespace, checks that `/data/media/0` there
is the data directory's own folder, and attaches the mounts below it. On
initialized storage this happens as Android init starts; on new storage during
its first start. Android makes `/data` a shared mount, so a collection attached
after vold's lower view or an app's namespace exists still reaches them; when
the lower view was already there, `droidloomd` asks MediaProvider to rescan the
collections. The test collection uses setgid directory inheritance so
Android-created files retain the mapped host group. MediaProvider's primary GID
is also mapped to a reserved subordinate host GID, satisfying Linux's
creator-credential checks before setgid inheritance. The temporary mapping
helper exits once the mounts are made; they retain the kernel's mapping
reference.

## Device allowlist

The initial device set is null, zero, full, random, urandom, tty/ptmx/devpts,
the standard FUSE node used by Android's private emulated-storage mount,
ashmem only if an AOSP compatibility audit still requires it, the three private
Binder devices, and exactly one configured DRM render node. No `card*`, KMS,
physical input, framebuffer, camera, media, USB, Bluetooth, modem, block, raw
memory, or host control node is allowed. The synthetic sysfs exposes the
otherwise-empty fusectl mount point but no host FUSE connections. It also
projects a read-only allowlist of battery and charger status attributes from
host power-supply sysfs. This lets Android schedule charging-only maintenance
against the real power source without exposing power-supply controls or device
links.

## Host paths

- package-owned: `/usr/lib/droidloom`, `/usr/bin/droidloom-*`, and
  `/usr/share/droidloom`;
- ephemeral: `/run/droidloom/cells/u<uid>` and the user's mode-0700 runtime
  directory socket endpoint;
- system state: `/var/lib/droidloom/images` and activation metadata;
- per-user Android data: a platform-owned parent containing `data` and
  `metadata` directories, retained on normal package removal and deleted only by
  explicit wipe; the containing host filesystem is never exposed to Android.
- per-user shared storage: an explicit allowlist of host-owned directories,
  each mapped to one top-level collection below `/storage/emulated/0`; an
  omitted list exposes no host directory. With `share_user_folders`, which the
  recipes set, the desktop user's Downloads, Pictures, Documents, Music and
  Videos (as `~/.config/user-dirs.dirs` names them, else those names below the
  home) become Android's Download, Pictures, Documents, Music and Movies. They
  are found at each start; a folder is shared while it is a real directory
  owned by the user and the user's primary group, and gets the setgid bit
  before the cell starts. Android keeps its own folder for the others.
  Host writes to shared folders bypass MediaProvider's FUSE layer, so while
  the cell runs `droidloomd` watches them with inotify and sends each changed
  path, batched after 250 ms of quiet, to the framework's control socket;
  system_server asks MediaProvider to scan it, which indexes a new file and
  drops a removed one.
- per-user application integration: managed `.desktop` files below
  `$XDG_DATA_HOME/applications` with `droidloom-` names, rendered icons below the user's
  hicolor icon tree, and reconciliation state below `$XDG_STATE_HOME/droidloom`.
  Only the unprivileged user service writes these paths.
- per-user window policy: user-selected initial geometry at
  `$XDG_CONFIG_HOME/droidloom/window-policy-v1.json` and automatically
  remembered stable sizes at
  `$XDG_STATE_HOME/droidloom/window-sizes-v1.json`. Only the unprivileged
  presenter and CLI read or write these bounded, versioned documents.

No source-checkout path may appear in a runtime mount or unit.
When selected, `android_init` must be a normalized absolute path to a regular,
executable package-owned file. The supervisor bind-mounts it read-only over the
immutable base's `/system/bin/init`; omitting it retains the base init.
Each `android_file_overrides` entry names a normalized absolute package-owned
regular file or directory and a unique normalized target below `/system`,
`/system_ext`, `/product`, or `/vendor`. Targets must already exist in the
mounted partition as the same kind, and are bind-mounted read-only with
executable mappings allowed.
`/system/bin/init` remains reserved for the separately validated
`android_init` field.
Each `android_runtime_directories` entry names a normalized, package-owned host
directory and a unique, non-overlapping target below the cell-private
`/droidloom` tree. The supervisor creates the target before entering Android
and bind-mounts the directory read-only with device and setuid semantics
disabled. These projections may contain executable compatibility helpers, but
they are not Android-writable storage and cannot target `/system`, `/vendor`,
`/data`, or another Android namespace.

## Private haptics, camera and fingerprint endpoints (DLDEV001)

The user `droidloom-devices.socket` unit owns `%t/droidloom/haptics.sock`,
`%t/droidloom/camera.sock` and `%t/droidloom/fingerprint.sock` under the same
mode-0700 runtime directory. The supervisor may bind only those inodes at
`/dev/socket/droidloom/haptics`, `/dev/socket/droidloom/camera` and
`/dev/socket/droidloom/fingerprint`. One broker, `droidloom-devices`, runs as
the host user and serves all three; it authenticates the kernel-mapped UID of
the vibrator HAL (system, 1000), the camera provider (cameraserver, 1047) and
the authentication service (root, 0) on each.

Records carry `DLDEV001`, a kind and bounded fields. Haptics clients send
patterns of at most 64 steps and 30 seconds, which the broker plays through
hapticd on the system bus; a client that disconnects stops its pattern. Camera
clients list braun's cameras or ask for a braun session, which the broker
passes on as braun's own session descriptor; the HAL then speaks braun's
protocol directly. Fingerprint clients ask for the reader and the user's
enrolled fingers, and start or cancel a verification. Where the compositor
offers `denial_fingerprint_verifier_v1` (Denial), the broker asks it: Denial
runs the reader, lights the panel and marks the sensor, and a pending
verification waits while the session is locked. Elsewhere the broker claims
fprintd's reader for each verification only, listens again after a no-match,
and stops and releases the reader however it ends. Without hapticd, braun or
fprintd the broker reports no vibrator, no cameras and no reader.

The broker only runs the reader: the authentication service counts a match
once the supervisor, as root, has seen fprintd announce it on the system bus,
and passes it through `/dev/socket/droidloom-authentication` (root, 0600).

## Authentication secrets

Android's system user is the host user in a cell, so what makes an Android
unlock or a fingerprint match stays with root:

- At each start the supervisor writes a fresh 32-byte key agreement key at
  `/dev/droidloom-agreement-key` (nobody, 0400). KeyMint and the
  authentication service derive their shared HMAC key from it.
- Android's hidden credential is `credential` in the cell's data directory
  (root, 0600), outside Android's storage. The supervisor moves one an earlier
  release left at `data/system/droidloom/credential` there, without following
  links, and gives it only to system_server (`CREDENTIAL`, framework op 10).
- servicemanager runs as root. In the cell it lets only root register the
  fingerprint, gatekeeper and `ISharedSecret/droidloom` names, only KeyMint's
  user (or root) register KeyMint's, and no process take over a name a live
  service holds.
- The authentication service runs as root and keeps its key and records in
  `/data/vendor/droidloom-authentication/private` (root, 0700).

## Private audio endpoint (DLAUD001)

The user `droidloom-audio.socket` unit owns `%t/droidloom/audio.sock` under a
mode-0700 runtime directory. The supervisor may bind only that socket inode at
`/dev/socket/droidloom/audio`. The broker runs as the host user, independently
of rendering; it authenticates audioserver's kernel-mapped UID (1041 in the
preview). A broker restart preserves the listener inode and replaces all stream
generations. Stopping Droidloom stops the broker and listener through `PartOf`.

Each connection negotiates PCM16 little-endian, 48 kHz, one or two channels,
and four fixed-size sample allocations. Control records are exactly 64 bytes,
carry `DLAUD001`, a strictly increasing nonzero request ID, a random generation
and five bounded fields. The host alone sends sample FDs; clients cannot register
memory or supply graph metadata. A single acquired credit governs slot, byte
offset and frame count until submit. Short capture reads retain the remainder.
Malformed or stale commands and backend failure close that generation.
The PipeWire callback performs no socket I/O, allocation or PCM copy.

Capture requires an explicit host-user grant in addition to Android app
permissions. Revocation by removing the grant and restarting the broker stops
new sampling and closes old connections. It cannot erase delivered samples.
Dedicated memfds have immutable sizes; capture gets read-only descriptors.
Neither ALSA devices nor the unrestricted PipeWire socket/session bus enters
Android. Missing endpoint or denied capture uses timed silence; failure of an
already live transport is reported to Android rather than fabricating progress.
Production mapped identity, route/capability notifications and per-app host
attribution remain unfinished. See [audio implementation](../audio-integration-proposal.md)
and [threat model v2](../threat-model-v2.md).

## Network

Each cell owns a network namespace and one veth peer. Host policy assigns its
address, forwarding, DNS broker, anti-spoofing, and NAT/firewall objects under
a stable cell tag. Android `netd` may operate only inside the cell namespace.
Teardown removes every tagged host object even if Android is unresponsive.

The executable preview starts authenticated `adbd` on the cell's private
`10.177.0.2:5555` endpoint. Its host input rule permits only established IPv4
TCP replies from that endpoint before dropping other cell-originated host
traffic; inbound forwarding to the cell remains blocked. No host LAN listener
or forwarding exception is created for ADB.

The lifecycle owner runs `/usr/bin/adb` with the configured host user's UID,
primary GID, that user's supplementary groups and passwd home directory. It
never retains inherited daemon/root groups or starts a root ADB server. It starts
that user's default local ADB server through a separate transient user service,
so the server never belongs to the Droidloom lifecycle daemon's cgroup. It reuses
an existing server without replacing it, derives the public key without reading
user-selected paths as root, and projects the public key read-only at
`/adb_keys`. The private key stays in the user's `~/.android/adbkey`.
Runtime-generated read-only product properties enforce `ro.adb.secure=1` and
the `droidloom` device identity. Boot parameters supply
`ro.serialno=droidloom`. The generated init policy starts the existing AOSP
adbd service independently of USB configuration and retains the previously
projected classpath and CPU policy. Persistent Android data is preserved.

The product-property projection also clears `traced.relay_producer_port`: the
cell uses Android’s local Perfetto producer and consumer Unix sockets, without
an incoming VM relay listener. This avoids a `traced` restart loop on hosts
without AF_VSOCK while preserving normal tracing and its enable setting.

It names Android after the host as well. `ro.product.model` is the host's
device-tree model, or `Droidloom` without one. `ro.droidloom.device_name` is
the host's pretty hostname from `/etc/machine-info`, else its hostname, and
system_server writes it to `Settings.Global.DEVICE_NAME` at every start.

A connection worker uses bounded requests to the standard ADB host protocol
to retry registration with the user's server, including after server restarts.
Host requests cannot implicitly fork a server into the lifecycle daemon's
cgroup or replace a server with a different client version.
Teardown joins the worker and disconnects
only this transport before removing its route and generated configuration.
Missing host ADB or an unusable user home disables automatic ADB without
preventing Android boot. TCP transport serials remain address-based; the
stable ADB selector is `device:droidloom`.

## Construction transaction

Construction is fail-closed and rolls back in reverse ownership order:
reserve identity/cgroup, create namespaces, populate UID/GID maps, assemble
mounts and devices, create Binder contexts, create network policy, apply
resource limits, start init, then publish the per-user lifecycle handle. A
handle is published only after every prerequisite succeeds.

Teardown first refuses new launches, terminates the pidfd-owned namespace,
waits with a bound, kills the remaining cgroup if necessary, removes network
policy/veth, unmounts the private tree, removes runtime paths, and finally
releases the cgroup. Repeating teardown must be safe.

## Lifecycle readiness and progress

`running` means the namespace owner is alive. Android readiness additionally
requires `sys.boot_completed=1`, `sys.droidloom.framework_ready=1`, the native
framework RPC endpoint, and the task-control socket. Public `droidloomctl start`
and `restart` wait for readiness; `--no-wait` and internal `--cell` orchestration
retain service/cell-only startup. `droidloomctl wait` observes readiness without
starting or restarting the cell. `status` reports the current observed stage.
The framework sets its readiness property only after native listener setup and
authenticated descriptor delegation are available; policy initialization and
zygote restart/stop clear it.

Legacy lifecycle requests remain bare JSON objects with a single JSON response.
Interactive clients opt into progress with `{"request": <control request>}`.
The daemon sends newline-delimited `{"progress": "..."}` frames followed by the
normal response object. APK file descriptors still accompany the first request
byte and are accepted only for installation. UID authorization is unchanged;
boot observations are emitted only after authorizing the cell owner.

`stats` answers from files only, without entering the cell: whether the
framework and task-control sockets exist, time since start, the cell control
group's memory without its file cache (`memory.current` less `file`, plus
`shmem`), its swap, its `cpu.stat` usage, its processes and threads
(droidloomd left out) and those under Android's `apps` group, the time from
the cell's start until both sockets existed, Android's release version from
`system/build.prop`, and the model and device name from `product/etc/build.prop`.
A stopped cell answers with why it last ended, when it did not end on request.
Settings asks for it every few seconds.
`display` answers the built-in display's density (`dpi`) and the one it started
with (`initial_dpi`) from the framework RPC endpoint; `set_dpi` changes it.
`application_details` and `application_action` go to the framework RPC
endpoint: a package's version, install dates and storage, and stop, uninstall
(not for system packages), clear cache, clear data, or opening Android's App
info for it in a window.

Boot waiting is bounded at 120 seconds, with short timeouts on readiness probes.
Native framework replies have a 120-second deadline. Package-manager commands
use 110 seconds. Timed-out subprocess groups receive
a forced kill after a short grace period. Clients enforce a 250-second wall-clock
request deadline, including queue time, even if progress or partial bytes keep
arriving. Socket writes are bounded, and a full accept queue fails promptly.
Client disconnection does not tear down Android. A timed-out client's operation
may still complete, so its error advises checking the result before retrying.

The lifecycle daemon connects directly through the owned Android init's root
to private system-owned framework sockets and authenticates UID 1000. The
framework accepts root peers only. RPC uses a 12-byte DLFC/ABI/opcode/length
header, with 16 KiB requests and 8 MiB replies. Catalog event descriptors carry
big-endian u64 generations; connection always emits an initial generation.
Only the authorized cell owner receives a descriptor. Healthy subscriptions
block on events; reconnects resnapshot. Framework ABI 1 binds services,
InputFlinger, the daemon and catalog. Notification ABI 3 binds SystemUI and
presenter and requires a snapshot before accepting versioned deltas.
