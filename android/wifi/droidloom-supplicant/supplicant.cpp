/* Copyright 2026 Droidloom. SPDX-License-Identifier: GPL-3.0-or-later */
// Droidloom's Wi-Fi supplicant: the framework's station HAL for a cell whose
// wlan0 is a plain link to the host. It joins the one access point wificond
// reports, with the callbacks wpa_supplicant sends for an open network, and
// reports a network it cannot find for any other. Addresses and routes come
// from DHCP on the link, as on any phone.

#include <unistd.h>

#include <algorithm>
#include <chrono>
#include <cstdlib>
#include <functional>
#include <map>
#include <memory>
#include <mutex>
#include <string>
#include <vector>

#include <aidl/android/hardware/wifi/supplicant/BnSupplicant.h>
#include <aidl/android/hardware/wifi/supplicant/BnSupplicantStaIface.h>
#include <aidl/android/hardware/wifi/supplicant/BnSupplicantStaNetwork.h>
#include <aidl/android/hardware/wifi/supplicant/SupplicantStatusCode.h>
#include <android-base/file.h>
#include <android-base/logging.h>
#include <android-base/parseint.h>
#include <android-base/strings.h>
#include <android/binder_manager.h>
#include <android/binder_process.h>

#include "droidloom/wifi_access_point.h"
#include "droidloom/wifi_dispatcher.h"

namespace {

using namespace aidl::android::hardware::wifi::supplicant;
using aidl::android::hardware::wifi::common::OuiKeyedData;
using ndk::ScopedAStatus;
namespace ap = droidloom::wifi;

// Each step of an association, as long as a nearby access point takes.
constexpr auto kAssociationStep = std::chrono::milliseconds(50);
// wpa_supplicant reports a missing network after its scans come back empty.
constexpr auto kNotFoundDelay = std::chrono::milliseconds(1000);
// terminate() replies before the process exits.
constexpr auto kExitDelay = std::chrono::milliseconds(100);
// ConnectionCapabilities.channelBandwidth counts like ScanResult: 2 is 80 MHz.
constexpr int32_t kChannelWidth80Mhz = 2;
static_assert(ap::kChannelWidthMhz == 80);

constexpr char kServiceName[] = "android.hardware.wifi.supplicant.ISupplicant/default";

ScopedAStatus Fail(SupplicantStatusCode code) {
    return ScopedAStatus::fromServiceSpecificError(static_cast<int32_t>(code));
}

bool Has(KeyMgmtMask mask, KeyMgmtMask flag) {
    return (static_cast<int32_t>(mask) & static_cast<int32_t>(flag)) != 0;
}

std::vector<uint8_t> AccessPointSsid() {
    return {ap::kSsid, ap::kSsid + sizeof(ap::kSsid) - 1};
}

std::vector<uint8_t> AccessPointBssid() {
    return {ap::kBssid.begin(), ap::kBssid.end()};
}

std::vector<uint8_t> InterfaceMac() {
    std::string text;
    std::vector<uint8_t> mac;
    const std::string path = std::string("/sys/class/net/") + ap::kInterface + "/address";
    if (!android::base::ReadFileToString(path, &text)) return mac;
    for (const auto& part : android::base::Split(android::base::Trim(text), ":")) {
        uint8_t byte;
        if (!android::base::ParseUint("0x" + part, &byte)) return {};
        mac.push_back(byte);
    }
    return mac;
}

// Calls the framework makes as settings for a radio: power save, coexistence,
// WPS identity, receive filters. The link to the host has none of them.
#define DROIDLOOM_ACCEPT(...) \
    ScopedAStatus __VA_ARGS__ override { return ScopedAStatus::ok(); }
// Features that need other stations or a real access point: DPP, WPS,
// Passpoint queries, TDLS, QoS policies, unsynchronized discovery.
#define DROIDLOOM_UNSUPPORTED(...) \
    ScopedAStatus __VA_ARGS__ override { return Fail(SupplicantStatusCode::FAILURE_UNSUPPORTED); }

// What the framework configured on one network. Only the fields that decide
// whether the network is Droidloom's access point are read back.
struct NetworkConfig {
    std::vector<uint8_t> ssid;
    std::vector<uint8_t> bssid;
    KeyMgmtMask key_mgmt = KeyMgmtMask::NONE;
    ProtoMask proto{};
    AuthAlgMask auth_alg{};
    GroupCipherMask group_cipher{};
    PairwiseCipherMask pairwise_cipher{};
    GroupMgmtCipherMask group_mgmt_cipher{};
    bool require_pmf = false;
    bool scan_ssid = false;
    std::string id_str;
    std::string psk_passphrase;
    std::vector<uint8_t> psk;
    std::string sae_password;
    std::string sae_password_id;
    int32_t wep_tx_key_idx = 0;
    bool edmg = false;
};

class StaIface;

class StaNetwork : public BnSupplicantStaNetwork {
  public:
    StaNetwork(int32_t id, std::weak_ptr<StaIface> iface) : id_(id), iface_(std::move(iface)) {}

