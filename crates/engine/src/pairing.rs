//! One-time pairing codes and challenge-response authentication.
//!
//! Design (see docs/TOKEN_PAIRING_DESIGN.md, approved with conditions):
//! * Code: 8 characters from a 32-character alphabet with no look-alikes
//!   (`ABCDEFGHJKLMNPQRSTUVWXYZ23456789` — no 0/O, 1/I/L). 8 chars x 5 bits
//!   = 40 bits of entropy, generated with the OS CSPRNG (`getrandom`,
//!   which is BCryptGenRandom on Windows and getrandom/urandom on Linux).
//! * Storage: only the SHA-256 hash of the code is kept, in memory only,
//!   never written to disk and never logged. The plaintext code exists
//!   only in the app's Pair dialog and in the user's hands for 60 seconds.
//! * Expiry: 60 seconds from generation, single use, one active code at
//!   a time (generating a new code invalidates the old one).
//! * Lockout is global: failed attempts are counted on the single shared
//!   PairingState (all clients are loopback), and after 5 wrong attempts
//!   the active code is INVALIDATED — it cannot be used even with the
//!   correct code afterwards. A new code must be generated in the app.
//! * Constant-time compare: hash comparison iterates over all 32 bytes
//!   and never returns early, so a wrong code does not leak how many
//!   bytes matched through timing.
//! * Challenge-response: before the extension sends its stored token to
//!   a server, it challenges that server with a random nonce; the server
//!   must answer with HMAC-SHA256(token, nonce). The extension verifies
//!   the HMAC with its stored token and refuses to send the token if it
//!   does not match. This proves the server knows the token — it does
//!   NOT prove anything about a server the extension has never paired
//!   with (see the honest threat model in TOKEN_PAIRING_DESIGN.md).

use sha2::{Digest, Sha256};
use std::time::{Duration, Instant};

/// 32-character alphabet, no look-alikes: no 0, O, 1, I, or L.
pub const CODE_ALPHABET: &[u8; 32] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
pub const CODE_LENGTH: usize = 8;
pub const CODE_TTL: Duration = Duration::from_secs(60);
pub const MAX_WRONG_ATTEMPTS: u32 = 5;

/// SHA-256 of a pairing code. Kept in memory only.
pub fn hash_code(code: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(code.as_bytes());
    hasher.finalize().into()
}

/// Constant-time equality for fixed-length hashes: no early return.
pub fn constant_time_eq(a: &[u8; 32], b: &[u8; 32]) -> bool {
    let mut diff: u8 = 0;
    for i in 0..32 {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

/// Generate a pairing code with the OS CSPRNG. Each random byte mod 32
/// is uniform because 256 is divisible by 32.
pub fn generate_code() -> Result<String, String> {
    let mut bytes = [0u8; CODE_LENGTH];
    getrandom::getrandom(&mut bytes).map_err(|e| format!("CSPRNG failed: {e}"))?;
    Ok(bytes
        .iter()
        .map(|b| CODE_ALPHABET[(b % 32) as usize] as char)
        .collect())
}

/// Display form: grouped as XXXX-XXXX for typing. The hyphen is not
/// part of the code; verification strips hyphens and spaces.
pub fn display_code(code: &str) -> String {
    if code.len() == CODE_LENGTH {
        format!("{}-{}", &code[..4], &code[4..])
    } else {
        code.to_string()
    }
}

pub fn normalise_code(input: &str) -> String {
    input
        .chars()
        .filter(|c| *c != '-' && *c != ' ')
        .collect::<String>()
        .to_ascii_uppercase()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairingError {
    NoActiveCode,
    Expired,
    AlreadyUsed,
    WrongCode,
    /// After 5 wrong attempts the active code was invalidated.
    Invalidated,
}

impl std::fmt::Display for PairingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PairingError::NoActiveCode => write!(f, "No pairing code is active"),
            PairingError::Expired => write!(f, "Pairing code expired"),
            PairingError::AlreadyUsed => write!(f, "Pairing code already used"),
            PairingError::WrongCode => write!(f, "Wrong pairing code"),
            PairingError::Invalidated => {
                write!(f, "Pairing code invalidated after too many wrong attempts")
            }
        }
    }
}

/// Single shared pairing state. One active code at a time; attempts are
/// global because every client is loopback.
pub struct PairingState {
    hash: Option<[u8; 32]>,
    expires_at: Option<Instant>,
    used: bool,
    wrong_attempts: u32,
}

impl Default for PairingState {
    fn default() -> Self {
        Self::new()
    }
}

impl PairingState {
    pub fn new() -> Self {
        Self {
            hash: None,
            expires_at: None,
            used: false,
            wrong_attempts: 0,
        }
    }

    /// Generate a new code, invalidating any previous one, and reset
    /// the wrong-attempt counter. Returns the plaintext code (the caller
    /// displays it in the app; the state keeps only its hash).
    pub fn generate(&mut self) -> Result<String, String> {
        let code = generate_code()?;
        self.hash = Some(hash_code(&code));
        self.expires_at = Some(Instant::now() + CODE_TTL);
        self.used = false;
        self.wrong_attempts = 0;
        Ok(code)
    }

    pub fn has_active(&self) -> bool {
        self.hash.is_some() && !self.used
    }

    pub fn wrong_attempts(&self) -> u32 {
        self.wrong_attempts
    }

