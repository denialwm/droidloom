//! Android gatekeeper, fingerprint and shared-secret services for Droidloom.
//!
//! One process serves `IGatekeeper/default`, `IFingerprint/default` and
//! `ISharedSecret/droidloom`. Keystore negotiates its HMAC key with this
//! participant as it does with KeyMint, and both kinds of auth token are
//! signed with it, so keys bound to the lock screen or to fingerprints work.
//!
//! The fingerprint sensor is the host user's fprintd reader, reached through
//! the device broker. One enrollment stands for every finger the user enrolled
//! on the host; it exists while the host has fingers and the Android user has
//! a secure ID. When either changes, a property tells Android's fingerprint
//! service to read the enrollment again. Removing it from Android does
//! nothing: the host alone decides.

#![deny(unsafe_op_in_unsafe_fn)]
#![allow(non_snake_case)]

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use android_hardware_biometrics_common::aidl::android::hardware::biometrics::common::{
    CommonProps::CommonProps,
    ICancellationSignal::{BnCancellationSignal, ICancellationSignal},
    OperationContext::OperationContext,
    SensorStrength::SensorStrength,
};
use android_hardware_biometrics_fingerprint::aidl::android::hardware::biometrics::fingerprint::{
    AcquiredInfo::AcquiredInfo,
    Error::Error,
    FingerprintSensorType::FingerprintSensorType,
    IFingerprint::{BnFingerprint, IFingerprint},
    ISession::{BnSession, ISession},
    ISessionCallback::ISessionCallback,
    PointerContext::PointerContext,
    SensorLocation::SensorLocation,
    SensorProps::SensorProps,
};
use android_hardware_gatekeeper::aidl::android::hardware::gatekeeper::{
    GatekeeperEnrollResponse::GatekeeperEnrollResponse,
    GatekeeperVerifyResponse::GatekeeperVerifyResponse,
    IGatekeeper::{self, BnGatekeeper},
};
use android_hardware_keymaster::aidl::android::hardware::keymaster::{
    HardwareAuthToken::HardwareAuthToken as BiometricToken,
    HardwareAuthenticatorType::HardwareAuthenticatorType as BiometricAuthenticator,
    Timestamp::Timestamp as BiometricTimestamp,
};
use android_hardware_security_keymint::aidl::android::hardware::security::keymint::{
    HardwareAuthToken::HardwareAuthToken, HardwareAuthenticatorType::HardwareAuthenticatorType,
};
use android_hardware_security_secureclock::aidl::android::hardware::security::secureclock::Timestamp::Timestamp;
use android_hardware_security_sharedsecret::aidl::android::hardware::security::sharedsecret::{
    ISharedSecret::{self, BnSharedSecret},
    SharedSecretParameters::SharedSecretParameters,
};
use binder::{BinderFeatures, ExceptionCode, Interface, ProcessState, Status, Strong};
use droidloom_authentication::{
    ENROLLMENT_ID, FINGERPRINT, FingerprintUser, GatekeeperUser, Handle, Lockout, MAC_LEN,
    PASSWORD, SALT_LEN, TIMED_LOCKOUT_MS, Token, authenticator_id, authenticator_id_input,
    same_mac, shared_secret_context,
};
use droidloom_denial_ipc::SeqPacket;
use droidloom_device_protocol::{MAX_RECORD, Message, Scan};
use kmr_common::crypto::{self, Ckdf, Hmac, Rng};
use kmr_crypto_boring::{aes_cmac::BoringAesCmac, hmac::BoringHmac, rng::BoringRng};
use kmr_wire::keymint::Digest;

const GATEKEEPER: &str = "android.hardware.gatekeeper.IGatekeeper/default";
const FINGERPRINT_SERVICE: &str = "android.hardware.biometrics.fingerprint.IFingerprint/default";
const SHARED_SECRET: &str = "android.hardware.security.sharedsecret.ISharedSecret/droidloom";
/// Created by the service's init file, owned by system.
const STATE: &str = "/data/vendor/droidloom-authentication";
/// The supervisor binds the broker's fingerprint socket here.
const SOCKET: &str = "/dev/socket/droidloom/fingerprint";
/// Replaced whenever the enrollment Android should see changes.
const GENERATION: &str = "generation";
static GENERATIONS: AtomicU64 = AtomicU64::new(0);
const SENSOR_ID: i32 = 0;
/// Gatekeeper users at and above this are Android's credential protectors,
/// not Android users.
const FIRST_PROTECTOR_UID: u32 = 100_000;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn illegal<T>() -> binder::Result<T> {
    Err(Status::new_exception(ExceptionCode::ILLEGAL_ARGUMENT, None))
}

