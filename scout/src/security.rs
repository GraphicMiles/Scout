//! Destination policy (SSRF), guarded HTTP fetching, and redirect validation.
//!
//! Two independent layers:
//!   1. `validate_url` checks the URL as written (scheme, credentials, port, host shape).
//!   2. `ValidatingResolver` re-checks every address the hostname actually resolves to,
//!      *at connect time*, which is what defeats DNS rebinding.
//!
//! IP ranges are matched with explicit integer comparisons rather than a blocklist
//! library. That is deliberate: the equivalent code in the reference implementation
//! silently rejected every public IPv4 because `net.BlockList` matches an IPv4 address
//! against IPv6 `::ffff:0:0/96` rules. Explicit arithmetic has no such overlap.

use crate::config::Config;
use crate::error::{ErrorCode, Result, ScoutError};
use futures_util::StreamExt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;

// ---------------------------------------------------------------- IP ranges

struct V4Range(u32, u32);

const fn v4(s: [u8; 4]) -> u32 {
    u32::from_be_bytes(s)
}

/// Ranges that must never be reachable from a fetch.
const V4_BLOCKED: &[V4Range] = &[
    V4Range(v4([0, 0, 0, 0]), 8),          // "this network"
    V4Range(v4([10, 0, 0, 0]), 8),         // private
    V4Range(v4([100, 64, 0, 0]), 10),      // CGNAT
    V4Range(v4([127, 0, 0, 0]), 8),        // loopback
    V4Range(v4([169, 254, 0, 0]), 16),     // link-local + cloud metadata
    V4Range(v4([172, 16, 0, 0]), 12),      // private
    V4Range(v4([192, 0, 0, 0]), 24),       // IETF protocol assignments
    V4Range(v4([192, 0, 2, 0]), 24),       // TEST-NET-1
    V4Range(v4([192, 88, 99, 0]), 24),     // deprecated 6to4 relay
    V4Range(v4([192, 168, 0, 0]), 16),     // private
    V4Range(v4([198, 18, 0, 0]), 15),      // benchmarking
    V4Range(v4([198, 51, 100, 0]), 24),    // TEST-NET-2
    V4Range(v4([203, 0, 113, 0]), 24),     // TEST-NET-3
    V4Range(v4([224, 0, 0, 0]), 4),        // multicast
    V4Range(v4([240, 0, 0, 0]), 4),        // reserved + broadcast
];

fn in_v4_range(ip: Ipv4Addr, ranges: &[V4Range]) -> bool {
    let n = u32::from(ip);
    ranges.iter().any(|V4Range(base, bits)| {
        let mask = if *bits == 0 { 0 } else { u32::MAX << (32 - bits) };
        (n & mask) == (*base & mask)
    })
}

pub fn is_blocked_v4(ip: Ipv4Addr) -> bool {
    in_v4_range(ip, V4_BLOCKED)
}

struct V6Range(u128, u32);

const fn v6(segs: [u16; 8]) -> u128 {
    let mut bytes = [0u8; 16];
    let mut i = 0;
    while i < 8 {
        bytes[i * 2] = (segs[i] >> 8) as u8;
        bytes[i * 2 + 1] = segs[i] as u8;
        i += 1;
    }
    u128::from_be_bytes(bytes)
}

const V6_BLOCKED: &[V6Range] = &[
    V6Range(v6([0x2001, 0, 0, 0, 0, 0, 0, 0]), 32), // Teredo
    V6Range(v6([0x2001, 0x0db8, 0, 0, 0, 0, 0, 0]), 32), // documentation
    V6Range(v6([0x2002, 0, 0, 0, 0, 0, 0, 0]), 16), // 6to4
    V6Range(v6([0xfc00, 0, 0, 0, 0, 0, 0, 0]), 7),  // unique local
    V6Range(v6([0xfe80, 0, 0, 0, 0, 0, 0, 0]), 10), // link-local
    V6Range(v6([0xfec0, 0, 0, 0, 0, 0, 0, 0]), 10), // deprecated site-local
    V6Range(v6([0xff00, 0, 0, 0, 0, 0, 0, 0]), 8),  // multicast
    V6Range(v6([0x0100, 0, 0, 0, 0, 0, 0, 0]), 64), // discard-only
];

