# Build cache strategy

Droidloom keeps one persistent compiler workspace per build machine:

```text
.work/arch/
├── containers/          rootless Podman storage for the package builder
├── source-ARCH/         temporary repository snapshot mounted at /source
└── build/               the shared compiler workspace mounted at /build
    ├── aosp-source/     one pinned sparse AOSP checkout
    ├── android-out/     one Soong output tree
    ├── cargo/           one Cargo target directory
    ├── ccache/          native compiler cache
    ├── mesa-*/          prepared Mesa sources and tools
    └── ARCH/            what one architecture prepares for itself: the
                         extracted base system, init.rc, its makepkg folder
```

The package build, standalone full build, and focused native-bridge/cross build
all use `.work/arch/build` by default. A focused native-bridge build may receive
`--work` when it is running inside an already prepared external workspace, such
as `/build` in the package container. That override is a workspace selection,
not a second cache created beside the default one.

Normal and cross builds share the source checkout, Soong intermediates and host
compiler caches. The workspace lock prevents them from modifying that state at
the same time. Do not run two build modes concurrently. The package builds of
the two architectures are the exception: each holds the workspace lock only
while it prepares Mesa and compiles Android, and does the rest beside the other.

## What is persistent

Keep `aosp-source`, `android-out`, `cargo`, `ccache`, prepared base inputs and
Podman storage between builds. They are expensive to download or compile and
are updated in place when a pin or source input changes.

Final package archives remain under `dist/arch`. Translation benchmark results
remain under `.work/translation`; they are measurements, not compiler cache.
Device images and explicitly retained test artifacts may remain elsewhere under
`.work`, but build commands must not create another AOSP checkout or Soong output
tree there.

## Cleanup policy

Safe routine cleanup removes temporary package staging and superseded release
assembly directories. `--clean` removes compiler outputs only when a genuinely
clean rebuild is required; it deliberately retains downloaded source inputs.

Legacy `.work/update` and `.work/aosp-m2-source` directories are not part of the
current layout. After confirming `.work/arch/build/aosp-source` matches the
current source lock, they may be removed. Recovery/download caches created for a
one-off storage repair are also not part of the build workspace.

When changing this layout, migrate the valid cache first, verify its source
manifest against `android/manifest/m2-sparse-source-lock.json`, and only then
remove the superseded path. Never keep two independent `android-out` trees as a
fallback; preserve the compatible one and rebuild missing targets in place.