    NetworkConfig Config() {
        std::lock_guard lock(mutex_);
        return config_;
    }

    ScopedAStatus getId(int32_t* out) override {
        *out = id_;
        return ScopedAStatus::ok();
    }
    ScopedAStatus getInterfaceName(std::string* out) override {
        *out = ap::kInterface;
        return ScopedAStatus::ok();
    }
    ScopedAStatus getType(IfaceType* out) override {
        *out = IfaceType::STA;
        return ScopedAStatus::ok();
    }
    ScopedAStatus registerCallback(const std::shared_ptr<ISupplicantStaNetworkCallback>&) override {
        // The callbacks report EAP, SIM and PMK events; an open network has none.
        return ScopedAStatus::ok();
    }

    ScopedAStatus select() override;
    ScopedAStatus enable(bool no_connect) override;
    ScopedAStatus disable() override;

    // The fields a connection reads, and the framework reads back.
    ScopedAStatus setSsid(const std::vector<uint8_t>& ssid) override {
        return Set(&NetworkConfig::ssid, ssid);
    }
    ScopedAStatus getSsid(std::vector<uint8_t>* out) override { return Get(&NetworkConfig::ssid, out); }
    ScopedAStatus setBssid(const std::vector<uint8_t>& bssid) override {
        return Set(&NetworkConfig::bssid, bssid);
    }
    ScopedAStatus getBssid(std::vector<uint8_t>* out) override { return Get(&NetworkConfig::bssid, out); }
    ScopedAStatus setKeyMgmt(KeyMgmtMask mask) override { return Set(&NetworkConfig::key_mgmt, mask); }
    ScopedAStatus getKeyMgmt(KeyMgmtMask* out) override { return Get(&NetworkConfig::key_mgmt, out); }
    ScopedAStatus setProto(ProtoMask mask) override { return Set(&NetworkConfig::proto, mask); }
    ScopedAStatus getProto(ProtoMask* out) override { return Get(&NetworkConfig::proto, out); }
    ScopedAStatus setAuthAlg(AuthAlgMask mask) override { return Set(&NetworkConfig::auth_alg, mask); }
    ScopedAStatus getAuthAlg(AuthAlgMask* out) override { return Get(&NetworkConfig::auth_alg, out); }
    ScopedAStatus setGroupCipher(GroupCipherMask mask) override {
        return Set(&NetworkConfig::group_cipher, mask);
    }
    ScopedAStatus getGroupCipher(GroupCipherMask* out) override {
        return Get(&NetworkConfig::group_cipher, out);
    }
    ScopedAStatus setPairwiseCipher(PairwiseCipherMask mask) override {
        return Set(&NetworkConfig::pairwise_cipher, mask);
    }
    ScopedAStatus getPairwiseCipher(PairwiseCipherMask* out) override {
        return Get(&NetworkConfig::pairwise_cipher, out);
    }
    ScopedAStatus setGroupMgmtCipher(GroupMgmtCipherMask mask) override {
        return Set(&NetworkConfig::group_mgmt_cipher, mask);
    }
    ScopedAStatus getGroupMgmtCipher(GroupMgmtCipherMask* out) override {
        return Get(&NetworkConfig::group_mgmt_cipher, out);
    }
    ScopedAStatus setRequirePmf(bool enable) override { return Set(&NetworkConfig::require_pmf, enable); }
    ScopedAStatus getRequirePmf(bool* out) override { return Get(&NetworkConfig::require_pmf, out); }
    ScopedAStatus setScanSsid(bool enable) override { return Set(&NetworkConfig::scan_ssid, enable); }
    ScopedAStatus getScanSsid(bool* out) override { return Get(&NetworkConfig::scan_ssid, out); }
    ScopedAStatus setIdStr(const std::string& id) override { return Set(&NetworkConfig::id_str, id); }
    ScopedAStatus getIdStr(std::string* out) override { return Get(&NetworkConfig::id_str, out); }
    ScopedAStatus setPskPassphrase(const std::string& psk) override {
        return Set(&NetworkConfig::psk_passphrase, psk);
    }
    ScopedAStatus getPskPassphrase(std::string* out) override {
        return Get(&NetworkConfig::psk_passphrase, out);
    }
    ScopedAStatus setPsk(const std::vector<uint8_t>& psk) override { return Set(&NetworkConfig::psk, psk); }
    ScopedAStatus getPsk(std::vector<uint8_t>* out) override { return Get(&NetworkConfig::psk, out); }
    ScopedAStatus setSaePassword(const std::string& password) override {
        return Set(&NetworkConfig::sae_password, password);
    }
    ScopedAStatus getSaePassword(std::string* out) override {
        return Get(&NetworkConfig::sae_password, out);
    }
    ScopedAStatus setSaePasswordId(const std::string& id) override {
        return Set(&NetworkConfig::sae_password_id, id);
    }
    ScopedAStatus getSaePasswordId(std::string* out) override {
        return Get(&NetworkConfig::sae_password_id, out);
    }
    ScopedAStatus setWepTxKeyIdx(int32_t index) override {
        return Set(&NetworkConfig::wep_tx_key_idx, index);
    }
    ScopedAStatus getWepTxKeyIdx(int32_t* out) override {
        return Get(&NetworkConfig::wep_tx_key_idx, out);
    }
    ScopedAStatus setEdmg(bool enable) override { return Set(&NetworkConfig::edmg, enable); }
    ScopedAStatus getEdmg(bool* out) override { return Get(&NetworkConfig::edmg, out); }

