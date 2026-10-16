//! Orchestration. Transport-agnostic: the HTTP layer is a thin adapter over this.

use crate::abort::Abort;
use crate::config::Config;
use crate::error::{ErrorCode, Result, ScoutError};
use crate::interaction::Effect;
use crate::js;
use crate::metrics::Metrics;
use crate::resources::ResourceManager;
use crate::security::{guarded_fetch, Fetched, FetchKind, FetchRequest};
use crate::session::{Session, SessionManager};
use crate::tasks::TaskManager;
use crate::validate::{clamp_timeout, ExtractReq, InteractReq, NavigateReq, TaskReq};
use crate::worker::{Cmd, ScriptSource, StrategySpec, WorkerPool};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub struct Scout {
    pub cfg: Arc<Config>,
    pub metrics: Arc<Metrics>,
    pub resources: ResourceManager,
    pub sessions: Arc<SessionManager>,
    pub tasks: Arc<TaskManager>,
    pub workers: Arc<WorkerPool>,
    pub client: reqwest::Client,
    /// Handle on the async runtime, so a blocking in-page request issued from a
    /// page worker can drive the same guarded client everything else uses.
    handle: Option<tokio::runtime::Handle>,
    /// Session id -> worker index, so a session always lands on the worker that
    /// holds its DOM.
    routing: Mutex<HashMap<String, usize>>,
    stopping: std::sync::atomic::AtomicBool,
}

impl Scout {
    pub fn new(cfg: Config) -> Result<Arc<Self>> {
        let cfg = Arc::new(cfg);
        let metrics = Arc::new(Metrics::new());
        let resources = ResourceManager::new(cfg.clone());
        let sessions = Arc::new(SessionManager::new((*cfg).clone()));
        let tasks = Arc::new(TaskManager::new(cfg.clone(), metrics.clone(), resources.clone()));
        let workers = Arc::new(WorkerPool::new(cfg.clone()));

        // Redirects are followed manually so every hop is validated; the client's
        // own redirect handling is therefore disabled.
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            // Without this, a small response written in two segments can be held
            // by Nagle until the peer's delayed ACK arrives (~40 ms on Linux).
            .tcp_nodelay(true)
            .dns_resolver(Arc::new(crate::security::ValidatingResolver::new(&cfg)))
            .pool_max_idle_per_host(4)
            .connect_timeout(Duration::from_millis(10_000))
            .build()
            .map_err(|e| ScoutError::new(ErrorCode::Internal, format!("HTTP client: {e}")))?;

        let scout = Arc::new(Self {
            cfg: cfg.clone(),
            metrics: metrics.clone(),
            resources,
            sessions,
            tasks,
            workers,
            client,
            handle: tokio::runtime::Handle::try_current().ok(),
            routing: Mutex::new(HashMap::new()),
            stopping: std::sync::atomic::AtomicBool::new(false),
        });

        let s = scout.clone();
        metrics.gauge(
            "scout_rss_bytes",
            "Resident set size of the Scout process",
            &[],
            move || s.resources.rss_bytes() as f64,
        );
        let s = scout.clone();
        metrics.gauge(
            "scout_sessions",
            "Live sessions",
            &[],
            move || s.sessions.size() as f64,
        );
        let s = scout.clone();
        metrics.gauge(
            "scout_tasks_queued",
            "Tasks admitted but not yet running",
            &[],
            move || s.tasks.stats().0 as f64,
        );
        let s = scout.clone();
        metrics.gauge(
            "scout_tasks_running",
            "Tasks currently running",
            &[],
            move || s.tasks.stats().1 as f64,
        );
        Ok(scout)
    }

    // ------------------------------------------------------------- sessions

    pub fn create_session(&self, owner: &str) -> Result<Value> {
        let s = self.sessions.create(owner.to_string(), false)?;
        let idx = self.workers.assign();
        self.routing.lock().unwrap().insert(s.id.clone(), idx);
        Ok(json!({
            "id": s.id,
            "createdAt": crate::logger::iso8601_from_unix_ms(s.created_at),
            "expiresInMs": self.cfg.session_ttl_ms,
        }))
    }

    pub fn destroy_session(&self, owner: &str, id: &str) -> Result<()> {
        self.sessions.destroy(owner, id)?;
        self.release_session(id);
        Ok(())
    }

    fn release_session(&self, id: &str) {
        self.sessions.destroy_internal(id);
        if let Some(idx) = self.routing.lock().unwrap().remove(id) {
            let _ = self.workers.send(idx, Cmd::Drop { session: id.to_string() });
        }
    }