fn boottime_ms() -> i64 {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `time` is valid storage for the result.
    unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &raw mut time) };
    time.tv_sec * 1000 + time.tv_nsec / 1_000_000
}

fn random<const N: usize>() -> [u8; N] {
    let mut bytes = [0; N];
    BoringRng.fill_bytes(&mut bytes);
    bytes
}

fn hmac(key: &[u8], parts: &[&[u8]]) -> Option<[u8; MAC_LEN]> {
    let mut operation = BoringHmac
        .begin(crypto::hmac::Key::new(key.to_vec()).into(), Digest::Sha256)
        .ok()?;
    for part in parts {
        operation.update(part).ok()?;
    }
    operation.finish().ok()?.try_into().ok()
}

/// Files under [`STATE`], each replaced whole.
struct Store(PathBuf);

impl Store {
    fn read(&self, name: &str) -> Option<Vec<u8>> {
        fs::read(self.0.join(name)).ok()
    }

    fn write(&self, name: &str, bytes: &[u8]) -> io::Result<()> {
        let fresh = self.0.join(format!("{name}.new"));
        let mut file = fs::File::create(&fresh)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(fresh, self.0.join(name))
    }

    /// Tells Android's fingerprint service to read the enrollment again: it
    /// watches this file being replaced.
    fn changed(&self) {
        let generation = GENERATIONS.fetch_add(1, Ordering::Relaxed);
        let value = format!("{}.{generation}", boottime_ms());
        if let Err(error) = self.write(GENERATION, value.as_bytes()) {
            eprintln!("droidloom-authentication: enrollment change not announced: {error}");
        }
    }

    fn remove(&self, name: &str) {
        let _ = fs::remove_file(self.0.join(name));
    }

    /// The service's own key, made once.
    fn key(&self) -> io::Result<[u8; MAC_LEN]> {
        if let Some(key) = self.read("key").and_then(|bytes| bytes.try_into().ok()) {
            return Ok(key);
        }
        let key = random();
        self.write("key", &key)?;
        Ok(key)
    }
}

#[derive(Default)]
struct Fingerprints {
    /// Each user's enrollment as Android last read it: secure ID and sorted fingers.
    seen: HashMap<i32, Option<(u64, Vec<String>)>>,
    /// Timed lockouts, until this `CLOCK_BOOTTIME` millisecond.
    timed: HashMap<i32, i64>,
    challenges: HashSet<i64>,
}

struct Authentication {
    store: Store,
    key: [u8; MAC_LEN],
    nonce: [u8; 32],
    /// The HMAC key Keystore negotiated; auth tokens are signed with it.
    shared: Mutex<Option<[u8; MAC_LEN]>>,
    gatekeeper: Mutex<()>,
    fingerprints: Mutex<Fingerprints>,
}

impl Authentication {
    fn sign(&self, token: &Token) -> Option<Vec<u8>> {
        let key = (*lock(&self.shared))?;
        hmac(&key, &[&token.mac_input()]).map(|mac| mac.to_vec())
    }

    fn signed(&self, token: &Token, mac: &[u8]) -> bool {
        self.sign(token)
            .is_some_and(|expected| same_mac(&expected, mac))
    }

    fn gatekeeper_user(&self, uid: u32) -> Option<GatekeeperUser> {
        GatekeeperUser::decode(&self.store.read(&format!("gatekeeper-{uid}"))?)
    }

    fn save_gatekeeper_user(&self, uid: u32, user: &GatekeeperUser) {
        if let Err(error) = self
            .store
            .write(&format!("gatekeeper-{uid}"), &user.encode())
        {
            eprintln!("droidloom-authentication: gatekeeper record not saved: {error}");
        }
    }

    /// An Android user's secure ID, once its credential is enrolled.
    fn sid(&self, user: i32) -> Option<u64> {
        let uid = u32::try_from(user).ok()?;
        let _gatekeeper = lock(&self.gatekeeper);
        self.gatekeeper_user(uid)
            .map(|user| user.sid)
            .filter(|sid| *sid != 0)
    }

    fn fingerprint_user(&self, user: i32) -> FingerprintUser {
        self.store
            .read(&format!("fingerprint-{user}"))
            .and_then(|bytes| FingerprintUser::decode(&bytes))
            .unwrap_or_default()
    }