    // Enterprise, WEP and SAE details: kept by the framework, never used to join.
    DROIDLOOM_ACCEPT(enableSaePkOnlyMode(bool))
    DROIDLOOM_ACCEPT(enableSuiteBEapOpenSslCiphers())
    DROIDLOOM_ACCEPT(enableTlsSuiteBEapPhase1Param(bool))
    DROIDLOOM_ACCEPT(getEapAltSubjectMatch(std::string*))
    DROIDLOOM_ACCEPT(getEapAnonymousIdentity(std::vector<uint8_t>*))
    DROIDLOOM_ACCEPT(getEapCACert(std::string*))
    DROIDLOOM_ACCEPT(getEapCAPath(std::string*))
    DROIDLOOM_ACCEPT(getEapClientCert(std::string*))
    DROIDLOOM_ACCEPT(getEapDomainSuffixMatch(std::string*))
    DROIDLOOM_ACCEPT(getEapEngine(bool*))
    DROIDLOOM_ACCEPT(getEapEngineId(std::string*))
    DROIDLOOM_ACCEPT(getEapIdentity(std::vector<uint8_t>*))
    DROIDLOOM_ACCEPT(getEapMethod(EapMethod*))
    DROIDLOOM_ACCEPT(getEapPassword(std::vector<uint8_t>*))
    DROIDLOOM_ACCEPT(getEapPhase2Method(EapPhase2Method*))
    DROIDLOOM_ACCEPT(getEapPrivateKeyId(std::string*))
    DROIDLOOM_ACCEPT(getEapSubjectMatch(std::string*))
    DROIDLOOM_ACCEPT(getOcsp(OcspType*))
    DROIDLOOM_ACCEPT(getWapiCertSuite(std::string*))
    DROIDLOOM_ACCEPT(getWepKey(int32_t, std::vector<uint8_t>*))
    DROIDLOOM_ACCEPT(sendNetworkEapIdentityResponse(const std::vector<uint8_t>&,
                                                    const std::vector<uint8_t>&))
    DROIDLOOM_ACCEPT(sendNetworkEapSimGsmAuthFailure())
    DROIDLOOM_ACCEPT(sendNetworkEapSimGsmAuthResponse(
            const std::vector<NetworkResponseEapSimGsmAuthParams>&))
    DROIDLOOM_ACCEPT(sendNetworkEapSimUmtsAuthFailure())
    DROIDLOOM_ACCEPT(sendNetworkEapSimUmtsAuthResponse(const NetworkResponseEapSimUmtsAuthParams&))
    DROIDLOOM_ACCEPT(sendNetworkEapSimUmtsAutsResponse(const std::vector<uint8_t>&))
    DROIDLOOM_ACCEPT(setDppKeys(const DppConnectionKeys&))
    DROIDLOOM_ACCEPT(setEapAltSubjectMatch(const std::string&))
    DROIDLOOM_ACCEPT(setEapAnonymousIdentity(const std::vector<uint8_t>&))
    DROIDLOOM_ACCEPT(setEapCACert(const std::string&))
    DROIDLOOM_ACCEPT(setEapCAPath(const std::string&))
    DROIDLOOM_ACCEPT(setEapClientCert(const std::string&))
    DROIDLOOM_ACCEPT(setEapDomainSuffixMatch(const std::string&))
    DROIDLOOM_ACCEPT(setEapEncryptedImsiIdentity(const std::vector<uint8_t>&))
    DROIDLOOM_ACCEPT(setEapEngine(bool))
    DROIDLOOM_ACCEPT(setEapEngineID(const std::string&))
    DROIDLOOM_ACCEPT(setEapErp(bool))
    DROIDLOOM_ACCEPT(setEapIdentity(const std::vector<uint8_t>&))
    DROIDLOOM_ACCEPT(setEapMethod(EapMethod))
    DROIDLOOM_ACCEPT(setEapPassword(const std::vector<uint8_t>&))
    DROIDLOOM_ACCEPT(setEapPhase2Method(EapPhase2Method))
    DROIDLOOM_ACCEPT(setEapPrivateKeyId(const std::string&))
    DROIDLOOM_ACCEPT(setEapSubjectMatch(const std::string&))
    DROIDLOOM_ACCEPT(setOcsp(OcspType))
    DROIDLOOM_ACCEPT(setPmkCache(const std::vector<uint8_t>&))
    DROIDLOOM_ACCEPT(setProactiveKeyCaching(bool))
    DROIDLOOM_ACCEPT(setSaeH2eMode(SaeH2eMode))
    DROIDLOOM_ACCEPT(setUpdateIdentifier(int32_t))
    DROIDLOOM_ACCEPT(setWapiCertSuite(const std::string&))
    DROIDLOOM_ACCEPT(setWepKey(int32_t, const std::vector<uint8_t>&))
    DROIDLOOM_ACCEPT(setRoamingConsortiumSelection(const std::vector<uint8_t>&))
    DROIDLOOM_ACCEPT(setMinimumTlsVersionEapPhase1Param(TlsVersion))
    DROIDLOOM_ACCEPT(setStrictConservativePeerMode(bool))
    DROIDLOOM_ACCEPT(disableEht())
    DROIDLOOM_ACCEPT(setVendorData(const std::vector<OuiKeyedData>&))
    DROIDLOOM_UNSUPPORTED(getWpsNfcConfigurationToken(std::vector<uint8_t>*))

