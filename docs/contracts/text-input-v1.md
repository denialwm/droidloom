# Host text input

Droidloom uses the compositor's ordinary `zwp_text_input_v3` interface on the
same seat and surfaces as its Android task windows. Denial needs no changes.

Android's headless `com.android.droidloom.ime/.HostInputMethod` observes
`onStartInput`, `onFinishInput`, and explicit input-panel requests. It never
draws an Android keyboard or requests fullscreen IME mode. Android init installs
the packaged APK when its hash changes, enables/selects the method, and enables
IME requests with a hardware keyboard once after boot. No per-frame settings
reads or editor polling are used.

Panel dismissal is separate from an editable input connection. The headless
method handles `hideWindow` directly: Android does not call `onWindowHidden`
when no Android keyboard window was visible. A subsequent `restartInput` must
preserve dismissal even if `EditorInfo.inputType` remains editable. An explicit
show request or a genuinely new editor can re-enable it. `EditorVisibilityTest`
covers hide-then-restart behavior on the host JVM.

IMMS annotates EditorInfo with the task ID of its validated editor window;
WMS checks that window's owner UID and resolves the exact task without enumeration.
The IME authenticates system_server's setup socket, and system_server checks the
IME's installed PackageManager UID before delegating a connected host descriptor.
Bounded JSON records then travel directly between the IME and presenter.
The host accepts Android system UID 1000 peers. The host socket's parent is 0700;
the supervisor exposes it only below a system-owned 0700 guest directory, allowing
host owners other than UID 1000 without granting ordinary Android apps path access.
Neither connection logs editor contents. Editor state contains only session,
task, input type, enablement, and a monotonically increasing show-request counter;
surrounding text is not exported. Show requests do not create a new editor
session: repeated `showSoftInput` refreshes content state with a Wayland commit,
not another `enable`. This preserves compositor touch authorization and permits
reopening a user-dismissed panel without invalidating the input connection.

The presenter enables text input only when Android's task, the host keyboard
focus, and Wayland's text-input enter surface agree. It forwards committed
transactions only for the current Wayland commit serial and Android editor
session. Focus loss, connection loss, and task destruction disable the session;
text-bridge failures do not terminate graphics or ordinary key injection.

Committed text and preedit return through Android's `InputConnection`.
UTF-8 deletion lengths are converted to UTF-16 without splitting code points.
Password fields advertise sensitive/hidden text hints. This first bridge does
not export surrounding text, cursor rectangles, selection, or editor-action
metadata; full prediction/context and precise IME candidate placement remain
future work. Physical key delivery remains on the existing routed-key path.

## Host keyboard layout

The same connection carries the host keyboard's layout to the input method as
`{"keyboard_layout":{"language":TAG,"type":TYPE}}`, once when the IME connects
and whenever the host keymap or its active group changes. The presenter reads
each group's layout from the keymap's `xkb_symbols` name, the RMLVO symbols the
compositor compiled it from, and names the Android layout of the same keys as
a BCP 47 language tag and one of Android's layout types (`qwerty`, `azerty`,
`qwertz`, `dvorak`, `colemak`, `workman`, `turkish_q`, `turkish_f`, `extended`,
`undefined`). A layout Android has none for types as US English.

The IME replaces its additional subtype with one whose physical keyboard hint
is that tag and type, and switches to it. Android's `KeyboardLayoutManager`
picks the host keyboard's layout from the current subtype's hint, as it does
for any input method, and key events are injected as that keyboard: apps read
characters from that layout, and its key map moves keys where the layout does
(AZERTY, QWERTZ). `0010-droidloom-host-keyboard-notification.patch` keeps the
keyboard out of Android's keyboard layout notification.

The [Rust package builder](../../packaging/arch/README.md) builds the IME,
framework services, IME, SystemUI, task launcher and Composer endpoint together. Task registration
is idempotent for restored and newly created activities.

## Activity routing

ActivityTaskManager publishes the real task selected by ActivityStarter and its
launch completion callbacks. Launcher aliases and no-display trampoline
successors do not require package-name matching or task-dump discovery.
Android-originated launches are published from in-process lifecycle events,
without replaying intents, preserving deep-link data, URI grants and activity
results. Reused host launches request activation through the optional
`TASK_ACTIVATION` capability; the host retains authority over window focus.
See the [framework boundary](../../android/framework/README.md) for eligibility
and runtime acceptance.

Presenter activation logs include `touch_age_ms`; IME logs distinguish
start/restart, finish, and show callbacks without logging editor text.

Denial uses focus-scoped touch authorization for keyboard activation and revokes
it on shell touches. Activation is counted when an editor consumes authorization,
not on raw protocol-client touches.

## Host keyboard dismissal

The optional `denial_text_input_panel_manager_v1` Wayland extension associates
dismissal feedback with our ordinary text-input-v3 object. Denial sends only a
completed user dismissal, identified by the text input's commit serial. The
presenter ignores stale and duplicate feedback and relays a bounded JSON record
containing `dismiss: true`, `session`, and `show_request` to the headless IME.
The IME checks both identities, records hidden state, finishes composition, and
calls Android's `requestHideSelf(0)`. That informs Android and the app instead of
faking Back/Done or forcing view focus loss. A later tap on the same focused
editor can request the keyboard again; actual view-focus behavior belongs to
the application.

The protocol XML is vendored in `graphics/droidloom-wayland/protocol/` and must
match Denial's copy in `compositor/protocol/`. No Denial checkout is needed to
build the presenter. Absence of the extension leaves ordinary text input usable
but cannot report manual host dismissal to Android. The extension has no Android-specific wire semantics.

Tests cover dismissal serials, duplicate events, stale Android show requests,
hide/restart preservation, and reopening the same editor.
