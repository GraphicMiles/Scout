//! Request validation.
//!
//! Unknown fields are rejected rather than ignored. Silently dropping a
//! misspelled field is how a caller ends up believing they configured something
//! they did not, so a typo returns `invalid_request` naming the field.

use crate::config::Config;
use crate::error::{ErrorCode, Result, ScoutError};
use crate::extract::Schema;
use crate::interaction::Action;
use serde_json::{Map, Value};

/// Node caps a task at 20 steps; keep the same ceiling so a spec that a Node
/// deployment rejects is not quietly accepted here.
pub const MAX_TASK_STEPS: usize = 20;

fn obj<'a>(v: &'a Value, what: &str) -> Result<&'a Map<String, Value>> {
    v.as_object()
        .ok_or_else(|| ScoutError::invalid(format!("{what} must be a JSON object")))
}

fn unknown(map: &Map<String, Value>, allowed: &[&str], what: &str) -> Result<()> {
    for k in map.keys() {
        if !allowed.contains(&k.as_str()) {
            return Err(
                ScoutError::invalid(format!("Unknown field \"{k}\" in {what}"))
                    .with_details(serde_json::json!({ "field": k, "allowed": allowed })),
            );
        }
    }
    Ok(())
}

fn opt_str(map: &Map<String, Value>, key: &str) -> Result<Option<String>> {
    match map.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(ScoutError::invalid(format!("\"{key}\" must be a string"))),
    }
}

fn opt_usize(map: &Map<String, Value>, key: &str) -> Result<Option<usize>> {
    match map.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => n
            .as_u64()
            .map(|v| Some(v as usize))
            .ok_or_else(|| ScoutError::invalid(format!("\"{key}\" must be a non-negative integer"))),
        Some(_) => Err(ScoutError::invalid(format!("\"{key}\" must be a number"))),
    }
}

fn opt_u64(map: &Map<String, Value>, key: &str) -> Result<Option<u64>> {
    match map.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => n
            .as_u64()
            .map(Some)
            .ok_or_else(|| ScoutError::invalid(format!("\"{key}\" must be a non-negative integer"))),
        Some(_) => Err(ScoutError::invalid(format!("\"{key}\" must be a number"))),
    }
}

pub fn create_session(v: &Value) -> Result<()> {
    let m = obj(v, "body")?;
    unknown(m, &[], "body")
}

#[derive(Debug, Clone)]
pub struct NavigateReq {
    pub url: String,
    pub session_id: Option<String>,
    pub timeout_ms: Option<u64>,
    pub method: String,
    pub body: Option<String>,
    pub content_type: Option<String>,
}

/// Node's `sessionId()`: `ses_` followed by 32 lowercase hex characters.
fn session_id(map: &Map<String, Value>, required: bool) -> Result<Option<String>> {
    let v = opt_str(map, "sessionId")?;
    match v {
        None if required => Err(ScoutError::invalid("\"sessionId\" is required")
            .with_details(serde_json::json!({ "field": "sessionId" }))),
        None => Ok(None),
        Some(s) => {
            let ok = s.len() == 36
                && s.starts_with("ses_")
                && s[4..]
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
            if !ok {
                return Err(ScoutError::invalid("\"sessionId\" is malformed")
                    .with_details(serde_json::json!({ "field": "sessionId" })));
            }
            Ok(Some(s))
        }
    }
}

/// Request-time selector syntax check. Node parses every selector it is handed
/// and refuses a broken one up front; doing the same here keeps a typo from
/// surfacing later as a misleading `element_not_found`.
pub(crate) fn check_selector(sel: &str) -> Result<()> {
    if sel.trim().is_empty() {
        return Err(ScoutError::invalid("Selector must be a non-empty string")
            .with_details(serde_json::json!({ "field": "selector" })));
    }
    crate::selector::compile(sel).map(|_| ()).map_err(|e| {
        let shown: String = sel.chars().take(40).collect();
        let ellipsis = if sel.chars().count() > 40 { "\u{2026}" } else { "" };
        ScoutError::invalid(format!(
            "Invalid CSS selector \"{shown}{ellipsis}\": {}",
            e.message
        ))
        .with_details(serde_json::json!({ "field": "selector" }))
    })
}