  private:
    template <typename T, typename V>
    ScopedAStatus Set(T NetworkConfig::*field, const V& value) {
        std::lock_guard lock(mutex_);
        config_.*field = value;
        return ScopedAStatus::ok();
    }
    template <typename T>
    ScopedAStatus Get(T NetworkConfig::*field, T* out) {
        std::lock_guard lock(mutex_);
        *out = config_.*field;
        return ScopedAStatus::ok();
    }

    const int32_t id_;
    const std::weak_ptr<StaIface> iface_;
    std::mutex mutex_;
    NetworkConfig config_;
};

class StaIface : public BnSupplicantStaIface {
  public:
    // Networks hold the interface weakly, so it needs its own owner.
    static std::shared_ptr<StaIface> Create() {
        auto iface = ndk::SharedRefBase::make<StaIface>();
        iface->self_ = iface;
        return iface;
    }

    ScopedAStatus getName(std::string* out) override {
        *out = ap::kInterface;
        return ScopedAStatus::ok();
    }
    ScopedAStatus getType(IfaceType* out) override {
        *out = IfaceType::STA;
        return ScopedAStatus::ok();
    }
    ScopedAStatus getMacAddress(std::vector<uint8_t>* out) override {
        *out = InterfaceMac();
        if (out->size() != 6) return Fail(SupplicantStatusCode::FAILURE_UNKNOWN);
        return ScopedAStatus::ok();
    }
    ScopedAStatus registerCallback(
            const std::shared_ptr<ISupplicantStaIfaceCallback>& callback) override {
        if (callback == nullptr) return Fail(SupplicantStatusCode::FAILURE_ARGS_INVALID);
        std::lock_guard lock(mutex_);
        callbacks_.push_back(callback);
        return ScopedAStatus::ok();
    }

    ScopedAStatus addNetwork(std::shared_ptr<ISupplicantStaNetwork>* out) override {
        std::lock_guard lock(mutex_);
        const int32_t id = next_network_id_++;
        auto network = ndk::SharedRefBase::make<StaNetwork>(id, self_);
        networks_[id] = network;
        *out = network;
        Emit([id](const auto& callback) { callback->onNetworkAdded(id); });
        return ScopedAStatus::ok();
    }
    ScopedAStatus getNetwork(int32_t id, std::shared_ptr<ISupplicantStaNetwork>* out) override {
        std::lock_guard lock(mutex_);
        auto found = networks_.find(id);
        if (found == networks_.end()) return Fail(SupplicantStatusCode::FAILURE_NETWORK_UNKNOWN);
        *out = found->second;
        return ScopedAStatus::ok();
    }
    ScopedAStatus listNetworks(std::vector<int32_t>* out) override {
        std::lock_guard lock(mutex_);
        out->clear();
        for (const auto& [id, network] : networks_) out->push_back(id);
        return ScopedAStatus::ok();
    }
    ScopedAStatus removeNetwork(int32_t id) override {
        std::lock_guard lock(mutex_);
        if (networks_.erase(id) == 0) return Fail(SupplicantStatusCode::FAILURE_NETWORK_UNKNOWN);
        if (id == current_network_) {
            Disconnect(StaIfaceReasonCode::DEAUTH_LEAVING);
            current_network_ = -1;
        }
        Emit([id](const auto& callback) { callback->onNetworkRemoved(id); });
        return ScopedAStatus::ok();
    }

    ScopedAStatus disconnect() override {
        std::lock_guard lock(mutex_);
        Disconnect(StaIfaceReasonCode::DEAUTH_LEAVING);
        return ScopedAStatus::ok();
    }
    ScopedAStatus reconnect() override {
        std::lock_guard lock(mutex_);
        if (current_network_ < 0) return Fail(SupplicantStatusCode::FAILURE_UNKNOWN);
        if (state_ == StaIfaceCallbackState::DISCONNECTED) Connect(current_network_);
        return ScopedAStatus::ok();
    }
    ScopedAStatus reassociate() override {
        std::lock_guard lock(mutex_);
        if (current_network_ < 0) return Fail(SupplicantStatusCode::FAILURE_UNKNOWN);
        Connect(current_network_);
        return ScopedAStatus::ok();
    }

