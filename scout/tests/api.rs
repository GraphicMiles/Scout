//! End-to-end tests: a real HTTP server, a real fixture site, real requests.
//!
//! The fixture site listens on loopback, which the SSRF policy blocks by default,
//! so these tests also exercise `SCOUT_PRIVATE_ALLOWLIST` — the explicit operator
//! opt-in for internal hosts.

use axum::response::{Html, IntoResponse};
use axum::routing::{get, post};
use axum::Router;
use serde_json::{json, Value};
use scout::config::Config;
use std::net::SocketAddr;
use std::sync::Arc;

const KEY: &str = "test_key_0123456789abcdef";

struct Harness {
    /// Scout's own API endpoint.
    base: String,
    /// The fixture site Scout is pointed at.
    fixture: String,
    scout: Arc<scout::scout::Scout>,
    client: reqwest::Client,
}

impl Harness {
    async fn get(&self, path: &str) -> (u16, Value) {
        let r = self
            .client
            .get(format!("{}{}", self.base, path))
            .bearer_auth(KEY)
            .send()
            .await
            .unwrap();
        let status = r.status().as_u16();
        let body = r.json().await.unwrap_or(Value::Null);
        (status, body)
    }

    async fn post(&self, path: &str, body: Value) -> (u16, Value) {
        let r = self
            .client
            .post(format!("{}{}", self.base, path))
            .bearer_auth(KEY)
            .json(&body)
            .send()
            .await
            .unwrap();
        let status = r.status().as_u16();
        let text = r.text().await.unwrap_or_default();
        (status, serde_json::from_str(&text).unwrap_or(Value::String(text)))
    }
}

/// Start a fixture site and a Scout instance pointed at it.
///
/// The fixture listener task is intentionally left running: `tokio::spawn`
/// keeps it alive for the duration of the test's runtime.
async fn harness() -> Harness {
    let fixture = Router::new()
        .route("/", get(|| async { Html(FIXTURE_HTML) }))
        .route("/deep", get(|| async { Html(FIXTURE_HTML) }))
        .route("/empty", get(|| async { Html("<html><body></body></html>") }))
        .route("/json", get(|| async { axum::Json(json!({ "hello": "world", "n": 7 })) }))
        .route("/image", get(|| async { ([( "content-type", "image/png")], vec![0u8; 64]) }))
        .route("/big", get(|| async { Html("<div>".to_string() + &"x".repeat(5_000_000) + "</div>") }))
        .route("/slow", get(|| async {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            Html("<html></html>")
        }))
        .route("/redirect", get(|| async { axum::response::Redirect::temporary("/") }))
        .route(
            "/redirect-loop",
            get(|| async { axum::response::Redirect::temporary("/redirect-loop") }),
        )
        .route("/status/500", get(|| async { (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "boom") }))
        .route("/status/403", get(|| async { (axum::http::StatusCode::FORBIDDEN, "no") }))
        .route("/echo", post(|body: String| async move { Html(format!("<html><body><pre>{body}</pre></body></html>")) }))
        .route("/setcookie", get(|| async {
            ([( "set-cookie", "sess=abc123; Path=/")], Html("<html><body>cookie set</body></html>"))
        }))
        .route("/needs-cookie", get(|headers: axum::http::HeaderMap| async move {
            let c = headers.get("cookie").and_then(|v| v.to_str().ok()).unwrap_or("");
            if c.contains("sess=abc123") {
                Html("<html><body><h1>authorised</h1></body></html>")
            } else {
                Html("<html><body><h1>denied</h1></body></html>")
            }
        }))
        // A sign-in flow: a form, a POST that only accepts one password, a
        // cookie, and a page behind it.
        .route("/login", get(|| async { Html(LOGIN_HTML) }))
        .route("/session", post(|body: String| async move {
            let good = body.contains("email=tester%40example.com") && body.contains("password=hunter2");
            if !good {
                return (
                    axum::http::StatusCode::UNAUTHORIZED,
                    [(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")],
                    LOGIN_FAILED.to_string(),
                )
                    .into_response();
            }
            (
                axum::http::StatusCode::SEE_OTHER,
                [
                    (axum::http::header::LOCATION, "/account".to_string()),
                    (
                        axum::http::header::SET_COOKIE,
                        "sid=session-xyz; Path=/; HttpOnly".to_string(),
                    ),
                ],
            )
                .into_response()
        }))
        .route("/account", get(|headers: axum::http::HeaderMap| async move {
            let c = headers.get("cookie").and_then(|v| v.to_str().ok()).unwrap_or("");
            if c.contains("sid=session-xyz") {
                Html("<html><head><title>Account</title></head><body><h1>authorised</h1><p>Welcome back</p></body></html>")
            } else {
                Html(LOGIN_HTML)
            }
        }))
        // A page whose content only exists after a fetch() resolves.
        .route("/spa", get(|| async { Html(SPA_HTML) }))
        // A lazy-loading page for the scroll action.
        .route("/lazy", get(|| async { Html(LAZY_HTML) }))
        // A key-reactive page for the press action.
        .route("/keys", get(|| async { Html(KEYS_HTML) }))
        // More items for the lazy page's scroll handler.
        .route("/api/more", get(|| async { axum::Json(json!({ "names": ["lazy-1", "lazy-2", "lazy-3"] })) }))
        // An authenticated JSON API, for the request tool to read and write.
        .route("/api/items", get(|| async { axum::Json(json!({ "items": [1, 2, 3] })) })
            .post(|body: String| async move {
                (axum::http::StatusCode::CREATED, axum::Json(json!({ "created": body })))
            }))
        // Echoes the headers it received, so a test can assert on what Scout
        // actually put on the wire.
        .route(
            "/headers",
            get(|headers: axum::http::HeaderMap| async move { echo_headers(headers) })
                .post(|headers: axum::http::HeaderMap| async move { echo_headers(headers) }),
        );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let fixture_addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, fixture).await;
    });

    let mut cfg = Config::dev();
    cfg.dev_mode = false;
    cfg.api_keys = vec![KEY.to_string()];
    cfg.port = 0;
    cfg.host = "127.0.0.1".into();
    cfg.private_allowlist = vec!["127.0.0.1".into()];
    cfg.nav_timeout_ms = 3_000;
    cfg.max_nav_timeout_ms = 5_000;
    cfg.request_timeout_ms = 8_000;
    cfg.max_response_bytes = 1_000_000;
    cfg.rate_limit_per_min = 1_000;
    cfg.session_ttl_ms = 60_000;
    cfg.js_timeout_ms = 2_000;

    let (scout, _server_task, local) = scout::server::start(cfg).await.unwrap();
    Harness {
        base: format!("http://127.0.0.1:{}", local.port()),
        fixture: format!("http://{}:{}", fixture_addr.ip(), fixture_addr.port()),
        scout,
        client: reqwest::Client::new(),
    }
}

