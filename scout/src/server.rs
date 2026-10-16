//! HTTP transport: routing, authentication, rate limits, body limits, metrics.
//!
//! Kept deliberately thin — every rule that matters lives in `scout.rs` and
//! below, so the same behaviour is reachable from an embedder that never starts
//! a socket.

use crate::abort::Abort;
use crate::config::Config;
use crate::error::{ErrorCode, Result, ScoutError};
use crate::scout::Scout;
use crate::util::{random_id, sha256_bytes};
use axum::body::Body;
use axum::extract::{ConnectInfo, Path, State};
use axum::http::{HeaderMap, Method, Request, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use subtle::ConstantTimeEq;

#[derive(Clone)]
pub struct AppState {
    pub scout: Arc<Scout>,
    pub cfg: Arc<Config>,
    keys: Arc<Vec<(String, [u8; 32])>>,
    metrics_digest: Option<[u8; 32]>,
    rate: Arc<RateLimiter>,
    auth_fail: Arc<RateLimiter>,
    pub shutting_down: Arc<std::sync::atomic::AtomicBool>,
    started: Instant,
}

/// Sliding-window limiter keyed by an opaque string.
pub struct RateLimiter {
    limit: u64,
    window: Duration,
    hits: Mutex<HashMap<String, Vec<Instant>>>,
}

impl RateLimiter {
    pub fn new(limit: u64) -> Self {
        Self {
            limit,
            window: Duration::from_secs(60),
            hits: Mutex::new(HashMap::new()),
        }
    }

    /// Record a hit. Returns `Err(retry_after_secs)` when over the limit.
    pub fn check(&self, key: &str) -> std::result::Result<(), u64> {
        let now = Instant::now();
        let mut map = self.hits.lock().unwrap();
        // Opportunistic cleanup keeps the map from growing without bound.
        if map.len() > 4_096 {
            map.retain(|_, v| v.iter().any(|t| now.duration_since(*t) < self.window));
        }
        let entry = map.entry(key.to_string()).or_default();
        entry.retain(|t| now.duration_since(*t) < self.window);
        if entry.len() as u64 >= self.limit {
            let oldest = entry.first().copied().unwrap_or(now);
            let elapsed = now.duration_since(oldest).as_secs();
            return Err(self.window.as_secs().saturating_sub(elapsed).max(1));
        }
        entry.push(now);
        Ok(())
    }

    /// Whether the key is currently blocked, without recording a hit.
    pub fn peek(&self, key: &str) -> bool {
        let now = Instant::now();
        let mut map = self.hits.lock().unwrap();
        let entry = map.entry(key.to_string()).or_default();
        entry.retain(|t| now.duration_since(*t) < self.window);
        (entry.len() as u64) < self.limit
    }
}

impl AppState {
    pub fn new(scout: Arc<Scout>) -> Self {
        let cfg = scout.cfg.clone();
        let keys = cfg
            .api_keys
            .iter()
            .map(|k| {
                let digest = sha256_bytes(k);
                let id = format!("key_{}", &hex(&digest)[..8]);
                (id, digest)
            })
            .collect();
        let metrics_digest = if cfg.metrics_token.is_empty() {
            None
        } else {
            Some(sha256_bytes(&cfg.metrics_token))
        };
        Self {
            rate: Arc::new(RateLimiter::new(cfg.rate_limit_per_min)),
            auth_fail: Arc::new(RateLimiter::new(20)),
            keys: Arc::new(keys),
            metrics_digest,
            scout,
            cfg,
            shutting_down: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            started: Instant::now(),
        }
    }

    fn bearer(headers: &HeaderMap) -> Option<String> {
        if let Some(v) = headers.get(axum::http::header::AUTHORIZATION).and_then(|v| v.to_str().ok()) {
            if let Some(rest) = v.strip_prefix("Bearer ") {
                let t = rest.trim();
                if !t.is_empty() {
                    return Some(t.to_string());
                }
            }
        }
        headers
            .get("x-api-key")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty())
    }

    pub fn authenticate(&self, headers: &HeaderMap) -> Result<String> {
        if self.cfg.dev_mode && self.keys.is_empty() {
            return Ok("dev".into());
        }
        let key = Self::bearer(headers).ok_or_else(ScoutError::unauthorized)?;
        let digest = sha256_bytes(&key);
        // Compare against every key without early exit, so timing does not reveal
        // which key matched (or how many exist).
        let mut found: Option<String> = None;
        for (id, expected) in self.keys.iter() {
            if digest.ct_eq(expected).into() {
                found = Some(id.clone());
            }
        }
        found.ok_or_else(ScoutError::unauthorized)
    }

    fn authenticate_metrics(&self, headers: &HeaderMap) -> Result<String> {
        match self.metrics_digest {
            None => self.authenticate(headers),
            Some(expected) => {
                let key = Self::bearer(headers).ok_or_else(ScoutError::unauthorized)?;
                if sha256_bytes(&key).ct_eq(&expected).into() {
                    Ok("metrics".into())
                } else {
                    Err(ScoutError::unauthorized())
                }
            }
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Middleware: request id, authentication, rate limiting, shutdown gate.
async fn guard(State(state): State<AppState>, req: Request<Body>, next: Next) -> Response {
    let started = Instant::now();
    let request_id = random_id("req_", 6);
    let path = req.uri().path().to_string();
    let method = req.method().clone();
    let route = route_name(&method, &path);
    let ip = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0.ip().to_string())
        .unwrap_or_else(|| "unknown".into());

    let mut response = match handle(&state, req, next, &ip, started).await {
        Ok(r) => r,
        Err(e) => {
            if e.code == ErrorCode::Internal {
                crate::logger::error("unhandled error", json!({ "requestId": request_id, "message": e.message }));
            }
            e.into_response()
        }
    };

    let status = response.status().as_u16();
    if let Ok(v) = axum::http::HeaderValue::from_str(&request_id) {
        response.headers_mut().insert("x-request-id", v);
    }
    response
        .headers_mut()
        .insert("cache-control", axum::http::HeaderValue::from_static("no-store"));
    response
        .headers_mut()
        .insert("x-content-type-options", axum::http::HeaderValue::from_static("nosniff"));

    state
        .scout
        .metrics
        .inc("scout_http_requests_total", "HTTP requests by route and status", &[
            ("route", route),
            ("status", &status.to_string()),
        ]);
    crate::logger::info(
        "request",
        json!({
            "requestId": request_id,
            "method": method.as_str(),
            "route": route,
            "status": status,
            "ms": started.elapsed().as_millis() as u64,
        }),
    );
    response
}

async fn handle(
    state: &AppState,
    req: Request<Body>,
    next: Next,
    ip: &str,
    _started: Instant,
) -> Result<Response> {
    let path = req.uri().path().to_string();
    let is_public = path == "/health";
    let is_metrics = path == "/metrics";

    if !is_public {
        // Throttle failed authentication per source address.
        if !state.auth_fail.peek(ip) {
            return Err(ScoutError::new(
                ErrorCode::RateLimited,
                "Too many failed authentication attempts",
            )
            .with_retry_after(30));
        }
        // `check` records the failure and returns the retry hint; the `peek`
        // gate above is what actually rejects, so the hint is not needed here.
        let owner = if is_metrics {
            state.authenticate_metrics(req.headers()).inspect_err(|_| {
                let _ = state.auth_fail.check(ip);
            })?
        } else {
            state.authenticate(req.headers()).inspect_err(|_| {
                let _ = state.auth_fail.check(ip);
            })?
        };

        if !is_metrics {
            if let Err(retry) = state.rate.check(&owner) {
                return Err(ScoutError::new(ErrorCode::RateLimited, "Rate limit exceeded")
                    .with_retry_after(retry));
            }
            if state.shutting_down.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(ScoutError::new(ErrorCode::ShuttingDown, "Service is shutting down"));
            }
        }
    }

    // The owner resolved during authentication travels with the request so the
    // handlers never re-derive it (and so a second, divergent lookup is impossible).
    let owner = if is_public {
        "public".to_string()
    } else if is_metrics {
        "metrics".to_string()
    } else if state.cfg.dev_mode && state.keys.is_empty() {
        "dev".to_string()
    } else {
        state.authenticate(req.headers())?
    };

    let mut req = req;
    req.extensions_mut().insert(Owner(owner));
    Ok(next.run(req).await)
}

#[derive(Clone)]
pub struct Owner(pub String);

fn owner_of(req: &Request<Body>) -> String {
    req.extensions()
        .get::<Owner>()
        .map(|o| o.0.clone())
        .unwrap_or_else(|| "dev".into())
}

fn route_name(method: &Method, path: &str) -> &'static str {
    match (method.as_str(), path) {
        ("GET", "/health") => "health",
        ("GET", "/metrics") => "metrics",
        ("GET", "/v1/strategies") => "strategies",
        ("POST", "/v1/sessions") => "sessions.create",
        ("POST", "/v1/navigate") => "navigate",
        ("POST", "/v1/extract") => "extract",
        ("POST", "/v1/interact") => "interact",
        ("POST", "/v1/eval") => "eval",
        ("POST", "/v1/tasks") => "tasks.create",
        _ if path.starts_with("/v1/sessions/") => "sessions.delete",
        _ if path.starts_with("/v1/tasks/") && path.ends_with("/cancel") => "tasks.cancel",
        _ if path.starts_with("/v1/tasks/") => "tasks.get",
        _ => "unmatched",
    }
}

