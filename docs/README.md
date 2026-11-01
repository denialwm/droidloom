# Droidloom documentation

- [Install Droidloom and APKs](INSTALL.md): package installation, first start and app management.
- [Build packages](BUILDING.md): prerequisites, Rust builder and package checks.
- [Known issues](KNOWN_ISSUES.md): preview limitations and workarounds.
- [Latency opportunities](latency-opportunities.md): where input and frames lose time, and what to try.

- [GPU forwarding](gpu-forwarding.md): Android's Vulkan on an NVIDIA host driver.
- [Architecture](architecture.md): Android cell, graphics path and source boundaries; [Linux host integration](architecture.md#host-service-integration) for audio. Telephony stays native, outside Droidloom.
- [Desktop integration](desktop-integration.md): windows, clipboard, notifications and diagnostics.
- [Native application integration proposal](native-app-integration.md): system-service backends and ordinary Linux application behavior.
- [Audio bridge and remaining work](audio-integration-proposal.md).
- [Security requirements](threat-model-v2.md): trust boundaries and isolation limitations.

## Integration contracts

- [Runtime layout](contracts/runtime-layout-v1.md): namespaces, mounts and lifecycle.
- [Private Binder](contracts/binderfs-v1.md): cell-local Binder devices.
- [Text input](contracts/text-input-v1.md): Android IME and Denial keyboard dismissal.
- [Navigation insets](contracts/navigation-insets.md): app geometry and host system bars.
- [Frame timeline](contracts/frame-timeline-v1.md): optional Denial protocol contract.

The private Android-to-host wire protocol is documented in [protocol/](../protocol/README.md).
Build and installation internals live with their code in
[tools/](../tools/README.md), [packaging/](../packaging/README.md) and
[android/](../android/README.md).
