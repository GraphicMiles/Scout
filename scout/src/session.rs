//! Sessions: isolated cookie jars, one active operation at a time, owned by the
//! API key that created them.

use crate::config::Config;
use crate::error::{ErrorCode, Result, ScoutError};
use crate::util::{now_ms, random_id};
use cookie_store::CookieStore;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};
use tokio_util::sync::CancellationToken;

/// Domain/path/Secure-scoped cookie jar, per session.
pub struct CookieJar {
    store: Mutex<CookieStore>,
}

impl Default for CookieJar {
    fn default() -> Self {
        Self::new()
    }
}

impl CookieJar {
    pub fn new() -> Self {
        Self {
            store: Mutex::new(CookieStore::default()),
        }
    }

    /// `Cookie` header value for a request, or `None` when nothing applies.
    pub fn header_for(&self, url: &str) -> Option<String> {
        let parsed = url::Url::parse(url).ok()?;
        let store = self.store.lock().unwrap();
        let pairs: Vec<String> = store
            .get_request_values(&parsed)
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        if pairs.is_empty() {
            None
        } else {
            Some(pairs.join("; "))
        }
    }

    /// Record `Set-Cookie` headers returned by `url`.
    ///
    /// Cookies carrying a `Domain` attribute are screened before the jar sees
    /// them. Without this, a `Domain=com` cookie would be accepted and then sent
    /// to *every* `.com` host the same session later visits — a real leak for an
    /// agent that browses several sites in one session.
    pub fn store_set_cookies(&self, url: &str, headers: &[(String, String)]) {
        let Ok(parsed) = url::Url::parse(url) else {
            return;
        };
        let mut store = self.store.lock().unwrap();
        for (value, _) in headers {
            if let Some(domain) = domain_attribute(value) {
                if is_public_suffix(&domain) {
                    continue;
                }
            }
            // Remaining errors are malformed cookies or domain mismatches, which
            // the jar already rejects; neither should fail a navigation.
            let _ = store.parse(value, &parsed);
        }
    }

    /// Every cookie in the jar, including HttpOnly ones that page scripts
    /// cannot read. Inspecting session state is the point: which cookies a
    /// target set, over what scope, and when they expire.
    pub fn cookies(&self) -> Vec<serde_json::Value> {
        let store = self.store.lock().unwrap();
        store
            .iter_any()
            .map(|c| {
                serde_json::json!({
                    "name": c.name(),
                    "value": c.value(),
                    "domain": c.domain(),
                    "path": c.path(),
                    "secure": c.secure().unwrap_or(false),
                    "httpOnly": c.http_only().unwrap_or(false),
                    "expires": c.expires().and_then(|e| e.datetime()).map(|d| {
                        format!(
                            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
                            d.year(),
                            u8::from(d.month()),
                            d.day(),
                            d.hour(),
                            d.minute(),
                            d.second()
                        )
                    }),
                })
            })
            .collect()
    }

    /// Seed the jar from a raw cookie value, so a session captured elsewhere
    /// can be resumed without replaying a login.
    pub fn set_cookie(&self, url: &str, value: &str) -> bool {
        let Ok(parsed) = url::Url::parse(url) else {
            return false;
        };
        let mut store = self.store.lock().unwrap();
        store.parse(value, &parsed).is_ok()
    }

    pub fn clear(&self) {
        let mut store = self.store.lock().unwrap();
        *store = CookieStore::default();
    }