    fn save_fingerprint_user(&self, user: i32, record: &FingerprintUser) {
        if let Err(error) = self
            .store
            .write(&format!("fingerprint-{user}"), &record.encode())
        {
            eprintln!("droidloom-authentication: fingerprint record not saved: {error}");
        }
    }

    fn authenticator_id(&self, user: i32, sid: u64, fingers: &[String]) -> i64 {
        let invalidations = self.fingerprint_user(user).invalidations;
        hmac(
            &self.key,
            &[&authenticator_id_input(user, sid, invalidations, fingers)],
        )
        .map_or(1, |mac| authenticator_id(&mac))
    }

    /// The user's enrollment: a secure ID and the host's fingers. A broker
    /// that does not answer leaves it as last read.
    fn enrollment(&self, user: i32) -> Option<(u64, Vec<String>)> {
        let Some((present, mut fingers)) = reader() else {
            return lock(&self.fingerprints).seen.get(&user).cloned().flatten();
        };
        fingers.sort_unstable();
        fingers.dedup();
        let enrollment = self
            .sid(user)
            .filter(|_| present && !fingers.is_empty())
            .map(|sid| (sid, fingers));
        let before = lock(&self.fingerprints)
            .seen
            .insert(user, enrollment.clone());
        if before.is_some_and(|before| before != enrollment) {
            self.store.changed();
        }
        enrollment
    }
}

/// Whether the host has a fingerprint reader, and the fingers enrolled on it.
fn reader() -> Option<(bool, Vec<String>)> {
    let socket = SeqPacket::connect(SOCKET).ok()?;
    socket.set_io_timeout(Duration::from_secs(5)).ok()?;
    socket
        .send_record(&Message::FingerprintQuery.encode(), &[])
        .ok()?;
    let mut bytes = vec![0; MAX_RECORD];
    let record = socket.receive_record_into(&mut bytes).ok()?;
    match Message::decode(record.bytes).ok()? {
        Message::FingerprintReader { present, fingers } => Some((present, fingers)),
        _ => None,
    }
}

struct SharedSecret(Arc<Authentication>);

impl Interface for SharedSecret {}

impl ISharedSecret::ISharedSecret for SharedSecret {
    fn getSharedSecretParameters(&self) -> binder::Result<SharedSecretParameters> {
        Ok(SharedSecretParameters {
            seed: Vec::new(),
            nonce: self.0.nonce.to_vec(),
        })
    }

    fn computeSharedSecret(&self, params: &[SharedSecretParameters]) -> binder::Result<Vec<u8>> {
        let participants: Vec<(&[u8], &[u8])> = params
            .iter()
            .map(|param| (param.seed.as_slice(), param.nonce.as_slice()))
            .collect();
        let Some(context) = shared_secret_context(&participants, &self.0.nonce) else {
            return illegal();
        };
        // Every participant derives the key from the same all-zero
        // agreement key, as Android's non-secure KeyMint does.
        let key: [u8; MAC_LEN] = BoringAesCmac
            .ckdf(
                &crypto::aes::Key::Aes256([0; 32]).into(),
                ISharedSecret::KEY_AGREEMENT_LABEL.as_bytes(),
                &[&context],
                MAC_LEN,
            )
            .ok()
            .and_then(|key| key.try_into().ok())
            .ok_or_else(|| Status::new_exception(ExceptionCode::ILLEGAL_STATE, None))?;
        let check = hmac(&key, &[ISharedSecret::KEY_CHECK_LABEL.as_bytes()])
            .ok_or_else(|| Status::new_exception(ExceptionCode::ILLEGAL_STATE, None))?;
        *lock(&self.0.shared) = Some(key);
        Ok(check.to_vec())
    }
}

struct Gatekeeper(Arc<Authentication>);

impl Interface for Gatekeeper {}

fn enroll_failed(timeout_ms: u32) -> GatekeeperEnrollResponse {
    GatekeeperEnrollResponse {
        statusCode: if timeout_ms > 0 {
            IGatekeeper::ERROR_RETRY_TIMEOUT
        } else {
            IGatekeeper::ERROR_GENERAL_FAILURE
        },
        timeoutMs: i32::try_from(timeout_ms).unwrap_or(i32::MAX),
        ..Default::default()
    }
}

fn verify_failed(timeout_ms: u32) -> GatekeeperVerifyResponse {
    GatekeeperVerifyResponse {
        statusCode: if timeout_ms > 0 {
            IGatekeeper::ERROR_RETRY_TIMEOUT
        } else {
            IGatekeeper::ERROR_GENERAL_FAILURE
        },
        timeoutMs: i32::try_from(timeout_ms).unwrap_or(i32::MAX),
        ..Default::default()
    }
}

