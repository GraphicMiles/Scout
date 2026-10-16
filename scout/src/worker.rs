//! The page worker: a dedicated thread that owns every parsed DOM and every
//! QuickJS context.
//!
//! ## Why a thread, not shared state
//!
//! `scraper::Html` is neither `Send` nor `Sync` — tendril uses non-atomic
//! reference counts (`Cell<usize>`), so a parsed document cannot cross threads.
//! QuickJS has the same character: its GC arenas are tied to the thread that
//! created them. Neither can be handed to an ordinary async task.
//!
//! So all page state lives on one thread per worker. Commands cross the boundary
//! as plain data (`String`, `Value`, enums) and replies come back as JSON. That
//! keeps the async side entirely free of non-`Send` types while still parsing
//! each document exactly once.
//!
//! The number of workers equals `SCOUT_CONCURRENCY`, and a session is pinned to
//! one worker for its lifetime, so its DOM never migrates.

use crate::config::Config;
use crate::error::{ErrorCode, Result, ScoutError};
use crate::extract::{self, Schema};
use crate::html;
use crate::interaction::{self, Action, Effect};
use crate::js;
use crate::page::Page;
use crate::security::Fetched;
use crate::selector;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::oneshot;

/// Which extraction strategy to run. Resolved to code inside the worker so the
/// async side never touches a DOM.
#[derive(Debug, Clone)]
pub enum StrategySpec {
    GenericPage,
    Selectors(Schema),
    YoutubeChannelVideos,
}

/// A script the page asked for, in document order.
pub enum ScriptSource {
    /// Body is already in the document.
    Inline(String),
    /// Needs fetching against the page URL before it can run.
    External(String),
}

pub struct LoadOutcome {
    /// The navigate response, unchanged.
    pub summary: Value,
    /// The page's scripts, in document order, capped by `js_max_scripts`.
    pub scripts: Vec<ScriptSource>,
}

pub enum Cmd {
    Load {
        session: String,
        fetched: Box<Fetched>,
        reply: oneshot::Sender<Result<LoadOutcome>>,
    },
    /// Run the page's scripts and rebuild the DOM from what they produced.
    RunScripts {
        session: String,
        /// Bodies in document order; an empty string means it could not be
        /// fetched, which is recorded rather than fatal.
        scripts: Vec<String>,
        /// The session's network, if in-page requests are allowed. Built on the
        /// async side, where the client, the jar and the SSRF policy live, and
        /// handed to the worker so scripts never touch them directly.
        net: Option<js::NetFn>,
        reply: oneshot::Sender<Result<Value>>,
    },
    /// Re-run the page's own scripts, then dispatch an event into the page:
    /// what a scroll action or a key press does. Listeners die with the
    /// context that registered them, so the scripts run again first.
    FireEvent {
        session: String,
        /// "scroll" or "press".
        kind: String,
        key: Option<String>,
        selector: Option<String>,
        scroll_px: u64,
        net: Option<js::NetFn>,
        reply: oneshot::Sender<Result<Value>>,
    },
    Extract {
        session: String,
        spec: StrategySpec,
        limit: usize,
        reply: oneshot::Sender<Result<Value>>,
    },
    Interact {
        session: String,
        action: Action,
        reply: oneshot::Sender<Result<Effect>>,
    },
    Eval {
        session: String,
        script: String,
        net: Option<js::NetFn>,
        reply: oneshot::Sender<Result<Value>>,
    },
    Drop {
        session: String,
    },
    Stats {
        reply: oneshot::Sender<Result<Value>>,
    },
}

struct WorkerState {
    pages: HashMap<String, Page>,
    cfg: Arc<Config>,
}

impl WorkerState {
    fn new(cfg: Arc<Config>) -> Self {
        Self {
            pages: HashMap::new(),
            cfg,
        }
    }