    pub fn len(&self) -> usize {
        self.store.lock().unwrap().iter_any().count()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Extract and normalize the `Domain=` attribute from a `Set-Cookie` value.
fn domain_attribute(set_cookie: &str) -> Option<String> {
    for part in set_cookie.split(';').skip(1) {
        let part = part.trim();
        let lower = part.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("domain=") {
            let d = rest.trim().trim_start_matches('.').trim_end_matches('.').to_string();
            if d.is_empty() {
                return None;
            }
            return Some(d);
        }
    }
    None
}

/// Common multi-label public suffixes that must never be used as a cookie domain.
///
/// This is a curated list, not the full Public Suffix List. It covers the cases
/// that matter most in practice; a complete PSL is the correct long-term fix and
/// is noted as a known gap rather than pretended away.
const PUBLIC_SUFFIXES: &[&str] = &[
    "co.uk", "org.uk", "ac.uk", "gov.uk", "me.uk", "net.uk", "sch.uk",
    "com.au", "net.au", "org.au", "edu.au", "gov.au", "id.au",
    "co.jp", "ne.jp", "or.jp", "ac.jp", "go.jp", "com.br", "net.br", "org.br",
    "co.nz", "net.nz", "org.nz", "govt.nz", "co.za", "org.za", "net.za",
    "com.cn", "net.cn", "org.cn", "gov.cn", "edu.cn",
    "co.in", "net.in", "org.in", "gov.in", "com.mx", "com.ar", "com.tr",
    "co.kr", "or.kr", "com.sg", "com.hk", "com.tw", "co.id", "com.ng",
    "co.ke", "com.gh", "co.ug", "com.pk", "com.bd", "com.my", "com.ph",
    "github.io", "pages.dev", "netlify.app", "vercel.app", "herokuapp.com",
    "s3.amazonaws.com", "cloudfront.net", "workers.dev",
];

/// True when `domain` is a bare TLD or a known public suffix.
fn is_public_suffix(domain: &str) -> bool {
    // A single label (`com`, `org`) is always a TLD.
    if !domain.contains('.') {
        return true;
    }
    PUBLIC_SUFFIXES.contains(&domain)
}

pub struct Session {
    pub id: String,
    pub owner: String,
    pub ephemeral: bool,
    pub jar: Arc<CookieJar>,
    pub created_at: i64,
    pub last_used: Mutex<i64>,
    /// The document requests are issued from, so a form POST or an XHR carries
    /// the Referer and Origin a browser would send and a server that checks
    /// them sees a coherent request instead of a bare one.
    referrer: Mutex<Option<String>>,
    pub ctl: CancellationToken,
    /// Serialises operations: one active operation per session, later callers wait.
    op: Arc<AsyncMutex<()>>,
    pub destroyed: std::sync::atomic::AtomicBool,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Deliberately omits the cookie jar: session material must not reach a log.
        f.debug_struct("Session")
            .field("id", &self.id)
            .field("owner", &self.owner)
            .field("ephemeral", &self.ephemeral)
            .finish_non_exhaustive()
    }
}

impl Session {
    pub fn new(owner: String, ephemeral: bool) -> Self {
        Self {
            id: random_id("ses_", 16),
            owner,
            ephemeral,
            jar: Arc::new(CookieJar::new()),
            created_at: now_ms(),
            last_used: Mutex::new(now_ms()),
            referrer: Mutex::new(None),
            ctl: CancellationToken::new(),
            op: Arc::new(AsyncMutex::new(())),
            destroyed: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub async fn lock(&self) -> Result<OwnedMutexGuard<()>> {
        if self.destroyed.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(ScoutError::not_found("Session no longer exists"));
        }
        let guard = self.op.clone().lock_owned().await;
        if self.destroyed.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(ScoutError::not_found("Session no longer exists"));
        }
        *self.last_used.lock().unwrap() = now_ms();
        Ok(guard)
    }

    pub fn destroy(&self) {
        self.destroyed.store(true, std::sync::atomic::Ordering::SeqCst);
        self.ctl.cancel();
        self.jar.clear();
        *self.referrer.lock().unwrap() = None;
    }

    /// Where this session's requests are considered to come from.
    pub fn referrer(&self) -> Option<String> {
        self.referrer.lock().unwrap().clone()
    }

    /// Record the document a session is now on.
    pub fn set_referrer(&self, url: &str) {
        *self.referrer.lock().unwrap() = Some(url.to_string());
    }

    /// Forget the document, as when the jar is emptied and the session is no
    /// longer signed in as whoever it was.
    pub fn clear_referrer(&self) {
        *self.referrer.lock().unwrap() = None;
    }