/// Node's `#strategy`: the default is `selectors` when a schema was given and
/// `generic-page` otherwise; an unknown name is a caller error, while a
/// declared-but-unimplemented adapter is refused as an unsupported capability.
pub fn resolve_strategy(strategy: Option<&str>, has_schema: bool) -> Result<&'static str> {
    let name = strategy.unwrap_or(if has_schema { "selectors" } else { "generic-page" });
    let registry = crate::adapters::registry();
    match registry.iter().find(|s| s.name == name) {
        None => Err(ScoutError::invalid(format!("Unknown strategy \"{name}\"")).with_details(
            serde_json::json!({
                "available": registry.iter().map(|s| s.name).collect::<Vec<_>>()
            }),
        )),
        Some(s) if matches!(s.status, crate::adapters::StrategyStatus::NotImplemented) => {
            Err(ScoutError::unsupported(
                s.reason.unwrap_or("Strategy is not implemented"),
            )
            .with_details(serde_json::json!({ "strategy": name })))
        }
        Some(s) => Ok(s.name),
    }
}

pub fn navigate(v: &Value, _cfg: &Config) -> Result<NavigateReq> {
    // Node's `parseNavigate`: navigation is a GET on an existing session. POST
    // bodies belong to the interaction path, so `method`, `body` and
    // `contentType` are not part of this request.
    let m = obj(v, "body")?;
    unknown(m, &["url", "sessionId", "timeoutMs"], "body")?;
    let url = opt_str(m, "url")?.ok_or_else(|| ScoutError::invalid("\"url\" is required"))?;
    if url.trim().is_empty() {
        return Err(ScoutError::invalid("\"url\" must not be empty"));
    }
    Ok(NavigateReq {
        url,
        session_id: session_id(m, true)?,
        timeout_ms: opt_u64(m, "timeoutMs")?,
        method: "GET".into(),
        body: None,
        content_type: None,
    })
}

#[derive(Debug, Clone)]
pub struct ExtractReq {
    pub url: Option<String>,
    pub session_id: Option<String>,
    pub strategy: Option<String>,
    pub schema: Option<Schema>,
    pub limit: Option<usize>,
    pub timeout_ms: Option<u64>,
}

fn extract_fields(
    m: &Map<String, Value>,
    cfg: &Config,
    session_allowed: bool,
    require_target: bool,
) -> Result<ExtractReq> {
    let url = opt_str(m, "url")?;
    let sid = if session_allowed { session_id(m, false)? } else { None };
    if require_target && url.is_none() && sid.is_none() {
        return Err(ScoutError::invalid("Provide either \"url\" or \"sessionId\""));
    }

    let schema = match m.get("schema") {
        None | Some(Value::Null) => None,
        Some(sv) => Some(
            Schema::from_json(sv).map_err(|e| ScoutError::invalid(e).with_details(
                serde_json::json!({ "field": "schema" }),
            ))?,
        ),
    };

    let strategy = opt_str(m, "strategy")?;
    if let Some(s) = &strategy {
        if schema.is_some() && s != "selectors" {
            return Err(ScoutError::invalid(
                "\"schema\" is only valid with the \"selectors\" strategy",
            )
            .with_details(serde_json::json!({ "field": "schema" })));
        }
    } else if schema.is_some() {
        // A schema without an explicit strategy means the selectors strategy.
    } else if strategy.is_none() {
        // Falls back to generic-page, matching the documented default.
    }
    if schema.is_none() && strategy.as_deref() == Some("selectors") {
        return Err(
            ScoutError::invalid("The \"selectors\" strategy requires \"schema\"")
                .with_details(serde_json::json!({ "field": "schema" })),
        );
    }
    // Resolve the capability before any network or queue use.
    resolve_strategy(strategy.as_deref(), schema.is_some())?;

    let limit = opt_usize(m, "limit")?;
    if let Some(l) = limit {
        if l == 0 {
            return Err(ScoutError::invalid("\"limit\" must be at least 1"));
        }
        if l > cfg.max_extract_items {
            return Err(ScoutError::invalid(format!(
                "\"limit\" must not exceed {}",
                cfg.max_extract_items
            )));
        }
    }

    Ok(ExtractReq {
        url,
        session_id: sid,
        strategy,
        schema,
        limit,
        timeout_ms: opt_u64(m, "timeoutMs")?,
    })
}