    // Called by StaNetwork.
    ScopedAStatus Select(int32_t id) {
        std::lock_guard lock(mutex_);
        if (networks_.count(id) == 0) return Fail(SupplicantStatusCode::FAILURE_NETWORK_UNKNOWN);
        Connect(id);
        return ScopedAStatus::ok();
    }
    void DisableNetwork(int32_t id) {
        std::lock_guard lock(mutex_);
        if (id == current_network_) Disconnect(StaIfaceReasonCode::DEAUTH_LEAVING);
    }
    // Called by Supplicant when the framework removes the interface.
    void Remove() {
        std::lock_guard lock(mutex_);
        Disconnect(StaIfaceReasonCode::DEAUTH_LEAVING);
        callbacks_.clear();
    }

    ScopedAStatus getKeyMgmtCapabilities(KeyMgmtMask* out) override {
        *out = static_cast<KeyMgmtMask>(static_cast<int32_t>(KeyMgmtMask::NONE) |
                                        static_cast<int32_t>(KeyMgmtMask::WPA_PSK));
        return ScopedAStatus::ok();
    }
    ScopedAStatus getWpaDriverCapabilities(WpaDriverCapabilitiesMask* out) override {
        *out = static_cast<WpaDriverCapabilitiesMask>(0);
        return ScopedAStatus::ok();
    }
    ScopedAStatus getConnectionCapabilities(ConnectionCapabilities* out) override {
        out->technology = WifiTechnology::HE;
        out->channelBandwidth = kChannelWidth80Mhz;
        out->maxNumberTxSpatialStreams = ap::kSpatialStreams;
        out->maxNumberRxSpatialStreams = ap::kSpatialStreams;
        out->legacyMode = LegacyMode::UNKNOWN;
        return ScopedAStatus::ok();
    }
    ScopedAStatus getConnectionMloLinksInfo(MloLinksInfo* out) override {
        // A single-link access point, as wpa_supplicant reports one.
        out->links.clear();
        out->apMloLinkId = -1;
        return ScopedAStatus::ok();
    }
    ScopedAStatus getSignalPollResults(std::vector<SignalPollResult>* out) override {
        std::lock_guard lock(mutex_);
        if (state_ != StaIfaceCallbackState::COMPLETED) {
            return Fail(SupplicantStatusCode::FAILURE_IFACE_DISABLED);
        }
        SignalPollResult result;
        result.linkId = 0;
        result.currentRssiDbm = ap::kRssiDbm;
        result.txBitrateMbps = ap::kLinkSpeedMbps;
        result.rxBitrateMbps = ap::kLinkSpeedMbps;
        result.frequencyMhz = ap::kFrequencyMhz;
        out->assign(1, result);
        return ScopedAStatus::ok();
    }
    ScopedAStatus getUsdCapabilities(UsdCapabilities* out) override {
        *out = UsdCapabilities{};
        return ScopedAStatus::ok();
    }

    DROIDLOOM_ACCEPT(enableAutoReconnect(bool))
    DROIDLOOM_ACCEPT(filsHlpAddRequest(const std::vector<uint8_t>&, const std::vector<uint8_t>&))
    DROIDLOOM_ACCEPT(filsHlpFlushRequest())
    DROIDLOOM_ACCEPT(addRxFilter(RxFilterType))
    DROIDLOOM_ACCEPT(removeRxFilter(RxFilterType))
    DROIDLOOM_ACCEPT(startRxFilter())
    DROIDLOOM_ACCEPT(stopRxFilter())
    DROIDLOOM_ACCEPT(setBtCoexistenceMode(BtCoexistenceMode))
    DROIDLOOM_ACCEPT(setBtCoexistenceScanModeEnabled(bool))
    DROIDLOOM_ACCEPT(setCountryCode(const std::vector<uint8_t>&))
    DROIDLOOM_ACCEPT(setExternalSim(bool))
    DROIDLOOM_ACCEPT(setMboCellularDataStatus(bool))
    DROIDLOOM_ACCEPT(setPowerSave(bool))
    DROIDLOOM_ACCEPT(setSuspendModeEnabled(bool))
    DROIDLOOM_ACCEPT(setWpsConfigMethods(WpsConfigMethods))
    DROIDLOOM_ACCEPT(setWpsDeviceName(const std::string&))
    DROIDLOOM_ACCEPT(setWpsDeviceType(const std::vector<uint8_t>&))
    DROIDLOOM_ACCEPT(setWpsManufacturer(const std::string&))
    DROIDLOOM_ACCEPT(setWpsModelName(const std::string&))
    DROIDLOOM_ACCEPT(setWpsModelNumber(const std::string&))
    DROIDLOOM_ACCEPT(setWpsSerialNumber(const std::string&))
    DROIDLOOM_ACCEPT(setQosPolicyFeatureEnabled(bool))