/// `::ffff:0:0/96` and `64:ff9b::/96`, the two prefixes carrying an embedded IPv4.
const MAPPED_PREFIX: u128 = v6([0, 0, 0, 0, 0, 0xffff, 0, 0]) >> 32;
const NAT64_PREFIX: u128 = v6([0x0064, 0xff9b, 0, 0, 0, 0, 0, 0]) >> 32;

fn in_v6_range(ip: Ipv6Addr, ranges: &[V6Range]) -> bool {
    let n = u128::from(ip);
    ranges.iter().any(|V6Range(base, bits)| {
        let mask = if *bits == 0 { 0 } else { u128::MAX << (128 - bits) };
        (n & mask) == (*base & mask)
    })
}

/// Extract the IPv4 address embedded in an IPv4-mapped (`::ffff:a.b.c.d`) or
/// NAT64 (`64:ff9b::/96`) prefix.
fn embedded_v4(ip: Ipv6Addr) -> Option<Ipv4Addr> {
    let n = u128::from(ip);
    let top = n >> 32;
    if top == MAPPED_PREFIX || top == NAT64_PREFIX {
        return Some(Ipv4Addr::from((n & 0xFFFF_FFFF) as u32));
    }
    None
}

pub fn is_blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_blocked_v4(v4),
        IpAddr::V6(v6) => {
            // Mapped and NAT64 forms are unwrapped and judged by their IPv4 payload,
            // so `::ffff:127.0.0.1` is blocked for the same reason `127.0.0.1` is,
            // while a legitimate mapped public address is not falsely rejected.
            if let Some(v4) = embedded_v4(v6) {
                return is_blocked_v4(v4);
            }
            let n = u128::from(v6);
            if n == 0 || n == 1 {
                return true; // unspecified, loopback
            }
            if v6.is_loopback() || v6.is_unspecified() {
                return true;
            }
            in_v6_range(v6, V6_BLOCKED)
        }
    }
}

// ------------------------------------------------------------- URL policy

const INTERNAL_SUFFIXES: &[&str] = &[
    ".localhost",
    ".local",
    ".internal",
    ".localdomain",
    ".lan",
    ".home.arpa",
];

/// Validate a URL as written. Does not resolve DNS.
pub fn validate_url(raw: &str, cfg: &Config) -> Result<url::Url> {
    let u = url::Url::parse(raw)
        .map_err(|_| ScoutError::invalid("URL is not valid").with_details(serde_json::json!({ "field": "url" })))?;

    match u.scheme() {
        "http" | "https" => {}
        other => {
            return Err(ScoutError::blocked(format!(
                "Scheme \"{other}:\" is not permitted"
            )))
        }
    }

    if !u.username().is_empty() || u.password().is_some() {
        return Err(ScoutError::blocked("URLs with embedded credentials are not permitted"));
    }

    // `url` normalizes decimal/octal/hex IPv4 forms (e.g. 2130706433) into dotted
    // form, so a numeric host is already canonical when it reaches the checks below.
    let host = u
        .host_str()
        .ok_or_else(|| ScoutError::invalid("URL has no host"))?
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim_end_matches('.')
        .to_ascii_lowercase();

    let allow_private = cfg.private_allowlist.iter().any(|h| h == &host);

    if !allow_private {
        let port = u.port_or_known_default().unwrap_or_else(|| {
            if u.scheme() == "https" {
                443
            } else {
                80
            }
        });
        if !cfg.allowed_ports.contains(&port) {
            return Err(ScoutError::blocked(format!("Port {port} is not permitted")));
        }
        if host == "localhost" || host == "metadata.google.internal" {
            return Err(ScoutError::blocked("Host is not permitted"));
        }
        if INTERNAL_SUFFIXES.iter().any(|s| host.ends_with(s)) {
            return Err(ScoutError::blocked("Host is not permitted"));
        }
        if let Ok(ip) = host.parse::<IpAddr>() {
            if is_blocked_ip(ip) {
                return Err(ScoutError::blocked("Destination address is in a blocked range"));
            }
        } else if !host.contains('.') {
            return Err(ScoutError::blocked("Single-label hostnames are not permitted"));
        }
    }

    if !cfg.allowed_domains.is_empty()
        && !cfg
            .allowed_domains
            .iter()
            .any(|d| host == *d || host.ends_with(&format!(".{d}")))
    {
        return Err(ScoutError::blocked("Host is not in the allowed domain list"));
    }

    Ok(u)
}

