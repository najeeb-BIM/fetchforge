//! Secure token generation (F04 fix, audit round 1).
//!
//! Previously the bridge generated its token by reading `/dev/urandom`
//! and falling back to a time ^ pid seed when that file was missing.
//! On Windows `/dev/urandom` does not exist, so the fallback was the
//! *only* path there and the token was predictable. Now tokens come
//! from the OS CSPRNG via the `getrandom` crate on every platform
//! (BCrypt on Windows, getrandom(2) on Linux). There is no fallback:
//! if the OS CSPRNG is unavailable, generation returns an error and
//! the bridge refuses to start rather than minting a weak token.

pub fn generate_secure_token() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).map_err(|e| format!("OS CSPRNG unavailable: {e}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}
