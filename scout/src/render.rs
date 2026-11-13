//! Layout and paint, in-process.
//!
//! Scout's other tools answer questions about a page's *text*: what it says, what
//! it links to, which form fields it has. This module answers the geometric
//! question — where each element actually sits, and what the page looks like —
//! using the `scout-render` engine instead of launching a browser.
//!
//! The engine is deliberately not given a network. Everything it needs to fetch
//! (stylesheets, `@font-face` files, images) comes back through [`Ctx::fetch`],
//! which runs Scout's own [`security::guarded_fetch`]: the same SSRF policy, the
//! same redirect re-validation, the same cookie jar as a navigation, and the same
//! byte budget. A render cannot reach a host that `/v1/navigate` would refuse,
//! because it is the same function that decides.

use crate::config::Config;
use crate::error::{ErrorCode, Result, ScoutError};
use crate::scout::Scout;
use crate::security::{self, FetchKind, FetchRequest};
use crate::session::CookieJar;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// What to render, and how big the canvas is.
#[derive(Clone)]
pub struct Render {
    pub html: String,
    /// The document's own address, so relative stylesheets and images resolve
    /// the way they do in the browser that produced this HTML.
    pub base: String,
    pub width: f32,
    pub height: f32,
    pub full_page: bool,
    /// Cap on elements in the returned geometry, so a huge page cannot produce a
    /// response the transport will reject.
    pub max_elements: usize,
}

impl Render {
    pub fn new(html: String, base: String) -> Self {
        Render {
            html,
            base,
            width: 1280.0,
            height: 800.0,
            full_page: false,
            max_elements: 1500,
        }
    }
}

/// A completed render: the geometry JSON, and the PNG when one was asked for.
pub struct Rendered {
    pub layout: Value,
    pub png: Option<Vec<u8>>,
    pub width: u32,
    pub height: u32,
    pub elapsed_ms: u128,
    /// Resources the engine could not load, as (url, reason).
    pub failed: Vec<Value>,
}

const MAX_RENDER_FETCHES: usize = 48;
const MAX_RENDER_ASSET_BYTES: usize = 16 * 1024 * 1024;
const RENDER_FETCH_DEADLINE_MS: u64 = 10_000;

/// One render's fetch context: policy, jar, cache and byte budget.
struct Ctx {
    cfg: Arc<Config>,
    client: reqwest::Client,
    handle: tokio::runtime::Handle,
    jar: Arc<CookieJar>,
    page_base: String,
    started: std::time::Instant,
    fetches: AtomicUsize,
    budget: AtomicUsize,
    cache: Mutex<HashMap<String, Option<Vec<u8>>>>,
}

thread_local! {
    /// The context of the render running on this thread. The engine's image and
    /// font loader is process-global (one engine per process), so the *request*
    /// context travels with the thread that is doing the work.
    static CURRENT: std::cell::RefCell<Option<Arc<Ctx>>> = const { std::cell::RefCell::new(None) };
}

impl Ctx {
    fn new(scout: &Scout, client: reqwest::Client, jar: Arc<CookieJar>, page_base: String) -> Arc<Ctx> {
        Arc::new(Ctx {
            cfg: scout.cfg.clone(),
            client,
            handle: tokio::runtime::Handle::current(),
            jar,
            page_base,
            started: std::time::Instant::now(),
            fetches: AtomicUsize::new(0),
            budget: AtomicUsize::new(0),
            cache: Mutex::new(HashMap::new()),
        })
    }

    fn charge(&self, bytes: usize) -> bool {
        let limit = self.cfg.max_response_bytes.saturating_mul(4).min(MAX_RENDER_ASSET_BYTES);
        let mut cur = self.budget.load(Ordering::Relaxed);
        loop {
            let Some(next) = cur.checked_add(bytes) else {
                return false;
            };
            if next > limit {
                return false;
            }
            match self.budget.compare_exchange_weak(
                cur,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(actual) => cur = actual,
            }
        }
    }