    /// A signal that fires when either the session is destroyed or the caller aborts.
    pub fn child_token(&self, parent: Option<&CancellationToken>) -> CancellationToken {
        let token = self.ctl.child_token();
        if let Some(p) = parent {
            let t = token.clone();
            let p = p.clone();
            tokio::spawn(async move {
                tokio::select! {
                    _ = p.cancelled() => t.cancel(),
                    _ = t.cancelled() => {}
                }
            });
        }
        token
    }
}

pub struct SessionManager {
    cfg: Config,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
}

impl SessionManager {
    pub fn new(cfg: Config) -> Self {
        Self {
            cfg,
            sessions: Mutex::new(HashMap::new()),
        }
    }

    pub fn size(&self) -> usize {
        self.sessions.lock().unwrap().len()
    }

    pub fn create(self: &Arc<Self>, owner: String, ephemeral: bool) -> Result<Arc<Session>> {
        {
            let mut map = self.sessions.lock().unwrap();
            if map.len() >= self.cfg.max_sessions {
                let now = now_ms();
                let ttl = self.cfg.session_ttl_ms as i64;
                let stale: Vec<String> = map
                    .iter()
                    .filter(|(_, s)| now - *s.last_used.lock().unwrap() > ttl)
                    .map(|(k, _)| k.clone())
                    .collect();
                for id in stale {
                    if let Some(s) = map.remove(&id) {
                        s.destroy();
                    }
                }
            }
            if map.len() >= self.cfg.max_sessions {
                return Err(ScoutError::new(
                    ErrorCode::TooManySessions,
                    format!("Session limit ({}) reached", self.cfg.max_sessions),
                )
                .with_retry_after(5));
            }
            let s = Arc::new(Session::new(owner, ephemeral));
            map.insert(s.id.clone(), s.clone());
            Ok(s)
        }
    }

    /// Sessions are bound to the key that created them; another key sees `not_found`
    /// rather than learning that the id exists.
    pub fn get(&self, owner: &str, id: &str) -> Result<Arc<Session>> {
        let map = self.sessions.lock().unwrap();
        match map.get(id) {
            Some(s) if s.owner == owner => {
                *s.last_used.lock().unwrap() = now_ms();
                Ok(s.clone())
            }
            _ => Err(ScoutError::not_found("Session not found")),
        }
    }

    pub fn destroy(&self, owner: &str, id: &str) -> Result<()> {
        let mut map = self.sessions.lock().unwrap();
        match map.get(id) {
            Some(s) if s.owner == owner => {
                let s = map.remove(id).unwrap();
                s.destroy();
                Ok(())
            }
            _ => Err(ScoutError::not_found("Session not found")),
        }
    }

    /// Unconditional removal used for ephemeral session cleanup.
    pub fn destroy_internal(&self, id: &str) {
        let mut map = self.sessions.lock().unwrap();
        if let Some(s) = map.remove(id) {
            s.destroy();
        }
    }

    pub fn sweep(&self) {
        let now = now_ms();
        let ttl = self.cfg.session_ttl_ms as i64;
        let mut map = self.sessions.lock().unwrap();
        let expired: Vec<String> = map
            .iter()
            .filter(|(_, s)| now - *s.last_used.lock().unwrap() > ttl)
            .map(|(k, _)| k.clone())
            .collect();
        for id in expired {
            if let Some(s) = map.remove(&id) {
                s.destroy();
            }
        }
    }

