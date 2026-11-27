# Host status bar and bottom navigation indicator

Android applications normally discover system navigation through
[`WindowInsets`](https://developer.android.com/reference/android/view/WindowInsets):

```java
WindowInsets insets = view.getRootWindowInsets(); // Once the view is attached.
boolean visible = insets.isVisible(WindowInsets.Type.navigationBars());
int bottom = insets.getInsets(WindowInsets.Type.navigationBars()).bottom;
boolean statusVisible = insets.isVisible(WindowInsets.Type.statusBars());
int top = insets.getInsets(WindowInsets.Type.statusBars()).top;
```

`getInsetsIgnoringVisibility(navigationBars())` describes the stable reserved
area. `mandatorySystemGestures()` and `systemGestures()` describe the bottom
gesture area. Android's resource values alone do not create an inset source,
and are not a substitute for these APIs.

Droidloom launches ordinary applications in Android freeform tasks. The pinned
Android 17 `InsetsPolicy.enforceInsetsPolicyForTarget()` removes navigation
sources from floating windows. Droidloom's existing `DenialMessage::Insets`
transport is also not applied to Android. Setting a navigation resource or
adding a task-local source alone therefore cannot fulfil this contract.

`android/framework/0001-droidloom-bottom-navigation-insets.patch` makes Android
report a visible **33.6 dp top status bar and 16.8 dp bottom navigation indicator**
to app windows: 84 px at the top and 42 px at the bottom at 400 dpi.
It uses
the existing `ro.vendor.droidloom.surfaceflinger_tasks=true` product marker,
already set by both Droidloom products, to preserve upstream behaviour outside
Droidloom. Density comes from each recipient's configuration, with mdpi as the
fallback for an undefined density.

The patch inserts status, navigation and mandatory-gesture sources after freeform,
transient-bar and keyboard policy, and also into activity `WindowMetrics`.
Android 17's attached-inset geometry follows the recipient's bounds, including
current versus maximum metrics, resizing and orientation changes. The existing
size-compatibility scaling still runs afterward. Android maps mandatory gestures
into system gestures as well. These sources have no surface or control leash,
so an immersive app cannot hide the host bars through InsetsController.
Its request still reaches the host: a task whose main window asks to hide the
status bar is shown with `xdg_toplevel` fullscreen
([native app integration](../native-app-integration.md)), and the compositor
may hide its own bar over it.
Android's own keyboard and cutout sources retain their own policy. The host's
on-screen keyboard is a separate IME source with its own id, added only while
the host reports a height for the window's task, and then in place of
Android's own, hidden one
([architecture](../architecture.md#on-screen-keyboard)); `WindowMetrics` never
include it. No tappable
inset or Android system-bar window is created. Drawing the bars and
handling gestures remain host responsibilities.

The app may consume or ignore its delivered insets. A dialog that does not
overlap a host edge correctly receives zero overlap for that edge, even though
bar visibility remains true. The heights are a fixed Droidloom policy;
the host-provided `DenialMessage::Insets` values remain separate future work.

Inset reporting and task surface geometry must stay consistent. The input bridge
uses Android's privileged `ActivityTaskManager.resizeTask` API after checking
the task's display. With Shell transitions enabled, that API collects the task
into a synchronized resize transition so its configuration and surface crop
update together. The resize path supports unorganized tasks in the minimal
SystemUI configuration as well as organized tasks.

## Build and packaging

The [Rust package build](../../packaging/arch/README.md) requests framework
services together with all other modified Android components. It reuses pinned
CI base partitions and projects the rebuilt services JAR into the runtime.

For specialist component work, the older vendor/HAL smoke build reuses CI system
partitions too, but rebuilding framework services is an explicit operation:

```sh
DROIDLOOM_SOONG_SERVICES_ONLY=1 tools/droidloom-soong-core-smoke
# For x86_64:
DROIDLOOM_ANDROID_PRODUCT=droidloom_x86_64 DROIDLOOM_SOONG_SERVICES_ONLY=1 \
    tools/droidloom-soong-core-smoke
```

This verifies the source lock, applies the framework patch only for the build,
builds the aligned, dex-containing `services.jar`, stages it under
`out/target/product/<product>/system/framework/`, writes a digest/provenance
sidecar and restores the source checkout on exit. It does not fetch additional
sources. Building framework
services needs a larger source closure than the vendor-only build; missing
dependencies must be added at the pinned superproject revisions through the
existing source-lock/reconciliation workflow.

The focused build temporarily imports the pinned SDK's public, system,
module-lib and system-server API jars for compilation. It does not rebuild
unmodified Mainline implementations. App-only parent build rules are projected
to package/license metadata while their required flag directories remain
visible. Both projections are restored on exit and also apply to vendor-only
build modes so that the expanded checkout remains usable for those builds.
Metalava is built from the pinned Android 17 sources. The focused jar target
avoids building native runtime binaries already supplied by the CI base.
Permission's module-lib API stubs are generated from its pinned API signatures
because the released SDK omits flagged role constants used by platform
services. This includes their upstream values without rebuilding the Permission
runtime. Source-to-signature validation does not apply to these snapshot imports;
their exact revision is verified by the source lock.

Full ARM64 and x86_64 packaging requires that verified `services.jar` and
projects it over `/system/framework/services.jar` through the existing cell
file-override mechanism. The base partition images and boot `framework.jar`
are reused. Incremental presenter/IME/launcher updates do not install this
framework change. Activation requires an Android-cell restart after an
authorized installation; rebuilding alone does not change a running cell.

## Validation

The patch adds `WmTests:DroidloomSystemBarInsetsTest`. Run that target from a
framework-capable Android build with the patch applied. It checks actual
WindowInsets calculations, density, moved/resized/maximum bounds, replacement
of hidden status/navigation sources, preservation of IME sources, the host
keyboard's visibility beside Android's hidden keyboard source, idempotence,
freeform immersive windows, metrics, lack of hide/show control, and confinement
to Droidloom app windows.

For runtime validation, inspect `getRootWindowInsets()` and both window-metrics
APIs before/after `hide(systemBars())`, keyboard show/hide, task resizing,
rotation and a density change. At 160/240/320/420 dpi, expect 17/25/34/44 px
at the bottom edge. The 33.6 dp status bar rounds to 34/50/67/88 px at the top
edge. Both `isVisible(statusBars())` and `isVisible(navigationBars())` stay true.