// ------------------------------------------------------- connect-time DNS

/// Resolver that refuses to hand back a blocked address.
///
/// Runs on every connection attempt, including reconnects, so a hostname that
/// flips from a public address to `127.0.0.1` between validation and connect
/// cannot be reached.
#[derive(Clone)]
pub struct ValidatingResolver {
    allowlist: Arc<Vec<String>>,
}

impl ValidatingResolver {
    pub fn new(cfg: &Config) -> Self {
        Self {
            allowlist: Arc::new(cfg.private_allowlist.clone()),
        }
    }
}

impl reqwest::dns::Resolve for ValidatingResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_string();
        let allow = self.allowlist.clone();
        Box::pin(async move {
            let lookup = tokio::net::lookup_host((host.as_str(), 0)).await?;
            let mut out: Vec<SocketAddr> = Vec::new();
            let allowed = allow.iter().any(|h| h == &host.to_ascii_lowercase());
            for addr in lookup {
                if !allowed && is_blocked_ip(addr.ip()) {
                    return Err(Box::new(ScoutError::blocked(
                        "Hostname resolves to a blocked address",
                    ))
                        as Box<dyn std::error::Error + Send + Sync>);
                }
                out.push(addr);
            }
            if out.is_empty() {
                return Err(Box::new(ScoutError::new(
                    ErrorCode::NavigationFailed,
                    "DNS lookup returned no addresses",
                )) as Box<dyn std::error::Error + Send + Sync>);
            }
            Ok(Box::new(out.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

// --------------------------------------------------------- guarded fetch

#[derive(Clone)]
pub struct Fetched {
    pub url: String,
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub content_type: String,
    pub body: String,
    /// Present when the response was not textual: the exact bytes,
    /// base64-encoded, so callers are not forced through a lossy UTF-8 decode.
    pub body_b64: Option<String>,
    pub bytes: usize,
    pub redirects: Vec<serde_json::Value>,
    /// True when the body came from the session's fetch cache after the
    /// server answered 304 Not Modified.
    pub from_cache: bool,
}

/// Reason phrase for the status codes a page script can observe. `Fetched`
/// carries the number; `response.statusText` needs the words.
pub fn status_text(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        303 => "See Other",
        304 => "Not Modified",
        307 => "Temporary Redirect",
        308 => "Permanent Redirect",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        418 => "I'm a teapot",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "",
    }
}

fn is_textual(ct: &str) -> bool {
    ct.is_empty() || ct.starts_with("text/") || ct.contains("json") || ct.contains("xml")
}

/// Standard base64 with padding, hand-rolled to avoid a dependency for
/// thirty lines of alphabet work.
fn encode_base64(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { TABLE[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { TABLE[n as usize & 63] as char } else { '=' });
    }
    out
}

fn map_net_error(e: reqwest::Error) -> ScoutError {
    if e.is_timeout() {
        return ScoutError::new(ErrorCode::NavigationTimeout, "Navigation exceeded its time limit");
    }
    if e.is_connect() {
        let s = e.to_string();
        if s.contains("blocked address") {
            return ScoutError::blocked("Hostname resolves to a blocked address");
        }
        return ScoutError::new(ErrorCode::NavigationFailed, "Could not connect to the destination")
            .with_retryable(true);
    }
    if let Some(src) = std::error::Error::source(&e) {
        let s = src.to_string();
        if s.contains("blocked address") {
            return ScoutError::blocked("Hostname resolves to a blocked address");
        }
        if s.contains("certificate") || s.contains("TLS") || s.contains("handshake") {
            return ScoutError::new(ErrorCode::NavigationFailed, "TLS verification failed");
        }
    }
    if e.is_decode() {
        return ScoutError::new(ErrorCode::NavigationFailed, "Response body could not be decoded")
            .with_retryable(true);
    }
    ScoutError::new(ErrorCode::NavigationFailed, "Network error while fetching the page")
        .with_retryable(true)
        .with_details(serde_json::json!({ "cause": e.to_string() }))
}

/// What the request is *for*. A browser varies Sec-Fetch-* by this, and a
/// server that reads those headers reads them to tell a navigation from a
/// subresource from an API call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchKind {
    /// A top-level document load.
    Document,
    /// A `<script src>` the document pulled in.
    Script,
    /// `fetch` or `XMLHttpRequest`, from the page or from the request tool.
    Xhr,
}

pub struct FetchRequest<'a> {
    pub url: &'a str,
    pub method: &'a str,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
    /// The session's cookie jar. Read before every hop and written after every
    /// hop, so a cookie issued by a login POST is presented on the redirect
    /// that follows it. Applying cookies only at the end of the chain is the
    /// difference between being able to sign in and landing back on the form.
    pub jar: Option<&'a crate::session::CookieJar>,
    pub kind: FetchKind,
    /// The document this request is issued from, when there is one. Drives
    /// `Referer`, `Origin` and `Sec-Fetch-Site` under the browser's default
    /// `strict-origin-when-cross-origin` policy.
    pub referer: Option<&'a str>,
    pub timeout_ms: u64,
    pub max_bytes: usize,
}

/// The header set a browser sends with a request of this kind.
///
/// This is identification, not evasion. Scout runs page scripts, keeps a cookie
/// jar and follows redirects, so it presents itself as the browser it stands in
/// for; it is a lie to say "Scout/0.1 (automation runtime)" to a server while
/// behaving like Chrome in every other respect. Nothing here defeats a
/// CAPTCHA, a rate limit, a login wall or any other access control — those
/// still stop a session, and Scout reports that it was stopped rather than
/// looking for a way around. `SCOUT_BROWSER_HEADERS=0` turns all of it off
/// except User-Agent and Accept-Language, and `SCOUT_USER_AGENT` declares
/// whatever you would rather declare.
fn browser_headers(
    cfg: &Config,
    kind: FetchKind,
    target: &url::Url,
    referer: Option<&str>,
    method: &str,
) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let (accept, dest, mode) = match kind {
        FetchKind::Document => (
            "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8",
            "document",
            "navigate",
        ),
        FetchKind::Script => ("*/*", "script", "no-cors"),
        FetchKind::Xhr => ("*/*", "empty", "cors"),
    };
    out.push(("accept".into(), accept.into()));

    if !cfg.browser_headers {
        return out;
    }

    let from = referer.and_then(|r| url::Url::parse(r).ok());
    let site = match &from {
        None => "none",
        Some(r) if r.origin() == target.origin() => "same-origin",
        Some(r) if same_site(r, target) => "same-site",
        Some(_) => "cross-site",
    };
    out.push(("sec-fetch-dest".into(), dest.into()));
    out.push(("sec-fetch-mode".into(), mode.into()));
    out.push(("sec-fetch-site".into(), site.into()));
    if kind == FetchKind::Document {
        out.push(("upgrade-insecure-requests".into(), "1".into()));
        out.push(("sec-fetch-user".into(), "?1".into()));
    }

    // Client hints are derived from the configured UA rather than hard-coded,
    // so a custom UA does not keep advertising a browser it is not.
    if let Some((brands, platform)) = client_hints(&cfg.user_agent) {
        out.push(("sec-ch-ua".into(), brands));
        out.push(("sec-ch-ua-mobile".into(), "?0".into()));
        out.push(("sec-ch-ua-platform".into(), format!("\"{platform}\"")));
    }

    if let Some(r) = &from {
        // strict-origin-when-cross-origin: full URL within the origin, origin
        // only across origins, nothing on an https -> http downgrade.
        let downgrade = r.scheme() == "https" && target.scheme() == "http";
        if !downgrade {
            let value = if r.origin() == target.origin() {
                r.to_string()
            } else {
                r.origin().ascii_serialization()
            };
            out.push(("referer".into(), value));
        }
        // Browsers send Origin on every state-changing request and on XHR.
        if !matches!(method, "GET" | "HEAD") || kind == FetchKind::Xhr {
            out.push(("origin".into(), r.origin().ascii_serialization()));
        }
    }
    out
}

