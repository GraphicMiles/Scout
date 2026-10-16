//! Structured JSON logging with secret redaction.
//!
//! Redaction is applied to *keys*, not values, so a credential is dropped even
//! when the value's shape is unknown. Request bodies are never logged.

use crate::config::Config;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Error = 0,
    Warn = 1,
    Info = 2,
    Debug = 3,
}

impl Level {
    fn as_str(self) -> &'static str {
        match self {
            Level::Error => "error",
            Level::Warn => "warn",
            Level::Info => "info",
            Level::Debug => "debug",
        }
    }
    fn parse(s: &str) -> Level {
        match s.to_lowercase().as_str() {
            "error" => Level::Error,
            "warn" | "warning" => Level::Warn,
            "debug" | "trace" => Level::Debug,
            _ => Level::Info,
        }
    }
}

static LEVEL: AtomicU8 = AtomicU8::new(2);

pub fn init(level: &str) {
    LEVEL.store(Level::parse(level) as u8, Ordering::Relaxed);
}

pub fn enabled(level: Level) -> bool {
    (level as u8) <= LEVEL.load(Ordering::Relaxed)
}

const REDACT_KEYS: &[&str] = &[
    "password", "passwd", "pwd", "secret", "token", "access_token", "refresh_token",
    "authorization", "auth", "cookie", "set-cookie", "api_key", "apikey", "credential",
    "session", "csrf", "xsrf", "signature", "private",
];

fn is_sensitive(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    REDACT_KEYS.iter().any(|r| k.contains(r))
}

/// Redact sensitive fields at any depth.
pub fn redact(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (k, v) in map {
                if is_sensitive(k) {
                    out.insert(k.clone(), Value::String("[redacted]".into()));
                } else {
                    out.insert(k.clone(), redact(v));
                }
            }
            Value::Object(out)
        }
        Value::Array(arr) => Value::Array(arr.iter().map(redact).collect()),
        other => other.clone(),
    }
}

pub fn log(level: Level, msg: &str, fields: Value) {
    if !enabled(level) {
        return;
    }
    let mut obj = match redact(&fields) {
        Value::Object(m) => m,
        _ => serde_json::Map::new(),
    };
    obj.insert("level".into(), json!(level.as_str()));
    obj.insert("msg".into(), json!(msg));
    obj.insert("t".into(), json!(now_iso8601()));
    let line = Value::Object(obj);
    // Single write to stderr keeps interleaving under concurrency from splitting lines.
    eprintln!("{line}");
}

pub fn error(msg: &str, fields: Value) {
    log(Level::Error, msg, fields)
}
pub fn warn(msg: &str, fields: Value) {
    log(Level::Warn, msg, fields)
}
pub fn info(msg: &str, fields: Value) {
    log(Level::Info, msg, fields)
}
pub fn debug(msg: &str, fields: Value) {
    log(Level::Debug, msg, fields)
}

pub fn now_iso8601() -> String {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    iso8601_from_unix_ms(d.as_millis() as i64)
}

/// Minimal UTC formatter (avoids a chrono dependency for one call site).
pub fn iso8601_from_unix_ms(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let millis = ms.rem_euclid(1000);
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (h, mi, s) = (tod / 3600, (tod % 3600) / 60, tod % 60);

    // Civil-from-days (Howard Hinnant's algorithm), epoch at 1970-01-01.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{millis:03}Z")
}

pub fn init_from(cfg: &Config) {
    init(&cfg.log_level);
}