/// Read and parse a JSON body, enforcing the size cap with our own error shape.
async fn read_json(req: Request<Body>, max: usize) -> Result<Value> {
    use axum::body::to_bytes;
    // A declared length over the cap is refused before reading anything.
    if let Some(len) = req
        .headers()
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<usize>().ok())
    {
        if len > max {
            return Err(ScoutError::new(
                ErrorCode::PayloadTooLarge,
                format!("Body exceeds {max} bytes"),
            ));
        }
    }

    if let Some(ct) = req.headers().get(axum::http::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()) {
        if !ct.to_ascii_lowercase().contains("json") {
            return Err(ScoutError::invalid("Content-Type must be application/json"));
        }
    }

    let bytes = to_bytes(req.into_body(), max).await.map_err(|_| {
        ScoutError::new(ErrorCode::PayloadTooLarge, format!("Body exceeds {max} bytes"))
    })?;
    if bytes.is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_slice::<Value>(&bytes)
        .map_err(|e| ScoutError::invalid(format!("Body is not valid JSON: {e}")))
}

// ------------------------------------------------------------------ handlers

async fn health(State(state): State<AppState>) -> Response {
    let down = state.shutting_down.load(std::sync::atomic::Ordering::SeqCst);
    let body = json!({
        "status": if down { "shutting_down" } else { "ok" },
        "version": state.cfg.version,
        "uptimeSec": state.started.elapsed().as_secs(),
    });
    (
        if down { StatusCode::SERVICE_UNAVAILABLE } else { StatusCode::OK },
        Json(body),
    )
        .into_response()
}