    fn worker_of(&self, session_id: &str) -> Result<usize> {
        self.routing
            .lock()
            .unwrap()
            .get(session_id)
            .copied()
            .ok_or_else(|| ScoutError::not_found("Session not found"))
    }

    // ---------------------------------------------------------- navigation

    /// Fetch and load a URL into a session.
    async fn do_navigate(
        &self,
        session: &Session,
        worker: usize,
        nav: &NavigateReq,
        abort: &Abort,
    ) -> Result<Value> {
        let timeout = clamp_timeout(nav.timeout_ms, self.cfg.nav_timeout_ms, self.cfg.max_nav_timeout_ms);
        let fetched = self.fetch(session, nav, timeout, abort).await?;
        let page_url = fetched.url.clone();

        // Parsing and DOM construction happen on the page worker.
        let loaded = self
            .workers
            .call(worker, |reply| Cmd::Load {
                session: session.id.clone(),
                fetched: Box::new(fetched),
                reply,
            })
            .await?;

        if loaded.scripts.is_empty() {
            return Ok(loaded.summary);
        }

        // External scripts are fetched on the async side through the same
        // guarded client as the document, so the SSRF policy applies to them
        // too rather than being bypassed by a page that loads its own code.
        let bodies = self
            .fetch_scripts(session, &loaded.summary, &loaded.scripts, timeout, abort)
            .await;

        let net_stats = std::sync::Arc::new(Mutex::new(js::NetStats::default()));
        let net = if self.cfg.js_network {
            Some(self.page_net(session, &page_url, net_stats.clone()))
        } else {
            None
        };

        let mut summary = loaded.summary;
        match self
            .workers
            .call(worker, |reply| Cmd::RunScripts {
                session: session.id.clone(),
                scripts: bodies,
                net,
                reply,
            })
            .await
        {
            Ok(js) => {
                let mut js = js;
                let stats = *net_stats.lock().unwrap();
                if stats.requests > 0 || stats.blocked > 0 {
                    if let Some(obj) = js.as_object_mut() {
                        obj.insert(
                            "network".into(),
                            json!({
                                "requests": stats.requests,
                                "failed": stats.failed,
                                "blocked": stats.blocked,
                                "bytes": stats.bytes,
                            }),
                        );
                    }
                }
                if let Some(obj) = summary.as_object_mut() {
                    obj.insert("js".into(), js);
                }
            }
            // A page stays usable even when its scripts cannot run; the
            // failure is reported alongside the document, not instead of it.
            Err(e) => {
                if let Some(obj) = summary.as_object_mut() {
                    obj.insert("js".into(), json!({ "error": e.message }));
                }
            }
        }
        Ok(summary)
    }

    /// Fetch a page's external scripts through the guarded client.
    ///
    /// A script that cannot be fetched becomes an empty body rather than
    /// failing the navigation: one missing analytics bundle must not make an
    /// otherwise readable page unreadable.
    async fn fetch_scripts(
        &self,
        session: &Session,
        summary: &Value,
        scripts: &[ScriptSource],
        timeout_ms: u64,
        abort: &Abort,
    ) -> Vec<String> {
        let base = summary.get("url").and_then(|u| u.as_str()).unwrap_or("");
        let mut budget = self.cfg.js_max_script_bytes;
        let mut out = Vec::with_capacity(scripts.len());

        for script in scripts {
            match script {
                ScriptSource::Inline(body) => out.push(body.clone()),
                ScriptSource::External(src) => {
                    let joined = match url::Url::parse(base).and_then(|b| b.join(src)) {
                        Ok(u) => u,
                        Err(_) => {
                            out.push(String::new());
                            continue;
                        }
                    };
                    if budget == 0 {
                        out.push(String::new());
                        continue;
                    }
                    let referer = session.referrer();
                    let fut = guarded_fetch(
                        &self.cfg,
                        &self.client,
                        FetchRequest {
                            url: joined.as_str(),
                            method: "GET",
                            headers: Vec::new(),
                            body: None,
                            jar: Some(&session.jar),
                            kind: FetchKind::Script,
                            referer: referer.as_deref(),
                            timeout_ms,
                            max_bytes: budget.min(self.cfg.max_response_bytes),
                        },
                    );
                    let res = tokio::select! {
                        biased;
                        _ = abort.token().cancelled() => Err(abort.reason()),
                        r = fut => r,
                    };
                    match res {
                        Ok(f) if f.status < 400 => {
                            budget = budget.saturating_sub(f.bytes);
                            out.push(f.body);
                        }
                        _ => out.push(String::new()),
                    }
                }
            }
        }
        out
    }

