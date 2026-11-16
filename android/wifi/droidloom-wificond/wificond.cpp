/* Copyright 2026 Droidloom. SPDX-License-Identifier: GPL-3.0-or-later */
// Droidloom's wificond: the framework's Wi-Fi control service for a cell whose
// wlan0 is a plain link to the host. It answers the same Binder interface as
// AOSP wificond, with one access point in every scan, a signal that does not
// change, and no radio to drive. Interface state, addresses and routes stay
// with netd and IpClient, as on any phone.

#include <time.h>
#include <unistd.h>

#include <algorithm>
#include <chrono>
#include <map>
#include <mutex>
#include <optional>
#include <string>
#include <vector>

#include <android-base/file.h>
#include <android-base/logging.h>
#include <android-base/parseint.h>
#include <android-base/strings.h>
#include <android/net/wifi/nl80211/BnClientInterface.h>
#include <android/net/wifi/nl80211/BnWifiScannerImpl.h>
#include <android/net/wifi/nl80211/BnWificond.h>
#include <android/net/wifi/nl80211/IInterfaceEventCallback.h>
#include <android/net/wifi/nl80211/IPnoScanEvent.h>
#include <android/net/wifi/nl80211/IScanEvent.h>
#include <android/net/wifi/nl80211/ISendMgmtFrameEvent.h>
#include <android/net/wifi/nl80211/IWificondEventCallback.h>
#include <binder/IPCThreadState.h>
#include <binder/IServiceManager.h>
#include <binder/ProcessState.h>

#include "droidloom/wifi_access_point.h"
#include "droidloom/wifi_dispatcher.h"
#include "wificond/device_wiphy_capabilities.h"
#include "wificond/ipc_constants.h"
#include "wificond/scanning/pno_settings.h"
#include "wificond/scanning/scan_result.h"
#include "wificond/scanning/single_scan_settings.h"

