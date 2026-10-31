/* SPDX-License-Identifier: GPL-3.0-or-later */
#include <array>
#include <algorithm>
#include <cassert>
#include <cstdint>
#include <cstring>
#include <fcntl.h>
#include <functional>
#include <map>
#include <mutex>
#include <set>
#include <sys/eventfd.h>
#include <sys/socket.h>
#include <thread>
#include <unistd.h>
#include <utility>
#include <vector>

// Only the Android RAII FD type needs a host substitute. The transport below
// is extracted from DroidloomLayers.cpp after applying the real patch chain.
namespace base {
class unique_fd {
    int fd = -1;
public:
    unique_fd() = default;
    explicit unique_fd(int value) : fd(value) {}
    ~unique_fd() { if (fd >= 0) close(fd); }
    unique_fd(const unique_fd&) = delete;
    unique_fd& operator=(const unique_fd&) = delete;
    unique_fd(unique_fd&& other) noexcept : fd(std::exchange(other.fd, -1)) {}
    unique_fd& operator=(unique_fd&& other) noexcept {
        if (fd >= 0) close(fd);
        fd = std::exchange(other.fd, -1);
        return *this;
    }
};
}
#include "DroidloomLayerTransport.inc"
#include "DroidloomTaskWork.h"

void workTests() {
    android::DroidloomTaskWork work;
    const auto gather = [&](bool reversed = false) {
        work.begin();
        for (uint64_t value : (reversed ? std::vector<uint64_t>{2, 1} : std::vector<uint64_t>{1, 2}))
            work.add(value);
    };
    gather();
    assert(work.needed(1, false));
    work.accept(1);
    gather();
    assert(!work.needed(1, false));
    assert(work.needed(1, true)); // Metadata/effects/front-buffer change.
    assert(work.needed(2, false)); // Configure/import/commit recovery.
    work.accept(1); // Completion racing with work leaves revision two pending.
    gather();
    assert(work.needed(2, false));
    work.accept(2);
    gather(true);
    assert(work.needed(2, false)); // Subtree/Z-order change.
    gather();
    assert(!work.needed(2, false)); // Failed work never updates accepted keys.
}

void batchTests() {
    for (uint32_t total : {1, 2, 16, 17, 64}) {
    for (bool fenced : {false, true}) {
        int sockets[2];
        assert(socketpair(AF_UNIX, SOCK_SEQPACKET | SOCK_CLOEXEC, 0, sockets) == 0);
        assert(fcntl(sockets[0], F_SETFL, O_NONBLOCK) == 0);
        SceneBatch batch;
        std::vector<int> originals;
        batch.begin();
        for (uint32_t id = 1; id <= total; ++id) {
            std::vector<uint8_t> layer;
            put64(layer, id);
            layer.resize(72);
            std::vector<int> fds;
            if (fenced) {
                int fd = eventfd(id, EFD_CLOEXEC | EFD_NONBLOCK);
                assert(fd >= 0);
                originals.push_back(fd);
                fds.push_back(fd);
            }
            assert(batch.add(sockets[0], layer, fds));
        }
        std::vector<uint8_t> commit;
        for (uint32_t word : {9u, 800u, 600u, total}) put32(commit, word);
        assert(batch.commit(sockets[0], 21, commit));
        for (int fd : originals) close(fd);
        const uint32_t perPacket = fenced ? 4 : 16;
        uint32_t next = 1;
        const uint32_t packets = (total + perPacket - 1) / perPacket;
        for (uint32_t packet = 0; packet < packets; ++packet) {
            std::array<uint8_t, 1260> bytes{};
            iovec iov{bytes.data(), bytes.size()};
            alignas(cmsghdr) std::array<uint8_t, CMSG_SPACE(4 * sizeof(int))> control{};
            msghdr msg{};
            msg.msg_iov = &iov; msg.msg_iovlen = 1;
            msg.msg_control = control.data(); msg.msg_controllen = control.size();
            const ssize_t size = recvmsg(sockets[1], &msg, MSG_CMSG_CLOEXEC);
            const bool final = packet + 1 == packets;
            const uint32_t chunk = std::min(perPacket, total - next + 1);
            assert(size == (final ? 44 : 20) + chunk * 76);
            assert(!(msg.msg_flags & (MSG_TRUNC | MSG_CTRUNC)));
            assert(u32(bytes.data() + 4) == (final ? 12u : 11u));
            assert(u32(bytes.data() + 12) == (fenced ? chunk : 0u));
            if (final) {
                assert(u32(bytes.data() + 16) == 21 && u32(bytes.data() + 20) == 0);
                assert(u32(bytes.data() + 24) == 9 && u32(bytes.data() + 36) == total);
            }
            const size_t offset = final ? 40 : 16;
            assert(u32(bytes.data() + offset) == chunk);
            auto* header = CMSG_FIRSTHDR(&msg);
            assert((header != nullptr) == fenced);
            std::array<int, 4> fds{};
            if (fenced) std::memcpy(fds.data(), CMSG_DATA(header), chunk * sizeof(int));
            for (uint32_t i = 0; i < chunk; ++i, ++next) {
                const auto* layer = bytes.data() + offset + 4 + i * 76;
                assert(u32(layer) == next);
                assert(u32(layer + 72) == (fenced ? i : UINT32_MAX));
                if (fenced) {
                    uint64_t value = 0;
                    assert(fcntl(fds[i], F_GETFD) & FD_CLOEXEC);
                    assert(read(fds[i], &value, sizeof(value)) == sizeof(value) && value == next);
                    close(fds[i]);
                }
            }
        }
        assert(next == total + 1);
        close(sockets[1]);
        close(sockets[0]);
    }
    }
}

