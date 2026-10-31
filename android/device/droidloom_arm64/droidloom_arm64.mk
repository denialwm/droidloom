# This product deliberately emits only the Droidloom vendor partition. The
# system, system_ext, and product partitions come from the pinned official
# Android CI artifact and are never rebuilt implicitly by this target.
PRODUCT_NAME := droidloom_arm64
PRODUCT_DEVICE := droidloom_arm64
PRODUCT_BRAND := Droidloom
PRODUCT_MODEL := Droidloom ARM64 cell
PRODUCT_MANUFACTURER := Droidloom

PRODUCT_SOONG_NAMESPACES += vendor/droidloom

PRODUCT_BUILD_SYSTEM_IMAGE := false
PRODUCT_BUILD_SYSTEM_OTHER_IMAGE := false
PRODUCT_BUILD_SYSTEM_EXT_IMAGE := false
PRODUCT_BUILD_PRODUCT_IMAGE := false
PRODUCT_BUILD_ODM_IMAGE := false
PRODUCT_BUILD_VENDOR_IMAGE := true
PRODUCT_BUILD_VENDOR_DLKM_IMAGE := false
PRODUCT_BUILD_ODM_DLKM_IMAGE := false
PRODUCT_BUILD_SYSTEM_DLKM_IMAGE := false
PRODUCT_BUILD_CACHE_IMAGE := false
PRODUCT_BUILD_RAMDISK_IMAGE := false
PRODUCT_BUILD_USERDATA_IMAGE := false
PRODUCT_BUILD_RECOVERY_IMAGE := false
PRODUCT_BUILD_BOOT_IMAGE := false
PRODUCT_BUILD_INIT_BOOT_IMAGE := false
PRODUCT_BUILD_DEBUG_BOOT_IMAGE := false
PRODUCT_BUILD_VENDOR_BOOT_IMAGE := false
PRODUCT_BUILD_VENDOR_KERNEL_BOOT_IMAGE := false
PRODUCT_BUILD_DEBUG_VENDOR_BOOT_IMAGE := false
PRODUCT_BUILD_VBMETA_IMAGE := false
PRODUCT_BUILD_SUPER_EMPTY_IMAGE := false

PRODUCT_SHIPPING_API_LEVEL := $(PLATFORM_SDK_VERSION)

# These upstream minigbm modules carry their own Allocator V3 / Mapper V5 init
# and VINTF fragments. Droidloom's Composer owns no physical display or card
# node: it exports one logical display per task to the authenticated Denial
# endpoint supplied by the enclosing runtime.
PRODUCT_PACKAGES += \
    vendor_compatibility_matrix.xml \
    selinux_policy_vendor \
    libdroidloom_selinux_compat \
    android.hardware.security.keymint-service.nonsecure \
    android.hardware.health-service.example \
    android.hardware.power-service.droidloom \
    com.android.hardware.audio.droidloom \
    DroidloomFrameworkDisplayOverlay \
    DroidloomConnectivityOverlay \
    DroidloomSettingsOverlay \
    android.hardware.ethernet.prebuilt.xml \
    android.hardware.graphics.composer3-service.droidloom \
    droidloom-framework-init \
    droidloom-lmkd-compat \
    droidloom-classpath-wrapper \
    android.hardware.graphics.allocator-service.minigbm \
    mapper.minigbm \
    libgallium_dri \
    libEGL_mesa \
    libGLESv1_CM_mesa \
    libGLESv2_mesa \
    vulkan.freedreno

