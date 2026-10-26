//! Download IDs (F02 fix, audit round 1).
//!
//! Previously IDs were `dl-<unix seconds>-<pid>`, so every download
//! added in the same second from the same process shared one ID and
//! overwrote the previous record (proven by the audit rapid-adds test:
//! 20 adds -> 1 record). IDs are now 128-bit OS CSPRNG values, so
//! collisions are negligible and adds in the same second are distinct.
//! The fallback (OS CSPRNG unavailable, which should not happen on
//! supported platforms) uses time-nanos + pid + a monotonic counter,
//! which is still unique within and across processes, unlike seconds+pid.

use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

pub fn generate_download_id() -> String {
    let mut bytes = [0u8; 16];
    if getrandom::getrandom(&mut bytes).is_ok() {
        let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        return format!("dl-{hex}");
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let c = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("dl-{nanos:x}-{}-{c}", std::process::id())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn ids_are_unique_and_prefixed() {
        let mut seen = HashSet::new();
        for _ in 0..10_000 {
            let id = generate_download_id();
            assert!(id.starts_with("dl-"), "{id}");
            assert!(seen.insert(id.clone()), "duplicate ID generated: {id}");
        }
    }
}