namespace {

using android::IBinder;
using android::IInterface;
using android::sp;
using android::binder::Status;
using android::net::wifi::nl80211::BnClientInterface;
using android::net::wifi::nl80211::BnWifiScannerImpl;
using android::net::wifi::nl80211::BnWificond;
using android::net::wifi::nl80211::DeviceWiphyCapabilities;
using android::net::wifi::nl80211::IApInterface;
using android::net::wifi::nl80211::IClientInterface;
using android::net::wifi::nl80211::IInterfaceEventCallback;
using android::net::wifi::nl80211::IPnoScanEvent;
using android::net::wifi::nl80211::IScanEvent;
using android::net::wifi::nl80211::ISendMgmtFrameEvent;
using android::net::wifi::nl80211::IWifiScannerImpl;
using android::net::wifi::nl80211::IWificondEventCallback;
using android::net::wifi::nl80211::NativeScanResult;
using android::net::wifi::nl80211::PnoSettings;
using android::net::wifi::nl80211::SingleScanSettings;
namespace ap = droidloom::wifi;

// A scan takes this long on a real radio. Reporting completion after the
// call returns keeps the framework's scan state machine on its usual path.
constexpr auto kScanDuration = std::chrono::milliseconds(50);
// PNO reports a match once the access point is "seen" by a background scan.
constexpr auto kPnoMatchDelay = std::chrono::milliseconds(500);
constexpr uint16_t kCapabilityEss = 0x0001;

std::string InterfacePath(const std::string& name, const std::string& leaf) {
    return "/sys/class/net/" + name + "/" + leaf;
}

bool InterfaceExists(const std::string& name) {
    return access(InterfacePath(name, "ifindex").c_str(), F_OK) == 0;
}

std::vector<uint8_t> InterfaceMac(const std::string& name) {
    std::string text;
    std::vector<uint8_t> mac;
    if (!android::base::ReadFileToString(InterfacePath(name, "address"), &text)) return mac;
    for (const auto& part : android::base::Split(android::base::Trim(text), ":")) {
        uint8_t byte;
        if (!android::base::ParseUint("0x" + part, &byte)) return {};
        mac.push_back(byte);
    }
    return mac;
}

int32_t InterfaceCounter(const std::string& name, const char* counter) {
    std::string text;
    int64_t value = 0;
    android::base::ReadFileToString(InterfacePath(name, std::string("statistics/") + counter),
                                    &text);
    android::base::ParseInt(android::base::Trim(text), &value);
    return static_cast<int32_t>(value);
}

uint64_t BoottimeMicros() {
    timespec now{};
    clock_gettime(CLOCK_BOOTTIME, &now);
    return static_cast<uint64_t>(now.tv_sec) * 1000000 + now.tv_nsec / 1000;
}

// The access point as a scan reports it, seen just now.
NativeScanResult AccessPointScanResult() {
    std::vector<uint8_t> ssid(ap::kSsid, ap::kSsid + sizeof(ap::kSsid) - 1);
    // SSID, then the mandatory OFDM rates of a 5 GHz access point.
    std::vector<uint8_t> elements = {0, static_cast<uint8_t>(ssid.size())};
    elements.insert(elements.end(), ssid.begin(), ssid.end());
    elements.insert(elements.end(), {1, 8, 0x8c, 0x12, 0x98, 0x24, 0xb0, 0x48, 0x60, 0x6c});
    NativeScanResult result;
    result.ssid = ssid;
    std::copy(ap::kBssid.begin(), ap::kBssid.end(), result.bssid.begin());
    result.info_element = elements;
    result.frequency = ap::kFrequencyMhz;
    result.signal_mbm = ap::kRssiDbm * 100;
    result.tsf = BoottimeMicros();
    result.capability = kCapabilityEss;
    result.associated = false;
    return result;
}

bool PnoLooksFor(const PnoSettings& settings) {
    const std::vector<uint8_t> ssid(ap::kSsid, ap::kSsid + sizeof(ap::kSsid) - 1);
    return std::any_of(settings.pno_networks_.begin(), settings.pno_networks_.end(),
                       [&](const auto& network) { return network.ssid_ == ssid; });
}

class Scanner : public BnWifiScannerImpl {
  public:
    Status getScanResults(std::vector<NativeScanResult>* out) override {
        out->assign(1, AccessPointScanResult());
        return Status::ok();
    }
    Status getPnoScanResults(std::vector<NativeScanResult>* out) override {
        return getScanResults(out);
    }
    Status getMaxSsidsPerScan(int32_t* out) override {
        *out = 4;
        return Status::ok();
    }
    Status scan(const SingleScanSettings&, bool* out) override {
        *out = true;
        ScheduleScanResult();
        return Status::ok();
    }
    Status scanRequest(const SingleScanSettings&, int32_t* out) override {
        *out = IWifiScannerImpl::SCAN_STATUS_SUCCESS;
        ScheduleScanResult();
        return Status::ok();
    }
    Status subscribeScanEvents(const sp<IScanEvent>& handler) override {
        std::lock_guard lock(mutex_);
        scan_handler_ = handler;
        return Status::ok();
    }
    Status unsubscribeScanEvents() override {
        std::lock_guard lock(mutex_);
        scan_handler_.clear();
        return Status::ok();
    }
    Status subscribePnoScanEvents(const sp<IPnoScanEvent>& handler) override {
        std::lock_guard lock(mutex_);
        pno_handler_ = handler;
        return Status::ok();
    }
    Status unsubscribePnoScanEvents() override {
        std::lock_guard lock(mutex_);
        pno_handler_.clear();
        return Status::ok();
    }
    Status startPnoScan(const PnoSettings& settings, bool* out) override {
        *out = true;
        std::lock_guard lock(mutex_);
        const uint64_t generation = ++pno_generation_;
        if (!PnoLooksFor(settings)) return Status::ok();
        sp<Scanner> self = this;
        ap::Dispatcher::Get().Post(kPnoMatchDelay, [self, generation] {
            sp<IPnoScanEvent> handler;
            {
                std::lock_guard lock(self->mutex_);
                if (self->pno_generation_ != generation) return;
                handler = self->pno_handler_;
            }
            if (handler != nullptr) handler->OnPnoNetworkFound();
        });
        return Status::ok();
    }
    Status stopPnoScan(bool* out) override {
        std::lock_guard lock(mutex_);
        ++pno_generation_;
        *out = true;
        return Status::ok();
    }
    Status abortScan() override { return Status::ok(); }