impl Gatekeeper {
    fn matches(&self, handle: &Handle, password: &[u8]) -> bool {
        hmac(
            &self.0.key,
            &[&Handle::mac_input(
                handle.sid,
                handle.uid,
                &handle.salt,
                password,
            )],
        )
        .is_some_and(|mac| same_mac(&mac, &handle.mac))
    }

    /// Counts a wrong password; the wait it now imposes.
    fn failed(&self, uid: u32, record: &mut GatekeeperUser, now: u64) -> u32 {
        record.failures = record.failures.saturating_add(1);
        record.last_failure_ms = now;
        self.0.save_gatekeeper_user(uid, record);
        record.remaining_ms(now)
    }
}

impl IGatekeeper::IGatekeeper for Gatekeeper {
    fn deleteAllUsers(&self) -> binder::Result<()> {
        let _gatekeeper = lock(&self.0.gatekeeper);
        if let Ok(entries) = fs::read_dir(&self.0.store.0) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                if let Some(name) = name.to_str().filter(|name| name.starts_with("gatekeeper-")) {
                    self.0.store.remove(name);
                }
            }
        }
        self.0.store.changed();
        Ok(())
    }

    fn deleteUser(&self, uid: i32) -> binder::Result<()> {
        let Ok(uid) = u32::try_from(uid) else {
            return illegal();
        };
        let _gatekeeper = lock(&self.0.gatekeeper);
        self.0.store.remove(&format!("gatekeeper-{uid}"));
        if uid < FIRST_PROTECTOR_UID {
            self.0.store.changed();
        }
        Ok(())
    }

    fn enroll(
        &self,
        uid: i32,
        currentPasswordHandle: &[u8],
        currentPassword: &[u8],
        desiredPassword: &[u8],
    ) -> binder::Result<GatekeeperEnrollResponse> {
        let Ok(uid) = u32::try_from(uid) else {
            return illegal();
        };
        if desiredPassword.is_empty() {
            return Ok(enroll_failed(0));
        }
        let _gatekeeper = lock(&self.0.gatekeeper);
        let now = u64::try_from(boottime_ms()).unwrap_or(0);
        let mut record = self.0.gatekeeper_user(uid).unwrap_or_default();
        let sid = if currentPasswordHandle.is_empty() {
            // A new credential: a new secure ID, never 0.
            u64::from_ne_bytes(random()) | 1
        } else {
            let Some(handle) =
                Handle::decode(currentPasswordHandle).filter(|handle| handle.uid == uid)
            else {
                return Ok(enroll_failed(0));
            };
            let waiting = record.remaining_ms(now);
            if waiting > 0 {
                return Ok(enroll_failed(waiting));
            }
            if !self.matches(&handle, currentPassword) {
                return Ok(enroll_failed(self.failed(uid, &mut record, now)));
            }
            handle.sid
        };
        let salt: [u8; SALT_LEN] = random();
        let Some(mac) = hmac(
            &self.0.key,
            &[&Handle::mac_input(sid, uid, &salt, desiredPassword)],
        ) else {
            return Ok(enroll_failed(0));
        };
        let renewed = record.sid != sid;
        let record = GatekeeperUser {
            sid,
            failures: 0,
            last_failure_ms: 0,
        };
        self.0.save_gatekeeper_user(uid, &record);
        if renewed && uid < FIRST_PROTECTOR_UID {
            self.0.store.changed();
        }
        Ok(GatekeeperEnrollResponse {
            statusCode: IGatekeeper::STATUS_OK,
            timeoutMs: 0,
            secureUserId: i64::from_ne_bytes(sid.to_ne_bytes()),
            data: Handle {
                sid,
                uid,
                salt,
                mac,
            }
            .encode(),
        })
    }

    fn verify(
        &self,
        uid: i32,
        challenge: i64,
        enrolledPasswordHandle: &[u8],
        providedPassword: &[u8],
    ) -> binder::Result<GatekeeperVerifyResponse> {
        let Ok(uid) = u32::try_from(uid) else {
            return illegal();
        };
        let Some(handle) =
            Handle::decode(enrolledPasswordHandle).filter(|handle| handle.uid == uid)
        else {
            return Ok(verify_failed(0));
        };
        let _gatekeeper = lock(&self.0.gatekeeper);
        let now = u64::try_from(boottime_ms()).unwrap_or(0);
        let mut record = self.0.gatekeeper_user(uid).unwrap_or_default();
        let waiting = record.remaining_ms(now);
        if waiting > 0 {
            return Ok(verify_failed(waiting));
        }
        if !self.matches(&handle, providedPassword) {
            return Ok(verify_failed(self.failed(uid, &mut record, now)));
        }
        let renewed = record.sid != handle.sid;
        if renewed || record.failures != 0 {
            record = GatekeeperUser {
                sid: handle.sid,
                failures: 0,
                last_failure_ms: 0,
            };
            self.0.save_gatekeeper_user(uid, &record);
        }
        if renewed && uid < FIRST_PROTECTOR_UID {
            self.0.store.changed();
        }
        let sid = i64::from_ne_bytes(handle.sid.to_ne_bytes());
        let token = Token {
            challenge,
            user_id: sid,
            authenticator_id: 0,
            authenticator_type: PASSWORD,
            timestamp_ms: boottime_ms(),
        };
        let Some(mac) = self.0.sign(&token) else {
            eprintln!("droidloom-authentication: no shared key yet; password not attested");
            return Ok(verify_failed(0));
        };
        Ok(GatekeeperVerifyResponse {
            statusCode: IGatekeeper::STATUS_OK,
            timeoutMs: 0,
            hardwareAuthToken: HardwareAuthToken {
                challenge,
                userId: sid,
                authenticatorId: 0,
                authenticatorType: HardwareAuthenticatorType::PASSWORD,
                timestamp: Timestamp {
                    milliSeconds: token.timestamp_ms,
                },
                mac,
            },
        })
    }
}