# Audio is intentionally non-networked and non-hardware-backed at this stage.
# The upstream AIDL example service provides Android's required primary module,
# while BUS endpoints select its timing-correct stub streams and never open a
# host ALSA device.
PRODUCT_COPY_FILES += \
    vendor/droidloom/android/device/droidloom_arm64/android.hardware.droidloom_input.xml:$(TARGET_COPY_OUT_VENDOR)/etc/permissions/android.hardware.droidloom_input.xml \
    vendor/droidloom/android/device/droidloom_arm64/android.software.activities_on_secondary_displays.xml:$(TARGET_COPY_OUT_VENDOR)/etc/permissions/android.software.activities_on_secondary_displays.xml \
    vendor/droidloom/android/device/droidloom_arm64/android.software.app_widgets.xml:$(TARGET_COPY_OUT_VENDOR)/etc/permissions/android.software.app_widgets.xml \
    frameworks/native/data/etc/android.software.freeform_window_management.xml:$(TARGET_COPY_OUT_VENDOR)/etc/permissions/android.software.freeform_window_management.xml \
    vendor/droidloom/android/device/droidloom_arm64/android.hardware.droidloom_app_compatibility.xml:$(TARGET_COPY_OUT_VENDOR)/etc/permissions/android.hardware.droidloom_app_compatibility.xml \
    vendor/droidloom/android/device/droidloom_arm64/audio_policy_configuration.xml:$(TARGET_COPY_OUT_VENDOR)/etc/audio_policy_configuration.xml \
    vendor/droidloom/android/device/droidloom_arm64/droidloom_primary_audio_policy_configuration.xml:$(TARGET_COPY_OUT_VENDOR)/etc/droidloom_primary_audio_policy_configuration.xml \
    frameworks/av/services/audiopolicy/config/audio_policy_volumes.xml:$(TARGET_COPY_OUT_VENDOR)/etc/audio_policy_volumes.xml \
    frameworks/av/services/audiopolicy/config/default_volume_tables.xml:$(TARGET_COPY_OUT_VENDOR)/etc/default_volume_tables.xml \
    frameworks/av/services/audiopolicy/config/surround_sound_configuration_5_0.xml:$(TARGET_COPY_OUT_VENDOR)/etc/surround_sound_configuration_5_0.xml

# Android's loaders derive these exact filenames from the properties:
# lib{EGL,GLESv1_CM,GLESv2}_mesa.so and vulkan.freedreno.so.
# The ARM64 Mesa rendering path supports GLES 3.2 (0x00030002 = 196610).
# The 64-bit-only base has no dex2oat32, so ART must select dex2oat64 for
# installation and background optimization, as core_64_bit_only.mk does.
# HWUI reports each frame's work to a hint session, which Droidloom's power
# service turns into utilization floors for the UI thread and RenderThread.
# SurfaceFlinger schedules by work durations. Its own stays one period minus
# 1 ms, which Composer's vsync relies on, and starts up to 3 ms earlier while
# it composes tasks on the GPU; apps wake 9.33 ms before SF instead of 15.67 ms,
# and 15.67 ms during early (transition) and GPU scheduling.
# Android's stream volumes start at full scale; the host mixer owns loudness.
# Codec2 clients use the AIDL software store: the cell has no hwservicemanager,
# so the default HIDL selection leaves apps with the in-process audio codecs.
PRODUCT_VENDOR_PROPERTIES += \
    debug.hwui.use_hint_manager=true \
    media.c2.hal.selection=aidl \
    debug.sf.use_phase_offsets_as_durations=1 \
    debug.sf.late.app.duration=9333333 \
    debug.sf.early.app.duration=15666666 \
    debug.sf.earlyGl.app.duration=15666666 \
    persist.audio.normal_playback_period_ms=5 \
    ro.config.media_vol_default=15 \
    ro.config.vc_call_vol_default=15 \
    ro.config.alarm_vol_default=7 \
    ro.config.system_vol_default=7 \
    ro.zygote=zygote64 \
    dalvik.vm.dex2oat64.enabled=true \
    ro.vendor.droidloom.surfaceflinger_direct=true \
    ro.vendor.droidloom.surfaceflinger_tasks=true \
    ro.hardware.egl=mesa \
    ro.hardware.vulkan=freedreno \
    ro.opengles.version=196610 \
    dalvik.vm.heapstartsize=16m \
    dalvik.vm.heapgrowthlimit=256m \
    dalvik.vm.heapsize=512m \
    dalvik.vm.heaptargetutilization=0.75 \
    dalvik.vm.heapminfree=512k \
    dalvik.vm.heapmaxfree=8m
