/* Copyright 2026 Droidloom. SPDX-License-Identifier: GPL-3.0-or-later */
#pragma once

#include <net/if.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/socket.h>

#include <cstdint>
#include <string>
#include <vector>

#include <android-base/unique_fd.h>

namespace droidloom::wifi {

// The cell shows no /sys/class/net, so the link is read through the socket
// interface every network namespace has.
inline bool InterfaceExists(const std::string& name) {
    return if_nametoindex(name.c_str()) != 0;
}

inline std::vector<uint8_t> InterfaceMac(const std::string& name) {
    android::base::unique_fd fd(socket(AF_INET, SOCK_DGRAM | SOCK_CLOEXEC, 0));
    ifreq request{};
    strlcpy(request.ifr_name, name.c_str(), sizeof(request.ifr_name));
    if (fd < 0 || ioctl(fd, SIOCGIFHWADDR, &request) != 0) return {};
    const auto* address = reinterpret_cast<const uint8_t*>(request.ifr_hwaddr.sa_data);
    return {address, address + 6};
}

}  // namespace droidloom::wifi