    async fn fetch(
        &self,
        session: &Session,
        nav: &NavigateReq,
        timeout_ms: u64,
        abort: &Abort,
    ) -> Result<Fetched> {
        let url = crate::security::validate_url(&nav.url, &self.cfg)?;

        let mut headers: Vec<(String, String)> = Vec::new();
        if let Some(ct) = &nav.content_type {
            headers.push(("content-type".into(), ct.clone()));
        }

        let referer = session.referrer();

        let fut = guarded_fetch(
            &self.cfg,
            &self.client,
            FetchRequest {
                url: url.as_str(),
                method: &nav.method,
                headers,
                body: nav.body.as_ref().map(|b| b.as_bytes().to_vec()),
                jar: Some(&session.jar),
                kind: FetchKind::Document,
                referer: referer.as_deref(),
                timeout_ms,
                max_bytes: self.cfg.max_response_bytes,
            },
        );

        let result = tokio::select! {
            biased;
            _ = abort.token().cancelled() => Err(abort.reason()),
            r = fut => r,
        }?;

        // Subsequent requests come *from* this document, which is what makes a
        // form POST carry the Referer and Origin a server expects to see.
        session.set_referrer(&result.url);

        self.metrics.inc(
            "scout_navigations_total",
            "Navigation attempts by outcome",
            &[("outcome", "ok")],
        );
        Ok(result)
    }

    /// The network a page's scripts see.
    ///
    /// Built here, on the async side, because everything that constrains a
    /// scripted request lives here: the guarded client with its SSRF policy,
    /// the session's cookie jar, and the configured budgets. The worker thread
    /// only ever holds this callback, so page JavaScript cannot reach the
    /// client, and a request from a script is the same request a navigation
    /// would make.
    fn page_net(
        &self,
        session: &Session,
        base_url: &str,
        stats: std::sync::Arc<Mutex<js::NetStats>>,
    ) -> js::NetFn {
        let client = self.client.clone();
        let cfg = self.cfg.clone();
        let jar = session.jar.clone();
        let base = base_url.to_string();
        let referer = session.referrer().unwrap_or_else(|| base_url.to_string());
        let handle = self.handle.clone();
        let max_requests = self.cfg.js_max_fetches;
        let max_bytes = self.cfg.js_max_fetch_bytes.min(self.cfg.max_response_bytes);
        let timeout_ms = self.cfg.js_fetch_timeout_ms;

        std::sync::Arc::new(move |req: js::NetRequest| -> std::result::Result<js::NetResponse, String> {
            {
                let mut st = stats.lock().unwrap();
                if st.requests >= max_requests {
                    st.blocked += 1;
                    return Err(format!(
                        "this page has used its budget of {max_requests} scripted requests"
                    ));
                }
                st.requests += 1;
            }
            if !matches!(
                req.method.as_str(),
                "GET" | "POST" | "PUT" | "PATCH" | "DELETE" | "HEAD" | "OPTIONS"
            ) {
                return Err(format!("method \"{}\" is not supported", req.method));
            }
            let joined = match url::Url::parse(&base).and_then(|b| b.join(&req.url)) {
                Ok(u) => u,
                Err(_) => {
                    return Err(format!(
                        "could not resolve \"{}\" against the page URL",
                        req.url
                    ))
                }
            };
            let body = match &req.body {
                None => None,
                Some(js::NetBody::Text(t)) => Some(t.as_bytes().to_vec()),
                Some(js::NetBody::Bytes(b)) => Some(b.clone()),
            };
            let fut = guarded_fetch(
                &cfg,
                &client,
                FetchRequest {
                    url: joined.as_str(),
                    method: &req.method,
                    headers: req.headers.clone(),
                    body,
                    jar: Some(&jar),
                    kind: FetchKind::Xhr,
                    referer: Some(referer.as_str()),
                    timeout_ms,
                    max_bytes,
                },
            );
            let result = match &handle {
                // The page worker is a plain thread, never a runtime worker, so
                // blocking here cannot stall the executor that is running this
                // very request.
                Some(h) => h.block_on(fut),
                None => {
                    return Err("no async runtime is available for scripted requests".into())
                }
            };
            match result {
                Ok(f) => {
                    let mut st = stats.lock().unwrap();
                    st.bytes += f.bytes;
                    Ok(js::NetResponse {
                        status: f.status,
                        status_text: crate::security::status_text(f.status).to_string(),
                        url: f.url,
                        headers: f.headers,
                        body: f.body,
                        body_b64: f.body_b64,
                    })
                }
                Err(e) => {
                    let mut st = stats.lock().unwrap();
                    st.failed += 1;
                    Err(e.message)
                }
            }
        })
    }