    fn handle(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Load { session, fetched, reply } => {
                let _ = reply.send(self.load(&session, *fetched));
            }
            Cmd::RunScripts { session, scripts, net, reply } => {
                let _ = reply.send(self.run_scripts(&session, scripts, net, true));
            }
            Cmd::FireEvent { session, kind, key, selector, scroll_px, net, reply } => {
                let _ = reply.send(self.fire_event(&session, &kind, key, selector, scroll_px, net));
            }
            Cmd::Extract { session, spec, limit, reply } => {
                let _ = reply.send(self.extract(&session, &spec, limit));
            }
            Cmd::Interact { session, action, reply } => {
                let _ = reply.send(self.interact(&session, action));
            }
            Cmd::Eval { session, script, net, reply } => {
                let _ = reply.send(self.eval(&session, &script, net));
            }
            Cmd::Drop { session } => {
                // Dropping the Page releases the DOM tree and any typed form values.
                self.pages.remove(&session);
            }
            Cmd::Stats { reply } => {
                let bytes: usize = self.pages.values().map(|p| p.bytes).sum();
                let _ = reply.send(Ok(json!({
                    "pages": self.pages.len(),
                    "bytes": bytes,
                })));
            }
        }
    }

    fn load(&mut self, session: &str, fetched: Fetched) -> Result<LoadOutcome> {
        if fetched.status >= 400 {
            let restricted = matches!(fetched.status, 401 | 403 | 429);
            let msg = if restricted {
                format!(
                    "Server responded with HTTP {}; access is restricted and Scout does not bypass authentication or anti-bot controls",
                    fetched.status
                )
            } else {
                format!("Server responded with HTTP {}", fetched.status)
            };
            return Err(ScoutError::new(ErrorCode::NavigationFailed, msg)
                .with_retryable(matches!(fetched.status, 408 | 500 | 502 | 503 | 504))
                .with_details(json!({ "status": fetched.status, "url": fetched.url })));
        }

        let page = if fetched.content_type.contains("json") {
            let parsed: Value = serde_json::from_str(&fetched.body).map_err(|_| {
                ScoutError::new(
                    ErrorCode::UnsupportedContent,
                    "Response declared JSON but could not be parsed",
                )
            })?;
            Page::json_page(
                fetched.url.clone(),
                fetched.status,
                fetched.content_type.clone(),
                fetched.bytes,
                parsed,
                Vec::new(),
                fetched.redirects.clone(),
            )
        } else {
            let doc = html::parse_html(&fetched.body, self.cfg.max_dom_nodes)?;
            let warnings = js_warnings(&doc);
            Page::html(
                fetched.url.clone(),
                fetched.status,
                fetched.content_type.clone(),
                fetched.bytes,
                doc,
                warnings,
                fetched.redirects.clone(),
            )
        };

        let scripts = if self.cfg.js_enabled {
            collect_scripts(page.doc.as_ref(), self.cfg.js_max_scripts)
        } else {
            Vec::new()
        };

        let summary = json!({
            "url": page.url,
            "status": page.status,
            "contentType": page.content_type,
            "bytes": page.bytes,
            "title": page.title(),
            "redirects": page.redirects,
            "warnings": page.warnings,
        });
        self.pages.insert(session.to_string(), page);
        Ok(LoadOutcome { summary, scripts })
    }

    fn page(&self, session: &str) -> Result<&Page> {
        self.pages
            .get(session)
            .ok_or_else(|| ScoutError::new(ErrorCode::Conflict, "Session has no loaded page; provide \"url\" or navigate first"))
    }

    fn extract(&self, session: &str, spec: &StrategySpec, limit: usize) -> Result<Value> {
        let page = self.page(session)?;
        let (data, warnings) = match spec {
            StrategySpec::GenericPage => extract::generic_page(page, limit),
            StrategySpec::Selectors(schema) => extract::extract_by_schema(page, schema, limit),
            StrategySpec::YoutubeChannelVideos => {
                crate::adapters::youtube::extract(page, limit, &self.cfg.youtube_hosts)?
            }
        };
        // Page-level warnings travel with the extraction too. A caller that
        // only ever calls /v1/extract must still be told when the page is a
        // JS shell that rendered nothing or a sign-in wall, instead of
        // receiving an empty result with no explanation.
        let mut all = page.warnings.clone();
        for w in warnings {
            if !all.contains(&w) {
                all.push(w);
            }
        }

        // The page identity travels with the result so the caller never has to
        // ask a second time (and never has to guess).
        Ok(json!({
            "data": data,
            "warnings": all,
            "page": { "url": page.url, "status": page.status },
        }))
    }

    fn interact(&mut self, session: &str, action: Action) -> Result<Effect> {
        // Interaction records typed values in the page's own form state, so it
        // needs a mutable borrow; the parsed tree itself is never rewritten.
        let cfg = self.cfg.clone();
        let page = self
            .pages
            .get_mut(session)
            .ok_or_else(|| ScoutError::new(ErrorCode::Conflict, "Session has no loaded page; provide \"url\" or navigate first"))?;
        interaction::apply(page, &action, &cfg)
    }

    /// Execute the page's scripts and rebuild the DOM from the result.
    ///
    /// The document is re-parsed from the serialised, mutated tree, which is
    /// what makes script-built content visible to extraction. A page whose
    /// scripts all fail keeps its original DOM rather than being blanked.
    fn run_scripts(
        &mut self,
        session: &str,
        scripts: Vec<String>,
        net: Option<js::NetFn>,
        save: bool,
    ) -> Result<Value> {
        let page = self
            .pages
            .get_mut(session)
            .ok_or_else(|| ScoutError::new(ErrorCode::NotFound, "Unknown session"))?;
        let Some(doc) = page.doc.as_ref() else {
            return Err(ScoutError::new(
                ErrorCode::CompatibilityError,
                "JavaScript execution needs an HTML page",
            ));
        };

        let snapshot = js::dom_snapshot(doc);
        let out = js::run_page(
            &scripts,
            &snapshot,
            &page.url,
            &self.cfg.user_agent,
            net,
            Duration::from_millis(self.cfg.js_timeout_ms),
            self.cfg.js_max_memory_bytes as usize,
            self.cfg.js_timer_budget,
            self.cfg.js_job_budget,
        )?;

        if let Some(html) = out.html {
            if let Ok(new_doc) = html::parse_html(&html, self.cfg.max_dom_nodes) {
                let warnings = js_warnings(&new_doc);
                page.doc = Some(new_doc);
                page.warnings = warnings;
            }
        }
        // Keep the page's own scripts for later event passes (scroll, key
        // press): those re-run them so their listeners exist again. Only the
        // navigation pass saves; an event pass must not append its dispatch
        // script to what gets re-run next time.
        if save {
            page.scripts = scripts;
        }

        let mut summary = json!({
            "scriptsRun": out.scripts_run,
            "scriptsFailed": out.scripts_failed,
            "timersRun": out.timers_run,
            "timersPending": out.timers_pending,
            "jobsRun": out.jobs_run,
            "durationMs": out.duration_ms,
            "timedOut": out.timed_out,
        });
        if !out.errors.is_empty() {
            summary["errors"] = json!(out.errors);
        }
        if !out.logs.is_empty() {
            summary["logs"] = json!(out.logs);
        }
        Ok(summary)
    }

    /// Re-run the page's scripts, then dispatch an event into the page.
    ///
    /// This is what makes `scroll` and `press` real: the page's scripts run
    /// again (registering their listeners in the fresh context), the event is
    /// dispatched, and pending jobs and timers drain, so a lazy-loading scroll
    /// handler actually fetches and appends before the DOM is rebuilt.
    ///
    /// Honest limits, reported rather than hidden: re-running a page's initial
    /// scripts against an already-mutated DOM can duplicate content they append
    /// unconditionally; scripts that overwrite (innerHTML = ...) are unaffected.
    fn fire_event(
        &mut self,
        session: &str,
        kind: &str,
        key: Option<String>,
        selector: Option<String>,
        scroll_px: u64,
        net: Option<js::NetFn>,
    ) -> Result<Value> {
        let (mut scripts, has_doc) = {
            let page = self
                .pages
                .get(session)
                .ok_or_else(|| ScoutError::new(ErrorCode::NotFound, "Unknown session"))?;
            (page.scripts.clone(), page.doc.is_some())
        };
        if !has_doc {
            return Err(ScoutError::new(
                ErrorCode::CompatibilityError,
                "Event dispatch needs an HTML page",
            ));
        }

        let extra = match kind {
            "scroll" => format!(
                "window.scrollTo(0, {px});\
                 try {{ document.dispatchEvent(new Event('scroll')); }} catch (e) {{}}",
                px = scroll_px
            ),
            "press" => {
                let Some(press_key) = key.clone().filter(|k| !k.is_empty()) else {
                    return Err(ScoutError::invalid("press needs \"key\""));
                };
                let sel = selector
                    .as_deref()
                    .map(|s| serde_json::to_string(s).unwrap_or_else(|_| "null".into()))
                    .unwrap_or_else(|| "null".into());
                format!(
                    "(function () {{
                       var el = {sel} ? document.querySelector({sel}) : (document.activeElement || document.body);
                       if (!el) return;
                       var key = {key};
                       var named = {{ 'enter': 'Enter', 'tab': 'Tab', 'escape': 'Escape', 'backspace': 'Backspace',
                                      'delete': 'Delete', 'space': 'Space', 'arrowup': 'ArrowUp', 'arrowdown': 'ArrowDown',
                                      'arrowleft': 'ArrowLeft', 'arrowright': 'ArrowRight', 'home': 'Home', 'end': 'End',
                                      'pageup': 'PageUp', 'pagedown': 'PageDown' }};
                       var code = key.length === 1
                         ? (/[a-z]/i.test(key) ? 'Key' + key.toUpperCase() : (/^[0-9]$/.test(key) ? 'Digit' + key : key))
                         : (named[key.toLowerCase()] || key);
                       var keyCode = key === 'Enter' ? 13 : (key.length === 1 ? (key.charCodeAt(0) || 0) : 0);
                       ['keydown', 'keypress', 'keyup'].forEach(function (t) {{
                         try {{
                           el.dispatchEvent(new KeyboardEvent(t, {{
                             key: key, code: code, bubbles: true, cancelable: true, keyCode: keyCode
                           }}));
                         }} catch (e) {{}}
                       }});
                     }})();",
                    sel = sel,
                    key = serde_json::to_string(&press_key).unwrap_or_else(|_| "\"\"".into()),
                )
            }
            other => {
                return Err(ScoutError::invalid(format!(
                    "Event kind \"{other}\" is not dispatchable"
                )))
            }
        };

        scripts.push(extra);
        let mut summary = self.run_scripts(session, scripts, net, false)?;
        if let Some(obj) = summary.as_object_mut() {
            obj.insert("event".into(), json!({ "kind": kind, "key": key, "scrollPx": scroll_px }));
        }
        Ok(summary)
    }

    fn eval(&self, session: &str, script: &str, net: Option<js::NetFn>) -> Result<Value> {
        let page = self.page(session)?;
        let Some(doc) = page.doc.as_ref() else {
            return Err(ScoutError::new(
                ErrorCode::CompatibilityError,
                "JavaScript execution needs an HTML page",
            ));
        };
        let snapshot = js::dom_snapshot(doc);
        let out = js::run(
            script,
            &snapshot,
            &page.url,
            &self.cfg.user_agent,
            net,
            Duration::from_millis(self.cfg.js_timeout_ms),
            self.cfg.js_max_memory_bytes as usize,
        )?;
        // `warnings` is additive: callers that only read `value` are unaffected.
        Ok(json!({
            "value": out.value,
            "logs": out.logs,
            "durationMs": out.duration_ms,
            "timedOut": out.timed_out,
            "warnings": out.warnings,
        }))
    }
}