const FIXTURE_HTML: &str = r#"<!doctype html>
<html lang="en">
<head>
  <title>Fixture Page</title>
  <meta name="description" content="A page for tests">
  <meta property="og:title" content="Fixture OG">
  <link rel="canonical" href="/canonical">
  <base href="/base/">
</head>
<body>
  <h1>Heading One</h1>
  <h2>Heading Two</h2>
  <ul class="items">
    <li class="item"><span class="name">Alpha</span><span class="price">$10.50</span><a href="/a">A</a></li>
    <li class="item"><span class="name">Beta</span><span class="price">$20.00</span><a href="/b">B</a></li>
    <li class="item"><span class="name">Gamma</span><span class="price">$30.25</span><a href="/c">C</a></li>
  </ul>
  <time datetime="2024-03-05T10:00:00Z">March 5</time>
  <script type="application/ld+json">{"@type":"Article","headline":"Fixture"}</script>
  <form action="/echo" method="POST">
    <input name="q" value="">
    <input type="hidden" name="token" value="abc">
    <button type="submit">Go</button>
  </form>
</body>
</html>"#;

/// The headers a request arrived with, as JSON.
fn echo_headers(headers: axum::http::HeaderMap) -> axum::Json<Value> {
    let mut map = serde_json::Map::new();
    for (k, v) in headers.iter() {
        map.insert(k.as_str().to_ascii_lowercase(), json!(v.to_str().unwrap_or("")));
    }
    axum::Json(Value::Object(map))
}

/// A sign-in form with a CSRF token, which is what makes the flow a real one.
const LOGIN_HTML: &str = r#"<!doctype html>
<html><head><title>Sign in</title></head>
<body>
  <h1>Sign in</h1>
  <form id="login" method="post" action="/session">
    <input type="hidden" name="csrf" value="t0ken">
    <input type="email" name="email" id="email">
    <input type="password" name="password" id="password">
    <button type="submit">Sign in</button>
  </form>
</body></html>"#;

/// The same form back again, which is what a real site does on a bad password.
const LOGIN_FAILED: &str = r#"<!doctype html>
<html><head><title>Sign in</title></head>
<body>
  <h1>Sign in</h1>
  <p class="error">Wrong email or password.</p>
  <form id="login" method="post" action="/session">
    <input type="hidden" name="csrf" value="t0ken">
    <input type="email" name="email" id="email">
    <input type="password" name="password" id="password">
    <button type="submit">Sign in</button>
  </form>
</body></html>"#;

/// Nothing in the markup: the items only exist once the script's fetch returns.
const SPA_HTML: &str = r#"<!doctype html>
<html><head><title>SPA</title></head>
<body>
  <ul id="out"></ul>
  <script>
    fetch('/api/items').then(function (r) { return r.json(); }).then(function (d) {
      var ul = document.getElementById('out');
      d.items.forEach(function (n) {
        var li = document.createElement('li');
        li.className = 'item';
        li.textContent = 'item-' + n;
        ul.appendChild(li);
      });
    });
  </script>
</body></html>"#;

/// A lazy-loading page: initial items render on load (guarded so a re-run does
/// not duplicate them), and a scroll listener fetches more. This is the shape
/// of page the scroll action exists for.
const LAZY_HTML: &str = r#"<!doctype html>
<html><head><title>Lazy</title></head>
<body>
  <ul id="list"></ul>
  <script>
    function add(names) {
      var ul = document.getElementById('list');
      for (var i = 0; i < names.length; i++) {
        var li = document.createElement('li');
        li.className = 'item';
        var s = document.createElement('span');
        s.className = 'name';
        s.textContent = names[i];
        li.appendChild(s);
        ul.appendChild(li);
      }
    }
    if (!document.getElementById('list').children.length) add(['initial-1', 'initial-2']);
    window.addEventListener('scroll', function () {
      fetch('/api/more').then(function (r) { return r.json(); }).then(function (d) {
        add(d.names);
      });
    });
  </script>
</body></html>"#;

/// A page that reacts to key events: the pattern search-as-you-type uses.
const KEYS_HTML: &str = r#"<!doctype html>
<html><head><title>Keys</title></head>
<body>
  <input id="q" type="text">
  <div id="out"></div>
  <script>
    document.getElementById('q').addEventListener('keydown', function (e) {
      if (e.type === 'keydown' && e.key.length === 1) {
        var out = document.getElementById('out');
        out.textContent = (out.textContent || '') + e.key;
      }
    });
  </script>
</body></html>"#;

// The fixture server must outlive each test, so it is started per-test above and
// the keepalive task holds the runtime slot.

/// The agent-facing tool layer: discoverable, authenticated, and unable to
/// reach anything the HTTP API would refuse.
mod tools_api {
    use super::*;