    /// One request through the session's jar and the SSRF policy, with the
    /// result returned rather than parsed into a page.
    ///
    /// This is the primitive behind authenticated CRUD: the session signs in
    /// once, and then a JSON API on that origin is addressable with the session
    /// cookie attached, under exactly the same policy as a navigation.
    pub async fn request(&self, owner: &str, req: crate::validate::RequestReq, abort: &Abort) -> Result<Value> {
        let (session, ephemeral) = match &req.session_id {
            Some(id) => (self.sessions.get(owner, id)?, false),
            None => (self.sessions.create(owner.to_string(), true)?, true),
        };
        let timeout = clamp_timeout(
            req.timeout_ms,
            self.cfg.nav_timeout_ms,
            self.cfg.max_nav_timeout_ms,
        );
        let result = async {
            let _guard = session.lock().await?;
            let referer = session.referrer();
            let content_type = req.content_type.clone();
            let mut headers = req.headers.clone();
            if let Some(ct) = &content_type {
                headers.push(("content-type".into(), ct.clone()));
            }
            let fut = guarded_fetch(
                &self.cfg,
                &self.client,
                FetchRequest {
                    url: &req.url,
                    method: &req.method,
                    headers,
                    body: req.body.as_ref().map(|b| b.as_bytes().to_vec()),
                    jar: Some(&session.jar),
                    kind: FetchKind::Xhr,
                    referer: referer.as_deref(),
                    timeout_ms: timeout,
                    max_bytes: self.cfg.max_response_bytes.min(512 * 1024),
                },
            );
            let fetched = tokio::select! {
                biased;
                _ = abort.token().cancelled() => Err(abort.reason()),
                r = fut => r,
            }?;
            let mut head = serde_json::Map::new();
            for (k, v) in &fetched.headers {
                head.insert(k.clone(), Value::String(v.clone()));
            }
            Ok(json!({
                "url": fetched.url,
                "status": fetched.status,
                "statusText": crate::security::status_text(fetched.status),
                "contentType": fetched.content_type,
                "headers": Value::Object(head),
                "body": fetched.body,
                "bodyBase64": fetched.body_b64,
                "bytes": fetched.bytes,
                "redirects": fetched.redirects,
            }))
        }
        .await;

        if ephemeral {
            self.release_session(&session.id);
        }
        result
    }

    // ------------------------------------------------------------- extracts

    fn resolve_strategy(&self, req: &ExtractReq) -> Result<(StrategySpec, &'static str, bool)> {
        let name: &str = match (&req.strategy, &req.schema) {
            (Some(s), _) => s.as_str(),
            (None, Some(_)) => "selectors",
            (None, None) => "generic-page",
        };
        let info = crate::adapters::registry()
            .into_iter()
            .find(|s| s.name == name)
            .ok_or_else(|| ScoutError::invalid(format!("Unknown strategy \"{name}\"")))?;

        if !info.status.is_runnable() {
            return Err(ScoutError::unsupported(
                info.reason.unwrap_or("Strategy is not implemented"),
            )
            .with_details(json!({ "strategy": name })));
        }

        // Strategies that read embedded JSON still work on a page whose visible
        // text is thin, so the heuristic JS warning is suppressed for them.
        let embedded = matches!(name, "youtube-channel-videos");
        let spec = match name {
            "selectors" => {
                let schema = req
                    .schema
                    .clone()
                    .ok_or_else(|| ScoutError::invalid("strategy \"selectors\" requires a \"schema\""))?;
                StrategySpec::Selectors(schema)
            }
            "youtube-channel-videos" => StrategySpec::YoutubeChannelVideos,
            _ => StrategySpec::GenericPage,
        };
        // Leak-free: names come from the static registry.
        let static_name: &'static str = crate::adapters::registry()
            .into_iter()
            .find(|s| s.name == name)
            .map(|s| s.name)
            .unwrap();
        Ok((spec, static_name, embedded))
    }

