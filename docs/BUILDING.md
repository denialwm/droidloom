# Build the pacman packages

The Rust package builder compiles Droidloom's host programs and modified Android
components, assembles the Android image, and produces a matching runtime/image
pacman package pair. It uses pinned upstream Android base and toolchain inputs;
it does not rebuild every upstream Android component from source.

## Shipping with GitHub Actions

`.github/workflows/ship.yml` runs on pushes to `main` or a manual dispatch of
`main`. It builds the matching x86_64 package pair, cross-builds the aarch64 pair,
runs the package checks, uploads archives to the `packages` GitHub Release, then
deploys one pacman database per architecture and `install.sh` to GitHub Pages. There is one job, with no promotion or
provenance pipeline. Pull requests do not run on this machine.

Before publishing changed packages, increment `packaging/arch/version.json`'s
`release` (or update `version` and reset `release` to 1). Published filenames are
never overwritten. A publication retry accepts existing assets only when their
bytes match. If a rebuilt package differs, increment the release. Old archives
stay available for users with cached pacman databases. Each package release also
includes a matching Git source snapshot; pinned upstream sources and licenses
are documented in that snapshot. Optional Google apps are not published.

### Manually controlled local worker

The runner and all its build state live under `/mnt/puck/logix/droidloom`:

- `runner/`: official GitHub runner, credentials and diagnostic logs.
- `work/`: Actions checkouts and temporary files, cleaned by checkout each run.
- `cache/arch/`: rootless Podman storage, downloaded inputs, AOSP sources and
  incremental compiler output.
- `cache/host-target/`: host Rust tooling build cache.
- `cache/cargo-home/`: host Cargo downloads.
- `sysroot/`: Arch Linux ARM root filesystem the aarch64 host programs link
  against, with development headers and pkg-config files. `runner start`
  refuses to arm without it.