struct Fingerprint(Arc<Authentication>);

impl Interface for Fingerprint {}

impl IFingerprint for Fingerprint {
    fn getSensorProps(&self) -> binder::Result<Vec<SensorProps>> {
        // Android reads the sensors once, when it starts.
        if !reader().is_some_and(|(present, _)| present) {
            return Ok(Vec::new());
        }
        Ok(vec![SensorProps {
            commonProps: CommonProps {
                sensorId: SENSOR_ID,
                sensorStrength: SensorStrength::STRONG,
                maxEnrollmentsPerUser: 1,
                componentInfo: Vec::new(),
            },
            // The host draws the sensor's mark; Android draws no overlay.
            sensorType: FingerprintSensorType::REAR,
            sensorLocations: vec![SensorLocation::default()],
            ..Default::default()
        }])
    }

    fn createSession(
        &self,
        sensorId: i32,
        userId: i32,
        cb: &Strong<dyn ISessionCallback>,
    ) -> binder::Result<Strong<dyn ISession>> {
        if sensorId != SENSOR_ID {
            return illegal();
        }
        Ok(BnSession::new_binder(
            Session(Arc::new(SessionState {
                authentication: Arc::clone(&self.0),
                user: userId,
                cb: cb.clone(),
            })),
            BinderFeatures::default(),
        ))
    }
}

/// One fingerprint operation, cancelled through the broker it waits on.
#[derive(Default)]
struct Operation {
    socket: Mutex<Option<Arc<SeqPacket>>>,
    cancelled: AtomicBool,
}

impl Operation {
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        if let Some(socket) = lock(&self.socket).as_ref() {
            let _ = socket.send_record(&Message::FingerprintCancel.encode(), &[]);
        }
    }
}

struct Cancel(Arc<Operation>);

impl Interface for Cancel {}

impl ICancellationSignal for Cancel {
    fn cancel(&self) -> binder::Result<()> {
        self.0.cancel();
        Ok(())
    }
}

fn signal(operation: Arc<Operation>) -> Strong<dyn ICancellationSignal> {
    BnCancellationSignal::new_binder(Cancel(operation), BinderFeatures::default())
}

struct SessionState {
    authentication: Arc<Authentication>,
    user: i32,
    cb: Strong<dyn ISessionCallback>,
}

impl SessionState {
    /// A timed lockout's remaining milliseconds, or `None` for a permanent one.
    fn lockout(&self) -> Option<Option<i64>> {
        if self.authentication.fingerprint_user(self.user).locked_out() {
            return Some(None);
        }
        let until = *lock(&self.authentication.fingerprints)
            .timed
            .get(&self.user)?;
        let remaining = until - boottime_ms();
        (remaining > 0).then_some(Some(remaining))
    }