    /// Fetch one asset under Scout's network policy. `None` means "the renderer
    /// renders without it", never "the policy was bypassed".
    fn fetch(&self, url: &str) -> Option<Vec<u8>> {
        if let Some(data) = inline_data(url, self.cfg.max_response_bytes.saturating_mul(4)) {
            if !self.charge(data.len()) {
                return None;
            }
            return Some(data);
        }
        if !url.starts_with("http://") && !url.starts_with("https://") {
            return None;
        }
        if let Some(hit) = self.cache.lock().ok()?.get(url) {
            return hit.clone();
        }
        // Bound both total subresource count and total asset-fetch wall time so a
        // media-heavy page with 200 lazy images or slow ad beacons renders promptly
        // with what arrived in budget instead of stalling a blocking thread.
        if self.started.elapsed().as_millis() as u64 >= RENDER_FETCH_DEADLINE_MS {
            return None;
        }
        if self.fetches.fetch_add(1, Ordering::Relaxed) >= MAX_RENDER_FETCHES {
            return None;
        }
        let per_asset_timeout = self.cfg.nav_timeout_ms.min(3_500);
        let per_asset_max_bytes = self.cfg.max_response_bytes.min(4 * 1024 * 1024);
        // The engine calls this from a blocking thread inside `spawn_blocking`,
        // so driving Scout's async client to completion here cannot starve the
        // reactor.
        let fetched = self.handle.block_on(security::guarded_fetch(
            &self.cfg,
            &self.client,
            FetchRequest {
                url,
                method: "GET",
                headers: vec![(
                    "accept".to_string(),
                    "text/css,image/avif,image/webp,image/svg+xml,font/woff2,*/*;q=0.8".to_string(),
                )],
                body: None,
                jar: Some(&self.jar),
                cookie_mode: security::CookieMode::Subresource,
                kind: FetchKind::Subresource,
                referer: (!self.page_base.is_empty()).then_some(self.page_base.as_str()),
                timeout_ms: per_asset_timeout,
                max_bytes: per_asset_max_bytes,
            },
        ));

        let bytes = match fetched {
            Ok(f) if (200..400).contains(&f.status) => f
                .body_b64
                .as_deref()
                .and_then(base64_decode)
                .or_else(|| Some(f.body.into_bytes())),
            _ => None,
        };

        // A shared budget across every asset of one render: a page that pulls in
        // a thousand fonts stops instead of filling the process.
        if let Some(b) = &bytes {
            if !self.charge(b.len()) {
                if let Ok(mut cache) = self.cache.lock() {
                    cache.insert(url.to_string(), None);
                }
                return None;
            }
        }
        if let Ok(mut cache) = self.cache.lock() {
            cache.insert(url.to_string(), bytes.clone());
        }
        bytes
    }
}

/// `data:` URIs are inline content, not a fetch: decode them directly within `max_bytes`.
fn inline_data(url: &str, max_bytes: usize) -> Option<Vec<u8>> {
    let rest = url.strip_prefix("data:")?;
    let (meta, payload) = rest.split_once(',')?;
    if payload.len() > max_bytes.saturating_mul(2) {
        return None;
    }
    let out = if meta.ends_with(";base64") {
        base64_decode(payload)?
    } else {
        payload.as_bytes().to_vec()
    };
    (out.len() <= max_bytes).then_some(out)
}

pub fn base64_decode(input: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    let mut acc = 0u32;
    let mut bits = 0u32;
    for c in input.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            b'\n' | b'\r' | b' ' | b'\t' => continue,
            _ => return None,
        } as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((acc >> bits) & 0xFF) as u8);
        }
    }
    Some(out)
}