/// `("Chromium";v="131", ...)` when the UA looks like Chrome, else nothing.
fn client_hints(ua: &str) -> Option<(String, &'static str)> {
    let major = ua.split("Chrome/").nth(1)?.split(&['.', ' ', ';'][..]).next()?;
    if !major.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let platform = if ua.contains("Windows") {
        "Windows"
    } else if ua.contains("Macintosh") || ua.contains("Mac OS X") {
        "macOS"
    } else if ua.contains("Android") {
        "Android"
    } else if ua.contains("iPhone") || ua.contains("iPad") {
        "iOS"
    } else {
        "Linux"
    };
    Some((
        format!(
            "\"Chromium\";v=\"{major}\", \"Google Chrome\";v=\"{major}\", \"Not?A_Brand\";v=\"99\""
        ),
        platform,
    ))
}

fn registrable(host: &str) -> String {
    let h = host.trim_end_matches('.').to_ascii_lowercase();
    let labels: Vec<&str> = h.split('.').collect();
    if labels.len() <= 2 {
        return h;
    }
    // Walk from the longest candidate down: the registrable domain is the
    // public suffix plus one label ("www.example.co.uk" -> "example.co.uk").
    for take in (2..labels.len()).rev() {
        let candidate: String = labels[labels.len() - take..].join(".");
        if crate::session::is_public_suffix(&candidate) {
            let n = take + 1;
            if labels.len() >= n {
                return labels[labels.len() - n..].join(".");
            }
            break;
        }
    }
    labels[labels.len() - 2..].join(".")
}

