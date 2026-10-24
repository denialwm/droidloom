# Framework integration boundary

`droidloom-home` is the cell's idle HOME destination: one SDK-only Java activity
with an opaque static background and hardware acceleration disabled. It has no
launcher entry, permissions, services, timers, or wake locks. Its task stays alive
under Android's ordinary lifecycle and is never registered for native export.
The activity is direct-boot aware, so once installed it is HOME from the start of
boot: Android draws it to end the boot animation and never starts Settings for
its `FallbackHome`. Before the first install, `FallbackHome` finishes into it.

The independent `droidloom-home-setup` init oneshot installs the signed APK and
selects it as user 0's default HOME after boot. The APK and script use the existing
`/droidloom/ime` support directory, shared with keyboard provisioning. Updates
replace the APK only when its digest changes and reassert HOME resolution on every
boot. After verifying HOME resolves to DroidloomHome, setup disables stock
`com.android.launcher3` for user 0 and force-stops any residual process.
Its base APK and data are retained; no Settings component is disabled. To undo
that package policy, use Android's `cmd package enable --user 0 com.android.launcher3`
and change provisioning before the next boot.
The existing composer init file imports `/droidloom/ime/droidloom-home.rc`, so
provisioning does not require adding a file to the immutable vendor image.

`droidloom-systemui` replaces the stock SystemUI APK with a platform-signed,
persistent service built from scratch. It registers the pinned `IStatusBar`
interface and implements the minimum keyguard boot callbacks, with no windows,
render loop, notification renderer, or resource bundle. Generated Binder
adapters preserve the platform transaction layout and reject callers outside
root/system. Regenerate them with `python3 tools/droidloom-systemui-stubs` when
advancing the pinned framework interfaces; `--check` verifies the checked-in
adapters. The updater also checks their source hashes against the build inputs.

Unsupported biometric prompts cancel without credential attestation, quick
settings tile requests return a negative result, and text toasts complete their
hidden callback and release the framework token. Screenshot requests complete
without an image, media projection requests cancel, and shell/proto output file
descriptors close. A cell with an Android credential configured is reported as
locked; this initial service has no Android credential-entry UI. Desktop locking
and notification presentation belong to the host.

The same process hosts event-driven clipboard and notification adapters. Private
UID-authenticated descriptor handoffs connect them directly to the
Rust Wayland presenter. The notification listener sends bounded active snapshots
to `org.freedesktop.Notifications` on the desktop session bus. Android updates
replace desktop IDs; removals close them. Default clicks and ordinary action
buttons invoke version-checked Android PendingIntents. Explicit desktop dismissal
cancels clearable Android notifications; desktop expiry does not.
Ongoing Android notifications also use the desktop's normal popup timeout; their
ongoing flag must not pin a popup indefinitely. Android LOW/MIN channel importance
maps to low desktop urgency; DEFAULT/HIGH maps to normal urgency. Channel
importance alone never requests a non-expiring critical desktop alert. Group summaries
are skipped to avoid duplicating their children. Inline replies and custom Android
notification layouts have no mapping in this initial standard D-Bus adapter.
Retry timers run only while a transport is disconnected.

`DroidloomNotificationProbe` and the presenter's `notification-test-server`
example support headless integration checks on a private D-Bus. The probe's
actions use broadcasts and its channel has no sound or vibration. Neither test
component belongs in production packaging. Visible desktop tests remain subject
to the workspace's explicit-trigger rules.

The APK overlays `/system_ext/priv-app/SystemUI/SystemUI.apk`. Because Android's
package parser cache uses the immutable containing directory's timestamp, the
supervisor invalidates only SystemUI's parser cache before Android starts. This
also covers rollback to the stock APK. The `DroidloomSystemUIProbe` build target
checks callback completion and Binder caller guards under Android shell UID
2000, without creating windows, prompts, notifications, or input events.

Only narrowly reviewed Android framework patches belong here. Package/task,
notification, clipboard, inset, lifecycle, and power-policy integration should
prefer a Droidloom system service over broad framework changes.

`0007-droidloom-detected-vulkan-capabilities.patch` fills Vulkan package features
during SystemConfig initialization, gated to the Droidloom product and system
process. The native probe uses Android's loader and checks hardware device type,
graphics queues, Android hardware levels and version-specific buffer/sync interop.
The detected version comes from a device supporting the detected level; separate
GPUs cannot contribute unrelated maxima. Missing loaders, failed queries and
CPU-only implementations leave the features absent. Explicit unavailable-feature
rules still apply after detection. No dEQP, compute or Vulkan profile certification
is inferred. The Java adapter and native probe ship as a matched services/InputFlinger
pair, checked by the bundle's Vulkan capability ABI marker.

