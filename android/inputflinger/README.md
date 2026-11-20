# Task-scoped Android input

`0001-application-token-targeted-injection.patch` extends InputDispatcher's
existing targeted-injection state with an optional input-application token.
For pointer DOWN it runs the normal touch hit test while excluding windows from
other application tokens; the resulting touch state remains latched for the
gesture. Keys select the frontmost eligible window belonging to that token
instead of consulting Android's global focused task. Targeted streams exclude
spies, outside-touch listeners, and wallpaper duplication so input cannot leak
to a different Android task.

This is downstream of Denial's ordinary window hit testing. The patch contains
no Denial policy and never changes Android task focus or stacking.

`0002-droidloom-targeted-injection-binder.patch` supplies the original root/system-only
native injection endpoint. It preserves the existing
`InputDispatcherInterface` ABI, parses the standard Android event parcel, and
passes the resolved application token into the concrete dispatcher path.

`0003-droidloom-window-policy.patch` links the Rust policy/task backend into
`libinputflinger`. `0004-droidloom-native-input.patch` adds an in-process Rust
input receiver and a small C++ event-construction adapter. The existing window
policy starts InputManager's Rust-owned listener through private transaction
`0x00444c03`, native input ABI 2. No init FD or support process is required. WMS
publishes bounds, application tokens and task incarnations through local native
Binder transactions `0x00444c04`/`0x00444c05`, outside its global lock.
Only system_server can configure the listener or publish routes; socket clients are
restricted to Android root/system. No public AIDL or stock transaction changes.

InputManager's class layout remains unchanged: the prebuilt `libandroid_servers`
allocates it with the upstream `sizeof`. Native ownership is kept in a private
registry used only for setup and route-publication calls, outside the input-event
path. The builder permits only its existing Binder virtual override in
`InputManager.h` while retaining that prebuilt JNI consumer. Rust owns listener
creation, permissions, shutdown and stale-socket cleanup in a system-private
directory. A restarted system_server starts its own listener again.

Keys are injected as the `Droidloom Host Keyboard` device that
`0002-droidloom-targeted-injection-binder.patch` registers, so apps read their
characters from the layout Android selected for it. The device has no
`VIRTUAL` class, which would keep InputReader from laying that layout over
its key map. Its character map also
remaps the scan code where that layout moves keys; otherwise the receiver's
US-position key code stands.

The receiver validates input ABI 7 and rejects stale incarnations. It sleeps in
socket polling at idle, cancels keys/gestures on disconnect or route retirement,
and retains Binder tokens through cancellation dispatch. Native input ABI 2
metadata prevents mixed framework/Composer builds. The Wayland presenter writes
directly to the native receiver. Composer delegates the connected writer FD at
startup/recovery and publishes framework task incarnations; it keeps no writer
duplicate and forwards no touch/key records. The existing framework policy
handler applies the low-frequency PowerManager callback. System_server delegates
authenticated connected descriptors to the existing IME/SystemUI adapters; their
payloads travel directly to the host. No standalone input bridge remains.