    /// Counts a rejected finger; a timed lockout ends by itself.
    fn rejected(self: &Arc<Self>) -> Lockout {
        let authentication = &self.authentication;
        let mut record = authentication.fingerprint_user(self.user);
        let lockout = record.fail();
        authentication.save_fingerprint_user(self.user, &record);
        if lockout == Lockout::Timed {
            let until = boottime_ms() + i64::try_from(TIMED_LOCKOUT_MS).unwrap_or(i64::MAX);
            lock(&authentication.fingerprints)
                .timed
                .insert(self.user, until);
            let state = Arc::clone(self);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(TIMED_LOCKOUT_MS));
                let ended = {
                    let mut fingerprints = lock(&state.authentication.fingerprints);
                    let ended = fingerprints.timed.get(&state.user) == Some(&until);
                    if ended {
                        fingerprints.timed.remove(&state.user);
                    }
                    ended
                };
                if ended {
                    let _ = state.cb.onLockoutCleared();
                }
            });
        }
        lockout
    }

    fn matched(&self) {
        let mut record = self.authentication.fingerprint_user(self.user);
        if record.failures != 0 {
            record.failures = 0;
            self.authentication
                .save_fingerprint_user(self.user, &record);
        }
        lock(&self.authentication.fingerprints)
            .timed
            .remove(&self.user);
    }

    fn token(&self, challenge: i64, sid: u64, fingers: &[String]) -> Option<BiometricToken> {
        let token = Token {
            challenge,
            user_id: i64::from_ne_bytes(sid.to_ne_bytes()),
            authenticator_id: self
                .authentication
                .authenticator_id(self.user, sid, fingers),
            authenticator_type: FINGERPRINT,
            timestamp_ms: boottime_ms(),
        };
        let mac = self.authentication.sign(&token)?;
        Some(BiometricToken {
            challenge,
            userId: token.user_id,
            authenticatorId: token.authenticator_id,
            authenticatorType: BiometricAuthenticator::FINGERPRINT,
            timestamp: BiometricTimestamp {
                milliSeconds: token.timestamp_ms,
            },
            mac,
        })
    }

    /// A password token from gatekeeper for this user, signed with the shared key.
    fn password_token(&self, hat: &BiometricToken) -> bool {
        hat.authenticatorType == BiometricAuthenticator::PASSWORD
            && self.authentication.signed(
                &Token {
                    challenge: hat.challenge,
                    user_id: hat.userId,
                    authenticator_id: hat.authenticatorId,
                    authenticator_type: PASSWORD,
                    timestamp_ms: hat.timestamp.milliSeconds,
                },
                &hat.mac,
            )
    }

    fn authenticate(self: &Arc<Self>, challenge: i64, operation: &Operation) {
        let cb = &self.cb;
        match self.lockout() {
            Some(None) => {
                let _ = cb.onLockoutPermanent();
                return;
            }
            Some(Some(remaining)) => {
                let _ = cb.onLockoutTimed(remaining);
                return;
            }
            None => {}
        }
        let Some((sid, fingers)) = self.authentication.enrollment(self.user) else {
            let _ = cb.onError(Error::UNABLE_TO_PROCESS, 0);
            return;
        };
        let Ok(socket) = SeqPacket::connect(SOCKET) else {
            let _ = cb.onError(Error::HW_UNAVAILABLE, 0);
            return;
        };
        let socket = Arc::new(socket);
        *lock(&operation.socket) = Some(Arc::clone(&socket));
        if operation.cancelled.load(Ordering::Acquire) {
            let _ = cb.onError(Error::CANCELED, 0);
            return;
        }
        if socket
            .send_record(&Message::FingerprintVerify.encode(), &[])
            .is_err()
        {
            let _ = cb.onError(Error::HW_UNAVAILABLE, 0);
            return;
        }
        let mut bytes = vec![0; MAX_RECORD];
        let mut lockout = Lockout::None;
        loop {
            let scan = match socket
                .receive_record_into(&mut bytes)
                .ok()
                .and_then(|record| Message::decode(record.bytes).ok())
            {
                Some(Message::FingerprintScan(scan)) => scan,
                _ => Scan::Unavailable,
            };
            match scan {
                Scan::Touch | Scan::Lift => {}
                Scan::Retry => {
                    let _ = cb.onAcquired(AcquiredInfo::INSUFFICIENT, 0);
                }
                Scan::NoMatch if lockout == Lockout::None => {
                    let _ = cb.onAcquired(AcquiredInfo::GOOD, 0);
                    lockout = self.rejected();
                    let _ = cb.onAuthenticationFailed();
                    if lockout != Lockout::None {
                        operation.cancel();
                    }
                }
                Scan::NoMatch => {}
                Scan::Match if lockout == Lockout::None => {
                    self.matched();
                    let _ = cb.onAcquired(AcquiredInfo::GOOD, 0);
                    match self.token(challenge, sid, &fingers) {
                        Some(hat) => {
                            let _ = cb.onAuthenticationSucceeded(ENROLLMENT_ID, &hat);
                        }
                        None => {
                            eprintln!(
                                "droidloom-authentication: no shared key yet; fingerprint not attested"
                            );
                            let _ = cb.onError(Error::HW_UNAVAILABLE, 0);
                        }
                    }
                    return;
                }
                Scan::Match | Scan::Unavailable | Scan::Cancelled => {
                    let _ = match lockout {
                        Lockout::Timed => {
                            cb.onLockoutTimed(i64::try_from(TIMED_LOCKOUT_MS).unwrap_or(i64::MAX))
                        }
                        Lockout::Permanent => cb.onLockoutPermanent(),
                        Lockout::None
                            if scan == Scan::Cancelled
                                || operation.cancelled.load(Ordering::Acquire) =>
                        {
                            cb.onError(Error::CANCELED, 0)
                        }
                        Lockout::None => cb.onError(Error::HW_UNAVAILABLE, 0),
                    };
                    return;
                }
            }
        }
    }
}