    pub fn close(&self) {
        let mut map = self.sessions.lock().unwrap();
        for (_, s) in map.drain() {
            s.destroy();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mgr() -> Arc<SessionManager> {
        Arc::new(SessionManager::new(Config::dev()))
    }

    #[test]
    fn cookie_jar_scopes_by_domain_and_path() {
        let jar = CookieJar::new();
        jar.store_set_cookies(
            "https://example.com/a/b",
            &[("sid=abc; Path=/a".to_string(), String::new())],
        );
        assert!(jar.header_for("https://example.com/a/x").is_some());
        assert!(jar.header_for("https://example.com/other").is_none());
        // Not sent to a different domain.
        assert!(jar.header_for("https://other.com/a/x").is_none());
    }

    #[test]
    fn cookie_jar_drops_secure_cookies_over_plain_http() {
        let jar = CookieJar::new();
        jar.store_set_cookies(
            "https://example.com/",
            &[("t=1; Secure".to_string(), String::new())],
        );
        assert!(jar.header_for("https://example.com/").is_some());
        assert!(jar.header_for("http://example.com/").is_none());
    }

    #[test]
    fn cookie_jar_rejects_cross_domain() {
        let jar = CookieJar::new();
        jar.store_set_cookies(
            "https://example.com/",
            &[("x=1; Domain=other.com".to_string(), String::new())],
        );
        assert_eq!(jar.len(), 0, "a site must not set cookies for another domain");
    }

    #[test]
    fn cookie_jar_rejects_subdomain_impersonation() {
        let jar = CookieJar::new();
        jar.store_set_cookies(
            "https://evil.com/",
            &[("z=3; Domain=example.com".to_string(), String::new())],
        );
        assert_eq!(jar.len(), 0);
        assert!(jar.header_for("https://example.com/").is_none());
    }

    /// A bare-TLD cookie would be broadcast to every host under that TLD.
    #[test]
    fn cookie_jar_rejects_bare_tld_domain() {
        let jar = CookieJar::new();
        jar.store_set_cookies(
            "https://example.com/",
            &[("y=2; Domain=com".to_string(), String::new())],
        );
        assert_eq!(jar.len(), 0, "a bare TLD must never be a cookie domain");
        assert!(jar.header_for("https://other.com/").is_none());
        assert!(jar.header_for("https://example.com/").is_none());
    }

    /// Multi-label public suffixes are equally unsafe as cookie domains.
    #[test]
    fn cookie_jar_rejects_public_suffixes() {
        for (host, domain) in [
            ("https://shop.example.co.uk/", "co.uk"),
            ("https://app.example.com.au/", "com.au"),
            ("https://me.github.io/", "github.io"),
        ] {
            let jar = CookieJar::new();
            jar.store_set_cookies(host, &[(format!("s=1; Domain={domain}"), String::new())]);
            assert_eq!(jar.len(), 0, "Domain={domain} must be rejected");
        }
    }

    #[test]
    fn cookie_jar_still_accepts_ordinary_parent_domain() {
        let jar = CookieJar::new();
        jar.store_set_cookies(
            "https://www.example.com/",
            &[("a=1; Domain=example.com".to_string(), String::new())],
        );
        assert_eq!(jar.len(), 1);
        assert!(jar.header_for("https://www.example.com/").is_some());
        assert!(jar.header_for("https://example.com/").is_some());
        // Still must not escape to an unrelated domain.
        assert!(jar.header_for("https://notexample.com/").is_none());
    }

    #[test]
    fn sessions_are_isolated_by_owner() {
        let m = mgr();
        let a = m.create("key_a".into(), false).unwrap();
        assert!(m.get("key_a", &a.id).is_ok());
        // A different key must not be able to address the session at all.
        assert!(m.get("key_b", &a.id).is_err());
        assert!(m.destroy("key_b", &a.id).is_err());
        assert!(m.get("key_a", &a.id).is_ok());
    }

    #[test]
    fn destroying_a_session_clears_its_jar() {
        let m = mgr();
        let s = m.create("k".into(), false).unwrap();
        s.jar
            .store_set_cookies("https://example.com/", &[("a=1".to_string(), String::new())]);
        assert_eq!(s.jar.len(), 1);
        m.destroy("k", &s.id).unwrap();
        assert_eq!(m.size(), 0);
    }

    #[test]
    fn session_limit_is_enforced() {
        let mut cfg = Config::dev();
        cfg.max_sessions = 2;
        let m = Arc::new(SessionManager::new(cfg));
        m.create("k".into(), false).unwrap();
        m.create("k".into(), false).unwrap();
        let err = m.create("k".into(), false).err().expect("third session must be refused");
        assert_eq!(err.code, ErrorCode::TooManySessions);
    }

    #[tokio::test]
    async fn operations_on_one_session_are_serialised() {
        let m = mgr();
        let s = m.create("k".into(), false).unwrap();
        let g = s.lock().await.unwrap();
        // Second lock cannot be taken while the first is held.
        let s2 = s.clone();
        let h = tokio::spawn(async move { s2.lock().await.map(|_| ()) });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(!h.is_finished());
        drop(g);
        assert!(h.await.unwrap().is_ok());
    }
}