The machine needs the usual build prerequisites plus `gh` (authenticated with
repository administration access for runner registration), `curl`, `tar`,
`sha256sum`, `cmp`, `jq`, `repo-add` (Arch's `pacman` package), and a working systemd
user manager. Configure GitHub Pages once with **Source: GitHub Actions**.
No host sudo is used by the runner or package builds.

Install or refresh the official runner while stopped:

```console
cargo run --locked -j 1 -p droidloom-package -- runner setup
```

Before pushing `main`, arm one job, then push:

```console
cargo run --locked -j 1 -p droidloom-package -- runner start
git push origin main
```

The ephemeral runner exits and unregisters after one job. It has no boot service.
It also stops after 24 hours if unused or stuck. Each queued run needs a new
`runner start`; concurrency prevents simultaneous publication. Start it only for
trusted changes. This runner uses the local user's account and is not a security
boundary around workflow code.

```console
cargo run --locked -j 1 -p droidloom-package -- runner status
journalctl --user -fu droidloom-actions-runner
cargo run --locked -j 1 -p droidloom-package -- runner stop
```

`stop` also removes a leftover GitHub registration, stops remaining build
containers, and preserves caches. The workflow calls `runner cleanup` after each
job to stop leftover containers in its package workspace. Cancel
an active workflow in GitHub before stopping its worker. Re-arm and rerun a failed
workflow after fixing its cause. A Pages failure can leave uploaded archives, but
the previous database remains live until deployment succeeds.

`DROIDLOOM_PACKAGE_WORK` overrides the default `.work/arch` directory for builds
and package checks; it must be an absolute path. The Rust runner tool sets it to the persistent
cache above, outside checkout cleanup; machine paths stay out of the workflow. Do not use `--clean` for normal releases.
The first build needs the full download/compile; later builds reuse the cache.
For a SurfaceFlinger-only change, reuse a prepared ARM64 workspace with
`cargo run --locked -j 2 -p droidloom-update -- --jobs 4 framework-build --work .work/arch/build --product droidloom_arm64 --surfaceflinger-only`.
This selects the native SurfaceFlinger target and its dependencies, without
building system_server, Android images or the complete runtime bundle. It does
not install or activate the output; verify compatibility and package the changed
component through the device's normal override/update path before validation.

The builder leaves two logical CPUs free and the host Cargo build uses one job.

To prepare the Pages tree locally after building, without publishing:

```console
cargo run --locked -j 1 -p droidloom-package -- ship prepare
```

This writes `dist/pages`. `ship publish` uploads to GitHub and is normally run
only by Actions with its scoped token. Pages stores only metadata and the
installer because Android packages approach its 1 GB site limit. Pacman's
`CacheServer` fetches the archives from Releases without requesting databases
there. Packages are currently unsigned; see [INSTALL.md](INSTALL.md) for the
repository's explicit signing policy.

## Frame-stage timing baselines

Build timing instrumentation before changing frame-processing behaviour. The
shared Rust logger samples every eighth frame, formats records on a separate
background thread and uses a bounded nonblocking queue. Disabled stage timers
read no clocks and create no logging thread. The first enabled sample initializes
the logger; exclude startup from a steady-state comparison.

The Android hooks are included by `framework-build --tasks`; rebuild the host
presenter too. Install a matching instrumented build through the normal workflow
before collecting a baseline. Source builds and the commands below for reporting
do not install or restart anything. Enabling tracing for an installed cell needs
the ordinary Droidloom-only restart; it closes Android application windows.

Enable the presenter's `frames` diagnostic for that run, for example with
`systemctl --user set-environment DROIDLOOM_TRACE=frames` on the packaged
user-service setup. Inside Android, set `persist.vendor.droidloom.trace=frames`
before restarting Droidloom. That persistent property survives a cell restart.
Both are read on the first check and remain fixed for that process; `audit` and
`input` can be added to either list. After measuring, set the Android property
to an empty value, run `systemctl --user unset-environment DROIDLOOM_TRACE`, and
restart Droidloom normally to return to the disabled path. Do not restart the
desktop session.

Capture Android `logcat -v raw -s DroidloomTiming:I` and the presenter's user
journal as separate text files under ignored `.work/frame-timing/`. Android
records use liblog, including SurfaceFlinger whose standard error may be closed;
host records use standard error. Capture full trace records, not only `timing_`
lines, so the sequence-gap check can distinguish omitted records. Keep a single
process incarnation per capture file. Start from a fresh process or clear capture
boundaries; avoid historic journal records and startup events. Record the exact
build, app, window sizes, refresh rate and workload in the report label.
Check process IDs at both capture boundaries. If Droidloom restarts, start new
files and reattach both collectors; a host journal filter for the old PID and an
Android logcat connection to the old cell can silently miss the replacement run.

| Stage | Measured work |
| --- | --- |
| `timing_sf_composite` | One complete SurfaceFlinger `composite()` call, including nested task and bootstrap work |
| `timing_sf_task_scan` | Task classification, bounds and work-key collection across the frame's layers |
| `timing_sf_snapshot_copy` | Copying a changing task's complete layer snapshots |
| `timing_sf_direct_prepare` | CompositionEngine geometry, colour and composition-state preparation before direct export |
| `timing_sf_direct_publish` | Direct-layer eligibility, imports, scene construction and transport submission; outcome 0 selects fallback/retry, 1 accepts the direct path |
| `timing_sf_fallback_render` | Fallback CompositionEngine `present()` CPU/submission work; GPU completion is outside this duration |
| `timing_sf_bootstrap_present` | Invisible bootstrap output's `present()` bookkeeping, including nested request construction |
| `timing_sf_bootstrap_requests` | Building the bootstrap's display settings and client rendering requests before its rendering shortcut |
| `timing_sf_bootstrap_state` | Updating colour/composition state, planning layers and writing HWC state; includes the bufferless path's fence-only Composer request before validation |
| `timing_sf_bootstrap_begin` | Beginning the bootstrap display frame |
| `timing_sf_bootstrap_prepare` | Selecting or predicting the HWC composition strategy; prediction may include nested dequeue/request work |
| `timing_sf_bootstrap_finish` | Completing client composition and queueing its target; includes nested dequeue/request/queue work when prediction is disabled |
| `timing_sf_bootstrap_dequeue` | Acquiring a client target from the render surface; outcome 0 means no buffer was obtained |
| `timing_sf_bootstrap_queue` | Queueing the client target, including display-surface advancement |
| `timing_sf_bootstrap_hwc_present` | HWC presentation and fence retrieval on the thread executing it |
| `timing_sf_bootstrap_complete` | Notifying the render/display surface that HWC presentation completed |
| `timing_sf_bootstrap_release` | Assigning and merging layer release fences and clearing released layers |
| `timing_sf_bootstrap_cache` | Scheduling cached layer-set rendering |
| `timing_presenter_commit` | Applying a valid direct-layer scene through the parent Wayland commit; excludes earlier receive/validation and later socket flush/display |

For the bufferless bootstrap path, `bootstrap_state` includes the fence-only
Composer request and `bootstrap_queue` measures display-surface advancement;
no `bootstrap_dequeue` occurs.
HWC preparation, presentation and release timers remain. Set
`persist.vendor.droidloom.bootstrap_bufferless=false`
inside Android and restart Droidloom to restore the buffer-backed path, then
restore `true` after comparison. Keep each switch outside the measured interval and allow the workload
to settle; retain the full trace for loss checks. The focused regression test is
`cargo test --locked -j 1 -p droidloom-update --test bufferless_bootstrap`; it
compiles the patch's real completion methods with isolated framework substitutes
and checks fence retention, FD ownership and fallback. A matched Android build
and real application validation remain necessary.

Bootstrap stages are nested inside `bootstrap_present`; do not add parent and
child durations. If HWC presentation is offloaded, its present/complete/release
timers run on that worker and may overlap other stages or outlive `present()`.
The thread CPU duration only includes the thread that owns each timer, excluding
Binder servers and GPU work. Fine-grained tracing itself adds clock reads and
queue operations to sampled frames; compare builds with identical tracing.

SurfaceFlinger samples one in eight composite calls; task stages share that sample
identity. The host independently samples one in eight asynchronous scene IDs.
Those identities are different domains and cannot establish cross-process or
input-to-display latency. A successful direct publication may reuse an unchanged
scene, and fallback/import-pending samples are retained separately by outcome.
Legacy synchronous scenes have no scene identity and omit the host timer.
Outcome 1 means that the measured operation completed or accepted publication;
it does not prove that Wayland flushed, a GPU finished or a frame was displayed.

Warm the app and shader caches for at least 15 seconds, then capture a repeatable
30–60 second workload. Keep idle, scrolling, a game and simultaneous animating
windows in separate runs. Repeat each run at least three times with identical
window sizes, refresh rate, tracing flags and workload. Include an idle run to
check that instrumentation does not introduce periodic frame activity. Fixed
one-in-eight sampling can alias periodic workloads; vary the starting phase and
inspect multiple runs. Keep thermal and background-load differences in mind.

Summarize the captured files, then compare the same workload on a later build:

```console
cargo run --locked -j 1 -p droidloom-frame-timing -- summarize .work/frame-timing/android.log .work/frame-timing/presenter.log --label "baseline BUILD_ID; APP; workload; SIZE; HZ" --output .work/frame-timing/baseline.json
cargo run --locked -j 1 -p droidloom-frame-timing -- compare .work/frame-timing/baseline.json .work/frame-timing/candidate.json
```

Reports include sample count, mean, p50, p95, p99 and maximum for elapsed and
thread CPU time, grouped by stage, outcome and layer count. Percentiles use
nearest rank over observed samples. Comparisons show matching groups and flag
missing groups; they are descriptive, not statistical significance tests.
Use sample counts when interpreting tails (p99 from a handful of samples is
effectively the maximum). Whole-composite samples group by the total frame layer
count; task samples group by their task layer count. Object zero means a global
stage and otherwise identifies an Android task or host object, not an app name.

The report exposes queue drops, observed sequence gaps, duplicates, malformed
timing records and observed 65,536-record process caps. Drops and gaps overlap
and must not be added together. Loss or a capped capture can bias results;
recapture a shorter run from fresh processes. Clearing logcat or starting a new
file does not reset the per-process cap, and a full logcat ring can discard the
start of a run even when the retained tail has no sequence gaps.
Worker-thread formatting/logging is outside each stage
duration but still consumes CPU, and outer spans include nested instrumentation
costs. Compare equally instrumented builds and retain an untraced control run.
Do not interpret wall-minus-CPU as pure IPC time or the fallback timer as GPU
time. No real application baseline has been recorded merely by testing this
instrumentation.

### GPU client accounting

Build the read-only counter tool without changing the Android image or enabling
frame tracing:

```console
cargo build --locked -j 1 -p droidloom-frame-timing --bin droidloom-gpu-usage
target/debug/droidloom-gpu-usage probe --output .work/gpu-probe.json
target/debug/droidloom-gpu-usage capture --process SurfaceFlinger=HOST_PID --process Presenter=HOST_PID --process Game=HOST_PID --seconds 60 --interval-ms 500 --label "BUILD; APP; workload; SIZE; HZ" --output .work/gpu-capture
```

Use distinct live host PIDs from `probe`, including app rendering subprocesses.
Replace the placeholders before running. Run in the host PID namespace;
sandbox process visibility can hide the running cell. Administrator access may
be needed to read Android processes owned by other UIDs. Capturing reads only
`/proc` accounting and writes its new output directory; it never opens a GPU
device, changes driver settings, launches apps or restarts services. Keep the
raw `samples.jsonl`, process/boot metadata, JSON report and Markdown summary.
The directory must not already exist. Samples are flushed as they arrive so
partial data survives interruption. For a manually ended run, pass
`--stop-file .work/gpu-capture.stop` using an initially absent marker; creating
that file ends sampling and writes the report without signalling a process.
The `--seconds` bound still applies. Fixed selected PIDs are not automatically
replaced after an app or service restart; start a separate capture with the new
identities. Probe uses kernel process names and does not collect arguments,
environments or app contents.

The tool uses the kernel's [DRM client accounting interface](https://www.kernel.org/doc/html/latest/gpu/drm-usage-stats.html).
It deduplicates `(driver, device, drm-client-id)` across duplicated and shared
handles, including handles in different selected processes. When several
processes share a client, its work cannot be attributed to each owner separately.
Reported percentages are changes in driver-reported engine busy nanoseconds,
divided by observed wall time and the reported engine capacity (default one).
They are not shader occupancy, power, queue delay or input latency. Do not sum
clients or different engines into a total GPU percentage: their accounting can
overlap. Frequency and other GPU clients can affect comparisons. A compositor
client covers the whole desktop, so its activity alone is not Droidloom overhead.

Missing or unsupported engine counters are unavailable, not zero. Cycles-only
drivers remain visible but are not converted to nanoseconds. The report exposes
read/identity errors, counter regressions, interrupted coverage and scan duration;
temporarily regressing counters retain their high-water mark until they catch up.
Resident and shared memory are retained per client; shared buffers can appear in
several clients, and deprecated `drm-memory-*` aliases are not added again.
This accounting does not depend on the frame logger or its per-process cap.
Keep app sizes, refresh rate and workload fixed, record unrelated GPU load and
repeat active and idle scenarios before claiming a GPU improvement.

## Startup timing

A start is measured from `systemctl --user start droidloom.service` until
Android's `sys.boot_completed` is 1. Stop Droidloom fully and let the device
settle for a few seconds before each run. Runs vary by about half a second, so
compare three of each. Use `CLOCK_MONOTONIC` for every source so that they line
up: `logcat -v monotonic`, `perf record -k CLOCK_MONOTONIC` and the host's
`/proc/uptime` on a device that has not suspended since boot.

Where the time goes, from coarse to fine:
- `getprop | grep ro.boottime.` gives when init started each Android service.
  The events buffer has `boot_progress_*`, and logcat has `SystemServerTiming`
  spans.
- `SystemServiceManager: Service NAME took N ms in onBootPhase` names slow
  system services. A slow service with little CPU time is usually waiting on a
  binder call.
- `perf record -a -k CLOCK_MONOTONIC -e binder:binder_transaction,binder:binder_transaction_received`
  shows that call: the destination process and the transaction `code`. A method's
  code is 1 plus its index in the frozen `aidl_api/INTERFACE/current` file of its
  AIDL interface. `INetd` code `0x31` is `bandwidthSetGlobalAlert`, for example.
- `sched:sched_switch` in the same recording shows what a waiting thread was
  blocked on.
- For the host part before Android `/init`, record
  `sched:sched_process_exec,sched:sched_process_exit,sched:sched_process_fork`
  system-wide. `perf` takes about a second to start recording, so start it two
  seconds early and run `/usr/bin/true` just before the unit start; that exec
  is time zero.
- The busy fraction of `/proc/stat` over the start tells whether startup is
  CPU-bound or a chain of waits.

On aston, the start is a chain of waits; the CPU is about a third busy. In
order: the user unit and droidloomd, host network setup, Android init's early
services (aconfigd, apexd, odsign, bpfloader), zygote preload, system_server's
services, then persistent apps and home.

## 1. Prepare an x86_64 build machine

The supported package workflow uses rootless Podman with an Arch build container.
Install Rust/Cargo with a C linker, Git, Podman and rsync. On Arch Linux:

```console
sudo pacman -S --needed rustup gcc git podman rsync
```

Sudo is needed only to install those host tools and dependencies under `/usr` and
update pacman's database. If you already have a working Rust installation, keep
it; you do not need to replace it with rustup. The checkout pins Rust 1.93.0.

Allow **120 GiB free** for a fresh build, including sources, toolchains, compiler
outputs, downloads and temporary package staging. The workflow has been exercised
on a machine with 32 GiB RAM and 16 logical CPUs. Retained build files are smaller
than peak space requirements: keep free space for staging and compression even
when rebuilding from cache.

Verify rootless Podman works as your ordinary user:

```console
podman info
```

Resolve any rootless Podman setup error before building; do not switch to a sudo
build. Internet access is required for the initial container and pinned inputs.

## 2. Get the source

```console
git clone https://github.com/denialwm/droidloom.git
cd droidloom
```

For a published preview, check out its corresponding source tag or commit before
building. The `main` branch may contain changes beyond that preview. Package
version and release are defined in `packaging/arch/version.json`.

## 3. Build binaries and packages together

```console
cargo run --locked -j 1 -p droidloom-package -- build
```

Run as your ordinary user, without sudo. The Rust tool performs compilation and
invokes makepkg inside the rootless container; no manual binary build or separate
packaging script is required. Shell is used only at required upstream interfaces
and in PKGBUILD functions.

The builder limits jobs to the available logical CPU count minus two. To limit compilation further:

```console
cargo run --locked -j 1 -p droidloom-package -- build --jobs 4
```

Container CPU pinning is opt-in, so the default build works without rootless
`cpuset` delegation. To restrict the container to a CPU subset as well:

```console
cargo run --locked -j 1 -p droidloom-package -- build --cpuset
```

This requires the host to delegate the `cpuset` controller to rootless Podman.
Without it, the job limit still leaves capacity for two logical CPUs and nested
Android builds retain their own CPU affinity control.

To cross-build the aarch64 pair for Arch Linux ARM on the same x86_64 machine,
pass an ARM64 root filesystem that has the libraries, headers and pkg-config
files the host programs link against:

```console
cargo run --locked -j 1 -p droidloom-package -- build --architecture aarch64 \
  --sysroot /absolute/path/to/sysroot
```

The sysroot is mounted read-only into the container. The Android image is built
as `droidloom_arm64`, from the same pinned inputs the phone builds use.
`--component` and `--native-bridge-source` apply to x86_64 builds.

Outputs are written to `dist/arch/<version-release>/<architecture>/`. For
revision 0.1.0-7:

```text
dist/arch/0.1.0-7/x86_64/droidloom-runtime-0.1.0-7-x86_64.pkg.tar.zst
dist/arch/0.1.0-7/x86_64/droidloom-image-0.1.0-7-x86_64.pkg.tar.zst
dist/arch/0.1.0-7/aarch64/droidloom-runtime-0.1.0-7-aarch64.pkg.tar.zst
dist/arch/0.1.0-7/aarch64/droidloom-image-0.1.0-7-aarch64.pkg.tar.zst
```

The command prints the exact installation command when finished. Install both
packages by following [INSTALL.md](INSTALL.md).

Normal builds fetch [Teto](https://github.com/denialwm/teto), the Translation
Engine with Target Optimization, at exact commit
`917021cf3ecd077eb1581146a79d2265011755b8`, pinned in
`android/manifest/native-bridge-lock.json`. It includes the memory-reservation,
FPCR rounding, breakpoint/ptrace and JNI-table isolation fixes. No local
development checkout is needed for these fixes.

For further Teto development, use a separate Git checkout whose `origin`
matches the repository URL in that lock and whose history contains the pinned commit.
Commits on top of that base and uncommitted edits are both supported:

```console
cargo run --locked -j 1 -p droidloom-package -- build --jobs 8 \
  --native-bridge-source /absolute/path/to/digitalis
```

The checkout is mounted read-only and copied into the generated AOSP workspace.
The installed `/system/etc/droidloom-native-bridge.json` records its base, actual
commit and hashes of all working source files. Keep any unpublished development
edits needed to reproduce such a package. Omit the option for the locked Teto
source. Existing caches with an old origin URL, an older revision, or
development edits deliberately fail verification instead of silently discarding
them. Use a fresh build workspace when switching to the new pin. For an existing
development clone, preserve the DigitalisX64 remote as `upstream`, set `origin`
to the lock's Teto URL, and fetch the pinned commit before building.

Teto's `droidloom` branch includes `android-mmap-noreserve`,
enabled by Droidloom's `ro.berberis.flags`.
It gives private anonymous guest mappings Android's expected
overcommit behavior without modifying the desktop's global memory policy. Strict
host overcommit policy still applies. The pin adds branding, licensing documents
and change notices to the previously validated translator code (3743 host tests
passed, three skipped; Brawl Stars
startup confirmed by the user). Pinning source does not publish new packages or
replace existing installed images and local library overrides. Teto derives from
Digitalis/AOSP Berberis; upstream history and notices remain intact. Existing
`berberis_*` ABI names, build targets, source paths, and the local
`.work/translation/digitalis` directory remain unchanged for compatibility.

### Translation stress and performance benchmarks

`droidloom-translation-bench` is a Rust command-line runner for Digitalis's
existing `berberis_arm64_host_tests`. Run it on an **x86_64 Linux host** as an
ordinary user. It does not install packages, start Android or restart a session.
An ARM64 phone runs ARM64 natively and cannot measure this translation path.

Before measuring a translator edit, rebuild **the host test binary**, not just
`libberberis_arm64.so`: the host tests statically link the translator. In the
already prepared Android build environment, use the updater's focused target:

```console
DROIDLOOM_NATIVE_BRIDGE_SOURCE=/absolute/path/to/digitalis \
  cargo run --locked -j 1 -p droidloom-update -- \
  native-bridge-build --work /absolute/path/to/prepared-build --tests
```

Use the paths visible inside the rootless builder when running there (`/build`
for the prepared build and the mounted Digitalis source path). This is the
existing source build workflow and requires its prepared dependencies. A package
build alone does not request the host tests. Keep the test executable in its
Soong output tree, alongside its test data and `../../lib64` support libraries.
Each benchmark run makes and verifies a private copy of these inputs under its
output directory, so a later rebuild cannot mix executables within a run. It
also retains the benchmark source and writes `inputs.json` before testing;
only `result.json` denotes a completed run. Avoid concurrent builds anyway,
because their CPU and memory traffic can distort the measurements.

From the Droidloom checkout, save a baseline (choose an available, idle logical
CPU and keep it the same for subsequent runs):

```console
cargo run --locked -j 1 -p droidloom-translation-bench -- run \
  --cpu 6 --label before --output .work/translation/bench-before
```

The default paths match this checkout's `.work/arch/build/android-out` host
test executable and `.work/translation/digitalis` benchmark source. Override
`--binary` and `--benchmark-source` for another build. The supplied benchmark
source must be the file used to build that binary; hashing it cannot prove that
a stale executable was rebuilt. Optional `--build-manifest` records the same
build's `target/product/droidloom_x86_64/droidloom-native-bridge-source.json`.
The executable hash identifies exactly what ran; the optional manifest is
supporting provenance, not independently verified linkage to that executable.

The runner first executes the complete non-benchmark correctness suite twice
in fresh processes, in its normal `two-gear` mode. These tests directly exercise
the interpreter, lite translator, heavy optimizer, decoder, guest ABI and kernel
emulation, including upstream differential fuzz cases. `--stress-repeats 10`
increases repeated stress; it does not generate new fuzz seeds or establish a
code-coverage percentage. Disabled and skipped tests remain visible in the logs
and correctness summaries. A failed test or timeout aborts the run before it can
publish `result.json`. Some upstream seccomp/ptrace tests require an ordinary
unsandboxed host process; an agent/container sandbox can cause failures. Do not
filter those failures away to manufacture a passing baseline.

After correctness passes, the runner measures each of these workload families
in `interpret-only`, `lite-translate-or-interpret`, and `two-gear` modes:

| Workload | What it exercises | Loop iterations per timed run |
| --- | --- | --- |
| Integer | Mixed integer arithmetic, bitwise operations and shifts | 3,000,000 |
| Branch | Comparisons and conditional branches | 3,000,000 |
| Call | Guest calls, returns and dispatch | 3,000,000 |
| NEON | Vector arithmetic | 3,000,000 |
| FP | Scalar double-precision arithmetic | 3,000,000 |
| Memory | Repeated loads/stores over a small hot buffer | 3,000,000 |
| Syscall | Guest `clock_gettime` emulation | 1,000,000 |

`--mode two-gear` selects a shorter, production-mode timing sweep.
`--samples 9` (the default) collects nine fresh-process results per case; each
result is the upstream median of five internal timings. Case order rotates
between sweeps. `--timeout-seconds 120` bounds each child process and preserves
logs on failure. All test processes inherit the selected CPU affinity and a
clean environment. Set translator flags explicitly with `--flags` when needed;
ambient `BERBERIS_*`, tracing and dynamic-loader overrides are removed.
The workload uses one logical CPU, leaving at least two physical cores outside
its affinity on this eight-core development host. To pin the runner's own
snapshotting/reporting work as well, first build it with
`cargo build --locked -j 1 -p droidloom-translation-bench`, then invoke
`taskset --cpu-list 6 target/debug/droidloom-translation-bench run --cpu 6 ...`.

After changing and rebuilding Digitalis, repeat with a fresh output directory:

```console
cargo run --locked -j 1 -p droidloom-translation-bench -- run \
  --cpu 6 --label after --output .work/translation/bench-after
cargo run --locked -j 1 -p droidloom-translation-bench -- compare \
  .work/translation/bench-before/result.json \
  .work/translation/bench-after/result.json --fail-on-regression
```

Comparison reports milliseconds, percentage time change and relative
interquartile range (IQR) for each mode/case. **Positive time change means
slower.** Changes beyond 5% are flagged by default; a row with more than 10%
IQR in either run is inconclusive. `--threshold` and `--noise-limit` adjust these
screening thresholds. These are heuristics, not statistical significance tests;
IQR is calculated over the saved process medians and does not expose variation
hidden inside each upstream median. With `--fail-on-regression`, exit code 2
means at least one non-noisy regression, 3 means noisy data without a detected
regression, and 1 means invalid input or execution failure. Without that flag,
performance findings are informational.

The comparison rejects changed machines, CPUs, kernel versions, recorded power
settings, mode/flag selections, benchmark source, work counts, support libraries
or test data and correctness summaries. It allows a changed test binary, which is the subject
of the comparison. Use the same build configuration, an idle host and consistent
power/thermal conditions. Pinning a CPU does not isolate it from other tasks or
its SMT sibling. Repeat unchanged builds to establish normal noise, then repeat
A/B or ABBA runs before accepting small gains. Reports and logs belong under
`.work/`, outside Git; existing result directories are never overwritten.

Scope: this stresses many paths but does **not** cover every ARM instruction or
every translator path. The timed kernels themselves use upstream `SUCCEED()`
rather than answer checks; correctness comes from the separate test gate, not
from their speed. The upstream warmup is only one loop iteration, so optimization
tier promotion can still occur inside the timed interval. These are guest-loop
elapsed times, not isolated translation-compilation costs or proven steady-state
heavy-JIT timings. The reported instruction counts are workload labels (the
branch case counts skipped instructions); the runner deliberately compares time
rather than treating the upstream MIPS number as an exact instruction rate.
Iteration counts are fixed in upstream source, not controlled by `--samples`.
Changing those kernels requires rebuilding the host tests and starting a new
baseline. Memory bandwidth, atomics under contention, Android API proxies,
graphics, app startup and cold translation need additional dedicated workloads
and app-level measurements. An optimizing-mode result can include fallback;
selecting that mode does not prove every instruction used the heavy optimizer.

## Focused audio checks

Host audio builds need the `libpipewire-0.3` development headers (`libpipewire`
on Arch). Integration tests also need `pipewire`, `wireplumber`, `pw-cli`, `pw-link`, `pw-dump` and
`systemd-socket-activate`. They create a private server and null-sink graph in
temporary storage; they do not change the user's audio session or record a mic.

```console
cargo test --locked -j 1 -p droidloom-audio -p droidloom-audio-client -p droidloom-audio-protocol
cargo run --locked -j 1 -p droidloom-update -- -j 1 audio-build --work .work/arch/build
```

`audio-build` reuses a prepared package workspace, compiles the audio APEX and
`libaudiobase.so`, restores its checked source projections, and does not install
or restart services. Use `--product droidloom_arm64` for the ARM64 audio components.
It leaves an existing translator development snapshot and its recorded artifact
provenance unchanged. Physical
audio and Android app validation are additional checks; see the
[audio guide](audio-integration-proposal.md).

## 4. Exercise package installation

```console
cargo run --locked -j 1 -p droidloom-package -- check --packages dist/arch/0.1.0-7/x86_64
```

Use your build's output directory if its version differs. This runs actual pacman
dependency installation, setup, reinstall, removal and data-retention checks in
a disposable rootless Arch container. It needs no host sudo and does not restart
your desktop. It uses a simulated GPU entry for setup checks; app rendering must
be tested separately on a real Wayland desktop.

Add `--previous dist/arch/<older-version-release>/<architecture>` to include
upgrade and downgrade transactions against an older pair.

For a pair built for another architecture, such as the aarch64 pair on an x86_64
machine, `check` reads both packages' metadata and requires every packaged
program to be built for that architecture; it does not install them.

## Rebuilds and troubleshooting

For a change confined to host components, increase the release in
`packaging/arch/version.json`, then select the component instead of compiling
Android again:

```console
cargo run --locked -j 1 -p droidloom-package -- build --component droidloom-supervisor
```

This compiles the selected Cargo package and its Rust dependencies. Selecting
`droidloom-supervisor` updates `droidloomctl`, `droidloomd` and the supervisor
together. Other supported components are `droidloom-wayland`,
`droidloom-applications`, `droidloom-doctor`, and `droidloom-package-support`.
Repeat `--component` to select more than one. Without it, the command builds
the complete runtime and Android components as before.

A component build reuses unchanged files from the newest completed older package
pair with the same version and architecture in `dist/arch`. It produces a new
matching pair and records its baseline and selections in `build-summary.json`.
It does not use installed binaries or rebuild Android, Mesa or SurfaceFlinger;
extracting and repackaging the image still takes some time.

Use a full build for Android changes, changes spanning the host/Android protocol,
or changes to dependencies and package integration. Component builds reject
changed Cargo lockfiles, pinned Android inputs, runtime contracts and packaging
recipes. They also reject `--clean` and `--source-cache`. A completed full build
provides the baseline; builds made before component support need a full rebuild
to record their inputs. Keep that baseline's archives, summary and
`component-inputs.json` together. Unselected source edits are not included.

Rerun the same build command to reuse caches. Downloads, sparse AOSP sources and
outputs live under `.work/arch`; build attempts save `build-*.log` there.
`build --clean` removes this workflow's compiler outputs while retaining downloads.
An existing sparse AOSP source checkout can seed a build with
`build --source-cache /path/to/checkout`; installed binaries are not build inputs.

If staging fails with `No space left on device`, free disposable outputs and rerun
the builder. Do not distribute partial archives from a failed attempt. APKs,
packages, caches and local test artifacts must stay out of Git.

The fully empty-cache source-download path has not yet been rehearsed; see
[KNOWN_ISSUES.md](KNOWN_ISSUES.md). Build logs should accompany reports of failures.

## Desktop image policy

The Android 17 x86_64 package omits legacy VNDK 31–34 APEXes from its derived
`system_ext.img`. The pinned upstream inputs remain unchanged. The policy rejects
other vendor products, SDK versions, or an explicit legacy VNDK selection. `adbd`
is retained. Image derivation runs in one fakeroot session to preserve Android
ownership and security xattrs. Before staging, it re-extracts the rebuilt image
and compares every retained file’s contents, permissions, ownership, timestamps,
symlink target and extended attributes. Missing expected VNDK files or any other
content/metadata change fails the build. A full package build is required.

## Automatic app compilation

Droidloom requests ART's `speed` compiler filter for normal installs, updates,
fast installs and bulk restores. This policy lives in
`android/framework/droidloom-window-policy/droidloom-framework.rc` and uses
PackageManager's shared installation path, covering app stores, APK sideloads
and ADB installs. It compiles eligible Java/Kotlin app code for the guest ABI
without waiting for a usage profile. Existing native libraries are unchanged.

After each Android cell boot, `droidloom-app-optimize` runs a background-priority
catch-up pass over installed apps, including preloaded apps. It does not depend
on Android's phone-style screen-idle job, which a host-managed desktop may never
satisfy. Valid compiled artifacts are reused: the command uses neither `-f` nor
`--reset`. Scheduled background dexopt also uses `speed`; ART's inactive-app
storage reclamation and installation exceptions remain in place.

Compilation adds install time and code-cache storage; the initial catch-up pass
also consumes CPU. Apps can remain open and use the new artifacts on their next
launch. Android still controls exclusions such as debuggable, incremental and
embedded-DEX installs, explicit installer compiler overrides and missing class
loader contexts. The catch-up pass covers APK DEX code, including code-bearing
splits; it does not force compilation of dynamically downloaded secondary DEX.

Inside Android, inspect `getprop pm.dexopt.install` and
`cmd package art dump PACKAGE`. After the next cell start,
`getprop init.svc.droidloom-app-optimize` reports `running` during catch-up and
`stopped` once the one-shot service exits; its exit status is recorded by init.
A stopped service alone does not prove every package compiled successfully.

## Optional Google apps

`droidloom-gapps` builds an optional add-on from a **locally supplied LiteGapps
regular lite archive for Android 17/API 37**. The guest architecture comes from
the APK payload and base build properties, independently of the build host.
Both raw ext4 developer partitions and the standard desktop EROFS partitions
are supported. Every selected APK must contain the guest's native ABI or be
Java-only; Google apps do not depend on ARM translation on x86_64.

Host tools are `bsdtar`, `xz`, `e2fsprogs`, Android SDK `aapt2` and `apksigner`,
and a Java runtime. EROFS derivation also requires `erofs-utils`, `fakeroot` and
`attr`. Pacman archives use the existing rootless Podman Arch builder
and its `makepkg`/`fakeroot` tools. Build as an ordinary user; assembly uses `debugfs` on private image
copies and does not mount images, install software or start services.
EROFS assembly runs extraction and rebuilding in one fakeroot session to retain
Android ownership and security attributes without administrator access.

Inspect the selected archive, then pass its reviewed SHA-256 to the builder:

```console
cargo run --locked -j 1 -p droidloom-gapps -- inspect /path/to/LiteGapps-arm64-17.0.zip
cargo run --locked -j 1 -p droidloom-gapps -- build \
  --archive /path/to/LiteGapps-arm64-17.0.zip --sha256 <archive-sha256> \
  --base /path/to/active-image-set --output .work/gapps-arm64 \
  --aapt2 /path/to/aapt2 --apksigner /path/to/apksigner
```

`--base` contains `images/system.img`, `images/system_ext.img` and
`images/product.img`. Use the actual activated images, including previous
Droidloom derivations. `--system-ext /path/to/system_ext.img` selects a separately
derived input without modifying the base directory. APK signatures, package IDs,
SDK and native ABI are checked. The importer does not execute upstream installer
scripts or resign APKs. A supplied archive checksum establishes input identity;
it is not an independent endorsement of its publisher.

For x86_64, an Android 17 ARM64 archive can supply the Java-only Services
Framework and Android 17 configuration when native Play APKs are provided:

```console
cargo run --locked -j 1 -p droidloom-gapps -- build \
  --archive /path/to/LiteGapps-arm64-17.0.zip --sha256 <archive-sha256> \
  --base /usr/lib/droidloom/images --output .work/gapps-x86_64 \
  --play-services-apk /path/to/x86_64/GmsCore.apk \
  --play-store-apk /path/to/x86_64/Phonesky.apk \
  --aapt2 /path/to/aapt2 --apksigner /path/to/apksigner
```

Replacement APKs must have the same package ID and signing certificate as the
archive's APK, or a verified ancestor in its signed certificate rotation lineage.
Their actual minimum SDK and native ABI are checked and their hashes are recorded
in the manifest and import report. Older archive release labels alone do not
establish APK compatibility: only the two native APKs are selected, never older
Services Framework, platform libraries or permission XML. Configuration is
filtered against the replacement APKs' requested permissions. Validate the result
on Android 17 before relying on sign-in or other Google APIs.

On the development desktop, native Play Services 24.23.37 and Play Store 41.3.25
from the Android 15 x86_64 archive completed account sign-in with the Android 17
framework and configuration. This older Play Services build also starts a
hotspot listener that crashes when Android has no Wi-Fi service. The local
workaround disables only that component for Android user 0, from inside the
Android cell:

```console
/system/bin/pm disable --user 0 com.google.android.gms/com.google.android.gms.magictether.host.TetherListenerService
```

This is a desktop workaround, not a general default for devices with Wi-Fi.
Google subsequently updated Play Services to 26.33.32 and Play Store to 53.0.27,
both targeting API 37. The user confirmed NTE launches successfully after an
official **Update from Play**, following permission repair and a local catalog
compatibility experiment; see the
[app-service limitations](KNOWN_ISSUES.md#application-compatibility).

The default selects Google Services Framework, Play Services and Play Store.
`--sync-adapters` also selects Google Contacts and Calendar sync. Configuration
is filtered to selected applications and their requested permissions, with
privileged allowlists on the apps' own partitions. Pixel feature declarations,
phone setup wizard configuration and unrelated applications are excluded.
`import-report.json` lists selected files and excluded upstream files. Original
license comments and the archive's license notice are retained.

Only `product` and `system_ext` are derived. Assembly checks every retained
file's contents, symlink target, UID/GID, mode, mtime and xattrs and checks filesystem
integrity. The manifest binds the outputs to all three exact base image hashes,
the source archive, SDK, architecture and verified APK signers. A failed build
does not publish the destination. Keep images, APKs and packages outside Git.

For a separately maintained ARM developer runtime:

```console
cargo run --locked -j 1 -p droidloom-gapps -- package \
  --addon .work/gapps-arm64 --version 4.9.20260513 --standalone \
  --output dist/droidloom-gapps-4.9.20260513-1-aarch64.pkg.tar.zst
```

For pacman-managed deployments, replace `--standalone` with
`--base-package-version <version-release>` to require the exact matching
`droidloom-runtime` and `droidloom-image` packages and include the lifecycle hook.
Standalone packages require manually stopping Droidloom before every install,
upgrade or removal. They contain images and notices, not an updated supervisor.

The runtime must include `gapps_dir` support before activation. Install the
package with pacman only when ready; administrator access is needed to write
the package-owned directory and update the package database. While Droidloom is
stopped, add `"gapps_dir": "/usr/lib/droidloom/addons/gapps"` to its root-owned
cell specification. Package installation alone does not activate Google apps
on an installation that is already set up. The first setup selects an installed
add-on by itself while Android has never booted on the storage, so installing
the package before that setup is all a new installation needs.
The supervisor verifies root ownership and every base/output hash before boot,
hashing an image again only after it changed since it last matched
(`/var/cache/droidloom/gapps-verified.json`).
An incompatible or missing selected add-on fails startup without falling back.

**First activation requires fresh Android data.** Configure a separately
provisioned fresh `data_dir` (with its `data` and `metadata` directories), or enable
GApps before the installation's first Android boot. Existing data is never wiped.
The runtime records Google-app selection and signers in the cell's data directory;
updates with the same selection/signers refresh only Google parser caches.
Changing selection or signers requires fresh data or a separately validated
migration. To disable GApps, stop Droidloom, remove `gapps_dir` and select fresh
data or restore a pre-GApps backup. Merely removing the package cannot undo
Google updates and account state in `/data`; startup rejects that mixed state.

A separately validated migration must also reconcile existing applications'
permission state. An app installed before GApps may request Google-defined
normal permissions yet retain `granted=false` after those permissions appear.
On the desktop, reinstalling NTE's unchanged base APK with
`pm install -r -p com.hottagames.nte --user 0 <installed-base-apk-path>` retained
its existing splits and data and restored `CHECK_LICENSE`, allowing it to bind
to Play's licensing service. This does not establish a Play license or repair
catalog incompatibility. Do not substitute a blanket runtime-permission grant
or reset application data.

Offline checks do not prove account sign-in, Play Store installation, push
delivery or Play Integrity behavior. Validate those on the intended device,
including repeat boots and package changes, before considering the integration
ready for use. Google certification and redistribution rights are separate from
successful image assembly; see [third-party scope](../THIRD_PARTY.md).

Play Store also filters its catalog using Android's reported capabilities.
The vendor product declares touch and distinct multitouch provided by Droidloom's
native InputFlinger path, which preserves independent contact IDs in Android MotionEvents.
The framework includes that routed input when computing display
configuration, since physical input devices remain private to Denial. The Mesa
products advertise OpenGL ES 3.2 (`ro.opengles.version=196610`), matching the
verified Moto and x86_64 AMD rendering paths. Both window orientations are
declared for freeform Android tasks. When validating another graphics backend, compare
the advertised version with SurfaceFlinger's actual GLES implementation.

Vulkan features are detected at each `system_server` start through Android's
Vulkan loader, before PackageManager publishes its feature list. Hardware GPUs
with a graphics queue and ETC2 support provide `android.hardware.vulkan.version`
and `android.hardware.vulkan.level`; CPU renderers are excluded. The version is
bounded by both the loader and the GPU at the reported hardware level. Reporting
1.1 or newer also requires Android hardware-buffer, YCbCr and SYNC_FD interop;
otherwise the version is capped at 1.0.3. A failed probe leaves these features
absent. This detection does not declare Vulkan dEQP certification, compute levels
or Android Vulkan profile conformance. Compare `cmd package list features` with
`cmd gpu vkjson`; the latter can expose a higher native API version when Android
interop requirements limit the package feature. Rebuild and ship `services.jar`
and `libinputflinger.so` together.

From inside the Android cell, `cmd package list features` should include the
touch features and a nonzero `reqGlEsVersion`; `cmd activity get-config` should
report `finger` for a touch-enabled Droidloom product. A feature XML alone does
not correct the display's input configuration. After updating these boot-time
capabilities, restart the cell and refresh Play Store's cache. Successful account
sign-in does not guarantee that Google has refreshed its device profile or that
every app is compatible. Only declare capabilities the runtime implements;
Google certification and missing camera, microphone or sensor integration are
not repaired by adding feature names.

### Play Store catalog refresh

After changing Android capabilities, restarting the cell activates the new
feature report but can leave Play Store's cached compatibility result unchanged.
Use this sequence:

1. Restart the cell after deploying the matching framework artifacts, then
   verify the new features with `/system/bin/cmd package list features`.
2. In Android Settings, open **Apps → Google Play Store → Storage & cache** and
   select **Clear cache**. Verify that **Cache** reaches **0 bytes**. Preserve
   **User data**; **Clear storage** is a different operation and is not needed.
3. Force-stop the Store with
   `/system/bin/cmd activity force-stop com.android.vending`.
4. Reopen the affected app's Play Store listing. For Endfield:

   ```console
   /system/bin/cmd activity start --user 0 -a android.intent.action.VIEW -d 'market://details?id=com.gryphline.endfield.gp' -p com.android.vending
   ```

From an Android root/system context, step 2 can instead use
`/system/bin/cmd package clear --user 0 --cache-only com.android.vending`.
Plain ADB shell (UID 2000) lacks `android.permission.INTERNAL_DELETE_CACHE_FILES`
on the pinned Android 17 build. PackageManager logs that it is silently ignoring
the request, and the shell command waits indefinitely for a callback. Use the
Settings button in that case; Settings has the required permission. A hung
shell command does not establish that the cache was cleared.

On the OnePlus Pad 4, Endfield 1.5.3 required
`android.hardware.vulkan.version`, which Droidloom originally omitted. After
deploying the detected Vulkan capabilities, Android reported version `4210688`
(Vulkan 1.4) and hardware level `1`, but Play still reported incompatibility.
Clearing the Store cache from 12.05 MB to zero through Settings, force-stopping
the Store and reopening Endfield's listing changed it to **Install**; the user
confirmed the fix. Google sign-in and Store user data were retained. This
verifies catalog availability, not installation or gameplay.

### Endfield startup workaround

After the CPU-discovery fix described below, the user relaunched Endfield from
its normal launcher without `-force-gfx-direct` and confirmed it started.
Inspection found a fresh process, a standard launcher intent without extras,
three job workers and an active `UnityGfxDeviceW` thread. The historical flag
below is therefore no longer required for the tested startup/introduction path;
longer gameplay remains under validation.

On the OnePlus Pad 4, the Play-installed Endfield 1.5.3 (version code 50,
ARM64, Unity 2021.3.34f5) repeatedly crashed about three seconds after launch.
Its native traces showed a null-pointer access at `libunity.so+0xe5146c`
on `Job.Worker 0`, followed by another at `+0xa4eed8` on `UnityMain`.
An Endfield-only Vulkan trace and Khronos validation reproduced those failures
after successful instance/device creation, swapchain creation and all traced
resource/memory allocations, without a recorded validation error.

Passing Unity's `-force-gfx-direct` argument selects single-threaded rendering.
This passed startup with diagnostic layers disabled, and the user confirmed the
game started. To apply it from an Android shell, restart only the game:

```console
/system/bin/cmd activity force-stop com.gryphline.endfield.gp
/system/bin/cmd activity start --user 0 -n com.gryphline.endfield.gp/com.u8.sdk.U8UnityContext --es unity -force-gfx-direct
```

The extra is a per-launch argument and is not persisted by Droidloom's normal
launcher. No game data reset, APK patch or cell restart was needed for that
workaround. Single-threaded rendering may affect performance. Initially it
passed startup but did not fix gameplay loading: loading the saved server's
game state reproduced a separate allocation failure on 2026-10-14. A PID-filtered kernel
syscall trace captured `UnityMain` growing allocations through roughly 16, 32,
64, 128, 256, 512 and 1024 MiB, then passing length `0xffffffff80001000` to
`madvise` (returns `EINVAL`) and `mprotect` (returns `ENOMEM`). This is a malformed
unsigned length with a 32-bit sign-extension pattern, not evidence that the
host exhausted its RAM. The failure snapshot still had approximately 4.1 GiB
available, and the trace reported no dropped events or overruns.

The original game tombstone places the failure in
`UnsafeList<EntityQueryV2.BlockRange>.Realloc`, called by `GetBlockRanges` and
`RemoteFactoryGameWorldControllerV2.LateUpdateGameWorld`. Unity logs
“Commit memory failed” and raises `SIGTRAP`; CrashSight subsequently crashes
while building its report, producing a secondary `SIGSEGV`.

Function-entry probes subsequently measured `worker_count=0` in
`QueryResultWrapper.Wrapper`, followed by repeated empty-list growth. Droidloom's
synthetic `/sys` omitted `/sys/devices/system/cpu/{possible,present,online}`.
Bionic's `get_nprocs`/`get_nprocs_conf` therefore returned their fallback of one
CPU despite an eight-core affinity mask. Unity also reads per-CPU capacity and
maximum-frequency attributes when classifying workers.

The supervisor now projects those discovery masks, `cpuN/cpu_capacity`, and
`cpuN/cpufreq/cpuinfo_max_freq` as individual read-only binds. It exposes no CPU
hotplug, frequency-control or uevent files. Activate the matched supervisor tools
with a Droidloom-only restart. Verify `getconf _NPROCESSORS_ONLN` and
`getconf _NPROCESSORS_CONF` inside Android against the host CPU masks.
On the Pad 4 both now return 8. With the existing rendering argument retained,
Endfield created three job workers; all 198 gameplay queries in the bounded
probe sample received `worker_count=3` without the runaway reallocation. The user
confirmed the game loaded and continued its new-account introduction. This
validated the loading fix; a subsequent user-initiated normal launch also
passed startup without the rendering flag, as described above. Sustained
gameplay remains to be established. No game binaries, data, memory limits or
kernel settings were changed.

An earlier reproduction also exposed a separate whole-desktop GPU stall after
the game crashed. Logs recorded a GMU timeout and GPU translation fault;
Denial and SurfaceFlinger then waited indefinitely on GPU fences. The GPU
recovery worker and fault-capture IRQ stacks suggest a recovery lock cycle.
That freeze preceded the four-worker override test and temporary CPU projection,
so those attempts were not valid behavioral tests. Logs were saved, the user
restarted the tablet, and the successful CPU fix validation used the healthy
new boot. No GPU recovery fix has been applied in this change.
Evidence is in the Pad 4 workspace under
`out/droidloom-optimization-20261014/allocation-trace.jsonl`,
`allocation-failure-excerpt.log`, and `tomb_1792000119393.txt`.

A temporary `-force-gles30` launch instead showed
“Unable to initialize the Unity Engine Gfx API,” so it was not a working fallback.