async fn metrics(State(state): State<AppState>) -> Response {
    (
        StatusCode::OK,
        [("content-type", "text/plain; version=0.0.4; charset=utf-8")],
        state.scout.metrics.render(),
    )
        .into_response()
}

async fn strategies() -> Response {
    Json(crate::adapters::list_json()).into_response()
}

/// Machine-readable tool manifest, so an agent can discover the surface
/// instead of having it hard-coded.
async fn tools_manifest(req: Request<Body>) -> Result<Response> {
    owner_of(&req); // authenticated, like every other /v1 route
    Ok(Json(crate::tools::manifest()).into_response())
}

async fn tools_call(State(state): State<AppState>, req: Request<Body>) -> Result<Response> {
    let owner = owner_of(&req);
    let body = read_json(req, state.cfg.max_body_bytes).await?;
    let m = body
        .as_object()
        .ok_or_else(|| ScoutError::invalid("body must be an object"))?;
    for k in m.keys() {
        if !["name", "arguments"].contains(&k.as_str()) {
            return Err(ScoutError::invalid(format!("Unknown field \"{k}\" in body")));
        }
    }
    let name = m
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ScoutError::invalid("\"name\" is required"))?
        .to_string();
    let arguments = m.get("arguments").cloned().unwrap_or(json!({}));

    // Deliberately not wrapped in `run_inline` here. The individual tools
    // take their own admission slot for the work that needs one; wrapping
    // again would try to acquire a second concurrency permit from inside the
    // first and deadlock at concurrency 1.
    // The token is never cancelled from here: the tools that run real work
    // mint their own under the task manager, which owns cancellation and
    // timeouts. This one only exists to satisfy the `evaluate` tool's signature.
    let abort = Abort::new();
    let out = state.scout.tools_call(&owner, &name, arguments, &abort).await;
    Ok(Json(out?).into_response())
}