pub fn extract(v: &Value, cfg: &Config) -> Result<ExtractReq> {
    let m = obj(v, "body")?;
    unknown(
        m,
        &["url", "sessionId", "strategy", "schema", "limit", "timeoutMs"],
        "body",
    )?;
    extract_fields(m, cfg, true, true)
}

#[derive(Debug, Clone)]
pub struct InteractReq {
    pub session_id: String,
    pub action: Action,
}

pub fn interact(v: &Value, _cfg: &Config) -> Result<InteractReq> {
    // Node's `parseInteract`: a session and an action, nothing else. The
    // per-action budget lives on the action itself (`action.timeoutMs`).
    let m = obj(v, "body")?;
    unknown(m, &["sessionId", "action"], "body")?;
    let session_id = session_id(m, true)?.expect("required above");
    let action_value = m
        .get("action")
        .ok_or_else(|| ScoutError::invalid("\"action\" is required"))?;
    let action = Action::from_json(action_value)?;
    Ok(InteractReq { session_id, action })
}

#[derive(Debug, Clone)]
pub struct TaskStep {
    pub op: String,
    /// The step's parameters with `op` removed, ready for the same validators the
    /// synchronous endpoints use — so a step and a direct call cannot drift apart.
    pub params: Value,
}

#[derive(Debug, Clone)]
pub struct TaskReq {
    pub steps: Vec<TaskStep>,
    pub session_id: Option<String>,
    pub retries: Option<u32>,
    pub timeout_ms: Option<u64>,
}