struct Session(Arc<SessionState>);

impl Interface for Session {}

impl Session {
    fn start_authentication(&self, operationId: i64) -> Strong<dyn ICancellationSignal> {
        let operation = Arc::new(Operation::default());
        let (state, running) = (Arc::clone(&self.0), Arc::clone(&operation));
        std::thread::spawn(move || state.authenticate(operationId, &running));
        signal(operation)
    }

    fn start_enrollment(&self, hat: &BiometricToken) -> Strong<dyn ICancellationSignal> {
        let state = &self.0;
        let issued = lock(&state.authentication.fingerprints)
            .challenges
            .remove(&hat.challenge);
        let enrolled = issued
            && state.password_token(hat)
            && state.authentication.enrollment(state.user).is_some();
        // The enrollment is the host's: Android adds no prints of its own.
        let _ = if enrolled {
            state.cb.onEnrollmentProgress(ENROLLMENT_ID, 0)
        } else {
            state.cb.onError(Error::UNABLE_TO_PROCESS, 0)
        };
        signal(Arc::new(Operation::default()))
    }

    fn refuse_detection(&self) -> Strong<dyn ICancellationSignal> {
        let _ = self.0.cb.onError(Error::UNABLE_TO_PROCESS, 0);
        signal(Arc::new(Operation::default()))
    }
}

impl ISession for Session {
    fn generateChallenge(&self) -> binder::Result<()> {
        let challenge = i64::from_ne_bytes(random());
        lock(&self.0.authentication.fingerprints)
            .challenges
            .insert(challenge);
        self.0.cb.onChallengeGenerated(challenge)
    }

    fn revokeChallenge(&self, challenge: i64) -> binder::Result<()> {
        lock(&self.0.authentication.fingerprints)
            .challenges
            .remove(&challenge);
        self.0.cb.onChallengeRevoked(challenge)
    }

    fn enroll(&self, hat: &BiometricToken) -> binder::Result<Strong<dyn ICancellationSignal>> {
        Ok(self.start_enrollment(hat))
    }

    fn authenticate(&self, operationId: i64) -> binder::Result<Strong<dyn ICancellationSignal>> {
        Ok(self.start_authentication(operationId))
    }

    fn detectInteraction(&self) -> binder::Result<Strong<dyn ICancellationSignal>> {
        Ok(self.refuse_detection())
    }

    fn enumerateEnrollments(&self) -> binder::Result<()> {
        let enrolled = self.0.authentication.enrollment(self.0.user).is_some();
        let ids: &[i32] = if enrolled { &[ENROLLMENT_ID] } else { &[] };
        self.0.cb.onEnrollmentsEnumerated(ids)
    }

    fn removeEnrollments(&self, _enrollmentIds: &[i32]) -> binder::Result<()> {
        // The host's fingers stay Android's; only the host removes them.
        self.0.cb.onError(Error::UNABLE_TO_REMOVE, 0)
    }

    fn getAuthenticatorId(&self) -> binder::Result<()> {
        let state = &self.0;
        let id = state
            .authentication
            .enrollment(state.user)
            .map_or(0, |(sid, fingers)| {
                state
                    .authentication
                    .authenticator_id(state.user, sid, &fingers)
            });
        state.cb.onAuthenticatorIdRetrieved(id)
    }

