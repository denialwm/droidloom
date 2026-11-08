//! The rules of Droidloom's Android gatekeeper, fingerprint sensor and
//! shared-secret participant, apart from binder, files and crypto.
//!
//! Keystore negotiates one HMAC key with every shared-secret participant; the
//! service signs gatekeeper and fingerprint auth tokens with it, so
//! `KeyMint` accepts both. Password handles and authenticator IDs are MACs under the
//! service's own key. The caller supplies HMAC-SHA256 and the clock.

/// `HardwareAuthenticatorType.PASSWORD`.
pub const PASSWORD: i32 = 1;
/// `HardwareAuthenticatorType.FINGERPRINT`.
pub const FINGERPRINT: i32 = 2;
/// Length of every shared-secret nonce.
pub const NONCE_LEN: usize = 32;
/// Length of a password handle's salt.
pub const SALT_LEN: usize = 16;
/// Length of an HMAC-SHA256.
pub const MAC_LEN: usize = 32;

/// The fields of a hardware auth token its MAC covers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Token {
    /// The operation it authorizes.
    pub challenge: i64,
    /// The user's secure ID.
    pub user_id: i64,
    /// The authenticator's ID; 0 for a password.
    pub authenticator_id: i64,
    /// [`PASSWORD`] or [`FINGERPRINT`].
    pub authenticator_type: i32,
    /// `CLOCK_BOOTTIME` in milliseconds.
    pub timestamp_ms: i64,
}

impl Token {
    /// The bytes `KeyMint` MACs: a zero version, three IDs in host order, then
    /// the type and timestamp in network order.
    pub fn mac_input(&self) -> [u8; 37] {
        let mut bytes = [0; 37];
        bytes[1..9].copy_from_slice(&self.challenge.to_ne_bytes());
        bytes[9..17].copy_from_slice(&self.user_id.to_ne_bytes());
        bytes[17..25].copy_from_slice(&self.authenticator_id.to_ne_bytes());
        bytes[25..29].copy_from_slice(&self.authenticator_type.to_be_bytes());
        bytes[29..37].copy_from_slice(&self.timestamp_ms.to_be_bytes());
        bytes
    }
}

/// The context every participant derives the shared HMAC key from: each
/// participant's seed and nonce in the order Keystore lists them. `None`
/// when a nonce is malformed or this participant's own (empty seed, `nonce`)
/// is missing.
pub fn shared_secret_context(participants: &[(&[u8], &[u8])], nonce: &[u8]) -> Option<Vec<u8>> {
    let mut context = Vec::new();
    let mut own = false;
    for (seed, theirs) in participants {
        if theirs.len() != NONCE_LEN {
            return None;
        }
        own |= seed.is_empty() && *theirs == nonce;
        context.extend_from_slice(seed);
        context.extend_from_slice(theirs);
    }
    own.then_some(context)
}

/// Compares MACs in constant time.
pub fn same_mac(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .fold(0, |difference, (x, y)| difference | (x ^ y))
            == 0
}

/// A gatekeeper password handle: the secure ID it grants, the user it was
/// enrolled for, a salt, and a MAC over those and the password.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Handle {
    /// The secure user ID.
    pub sid: u64,
    /// The gatekeeper user.
    pub uid: u32,
    /// Random per enrollment.
    pub salt: [u8; SALT_LEN],
    /// HMAC-SHA256 of [`Handle::mac_input`].
    pub mac: [u8; MAC_LEN],
}

const HANDLE_VERSION: u8 = 1;
const HANDLE_LEN: usize = 1 + 8 + 4 + SALT_LEN + MAC_LEN;

impl Handle {
    /// The bytes the handle's MAC covers.
    pub fn mac_input(sid: u64, uid: u32, salt: &[u8; SALT_LEN], password: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(1 + 8 + 4 + SALT_LEN + password.len());
        bytes.push(HANDLE_VERSION);
        bytes.extend_from_slice(&sid.to_le_bytes());
        bytes.extend_from_slice(&uid.to_le_bytes());
        bytes.extend_from_slice(salt);
        bytes.extend_from_slice(password);
        bytes
    }

    /// The handle's bytes.
    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(HANDLE_LEN);
        bytes.push(HANDLE_VERSION);
        bytes.extend_from_slice(&self.sid.to_le_bytes());
        bytes.extend_from_slice(&self.uid.to_le_bytes());
        bytes.extend_from_slice(&self.salt);
        bytes.extend_from_slice(&self.mac);
        bytes
    }

    /// The handle in `bytes`, if it is one of this version.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != HANDLE_LEN || bytes[0] != HANDLE_VERSION {
            return None;
        }
        Some(Self {
            sid: u64::from_le_bytes(bytes[1..9].try_into().ok()?),
            uid: u32::from_le_bytes(bytes[9..13].try_into().ok()?),
            salt: bytes[13..13 + SALT_LEN].try_into().ok()?,
            mac: bytes[13 + SALT_LEN..].try_into().ok()?,
        })
    }
}