    DROIDLOOM_UNSUPPORTED(addDppPeerUri(const std::string&, int32_t*))
    DROIDLOOM_UNSUPPORTED(removeDppUri(int32_t))
    DROIDLOOM_UNSUPPORTED(generateDppBootstrapInfoForResponder(const std::vector<uint8_t>&,
                                                               const std::string&, DppCurve,
                                                               DppResponderBootstrapInfo*))
    DROIDLOOM_UNSUPPORTED(generateSelfDppConfiguration(const std::string&,
                                                       const std::vector<uint8_t>&))
    DROIDLOOM_UNSUPPORTED(startDppConfiguratorInitiator(int32_t, int32_t, const std::string&,
                                                        const std::string&, const std::string&,
                                                        DppNetRole, DppAkm,
                                                        const std::vector<uint8_t>&,
                                                        std::vector<uint8_t>*))
    DROIDLOOM_UNSUPPORTED(startDppEnrolleeInitiator(int32_t, int32_t))
    DROIDLOOM_UNSUPPORTED(startDppEnrolleeResponder(int32_t))
    DROIDLOOM_UNSUPPORTED(stopDppInitiator())
    DROIDLOOM_UNSUPPORTED(stopDppResponder(int32_t))
    DROIDLOOM_UNSUPPORTED(addExtRadioWork(const std::string&, int32_t, int32_t, int32_t*))
    DROIDLOOM_UNSUPPORTED(removeExtRadioWork(int32_t))
    DROIDLOOM_UNSUPPORTED(initiateAnqpQuery(const std::vector<uint8_t>&,
                                            const std::vector<AnqpInfoId>&,
                                            const std::vector<Hs20AnqpSubtypes>&))
    DROIDLOOM_UNSUPPORTED(initiateHs20IconQuery(const std::vector<uint8_t>&, const std::string&))
    DROIDLOOM_UNSUPPORTED(initiateVenueUrlAnqpQuery(const std::vector<uint8_t>&))
    DROIDLOOM_UNSUPPORTED(initiateTdlsDiscover(const std::vector<uint8_t>&))
    DROIDLOOM_UNSUPPORTED(initiateTdlsSetup(const std::vector<uint8_t>&))
    DROIDLOOM_UNSUPPORTED(initiateTdlsTeardown(const std::vector<uint8_t>&))
    DROIDLOOM_UNSUPPORTED(cancelWps())
    DROIDLOOM_UNSUPPORTED(startWpsPbc(const std::vector<uint8_t>&))
    DROIDLOOM_UNSUPPORTED(startWpsPinDisplay(const std::vector<uint8_t>&, std::string*))
    DROIDLOOM_UNSUPPORTED(startWpsPinKeypad(const std::string&))
    DROIDLOOM_UNSUPPORTED(startWpsRegistrar(const std::vector<uint8_t>&, const std::string&))
    DROIDLOOM_UNSUPPORTED(sendQosPolicyResponse(int32_t, bool,
                                                const std::vector<QosPolicyStatus>&))
    DROIDLOOM_UNSUPPORTED(removeAllQosPolicies())
    DROIDLOOM_UNSUPPORTED(addQosPolicyRequestForScs(const std::vector<QosPolicyScsData>&,
                                                    std::vector<QosPolicyScsRequestStatus>*))
    DROIDLOOM_UNSUPPORTED(removeQosPolicyForScs(const std::vector<uint8_t>&,
                                                std::vector<QosPolicyScsRequestStatus>*))
    DROIDLOOM_UNSUPPORTED(configureMscs(const MscsParams&))
    DROIDLOOM_UNSUPPORTED(disableMscs())
    DROIDLOOM_UNSUPPORTED(startUsdPublish(int32_t, const UsdPublishConfig&))
    DROIDLOOM_UNSUPPORTED(startUsdSubscribe(int32_t, const UsdSubscribeConfig&))
    DROIDLOOM_UNSUPPORTED(updateUsdPublish(int32_t, const std::vector<uint8_t>&))
    DROIDLOOM_UNSUPPORTED(cancelUsdPublish(int32_t))
    DROIDLOOM_UNSUPPORTED(cancelUsdSubscribe(int32_t))
    DROIDLOOM_UNSUPPORTED(sendUsdMessage(const UsdMessageInfo&))

  private:
    using Callback = std::shared_ptr<ISupplicantStaIfaceCallback>;

    bool Associated() const {
        return state_ == StaIfaceCallbackState::ASSOCIATED ||
               state_ == StaIfaceCallbackState::COMPLETED;
    }