async fn create_session(State(state): State<AppState>, req: Request<Body>) -> Result<Response> {
    let owner = owner_of(&req);
    let body = read_json(req, state.cfg.max_body_bytes).await?;
    crate::validate::create_session(&body)?;
    Ok((StatusCode::CREATED, Json(state.scout.create_session(&owner)?)).into_response())
}

async fn destroy_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
    req: Request<Body>,
) -> Result<Response> {
    let owner = owner_of(&req);
    state.scout.destroy_session(&owner, &id)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn navigate(State(state): State<AppState>, req: Request<Body>) -> Result<Response> {
    let owner = owner_of(&req);
    let body = read_json(req, state.cfg.max_body_bytes).await?;
    let nav = crate::validate::navigate(&body, &state.cfg)?;
    // The abort token is owned by the admission wrapper below.

    // Node's `parseNavigate` requires a session, so there is no implicit one to
    // release afterwards: `validate::navigate` has already refused a body without
    // a well-formed `sessionId`.
    let session_id = nav.session_id.clone().unwrap_or_default();
    let session = state.scout.sessions.get(&owner, &session_id)?;
    let worker = state.scout.worker_index(&session.id)?;

    // Requests take the same admission path as tasks: above the soft memory
    // budget or with the queue full they are shed, and they hold a concurrency
    // permit while they run rather than running unbounded.
    let st = &state;
    let out = st
        .scout
        .tasks
        .run_inline(None, None, |abort| {
            let session = session.clone();
            let nav = nav.clone();
            async move { st.scout.navigate_public(&session, worker, &nav, &abort).await }
        })
        .await;

    Ok(Json(out?).into_response())
}

async fn extract(State(state): State<AppState>, req: Request<Body>) -> Result<Response> {
    let owner = owner_of(&req);
    let body = read_json(req, state.cfg.max_body_bytes).await?;
    let er = crate::validate::extract(&body, &state.cfg)?;
    // The abort token is owned by the admission wrapper below.
    let st = &state;
    let out = st
        .scout
        .tasks
        .run_inline(None, None, |abort| {
            let owner = owner.clone();
            let er = er.clone();
            async move { st.scout.extract(&owner, er, &abort).await }
        })
        .await;
    Ok(Json(out?).into_response())
}

async fn interact(State(state): State<AppState>, req: Request<Body>) -> Result<Response> {
    let owner = owner_of(&req);
    let body = read_json(req, state.cfg.max_body_bytes).await?;
    let ir = crate::validate::interact(&body, &state.cfg)?;
    // The abort token is owned by the admission wrapper below.
    let st = &state;
    let out = st
        .scout
        .tasks
        .run_inline(None, None, |abort| {
            let owner = owner.clone();
            let ir = ir.clone();
            async move { st.scout.interact(&owner, ir, &abort).await }
        })
        .await;
    Ok(Json(out?).into_response())
}

async fn eval(State(state): State<AppState>, req: Request<Body>) -> Result<Response> {
    let owner = owner_of(&req);
    let body = read_json(req, state.cfg.max_body_bytes).await?;
    let m = body
        .as_object()
        .ok_or_else(|| ScoutError::invalid("body must be an object"))?;
    for k in m.keys() {
        if !["sessionId", "script"].contains(&k.as_str()) {
            return Err(ScoutError::invalid(format!("Unknown field \"{k}\" in body")));
        }
    }
    let session_id = m
        .get("sessionId")
        .and_then(|s| s.as_str())
        .ok_or_else(|| ScoutError::invalid("\"sessionId\" is required"))?
        .to_string();
    let script = m
        .get("script")
        .and_then(|s| s.as_str())
        .ok_or_else(|| ScoutError::invalid("\"script\" is required"))?
        .to_string();
    if script.len() > 64 * 1024 {
        return Err(ScoutError::invalid("\"script\" must not exceed 64 KB"));
    }
    // The abort token is owned by the admission wrapper below.
    let st = &state;
    let out = st
        .scout
        .tasks
        .run_inline(None, None, |abort| {
            let owner = owner.clone();
            let session_id = session_id.clone();
            let script = script.clone();
            async move { st.scout.eval(&owner, &session_id, script, &abort).await }
        })
        .await;
    Ok(Json(out?).into_response())
}

async fn create_task(State(state): State<AppState>, req: Request<Body>) -> Result<Response> {
    let owner = owner_of(&req);
    let body = read_json(req, state.cfg.max_body_bytes).await?;
    let tr = crate::validate::task(&body, &state.cfg)?;
    let view = state.scout.submit_task(&owner, tr)?;
    let id = view.get("id").and_then(|i| i.as_str()).unwrap_or("").to_string();
    let mut res = (StatusCode::ACCEPTED, Json(view)).into_response();
    if let Ok(v) = axum::http::HeaderValue::from_str(&format!("/v1/tasks/{id}")) {
        res.headers_mut().insert(axum::http::header::LOCATION, v);
    }
    Ok(res)
}

async fn get_task(State(state): State<AppState>, Path(id): Path<String>, req: Request<Body>) -> Result<Response> {
    let owner = owner_of(&req);
    Ok(Json(state.scout.get_task(&owner, &id)?).into_response())
}

async fn cancel_task(State(state): State<AppState>, Path(id): Path<String>, req: Request<Body>) -> Result<Response> {
    let owner = owner_of(&req);
    Ok(Json(state.scout.cancel_task(&owner, &id)?).into_response())
}

async fn not_found(method: Method, uri: axum::http::Uri) -> Response {
    let allow = match uri.path() {
        "/health" | "/metrics" | "/v1/strategies" => "GET",
        "/v1/sessions" | "/v1/navigate" | "/v1/extract" | "/v1/interact" | "/v1/eval" | "/v1/tasks" => "POST",
        p if p.starts_with("/v1/sessions/") => "DELETE",
        p if p.starts_with("/v1/tasks/") => "GET, POST",
        _ => "",
    };
    if !allow.is_empty() {
        let mut res = ScoutError::new(
            ErrorCode::MethodNotAllowed,
            format!("Method {} is not allowed for {}", method.as_str(), uri.path()),
        )
        .into_response();
        if let Ok(v) = axum::http::HeaderValue::from_str(allow) {
            res.headers_mut().insert(axum::http::header::ALLOW, v);
        }
        return res;
    }
    ScoutError::not_found("No such endpoint").into_response()
}

pub fn build_router(state: AppState) -> Router {
    let mw = state.clone();
    Router::new()
        .route("/health", get(health))
        .route("/metrics", get(metrics))
        .route("/v1/strategies", get(strategies))
        .route("/v1/tools", get(tools_manifest))
        .route("/v1/tools/call", post(tools_call))
        .route("/v1/sessions", post(create_session))
        .route("/v1/sessions/:id", delete(destroy_session))
        .route("/v1/navigate", post(navigate))
        .route("/v1/extract", post(extract))
        .route("/v1/interact", post(interact))
        .route("/v1/eval", post(eval))
        .route("/v1/tasks", post(create_task))
        .route("/v1/tasks/:id", get(get_task))
        .route("/v1/tasks/:id/cancel", post(cancel_task))
        .fallback(not_found)
        .layer(middleware::from_fn_with_state(mw, guard))
        .with_state(state)
}

/// Start the service and return a handle that owns the listener.
pub async fn start(cfg: Config) -> Result<(Arc<Scout>, tokio::task::JoinHandle<()>, std::net::SocketAddr)> {
    let scout = Scout::new(cfg.clone())?;
    let state = AppState::new(scout.clone());
    let router = build_router(state);
    let addr: SocketAddr = format!("{}:{}", cfg.host, cfg.port)
        .parse()
        .map_err(|e| ScoutError::new(ErrorCode::Internal, format!("Invalid bind address: {e}")))?;
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| ScoutError::new(ErrorCode::Internal, format!("Could not bind {addr}: {e}")))?;
    let local = listener
        .local_addr()
        .map_err(|e| ScoutError::new(ErrorCode::Internal, format!("{e}")))?;

    // Own the accept loop rather than handing the listener to `axum::serve`, so
    // TCP_NODELAY can be set per connection. Without it, a response written in
    // more than one segment is held by Nagle until the peer's delayed ACK
    // arrives — a flat ~40 ms on Linux for every request after the first.
    let handle = tokio::spawn(async move {
        let make = router.into_make_service_with_connect_info::<SocketAddr>();
        loop {
            let (stream, peer) = match listener.accept().await {
                Ok(v) => v,
                Err(_) => continue,
            };
            let _ = stream.set_nodelay(true);
            let mut make = make.clone();
            tokio::spawn(async move {
                use tower::Service;
                let service = match make.call(peer).await {
                    Ok(s) => s,
                    Err(_) => return,
                };
                let hyper_service =
                    hyper_util::service::TowerToHyperService::new(service);
                let builder: hyper_util::server::conn::auto::Builder<hyper_util::rt::TokioExecutor> =
                    hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new());
                let _ = builder
                    .serve_connection_with_upgrades(
                        hyper_util::rt::TokioIo::new(stream),
                        hyper_service,
                    )
                    .await;
            });
        }
    });

    Ok((scout, handle, local))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limiter_blocks_after_the_limit() {
        let rl = RateLimiter::new(3);
        assert!(rl.check("k").is_ok());
        assert!(rl.check("k").is_ok());
        assert!(rl.check("k").is_ok());
        assert!(rl.check("k").is_err());
        // A different key is unaffected.
        assert!(rl.check("other").is_ok());
    }

    #[test]
    fn rate_limiter_reports_retry_after() {
        let rl = RateLimiter::new(1);
        rl.check("k").unwrap();
        let retry = rl.check("k").unwrap_err();
        assert!(retry >= 1 && retry <= 60, "retry-after was {retry}");
    }

    #[test]
    fn route_names_are_stable() {
        assert_eq!(route_name(&Method::GET, "/health"), "health");
        assert_eq!(route_name(&Method::POST, "/v1/extract"), "extract");
        assert_eq!(route_name(&Method::GET, "/v1/tasks/tsk_1"), "tasks.get");
        assert_eq!(route_name(&Method::POST, "/v1/tasks/tsk_1/cancel"), "tasks.cancel");
    }
}