void replyWords(int socket, uint32_t opcode, const std::vector<uint32_t>& words) {
    std::vector<uint8_t> bytes;
    for (auto word : words) put32(bytes, word);
    assert(send(socket, opcode, bytes));
}

void configurationTests() {
    ConfigCache cache;
    int repaints = 0;
    const auto repaint = [&] {
        // Callback may read the snapshot; the update must release its lock.
        assert(cache.snapshot()[2] == 800);
        ++repaints;
    };
    assert(configurationEvent(cache, {2, 0, 15, 10, 800, 600}, repaint));
    assert(repaints == 1);
    assert(configurationEvent(cache, {2, 0, 15, 10, 800, 600}, repaint));
    assert(configurationEvent(cache, {1, 0, 15, 9, 800, 600}, repaint));
    assert(repaints == 1);
    assert(!configurationEvent(cache, {2, 0, 0, 10, 800, 600}, repaint));
    assert(!configurationEvent(cache, {0, 0, 15, 10, 800, 600}, repaint));
    assert(!configurationEvent(cache, {3, 0, 15, 10}, repaint));
    assert(repaints == 1);
    // Visibility can change while Android's configure serial stays the same.
    assert(configurationEvent(cache, {3, 0, 0, 10, 800, 600}, repaint));
    assert(repaints == 2 && cache.snapshot()[0] == 0);

    int sockets[2];
    assert(socketpair(AF_UNIX, SOCK_SEQPACKET | SOCK_CLOEXEC, 0, sockets) == 0);
    ConfigCache current;
    bool subscribed = false;
    std::array<uint32_t, 4> config;
    std::thread host([&] {
        std::vector<uint32_t> request;
        assert(receive(sockets[1], 1, &request) && request.empty());
        replyWords(sockets[1], 0x8002, {15, 10, 800, 600});
        assert(receive(sockets[1], 8, &request) && request.empty());
        // Event receiver wins the race with the initial reply on the other FD.
        assert(current.update({2, 0, 15, 11, 1024, 768}));
        replyWords(sockets[1], 0x8005, {1, 0, 15, 10, 800, 600});
    });
    assert(!configuration(sockets[0], current, subscribed, 800, 600, &config));
    host.join();
    assert(subscribed && config[1] == 11);
    assert(fcntl(sockets[0], F_SETFL, O_NONBLOCK) == 0);
    for (int frame = 0; frame != 1000; ++frame)
        assert(configuration(sockets[0], current, subscribed, 1024, 768, &config));
    char byte;
    assert(recv(sockets[1], &byte, 1, MSG_DONTWAIT) == -1 && errno == EAGAIN);
    assert(current.update({3, 0, 0, 11, 1024, 768}));
    // Geometry remains available when the host cannot export direct layers.
    // Fallback can allocate the current size without borrowing a target first.
    assert(configurationSnapshot(sockets[0], current, subscribed, &config));
    assert(config[0] == 0 && config[2] == 1024 && config[3] == 768);
    assert(!configuration(sockets[0], current, subscribed, 1024, 768, &config));
    assert(current.update({4, 0, 15, 11, 1024, 768}));
    assert(configuration(sockets[0], current, subscribed, 1024, 768, &config));
    assert(recv(sockets[1], &byte, 1, MSG_DONTWAIT) == -1 && errno == EAGAIN);
    // A host without CONFIG_EVENTS still receives one Config per call.
    assert(fcntl(sockets[0], F_SETFL, 0) == 0);
    subscribed = false;
    std::thread legacy([&] {
        for (uint32_t serial = 12; serial != 14; ++serial) {
            std::vector<uint32_t> request;
            assert(receive(sockets[1], 1, &request) && request.empty());
            replyWords(sockets[1], 0x8002, {7, serial, 800, 600});
        }
    });
    for (uint32_t serial = 12; serial != 14; ++serial) {
        assert(configuration(sockets[0], current, subscribed, 800, 600, &config));
        assert(!subscribed && config[1] == serial);
    }
    legacy.join();
    // A host with PLACEMENT_GRID answers opcode 13 with its logical size too.
    ConfigCache placed;
    subscribed = false;
    std::thread grid([&] {
        std::vector<uint32_t> request;
        assert(receive(sockets[1], 1, &request) && request.empty());
        replyWords(sockets[1], 0x8002, {15 | 128, 20, 1216, 2640});
        assert(receive(sockets[1], 13, &request) && request.empty());
        replyWords(sockets[1], 0x8005, {1, 0, 15 | 128, 20, 1216, 2640, 608, 1320});
    });
    assert(configuration(sockets[0], placed, subscribed, 1216, 2640, &config));
    grid.join();
    assert(subscribed && (placed.logicalSize() == std::array<uint32_t, 2>{608, 1320}));
    assert(!placed.update({2, 0, 15 | 128, 20, 1216, 2640}));
    assert(placed.update({2, 0, 15 | 128, 21, 1216, 2640, 405, 880}));
    assert((placed.logicalSize() == std::array<uint32_t, 2>{405, 880}));
    assert((current.logicalSize() == std::array<uint32_t, 2>{0, 0}));
    // Firefox's video layer at y=281 sits between logical pixels at scale 2.
    assert(onLogicalGrid({0, 280, 1216, 2640}, {608, 1320}, 1216, 2640));
    assert(!onLogicalGrid({0, 281, 1216, 2640}, {608, 1320}, 1216, 2640));
    assert(!onLogicalGrid({-1, 0, 1216, 2640}, {608, 1320}, 1216, 2640));
    assert(onLogicalGrid({0, 281, 1216, 2640}, {0, 0}, 1216, 2640));
    close(sockets[0]);
    close(sockets[1]);
}