/// "Same site" as a browser computes it: equal registrable domains. Used only
/// for a Sec-Fetch-Site value, never for a decision about whether a request
/// may proceed.
fn same_site(a: &url::Url, b: &url::Url) -> bool {
    match (a.host_str(), b.host_str()) {
        (Some(h1), Some(h2)) => registrable(h1) == registrable(h2),
        _ => false,
    }
}

/// Fetch one document with every hop validated.
///
/// Redirects are followed manually (the client has redirect following disabled)
/// so each hop can be re-validated and credential headers stripped on origin change.
pub async fn guarded_fetch(
    cfg: &Config,
    client: &reqwest::Client,
    req: FetchRequest<'_>,
) -> Result<Fetched> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(req.timeout_ms);
    let mut current = validate_url(req.url, cfg)?;
    let mut method = req.method.to_ascii_uppercase();
    let mut body = req.body.clone();
    let mut headers = req.headers.clone();
    let mut redirects = Vec::new();

    for hop in 0..=cfg.max_redirects {
        // Recomputed per hop: a cookie set by hop N belongs on hop N+1.
        let cookie_header = req.jar.and_then(|j| j.header_for(current.as_str()));

        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Err(ScoutError::new(
                ErrorCode::NavigationTimeout,
                "Navigation time budget exhausted",
            )
            .with_retryable(true));
        }

        let mut rb = client.request(
            reqwest::Method::from_bytes(method.as_bytes())
                .map_err(|_| ScoutError::invalid(format!("Unsupported method \"{method}\"")))?,
            current.clone(),
        );
        rb = rb
            .header("user-agent", &cfg.user_agent)
            .header("accept-language", "en-US,en;q=0.8");
        for (k, v) in browser_headers(cfg, req.kind, &current, req.referer, &method) {
            rb = rb.header(k, v);
        }

        for (k, v) in &headers {
            if k.eq_ignore_ascii_case("host") || k.eq_ignore_ascii_case("content-length") {
                continue;
            }
            rb = rb.header(k, v);
        }
        if let Some(c) = &cookie_header {
            if !c.is_empty() {
                rb = rb.header("cookie", c);
            }
        }
        if let Some(b) = &body {
            rb = rb.body(b.clone());
        }
        rb = rb.timeout(remaining);

        let res = rb.send().await.map_err(map_net_error)?;
        let status = res.status().as_u16();

        // Collect Set-Cookie into the session jar, then drop it from the
        // headers a caller or a page script is allowed to see.
        let mut exposed = Vec::new();
        let mut set_cookies: Vec<(String, String)> = Vec::new();
        for (k, v) in res.headers().iter() {
            let name = k.as_str().to_ascii_lowercase();
            if name == "set-cookie" {
                if let Ok(s) = v.to_str() {
                    set_cookies.push((s.to_string(), current.to_string()));
                }
                continue;
            }
            if let Ok(s) = v.to_str() {
                exposed.push((name, s.to_string()));
            }
        }
        if let Some(jar) = req.jar {
            jar.store_set_cookies(current.as_str(), &set_cookies);
        }

        let location = res
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());

        if matches!(status, 301 | 302 | 303 | 307 | 308) {
            let Some(loc) = location else {
                return Err(ScoutError::new(
                    ErrorCode::NavigationFailed,
                    "Redirect response had no Location header",
                ));
            };
            if hop >= cfg.max_redirects {
                return Err(ScoutError::new(
                    ErrorCode::NavigationFailed,
                    format!("Too many redirects (limit {})", cfg.max_redirects),
                )
                .with_details(serde_json::json!({ "redirects": redirects })));
            }
            let next = current.join(&loc).map_err(|_| {
                ScoutError::new(ErrorCode::NavigationFailed, "Server sent an invalid redirect location")
            })?;
            let validated = validate_url(next.as_str(), cfg).map_err(|e| {
                if e.code == ErrorCode::BlockedDestination {
                    ScoutError::blocked(format!("Redirect to {} blocked: {}", next, e.message))
                } else {
                    e
                }
            })?;

            redirects.push(serde_json::json!({
                "from": current.as_str(),
                "to": validated.as_str(),
                "status": status,
            }));

            if validated.origin() != current.origin() {
                headers.retain(|(k, _)| {
                    !k.eq_ignore_ascii_case("authorization") && !k.eq_ignore_ascii_case("referer")
                });
            }
            if status == 303 || ((status == 301 || status == 302) && method == "POST") {
                method = "GET".into();
                body = None;
                headers.retain(|(k, _)| !k.eq_ignore_ascii_case("content-type"));
            }
            current = validated;
            continue;
        }

        let content_type = res
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();

        // Documents and scripts must be textual; an XHR/fetch from a page
        // script (or the /v1/request tool) may fetch binary data, which comes
        // back as base64 instead of a corrupted string.
        let binary = !is_textual(&content_type);
        if binary && req.kind != FetchKind::Xhr {
            return Err(ScoutError::new(
                ErrorCode::UnsupportedContent,
                format!("\"{content_type}\" is not downloaded (documents and data only)"),
            )
            .with_retryable(false)
            .with_details(serde_json::json!({ "contentType": content_type })));
        }

        // Enforce the cap while streaming: a declared Content-Length is only a hint,
        // and decompression happens below it.
        let mut stream = res.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(map_net_error)?;
            if buf.len() + chunk.len() > req.max_bytes {
                return Err(ScoutError::new(
                    ErrorCode::ResponseTooLarge,
                    format!("Response exceeds {} bytes", req.max_bytes),
                )
                .with_retryable(false));
            }
            buf.extend_from_slice(&chunk);
        }

        let charset = extract_charset(&exposed);
        let body = decode_body(&buf, charset.as_deref());
        let body_b64 = if binary { Some(encode_base64(&buf)) } else { None };
        let bytes = buf.len();

        return Ok(Fetched {
            url: current.to_string(),
            status,
            headers: exposed,
            content_type,
            body,
            body_b64,
            bytes,
            redirects,
            from_cache: false,
        });
    }

    Err(ScoutError::new(
        ErrorCode::NavigationFailed,
        "Too many redirects",
    ))
}