    fn invalidateAuthenticatorId(&self) -> binder::Result<()> {
        let state = &self.0;
        let mut record = state.authentication.fingerprint_user(state.user);
        record.invalidations = record.invalidations.wrapping_add(1);
        state
            .authentication
            .save_fingerprint_user(state.user, &record);
        let id = state
            .authentication
            .enrollment(state.user)
            .map_or(0, |(sid, fingers)| {
                state
                    .authentication
                    .authenticator_id(state.user, sid, &fingers)
            });
        state.cb.onAuthenticatorIdInvalidated(id)
    }

    fn resetLockout(&self, hat: &BiometricToken) -> binder::Result<()> {
        let state = &self.0;
        if !state.password_token(hat) {
            return state.cb.onError(Error::UNABLE_TO_PROCESS, 0);
        }
        let mut record = state.authentication.fingerprint_user(state.user);
        record.failures = 0;
        state
            .authentication
            .save_fingerprint_user(state.user, &record);
        lock(&state.authentication.fingerprints)
            .timed
            .remove(&state.user);
        state.cb.onLockoutCleared()
    }

    fn close(&self) -> binder::Result<()> {
        self.0.cb.onSessionClosed()
    }

    fn onPointerDown(
        &self,
        _pointerId: i32,
        _x: i32,
        _y: i32,
        _minor: f32,
        _major: f32,
    ) -> binder::Result<()> {
        Ok(())
    }

    fn onPointerUp(&self, _pointerId: i32) -> binder::Result<()> {
        Ok(())
    }

    fn onUiReady(&self) -> binder::Result<()> {
        Ok(())
    }

    fn authenticateWithContext(
        &self,
        operationId: i64,
        _context: &OperationContext,
    ) -> binder::Result<Strong<dyn ICancellationSignal>> {
        Ok(self.start_authentication(operationId))
    }

    fn enrollWithContext(
        &self,
        hat: &BiometricToken,
        _context: &OperationContext,
    ) -> binder::Result<Strong<dyn ICancellationSignal>> {
        Ok(self.start_enrollment(hat))
    }

    fn detectInteractionWithContext(
        &self,
        _context: &OperationContext,
    ) -> binder::Result<Strong<dyn ICancellationSignal>> {
        Ok(self.refuse_detection())
    }

    fn onPointerDownWithContext(&self, _context: &PointerContext) -> binder::Result<()> {
        Ok(())
    }

    fn onPointerUpWithContext(&self, _context: &PointerContext) -> binder::Result<()> {
        Ok(())
    }

    fn onContextChanged(&self, _context: &OperationContext) -> binder::Result<()> {
        Ok(())
    }

    fn onPointerCancelWithContext(&self, _context: &PointerContext) -> binder::Result<()> {
        Ok(())
    }

    fn setIgnoreDisplayTouches(&self, _shouldIgnore: bool) -> binder::Result<()> {
        Ok(())
    }
}

fn main() {
    let store = Store(PathBuf::from(STATE));
    let key = match store.key() {
        Ok(key) => key,
        Err(error) => {
            eprintln!("droidloom-authentication: no key in {STATE}: {error}");
            std::process::exit(1);
        }
    };
    let authentication = Arc::new(Authentication {
        store,
        key,
        nonce: random(),
        shared: Mutex::new(None),
        gatekeeper: Mutex::new(()),
        fingerprints: Mutex::new(Fingerprints::default()),
    });
    ProcessState::start_thread_pool();
    // Keystore negotiates with every declared participant before the first
    // token is signed, so the shared secret registers first.
    let services = [
        (
            SHARED_SECRET,
            BnSharedSecret::new_binder(
                SharedSecret(Arc::clone(&authentication)),
                BinderFeatures::default(),
            )
            .as_binder(),
        ),
        (
            GATEKEEPER,
            BnGatekeeper::new_binder(
                Gatekeeper(Arc::clone(&authentication)),
                BinderFeatures::default(),
            )
            .as_binder(),
        ),
        (
            FINGERPRINT_SERVICE,
            BnFingerprint::new_binder(Fingerprint(authentication), BinderFeatures::default())
                .as_binder(),
        ),
    ];
    for (name, binder) in services {
        if let Err(error) = binder::add_service(name, binder) {
            eprintln!("droidloom-authentication: {name} not registered: {error:?}");
            std::process::exit(1);
        }
    }
    ProcessState::join_thread_pool();
}