The same probe sets `ro.hwui.use_vulkan=true` when HWUI's Vulkan pipeline can
run: the first physical device (the one HWUI takes) has Vulkan 1.1 with that
interop and two queues in its first graphics family. Apps read it when they
start; `debug.hwui.renderer` still overrides it and a product value is kept.

Run the driver-independent native fixtures and Java failure-boundary checks with
`cargo test --locked -j 1 -p droidloom-update --test vulkan_capabilities -- --ignored`.
These checks require the prepared AOSP Vulkan headers and a host JDK/C++ compiler.

`droidloom-window-policy` implements the Droidloom replacement for
PhoneWindowManager. `0004-droidloom-window-policy.patch` selects it at the existing
SystemServer policy injection point only for the Droidloom task-window product.
It implements `WindowManagerPolicy` directly. The Rust crate
`droidloom-android-policy` owns screen generations/readiness, key routing and
window authorization. Its Java adapter collects Android permission facts and
performs framework effects, retaining `KeyguardServiceDelegate`, AppOps and WMS
window-token validation. It never instantiates PhoneWindowManager. The Rust
engine links into the already rebuilt `libinputflinger.so`; ship that library and `services.jar`
together. The JNI ABI is checked at startup and in bundle verification.

The host owns phone shell keys, bars and physical display power. Android owns
Back/IME dispatch, media-session fallback, sleep tokens, credentials and trust.
Screen waits reject stale completion generations; an Android credential remains
locked when the keyguard service cannot bind. No credential-entry UI is added.
Android's phone-specific recents/global-actions/haptic policy is absent; the
host supplies those features. Android's credential lock-while-on policy remains
active for apps requesting it, using Android user activity and timeout settings. The adapter
currently handles the default display power group. Android keyguard boot and
screen-drawn timeouts remain bounded, and verify-unlock requests fail on timeout.
The presenter retains initial host display power while the native input socket is
still starting. A temporary retry worker delivers the latest state, then exits;
startup ordering cannot abort presentation and there are no idle retry wakeups.
Task publication and direct native input are described below. See
[native application integration](../../docs/native-app-integration.md).

Build the paired framework artifacts without installing or restarting anything:

```console
cargo run --locked -j 1 -p droidloom-update -- --jobs 1 framework-build --work .work/arch/build
```

Add `--product droidloom_arm64` for ARM64. Pure policy tests run with
`cargo test --locked -j 1 -p droidloom-android-policy`. The JNI probe executes the
actual native engine under a host JVM without Android, windows or input events:

```console
cargo build --locked -j 1 -p droidloom-android-policy
mkdir -p .work/window-policy-tests
ln -sfn ../../target/debug/libdroidloom_android_policy.so .work/window-policy-tests/libinputflinger.so
javac -d .work/window-policy-tests android/framework/droidloom-window-policy/java/com/android/server/policy/DroidloomPolicyNative.java android/framework/droidloom-window-policy/tests/com/android/server/policy/NativePolicyTest.java
java -Xcheck:jni -Djava.library.path=.work/window-policy-tests -cp .work/window-policy-tests com.android.server.policy.NativePolicyTest
```

The native link hook is `android/inputflinger/0003-droidloom-window-policy.patch`.
The symlink gives the host probe the same JNI library name as Android. It stays
under `.work`; it is never packaged as Android's `libinputflinger.so`.
Before shipping, an authorized runtime check must confirm the policy line in
`dumpsys window policy`, boot callbacks, display-off/on, Back/IME behavior,
permission/overlay handling and locked behavior with a configured credential.

`DroidloomTaskBackend` is the framework-owned task publication adapter, built
into `services.jar`. ActivityTaskManager lifecycle callbacks schedule a coalesced
worker that copies attached leaf-task identities under WMS's global lock. The
Rust backend (`droidloom-window-policy/src/tasks.rs`) registers them with the
existing root/system Composer socket after releasing the lock. Socket operations
have bounded timeouts; a stalled peer does not hold the window-management lock.
No external TaskStackListener, stack-list parsing or discovery subprocess remains.

Host launches use a root-authenticated native RPC in the existing system_server.
Rust owns the bounded wire codec and listeners; Java calls Android's existing
setup, density and ActivityStarter APIs. The task launcher executable and its
`cmd` children are retired. The diagnostic `cmd activity_task droidloom-launch`
remains restricted to root, system or shell. Both paths support user 0/display 0
and use ActivityStarter's real
launch completion record rather than matching package names from a task dump.
Aliases, reused tasks and no-display trampoline successors follow Android's
selected activity. Android-originated intents retain their extras, URI grants,
permissions and activity-result relationships.
An explicit launch resolution stops that package before starting it with launch
bounds, preserving the previous `-S` behavior; ordinary reopens reuse tasks.