  private:
    void ScheduleScanResult() {
        sp<Scanner> self = this;
        ap::Dispatcher::Get().Post(kScanDuration, [self] {
            sp<IScanEvent> handler;
            {
                std::lock_guard lock(self->mutex_);
                handler = self->scan_handler_;
            }
            if (handler != nullptr) handler->OnScanResultReady();
        });
    }

    std::mutex mutex_;
    sp<IScanEvent> scan_handler_;
    sp<IPnoScanEvent> pno_handler_;
    // Stopping or replacing a PNO scan cancels the match it scheduled.
    uint64_t pno_generation_ = 0;
};

class ClientInterface : public BnClientInterface {
  public:
    explicit ClientInterface(std::string name) : name_(std::move(name)) {}

    Status getPacketCounters(std::vector<int32_t>* out) override {
        *out = {InterfaceCounter(name_, "tx_packets"), InterfaceCounter(name_, "tx_errors")};
        return Status::ok();
    }
    // RSSI, transmit rate, frequency and receive rate, as wificond reports them.
    Status signalPoll(std::vector<int32_t>* out) override {
        *out = {ap::kRssiDbm, ap::kLinkSpeedMbps, ap::kFrequencyMhz, ap::kLinkSpeedMbps};
        return Status::ok();
    }
    Status getMacAddress(std::vector<uint8_t>* out) override {
        *out = InterfaceMac(name_);
        return Status::ok();
    }
    Status getInterfaceName(std::string* out) override {
        *out = name_;
        return Status::ok();
    }
    Status getWifiScannerImpl(sp<IWifiScannerImpl>* out) override {
        *out = scanner_;
        return Status::ok();
    }
    Status SendMgmtFrame(const std::vector<uint8_t>&, const sp<ISendMgmtFrameEvent>& callback,
                         int32_t) override {
        // There is no air to send a frame on.
        callback->OnFailure(ISendMgmtFrameEvent::SEND_MGMT_FRAME_ERROR_UNKNOWN);
        return Status::ok();
    }