pub fn base64_encode(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(T[((n >> (18 - 6 * i)) & 0x3F) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

static RENDER_SLOTS: std::sync::OnceLock<tokio::sync::Semaphore> = std::sync::OnceLock::new();

/// Render `req` and return geometry (and pixels, when `png` is set).
pub async fn render(
    scout: &Scout,
    client: reqwest::Client,
    jar: Arc<CookieJar>,
    req: Render,
    png: bool,
) -> Result<Rendered> {
    let sem = RENDER_SLOTS.get_or_init(|| tokio::sync::Semaphore::new(4));
    let _permit = sem
        .acquire()
        .await
        .map_err(|_| ScoutError::new(ErrorCode::ShuttingDown, "renderer shutting down"))?;
    let max_asset_bytes = scout.cfg.max_response_bytes.saturating_mul(4).min(MAX_RENDER_ASSET_BYTES);
    let ctx = Ctx::new(scout, client, jar, req.base.clone());

    let started = std::time::Instant::now();
    let work = tokio::task::spawn_blocking(move || -> Result<(Value, Option<Vec<u8>>, u32, u32, Vec<Value>)> {
        CURRENT.with(|c| *c.borrow_mut() = Some(ctx.clone()));
        let fetcher: scout_render::image::Fetcher = Arc::new(|url: &str| {
            CURRENT.with(|c| c.borrow().as_ref().and_then(|ctx| ctx.fetch(url)))
        });
        let result = scout_render::image::with_render_scope(
            &req.base,
            fetcher,
            max_asset_bytes,
            || -> Result<(Value, Option<Vec<u8>>, u32, u32, Vec<Value>)> {
                let vw = if req.width.is_finite() { req.width.clamp(64.0, 8192.0) } else { 1280.0 };
                let vh = if req.height.is_finite() { req.height.clamp(64.0, 8192.0) } else { 800.0 };
                let viewport = scout_render::Viewport {
                    width: vw,
                    height: vh,
                };
                let mut fonts = scout_render::text::FontContext::new(scout_render::text::root_font_size());
                let loader = Loader;
                let (doc, styles, tree, failures) = scout_render::render_with(
                    &req.html,
                    &req.base,
                    viewport,
                    &mut fonts,
                    &loader,
                );

                let mut layout: Value = serde_json::from_str(&scout_render::snapshot_json(
                    &doc,
                    &styles,
                    &tree,
                    viewport,
                    &mut fonts,
                    false,
                    0,
                ))
                .unwrap_or_else(|_| json!({}));

                // Truncation is explicit: an agent that gets 1500 boxes must know the
                // page had more, or it will reason about half a document.
                if let Some(elements) = layout.get_mut("elements").and_then(|e| e.as_array_mut()) {
                    let total = elements.len();
                    if total > req.max_elements {
                        elements.truncate(req.max_elements);
                        layout["truncated"] = json!({ "shown": req.max_elements, "total": total });
                    }
                }
                layout["elapsed_ms"] = json!(started.elapsed().as_millis() as u64);

                let raw_w = if req.full_page && tree.document_width.is_finite() {
                    tree.document_width.max(vw)
                } else {
                    vw
                };
                let raw_h = if req.full_page && tree.document_height.is_finite() {
                    tree.document_height.max(vh)
                } else {
                    vh
                };
                let wanted_w = raw_w.clamp(1.0, scout_render::paint::MAX_CANVAS_DIM as f32).ceil() as u32;
                let wanted_h = raw_h.clamp(1.0, scout_render::paint::MAX_CANVAS_DIM as f32).ceil() as u32;
                let (cw, ch) = scout_render::paint::clamp_canvas_dimensions(wanted_w, wanted_h);

                let png_out = if png {
                    let canvas = scout_render::paint(
                        &doc,
                        &styles,
                        &tree,
                        &mut fonts,
                        scout_render::paint::Options {
                            width: cw,
                            height: ch,
                            ..Default::default()
                        },
                    );
                    Some(canvas.to_png().map_err(|e| {
                        ScoutError::new(ErrorCode::Internal, format!("png encode failed: {e}"))
                    })?)
                } else {
                    None
                };
                let failed = failures
                    .into_iter()
                    .map(|(url, why)| json!({ "url": url, "reason": why }))
                    .collect();
                Ok((layout, png_out, cw, ch, failed))
            },
        );
        CURRENT.with(|c| *c.borrow_mut() = None);
        result
    })
    .await
    .map_err(|e| ScoutError::new(ErrorCode::Internal, format!("render worker failed: {e}")))??;

    let (mut layout, png, width, height, failed) = work;
    scout.resources.maybe_trim();
    layout["viewport"] = json!({ "width": width, "height": height });
    if !failed.is_empty() {
        layout["failed"] = Value::Array(failed.clone());
    }
    Ok(Rendered {
        layout,
        png,
        width,
        height,
        elapsed_ms: started.elapsed().as_millis(),
        failed,
    })
}

/// The engine's stylesheet loader. It never fetches anything itself — the global
/// fetcher installed above is the only path out, and it enforces the policy.
struct Loader;

impl scout_render::resource::ResourceLoader for Loader {
    fn load(&self, url: &str) -> Option<Vec<u8>> {
        CURRENT.with(|c| c.borrow().as_ref().and_then(|ctx| ctx.fetch(url)))
    }
}

/// Geometry of the session's current page.
pub async fn page_layout(scout: &Scout, owner: &str, session_id: &str, req: Render) -> Result<Value> {
    let session = scout.sessions.get(owner, session_id)?;
    let client = scout.session_client(&session)?;
    let rendered = render(scout, client, session.jar.clone(), req, false).await?;
    Ok(rendered.layout)
}

/// A PNG of the session's current page, base64 for JSON transport.
pub async fn page_screenshot(
    scout: &Scout,
    owner: &str,
    session_id: &str,
    req: Render,
) -> Result<Value> {
    let session = scout.sessions.get(owner, session_id)?;
    let client = scout.session_client(&session)?;
    let max = scout.cfg.max_response_bytes;
    let rendered = render(scout, client, session.jar.clone(), req, true).await?;
    let png = rendered.png.unwrap_or_default();
    if png.len() > max {
        return Err(ScoutError::invalid("Screenshot exceeds the response size limit")
            .with_details(json!({ "bytes": png.len(), "limit": max })));
    }
    Ok(json!({
        "width": rendered.width,
        "height": rendered.height,
        "bytes": png.len(),
        "elapsedMs": rendered.elapsed_ms as u64,
        "encoding": "base64",
        "png": base64_encode(&png),
    }))
}

/// The `image/png` bytes for the HTTP endpoint, without a second render path.
pub async fn png_bytes(
    scout: &Scout,
    owner: &str,
    session_id: &str,
    req: Render,
) -> Result<(Vec<u8>, u32, u32)> {
    let session = scout.sessions.get(owner, session_id)?;
    let client = scout.session_client(&session)?;
    let rendered = render(scout, client, session.jar.clone(), req, true).await?;
    Ok((rendered.png.unwrap_or_default(), rendered.width, rendered.height))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trips() {
        for sample in [&b""[..], b"a", b"ab", b"abc", b"hello world"] {
            let encoded = base64_encode(sample);
            assert_eq!(base64_decode(&encoded).unwrap(), sample.to_vec());
        }
    }

    #[test]
    fn data_uris_are_inline() {
        assert_eq!(inline_data("data:text/css,body{}", 1024).unwrap(), b"body{}".to_vec());
        assert_eq!(inline_data("data:text/css;base64,YSB7fQ==", 1024).unwrap(), b"a {}".to_vec());
        assert!(inline_data("data:text/css,body{}", 2).is_none());
        assert!(inline_data("https://example.com/a.css", 1024).is_none());
    }
}
