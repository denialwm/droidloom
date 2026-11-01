# Vendor-only x86_64 product paired with the pinned official x86_64 Android CI
# system, system_ext, and product partitions.
PRODUCT_NAME := droidloom_x86_64
PRODUCT_DEVICE := droidloom_x86_64
PRODUCT_BRAND := Droidloom
PRODUCT_MODEL := Droidloom x86_64 cell
PRODUCT_MANUFACTURER := Droidloom

PRODUCT_SOONG_NAMESPACES += vendor/droidloom

$(call inherit-product, vendor/droidloom/android/native-bridge/product.mk)

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
    libgbm_mesa \
    dri_gbm \
    radeonsi_dri \
    libgallium_dri \
    libEGL_mesa \
    libGLESv1_CM_mesa \
    libGLESv2_mesa \
    vulkan.radeon \
    vulkan.droidloom

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

# HWUI reports each frame's work to a hint session, which Droidloom's power
# service turns into utilization floors for the UI thread and RenderThread.
# Android's stream volumes start at full scale; the host mixer owns loudness.
# Codec2 clients use the AIDL software store: the cell has no hwservicemanager,
# so the default HIDL selection leaves apps with the in-process audio codecs.
PRODUCT_VENDOR_PROPERTIES += \
    debug.hwui.use_hint_manager=true \
    media.c2.hal.selection=aidl \
    persist.audio.normal_playback_period_ms=5 \
    ro.config.media_vol_default=15 \
    ro.config.vc_call_vol_default=15 \
    ro.config.alarm_vol_default=7 \
    ro.config.system_vol_default=7 \
    ro.zygote=zygote64 \
    ro.vendor.droidloom.surfaceflinger_direct=true \
    ro.vendor.droidloom.surfaceflinger_tasks=true \
    ro.opengles.version=196610 \
    ro.hardware.egl=mesa \
    ro.hardware.vulkan=radeon \
    dalvik.vm.heapstartsize=16m \
    dalvik.vm.heapgrowthlimit=256m \
    dalvik.vm.heapsize=512m \
    dalvik.vm.heaptargetutilization=0.75 \
    dalvik.vm.heapminfree=512k \
    dalvik.vm.heapmaxfree=8m
