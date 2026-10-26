//! F04: bridge token must come from the OS CSPRNG on every platform,
//! never from a time+pid fallback. This test fails (compile error)
//! until `generate_secure_token` exists in the engine.

use fetchforge_engine::generate_secure_token;
use std::collections::HashSet;

#[test]
fn secure_token_is_32_hex_and_unique() {
    let mut seen = HashSet::new();
    for _ in 0..1000 {
        let tok = generate_secure_token().expect("CSPRNG must be available");
        assert_eq!(tok.len(), 32, "{tok}");
        assert!(tok.chars().all(|c| c.is_ascii_hexdigit()), "{tok}");
        assert!(seen.insert(tok), "duplicate token");
    }
}
