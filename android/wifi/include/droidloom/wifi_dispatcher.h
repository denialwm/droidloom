/* Copyright 2026 Droidloom. SPDX-License-Identifier: GPL-3.0-or-later */
#pragma once

#include <chrono>
#include <condition_variable>
#include <functional>
#include <map>
#include <mutex>
#include <thread>

namespace droidloom::wifi {

// Runs Wi-Fi events after the Binder call that caused them has replied, in
// the order they were posted. A radio answers the same way: the call starts
// the work, a callback reports it.
class Dispatcher {
  public:
    static Dispatcher& Get() {
        static Dispatcher* instance = new Dispatcher();
        return *instance;
    }

    void Post(std::chrono::milliseconds delay, std::function<void()> work) {
        std::lock_guard lock(mutex_);
        // Equal times run in posting order: multimap inserts after equal keys.
        tasks_.emplace(std::chrono::steady_clock::now() + delay, std::move(work));
        wake_.notify_one();
    }

  private:
    Dispatcher() : thread_([this] { Run(); }) {}

    void Run() {
        std::unique_lock lock(mutex_);
        for (;;) {
            if (tasks_.empty()) {
                wake_.wait(lock);
                continue;
            }
            auto next = tasks_.begin();
            if (std::chrono::steady_clock::now() < next->first) {
                wake_.wait_until(lock, next->first);
                continue;
            }
            auto work = std::move(next->second);
            tasks_.erase(next);
            lock.unlock();
            work();
            lock.lock();
        }
    }

    std::mutex mutex_;
    std::condition_variable wake_;
    std::multimap<std::chrono::steady_clock::time_point, std::function<void()>> tasks_;
    std::thread thread_;
};

}  // namespace droidloom::wifi