  private:
    const std::string name_;
    const sp<Scanner> scanner_ = sp<Scanner>::make();
};

class Wificond : public BnWificond {
  public:
    Status createApInterface(const std::string&, sp<IApInterface>* out) override {
        *out = nullptr;
        return Status::ok();
    }
    Status createClientInterface(const std::string& name, sp<IClientInterface>* out) override {
        *out = nullptr;
        if (name != ap::kInterface || !InterfaceExists(name)) {
            LOG(ERROR) << "No client interface " << name;
            return Status::ok();
        }
        std::vector<sp<IInterfaceEventCallback>> callbacks;
        {
            std::lock_guard lock(mutex_);
            auto& client = clients_[name];
            if (client == nullptr) client = sp<ClientInterface>::make(name);
            *out = client;
            callbacks = interface_callbacks_;
        }
        for (const auto& callback : callbacks) callback->OnClientInterfaceReady(*out);
        LOG(INFO) << "Client interface " << name << " ready";
        return Status::ok();
    }
    Status tearDownApInterface(const std::string&, bool* out) override {
        *out = false;
        return Status::ok();
    }
    Status tearDownClientInterface(const std::string& name, bool* out) override {
        sp<ClientInterface> client;
        std::vector<sp<IInterfaceEventCallback>> callbacks;
        {
            std::lock_guard lock(mutex_);
            auto found = clients_.find(name);
            if (found != clients_.end()) {
                client = found->second;
                clients_.erase(found);
            }
            callbacks = interface_callbacks_;
        }
        *out = client != nullptr;
        if (client != nullptr) {
            for (const auto& callback : callbacks) callback->OnClientTorndownEvent(client);
        }
        return Status::ok();
    }
    Status tearDownInterfaces() override {
        std::vector<std::string> names;
        {
            std::lock_guard lock(mutex_);
            for (const auto& [name, client] : clients_) names.push_back(name);
        }
        bool removed;
        for (const auto& name : names) tearDownClientInterface(name, &removed);
        return Status::ok();
    }
    Status GetClientInterfaces(std::vector<sp<IBinder>>* out) override {
        std::lock_guard lock(mutex_);
        out->clear();
        for (const auto& [name, client] : clients_) out->push_back(IInterface::asBinder(client));
        return Status::ok();
    }
    Status GetApInterfaces(std::vector<sp<IBinder>>* out) override {
        out->clear();
        return Status::ok();
    }
    Status getAvailable2gChannels(std::optional<std::vector<int32_t>>* out) override {
        std::vector<int32_t> channels;
        for (int32_t frequency = 2412; frequency <= 2462; frequency += 5) {
            channels.push_back(frequency);
        }
        *out = channels;
        return Status::ok();
    }
    Status getAvailable5gNonDFSChannels(std::optional<std::vector<int32_t>>* out) override {
        *out = std::vector<int32_t>{5180, 5200, 5220, 5240, 5745, 5765, 5785, 5805, 5825};
        return Status::ok();
    }
    Status getAvailableDFSChannels(std::optional<std::vector<int32_t>>* out) override {
        *out = std::vector<int32_t>{};
        return Status::ok();
    }
    Status getAvailable6gChannels(std::optional<std::vector<int32_t>>* out) override {
        *out = std::vector<int32_t>{};
        return Status::ok();
    }
    Status getAvailable60gChannels(std::optional<std::vector<int32_t>>* out) override {
        *out = std::vector<int32_t>{};
        return Status::ok();
    }
    Status RegisterCallback(const sp<IInterfaceEventCallback>& callback) override {
        std::lock_guard lock(mutex_);
        Add(interface_callbacks_, callback);
        return Status::ok();
    }
    Status UnregisterCallback(const sp<IInterfaceEventCallback>& callback) override {
        std::lock_guard lock(mutex_);
        Remove(interface_callbacks_, callback);
        return Status::ok();
    }
    Status registerWificondEventCallback(const sp<IWificondEventCallback>& callback) override {
        std::lock_guard lock(mutex_);
        Add(event_callbacks_, callback);
        return Status::ok();
    }
    Status unregisterWificondEventCallback(const sp<IWificondEventCallback>& callback) override {
        std::lock_guard lock(mutex_);
        Remove(event_callbacks_, callback);
        return Status::ok();
    }
    Status getDeviceWiphyCapabilities(const std::string& name,
                                      std::optional<DeviceWiphyCapabilities>* out) override {
        if (name != ap::kInterface) {
            out->reset();
            return Status::ok();
        }
        DeviceWiphyCapabilities capabilities;
        capabilities.is80211nSupported_ = true;
        capabilities.is80211acSupported_ = true;
        capabilities.is80211axSupported_ = true;
        capabilities.maxTxStreams_ = ap::kSpatialStreams;
        capabilities.maxRxStreams_ = ap::kSpatialStreams;
        *out = capabilities;
        return Status::ok();
    }
    // The framework does not wait for a regulatory domain event
    // (config_wifiDriverSupportedNl80211RegChangedEvent), and the access point
    // is on a channel every domain allows.
    Status notifyCountryCodeChanged() override { return Status::ok(); }

  private:
    template <typename T>
    static void Add(std::vector<sp<T>>& callbacks, const sp<T>& callback) {
        if (std::none_of(callbacks.begin(), callbacks.end(), [&](const sp<T>& known) {
                return IInterface::asBinder(known) == IInterface::asBinder(callback);
            })) {
            callbacks.push_back(callback);
        }
    }
    template <typename T>
    static void Remove(std::vector<sp<T>>& callbacks, const sp<T>& callback) {
        std::erase_if(callbacks, [&](const sp<T>& known) {
            return IInterface::asBinder(known) == IInterface::asBinder(callback);
        });
    }

    std::mutex mutex_;
    std::map<std::string, sp<ClientInterface>> clients_;
    std::vector<sp<IInterfaceEventCallback>> interface_callbacks_;
    std::vector<sp<IWificondEventCallback>> event_callbacks_;
};

}  // namespace

int main() {
    android::base::InitLogging(nullptr, android::base::LogdLogger(android::base::SYSTEM));
    android::ProcessState::self()->setThreadPoolMaxThreadCount(4);
    android::ProcessState::self()->startThreadPool();
    const auto status = android::defaultServiceManager()->addService(
            android::String16(android::wificond::ipc_constants::kServiceName),
            sp<Wificond>::make());
    CHECK_EQ(status, android::OK) << "Could not register "
                                  << android::wificond::ipc_constants::kServiceName;
    LOG(INFO) << "Droidloom wificond serving " << ap::kInterface;
    android::IPCThreadState::self()->joinThreadPool();
    return 0;
}