void commitTests() {
    CommitQueue queue;
    std::vector<uint64_t> rejected;
    bool repaint = false;
    bool unplaceable = false;
    assert(!queue.busy() && !queue.defer());
    auto first = queue.begin({91, 92});
    assert(first == 1 && queue.busy());
    assert(queue.begin({93}) == 0);
    assert(!queue.complete({2, 0, 0}, &rejected, &repaint));
    assert(!queue.complete({1, 0}, &rejected, &repaint));
    assert(!queue.complete({1, 0, 1}, &rejected, &repaint));
    assert(queue.busy());
    assert(queue.complete({1, 0, 0}, &rejected, &repaint));
    assert(rejected.empty() && !repaint && !queue.takeFallback(&unplaceable));
    assert(!queue.complete({1, 0, 0}, &rejected, &repaint));
    auto second = queue.begin({93});
    assert(second == 2);
    for (int frame = 0; frame < 1000; ++frame) assert(queue.defer());
    assert(queue.complete({2, 0, 0}, &rejected, &repaint));
    assert(rejected.empty() && repaint && !queue.busy());
    auto third = queue.begin({94, 95});
    assert(third == 3);
    assert(queue.complete({3, 0, static_cast<uint32_t>(-95)}, &rejected, &repaint));
    assert((rejected == std::vector<uint64_t>{94, 95}));
    assert(repaint && queue.takeFallback(&unplaceable) && unplaceable);
    assert(!queue.takeFallback(&unplaceable) && !unplaceable);
    // Only a rejected scene's new reads complete; accepted reads keep GPU gates.
    assert(std::find(rejected.begin(), rejected.end(), 91) == rejected.end());
    // A rejection of this scene alone falls back once and asks again.
    assert(queue.begin({96}) == 4);
    assert(queue.complete({4, 0, static_cast<uint32_t>(-11)}, &rejected, &repaint));
    assert(repaint && queue.takeFallback(&unplaceable) && !unplaceable);
    // The shape keeps placement and drops each layer's buffer and frame.
    const std::vector<uint64_t> scene{7, 800, 600, 1, 41, 9, 0, 281, 800, 600,
                                      0, 0, 0, 0, 0, 255, 0, 1, 0x34325258};
    std::vector<uint64_t> next = scene;
    next[4] = 42;
    next[5] = 10;
    assert(sceneShape(scene) == sceneShape(next) && sceneShape(scene) != scene);
    next[7] = 282;
    assert(sceneShape(scene) != sceneShape(next));

    int sockets[2];
    assert(socketpair(AF_UNIX, SOCK_SEQPACKET | SOCK_CLOEXEC, 0, sockets) == 0);
    assert(fcntl(sockets[0], F_SETFL, O_NONBLOCK) == 0);
    std::vector<uint8_t> payload;
    for (uint32_t v : {17, 800, 600, 64}) put32(payload, v);
    constexpr uint64_t id = UINT64_C(0x1234567800000001);
    assert(commitAsync(sockets[0], id, payload)); // No host reply exists yet.
    std::vector<uint32_t> words;
    assert(receive(sockets[1], 9, &words));
    assert((words == std::vector<uint32_t>{1, 0x12345678, 17, 800, 600, 64}));
    char byte;
    assert(recv(sockets[0], &byte, 1, MSG_DONTWAIT) == -1 && errno == EAGAIN);
    close(sockets[1]);
    assert(!commitAsync(sockets[0], id + 1, payload));
    close(sockets[0]);
}