/// How long gatekeeper refuses after `failures` wrong passwords in a row:
/// 30 s after every fifth of the first ten, after each one up to 30, then
/// doubling every ten up to a day.
pub fn retry_timeout_ms(failures: u32) -> u32 {
    const DAY: u32 = 24 * 60 * 60 * 1000;
    match failures {
        1..=10 if failures % 5 == 0 => 30_000,
        0..=10 => 0,
        11..=29 => 30_000,
        30..=139 => 30_000u32
            .saturating_mul(1 << ((failures - 30) / 10))
            .min(DAY),
        _ => DAY,
    }
}

/// A gatekeeper user: the secure ID enrolled last and its wrong passwords.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct GatekeeperUser {
    /// The secure user ID.
    pub sid: u64,
    /// Wrong passwords since the last right one.
    pub failures: u32,
    /// When the last one was given, `CLOCK_BOOTTIME` milliseconds.
    pub last_failure_ms: u64,
}

impl GatekeeperUser {
    /// How long verification is refused at `now_ms`. A clock behind the
    /// last failure (the host restarted) counts the wait as over.
    pub fn remaining_ms(&self, now_ms: u64) -> u32 {
        let timeout = u64::from(retry_timeout_ms(self.failures));
        let elapsed = now_ms.checked_sub(self.last_failure_ms).unwrap_or(timeout);
        u32::try_from(timeout.saturating_sub(elapsed)).unwrap_or(u32::MAX)
    }

    /// The record's bytes.
    pub fn encode(&self) -> [u8; 20] {
        let mut bytes = [0; 20];
        bytes[..8].copy_from_slice(&self.sid.to_le_bytes());
        bytes[8..12].copy_from_slice(&self.failures.to_le_bytes());
        bytes[12..].copy_from_slice(&self.last_failure_ms.to_le_bytes());
        bytes
    }

    /// The record in `bytes`.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let bytes: &[u8; 20] = bytes.try_into().ok()?;
        Some(Self {
            sid: u64::from_le_bytes(bytes[..8].try_into().ok()?),
            failures: u32::from_le_bytes(bytes[8..12].try_into().ok()?),
            last_failure_ms: u64::from_le_bytes(bytes[12..].try_into().ok()?),
        })
    }
}

/// The one enrollment that stands for every finger the host user enrolled.
pub const ENROLLMENT_ID: i32 = 1;
/// Failures in a row that lock fingerprints for [`TIMED_LOCKOUT_MS`].
pub const TIMED_LOCKOUT_FAILURES: u32 = 5;
/// Failures in a row after which only the device credential unlocks.
pub const PERMANENT_LOCKOUT_FAILURES: u32 = 20;
/// How long a timed lockout lasts.
pub const TIMED_LOCKOUT_MS: u64 = 30_000;

/// What a rejected finger leads to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Lockout {
    /// The reader keeps listening.
    None,
    /// Fingerprints are refused for [`TIMED_LOCKOUT_MS`].
    Timed,
    /// Fingerprints are refused until the device credential is verified.
    Permanent,
}

/// A user's fingerprint state.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FingerprintUser {
    /// Rejected fingers since the last match or reset.
    pub failures: u32,
    /// Times Android invalidated the authenticator ID.
    pub invalidations: u64,
}

impl FingerprintUser {
    /// Counts a rejected finger.
    pub fn fail(&mut self) -> Lockout {
        self.failures = self.failures.saturating_add(1);
        if self.failures >= PERMANENT_LOCKOUT_FAILURES {
            Lockout::Permanent
        } else if self.failures % TIMED_LOCKOUT_FAILURES == 0 {
            Lockout::Timed
        } else {
            Lockout::None
        }
    }

    /// Only the device credential unlocks fingerprints.
    pub fn locked_out(&self) -> bool {
        self.failures >= PERMANENT_LOCKOUT_FAILURES
    }

    /// The record's bytes.
    pub fn encode(&self) -> [u8; 12] {
        let mut bytes = [0; 12];
        bytes[..4].copy_from_slice(&self.failures.to_le_bytes());
        bytes[4..].copy_from_slice(&self.invalidations.to_le_bytes());
        bytes
    }

    /// The record in `bytes`.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let bytes: &[u8; 12] = bytes.try_into().ok()?;
        Some(Self {
            failures: u32::from_le_bytes(bytes[..4].try_into().ok()?),
            invalidations: u64::from_le_bytes(bytes[4..].try_into().ok()?),
        })
    }
}

