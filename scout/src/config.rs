//! Environment configuration. Variable names are part of the deployment contract
//! and deliberately mirror the reference implementation.

use std::env;

const MB: u64 = 1024 * 1024;

#[derive(Debug, Clone)]
pub struct Config {
    pub version: &'static str,
    pub host: String,
    pub port: u16,
    pub dev_mode: bool,
    pub api_keys: Vec<String>,
    pub metrics_token: String,

    // Outbound policy
    pub private_allowlist: Vec<String>,
    pub allowed_domains: Vec<String>,
    pub allowed_ports: Vec<u16>,

    // Limits
    pub max_body_bytes: usize,
    pub max_response_bytes: usize,
    /// Ceiling on a single uploaded file, decoded. JSON bodies carrying the
    /// base64 are separately capped by `max_body_bytes`.
    pub max_upload_bytes: usize,
    pub max_dom_nodes: usize,
    pub max_redirects: usize,
    pub max_extract_items: usize,
    pub nav_timeout_ms: u64,
    pub max_nav_timeout_ms: u64,
    pub task_timeout_ms: u64,
    pub max_task_timeout_ms: u64,
    pub request_timeout_ms: u64,
    pub max_sessions: usize,
    pub session_ttl_ms: u64,
    pub concurrency: usize,
    pub max_queued: usize,
    pub task_result_ttl_ms: u64,
    pub max_retained_tasks: usize,
    pub max_retries: u32,
    pub rate_limit_per_min: u64,

    // Memory budget (container is ~512 MB)
    pub memory_soft_limit_bytes: u64,
    pub memory_hard_limit_bytes: u64,
    pub gc_above_bytes: u64,

    pub shutdown_grace_ms: u64,
    pub log_level: String,
    pub user_agent: String,
    /// Hosts the YouTube adapter will accept. Overridable so the adapter can be
    /// exercised against a fixture server, exactly as the Node config allowed.
    pub youtube_hosts: Vec<String>,
    pub js_timeout_ms: u64,
    pub js_max_memory_bytes: u64,
    /// Run page scripts during navigation. Off means the old read-only
    /// behaviour: no globals, no mutation, no timer drain.
    pub js_enabled: bool,
    /// Cap on how many scripts a page may execute, external and inline combined.
    pub js_max_scripts: usize,
    /// Cap on the bytes fetched for external scripts, across the whole page.
    pub js_max_script_bytes: usize,
    /// How many queued timer callbacks to run after the scripts finish.
    pub js_timer_budget: u32,
    /// Total promise jobs a page may run across the whole navigation. This is
    /// what stops a `.then` chain that keeps queueing work.
    pub js_job_budget: u32,
    /// Allow page scripts to issue requests at all. Off means `fetch` and
    /// `XMLHttpRequest` refuse by name instead of being quietly absent.
    pub js_network: bool,
    /// Requests one page may issue from script, across the whole navigation.
    pub js_max_fetches: usize,
    /// Bytes one scripted response may cost.
    pub js_max_fetch_bytes: usize,
    /// Per-request budget for a scripted fetch. The page's own JS budget is
    /// separate and still applies.
    pub js_fetch_timeout_ms: u64,
    /// Send the header set a browser sends (Sec-Fetch-*, Sec-CH-UA, Referer).
    /// Off leaves only User-Agent, Accept and Accept-Language.
    pub browser_headers: bool,
}

fn num(env: &std::collections::HashMap<String, String>, key: &str, default: u64) -> Result<u64, String> {
    match env.get(key) {
        None => Ok(default),
        Some(v) if v.is_empty() => Ok(default),
        Some(v) => v
            .parse::<u64>()
            .map_err(|_| format!("{key} must be a number")),
    }
}

