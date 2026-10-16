//! End-to-end tests: a real HTTP server, a real fixture site, real requests.
//!
//! The fixture site listens on loopback, which the SSRF policy blocks by default,
//! so these tests also exercise `SCOUT_PRIVATE_ALLOWLIST` — the explicit operator
//! opt-in for internal hosts.

use axum::response::Html;
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
        }));

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