pub fn task(v: &Value, cfg: &Config) -> Result<TaskReq> {
    let m = obj(v, "body")?;
    unknown(m, &["sessionId", "steps", "timeoutMs", "retries"], "body")?;

    let steps = match m.get("steps").and_then(|s| s.as_array()) {
        Some(list) if !list.is_empty() && list.len() <= MAX_TASK_STEPS => list,
        _ => {
            return Err(ScoutError::invalid(format!(
                "\"steps\" must be an array of 1 to {MAX_TASK_STEPS} steps"
            ))
            .with_details(serde_json::json!({ "field": "steps" })))
        }
    };

    let mut out = Vec::with_capacity(steps.len());
    for (i, step) in steps.iter().enumerate() {
        let where_ = format!("steps[{i}]");
        let sm = step
            .as_object()
            .ok_or_else(|| ScoutError::invalid(format!("{where_} must be an object")))?;
        let op = sm.get("op").and_then(|o| o.as_str()).unwrap_or("");
        match op {
            "navigate" => {
                // Node's parseTask inlines the navigate step rules rather than
                // reusing parseNavigate: a step runs on the task's session, so
                // it carries no session of its own.
                unknown(sm, &["op", "url", "timeoutMs"], &where_)?;
                match opt_str(sm, "url")? {
                    Some(u) if !u.trim().is_empty() => {}
                    _ => {
                        return Err(ScoutError::invalid("\"url\" is required")
                            .with_details(serde_json::json!({ "field": "url" })))
                    }
                }
                if let Some(t) = opt_u64(sm, "timeoutMs")? {
                    if !(100..=cfg.max_nav_timeout_ms).contains(&t) {
                        return Err(ScoutError::invalid(format!(
                            "\"timeoutMs\" must be an integer between 100 and {}",
                            cfg.max_nav_timeout_ms
                        ))
                        .with_details(serde_json::json!({ "field": "timeoutMs" })));
                    }
                }
            }
            "extract" => {
                unknown(
                    sm,
                    &["op", "url", "strategy", "schema", "limit", "timeoutMs"],
                    &where_,
                )?;
                let mut probe = sm.clone();
                probe.remove("op");
                // A task runs against its own session, so a step needs no target
                // of its own; the capability is still resolved here so an
                // unimplemented adapter is refused at submit time, not later.
                extract_fields(&probe, cfg, false, false)?;
            }
            "interact" => {
                unknown(sm, &["op", "action"], &where_)?;
                let action = sm
                    .get("action")
                    .ok_or_else(|| ScoutError::invalid(format!("{where_}.action is required")))?;
                Action::from_json(action)?;
            }
            other => {
                return Err(ScoutError::invalid(format!(
                    "{where_}.op must be \"navigate\", \"extract\" or \"interact\""
                ))
                .with_details(serde_json::json!({ "field": "steps", "op": other })))
            }
        }
        let mut params = sm.clone();
        params.remove("op");
        out.push(TaskStep {
            op: op.to_string(),
            params: Value::Object(params),
        });
    }

    let retries = match opt_usize(m, "retries")? {
        Some(r) if r > cfg.max_retries as usize => {
            return Err(ScoutError::invalid(format!(
                "\"retries\" must be an integer between 0 and {}",
                cfg.max_retries
            ))
            .with_details(serde_json::json!({ "field": "retries" })))
        }
        other => other.map(|r| r as u32),
    };

    let timeout_ms = match opt_u64(m, "timeoutMs")? {
        Some(t) if !(1_000..=cfg.max_task_timeout_ms).contains(&t) => {
            return Err(ScoutError::invalid(format!(
                "\"timeoutMs\" must be an integer between 1000 and {}",
                cfg.max_task_timeout_ms
            ))
            .with_details(serde_json::json!({ "field": "timeoutMs" })))
        }
        other => other,
    };

    Ok(TaskReq {
        steps: out,
        session_id: session_id(m, false)?,
        retries,
        timeout_ms,
    })
}

/// Clamp a requested timeout to the configured ceiling.
pub fn clamp_timeout(requested: Option<u64>, default_ms: u64, max_ms: u64) -> u64 {
    requested.unwrap_or(default_ms).min(max_ms)
}