/// Collect a page's scripts in document order.
///
/// Non-JavaScript `<script>` blocks (templates, JSON payloads, import maps)
/// are skipped: executing them would either throw or, worse, parse successfully
/// and do nothing useful.
fn collect_scripts(doc: Option<&scraper::Html>, limit: usize) -> Vec<ScriptSource> {
    let mut out = Vec::new();
    let Some(doc) = doc else { return out };
    let Ok(sel) = selector::compile("script") else { return out };

    for el in doc.select(&sel) {
        if out.len() >= limit {
            break;
        }
        let node = el.value();
        let kind = node.attr("type").unwrap_or("").trim().to_ascii_lowercase();
        if !kind.is_empty()
            && kind != "module"
            && !kind.contains("javascript")
            && !kind.contains("ecmascript")
            && !kind.contains("text/jsx")
        {
            continue;
        }
        match node.attr("src").map(|s| s.trim()).filter(|s| !s.is_empty()) {
            Some(src) => out.push(ScriptSource::External(src.to_string())),
            None => out.push(ScriptSource::Inline(el.text().collect::<String>())),
        }
    }
    out
}

/// Heuristic: does this page look like it renders its content with JavaScript?
///
/// Reports a possibility, never a result. A page that trips this is *not*
/// silently treated as empty; the flag travels with the extraction output.
fn js_warnings(doc: &scraper::Html) -> Vec<Value> {
    let body = selector::compile("body")
        .ok()
        .and_then(|s| selector::select_one(doc, &s));
    let visible = match &body {
        Some(b) => html::text(&**b).len(),
        None => html::text(&doc.tree.root()).len(),
    };
    let scripts = selector::compile("script:not([type*=\"json\"])")
        .ok()
        .map(|s| doc.select(&s).count())
        .unwrap_or(0);
    let noscript = selector::compile("noscript")
        .ok()
        .and_then(|s| selector::select_one(doc, &s));
    let noscript_needs_js = noscript
        .as_ref()
        .map(|n| {
            let t = html::text(&**n);
            t.to_ascii_lowercase().contains("javascript") && visible < 500
        })
        .unwrap_or(false);

    let links = selector::compile("a[href]")
        .ok()
        .map(|s| doc.select(&s).count())
        .unwrap_or(0);
    let password_fields = selector::compile("input[type=password]")
        .ok()
        .map(|s| doc.select(&s).count())
        .unwrap_or(0);

    let mut out = Vec::new();

    // A script-heavy page that rendered nothing is a shell: the content
    // arrives over fetch/XHR after load, which Scout does not execute. An
    // empty extraction that looks like "a page with nothing on it" is the one
    // result a caller cannot safely act on, so it is named.
    if scripts >= 5 && visible < 1_000 && links == 0 {
        out.push(json!({
            "code": "js_shell_not_rendered",
            "message": "The page ran its scripts but rendered no links or content. It almost                         certainly builds its DOM from fetch/XHR after load, which Scout does not                         execute: there is no in-page fetch and no event loop. Any extraction from                         this page is empty, not authoritative."
        }));
    } else if (scripts > 0 && visible < 200) || noscript_needs_js {
        out.push(json!({
            "code": "js_required_likely",
            "message": "Page appears to render its content with JavaScript. Scout runs page scripts against the parsed document and rebuilds it, but there is no event loop, no in-page fetch and no layout, so some content may still be missing.",
        }));
    }

    // A thin page whose only form is a credential form is a sign-in wall, not
    // a broken extraction. Saying so is both more useful and more honest, and
    // Scout does not attempt to get past it.
    if password_fields > 0 && visible < 3_000 && links < 60 {
        out.push(json!({
            "code": "auth_wall_likely",
            "message": "This page appears to be a sign-in wall. Scout does not bypass                         authentication, access controls or anti-bot protections; supply an                         authenticated session or target a page that does not require one."
        }));
    }

    out
}

