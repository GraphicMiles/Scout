//! Small shared helpers.

/// Resolve `rel` against `base`, returning `None` when either is unusable.
/// Fragments and non-http(s) schemes are rejected by callers that care.
pub fn resolve_url(rel: &str, base: &str) -> Option<String> {
    let base = url::Url::parse(base).ok()?;
    let joined = base.join(rel.trim()).ok()?;
    match joined.scheme() {
        "http" | "https" => Some(joined.to_string()),
        _ => None,
    }
}

/// Hex-encoded SHA-256, used to derive stable key identifiers without storing keys.
pub fn sha256_hex(input: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(input.as_bytes());
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

pub fn sha256_bytes(input: &str) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(input.as_bytes());
    h.finalize().into()
}

/// Random URL-safe identifier with a typed prefix.
pub fn random_id(prefix: &str, bytes: usize) -> String {
    use rand::RngCore;
    let mut buf = vec![0u8; bytes];
    rand::thread_rng().fill_bytes(&mut buf);
    let hex: String = buf.iter().map(|b| format!("{b:02x}")).collect();
    format!("{prefix}{hex}")
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_relative_urls() {
        assert_eq!(
            resolve_url("/a", "https://example.com/x/y").as_deref(),
            Some("https://example.com/a")
        );
        assert_eq!(
            resolve_url("b", "https://example.com/x/").as_deref(),
            Some("https://example.com/x/b")
        );
        assert_eq!(resolve_url("javascript:void(0)", "https://example.com/"), None);
        // A bare fragment resolves against the base and keeps the fragment.
        assert_eq!(
            resolve_url("#frag", "https://example.com/").as_deref(),
            Some("https://example.com/#frag")
        );
    }
}