Only attached standard application tasks on user 0/display 0 acquire host
windows. HOME, recents, organizer containers and non-visible new tasks are
excluded. Published background tasks retain their windows until detached. The
base activity supplies task ownership even when another package owns its top
permission or authentication activity. Incarnations protect against reused task
IDs; Composer ignores retirement from a superseded SurfaceFlinger channel.

Task-control protocol 3.2 retains separate activation intent, so reconnecting
background tasks cannot request focus. Registrations are idempotent; explicit
reopens and foreground changes request
host activation. The host compositor retains focus authority. Reconnects
revalidate cached bindings; the five-second connection-health check does not
enumerate Android tasks while connected and idle. Failed publication retries
stop at the first transport failure in each pass.

Host focus, resize and close use a dedicated command subscription on the same
root/system-only task-control endpoint. The Composer event pump only queues
commands; socket waits run on its subscription thread. Each command carries the
native object and framework incarnation captured at publication. Rust validates
both against its current registration. A separate system-server receiver checks the actual attached task,
incarnation, user and display under WMS's lock, then calls ATMS's normal focus,
resize/transition and removal paths. Socket reads/acknowledgements never hold
that lock. This control path makes no task-list query or reflective call.

The bounded command queue coalesces resize and focus updates, preserves the
existing 80 ms resize debounce and retains each command until acknowledged.
Close invalidates its window's pending resize/focus operations. Reconnects can
retry an unacknowledged command, while old object/task incarnations are ignored.
Idle receivers sleep on events; host-originated focus does not echo activation.

Rebuild all affected Android peers together:

```console
cargo run --locked -j 1 -p droidloom-update -- --jobs 8 framework-build --tasks --work .work/arch/build --product droidloom_x86_64
cargo test --locked -j 1 -p droidloom-android-policy -p droidloom-supervisor -p droidloom-task-control -p droidloom-applications
```

The focused build also compiles and stages their Droidloom shared-library
dependencies, so a new executable cannot retain an old task-control codec.
When testing over an older installation, also build and activate the matching
`droidloom-wayland` host presenter. It must advertise the private protocol's
required capabilities, including display power; a stale presenter can reject
the rebuilt Composer before Android boots. Restart Droidloom after selecting
the matching components.
Bundle checks require `DROIDLOOM_TASK_BACKEND_ABI=2;` in services, InputFlinger,
the launch client and Composer. Runtime acceptance must cover cold/reused
launches, aliases/trampolines, permission/authentication handoffs, Play Store
Open, deep links, shares, task removal/reuse and Composer reconnects. Unit and
build checks do not establish those visible workflows.

For manual desktop acceptance after activating a matching build:

1. Open two Android apps. Alternate focus with a native Linux app; verify keys
   and IME text reach the selected Android window without focus bouncing.
2. Resize each Android window repeatedly and release the drag at several sizes.
   Verify content reflows to the final size without changing Android density.
3. Close a window with the host titlebar/shortcut, then reopen it. Repeat with
   multiple windows from the same app and check the intended task is removed.
4. Check launcher-created and Android-originated windows (for example Play
   Store Open or a deep link) with the same focus/resize/close operations.
5. During an authorized Droidloom-only restart, verify the command receiver
   reconnects and no stale close/resize affects a newly opened task.

Routed input is received inside `libinputflinger`. Rust owns its listening socket
below `/dev/socket/droidloom-framework`, a system-owned private directory. The
existing policy's systemReady hook starts it without a support process or FD relay.
The Rust receiver validates fixed 48-byte input ABI 7 records, including the
framework task incarnation, and tracks gestures, repeats and modifiers. A small
C++ adapter constructs native Android events and calls the existing targeted
InputDispatcher entry point without the support bridge's per-event Java objects
or Binder parcels.
WMS publishes task bounds and input-application tokens on its task worker,
outside the global lock. Bounds are updated on changes; pointer motion performs
no task-bounds Binder query or SurfaceFlinger token lookup. Retirement, token
replacement and disconnects cancel active input. The presenter writes directly
to InputFlinger over a dedicated nonblocking socket. Composer only delegates a
connected writer FD on startup/recovery and publishes task-route metadata;
it forwards no touch/key events. Native input ABI 2
metadata pairs services.jar, InputFlinger and Composer, and bundle
verification also checks InputFlinger's input wire version against Composer.
Boot `framework.jar`, public AIDL and stock Binder transaction numbers remain
unchanged. Focus, resize and close stay in ATMS/WMS.