fn extract_charset(headers: &[(String, String)]) -> Option<String> {
    let ct = headers
        .iter()
        .find(|(k, _)| k == "content-type")
        .map(|(_, v)| v.clone())?;
    for part in ct.split(';') {
        let part = part.trim();
        if let Some(rest) = part.strip_prefix("charset=") {
            return Some(rest.trim_matches('"').trim_matches('\'').to_string());
        }
    }
    None
}

/// Decode a document body to a String, honouring the declared charset and
/// falling back to UTF-8 (lossy) rather than failing the whole request.
fn decode_body(buf: &[u8], declared: Option<&str>) -> String {
    let mut charset = declared.map(|s| s.to_ascii_lowercase());
    if charset.is_none() {
        // Sniff <meta charset> from the first 2 KB, matching the reference behaviour.
        let head = String::from_utf8_lossy(&buf[..buf.len().min(2048)]).to_ascii_lowercase();
        if let Some(pos) = head.find("charset=") {
            let rest = &head[pos + 8..];
            let cand: String = rest
                .trim_start_matches(|c| c == '"' || c == '\'')
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
                .collect();
            if !cand.is_empty() {
                charset = Some(cand);
            }
        }
    }
    match charset.as_deref() {
        Some("latin1") | Some("iso-8859-1") | Some("windows-1252") | Some("ascii") | Some("us-ascii") => {
            buf.iter().map(|&b| b as char).collect()
        }
        _ => String::from_utf8_lossy(buf).into_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn registrable_domain_respects_public_suffixes() {
        // Two-label hosts pass through unchanged.
        assert_eq!(registrable("example.com"), "example.com");
        // The suffix plus one label, not the last two labels.
        assert_eq!(registrable("www.example.co.uk"), "example.co.uk");
        assert_eq!(registrable("deep.shop.example.com.ng"), "example.com.ng");
        assert_eq!(registrable("a.b.me.github.io"), "me.github.io");
        // A suffix list miss falls back to the last two labels.
        assert_eq!(registrable("a.b.example.de"), "example.de");
    }

    #[test]
    fn blocks_private_and_special_v4() {
        for s in [
            "0.0.0.0", "10.0.0.1", "10.255.255.254", "100.64.0.1", "127.0.0.1", "127.1.2.3",
            "169.254.169.254", "172.16.0.1", "172.31.255.255", "192.0.0.1", "192.0.2.5",
            "192.88.99.1", "192.168.1.1", "198.18.0.1", "198.51.100.1", "203.0.113.1",
            "224.0.0.1", "240.0.0.1", "255.255.255.255",
        ] {
            assert!(is_blocked_ip(ip(s)), "{s} should be blocked");
        }
    }

    /// The regression that mattered: every public IPv4 was rejected by an
    /// overlapping IPv6 rule. Public addresses must pass.
    #[test]
    fn allows_public_v4() {
        for s in [
            "1.1.1.1", "8.8.8.8", "93.184.216.34", "142.250.185.78", "172.15.255.255",
            "172.32.0.0", "100.63.255.255", "100.128.0.0", "223.255.255.255", "11.0.0.1",
        ] {
            assert!(!is_blocked_ip(ip(s)), "{s} should be allowed");
        }
    }

    #[test]
    fn blocks_private_and_special_v6() {
        for s in [
            "::", "::1", "2001::1", "2001:db8::1", "2002::1", "fc00::1", "fd00::1",
            "fe80::1", "fec0::1", "ff02::1", "100::1",
        ] {
            assert!(is_blocked_ip(ip(s)), "{s} should be blocked");
        }
    }

    #[test]
    fn allows_public_v6() {
        for s in ["2606:4700::1111", "2001:4860:482b:400::1", "2a00:1450:4001:80f::200e"] {
            assert!(!is_blocked_ip(ip(s)), "{s} should be allowed");
        }
    }

    /// IPv4-mapped and NAT64 addresses are judged by their embedded IPv4 payload,
    /// so a wrapped loopback is blocked but a wrapped public address is not.
    #[test]
    fn unwraps_mapped_and_nat64() {
        assert!(is_blocked_ip(ip("::ffff:127.0.0.1")));
        assert!(is_blocked_ip(ip("::ffff:169.254.169.254")));
        assert!(is_blocked_ip(ip("::ffff:10.0.0.1")));
        assert!(is_blocked_ip(ip("64:ff9b::127.0.0.1")));
        assert!(is_blocked_ip(ip("64:ff9b::10.1.2.3")));
        assert!(!is_blocked_ip(ip("::ffff:8.8.8.8")));
        assert!(!is_blocked_ip(ip("64:ff9b::8.8.8.8")));
    }

    #[test]
    fn rejects_unsafe_url_shapes() {
        let cfg = crate::config::Config::dev();
        for bad in [
            "file:///etc/passwd",
            "ftp://example.com/x",
            "gopher://example.com",
            "http://user:pass@example.com/",
            "http://localhost/",
            "http://localhost:8080/",
            "http://metadata.google.internal/",
            "http://169.254.169.254/latest/meta-data/",
            "http://127.0.0.1:80/",
            "http://[::1]/",
            "http://foo.internal/",
            "http://box.local/",
            "http://intranet/",
            "http://example.com:8080/",
            "http://example.com:22/",
        ] {
            assert!(validate_url(bad, &cfg).is_err(), "{bad} should be rejected");
        }
    }

    /// Numeric IPv4 forms are normalized by the URL parser before the range check,
    /// so they cannot be used to smuggle a loopback address past validation.
    #[test]
    fn normalizes_numeric_ip_forms() {
        let cfg = crate::config::Config::dev();
        for bad in [
            "http://2130706433/",
            "http://0x7f000001/",
            "http://017700000001/",
            "http://127.1/",
        ] {
            assert!(validate_url(bad, &cfg).is_err(), "{bad} should be rejected");
        }
    }

    #[test]
    fn accepts_ordinary_public_urls() {
        let cfg = crate::config::Config::dev();
        for good in [
            "https://example.com/",
            "https://www.youtube.com/@NASA/videos",
            "http://example.com/",
            "https://example.com:8443/x",
        ] {
            // 8443 is not in the default allowlist; assert the rest pass.
            if good.contains("8443") {
                assert!(validate_url(good, &cfg).is_err());
            } else {
                assert!(validate_url(good, &cfg).is_ok(), "{good} should be accepted");
            }
        }
    }

    #[test]
    fn allowed_domains_restricts_everything_else() {
        let mut cfg = crate::config::Config::dev();
        cfg.allowed_domains = vec!["example.com".into()];
        assert!(validate_url("https://example.com/a", &cfg).is_ok());
        assert!(validate_url("https://sub.example.com/a", &cfg).is_ok());
        assert!(validate_url("https://notexample.com/a", &cfg).is_err());
        assert!(validate_url("https://example.com.evil.com/a", &cfg).is_err());
    }
}
