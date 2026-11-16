/* Copyright 2026 Droidloom. SPDX-License-Identifier: GPL-3.0-or-later */
#pragma once

#include <array>
#include <cstdint>

// The one access point Android's Wi-Fi reaches: the cell's link to the host.
// wificond lists it in every scan and the supplicant joins it, so both read
// these values and Android sees one network that agrees with itself.
namespace droidloom::wifi {

inline constexpr char kInterface[] = "wlan0";
inline constexpr char kSsid[] = "Droidloom";
// Locally administered, so it never names real hardware.
inline constexpr std::array<uint8_t, 6> kBssid = {0x02, 0x64, 0x6c, 0x00, 0x00, 0x01};
// Channel 36: 5 GHz, non-DFS, allowed in every regulatory domain.
inline constexpr int32_t kFrequencyMhz = 5180;
inline constexpr int32_t kRssiDbm = -45;
// Wi-Fi 6 at 80 MHz with two spatial streams.
inline constexpr int32_t kLinkSpeedMbps = 1201;
inline constexpr int32_t kChannelWidthMhz = 80;
inline constexpr int32_t kSpatialStreams = 2;

}  // namespace droidloom::wifi