    // Sends an event to every callback once the current call has replied.
    // Called with mutex_ held, so events keep the order of the calls.
    void Emit(std::function<void(const Callback&)> event, std::chrono::milliseconds delay = {}) {
        ap::Dispatcher::Get().Post(delay, [callbacks = callbacks_, event = std::move(event)] {
            for (const auto& callback : callbacks) event(callback);
        });
    }

    void EmitState(StaIfaceCallbackState state, bool with_bssid, const std::vector<uint8_t>& ssid,
                   std::chrono::milliseconds delay = {}) {
        SupplicantStateChangeData data;
        data.newState = state;
        data.id = current_network_;
        data.ssid = ssid;
        if (with_bssid) std::copy(ap::kBssid.begin(), ap::kBssid.end(), data.bssid.begin());
        // wpa_supplicant reports the key management and channel once associated.
        if (state == StaIfaceCallbackState::ASSOCIATED || state == StaIfaceCallbackState::COMPLETED) {
            data.keyMgmtMask = KeyMgmtMask::NONE;
            data.frequencyMhz = ap::kFrequencyMhz;
        }
        Emit([data](const auto& callback) { callback->onSupplicantStateChanged(data); }, delay);
    }

    // Runs `step` after `delay` unless the connection changed in the meantime.
    void Step(uint64_t generation, std::chrono::milliseconds delay, std::function<void()> step) {
        auto self = self_.lock();
        ap::Dispatcher::Get().Post(delay, [self, generation, step = std::move(step)] {
            std::lock_guard lock(self->mutex_);
            if (self->generation_ == generation) step();
        });
    }

    // Starts joining network `id`, leaving any current access point first.
    void Connect(int32_t id) {
        const auto config = networks_.at(id)->Config();
        if (state_ != StaIfaceCallbackState::DISCONNECTED) {
            Disconnect(StaIfaceReasonCode::DEAUTH_LEAVING);
        }
        const uint64_t generation = ++generation_;
        current_network_ = id;
        current_ssid_ = config.ssid;
        state_ = StaIfaceCallbackState::SCANNING;
        EmitState(state_, false, current_ssid_);

        const bool any_bssid = std::all_of(config.bssid.begin(), config.bssid.end(),
                                           [](uint8_t byte) { return byte == 0; });
        const bool reachable = config.ssid == AccessPointSsid() &&
                               Has(config.key_mgmt, KeyMgmtMask::NONE) &&
                               (any_bssid || config.bssid == AccessPointBssid());
        if (!reachable) {
            // wpa_supplicant keeps scanning; the framework gives up on the attempt.
            Step(generation, kNotFoundDelay, [this, ssid = config.ssid] {
                Emit([ssid](const auto& callback) { callback->onNetworkNotFound(ssid); });
            });
            LOG(INFO) << "Network " << id << " is not Droidloom's access point";
            return;
        }
        Step(generation, kAssociationStep, [this] {
            state_ = StaIfaceCallbackState::ASSOCIATING;
            EmitState(state_, true, current_ssid_);
        });
        Step(generation, 2 * kAssociationStep, [this] {
            state_ = StaIfaceCallbackState::ASSOCIATED;
            EmitState(state_, true, current_ssid_);
            const auto bssid = AccessPointBssid();
            Emit([bssid](const auto& callback) {
                callback->onBssidChanged(BssidChangeReason::ASSOC_COMPLETE, bssid);
            });
            // An open network has no handshake: associated is connected.
            state_ = StaIfaceCallbackState::COMPLETED;
            EmitState(state_, true, current_ssid_);
            LOG(INFO) << "Connected to " << ap::kSsid;
        });
    }

    // Leaves the access point, or stops trying to reach it.
    void Disconnect(StaIfaceReasonCode reason) {
        ++generation_;
        if (state_ == StaIfaceCallbackState::DISCONNECTED) return;
        if (Associated()) {
            const auto bssid = AccessPointBssid();
            Emit([bssid, reason](const auto& callback) {
                callback->onDisconnected(bssid, true, reason);
            });
        }
        state_ = StaIfaceCallbackState::DISCONNECTED;
        EmitState(state_, false, current_ssid_);
        Emit([](const auto& callback) {
            callback->onBssidChanged(BssidChangeReason::DISASSOC, std::vector<uint8_t>(6, 0));
        });
        LOG(INFO) << "Disconnected";
    }