/// The bytes an authenticator ID is the MAC of: it changes with the user's
/// secure ID, Android's invalidations and the set of host fingers, so keys
/// bound to "the current fingerprints" end when a finger is added or removed.
pub fn authenticator_id_input(
    user: i32,
    sid: u64,
    invalidations: u64,
    fingers: &[String],
) -> Vec<u8> {
    let mut sorted: Vec<&str> = fingers.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    sorted.dedup();
    let mut bytes = b"droidloom fingerprint authenticator\0".to_vec();
    bytes.extend_from_slice(&user.to_le_bytes());
    bytes.extend_from_slice(&sid.to_le_bytes());
    bytes.extend_from_slice(&invalidations.to_le_bytes());
    for finger in sorted {
        bytes.extend_from_slice(finger.as_bytes());
        bytes.push(0);
    }
    bytes
}

/// An authenticator ID from its MAC; never 0, which means no enrollment.
pub fn authenticator_id(mac: &[u8; MAC_LEN]) -> i64 {
    let mut bytes = [0; 8];
    bytes.copy_from_slice(&mac[..8]);
    let id = i64::from_le_bytes(bytes);
    if id == 0 { 1 } else { id }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_mac_input_follows_keymint() {
        let token = Token {
            challenge: 0x0102_0304_0506_0708,
            user_id: 9,
            authenticator_id: -1,
            authenticator_type: FINGERPRINT,
            timestamp_ms: 0x0a0b,
        };
        let bytes = token.mac_input();
        assert_eq!(bytes[0], 0);
        assert_eq!(bytes[1..9], 0x0102_0304_0506_0708i64.to_ne_bytes());
        assert_eq!(bytes[17..25], [0xff; 8]);
        assert_eq!(bytes[25..29], [0, 0, 0, 2]);
        assert_eq!(bytes[29..37], [0, 0, 0, 0, 0, 0, 0x0a, 0x0b]);
    }

    #[test]
    fn the_shared_context_needs_our_own_nonce() {
        let ours = [7u8; NONCE_LEN];
        let keymint = [1u8; NONCE_LEN];
        let context =
            shared_secret_context(&[(b"", &keymint), (b"", &ours)], &ours).expect("context");
        assert_eq!(context.len(), 2 * NONCE_LEN);
        assert_eq!(context[..NONCE_LEN], keymint);
        assert!(shared_secret_context(&[(b"", &keymint)], &ours).is_none());
        assert!(shared_secret_context(&[(b"", &ours), (b"", &[1, 2])], &ours).is_none());
    }

    #[test]
    fn handles_round_trip_and_refuse_other_shapes() {
        let handle = Handle {
            sid: 42,
            uid: 100_000,
            salt: [3; SALT_LEN],
            mac: [4; MAC_LEN],
        };
        assert_eq!(Handle::decode(&handle.encode()), Some(handle.clone()));
        let mut other = handle.encode();
        other[0] = 2;
        assert_eq!(Handle::decode(&other), None);
        assert_eq!(Handle::decode(&handle.encode()[1..]), None);
    }

    #[test]
    fn gatekeeper_waits_grow() {
        assert_eq!(retry_timeout_ms(4), 0);
        assert_eq!(retry_timeout_ms(5), 30_000);
        assert_eq!(retry_timeout_ms(6), 0);
        assert_eq!(retry_timeout_ms(11), 30_000);
        assert_eq!(retry_timeout_ms(40), 60_000);
        assert_eq!(retry_timeout_ms(500), 86_400_000);
        let user = GatekeeperUser {
            sid: 1,
            failures: 5,
            last_failure_ms: 1_000,
        };
        assert_eq!(user.remaining_ms(11_000), 20_000);
        assert_eq!(user.remaining_ms(500), 0);
        assert_eq!(GatekeeperUser::decode(&user.encode()), Some(user));
    }

    #[test]
    fn fingerprint_lockouts_follow_failures() {
        let mut user = FingerprintUser::default();
        let outcomes: Vec<Lockout> = (0..20).map(|_| user.fail()).collect();
        assert_eq!(outcomes[3], Lockout::None);
        assert_eq!(outcomes[4], Lockout::Timed);
        assert_eq!(outcomes[9], Lockout::Timed);
        assert_eq!(outcomes[19], Lockout::Permanent);
        assert!(user.locked_out());
        assert_eq!(FingerprintUser::decode(&user.encode()), Some(user));
    }

    #[test]
    fn authenticator_ids_follow_the_finger_set_not_its_order() {
        let a = authenticator_id_input(0, 5, 0, &["right-thumb".into(), "left-thumb".into()]);
        let b = authenticator_id_input(0, 5, 0, &["left-thumb".into(), "right-thumb".into()]);
        let c = authenticator_id_input(0, 5, 0, &["left-thumb".into()]);
        let d = authenticator_id_input(0, 5, 1, &["left-thumb".into(), "right-thumb".into()]);
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_ne!(a, d);
        assert_eq!(authenticator_id(&[0; MAC_LEN]), 1);
    }

    #[test]
    fn macs_compare_whole() {
        assert!(same_mac(&[1, 2], &[1, 2]));
        assert!(!same_mac(&[1, 2], &[1, 3]));
        assert!(!same_mac(&[1, 2], &[1]));
    }
}