    #[tokio::test]
    async fn manifest_lists_every_tool_with_a_schema() {
        let h = harness().await;
        let (status, body) = h.get("/v1/tools").await;
        assert_eq!(status, 200);
        let tools = body["tools"].as_array().expect("tools array");
        assert!(!tools.is_empty());
        for t in tools {
            // An agent cannot call a tool whose inputs it cannot discover.
            assert!(t["name"].is_string(), "tool needs a name");
            assert!(t["description"].is_string(), "tool needs a description");
            assert_eq!(t["inputSchema"]["type"], json!("object"));
        }
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        for expected in ["navigate", "extract", "page.forms", "cookies.get", "fill", "submit"] {
            assert!(names.contains(&expected), "missing tool {expected}");
        }
    }

    #[tokio::test]
    async fn a_session_round_trips_through_tools() {
        let h = harness().await;
        let (status, created) = h
            .post("/v1/tools/call", json!({ "name": "session.create", "arguments": {} }))
            .await;
        assert_eq!(status, 200);
        let id = created["id"].as_str().expect("session id").to_string();

        let (status, _) = h
            .post("/v1/tools/call", json!({ "name": "session.destroy", "arguments": { "sessionId": id } }))
            .await;
        assert_eq!(status, 200);

        // Destroyed sessions stay gone; the tool layer must not resurrect them.
        let (status, _) = h
            .post("/v1/tools/call", json!({ "name": "session.destroy", "arguments": { "sessionId": id } }))
            .await;
        assert_eq!(status, 404);
    }

    #[tokio::test]
    async fn unknown_tools_and_unknown_fields_are_refused() {
        let h = harness().await;
        let (status, body) = h
            .post("/v1/tools/call", json!({ "name": "not.a.tool", "arguments": {} }))
            .await;
        assert_eq!(status, 400);
        assert_eq!(body["error"]["code"], "invalid_request");

        let (status, body) = h
            .post("/v1/tools/call", json!({ "name": "strategies", "arguments": {}, "extra": 1 }))
            .await;
        assert_eq!(status, 400);
        assert!(body["error"]["message"].as_str().unwrap().contains("extra"));
    }

    #[tokio::test]
    async fn navigate_then_inspect_the_page_through_tools() {
        let h = harness().await;
        let (_, created) = h
            .post("/v1/tools/call", json!({ "name": "session.create", "arguments": {} }))
            .await;
        let id = created["id"].as_str().unwrap().to_string();

        let (status, nav) = h
            .post("/v1/tools/call", json!({
                "name": "navigate",
                "arguments": { "sessionId": id, "url": format!("{}/", h.fixture) }
            }))
            .await;
        assert_eq!(status, 200, "navigate failed: {nav}");

        let (status, forms) = h
            .post("/v1/tools/call", json!({ "name": "page.forms", "arguments": { "sessionId": id } }))
            .await;
        assert_eq!(status, 200);
        assert!(forms["count"].as_u64().unwrap() >= 1, "expected a form: {forms}");

        let (status, links) = h
            .post("/v1/tools/call", json!({ "name": "page.links", "arguments": { "sessionId": id } }))
            .await;
        assert_eq!(status, 200);
        // Links come back absolute: a relative href is useless to a caller.
        for link in links["links"].as_array().unwrap() {
            let href = link["href"].as_str().unwrap_or("");
            assert!(
                href.starts_with("http://") || href.starts_with("https://") || href.starts_with('#'),
                "href was not resolved against the document: {href}"
            );
        }
    }

    #[tokio::test]
    async fn tools_require_authentication() {
        let h = harness().await;
        let r = h.client.get(format!("{}/v1/tools", h.base)).send().await.unwrap();
        assert_eq!(r.status().as_u16(), 401);
        let r = h
            .client
            .post(format!("{}/v1/tools/call", h.base))
            .json(&json!({ "name": "strategies", "arguments": {} }))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 401);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn health_is_public_and_other_routes_require_a_key() {
    let h = harness().await;

    let r = h.client.get(format!("{}/health", h.base)).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["status"], "ok");
    // Every response carries a request id.
    assert!(body.is_object());