void importTests() {
    ImportQueue queue;
    using Status = ImportQueue::Status;
    bool repaint = false;
    assert(queue.ready({}) && !queue.ready({100}));
    assert(queue.begin(0) == 0);
    std::vector<uint64_t> attempts;
    for (uint64_t buffer = 100; buffer < 116; ++buffer) {
        attempts.push_back(queue.begin(buffer));
        assert(attempts.back() != 0 && queue.status(buffer) == Status::Pending);
        assert(!queue.forget(buffer));
    }
    assert(queue.begin(116) == 0); // Batch is bounded, not just the total cache.
    assert(queue.begin(100) == 0);
    assert(!queue.complete({1, 0, 999, 0, 0}, &repaint));
    assert(!queue.complete({999, 0, 100, 0, 0}, &repaint));
    assert(!queue.complete({1, 0, 100, 0, 1}, &repaint));
    assert(!queue.complete({1, 0, 100, 0}, &repaint));
    assert(queue.status(100) == Status::Pending);
    // Complete in reverse order. Scene readiness requires every source, and
    // results coalesce into one repaint only when the batch drains.
    for (int i = 15; i >= 0; --i) {
        assert(!queue.ready({100, 115}));
        assert(queue.complete({static_cast<uint32_t>(attempts[i]), 0,
                               static_cast<uint32_t>(100 + i), 0, 0}, &repaint));
        assert(repaint == (i == 0));
    }
    assert(queue.ready({100, 115}));
    assert(!queue.complete({1, 0, 100, 0, 0}, &repaint)); // Duplicate.
    assert(queue.forget(100));
    auto replacement = queue.begin(100);
    assert(replacement > attempts.back());
    assert(!queue.complete({1, 0, 100, 0, 0}, &repaint)); // Stale prior attempt.
    assert(queue.status(100) == Status::Pending);
    assert(queue.complete({static_cast<uint32_t>(replacement), 0, 100, 0,
                           static_cast<uint32_t>(-95)}, &repaint));
    assert(repaint && queue.status(100) == Status::Failed && !queue.ready({100}));
    assert(queue.begin(100) == 0); // A failed import is not retried every frame.
    assert(queue.ready({115})); // Failure does not poison unrelated scenes.
    ImportQueue reconnect;
    assert(!reconnect.complete({static_cast<uint32_t>(replacement), 0, 100, 0, 0}, &repaint));
    for (uint64_t buffer = 116; buffer < 228; ++buffer) {
        auto attempt = queue.begin(buffer);
        assert(attempt != 0);
        assert(queue.complete({static_cast<uint32_t>(attempt), 0,
                               static_cast<uint32_t>(buffer), 0, 0}, &repaint));
    }
    assert(queue.begin(228) == 0); // Confirmed/failed cache entries are bounded too.
    assert(queue.forget(100));
    assert(queue.begin(228) != 0);

    int sockets[2];
    assert(socketpair(AF_UNIX, SOCK_SEQPACKET | SOCK_CLOEXEC, 0, sockets) == 0);
    assert(fcntl(sockets[0], F_SETFL, O_NONBLOCK) == 0);
    int plane = eventfd(0, EFD_CLOEXEC | EFD_NONBLOCK);
    assert(plane >= 0);
    for (uint64_t id = 1; id <= 16; ++id) {
        std::vector<uint8_t> payload;
        put64(payload, id + 100);
        payload.resize(48);
        assert(importBuffer(sockets[0], id, payload, {plane}));
    }
    // Every registration was sent before any reply. Receive the actual
    // descriptor-bearing wire records and verify attempt/allocation identities.
    for (uint64_t id = 1; id <= 16; ++id) {
        std::array<uint8_t, 128> bytes{};
        iovec iov{bytes.data(), bytes.size()};
        alignas(cmsghdr) std::array<uint8_t, CMSG_SPACE(sizeof(int))> control{};
        msghdr msg{};
        msg.msg_iov = &iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.data();
        msg.msg_controllen = control.size();
        assert(recvmsg(sockets[1], &msg, MSG_CMSG_CLOEXEC) == 72);
        assert(u32(bytes.data() + 4) == 10 && u32(bytes.data() + 12) == 1);
        assert(u32(bytes.data() + 16) == id && u32(bytes.data() + 24) == id + 100);
        auto* header = CMSG_FIRSTHDR(&msg);
        assert(header && header->cmsg_type == SCM_RIGHTS);
        int imported;
        std::memcpy(&imported, CMSG_DATA(header), sizeof(imported));
        assert(fcntl(imported, F_GETFD) & FD_CLOEXEC);
        close(imported);
    }
    char byte;
    assert(recv(sockets[0], &byte, 1, MSG_DONTWAIT) == -1 && errno == EAGAIN);
    assert(fcntl(sockets[0], F_SETFL, 0) == 0);
    for (bool accepted : {true, false}) {
        std::thread host([&] {
            std::vector<uint32_t> words;
            assert(receive(sockets[1], 2, &words));
            replyWords(sockets[1], 0x8001, {accepted ? 0u : static_cast<uint32_t>(-95)});
        });
        assert(importBuffer(sockets[0], 0, std::vector<uint8_t>(48), {}) == accepted);
        host.join();
    }
    close(plane);
    close(sockets[1]);
    assert(!importBuffer(sockets[0], 17, std::vector<uint8_t>(48), {}));
    close(sockets[0]);
}

