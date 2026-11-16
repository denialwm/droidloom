# Android source and base-image locks

`native-bridge-lock.json` independently pins the
[Teto](https://github.com/denialwm/teto/tree/droidloom) (Translation Engine with Target Optimization),
based on [DigitalisX64's ARM64-to-x86_64 Berberis implementation](https://github.com/DigitalisX64/platform_frameworks_libs_binary_translation).
The updater verifies Teto's Git origin, exact commit and clean worktree
before building. The pin includes Droidloom's memory-reservation, FPCR rounding,
breakpoint/ptrace and JNI-table isolation fixes; it never follows a moving branch.
It is a separate source repository from AOSP and does not
relax the sparse lock's requirement that Android dependencies match the pinned
superproject. The accompanying AOSP `frameworks/libs/native_bridge_support`
project supplies the guest runtime and API stubs at that Android revision.
An explicit `--native-bridge-source` development checkout can contain commits
and edits on top of the locked base; its actual commit and source-file hashes
are recorded in the image provenance. See [the build guide](../../docs/BUILDING.md).

`source-lock.json` pins two deliberately different inputs:

- `aosp` is the immutable source baseline for Droidloom-owned product and HAL
  code.
- `aosp_ci_base` is the official ARM64 Android CI image used as the reusable
  framework/system base while the thin Droidloom vendor layer is built.

The CI artifact does **not** become a Droidloom image as-is. Its Cuttlefish
vendor, ODM, boot, and kernel artifacts depend on virtual-device hardware and
are excluded. Only the checksum-identified `system`, `system_ext`, and
`product` logical partitions are eligible as assembler inputs.

This split avoids a full AOSP checkout for ordinary runtime and graphics work.
A sparse, pinned source checkout is still required when rebuilding the Android
product glue, stable AIDL interfaces, SELinux policy, or Bionic Mesa/HAL
binaries.

`m2-sparse-source-lock.json` is the source/codegen, vendor-image and focused
framework-services build closure. Every project revision is an exact Gitlink
from the pinned Android 17 AOSP superproject. Its `build_proven` status records
the original ARM64 Soong validation of `libdroidloom_transport`,
`libdroidloom_denial_protocol`, `libdroidloom_minigbm`,
`libdroidloom_denial_ipc`, `libdroidloom_syncobj`, `libdroidloom_composer`, the
frozen Composer V5 bindings, the complete generic Composer Binder library
`libdroidloom_composer_aidl`, the ARM64
`android.hardware.graphics.composer3-service.droidloom` executable, and
upstream minigbm's Allocator V3/Mapper V5 artifacts succeeds and assembles a
real sparse ext4 `vendor.img`. The image also contains pinned Mesa 26.3.0
Freedreno EGL/GLES and Turnip Vulkan drivers. ANGLE is rebuilt from
`external/angle` at the superproject's commit, the one the base image ships,
and the runtime mounts it over the base's `/system/lib64/lib*_angle.so`.
A private Android SELinux policy
cannot be loaded by a shared-kernel cell, so the package-owned VINTF device
matrix records kernel policy version zero. A development-only compatibility
interposer supplies the context operations Android init requires without
claiming that the host loaded Android policy. The production security boundary belongs to host
namespaces, device allowlists, cgroups, and a host LSM. Build-proven
transitive dependencies may still be added at
their superproject commits, but tooling must not silently replace the closure
with an unrestricted `repo sync`.

`droidloom-source reconcile` handles the intended seed-lock growth without
re-fetching the verified tree. It accepts only a monotonic extension at the
same superproject commit, verifies that every existing checkout is clean and
still has its exact origin/HEAD, atomically adds missing projects, and publishes
the expanded manifest only after the complete tree passes verification.

Individual projects may additionally declare bounded `sparse_paths`. The tool
uses Git cone-mode sparse checkout, retains ancestor metadata such as package
and license files, and permits reconciliation to add explicitly locked paths at
the same exact commit. It never removes an already materialized sparse path.
The Android hardware/interfaces project exposes the frozen graphics contracts
Droidloom builds plus the Java HAL bindings needed by framework services.
Framework dependencies are similarly restricted to required source directories,
flags, API jars and compiler tooling. This does not add a full application or
Mainline implementation build. See the [navigation build contract](../../docs/contracts/navigation-insets.md).

Run `tools/droidloom-soong-core-smoke` to stage the Droidloom modules into an
existing materialization and repeat the focused ARM64 build. The command uses
temporary AOSP Finder projection markers to omit an unrelated serial-service
ownership reference and ART's test-only `csuite_test` and service API-tracking
graphs. It also omits an unrelated
framework-virtualization module whose unbundled variant forms a global Soong
cycle in the sparse tree. The markers are removed on exit; ICU and the required
ART runtime builds remain in the graph.

Both build commands also project the repo-owned valid empty
`sparse-build-placeholder.proto` into an otherwise empty framework proto glob.
This prevents an unreachable `gensrcs` module from panicking during global
Soong analysis. Two equally temporary empty AIDL interfaces satisfy framework
globs whose real sources are outside the locked sparse paths. The trap removes
all three fixtures together with the Finder markers before returning.
Incremental Soong analysis is disabled for these sparse builds so a prior
larger projection cannot leave pruned modules in the analysis cache.

Run `tools/droidloom-vendor-image-smoke` for the complete vendor image. It uses
only the unrelated serial-service, ART test/API-tracking, and
framework-virtualization Finder exclusions, removes the markers on exit,
verifies the exact
materialized source lock, stages Droidloom, and builds `vendorimage` with
missing-dependency tolerance enabled for the deliberately sparse AOSP tree.
Before entering AOSP it runs the Rust `droidloom-mesa` tool, which verifies both
pinned archive digests, applies the bounded local patches, and verifies the
complete prepared-source tree.