    /// Strategy-level URL preparation (`prepareUrl` in Node): a channel root is
    /// pointed at the tab the strategy reads, and a foreign host is refused
    /// before any fetch.
    fn prepare_strategy_url(
        &self,
        spec: &StrategySpec,
        url: &str,
    ) -> Result<String> {
        match spec {
            StrategySpec::YoutubeChannelVideos => {
                crate::adapters::youtube::prepare_url(&self.cfg.youtube_hosts, url)
            }
            _ => Ok(url.to_string()),
        }
    }

    pub async fn extract(&self, owner: &str, req: ExtractReq, abort: &Abort) -> Result<Value> {
        // Resolve the strategy before any network or queue use, so an unknown
        // strategy fails fast and cheaply.
        let (spec, strategy_name, embedded) = self.resolve_strategy(&req)?;
        let default_limit = crate::adapters::registry()
            .into_iter()
            .find(|s| s.name == strategy_name)
            .map(|s| s.default_limit)
            .unwrap_or(20);
        let limit = req.limit.unwrap_or(default_limit).min(self.cfg.max_extract_items);

        // Ephemeral session when the caller supplied a URL but no session.
        let (session, worker, ephemeral) = match &req.session_id {
            Some(id) => {
                let s = self.sessions.get(owner, id)?;
                let w = self.worker_of(id)?;
                (s, w, false)
            }
            None => {
                let s = self.sessions.create(owner.to_string(), true)?;
                let w = self.workers.assign();
                self.routing.lock().unwrap().insert(s.id.clone(), w);
                (s.clone(), w, true)
            }
        };

        let result = async {
            let _guard = session.lock().await?;
            let mut nav_warnings: Vec<Value> = Vec::new();

            if let Some(url) = &req.url {
                // Strategy-level URL preparation runs before any fetch, so a bad
                // host is refused cheaply and a channel root is pointed at the
                // tab the strategy actually reads.
                let target = self.prepare_strategy_url(&spec, url)?;
                let nav = NavigateReq {
                    url: target,
                    session_id: Some(session.id.clone()),
                    timeout_ms: req.timeout_ms,
                    method: "GET".into(),
                    body: None,
                    content_type: None,
                };
                let summary = self.navigate_with_retry(&session, worker, &nav, abort, self.cfg.max_retries).await?;
                if let Some(w) = summary.get("warnings").and_then(|w| w.as_array()) {
                    nav_warnings.extend(w.iter().cloned());
                }
            }

            let started = std::time::Instant::now();
            let out: Value = self
                .workers
                .call(worker, |reply| Cmd::Extract {
                    session: session.id.clone(),
                    spec: spec.clone(),
                    limit,
                    reply,
                })
                .await?;

            let mut warnings: Vec<Value> = nav_warnings
                .into_iter()
                .filter(|w| !(embedded && w.get("code").and_then(|c| c.as_str()) == Some("js_required_likely")))
                .collect();
            if let Some(ws) = out.get("warnings").and_then(|w| w.as_array()) {
                warnings.extend(ws.iter().cloned());
            }

            Ok(json!({
                "data": out.get("data").cloned().unwrap_or(Value::Null),
                "warnings": warnings,
                "meta": {
                    "strategy": strategy_name,
                    "url": out.get("page").and_then(|p| p.get("url")).cloned().unwrap_or(Value::Null),
                    "status": out.get("page").and_then(|p| p.get("status")).cloned().unwrap_or(Value::Null),
                    "durationMs": started.elapsed().as_millis() as u64,
                },
            }))
        }
        .await;

        if ephemeral {
            self.release_session(&session.id);
        }
        result
    }