The Wayland presenter also subscribes to the compositor's standard
`zwlr_output_power_management_v1` state. Once every physical output has reported
an initial state, it sends the aggregate state over the direct native input
connection. InputFlinger sends an infrequent authenticated Binder callback
to the existing framework policy handler, which mirrors transitions into Android's
PowerManager with `goToSleep` and `wakeUp`; repeated states are no-ops. This
keeps Android lifecycle, idle policy, clocks, and regulator votes aligned with
the real display without polling, timers, or a device-specific power daemon.
The presenter, Composer service, protocol libraries and framework services must be
updated together when this record changes.

System_server authenticates the installed IME/SystemUI UIDs, opens the scoped host
channel, checks its owner credentials, and delegates the connected descriptor.
Only connection setup passes through this hook; IME/clipboard/notification bytes
flow directly to their existing adapters. IMMS attaches the exact editor task ID
to EditorInfo, replacing package-name/task-list discovery. Channel ABI 1 checks
services, SystemUI, IME and the presenter together. The host parent and guest
mount directory remain private; ordinary Android apps cannot open host channels.
The standalone root app_process, its init service and relay workers are removed.

The application catalog adapter now lives in services.jar and resolves launcher
activities, localized labels and adaptive icons through the existing package
manager and resource stack. It no longer launches app_process or uses a dedicated
catalog JAR. A root-only catalog subscription emits a generation at connection
and on package/user/locale changes. The lifecycle daemon delegates its connected
descriptor to the authorized user; it does not relay event bytes. The host
catalog blocks while idle and fetches only missing/changed icons in batches of
up to 32. Failed requests retry with backoff; reconnects take a full snapshot.
Linux never parses APK manifests or resources.

Framework control ABI 1 pairs services, InputFlinger, the lifecycle daemon and
host catalog. Notification ABI 3 pairs SystemUI with the presenter: a full
snapshot establishes each connection, followed by changed posts and versioned
removals. Unchanged notifications and their icons are not encoded again. Each
post carries Android's heads-up decision, group key and summary flag, `when`,
progress, launcher activity, large icon and last MessagingStyle messages.
Framework readiness waits for both WMS policy and the ATMS backend, in either
callback order. The startup gate has a host Java regression test:

```console
javac -d .work/framework-startup-tests android/framework/droidloom-window-policy/java/com/android/server/wm/DroidloomFrameworkStartup.java android/framework/droidloom-window-policy/tests/com/android/server/wm/FrameworkStartupTest.java
java -cp .work/framework-startup-tests com.android.server.wm.FrameworkStartupTest
```

The local x86_64 build passed boot/readiness, mounted/running byte comparisons,
native RPC validation, direct catalog descriptor ownership, a 26-icon batch and
subscription reconnection checks. The host catalog blocks on its stream while
idle. Notification deltas passed isolated desktop D-Bus tests. Manual launch,
resolution/density, package/locale changes and notification action acceptance
remain for the user; these changes have not been validated on ARM64 and their
end-to-end latency and net memory gains have not been measured.

The bridge-free x86_64 runtime has passed boot/readiness checks, mounted/running
artifact comparisons, native input ownership checks, direct SystemUI channel
checks and one-shot catalog access. The nonvisual `ChannelsProbe`, run as Android
shell UID 2000, verifies that unauthorized clients cannot obtain descriptors or
open the private host paths, and repeated denials do not leak descriptors.
Manual editor/IME behavior, clipboard and notification actions, recovery and
host display cycling still need acceptance on each target. These checks establish
the removed process and relay hops, without claiming measured latency or net RSS
savings.

`0001-droidloom-bottom-navigation-insets.patch` is the narrow system-server
exception for host system bars. Android's freeform policy strips system-bar
insets, so the patch supplies visible 33.6 dp top status-bar and 16.8 dp bottom navigation
sources, plus bottom mandatory gestures, after that policy and to activity
WindowMetrics. It is enabled
only by Droidloom's existing read-only task-window product marker. See
[`navigation-insets.md`](../../docs/contracts/navigation-insets.md) for the API,
explicit services build, packaging and validation contract.

Android auto-power modes are enabled by the vendor framework overlay. The cell
does not require a motion sensor to enter Doze: host display state drives Android
interactivity, while Linux owns whole-host suspend and notification recovery
windows. Android Doze remains an app scheduling/network policy, not the host
suspend controller. Verify push delivery across host wake windows when changing it.