fn list(env: &std::collections::HashMap<String, String>, key: &str) -> Vec<String> {
    env.get(key)
        .map(|v| {
            v.split(',')
                .map(|s| s.trim().to_lowercase())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

impl Config {
    /// A development-mode configuration built without consulting the environment.
    /// Used by tests and by embedders that construct the runtime programmatically.
    /// Builds from a caller-supplied environment map rather than mutating the
    /// process environment. `set_var`/`remove_var` here raced with any other
    /// test reading the environment concurrently, which made `interaction`
    /// tests fail intermittently as the suite grew.
    pub fn dev() -> Self {
        let mut env: std::collections::HashMap<String, String> = env::vars().collect();
        env.insert("SCOUT_DEV_MODE".into(), "1".into());
        Self::from_map(env).expect("dev config is always valid")
    }

    pub fn from_env() -> Result<Self, String> {
        Self::from_map(env::vars().collect())
    }

    pub fn from_map(env: std::collections::HashMap<String, String>) -> Result<Self, String> {

        let api_keys: Vec<String> = env
            .get("SCOUT_API_KEYS")
            .or_else(|| env.get("SCOUT_API_KEY"))
            .map(|v| v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect())
            .unwrap_or_default();

        let allowed_ports = {
            let raw = env.get("SCOUT_ALLOWED_PORTS").cloned().unwrap_or_else(|| "80,443".into());
            raw.split(',')
                .filter_map(|s| s.trim().parse::<u16>().ok())
                .collect::<Vec<_>>()
        };

        let cfg = Config {
            version: "0.1.0",
            host: env.get("HOST").cloned().unwrap_or_else(|| "0.0.0.0".into()),
            port: num(&env, "PORT", 8080)? as u16,
            dev_mode: env.get("SCOUT_DEV_MODE").map(|v| v == "1").unwrap_or(false),
            api_keys,
            metrics_token: env.get("SCOUT_METRICS_TOKEN").cloned().unwrap_or_default(),

            private_allowlist: list(&env, "SCOUT_PRIVATE_ALLOWLIST"),
            allowed_domains: list(&env, "SCOUT_ALLOWED_DOMAINS"),
            allowed_ports: if allowed_ports.is_empty() { vec![80, 443] } else { allowed_ports },

            max_body_bytes: num(&env, "SCOUT_MAX_BODY_BYTES", 6 * 1024 * 1024)? as usize,
            max_upload_bytes: num(&env, "SCOUT_MAX_UPLOAD_BYTES", 4 * 1024 * 1024)? as usize,
            max_response_bytes: num(&env, "SCOUT_MAX_RESPONSE_BYTES", 2 * MB)? as usize,
            max_dom_nodes: num(&env, "SCOUT_MAX_DOM_NODES", 150_000)? as usize,
            max_redirects: num(&env, "SCOUT_MAX_REDIRECTS", 5)? as usize,
            max_extract_items: num(&env, "SCOUT_MAX_EXTRACT_ITEMS", 1000)? as usize,
            nav_timeout_ms: num(&env, "SCOUT_NAV_TIMEOUT_MS", 15_000)?,
            max_nav_timeout_ms: 30_000,
            task_timeout_ms: num(&env, "SCOUT_TASK_TIMEOUT_MS", 60_000)?,
            max_task_timeout_ms: 120_000,
            request_timeout_ms: num(&env, "SCOUT_REQUEST_TIMEOUT_MS", 60_000)?,
            max_sessions: num(&env, "SCOUT_MAX_SESSIONS", 32)? as usize,
            session_ttl_ms: num(&env, "SCOUT_SESSION_TTL_MS", 300_000)?,
            concurrency: num(&env, "SCOUT_CONCURRENCY", 1)? as usize,
            max_queued: num(&env, "SCOUT_MAX_QUEUED", 64)? as usize,
            task_result_ttl_ms: num(&env, "SCOUT_TASK_RESULT_TTL_MS", 600_000)?,
            max_retained_tasks: 500,
            max_retries: num(&env, "SCOUT_MAX_RETRIES", 2)? as u32,
            rate_limit_per_min: num(&env, "SCOUT_RATE_LIMIT_PER_MIN", 120)?,

            memory_soft_limit_bytes: num(&env, "SCOUT_MEM_SOFT_MB", 300)? * MB,
            memory_hard_limit_bytes: num(&env, "SCOUT_MEM_HARD_MB", 400)? * MB,
            gc_above_bytes: num(&env, "SCOUT_GC_ABOVE_MB", 100)? * MB,

            shutdown_grace_ms: num(&env, "SCOUT_SHUTDOWN_GRACE_MS", 10_000)?,
            log_level: env.get("SCOUT_LOG_LEVEL").cloned().unwrap_or_else(|| "info".into()),
            // A runtime that executes page scripts and carries a real cookie
            // jar presents itself the way it behaves: as the browser it is a
            // stand-in for. Sites are entitled to refuse automation outright,
            // and this does not get around any of them — nothing here defeats
            // a CAPTCHA, a rate limit or an access control — but a site that
            // simply serves a different page to an unnamed client would
            // otherwise make every result a measurement of that difference.
            // Override with SCOUT_USER_AGENT to declare what you are.
            user_agent: env.get("SCOUT_USER_AGENT").cloned().unwrap_or_else(|| {
                "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36".into()
            }),
            youtube_hosts: env
                .get("SCOUT_YOUTUBE_HOSTS")
                .map(|v| {
                    v.split(',')
                        .map(|h| h.trim().to_ascii_lowercase())
                        .filter(|h| !h.is_empty())
                        .collect()
                })
                .unwrap_or_else(|| {
                    vec![
                        "youtube.com".into(),
                        "www.youtube.com".into(),
                        "m.youtube.com".into(),
                    ]
                }),
            js_timeout_ms: num(&env, "SCOUT_JS_TIMEOUT_MS", 5_000)?,
            js_max_memory_bytes: num(&env, "SCOUT_JS_MAX_MEMORY_MB", 256)? * MB,
            js_enabled: env
                .get("SCOUT_JS_ENABLED")
                .map(|v| v != "0" && !v.eq_ignore_ascii_case("false"))
                .unwrap_or(true),
            js_max_scripts: num(&env, "SCOUT_JS_MAX_SCRIPTS", 32)? as usize,
            js_max_script_bytes: num(&env, "SCOUT_JS_MAX_SCRIPT_BYTES", 2 * MB)? as usize,
            js_timer_budget: num(&env, "SCOUT_JS_TIMER_BUDGET", 64)? as u32,
            js_job_budget: num(&env, "SCOUT_JS_JOB_BUDGET", 5_000)? as u32,
            js_network: env
                .get("SCOUT_JS_NETWORK")
                .map(|v| v != "0" && !v.eq_ignore_ascii_case("false"))
                .unwrap_or(true),
            js_max_fetches: num(&env, "SCOUT_JS_MAX_FETCHES", 8)? as usize,
            js_max_fetch_bytes: num(&env, "SCOUT_JS_MAX_FETCH_BYTES", MB)? as usize,
            js_fetch_timeout_ms: num(&env, "SCOUT_JS_FETCH_TIMEOUT_MS", 8_000)?,
            browser_headers: env
                .get("SCOUT_BROWSER_HEADERS")
                .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                .unwrap_or(true),
        };

        if !cfg.dev_mode {
            if cfg.api_keys.is_empty() {
                return Err("SCOUT_API_KEYS is required unless SCOUT_DEV_MODE=1".into());
            }
            if cfg.api_keys.iter().any(|k| k.len() < 16) {
                return Err("Every API key must be at least 16 characters".into());
            }
        }
        Ok(cfg)
    }
}