    // No key.
    let r = h.client.get(format!("{}/v1/strategies", h.base)).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 401);
    assert!(r.headers().contains_key("www-authenticate"));

    // Wrong key.
    let r = h
        .client
        .get(format!("{}/v1/strategies", h.base))
        .bearer_auth("wrong_key_0123456789abcdef")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 401);

    // x-api-key also works.
    let r = h
        .client
        .get(format!("{}/v1/strategies", h.base))
        .header("x-api-key", KEY)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn error_shape_is_consistent() {
    let h = harness().await;
    let (status, body) = h.post("/v1/navigate", json!({})).await;
    assert_eq!(status, 400);
    assert_eq!(body["error"]["code"], "invalid_request");
    assert!(body["error"]["message"].is_string());
    assert!(body["error"]["retryable"].is_boolean());

    let (status, body) = h.get("/v1/tasks/tsk_nope").await;
    assert_eq!(status, 404);
    assert_eq!(body["error"]["code"], "not_found");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn extract_generic_page_from_a_url() {
    let h = harness().await;
    let (status, body) = h
        .post("/v1/extract", json!({ "url": "http://127.0.0.1:1/", "strategy": "generic-page" }))
        .await;
    // Port 1 is unreachable; this asserts we get a typed failure, not a panic.
    assert!(status == 502 || status == 504, "unexpected {status}: {body}");
    assert!(body["error"]["code"] == "navigation_failed" || body["error"]["code"] == "navigation_timeout");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn full_session_flow_against_fixtures() {
    let h = harness().await;

    // Start a session.
    let (status, sess) = h.post("/v1/sessions", json!({})).await;
    assert_eq!(status, 201, "{sess}");
    let sid = sess["id"].as_str().unwrap().to_string();

    // Navigate (base href is /base/, so links resolve through it).
    let (status, nav) = h
        .post("/v1/navigate", json!({ "sessionId": sid, "url": format!("{}/", h.fixture) }))
        .await;
    assert_eq!(status, 200, "navigate failed: {nav}");
    assert_eq!(nav["status"], 200);
    assert_eq!(nav["title"], "Fixture Page");

    // Schema extraction.
    let (status, ex) = h
        .post(
            "/v1/extract",
            json!({
                "sessionId": sid,
                "strategy": "selectors",
                "schema": {
                    "item": "li.item",
                    "fields": {
                        "name": { "selector": ".name" },
                        "price": { "selector": ".price", "type": "price" },
                        "link": { "selector": "a", "type": "url" }
                    }
                }
            }),
        )
        .await;
    assert_eq!(status, 200, "extract failed: {ex}");
    let rows = ex["data"].as_array().unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0]["name"], "Alpha");
    assert_eq!(rows[0]["price"]["amount"], 10.5);
    assert_eq!(rows[0]["price"]["currency"], "USD");
    // Resolved against <base href="/base/">.
    assert_eq!(rows[0]["link"], format!("{}/a", h.fixture));
    assert_eq!(ex["meta"]["strategy"], "selectors");

    // Generic extraction sees the structured data.
    let (status, gen) = h
        .post("/v1/extract", json!({ "sessionId": sid, "strategy": "generic-page" }))
        .await;
    assert_eq!(status, 200);
    assert_eq!(gen["data"]["description"], "A page for tests");
    assert_eq!(gen["data"]["openGraph"]["title"], "Fixture OG");
    assert_eq!(gen["data"]["jsonLd"][0]["headline"], "Fixture");
    assert_eq!(gen["data"]["times"][0]["datetime"], "2024-03-05T10:00:00Z");

    // Interaction: fill and submit the form.
    let (status, _) = h
        .post(
            "/v1/interact",
            json!({ "sessionId": sid, "action": { "type": "fill", "selector": "input[name=q]", "value": "hello there" } }),
        )
        .await;
    assert_eq!(status, 200);

    let (status, act) = h
        .post(
            "/v1/interact",
            json!({ "sessionId": sid, "action": { "type": "submit", "selector": "form" } }),
        )
        .await;
    assert_eq!(status, 200, "submit failed: {act}");
    assert_eq!(act["effect"], "navigate");
    // The fixture echoes the POST body back.
    let html = act["navigation"]["url"].as_str().unwrap();
    assert!(html.contains("/echo"), "{html}");

    let (_, ex2) = h
        .post("/v1/extract", json!({ "sessionId": sid, "strategy": "generic-page" }))
        .await;
    let _ = ex2;

    // Destroy.
    let r = h
        .client
        .delete(format!("{}/v1/sessions/{}", h.base, sid))
        .bearer_auth(KEY)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 204);

    // Gone.
    let (status, _) = h
        .post("/v1/extract", json!({ "sessionId": sid, "strategy": "generic-page" }))
        .await;
    assert_eq!(status, 404);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cookies_persist_within_a_session() {
    let h = harness().await;
    let (_, sess) = h.post("/v1/sessions", json!({})).await;
    let sid = sess["id"].as_str().unwrap().to_string();

    h.post("/v1/navigate", json!({ "sessionId": sid, "url": format!("{}/setcookie", h.fixture) })).await;
    let (status, nav) = h
        .post("/v1/navigate", json!({ "sessionId": sid, "url": format!("{}/needs-cookie", h.fixture) }))
        .await;
    assert_eq!(status, 200, "{nav}");

    let (_, ex) = h
        .post("/v1/extract", json!({ "sessionId": sid, "strategy": "generic-page" }))
        .await;
    let heading = ex["data"]["headings"][0]["text"].as_str().unwrap_or("");
    assert_eq!(heading, "authorised", "cookie was not replayed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cookies_do_not_leak_between_sessions() {
    let h = harness().await;
    let (_, a) = h.post("/v1/sessions", json!({})).await;
    let (_, b) = h.post("/v1/sessions", json!({})).await;
    let sa = a["id"].as_str().unwrap();
    let sb = b["id"].as_str().unwrap();

    h.post("/v1/navigate", json!({ "sessionId": sa, "url": format!("{}/setcookie", h.fixture) })).await;
    h.post("/v1/navigate", json!({ "sessionId": sb, "url": format!("{}/needs-cookie", h.fixture) })).await;

    let (_, ex) = h
        .post("/v1/extract", json!({ "sessionId": sb, "strategy": "generic-page" }))
        .await;
    let heading = ex["data"]["headings"][0]["text"].as_str().unwrap_or("");
    assert_eq!(heading, "denied", "a cookie crossed session boundaries");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unsupported_content_types_are_refused() {
    let h = harness().await;
    let (status, body) = h.post("/v1/extract", json!({ "url": format!("{}/image", h.fixture) })).await;
    assert_eq!(status, 415, "{body}");
    assert_eq!(body["error"]["code"], "unsupported_content");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn scroll_action_triggers_lazy_loading() {
    let h = harness().await;
    let (_, sess) = h.post("/v1/sessions", json!({})).await;
    let sid = sess["id"].as_str().expect("session id").to_string();
    let (status, body) = h
        .post(
            "/v1/navigate",
            json!({ "sessionId": sid, "url": format!("{}/lazy", h.fixture) }),
        )
        .await;
    assert_eq!(status, 200, "{body}");

    let count_items = |body: &Value| body["data"].as_array().map(|a| a.len()).unwrap_or(0);
    let (_, before) = h
        .post(
            "/v1/extract",
            json!({ "sessionId": sid, "strategy": "selectors", "schema": {
                "item": "li.item", "fields": { "name": ".name" }
            }}),
        )
        .await;
    assert_eq!(count_items(&before), 2, "initial render: {before}");

    let (status, scrolled) = h
        .post(
            "/v1/interact",
            json!({ "sessionId": sid, "action": { "type": "scroll", "value": 1600 } }),
        )
        .await;
    assert_eq!(status, 200, "{scrolled}");
    assert_eq!(scrolled["ok"], json!(true), "{scrolled}");
    assert_eq!(scrolled["effect"], json!("events"), "{scrolled}");

    let (_, after) = h
        .post(
            "/v1/extract",
            json!({ "sessionId": sid, "strategy": "selectors", "schema": {
                "item": "li.item", "fields": { "name": ".name" }
            }}),
        )
        .await;
    let names: Vec<&str> = after["data"]
        .as_array()
        .map(|a| a.iter().filter_map(|i| i["name"].as_str()).collect())
        .unwrap_or_default();
    assert_eq!(
        names,
        ["initial-1", "initial-2", "lazy-1", "lazy-2", "lazy-3"],
        "after scroll: {after}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn press_dispatches_key_events_to_page_scripts() {
    let h = harness().await;
    let (_, sess) = h.post("/v1/sessions", json!({})).await;
    let sid = sess["id"].as_str().expect("session id").to_string();
    let (status, body) = h
        .post(
            "/v1/navigate",
            json!({ "sessionId": sid, "url": format!("{}/keys", h.fixture) }),
        )
        .await;
    assert_eq!(status, 200, "{body}");

    let (status, pressed) = h
        .post(
            "/v1/interact",
            json!({ "sessionId": sid, "action": { "type": "press", "selector": "#q", "key": "a" } }),
        )
        .await;
    assert_eq!(status, 200, "{pressed}");
    assert_eq!(pressed["effect"], json!("events"), "{pressed}");

    let (_, text) = h
        .post(
            "/v1/tools/call",
            json!({ "name": "page.text", "arguments": { "sessionId": sid, "selector": "#out" } }),
        )
        .await;
    assert_eq!(text["text"], json!("a"), "the keydown handler must see the key exactly once: {text}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn binary_responses_come_back_as_base64_not_lossy_text() {
    let h = harness().await;
    // The request tool is the raw primitive; a binary body must arrive intact.
    let (status, body) = h
        .post(
            "/v1/tools/call",
            json!({ "name": "request", "arguments": { "url": format!("{}/image", h.fixture) } }),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["contentType"], "image/png");
    let b64 = body["bodyBase64"].as_str().expect("bodyBase64 present for binary");
    // 64 zero bytes encode to 21 groups of "AAAA" plus a final "AA==".
    let expected = format!("{}AA==", "AAAA".repeat(21));
    assert_eq!(b64, expected);
    // And the textual body must not be pretending to be the binary content.
    assert!(body["body"].as_str().unwrap_or("").trim_matches('\0').is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn oversized_responses_report_response_too_large() {
    let h = harness().await;
    let (status, body) = h.post("/v1/extract", json!({ "url": format!("{}/big", h.fixture) })).await;
    assert_eq!(status, 502, "{body}");
    assert_eq!(body["error"]["code"], "response_too_large");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn redirect_loops_are_bounded() {
    let h = harness().await;
    let (status, body) = h
        .post("/v1/extract", json!({ "url": format!("{}/redirect-loop", h.fixture) }))
        .await;
    assert_eq!(status, 502, "{body}");
    assert_eq!(body["error"]["code"], "navigation_failed");
    assert!(body["error"]["message"].as_str().unwrap().contains("redirect"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn json_responses_are_returned_as_data() {
    let h = harness().await;
    let (status, body) = h.post("/v1/extract", json!({ "url": format!("{}/json", h.fixture) })).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["data"]["json"]["hello"], "world");
    assert_eq!(body["data"]["json"]["n"], 7);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ssrf_blocks_metadata_and_plain_private_addresses() {
    let h = harness().await;
    // Loopback is on the allowlist for these tests, so use addresses that must
    // stay blocked even then: the metadata service and link-local ranges.
    for url in [
        "http://169.254.169.254/latest/meta-data/",
        "http://metadata.google.internal/",
        "http://10.0.0.1/",
        "http://192.168.1.1/",
        "file:///etc/passwd",
        "http://user:pass@example.com/",
        "http://localhost:9999/",
    ] {
        let (status, body) = h.post("/v1/extract", json!({ "url": url })).await;
        assert!(
            status == 403 || status == 400,
            "{url} should be refused, got {status}: {body}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tasks_run_and_report() {
    let h = harness().await;
    let (status, t) = h
        .post(
            "/v1/tasks",
            json!({
                "steps": [
                    { "op": "navigate", "url": format!("{}/", h.fixture) },
                    { "op": "extract", "strategy": "generic-page" }
                ]
            }),
        )
        .await;
    assert_eq!(status, 202, "{t}");
    let id = t["id"].as_str().unwrap().to_string();
    assert_eq!(t["status"], "queued");

    let mut final_view = Value::Null;
    for _ in 0..100 {
        let (_, v) = h.get(&format!("/v1/tasks/{id}")).await;
        if v["status"] == "succeeded" || v["status"] == "failed" {
            final_view = v;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(final_view["status"], "succeeded", "{final_view}");
    let steps = final_view["result"]["steps"].as_array().unwrap();
    assert_eq!(steps.len(), 2);
    assert_eq!(steps[0]["ok"], true);
    assert_eq!(final_view["result"]["data"]["title"], "Fixture Page");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failing_task_reports_the_failed_step() {
    let h = harness().await;
    let (status, t) = h
        .post(
            "/v1/tasks",
            json!({
                "steps": [
                    { "op": "navigate", "url": format!("{}/", h.fixture) },
                    { "op": "navigate", "url": "http://127.0.0.1:1/nope" }
                ],
                "retries": 0
            }),
        )
        .await;
    assert_eq!(status, 202, "{t}");
    let id = t["id"].as_str().unwrap().to_string();

    let mut final_view = Value::Null;
    for _ in 0..120 {
        let (_, v) = h.get(&format!("/v1/tasks/{id}")).await;
        if v["status"] == "succeeded" || v["status"] == "failed" {
            final_view = v;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(final_view["status"], "failed", "{final_view}");
    assert_eq!(final_view["error"]["details"]["failedStep"], 1);
    assert_eq!(final_view["error"]["details"]["completedSteps"], 1);
    assert!(final_view["result"].is_null(), "a failed task must not report a result");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn task_cancellation() {
    let h = harness().await;
    let (_, t) = h
        .post(
            "/v1/tasks",
            json!({ "steps": [{ "op": "navigate", "url": format!("{}/slow", h.fixture), "timeoutMs": 4000 }] }),
        )
        .await;
    let id = t["id"].as_str().unwrap().to_string();
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;

    let (status, c) = h.post(&format!("/v1/tasks/{id}/cancel"), json!({})).await;
    assert_eq!(status, 200, "{c}");
    assert_eq!(c["status"], "cancelled");

    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let (_, v) = h.get(&format!("/v1/tasks/{id}")).await;
    assert_eq!(v["status"], "cancelled");
    assert!(v["result"].is_null());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unknown_fields_are_rejected() {
    let h = harness().await;
    let (status, body) = h
        .post("/v1/navigate", json!({ "url": format!("{}/", h.fixture), "typoField": 1 }))
        .await;
    assert_eq!(status, 400, "{body}");
    assert!(body["error"]["message"].as_str().unwrap().contains("typoField"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unimplemented_strategies_return_unsupported_capability() {
    let h = harness().await;
    let (status, body) = h
        .post("/v1/extract", json!({ "url": format!("{}/", h.fixture), "strategy": "tiktok-profile" }))
        .await;
    assert_eq!(status, 501, "{body}");
    assert_eq!(body["error"]["code"], "unsupported_capability");

    // And the registry reports it honestly.
    let (_, list) = h.get("/v1/strategies").await;
    let arr = list["strategies"].as_array().unwrap();
    let tiktok = arr.iter().find(|s| s["name"] == "tiktok-profile").unwrap();
    assert_eq!(tiktok["status"], "not_implemented");
    assert!(tiktok["reason"].is_string());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn method_mismatch_reports_allow_header() {
    let h = harness().await;
    let r = h
        .client
        .get(format!("{}/v1/extract", h.base))
        .bearer_auth(KEY)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 405);
    assert_eq!(r.headers().get("allow").unwrap(), "POST");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unknown_route_is_404() {
    let h = harness().await;
    let (status, body) = h.get("/v1/nope").await;
    assert_eq!(status, 404);
    assert_eq!(body["error"]["code"], "not_found");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn metrics_are_exposed_and_protected() {
    let h = harness().await;
    // Generate some traffic first.
    h.post("/v1/sessions", json!({})).await;

    let r = h
        .client
        .get(format!("{}/metrics", h.base))
        .bearer_auth(KEY)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    let text = r.text().await.unwrap();
    assert!(text.contains("scout_http_requests_total"), "{text}");
    assert!(text.contains("scout_rss_bytes"), "{text}");

    // Without a key the metrics endpoint is protected too (no metrics token set).
    let r = h.client.get(format!("{}/metrics", h.base)).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 401);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_requests_are_served_without_corruption() {
    let h = harness().await;
    let base = h.base.clone();
    let fixture = h.fixture.clone();
    let mut handles = Vec::new();
    for i in 0..12 {
        let base = base.clone();
        let fixture = fixture.clone();
        handles.push(tokio::spawn(async move {
            let c = reqwest::Client::new();
            let r = c
                .post(format!("{base}/v1/extract"))
                .bearer_auth(KEY)
                .json(&json!({ "url": format!("{fixture}/"), "strategy": "generic-page" }))
                .send()
                .await
                .unwrap();
            // Under saturation some requests are refused with a retryable error;
            // none may panic or return a malformed body.
            (i, r.status().as_u16(), r.text().await.unwrap_or_default())
        }));
    }
    let mut ok = 0;
    for handle in handles {
        let (i, status, text) = handle.await.unwrap();
        let parsed: Result<Value, _> = serde_json::from_str(&text);
        assert!(parsed.is_ok(), "worker {i} returned non-JSON: {text}");
        let v = parsed.unwrap();
        if status == 200 {
            assert_eq!(v["data"]["title"], "Fixture Page");
            ok += 1;
        } else {
            assert!(v["error"]["code"].is_string(), "worker {i}: {v}");
            assert!(status == 503 || status == 429, "worker {i} unexpected {status}");
        }
    }
    assert!(ok > 0, "at least one request should have succeeded");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn oversized_request_bodies_are_refused() {
    let h = harness().await;
    let big = "x".repeat(200 * 1024);
    let r = h
        .client
        .post(format!("{}/v1/navigate", h.base))
        .bearer_auth(KEY)
        .header("content-type", "application/json")
        .body(format!("{{\"url\":\"{}\"}}", big))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 413);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn javascript_executes_against_the_loaded_page() {
    let h = harness().await;
    let (_, sess) = h.post("/v1/sessions", json!({})).await;
    let sid = sess["id"].as_str().unwrap();

    let (status, nav) = h
        .post("/v1/navigate", json!({ "sessionId": sid, "url": format!("{}/", h.fixture) }))
        .await;
    assert_eq!(status, 200, "{nav}");

    let (status, out) = h
        .post(
            "/v1/eval",
            json!({ "sessionId": sid, "script": "return document.querySelector('h1').textContent;" }),
        )
        .await;
    assert_eq!(status, 200, "{out}");
    assert_eq!(out["value"], "Heading One");

    // A runaway script is stopped, not hung.
    let (status, out) = h
        .post("/v1/eval", json!({ "sessionId": sid, "script": "while(true){}" }))
        .await;
    assert_eq!(status, 422, "{out}");
    assert_eq!(out["error"]["code"], "compatibility_error");
}

/// Signing in with credentials the operator owns, and then acting as that user:
/// the session cookie, the pages behind the wall, and the API calls that go
/// with them.
mod auth_api {
    use super::*;

    async fn new_session(h: &Harness) -> String {
        let (_, body) = h.post("/v1/sessions", json!({})).await;
        body["id"].as_str().expect("session id").to_string()
    }

    async fn call(h: &Harness, name: &str, args: Value) -> (u16, Value) {
        h.post("/v1/tools/call", json!({ "name": name, "arguments": args }))
            .await
    }

    #[tokio::test]
    async fn login_with_valid_credentials_establishes_a_session() {
        let h = harness().await;
        let sid = new_session(&h).await;
        let (status, body) = call(
            &h,
            "auth.login",
            json!({
                "sessionId": sid,
                "url": format!("{}/login", h.fixture),
                "username": "tester@example.com",
                "password": "hunter2"
            }),
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["looksAuthenticated"], json!(true), "{body}");
        assert_eq!(body["cookies"]["added"], json!(["sid"]), "{body}");
        assert_eq!(body["passwordFieldStillPresent"], json!(false));

        // And the session is real: the page behind the wall is now readable.
        let (_, nav) = call(
            &h,
            "navigate",
            json!({ "sessionId": sid, "url": format!("{}/account", h.fixture) }),
        )
        .await;
        assert_eq!(nav["status"], json!(200), "{nav}");
        let (_, text) = call(&h, "page.text", json!({ "sessionId": sid })).await;
        let text = text.to_string();
        assert!(text.contains("authorised"), "expected the account page, got {text}");
    }

    #[tokio::test]
    async fn login_with_wrong_credentials_is_refused_and_says_so() {
        let h = harness().await;
        let sid = new_session(&h).await;
        let (status, body) = call(
            &h,
            "auth.login",
            json!({
                "sessionId": sid,
                "url": format!("{}/login", h.fixture),
                "username": "tester@example.com",
                "password": "not-the-password"
            }),
        )
        .await;
        assert_eq!(status, 200, "a refused login is a result, not an error: {body}");
        assert_eq!(body["looksAuthenticated"], json!(false), "{body}");
        assert_eq!(body["cookies"]["added"], json!([]), "{body}");
        let reason = body["reason"].as_str().unwrap_or("");
        assert!(reason.contains("401"), "the refusal should name the status: {reason}");

        // Nothing was signed in, so the wall is still there.
        let (_, nav) = call(
            &h,
            "navigate",
            json!({ "sessionId": sid, "url": format!("{}/account", h.fixture) }),
        )
        .await;
        assert_eq!(nav["status"], json!(200));
        let (_, forms) = call(&h, "page.forms", json!({ "sessionId": sid })).await;
        assert!(
            forms.to_string().contains("password"),
            "an unsigned session must still see the sign-in form: {forms}"
        );
    }

    #[tokio::test]
    async fn an_exported_session_can_be_resumed_in_another_one() {
        let h = harness().await;
        let sid = new_session(&h).await;
        let (_, body) = call(
            &h,
            "auth.login",
            json!({
                "sessionId": sid,
                "url": format!("{}/login", h.fixture),
                "username": "tester@example.com",
                "password": "hunter2"
            }),
        )
        .await;
        assert_eq!(body["looksAuthenticated"], json!(true), "{body}");

        let (_, exported) = call(&h, "session.export", json!({ "sessionId": sid })).await;
        let cookies = exported["cookies"].clone();
        assert!(cookies.as_array().map(|a| !a.is_empty()).unwrap_or(false));

        let fresh = new_session(&h).await;
        let (_, imported) = call(
            &h,
            "session.import",
            json!({ "sessionId": fresh, "url": format!("{}/", h.fixture), "cookies": cookies }),
        )
        .await;
        assert_eq!(imported["imported"], json!(1), "{imported}");

        // No login in the new session: the imported cookie is enough.
        let (_, nav) = call(
            &h,
            "navigate",
            json!({ "sessionId": fresh, "url": format!("{}/account", h.fixture) }),
        )
        .await;
        assert_eq!(nav["status"], json!(200));
        let (_, text) = call(&h, "page.text", json!({ "sessionId": fresh })).await;
        assert!(text.to_string().contains("authorised"), "{text}");
    }

    #[tokio::test]
    async fn the_request_tool_reads_and_writes_with_the_session_cookie() {
        let h = harness().await;
        let sid = new_session(&h).await;

        // Sign in first: the API is behind the same session.
        let (_, body) = call(
            &h,
            "auth.login",
            json!({
                "sessionId": sid,
                "url": format!("{}/login", h.fixture),
                "username": "tester@example.com",
                "password": "hunter2"
            }),
        )
        .await;
        assert_eq!(body["looksAuthenticated"], json!(true), "{body}");

        let (_, get) = call(
            &h,
            "request",
            json!({ "sessionId": sid, "url": format!("{}/api/items", h.fixture) }),
        )
        .await;
        assert_eq!(get["status"], json!(200), "{get}");
        assert!(get["body"].as_str().unwrap_or("").contains("items"), "{get}");

        let (_, created) = call(
            &h,
            "request",
            json!({
                "sessionId": sid,
                "url": format!("{}/api/items", h.fixture),
                "method": "POST",
                "contentType": "application/json",
                "body": "{\"name\":\"new\"}"
            }),
        )
        .await;
        assert_eq!(created["status"], json!(201), "{created}");
        assert!(created["body"].as_str().unwrap_or("").contains("new"), "{created}");
    }

    #[tokio::test]
    async fn a_request_carries_cookies_picked_up_earlier_in_the_session() {
        let h = harness().await;
        let sid = new_session(&h).await;
        let (_, _) = call(
            &h,
            "navigate",
            json!({ "sessionId": sid, "url": format!("{}/setcookie", h.fixture) }),
        )
        .await;
        let (_, out) = call(
            &h,
            "request",
            json!({ "sessionId": sid, "url": format!("{}/needs-cookie", h.fixture) }),
        )
        .await;
        assert_eq!(out["status"], json!(200), "{out}");
        assert!(out["body"].as_str().unwrap_or("").contains("authorised"), "{out}");
    }

    #[tokio::test]
    async fn a_request_to_a_blocked_address_is_refused() {
        let h = harness().await;
        let sid = new_session(&h).await;
        let (status, body) = call(
            &h,
            "request",
            json!({ "sessionId": sid, "url": "http://169.254.169.254/latest/meta-data/" }),
        )
        .await;
        assert_ne!(status, 200);
        assert_eq!(body["error"]["code"], json!("blocked_destination"), "{body}");
    }
}

/// Page scripts that talk to the network, and the policy that still applies to
/// them.
mod page_network_api {
    use super::*;

    async fn new_session(h: &Harness) -> String {
        let (_, body) = h.post("/v1/sessions", json!({})).await;
        body["id"].as_str().expect("session id").to_string()
    }

    async fn call(h: &Harness, name: &str, args: Value) -> (u16, Value) {
        h.post("/v1/tools/call", json!({ "name": name, "arguments": args }))
            .await
    }

    #[tokio::test]
    async fn a_page_that_renders_itself_with_fetch_is_readable() {
        let h = harness().await;
        let sid = new_session(&h).await;
        let (_, nav) = call(
            &h,
            "navigate",
            json!({ "sessionId": sid, "url": format!("{}/spa", h.fixture) }),
        )
        .await;
        assert_eq!(nav["status"], json!(200), "{nav}");
        assert_eq!(nav["js"]["scriptsRun"], json!(1), "{nav}");
        assert_eq!(nav["js"]["network"]["requests"], json!(1), "{nav}");
        assert!(nav["js"]["jobsRun"].as_u64().unwrap_or(0) >= 1, "{nav}");

        // The fetched data is in the DOM, not just in the response.
        let (_, text) = call(&h, "page.text", json!({ "sessionId": sid })).await;
        let text = text.to_string();
        assert!(text.contains("item-1") && text.contains("item-3"), "got {text}");
    }

    #[tokio::test]
    async fn a_script_can_call_the_api_from_evaluate() {
        let h = harness().await;
        let sid = new_session(&h).await;
        let (_, _) = call(
            &h,
            "navigate",
            json!({ "sessionId": sid, "url": format!("{}/", h.fixture) }),
        )
        .await;
        let (_, out) = call(
            &h,
            "evaluate",
            json!({
                "sessionId": sid,
                "script": "return fetch('/api/items').then(function (r) { return r.json(); }).then(function (d) { return d.items.length; });"
            }),
        )
        .await;
        assert_eq!(out["value"], json!(3), "{out}");
    }

    #[tokio::test]
    async fn a_scripted_request_is_blocked_by_the_ssrf_policy() {
        let h = harness().await;
        let sid = new_session(&h).await;
        let (_, _) = call(
            &h,
            "navigate",
            json!({ "sessionId": sid, "url": format!("{}/", h.fixture) }),
        )
        .await;
        let (_, out) = call(
            &h,
            "evaluate",
            json!({
                "sessionId": sid,
                "script": "return fetch('http://169.254.169.254/').then(function () { return 'reached'; }).catch(function (e) { return 'refused: ' + e.message; });"
            }),
        )
        .await;
        let value = out["value"].as_str().unwrap_or("").to_string();
        assert!(value.starts_with("refused"), "the SSRF policy must apply to page scripts too: {value}");
    }

    #[tokio::test]
    async fn requests_look_like_the_browser_the_runtime_stands_in_for() {
        let h = harness().await;
        let sid = new_session(&h).await;
        let (_, out) = call(
            &h,
            "request",
            json!({ "sessionId": sid, "url": format!("{}/headers", h.fixture) }),
        )
        .await;
        let body: Value = serde_json::from_str(out["body"].as_str().unwrap_or("{}")).unwrap_or(Value::Null);
        let ua = body["user-agent"].as_str().unwrap_or("");
        assert!(ua.contains("Chrome/"), "expected a browser UA, got {ua}");
        assert!(body["sec-ch-ua"].as_str().unwrap_or("").contains("Chromium"), "{body}");
        assert_eq!(body["sec-fetch-mode"], json!("cors"), "{body}");
        assert_eq!(body["accept-language"], json!("en-US,en;q=0.8"), "{body}");
    }

    #[tokio::test]
    async fn a_form_post_carries_the_origin_and_referer_a_server_expects() {
        let h = harness().await;
        let sid = new_session(&h).await;
        // Land on the page first, so the POST has somewhere to come from.
        let (_, _) = call(
            &h,
            "navigate",
            json!({ "sessionId": sid, "url": format!("{}/", h.fixture) }),
        )
        .await;
        let (_, out) = call(
            &h,
            "request",
            json!({
                "sessionId": sid,
                "url": format!("{}/headers", h.fixture),
                "method": "POST",
                "contentType": "application/x-www-form-urlencoded",
                "body": "a=1"
            }),
        )
        .await;
        let body: Value = serde_json::from_str(out["body"].as_str().unwrap_or("{}")).unwrap_or(Value::Null);
        assert_eq!(body["origin"], json!(h.fixture), "{body}");
        assert_eq!(body["referer"], json!(format!("{}/", h.fixture)), "{body}");
        assert_eq!(body["sec-fetch-site"], json!("same-origin"), "{body}");
    }
}
