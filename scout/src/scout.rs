//! Orchestration. Transport-agnostic: the HTTP layer is a thin adapter over this.

use crate::abort::Abort;
use crate::config::Config;
use crate::error::{ErrorCode, Result, ScoutError};
use crate::interaction::Effect;
use crate::js;
use crate::metrics::Metrics;
use crate::resources::ResourceManager;
use crate::security::{guarded_fetch, Fetched, FetchKind, FetchRequest};
use crate::session::{CachedFetch, Session, SessionManager};
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
    /// Recurring task schedules and their change history.
    pub schedules: Arc<crate::schedules::Scheduler>,
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

        // The worker pool shares the client and the runtime handle: YouTube
        // continuation fetches run from the worker side, where the page's
        // JSON data lives.
        let workers = Arc::new(WorkerPool::new(
            cfg.clone(),
            client.clone(),
            tokio::runtime::Handle::try_current().ok(),
        ));

        let schedules = Arc::new(crate::schedules::Scheduler::new(&cfg));

        let scout = Arc::new(Self {
            cfg: cfg.clone(),
            metrics: metrics.clone(),
            resources,
            sessions,
            tasks,
            workers,
            schedules,
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

    pub(crate) fn worker_of(&self, session_id: &str) -> Result<usize> {
        self.routing
            .lock()
            .unwrap()
            .get(session_id)
            .copied()
            .ok_or_else(|| ScoutError::not_found("Session not found"))
    }

    // ---------------------------------------------------------- navigation

    /// Fetch and load a URL into a session. `script_nav_depth` bounds chains
    /// of script-requested navigations so two pages cannot ping-pong forever.
    async fn do_navigate(
        &self,
        session: &Session,
        worker: usize,
        nav: &NavigateReq,
        abort: &Abort,
        script_nav_depth: usize,
    ) -> Result<Value> {
        let timeout = clamp_timeout(nav.timeout_ms, self.cfg.nav_timeout_ms, self.cfg.max_nav_timeout_ms);
        let mut fetched = self.fetch(session, nav, timeout, abort).await?;
        // <meta http-equiv="refresh" content="N; url=U">: a browser follows it.
        // Follow up to 4 hops, then load whatever the last response was.
        let mut refresh_hops = 0usize;
        while refresh_hops < 4 {
            let Some(target) = crate::page::meta_refresh(&fetched.body, &fetched.url) else {
                break;
            };
            refresh_hops += 1;
            let req = NavigateReq {
                url: target,
                session_id: Some(session.id.clone()),
                timeout_ms: nav.timeout_ms,
                method: "GET".into(),
                body: None,
                content_type: None,
            };
            fetched = self.fetch(session, &req, timeout, abort).await?;
        }
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

        let mut summary = loaded.summary;
        if refresh_hops > 0 {
            if let Some(obj) = summary.as_object_mut() {
                obj.insert("metaRefreshHops".into(), json!(refresh_hops));
            }
        }
        if loaded.scripts.is_empty() {
            return Ok(summary);
        }

        // External scripts are fetched on the async side through the same
        // guarded client as the document, so the SSRF policy applies to them
        // too rather than being bypassed by a page that loads its own code.
        let bodies = self
            .fetch_scripts(session, &summary, &loaded.scripts, timeout, abort)
            .await;

        let net_stats = std::sync::Arc::new(Mutex::new(js::NetStats::default()));
        let net = if self.cfg.js_network {
            Some(self.page_net(session, &page_url, net_stats.clone()))
        } else {
            None
        };

        match self
            .workers
            .call(worker, |reply| Cmd::RunScripts {
                session: session.id.clone(),
                scripts: bodies.clone(),
                net,
                reply,
            })
            .await
        {
            Ok(js) => {
                let mut js = js;
                // Engine memory and the scripts' own warnings belong to the
                // navigation summary too, not just to an event pass.
                if let Some(peak) = js.get("peakMemoryBytes").and_then(|v| v.as_u64()) {
                    if peak > 0 {
                        if let Some(obj) = summary.as_object_mut() {
                            obj.insert("peakMemoryBytes".into(), json!(peak));
                        }
                    }
                }
                if let Some(mut w) = js.get("warnings").and_then(|v| v.as_array()).cloned() {
                    if !w.is_empty() {
                        if let Some(obj) = summary.as_object_mut() {
                            let mut all = obj
                                .get("warnings")
                                .and_then(|v| v.as_array())
                                .cloned()
                                .unwrap_or_default();
                            all.append(&mut w);
                            obj.insert("warnings".into(), json!(all));
                        }
                    }
                }
                // A script or timer asked to leave the page (location.href,
                // assign, replace). Follow it like a browser would, up to a
                // small chain limit, so redirect-via-script pages land where
                // a real visitor lands.
                let pending = js
                    .get("pendingNavigation")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                if let Some(target) = pending.filter(|_| script_nav_depth < 4) {
                    let nav = NavigateReq {
                        url: target,
                        session_id: Some(session.id.clone()),
                        timeout_ms: None,
                        method: "GET".into(),
                        body: None,
                        content_type: None,
                    };
                    let followed = Box::pin(self.do_navigate(&session, worker, &nav, &abort, script_nav_depth + 1)).await?;
                    if let Some(obj) = summary.as_object_mut() {
                        obj.insert("scriptNavigation".into(), followed);
                    }
                    return Ok(summary);
                }
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
    ) -> Vec<crate::js::Script> {
        let base = summary.get("url").and_then(|u| u.as_str()).unwrap_or("");
        let mut budget = self.cfg.js_max_script_bytes;
        let mut out = Vec::with_capacity(scripts.len());

        for script in scripts {
            match script {
                ScriptSource::Inline { body, module } => out.push(crate::js::Script {
                    body: body.clone(),
                    module: *module,
                }),
                ScriptSource::External { src, module } => {
                    let joined = match url::Url::parse(base).and_then(|b| b.join(src)) {
                        Ok(u) => u,
                        Err(_) => {
                            out.push(crate::js::Script::classic(""));
                            continue;
                        }
                    };
                    if budget == 0 {
                        out.push(crate::js::Script::classic(""));
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
                            out.push(crate::js::Script {
                                body: f.body,
                                module: *module,
                            });
                        }
                        _ => out.push(crate::js::Script::classic("")),
                    }
                }
            }
        }
        out
    }

    /// Case-insensitive header lookup on a fetched response.
    fn header_of(f: &Fetched, name: &str) -> Option<String> {
        f.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
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

        // Conditional GET: if this session already holds a validator-tagged
        // copy of this URL, offer it. A 304 answer reuses the cached body.
        let cacheable = nav.method == "GET" && nav.body.is_none();
        let cached = if cacheable {
            session
                .fetch_cache
                .lock()
                .unwrap()
                .get(url.as_str())
                .map(|c| (c.etag.clone(), c.last_modified.clone()))
        } else {
            None
        };
        if let Some((etag, last_modified)) = &cached {
            if let Some(e) = etag {
                headers.push(("if-none-match".into(), e.clone()));
            }
            if let Some(lm) = last_modified {
                headers.push(("if-modified-since".into(), lm.clone()));
            }
        }

        let referer = session.referrer();

        let fut = guarded_fetch(
            &self.cfg,
            &self.client,
            FetchRequest {
                url: url.as_str(),
                method: &nav.method,
                headers,
                body: nav.body.clone(),
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

        let mut result = result;
        if result.status == 304 {
            let fresh_etag = Self::header_of(&result, "etag");
            let fresh_lm = Self::header_of(&result, "last-modified");
            if let Some(entry) = session.fetch_cache.lock().unwrap().get_mut(url.as_str()) {
                // Validators can rotate between responses; keep the newest.
                if fresh_etag.is_some() {
                    entry.etag = fresh_etag;
                }
                if fresh_lm.is_some() {
                    entry.last_modified = fresh_lm;
                }
                let mut again = entry.fetched.clone();
                again.from_cache = true;
                result = again;
            }
        } else if result.status == 200 && cacheable {
            let etag = Self::header_of(&result, "etag");
            let last_modified = Self::header_of(&result, "last-modified");
            if etag.is_some() || last_modified.is_some() {
                let mut cache = session.fetch_cache.lock().unwrap();
                if cache.len() >= 16 {
                    if let Some(oldest) = cache.keys().next().cloned() {
                        cache.remove(&oldest);
                    }
                }
                cache.insert(
                    url.to_string(),
                    CachedFetch { etag, last_modified, fetched: result.clone() },
                );
            }
        }

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
            match self.do_navigate(session, worker, nav, abort, 0).await {
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
        self.interact_locked(&session, worker, req, abort).await
    }

    /// The interaction pipeline without the session lock. Task steps already
    /// hold the lock for the whole task, so they call this directly; the HTTP
    /// path takes the lock in `interact`.
    async fn interact_locked(
        &self,
        session: &Session,
        worker: usize,
        req: InteractReq,
        abort: &Abort,
    ) -> Result<Value> {
        let req = &req;

        // Scroll and non-Enter key presses are delivered to the page's scripts:
        // they re-run first (so their listeners exist in the fresh context),
        // then the event is dispatched and the DOM is rebuilt from the result.
        let is_enter = req
            .action
            .key
            .as_deref()
            .map(|k| k.eq_ignore_ascii_case("enter"))
            .unwrap_or(false);
        let needs_event_pass =
            req.action.kind == "scroll" || (req.action.kind == "press" && !is_enter);
        if needs_event_pass {
            let kind = req.action.kind.clone();
            let key = req.action.key.clone();
            let selector = req.action.selector.clone();
            let scroll_px: u64 = req
                .action
                .values
                .first()
                .and_then(|v| v.parse().ok())
                .unwrap_or(800);
            let url = session.referrer().unwrap_or_default();
            let net_stats = std::sync::Arc::new(Mutex::new(js::NetStats::default()));
            let net = if self.cfg.js_network {
                Some(self.page_net(&session, &url, net_stats.clone()))
            } else {
                None
            };
            let detail = self
                .workers
                .call(worker, |reply| Cmd::FireEvent {
                    session: session.id.clone(),
                    kind,
                    key,
                    selector,
                    scroll_px,
                    net,
                    reply,
                })
                .await?;
            let mut out = json!({
                "ok": true,
                "type": req.action.kind,
                "navigated": false,
                "effect": "events",
                "detail": detail,
            });
            let stats = *net_stats.lock().unwrap();
            if stats.requests > 0 || stats.blocked > 0 {
                out["network"] = json!({
                    "requests": stats.requests,
                    "failed": stats.failed,
                    "blocked": stats.blocked,
                    "bytes": stats.bytes,
                });
            }
            return Ok(out);
        }

        // History walking is not a page interaction: it asks the session where
        // it would go, then navigates there like any other load.
        if matches!(req.action.kind.as_str(), "back" | "forward") {
            let forward = req.action.kind == "forward";
            let target = self
                .workers
                .call(worker, |reply| Cmd::History {
                    session: session.id.clone(),
                    forward,
                    reply,
                })
                .await?
                .ok_or_else(|| {
                    ScoutError::new(
                        ErrorCode::NotFound,
                        if forward {
                            "No page to go forward to"
                        } else {
                            "No page to go back to"
                        },
                    )
                })?;
            let nav = NavigateReq {
                url: target,
                session_id: Some(session.id.clone()),
                timeout_ms: req.action.timeout_ms,
                method: "GET".into(),
                body: None,
                content_type: None,
            };
            let summary = self.do_navigate(&session, worker, &nav, &abort, 0).await?;
            return Ok(json!({
                "ok": true,
                "type": req.action.kind,
                "navigated": true,
                "effect": "navigate",
                "navigation": summary,
            }));
        }

        // Waiting observes the page until a condition holds or the budget
        // runs out; it is a poll, not an interaction.
        if req.action.kind == "wait" {
            return self
                .wait_for(
                    &session,
                    worker,
                    req.action.selector.as_deref(),
                    req.action.values.first().map(|s| s.as_str()),
                    req.action.url.as_deref(),
                    req.action.timeout_ms.unwrap_or(5_000),
                    &abort,
                )
                .await;
        }

        let effect: Effect = match self
            .workers
            .call(worker, |reply| Cmd::Interact {
                session: session.id.clone(),
                action: req.action.clone(),
                reply,
            })
            .await
        {
            Ok(e) => e,
            // A click on an element with no semantic behaviour (plain button,
            // javascript: link, div with a handler) is delivered to the page's
            // own listeners, exactly as a browser delivers it, and the result
            // is whatever the handler did.
            Err(e) if req.action.kind == "click" && e.code == ErrorCode::UnsupportedCapability => {
                let url = session.referrer().unwrap_or_default();
                let net_stats = std::sync::Arc::new(Mutex::new(js::NetStats::default()));
                let net = if self.cfg.js_network {
                    Some(self.page_net(&session, &url, net_stats.clone()))
                } else {
                    None
                };
                let detail = self
                    .workers
                    .call(worker, |reply| Cmd::FireEvent {
                        session: session.id.clone(),
                        kind: "click".into(),
                        key: None,
                        selector: req.action.selector.clone(),
                        scroll_px: 0,
                        net,
                        reply,
                    })
                    .await?;
                // The handler may have navigated.
                let pending = detail
                    .get("pendingNavigation")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                if let Some(target) = pending {
                    let nav = NavigateReq {
                        url: target,
                        session_id: Some(session.id.clone()),
                        timeout_ms: req.action.timeout_ms,
                        method: "GET".into(),
                        body: None,
                        content_type: None,
                    };
                    let summary = self.do_navigate(&session, worker, &nav, &abort, 0).await?;
                    return Ok(json!({
                        "ok": true,
                        "type": "click",
                        "navigated": true,
                        "effect": "navigate",
                        "navigation": summary,
                    }));
                }
                return Ok(json!({
                    "ok": true,
                    "type": "click",
                    "navigated": false,
                    "effect": "events",
                    "detail": detail,
                }));
            }
            Err(e) => return Err(e),
        };

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

    /// Poll a session until a selector appears, a text shows up, or the URL
    /// contains a fragment. All given conditions must hold at the same poll.
    /// The budget is clamped to the same 30s ceiling interactions use.
    async fn wait_for(
        &self,
        session: &Session,
        worker: usize,
        selector: Option<&str>,
        text: Option<&str>,
        url: Option<&str>,
        timeout_ms: u64,
        abort: &Abort,
    ) -> Result<Value> {
        if selector.is_none() && text.is_none() && url.is_none() {
            return Err(ScoutError::invalid(
                "wait needs one of \"selector\", \"value\" (text) or \"url\"",
            ));
        }
        let budget = timeout_ms.clamp(100, 30_000);
        let started = std::time::Instant::now();
        let deadline = started + Duration::from_millis(budget);
        // One script answers every condition: booleans ANDed together.
        let mut script = String::from("return ");
        let mut parts: Vec<String> = Vec::new();
        if let Some(sel) = selector {
            parts.push(format!(
                "(document.querySelector({}) !== null)",
                crate::tools::js_lit(sel)
            ));
        }
        if let Some(txt) = text {
            parts.push(format!(
                "((document.body ? document.body.textContent : '').indexOf({}) !== -1)",
                crate::tools::js_lit(txt)
            ));
        }
        if let Some(frag) = url {
            parts.push(format!(
                "(location.href.indexOf({}) !== -1)",
                crate::tools::js_lit(frag)
            ));
        }
        script.push_str(&parts.join(" && "));
        script.push(';');

        loop {
            let out: Value = self
                .workers
                .call(worker, |reply| Cmd::Eval {
                    session: session.id.clone(),
                    script: script.clone(),
                    net: None,
                    reply,
                })
                .await?;
            let matched = out.get("value").and_then(|v| v.as_bool()).unwrap_or(false);
            if matched {
                return Ok(json!({
                    "ok": true,
                    "type": "wait",
                    "navigated": false,
                    "effect": "none",
                    "waitedMs": started.elapsed().as_millis() as u64,
                }));
            }
            let now = std::time::Instant::now();
            if now >= deadline {
                return Err(ScoutError::new(
                    ErrorCode::WaitTimeout,
                    format!(
                        "waited {}ms without the condition holding",
                        started.elapsed().as_millis()
                    ),
                )
                .with_details(json!({
                    "waitedMs": started.elapsed().as_millis() as u64,
                    "selector": selector,
                    "text": text,
                    "url": url,
                })));
            }
            tokio::select! {
                biased;
                _ = abort.token().cancelled() => return Err(abort.reason()),
                _ = tokio::time::sleep_until(tokio::time::Instant::from_std(
                    (now + Duration::from_millis(150)).min(deadline)
                )) => {}
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

    /// Look up `name` or `name.path.to.value` (array indices allowed) in the
    /// step variables.
    fn var_lookup(vars: &std::collections::HashMap<String, Value>, path: &str) -> Option<Value> {
        let mut parts = path.split('.');
        let first = parts.next()?;
        let mut cur = vars.get(first)?;
        for p in parts {
            cur = match p.parse::<usize>() {
                Ok(i) => cur.get(i)?,
                Err(_) => cur.get(p)?,
            };
        }
        Some(cur.clone())
    }

    /// Replace `{{name}}` / `{{name.path}}` in every string of a step's
    /// parameters. A string that is exactly one reference takes the value's
    /// type; a string with embedded references gets them stringified in place.
    fn interpolate_vars(v: &Value, vars: &std::collections::HashMap<String, Value>) -> Value {
        match v {
            Value::String(s) => {
                if let Some(name) = s
                    .strip_prefix("{{")
                    .and_then(|x| x.strip_suffix("}}"))
                    .map(str::trim)
                    .filter(|n| !n.is_empty() && !n.contains("{{"))
                {
                    if let Some(val) = Self::var_lookup(vars, name) {
                        return val;
                    }
                }
                if !s.contains("{{") {
                    return v.clone();
                }
                let mut out = String::new();
                let mut rest = s.as_str();
                while let Some(start) = rest.find("{{") {
                    out.push_str(&rest[..start]);
                    let after = &rest[start + 2..];
                    match after.find("}}") {
                        Some(end) => {
                            let name = after[..end].trim();
                            match Self::var_lookup(vars, name) {
                                Some(Value::String(t)) => out.push_str(&t),
                                Some(other) => out.push_str(&other.to_string()),
                                None => out.push_str(&rest[start..start + 2 + end + 2]),
                            }
                            rest = &after[end + 2..];
                        }
                        None => {
                            out.push_str(rest);
                            rest = "";
                        }
                    }
                }
                out.push_str(rest);
                Value::String(out)
            }
            Value::Object(m) => Value::Object(
                m.iter()
                    .map(|(k, v)| (k.clone(), Self::interpolate_vars(v, vars)))
                    .collect(),
            ),
            Value::Array(a) => Value::Array(a.iter().map(|v| Self::interpolate_vars(v, vars)).collect()),
            other => other.clone(),
        }
    }

    pub fn submit_task(self: &Arc<Self>, owner: &str, req: TaskReq) -> Result<Value> {
        // Validate sessions up front so an unknown session is a 404 at submit
        // time rather than a task that fails asynchronously.
        if let Some(sid) = &req.session_id {
            self.sessions.get(owner, sid)?;
        }

        // Recurring: a schedule, not a task. The first run starts now.
        if let Some(every_ms) = req.every_ms {
            let view = self.schedules.create(
                owner,
                req.to_spec(),
                every_ms,
                req.callback_url.clone(),
                req.callback_secret.clone(),
            )?;
            // First run immediately, in the background.
            let this = self.clone();
            let spec = req.to_spec();
            let cb_url = req.callback_url.clone();
            let cb_secret = req.callback_secret.clone();
            let id = view["id"].as_str().unwrap_or_default().to_string();
            let owner = owner.to_string();
            if !id.is_empty() {
                tokio::spawn(async move {
                    this.run_schedule(&owner, &id, &spec, cb_url, cb_secret).await;
                });
            }
            self.metrics.inc(
                "scout_schedules_created_total",
                "Recurring schedules created",
                &[],
            );
            return Ok(view);
        }

        let retries = req.retries.unwrap_or(self.cfg.max_retries);
        let timeout = clamp_timeout(req.timeout_ms, self.cfg.task_timeout_ms, self.cfg.max_task_timeout_ms);
        let step_count = req.steps.len();

        // A one-shot task with a callbackUrl hears about its terminal state,
        // wherever that state came from: completion, failure, cancellation.
        let hook: Option<Box<dyn FnOnce(Value) + Send>> = req.callback_url.as_ref().map(|url| {
            let this = self.clone();
            let url = url.clone();
            let secret = req.callback_secret.clone();
            Box::new(move |view: Value| {
                let payload = json!({ "type": "task", "task": view });
                tokio::spawn(async move {
                    crate::schedules::fire_callback(&this.client, &url, secret.as_deref(), &payload).await;
                });
            }) as Box<dyn FnOnce(Value) + Send>
        });

        let this = self.clone();
        let owner_owned = owner.to_string();
        let view = self.tasks.submit_with_hook(owner.to_string(), Some(timeout), hook, move |abort| async move {
            this.run_task(&owner_owned, req, retries, abort).await
        })?;
        self.metrics.inc(
            "scout_tasks_submitted_total",
            "Tasks accepted by the queue",
            &[("steps", &step_count.to_string())],
        );
        Ok(view)
    }

    /// One run of a schedule: validate the spec fresh, run it, hash the
    /// result, and fire the callback only when the result changed.
    pub async fn run_schedule(
        self: &Arc<Self>,
        owner: &str,
        id: &str,
        spec: &Value,
        callback_url: Option<String>,
        callback_secret: Option<String>,
    ) {
        let req = match crate::validate::task(spec, &self.cfg) {
            Ok(r) => r,
            Err(e) => {
                // A spec that no longer validates (a strategy was removed, a
                // session was destroyed) is recorded, not silently dropped.
                self.schedules.record(id, "failed", "-", None);
                crate::logger::warn(
                    "schedule run rejected its own spec",
                    json!({ "schedule": id, "error": e.message }),
                );
                return;
            }
        };
        if let Some(sid) = &req.session_id {
            if self.sessions.get(owner, sid).is_err() {
                self.schedules.record(id, "failed", "-", None);
                return;
            }
        }
        let retries = req.retries.unwrap_or(self.cfg.max_retries);
        let timeout = clamp_timeout(req.timeout_ms, self.cfg.task_timeout_ms, self.cfg.max_task_timeout_ms);
        let run_id = crate::util::random_id("run_", 12);
        let this = self.clone();
        let owner = owner.to_string();
        let out = self
            .tasks
            .run_inline(None, Some(timeout), move |abort| async move {
                this.run_task(&owner, req, retries, abort).await
            })
            .await;
        match out {
            Ok(v) => {
                let hash = crate::schedules::fnv1a64(v.to_string().as_bytes());
                let changed = self.schedules.record(id, "succeeded", &run_id, Some(hash));
                if changed == Some(true) {
                    if let Some(url) = &callback_url {
                        let payload = json!({
                            "type": "schedule.run",
                            "scheduleId": id,
                            "runId": run_id,
                            "changed": true,
                            "result": v,
                        });
                        crate::schedules::fire_callback(&self.client, url, callback_secret.as_deref(), &payload).await;
                    }
                }
            }
            Err(e) => {
                self.schedules.record(id, "failed", &run_id, None);
                let _ = e;
            }
        }
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

            // Named step results, for `{{name}}` interpolation in later steps.
            let mut vars: std::collections::HashMap<String, Value> = std::collections::HashMap::new();
            for (index, step) in req.steps.iter().enumerate() {
                if abort.is_cancelled() {
                    return Err(abort.reason());
                }
                let started = std::time::Instant::now();
                // Interpolate `{{name}}` and `{{name.path}}` references into a
                // local copy of the step's parameters. Unknown names stay as
                // written; the step's own validation then reports the problem.
                let step_params = Self::interpolate_vars(&step.params, &vars);
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
                            url: step_params
                                .get("url")
                                .and_then(|u| u.as_str())
                                .unwrap_or_default()
                                .to_string(),
                            session_id: Some(session.id.clone()),
                            timeout_ms: step_params.get("timeoutMs").and_then(|t| t.as_u64()),
                            method: "GET".into(),
                            body: None,
                            content_type: None,
                        };
                        self.navigate_with_retry(&session, worker, &nav, &abort, retries).await
                    }
                    "extract" => {
                        let mut er = {
                            let mut probe = step_params.clone();
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
                        // Goes through the same path as POST /v1/interact, so
                        // wait, back, forward and click-with-listeners work in
                        // tasks exactly as they do over HTTP.
                        let req = InteractReq {
                            session_id: session.id.clone(),
                            action: {
                                let mut probe = step_params.clone();
                                if let Some(o) = probe.as_object_mut() {
                                    o.insert("sessionId".into(), json!(session.id));
                                }
                                crate::validate::interact(&probe, &self.cfg)?.action
                            },
                        };
                        let out = self.interact_locked(&session, worker, req, &abort).await?;
                        Ok(json!({
                            "effect": out.get("effect").cloned().unwrap_or(Value::Null),
                            "navigation": out.get("navigation").cloned().unwrap_or(Value::Null),
                            "detail": out.get("detail").cloned().unwrap_or(Value::Null),
                        }))
                    }
                        other => Err(ScoutError::unsupported(format!(
                            "Step op \"{other}\" is not supported"
                        ))),
                    }
                }
                .await;

                match outcome {
                    Ok(v) => {
                        if let Some(name) = &step.as_name {
                            vars.insert(name.clone(), v.clone());
                        }
                        let mut entry = json!({
                            "index": index,
                            "op": step.op,
                            "ok": true,
                            "durationMs": started.elapsed().as_millis() as u64,
                            "result": v,
                        });
                        if let Some(name) = &step.as_name {
                            entry["as"] = json!(name);
                        }
                        steps.push(entry);
                    }
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

#[cfg(test)]
mod step_var_tests {
    use super::Scout;
    use serde_json::json;
    use std::collections::HashMap;

    fn vars() -> HashMap<String, serde_json::Value> {
        let mut m = HashMap::new();
        m.insert("page".to_string(), json!({ "url": "https://example.com/a", "status": 200 }));
        m.insert("rows".to_string(), json!(["one", "two"]));
        m.insert("count".to_string(), json!(7));
        m
    }

    #[test]
    fn whole_reference_keeps_the_type() {
        let out = Scout::interpolate_vars(&json!("{{page.url}}"), &vars());
        assert_eq!(out, json!("https://example.com/a"));
        let out = Scout::interpolate_vars(&json!("{{count}}"), &vars());
        assert_eq!(out, json!(7));
    }

    #[test]
    fn embedded_references_are_stringified() {
        let out = Scout::interpolate_vars(&json!("got {{count}} rows"), &vars());
        assert_eq!(out, json!("got 7 rows"));
    }

    #[test]
    fn array_paths_and_unknown_names() {
        let out = Scout::interpolate_vars(&json!("{{rows.1}}"), &vars());
        assert_eq!(out, json!("two"));
        // Unknown names stay exactly as written.
        let out = Scout::interpolate_vars(&json!("{{missing.name}}"), &vars());
        assert_eq!(out, json!("{{missing.name}}"));
        // A ref that resolves partway stays too.
        let out = Scout::interpolate_vars(&json!("x {{page.nope}} y"), &vars());
        assert_eq!(out, json!("x {{page.nope}} y"));
    }

    #[test]
    fn interpolation_reaches_nested_params() {
        let step = json!({ "url": "{{page.url}}?page={{count}}", "opts": { "tag": "{{rows.0}}" } });
        let out = Scout::interpolate_vars(&step, &vars());
        assert_eq!(out["url"], json!("https://example.com/a?page=7"));
        assert_eq!(out["opts"]["tag"], json!("one"));
    }
}