    std::weak_ptr<StaIface> self_;
    std::mutex mutex_;
    std::vector<Callback> callbacks_;
    std::map<int32_t, std::shared_ptr<StaNetwork>> networks_;
    int32_t next_network_id_ = 0;
    int32_t current_network_ = -1;
    std::vector<uint8_t> current_ssid_;
    StaIfaceCallbackState state_ = StaIfaceCallbackState::DISCONNECTED;
    // Bumped by every connect and disconnect; pending steps of an older one drop.
    uint64_t generation_ = 0;
};

ScopedAStatus StaNetwork::select() {
    auto iface = iface_.lock();
    if (iface == nullptr) return Fail(SupplicantStatusCode::FAILURE_IFACE_INVALID);
    return iface->Select(id_);
}

ScopedAStatus StaNetwork::enable(bool no_connect) {
    // wpa_supplicant only joins an enabled network on its own when nothing is
    // selected; the framework always selects, so enabling joins nothing here.
    (void)no_connect;
    return iface_.expired() ? Fail(SupplicantStatusCode::FAILURE_IFACE_INVALID)
                            : ScopedAStatus::ok();
}

ScopedAStatus StaNetwork::disable() {
    auto iface = iface_.lock();
    if (iface == nullptr) return Fail(SupplicantStatusCode::FAILURE_IFACE_INVALID);
    iface->DisableNetwork(id_);
    return ScopedAStatus::ok();
}

class Supplicant : public BnSupplicant {
  public:
    ScopedAStatus addStaInterface(const std::string& name,
                                  std::shared_ptr<ISupplicantStaIface>* out) override {
        if (name != ap::kInterface) return Fail(SupplicantStatusCode::FAILURE_ARGS_INVALID);
        std::lock_guard lock(mutex_);
        if (iface_ == nullptr) iface_ = StaIface::Create();
        *out = iface_;
        LOG(INFO) << "Station interface " << name << " added";
        return ScopedAStatus::ok();
    }
    ScopedAStatus getStaInterface(const std::string& name,
                                  std::shared_ptr<ISupplicantStaIface>* out) override {
        std::lock_guard lock(mutex_);
        if (name != ap::kInterface || iface_ == nullptr) {
            return Fail(SupplicantStatusCode::FAILURE_IFACE_UNKNOWN);
        }
        *out = iface_;
        return ScopedAStatus::ok();
    }
    ScopedAStatus listInterfaces(std::vector<IfaceInfo>* out) override {
        std::lock_guard lock(mutex_);
        out->clear();
        if (iface_ != nullptr) {
            IfaceInfo info;
            info.type = IfaceType::STA;
            info.name = ap::kInterface;
            out->push_back(info);
        }
        return ScopedAStatus::ok();
    }
    ScopedAStatus removeInterface(const IfaceInfo& info) override {
        std::lock_guard lock(mutex_);
        if (info.type != IfaceType::STA || info.name != ap::kInterface || iface_ == nullptr) {
            return Fail(SupplicantStatusCode::FAILURE_IFACE_UNKNOWN);
        }
        iface_->Remove();
        iface_.reset();
        return ScopedAStatus::ok();
    }
    ScopedAStatus terminate() override {
        LOG(INFO) << "Terminating";
        ap::Dispatcher::Get().Post(kExitDelay, [] { _exit(0); });
        return ScopedAStatus::ok();
    }
    ScopedAStatus getDebugLevel(DebugLevel* out) override {
        std::lock_guard lock(mutex_);
        *out = debug_level_;
        return ScopedAStatus::ok();
    }
    ScopedAStatus setDebugParams(DebugLevel level, bool, bool) override {
        std::lock_guard lock(mutex_);
        debug_level_ = level;
        return ScopedAStatus::ok();
    }

    DROIDLOOM_ACCEPT(isDebugShowKeysEnabled(bool*))
    DROIDLOOM_ACCEPT(isDebugShowTimestampEnabled(bool*))
    DROIDLOOM_ACCEPT(registerCallback(const std::shared_ptr<ISupplicantCallback>&))
    DROIDLOOM_ACCEPT(registerNonStandardCertCallback(
            const std::shared_ptr<INonStandardCertCallback>&))
    DROIDLOOM_ACCEPT(setConcurrencyPriority(IfaceType))
    DROIDLOOM_ACCEPT(setCurrentUserIdentity(int32_t))
    DROIDLOOM_UNSUPPORTED(addP2pInterface(const std::string&, std::shared_ptr<ISupplicantP2pIface>*))
    DROIDLOOM_UNSUPPORTED(getP2pInterface(const std::string&, std::shared_ptr<ISupplicantP2pIface>*))
    DROIDLOOM_UNSUPPORTED(createRttController(const std::string&,
                                              std::shared_ptr<ISupplicantWifiRttController>*))

  private:
    std::mutex mutex_;
    std::shared_ptr<StaIface> iface_;
    DebugLevel debug_level_ = DebugLevel::INFO;
};

}  // namespace

int main() {
    android::base::InitLogging(nullptr, android::base::LogdLogger(android::base::SYSTEM));
    ABinderProcess_setThreadPoolMaxThreadCount(4);
    auto supplicant = ndk::SharedRefBase::make<Supplicant>();
    const binder_status_t status =
            AServiceManager_addService(supplicant->asBinder().get(), kServiceName);
    CHECK_EQ(status, STATUS_OK) << "Could not register " << kServiceName;
    LOG(INFO) << "Droidloom supplicant serving " << ap::kInterface;
    ABinderProcess_startThreadPool();
    ABinderProcess_joinThreadPool();
    return EXIT_FAILURE;
}