pub fn require_error(code: ErrorCode, msg: &str) -> ScoutError {
    ScoutError::new(code, msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        Config::dev()
    }

    #[test]
    fn rejects_unknown_fields() {
        let v = serde_json::json!({ "url": "https://example.com/", "typo": 1 });
        let err = navigate(&v, &cfg()).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidRequest);
        assert!(err.message.contains("typo"));
    }

    const SID: &str = "ses_0123456789abcdef0123456789abcdef";

    #[test]
    fn navigate_requires_url_and_session() {
        assert!(navigate(&serde_json::json!({}), &cfg()).is_err());
        assert!(navigate(&serde_json::json!({ "url": "" }), &cfg()).is_err());
        // Node's parseNavigate requires a session, so a bare url is a 400.
        assert!(navigate(&serde_json::json!({ "url": "https://e.com" }), &cfg()).is_err());
        assert!(navigate(&serde_json::json!({ "sessionId": SID, "url": "https://e.com" }), &cfg()).is_ok());
        assert_eq!(
            navigate(&serde_json::json!({ "sessionId": "ses_x", "url": "https://e.com" }), &cfg())
                .unwrap_err()
                .message,
            "\"sessionId\" is malformed"
        );
    }

    #[test]
    fn extract_requires_url_or_session() {
        assert!(extract(&serde_json::json!({}), &cfg()).is_err());
        assert!(extract(&serde_json::json!({ "url": "https://e.com" }), &cfg()).is_ok());
        assert!(extract(&serde_json::json!({ "sessionId": SID }), &cfg()).is_ok());
        assert!(extract(&serde_json::json!({ "sessionId": "ses_1" }), &cfg()).is_err());
        // A schema implies the selectors strategy, and that strategy needs a schema.
        assert!(extract(&serde_json::json!({ "url": "https://e.com", "strategy": "selectors" }), &cfg()).is_err());
        let e = extract(&serde_json::json!({ "url": "https://e.com", "strategy": "tiktok-profile" }), &cfg()).unwrap_err();
        assert_eq!(e.code, ErrorCode::UnsupportedCapability, "declared but unimplemented");
    }

    #[test]
    fn extract_rejects_unknown_strategy() {
        let v = serde_json::json!({ "url": "https://e.com", "strategy": "nope" });
        let err = extract(&v, &cfg()).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidRequest);
    }

    #[test]
    fn extract_rejects_bad_schema_type() {
        let v = serde_json::json!({ "url": "https://e.com", "schema": { "fields": { "a": { "type": "wat" } } } });
        assert!(extract(&v, &cfg()).is_err());
    }

    #[test]
    fn limit_is_bounded() {
        let v = serde_json::json!({ "url": "https://e.com", "limit": 0 });
        assert!(extract(&v, &cfg()).is_err());
        let v = serde_json::json!({ "url": "https://e.com", "limit": 10_000 });
        assert!(extract(&v, &cfg()).is_err());
        let v = serde_json::json!({ "url": "https://e.com", "limit": 5 });
        assert!(extract(&v, &cfg()).is_ok());
    }

    #[test]
    fn task_rejects_unknown_op_as_invalid_request() {
        // Node's parseTask: an op outside the three it knows is a caller error.
        let v = serde_json::json!({ "steps": [{ "op": "teleport" }] });
        let err = task(&v, &cfg()).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidRequest);
        assert!(err.message.contains("steps[0].op must be"), "got: {}", err.message);
    }

    #[test]
    fn task_validates_each_step() {
        let v = serde_json::json!({ "steps": [{ "op": "navigate" }] });
        assert!(task(&v, &cfg()).is_err(), "navigate step needs a url");
        let v = serde_json::json!({ "steps": [{ "op": "navigate", "url": "https://e.com" }] });
        assert!(task(&v, &cfg()).is_ok());
        let v = serde_json::json!({ "steps": [] });
        assert!(task(&v, &cfg()).is_err());
        // Node's ceiling is 20 steps.
        let many = serde_json::json!({ "steps": vec![serde_json::json!({ "op": "navigate", "url": "https://e.com" }); 21] });
        let err = task(&many, &cfg()).unwrap_err();
        assert!(err.message.contains("1 to 20 steps"), "got: {}", err.message);
        // An unimplemented adapter is refused at submit, before any work starts.
        let v = serde_json::json!({ "steps": [{ "op": "extract", "strategy": "tiktok-profile" }] });
        assert_eq!(task(&v, &cfg()).unwrap_err().code, ErrorCode::UnsupportedCapability);
        // An unknown action type is a caller error, not an unsupported feature.
        let v = serde_json::json!({ "steps": [{ "op": "interact", "action": { "type": "teleport" } }] });
        assert_eq!(task(&v, &cfg()).unwrap_err().code, ErrorCode::InvalidRequest);
    }

    #[test]
    fn timeout_clamping() {
        assert_eq!(clamp_timeout(None, 100, 1000), 100);
        assert_eq!(clamp_timeout(Some(50), 100, 1000), 50);
        assert_eq!(clamp_timeout(Some(99999), 100, 1000), 1000);
    }
}
