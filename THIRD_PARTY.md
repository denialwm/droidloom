# License scope and upstream notices

Droidloom's original implementation, tools, configuration and documentation
are licensed under GPL-3.0-or-later, as granted in LICENSE. Original Droidloom
additions carried in patches use the same license. Existing upstream code
and copyright notices in the patched projects remain under their original
licenses; applying a patch does not give Droidloom ownership of that code.

## Retained licenses

- Teto (Translation Engine with Target Optimization), derived from
  Digitalis/Berberis, retains the Apache-2.0 notices of the Android Open Source
  Project and Digitalis author utzcoz. `android/manifest/native-bridge-lock.json`
  pins the source-built ARM64 translator in the
  [Teto repository](https://github.com/denialwm/teto),
  retaining its [DigitalisX64 upstream](https://github.com/DigitalisX64/platform_frameworks_libs_binary_translation)
  history. Translator fixes in that separate repository retain their per-file
  Apache-2.0 licensing; its Android support dependencies are
  pinned in the AOSP sparse-source lock. No proprietary translation binaries
  are used.
- Android/AOSP code, interfaces, base images and upstream build recipes retain
  their individual licenses, predominantly Apache-2.0 and BSD/MIT variants.
  The copied
  `android/device/droidloom_arm64/android.software.activities_on_secondary_displays.xml`
  retains its Android Open Source Project copyright and Apache-2.0 notice.
  The audio APEX recipe preserves the upstream Apache license alongside
  Droidloom's GPL-covered changes.
  The Java WindowManager policy adapter follows the pinned PhoneWindowManager
  window-type classification and framework lifecycle contracts; its retained
  AOSP portions carry their Apache-2.0 notice. The Rust policy/task engine, its
  narrow JNI string adapter, and original
  adapter additions are GPL-3.0-or-later.
- Mesa, minigbm, libdrm and other graphics dependencies retain their upstream
  licenses. Mesa's source tree includes tools under different licenses; its
  core library is MIT-licensed. Consult the individual upstream files and
  Mesa's license catalog rather than assigning one license to the whole tree.
- Cargo dependencies retain their published license expressions and notices.
  The audio broker also uses the host distribution's PipeWire/SPA development
  headers and shared library under their upstream terms. Its narrow C ABI adapter
  is original Droidloom code; PipeWire is not vendored or linked into Android.
  The Rust package workflow collects their supplied license/copyright files
  and an inventory from locked Cargo metadata. This inventory includes
  workspace build/test and platform dependencies as well as runtime libraries;
  inclusion in the inventory does not mean every crate is linked into each binary.
  Notices omitted from published crate archives are supplied, with provenance,
  under `packaging/licenses/cargo/` and copied into the inventory's
  `supplemental/` directories. r-efi's supplied AUTHORS file contains its
  MIT terms and copyright notices and is retained as well.
- These interface definitions retain their existing MIT terms:
  `protocol/denial-frame-timeline-v1.xml`,
  `graphics/droidloom-wayland/protocol/denial-text-input-panel-v1.xml`,
  `graphics/droidloom-wayland/protocol/denial-insets-v1.xml`, and
  `runtime/droidloom-devices/protocol/denial-fingerprint-verify-v1.xml`.
  Their copyright holders are recorded in the files. The interface license
  does not change the GPL license of Droidloom's implementation.
- Android applications installed by users, including WhatsApp, remain under
  their own terms. They are not part of the Droidloom source license.

The license texts in LICENSES are provided verbatim. Their presence does not
offer alternative licenses for Droidloom's GPL-covered implementation.

## Binary distribution

The optional `droidloom-gapps` workflow imports a user-supplied LiteGapps archive.
Google APKs retain their separate proprietary terms. LiteGapps' installer license
does not license Google's APKs. The workflow records source and APK hashes,
preserves signer identity and includes the supplied notices; it does not grant
redistribution rights or establish Google device certification. Keep these local
payloads out of source control and resolve distribution rights independently
before publishing a package containing them.

Both pacman packages include Droidloom's LICENSE, the full GPLv3 text and this
scope document under `/usr/share/licenses/<package>/`. The runtime package also
includes the Cargo dependency notices under `rust/`. The image package retains
the Android notice catalog and Mesa license material.

Distribute GPL-covered binaries with access to their complete corresponding
source under GPLv3 section 6, including the applicable upstream sources,
Droidloom modifications and build/install tooling. A source tag containing
only Droidloom's patches is not, by itself, the complete source for all covered
binaries. Upstream notices and source obligations must also be respected.
The pinned input manifests and build instructions identify the inputs used by
the build; release preparation must make the corresponding source available.

Earlier copies legitimately supplied under Apache-2.0 or MIT keep those grants.
This change does not revoke permissions already granted for those copies.
