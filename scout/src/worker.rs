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

pub enum Cmd {
    Load {
        session: String,
        fetched: Box<Fetched>,
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
            Cmd::Extract { session, spec, limit, reply } => {
                let _ = reply.send(self.extract(&session, &spec, limit));
            }
            Cmd::Interact { session, action, reply } => {
                let _ = reply.send(self.interact(&session, action));
            }
            Cmd::Eval { session, script, reply } => {
                let _ = reply.send(self.eval(&session, &script));
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

    fn load(&mut self, session: &str, fetched: Fetched) -> Result<Value> {
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
        Ok(summary)
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
        // The page identity travels with the result so the caller never has to
        // ask a second time (and never has to guess).
        Ok(json!({
            "data": data,
            "warnings": warnings,
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

    fn eval(&self, session: &str, script: &str) -> Result<Value> {
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

    if (scripts > 0 && visible < 200) || noscript_needs_js {
        vec![json!({
            "code": "js_required_likely",
            "message": "Page appears to render its content with JavaScript; extraction may be empty or incomplete. Scout's JavaScript support is limited to script execution against a read-only DOM snapshot.",
        })]
    } else {
        Vec::new()
    }
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