/// A pool of worker threads; sessions are pinned to one by index.
pub struct WorkerPool {
    senders: Vec<mpsc::Sender<Cmd>>,
    handles: Vec<std::thread::JoinHandle<()>>,
    next: std::sync::atomic::AtomicUsize,
}

impl WorkerPool {
    pub fn new(cfg: Arc<Config>) -> Self {
        let n = cfg.concurrency.max(1);
        let mut senders = Vec::with_capacity(n);
        let mut handles = Vec::with_capacity(n);
        for i in 0..n {
            let (tx, rx) = mpsc::channel::<Cmd>();
            let cfg = cfg.clone();
            let handle = std::thread::Builder::new()
                .name(format!("scout-page-{i}"))
                .stack_size(4 * 1024 * 1024)
                .spawn(move || {
                    let mut state = WorkerState::new(cfg);
                    // Blocking receive: this thread is dedicated to CPU-bound
                    // DOM and JS work and must not run on the async executor.
                    while let Ok(cmd) = rx.recv() {
                        state.handle(cmd);
                    }
                })
                .expect("page worker thread");
            senders.push(tx);
            handles.push(handle);
        }
        Self {
            senders,
            handles,
            next: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    pub fn worker_count(&self) -> usize {
        self.senders.len()
    }

    /// Pick a worker for a new session, spreading sessions across the pool.
    pub fn assign(&self) -> usize {
        self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % self.senders.len()
    }

    pub fn send(&self, idx: usize, cmd: Cmd) -> Result<()> {
        self.senders
            .get(idx)
            .ok_or_else(|| ScoutError::new(ErrorCode::Internal, "No page worker available"))?
            .send(cmd)
            .map_err(|_| ScoutError::new(ErrorCode::ShuttingDown, "Page worker has stopped"))
    }

    /// Round-trip a command that yields a value.
    pub async fn call<T, F>(&self, idx: usize, build: F) -> Result<T>
    where
        F: FnOnce(oneshot::Sender<Result<T>>) -> Cmd,
        T: Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        self.send(idx, build(tx))?;
        rx.await
            .map_err(|_| ScoutError::new(ErrorCode::ShuttingDown, "Page worker dropped the request"))?
    }

    pub fn shutdown(&self) {
        // Dropping the senders ends each worker's receive loop.
        for _ in 0..self.senders.len() {
            // Channels close when all senders drop; nothing further to send.
        }
    }

    pub fn join(self) {
        for h in self.handles {
            let _ = h.join();
        }
    }
}