int main() {
    workTests();
    batchTests();
    configurationTests();
    commitTests();
    importTests();
    int sockets[2];
    assert(socketpair(AF_UNIX, SOCK_SEQPACKET | SOCK_CLOEXEC, 0, sockets) == 0);
    // Nonblocking makes any accidental stage receive fail immediately. Queue
    // the entire 64-layer scene before the mock compositor sends any reply.
    assert(fcntl(sockets[0], F_SETFL, O_NONBLOCK) == 0);
    int event = eventfd(1, EFD_CLOEXEC | EFD_NONBLOCK);
    assert(event >= 0);
    for (uint64_t layer = 1; layer <= 64; ++layer) {
        std::vector<uint8_t> payload;
        put64(payload, layer);
        payload.resize(72);
        assert(stage(sockets[0], true, payload, {event}));
    }
    close(event);
    std::vector<uint8_t> commit;
    for (uint32_t value : {1, 800, 600, 64}) put32(commit, value);
    assert(send(sockets[0], 5, commit));
    for (uint64_t layer = 1; layer <= 64; ++layer) {
        std::array<uint8_t, 128> bytes{};
        iovec iov{bytes.data(), bytes.size()};
        alignas(cmsghdr) std::array<uint8_t, CMSG_SPACE(sizeof(int))> control{};
        msghdr msg{};
        msg.msg_iov = &iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.data();
        msg.msg_controllen = control.size();
        assert(recvmsg(sockets[1], &msg, MSG_CMSG_CLOEXEC) == 88);
        assert(msg.msg_flags == 0 || msg.msg_flags == MSG_CMSG_CLOEXEC);
        assert(u32(bytes.data() + 4) == 7 && u32(bytes.data() + 12) == 1);
        assert(u32(bytes.data() + 16) == layer);
        auto* header = CMSG_FIRSTHDR(&msg);
        assert(header && header->cmsg_type == SCM_RIGHTS);
        int imported;
        std::memcpy(&imported, CMSG_DATA(header), sizeof(imported));
        assert(fcntl(imported, F_GETFD) & FD_CLOEXEC);
        close(imported);
    }
    std::vector<uint32_t> words;
    assert(receive(sockets[1], 5, &words) && words.size() == 4 && words[3] == 64);
    // Both rejection and acceptance are consumed only at the explicit barrier.
    std::vector<uint8_t> rejected;
    put32(rejected, static_cast<uint32_t>(-95));
    assert(send(sockets[1], 0x8001, rejected));
    assert(receive(sockets[0], 0x8001, &words) && words[0] == static_cast<uint32_t>(-95));
    assert(fcntl(sockets[0], F_SETFL, 0) == 0);
    for (bool accepted : {true, false}) {
        std::thread host([&] {
            std::vector<uint32_t> request;
            assert(receive(sockets[1], 4, &request));
            std::vector<uint8_t> reply;
            put32(reply, accepted ? 0 : static_cast<uint32_t>(-95));
            assert(send(sockets[1], 0x8001, reply));
        });
        assert(stage(sockets[0], false, std::vector<uint8_t>(72), {}) == accepted);
        host.join();
    }
    close(sockets[1]);
    assert(!stage(sockets[0], true, std::vector<uint8_t>(72), {}));
    close(sockets[0]);
}
