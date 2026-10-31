# Droidloom pacman packaging

User-facing instructions are maintained in:

- [Installation and APKs](../../docs/INSTALL.md)
- [Building packages](../../docs/BUILDING.md)
- [Known issues](../../docs/KNOWN_ISSUES.md)

`version.json` defines the matching package release for both architectures,
x86_64 and aarch64. `PKGBUILD.in` calls the Rust builder, and `Containerfile`
supplies its rootless Arch environment, including the aarch64 cross compiler. The runtime
and image packages must be installed together. Lifecycle hooks stop the runtime
before replacement or removal and preserve Android data.

Both packages include Droidloom's GPL notice and license text under
`/usr/share/licenses/<package>/`. The runtime includes Cargo dependency notices;
the image includes Android and Mesa notices. See [THIRD_PARTY.md](../../THIRD_PARTY.md)
for license scope and corresponding-source requirements.

`droidloom-gapps` is a separate, explicitly enabled optional package built from
a locally supplied archive. See [optional Google apps](../../docs/BUILDING.md#optional-google-apps).
Managed add-on packages depend on the exact runtime/image release and stop
Droidloom before upgrade or removal. The standalone ARM developer package has
no dependency on the x86_64 package workflow and requires an explicit stop
before installation, replacement or removal. Neither form starts Android or
changes its data automatically.