    /// Retry only navigation. Interactions are never retried, because repeating
    /// one is not safe.
    async fn navigate_with_retry(
        &self,
        session: &Session,
        worker: usize,
        nav: &NavigateReq,
        abort: &Abort,
        retries: u32,
    ) -> Result<Value> {
        let mut attempt = 0u32;
        loop {
            match self.do_navigate(session, worker, nav, abort).await {
                Ok(v) => {
                    self.metrics.inc(
                        "scout_navigations_total",
                        "Navigation attempts by outcome",
                        &[("outcome", "ok")],
                    );
                    return Ok(v);
                }
                Err(e) => {
                    self.metrics.inc(
                        "scout_navigations_total",
                        "Navigation attempts by outcome",
                        &[("outcome", if e.retryable { "retryable_error" } else { "error" })],
                    );
                    if !e.retryable || attempt >= retries {
                        return Err(e);
                    }
                    self.metrics.inc(
                        "scout_navigation_retries_total",
                        "Navigation retries performed",
                        &[],
                    );
                    let backoff = std::cmp::min(250u64 * 2u64.pow(attempt), 2_000);
                    let jitter = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.subsec_nanos() as u64)
                        .unwrap_or(0)
                        % 100;
                    tokio::select! {
                        biased;
                        _ = abort.token().cancelled() => return Err(abort.reason()),
                        _ = tokio::time::sleep(Duration::from_millis(backoff + jitter)) => {}
                    }
                    attempt += 1;
                }
            }
        }
    }

    // ------------------------------------------------------------ interact

    pub async fn interact(&self, owner: &str, req: InteractReq, abort: &Abort) -> Result<Value> {
        let session = self.sessions.get(owner, &req.session_id)?;
        let worker = self.worker_of(&req.session_id)?;
        let _guard = session.lock().await?;

        let effect: Effect = self
            .workers
            .call(worker, |reply| Cmd::Interact {
                session: session.id.clone(),
                action: req.action.clone(),
                reply,
            })
            .await?;

        // The response carries both the descriptive `effect` field and the
        // `ok`/`type`/`navigated`/`page` fields the published contract uses, so
        // existing SDK consumers read what they always read.
        let kind = req.action.kind.clone();
        match effect {
            Effect::None { detail } => Ok(json!({
                "ok": true,
                "type": kind,
                "navigated": false,
                "effect": "none",
                "detail": detail,
            })),
            Effect::Navigate {
                url,
                method,
                body,
                content_type,
                timeout_ms,
            } => {
                let nav = NavigateReq {
                    url,
                    session_id: Some(session.id.clone()),
                    // A per-action budget wins; otherwise the request's.
                    timeout_ms: timeout_ms.or(req.action.timeout_ms),
                    method,
                    body,
                    content_type,
                };
                // The interaction itself is not retried, but the navigation it
                // produced is a plain fetch and may be.
                let summary = self
                    .navigate_with_retry(&session, worker, &nav, abort, self.cfg.max_retries)
                    .await?;
                Ok(json!({
                    "ok": true,
                    "type": kind,
                    "navigated": true,
                    "effect": "navigate",
                    "navigation": summary,
                    "page": {
                        "url": summary.get("url").cloned().unwrap_or(Value::Null),
                        "status": summary.get("status").cloned().unwrap_or(Value::Null),
                        "title": summary.get("title").cloned().unwrap_or(Value::Null),
                    },
                    "warnings": summary.get("warnings").cloned().unwrap_or(json!([])),
                }))
            }
        }
    }

    #[allow(dead_code)]
    async fn worker_stats(&self, worker: usize) -> Result<Value> {
        self.workers.call(worker, |reply| Cmd::Stats { reply }).await
    }

    /// Execute JavaScript against a session's loaded page.
    pub async fn eval(&self, owner: &str, session_id: &str, script: String, abort: &Abort) -> Result<Value> {
        let session = self.sessions.get(owner, session_id)?;
        let worker = self.worker_of(session_id)?;
        let _guard = session.lock().await?;
        // /v1/eval runs on the page that is loaded, so relative URLs and
        // cookies resolve against it exactly as they do for a page script.
        let page_url = session.referrer().unwrap_or_default();
        let net = if self.cfg.js_network {
            Some(self.page_net(&session, &page_url, std::sync::Arc::new(Mutex::new(js::NetStats::default()))))
        } else {
            None
        };
        tokio::select! {
            biased;
            _ = abort.token().cancelled() => Err(abort.reason()),
            r = self.workers.call(worker, |reply| Cmd::Eval { session: session.id.clone(), script, net, reply }) => r,
        }
    }

    // --------------------------------------------------------------- tasks

    pub fn submit_task(self: &Arc<Self>, owner: &str, req: TaskReq) -> Result<Value> {
        // Validate sessions up front so an unknown session is a 404 at submit
        // time rather than a task that fails asynchronously.
        if let Some(sid) = &req.session_id {
            self.sessions.get(owner, sid)?;
        }
        let retries = req.retries.unwrap_or(self.cfg.max_retries);
        let timeout = clamp_timeout(req.timeout_ms, self.cfg.task_timeout_ms, self.cfg.max_task_timeout_ms);
        let step_count = req.steps.len();

        let this = self.clone();
        let owner_owned = owner.to_string();
        let view = self.tasks.submit(owner.to_string(), Some(timeout), move |abort| async move {
            this.run_task(&owner_owned, req, retries, abort).await
        })?;
        self.metrics.inc(
            "scout_tasks_submitted_total",
            "Tasks accepted by the queue",
            &[("steps", &step_count.to_string())],
        );
        Ok(view)
    }

    async fn run_task(self: &Arc<Self>, owner: &str, req: TaskReq, retries: u32, abort: Abort) -> Result<Value> {
        let (session, worker, ephemeral) = match &req.session_id {
            Some(id) => (self.sessions.get(owner, id)?, self.worker_of(id)?, false),
            None => {
                let s = self.sessions.create(owner.to_string(), true)?;
                let w = self.workers.assign();
                self.routing.lock().unwrap().insert(s.id.clone(), w);
                (s.clone(), w, true)
            }
        };

        let result = async {
            let _guard = session.lock().await?;
            let mut steps: Vec<Value> = Vec::new();
            let mut data: Option<Value> = None;

            for (index, step) in req.steps.iter().enumerate() {
                if abort.is_cancelled() {
                    return Err(abort.reason());
                }
                let started = std::time::Instant::now();
                // Each step runs inside its own future so that a `?` inside an
                // arm reports the *step* that failed rather than escaping the
                // loop and losing the step index.
                let outcome: Result<Value> = async {
                    match step.op.as_str() {
                    "navigate" => {
                        // The step was validated at submit time. Build the request
                        // directly rather than re-running the endpoint validator,
                        // which (like Node's `parseNavigate`) expects a session to
                        // come from the caller.
                        let nav = NavigateReq {
                            url: step
                                .params
                                .get("url")
                                .and_then(|u| u.as_str())
                                .unwrap_or_default()
                                .to_string(),
                            session_id: Some(session.id.clone()),
                            timeout_ms: step.params.get("timeoutMs").and_then(|t| t.as_u64()),
                            method: "GET".into(),
                            body: None,
                            content_type: None,
                        };
                        self.navigate_with_retry(&session, worker, &nav, &abort, retries).await
                    }
                    "extract" => {
                        let mut er = {
                            let mut probe = step.params.clone();
                            if !probe.get("url").is_some() {
                                if let Some(o) = probe.as_object_mut() {
                                    o.insert("sessionId".into(), json!(session.id));
                                }
                            }
                            crate::validate::extract(&probe, &self.cfg)?
                        };
                        // The task's session already holds the page, so an explicit
                        // url on the step is cleared rather than re-fetched.
                        er.session_id = Some(session.id.clone());
                        er.url = None;
                        let (spec, name, embedded) = self.resolve_strategy(&er)?;
                        // Node's `#extract`: a url on the step navigates the session
                        // there first, through the strategy's URL preparation.
                        if let Some(u) = er.url.clone() {
                            let target = self.prepare_strategy_url(&spec, &u)?;
                            let nav = NavigateReq {
                                url: target,
                                session_id: Some(session.id.clone()),
                                timeout_ms: er.timeout_ms,
                                method: "GET".into(),
                                body: None,
                                content_type: None,
                            };
                            self.navigate_with_retry(&session, worker, &nav, &abort, retries)
                                .await?;
                        }
                        let default_limit = crate::adapters::registry()
                            .into_iter()
                            .find(|s| s.name == name)
                            .map(|s| s.default_limit)
                            .unwrap_or(20);
                        let limit = er.limit.unwrap_or(default_limit).min(self.cfg.max_extract_items);
                        let out: Value = self
                            .workers
                            .call(worker, |reply| Cmd::Extract {
                                session: session.id.clone(),
                                spec: spec.clone(),
                                limit,
                                reply,
                            })
                            .await?;
                        data = out.get("data").cloned();
                        let mut warnings: Vec<Value> = out
                            .get("warnings")
                            .and_then(|w| w.as_array())
                            .cloned()
                            .unwrap_or_default();
                        if embedded {
                            warnings.retain(|w| w.get("code").and_then(|c| c.as_str()) != Some("js_required_likely"));
                        }
                        Ok(json!({ "data": out.get("data").cloned().unwrap_or(Value::Null), "warnings": warnings }))
                    }
                    "interact" => {
                        let mut probe = step.params.clone();
                        if let Some(o) = probe.as_object_mut() {
                            o.insert("sessionId".into(), json!(session.id));
                        }
                        let ir = crate::validate::interact(&probe, &self.cfg)?;
                        let action = ir.action.clone();
                        let effect: Effect = self
                            .workers
                            .call(worker, |reply| Cmd::Interact {
                                session: session.id.clone(),
                                action,
                                reply,
                            })
                            .await?;
                        // Interactions are never retried: repeating one is not safe.
                        match effect {
                            Effect::None { detail } => Ok(json!({ "effect": "none", "detail": detail })),
                            Effect::Navigate {
                                url,
                                method,
                                body,
                                content_type,
                                timeout_ms,
                            } => {
                                let nav = NavigateReq {
                                    url,
                                    session_id: Some(session.id.clone()),
                                    timeout_ms,
                                    method,
                                    body,
                                    content_type,
                                };
                                let summary = self.do_navigate(&session, worker, &nav, &abort).await?;
                                Ok(json!({ "effect": "navigate", "navigation": summary }))
                            }
                        }
                    }
                        other => Err(ScoutError::unsupported(format!(
                            "Step op \"{other}\" is not supported"
                        ))),
                    }
                }
                .await;

                match outcome {
                    Ok(v) => steps.push(json!({
                        "index": index,
                        "op": step.op,
                        "ok": true,
                        "durationMs": started.elapsed().as_millis() as u64,
                        "result": v,
                    })),
                    Err(e) => {
                        return Err(ScoutError::new(e.code, e.message)
                            .with_retryable(e.retryable)
                            .with_details(json!({
                                "failedStep": index,
                                "completedSteps": steps.len(),
                                "stepError": e.details,
                            })));
                    }
                }
            }

            Ok(json!({ "steps": steps, "data": data }))
        }
        .await;

        if ephemeral {
            self.release_session(&session.id);
        }
        result
    }

    pub fn get_task(&self, owner: &str, id: &str) -> Result<Value> {
        self.tasks.get(owner, id)
    }

    pub fn cancel_task(&self, owner: &str, id: &str) -> Result<Value> {
        self.tasks.cancel(owner, id)
    }

    // ------------------------------------------------------------ lifecycle

    /// Invoke an agent-facing tool by name. Thin adapter over `tools::call`
    /// so the server does not need to know the tool set.
    pub async fn tools_call(
        self: &Arc<Self>,
        owner: &str,
        name: &str,
        arguments: Value,
        abort: &Abort,
    ) -> Result<Value> {
        crate::tools::call(self, owner, name, arguments, abort).await
    }

    pub fn stats(&self) -> Value {
        json!({
            "sessions": self.sessions.size(),
            "workers": self.workers.worker_count(),
            "tasks": { "queued": self.tasks.stats().0, "running": self.tasks.stats().1 },
            "rssBytes": self.resources.rss_bytes(),
        })
    }

    pub async fn close(&self) {
        self.tasks.shutdown(self.cfg.shutdown_grace_ms).await;
        self.sessions.close();
        self.workers.shutdown();
    }

    pub fn is_stopping(&self) -> bool {
        self.tasks.is_stopping()
    }

    /// Flag consulted by the HTTP layer to refuse new work during shutdown.
    pub fn shutting_down_flag(&self) -> &std::sync::atomic::AtomicBool {
        &self.stopping
    }

    // ---- helpers used by the HTTP layer ----

    pub fn worker_index(&self, session_id: &str) -> Result<usize> {
        self.worker_of(session_id)
    }

    /// Navigate on behalf of a session the caller already resolved.
    pub async fn navigate_public(
        &self,
        session: &Session,
        worker: usize,
        nav: &NavigateReq,
        abort: &Abort,
    ) -> Result<Value> {
        let _guard = session.lock().await?;
        self.navigate_with_retry(session, worker, nav, abort, self.cfg.max_retries)
            .await
    }
}

/// Periodic maintenance: expire idle sessions.
pub fn spawn_session_sweeper(scout: Arc<Scout>) -> tokio::task::JoinHandle<()> {
    let every = std::cmp::max(1_000, scout.cfg.session_ttl_ms / 2);
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(every)).await;
            scout.sessions.sweep();
        }
    })
}

/// Expose worker reassignment clearing for session cleanup paths.
impl Scout {
    pub fn clear_routing(&self, id: &str) {
        self.routing.lock().unwrap().remove(id);
    }
}

/// Keeps the compiler honest about unused atomic imports in release builds.
#[allow(dead_code)]
fn _order() -> Ordering {
    Ordering::SeqCst
}