    pub fn verify(&mut self, input: &str) -> Result<(), PairingError> {
        self.verify_at(input, Instant::now())
    }

    /// Testable core: `now` is injected so expiry can be tested without
    /// waiting 60 seconds.
    pub fn verify_at(&mut self, input: &str, now: Instant) -> Result<(), PairingError> {
        if self.used {
            return Err(PairingError::AlreadyUsed);
        }
        let hash = match self.hash {
            Some(h) => h,
            None => return Err(PairingError::NoActiveCode),
        };
        if let Some(exp) = self.expires_at {
            if now > exp {
                self.invalidate();
                return Err(PairingError::Expired);
            }
        }
        let candidate = hash_code(&normalise_code(input));
        if constant_time_eq(&hash, &candidate) {
            self.used = true;
            self.hash = None;
            self.expires_at = None;
            Ok(())
        } else {
            self.wrong_attempts += 1;
            if self.wrong_attempts >= MAX_WRONG_ATTEMPTS {
                self.invalidate();
                return Err(PairingError::Invalidated);
            }
            Err(PairingError::WrongCode)
        }
    }

    fn invalidate(&mut self) {
        self.hash = None;
        self.expires_at = None;
        self.used = false;
        self.wrong_attempts = 0;
    }
}

/// HMAC-SHA256(key=token, message=nonce), hex-encoded, for the
/// challenge-response check. The token itself is never sent in a
/// challenge; only this HMAC is.
pub fn challenge_hmac(token: &str, nonce: &str) -> String {
    use hmac::{Hmac, Mac};
    type HmacSha256 = Hmac<Sha256>;
    let mut mac =
        HmacSha256::new_from_slice(token.as_bytes()).expect("HMAC key length is always valid");
    mac.update(nonce.as_bytes());
    let out = mac.finalize().into_bytes();
    out.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_uses_only_safe_alphabet_and_has_40_bits() {
        for _ in 0..100 {
            let code = generate_code().unwrap();
            assert_eq!(code.len(), CODE_LENGTH);
            assert!(
                code.bytes().all(|b| CODE_ALPHABET.contains(&b)),
                "code {code} contains a look-alike or unexpected char"
            );
            // No look-alikes at all.
            assert!(!code.contains(['0', 'O', '1', 'I', 'l']));
        }
        // Two codes are (overwhelmingly likely) different.
        assert_ne!(generate_code().unwrap(), generate_code().unwrap());
    }

    #[test]
    fn display_and_normalise_round_trip() {
        assert_eq!(display_code("ABCDEFGH"), "ABCD-EFGH");
        assert_eq!(normalise_code("abcd-efgh"), "ABCDEFGH");
        assert_eq!(normalise_code(" ab cd "), "ABCD");
    }

    #[test]
    fn verify_correct_single_use_and_old_code_invalidated() {
        let mut s = PairingState::new();
        let code1 = s.generate().unwrap();
        let code2 = s.generate().unwrap();
        // Generating code2 invalidated code1.
        assert!(s.verify(&code1).is_err());
        assert!(s.verify(&code2).is_ok());
        // Single use: the same code cannot be used twice.
        assert_eq!(s.verify(&code2), Err(PairingError::AlreadyUsed));
    }

    #[test]
    fn expiry_after_60_seconds() {
        let mut s = PairingState::new();
        let code = s.generate().unwrap();
        let future = Instant::now() + Duration::from_secs(61);
        assert_eq!(s.verify_at(&code, future), Err(PairingError::Expired));
        // Expired code is gone, not reusable.
        assert_eq!(s.verify(&code), Err(PairingError::NoActiveCode));
    }

    #[test]
    fn five_wrong_attempts_invalidate_the_code() {
        let mut s = PairingState::new();
        let code = s.generate().unwrap();
        for _ in 0..4 {
            assert_eq!(s.verify("WRONG123"), Err(PairingError::WrongCode));
        }
        // Fifth wrong attempt invalidates, it does not just say wrong.
        assert_eq!(s.verify("WRONG123"), Err(PairingError::Invalidated));
        // Even the correct code no longer works after invalidation.
        assert_eq!(s.verify(&code), Err(PairingError::NoActiveCode));
    }

    #[test]
    fn constant_time_eq_behaves_correctly() {
        let a = hash_code("ABCDEFGH");
        assert!(constant_time_eq(&a, &hash_code("ABCDEFGH")));
        assert!(!constant_time_eq(&a, &hash_code("ABCDEFGI")));
    }

    #[test]
    fn challenge_hmac_matches_rfc4231_test_case_1() {
        // RFC 4231 Test Case 1: key = 20 x 0x0b, data = "Hi There".
        use hmac::{Hmac, Mac};
        type HmacSha256 = Hmac<Sha256>;
        let mut mac = HmacSha256::new_from_slice(&[0x0b; 20]).unwrap();
        mac.update(b"Hi There");
        let out: String = mac
            .finalize()
            .into_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(
            out,
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
        // And the helper is deterministic for a given token + nonce.
        assert_eq!(
            challenge_hmac("token123", "nonce456"),
            challenge_hmac("token123", "nonce456")
        );
        assert_ne!(
            challenge_hmac("token123", "nonce456"),
            challenge_hmac("token123", "other-nonce")
        );
        assert_ne!(
            challenge_hmac("token123", "nonce456"),
            challenge_hmac("other-token", "nonce456")
        );
    }
}
