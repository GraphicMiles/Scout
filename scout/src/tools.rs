//! The agent-facing tool layer.
//!
//! A flat, self-describing set of tools over the same primitives the HTTP API
//! exposes, so an agent can discover what it can do (`GET /v1/tools`) and then
//! do it (`POST /v1/tools/call`) without hard-coding endpoint shapes.
//!
//! Every tool routes through the same validation, SSRF policy, rate limiting and
//! admission control as the equivalent HTTP endpoint. This is a convenience and
//! discovery layer, not a second path around the runtime's own rules: a tool
//! that could reach something the HTTP API refuses would be a hole, not a
//! feature.

use crate::abort::Abort;
use crate::error::{ErrorCode, Result, ScoutError};
use crate::scout::Scout;
use crate::validate;
use serde_json::{json, Value};
use std::sync::Arc;

pub struct ToolDef {
    pub name: &'static str,
    pub description: &'static str,
    pub input_schema: Value,
}

fn tool(
    name: &'static str,
    description: &'static str,
    properties: Value,
    required: &[&str],
) -> ToolDef {
    ToolDef {
        name,
        description,
        input_schema: json!({
            "type": "object",
            "properties": properties,
            "required": required,
            "additionalProperties": false,
        }),
    }
}

fn sid() -> Value {
    json!({ "type": "string", "description": "Session id from session.create" })
}

/// Machine-readable manifest: name, description and JSON Schema per tool.
pub fn manifest() -> Value {
    let tools = vec![
        tool("session.create", "Create an isolated browsing session with its own cookie jar. Sessions never share cookies or credentials.", json!({}), &[]),
        tool("session.destroy", "Destroy a session and release its DOM, jar and worker slot.", json!({ "sessionId": sid() }), &["sessionId"]),

        tool("session.create", "Create a session with options: a label, a locale sent as Accept-Language and shown to page scripts, a timezone name, human-feeling interaction pacing, or an http/https proxy.", json!({
            "label": { "type": "string", "maxLength": 64 },
            "locale": { "type": "string", "description": "Language tag like en-GB" },
            "timezone": { "type": "string", "description": "IANA name like Europe/Lagos; stored and reported, Date objects stay UTC" },
            "humanize": { "type": "boolean" },
            "proxyUrl": { "type": "string" }
        }), &[]),
        tool("session.list", "List this key's sessions with labels and referrers.", json!({}), &[]),
        tool("session.clone", "Fork a session: the same cookies, storage and current page under a new id.", json!({ "sessionId": sid() }), &["sessionId"]),        tool("session.export", "Export the session's cookies and current document, so a signed-in session can be saved and resumed later.", json!({ "sessionId": sid() }), &["sessionId"]),
        tool("session.import", "Restore cookies exported by session.export into a session, so an existing signed-in session can be resumed without replaying a login.", json!({
            "sessionId": sid(),
            "url": { "type": "string", "description": "An origin URL the cookies are scoped to, e.g. https://example.com/" },
            "cookies": { "type": "array", "description": "Cookie objects as produced by session.export" }
        }), &["sessionId", "url", "cookies"]),
        tool("auth.login", "Sign in to a site with credentials the operator supplies: loads the login page, fills the form, submits it and reports whether a session was actually issued. Does not solve CAPTCHAs or second factors.", json!({
            "sessionId": sid(),
            "url": { "type": "string", "description": "The sign-in page" },
            "username": { "type": "string" },
            "password": { "type": "string" },
            "usernameSelector": { "type": "string", "description": "Override the discovered username field" },
            "passwordSelector": { "type": "string", "description": "Override the discovered password field" },
            "submitSelector": { "type": "string", "description": "Element whose form is submitted; defaults to the password field's form" },
            "usernameIndex": { "type": "integer", "minimum": 0 },
            "passwordIndex": { "type": "integer", "minimum": 0 },
            "fields": { "type": "object", "description": "Extra fields as selector -> value" }
        }), &["sessionId", "url", "username", "password"]),
        tool("request", "Make an HTTP request with the session's cookies and the SSRF policy applied, and return the response instead of parsing it into a page. This is the primitive for authenticated reads and writes on an API.", json!({
            "sessionId": sid(),
            "url": { "type": "string", "format": "uri" },
            "method": { "type": "string", "enum": ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"] },
            "headers": { "type": "object", "description": "Extra request headers" },
            "body": { "type": "string" },
            "contentType": { "type": "string" },
            "timeoutMs": { "type": "integer", "minimum": 1 }
        }), &["url"]),
        tool("navigate", "Navigate a session to a URL. Page scripts run and the DOM is rebuilt before this returns.", json!({
            "sessionId": sid(),
            "url": { "type": "string", "format": "uri" },
            "method": { "type": "string", "enum": ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD"] },
            "body": { "type": "string" },
            "timeoutMs": { "type": "integer", "minimum": 1 },
        }), &["sessionId", "url"]),
        tool("extract", "Extract structured data using CSS selectors or a named strategy.", json!({
            "sessionId": sid(),
            "url": { "type": "string", "format": "uri" },
            "strategy": { "type": "string" },
            "schema": { "type": "object" },
            "limit": { "type": "integer", "minimum": 1 },
        }), &[]),
        tool("interact", "Perform a raw interaction action (click, fill, select, check, uncheck, press, submit, wait, back, forward).", json!({
            "sessionId": sid(), "action": { "type": "object" },
        }), &["sessionId", "action"]),
        tool("click", "Click an element matched by selector.", json!({ "sessionId": sid(), "selector": { "type": "string" }, "index": { "type": "integer", "minimum": 0 } }), &["sessionId", "selector"]),
        tool("fill", "Type a value into an input, textarea or select.", json!({ "sessionId": sid(), "selector": { "type": "string" }, "value": { "type": "string" } }), &["sessionId", "selector", "value"]),
        tool("select", "Choose an option in a <select>. A <select multiple> accepts an array of values.", json!({ "sessionId": sid(), "selector": { "type": "string" }, "value": { "type": ["string", "array"], "items": { "type": "string" } } }), &["sessionId", "selector", "value"]),
        tool("check", "Check or uncheck a checkbox or radio.", json!({ "sessionId": sid(), "selector": { "type": "string" }, "checked": { "type": "boolean" } }), &["sessionId", "selector"]),
        tool("press", "Press a key on an element. Enter submits the element's form; any other key is dispatched to the page's scripts as keydown/keypress/keyup events.", json!({ "sessionId": sid(), "selector": { "type": "string" }, "key": { "type": "string" } }), &["sessionId", "selector", "key"]),
        tool("upload", "Stage a file on an input[type=file] control. The contents are base64; submitting the form sends it as multipart/form-data. Runs no scripts.", json!({
            "sessionId": sid(),
            "selector": { "type": "string" },
            "filename": { "type": "string", "description": "File name to send" },
            "contentBase64": { "type": "string", "description": "File contents, base64 encoded" }
        }), &["sessionId", "selector", "filename", "contentBase64"]),
        tool("scroll", "Scroll the page. Fires scroll events into the page's scripts, so lazy-loading pages fetch and render more content. Runs the page's scripts again first; scripts that append unconditionally on load may duplicate content.", json!({ "sessionId": sid(), "value": { "type": "integer", "minimum": 0, "description": "Pixels to scroll to; default 800" } }), &["sessionId"]),
        tool("submit", "Submit the form containing the matched element.", json!({ "sessionId": sid(), "selector": { "type": "string" } }), &["sessionId", "selector"]),
        tool("wait", "Wait until a selector appears, a text appears, or the URL matches, then return. Fails with wait_timeout otherwise.", json!({
            "sessionId": sid(),
            "selector": { "type": "string" },
            "text": { "type": "string" },
            "url": { "type": "string", "description": "Substring the URL must contain" },
            "timeoutMs": { "type": "integer", "minimum": 100, "description": "Budget, default 5000" }
        }), &["sessionId"]),
        tool("back", "Go back one page in the session's history.", json!({ "sessionId": sid() }), &["sessionId"]),
        tool("forward", "Go forward one page in the session's history.", json!({ "sessionId": sid() }), &["sessionId"]),
        tool("evaluate", "Run JavaScript in the page and return a value. Scripts run as a function body, so use an explicit `return`.", json!({ "sessionId": sid(), "script": { "type": "string" } }), &["sessionId", "script"]),
        tool("page.text", "Visible text of the page or of one element.", json!({ "sessionId": sid(), "selector": { "type": "string" } }), &["sessionId"]),
        tool("page.html", "Outer HTML of the document or of one element.", json!({ "sessionId": sid(), "selector": { "type": "string" } }), &["sessionId"]),
        tool("page.links", "Every link on the page, with absolute URLs.", json!({ "sessionId": sid(), "limit": { "type": "integer", "minimum": 1 } }), &["sessionId"]),
        tool("page.forms", "Every form on the page with its fields, methods and actions.", json!({ "sessionId": sid() }), &["sessionId"]),
        tool("page.title", "Document title and current URL.", json!({ "sessionId": sid() }), &["sessionId"]),
        tool("page.digest", "A token-cheap summary of the page: title, top headings, links, forms with their fields, and button labels.", json!({ "sessionId": sid() }), &["sessionId"]),
        tool("cookies.get", "List every cookie in the session jar, including HttpOnly ones that page scripts cannot see.", json!({ "sessionId": sid() }), &["sessionId"]),
        tool("cookies.set", "Seed the jar with a raw cookie value, for example to resume a signed-in session.", json!({ "sessionId": sid(), "url": { "type": "string" }, "cookie": { "type": "string" } }), &["sessionId", "url", "cookie"]),
        tool("cookies.clear", "Empty the session cookie jar.", json!({ "sessionId": sid() }), &["sessionId"]),
        tool("task.submit", "Run a multi-step automation task in the background. With everyMs it becomes a recurring schedule that only fires its callback when the result changes.", json!({ "sessionId": sid(), "steps": { "type": "array" }, "timeoutMs": { "type": "integer" }, "retries": { "type": "integer" }, "everyMs": { "type": "integer", "minimum": 5000 }, "callbackUrl": { "type": "string" }, "callbackSecret": { "type": "string" } }), &["steps"]),
        tool("task.status", "Status and result of a submitted task.", json!({ "taskId": { "type": "string" } }), &["taskId"]),
        tool("task.cancel", "Cancel a queued or running task.", json!({ "taskId": { "type": "string" } }), &["taskId"]),
        tool("schedule.list", "List recurring task schedules with their last run and changed flag.", json!({}), &[]),
        tool("schedule.get", "One recurring schedule by id.", json!({ "scheduleId": { "type": "string" } }), &["scheduleId"]),
        tool("schedule.delete", "Stop and remove a recurring schedule.", json!({ "scheduleId": { "type": "string" } }), &["scheduleId"]),
        tool("strategies", "List extraction strategies and their verification state.", json!({}), &[]),
    ];

    json!({
        "tools": tools.iter().map(|t| json!({
            "name": t.name,
            "description": t.description,
            "inputSchema": t.input_schema,
        })).collect::<Vec<_>>(),
    })
}

fn invalid(msg: impl Into<String>) -> ScoutError {
    ScoutError::new(ErrorCode::InvalidRequest, msg).with_retryable(false)
}

pub async fn call(
    scout: &Arc<Scout>,
    owner: &str,
    name: &str,
    args: Value,
    abort: &Abort,
) -> Result<Value> {
    let obj = args
        .as_object()
        .ok_or_else(|| invalid("arguments must be an object"))?;

    let str_arg = |key: &str| -> Result<String> {
        obj.get(key)
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| invalid(format!("\"{key}\" is required and must be a string")))
    };
    let opt_str = |key: &str| -> Option<String> {
        obj.get(key).and_then(|v| v.as_str()).map(|s| s.to_string())
    };

    // Convenience wrappers build a real action and hand it to the same
    // validation the `interact` endpoint uses, so an unsupported action is
    // refused here exactly as it would be there.
    let interact = |sid: &str, action: Value| -> Result<crate::validate::InteractReq> {
        validate::interact(&json!({ "sessionId": sid, "action": action }), &scout.cfg)
    };

    let out = match name {
        "session.create" => {
            let opts = validate::create_session(&args)?;
            scout.create_session_with(owner, opts)?
        }
        "session.list" => scout.list_sessions(owner),
        "session.clone" => scout.clone_session(owner, &str_arg("sessionId")?).await?,
        "session.destroy" => {
            scout.destroy_session(owner, &str_arg("sessionId")?)?;
            json!({ "ok": true })
        }

        "navigate" => {
            let mut body = json!({ "sessionId": str_arg("sessionId")?, "url": str_arg("url")? });
            if let Some(m) = opt_str("method") {
                body["method"] = json!(m);
            }
            if let Some(b) = opt_str("body") {
                body["body"] = json!(b);
            }
            if let Some(t) = obj.get("timeoutMs") {
                body["timeoutMs"] = t.clone();
            }
            let nav = validate::navigate(&body, &scout.cfg)?;
            let session = scout.sessions.get(owner, &nav.session_id.clone().unwrap_or_default())?;
            let worker = scout.worker_index(&session.id)?;
            scout
                .tasks
                .run_inline(None, None, |a| {
                    let session = session.clone();
                    let nav = nav.clone();
                    async move { scout.navigate_public(&session, worker, &nav, &a).await }
                })
                .await?
        }

        "extract" => {
            let mut body = json!({});
            for key in ["sessionId", "url", "strategy", "schema", "limit"] {
                if let Some(v) = obj.get(key) {
                    body[key] = v.clone();
                }
            }
            let er = validate::extract(&body, &scout.cfg)?;
            scout
                .tasks
                .run_inline(None, None, |a| {
                    let er = er.clone();
                    let owner = owner.to_string();
                    async move { scout.extract(&owner, er, &a).await }
                })
                .await?
        }

        "request" => {
            let mut body = json!({ "url": str_arg("url")? });
            for key in ["sessionId", "method", "headers", "body", "contentType", "timeoutMs"] {
                if let Some(v) = obj.get(key) {
                    body[key] = v.clone();
                }
            }
            let rr = validate::request(&body, &scout.cfg)?;
            scout
                .tasks
                .run_inline(None, None, |a| {
                    let rr = rr.clone();
                    let owner = owner.to_string();
                    async move { scout.request(&owner, rr, &a).await }
                })
                .await?
        }

        "auth.login" => crate::auth::login(scout, owner, &args, abort).await?,

        "session.export" => {
            let session = scout.sessions.get(owner, &str_arg("sessionId")?)?;
            json!({
                "sessionId": session.id,
                "referrer": session.referrer(),
                "cookies": session.jar.cookies()
            })
        }
        "session.import" => {
            let session = scout.sessions.get(owner, &str_arg("sessionId")?)?;
            let url = str_arg("url")?;
            let cookies = obj
                .get("cookies")
                .and_then(|c| c.as_array())
                .ok_or_else(|| invalid("\"cookies\" must be an array as produced by session.export"))?;
            let mut imported = 0usize;
            for entry in cookies {
                let name = entry.get("name").and_then(|v| v.as_str()).unwrap_or_default();
                let value = entry.get("value").and_then(|v| v.as_str()).unwrap_or("");
                if name.is_empty() {
                    continue;
                }
                let path = entry.get("path").and_then(|v| v.as_str()).unwrap_or("/");
                let mut raw = format!("{name}={value}; Path={path}");
                if let Some(d) = entry.get("domain").and_then(|v| v.as_str()).filter(|d| !d.is_empty()) {
                    raw.push_str(&format!("; Domain={d}"));
                }
                if entry.get("secure").and_then(|v| v.as_bool()).unwrap_or(false) {
                    raw.push_str("; Secure");
                }
                if entry.get("httpOnly").and_then(|v| v.as_bool()).unwrap_or(false) {
                    raw.push_str("; HttpOnly");
                }
                if session.jar.set_cookie(&url, &raw) {
                    imported += 1;
                }
            }
            json!({ "ok": true, "imported": imported, "cookies": session.jar.len() })
        }

        "interact" => {
            let ir = validate::interact(
                &json!({ "sessionId": str_arg("sessionId")?, "action": obj.get("action").cloned().unwrap_or(json!({})) }),
                &scout.cfg,
            )?;
            run_interact(scout, owner, ir).await?
        }
        "click" => {
            let mut action = json!({ "type": "click", "selector": str_arg("selector")? });
            if let Some(i) = obj.get("index") {
                action["index"] = i.clone();
            }
            let ir = interact(&str_arg("sessionId")?, action)?;
            run_interact(scout, owner, ir).await?
        }
        "fill" => {
            let ir = interact(
                &str_arg("sessionId")?,
                json!({ "type": "fill", "selector": str_arg("selector")?, "value": str_arg("value")? }),
            )?;
            run_interact(scout, owner, ir).await?
        }
        "select" => {
            // A plain string or an array of strings: a <select multiple> takes
            // several values at once.
            let value = match obj.get("value") {
                Some(Value::Array(items)) => {
                    let list: Vec<String> = items
                        .iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect();
                    if list.len() != items.len() {
                        return Err(ScoutError::invalid("\"value\" array items must be strings"));
                    }
                    json!(list)
                }
                _ => json!(str_arg("value")?),
            };
            let ir = interact(
                &str_arg("sessionId")?,
                json!({ "type": "select", "selector": str_arg("selector")?, "value": value }),
            )?;
            run_interact(scout, owner, ir).await?
        }
        "check" => {
            let checked = obj.get("checked").and_then(|v| v.as_bool()).unwrap_or(true);
            let kind = if checked { "check" } else { "uncheck" };
            let ir = interact(
                &str_arg("sessionId")?,
                json!({ "type": kind, "selector": str_arg("selector")? }),
            )?;
            run_interact(scout, owner, ir).await?
        }
        "upload" => {
            let ir = interact(
                &str_arg("sessionId")?,
                json!({ "type": "upload", "selector": str_arg("selector")?, "value": str_arg("filename")?, "contentBase64": str_arg("contentBase64")? }),
            )?;
            run_interact(scout, owner, ir).await?
        }
        "press" => {
            let ir = interact(
                &str_arg("sessionId")?,
                json!({ "type": "press", "selector": str_arg("selector")?, "key": opt_str("key").unwrap_or_else(|| "Enter".into()) }),
            )?;
            run_interact(scout, owner, ir).await?
        }
        "submit" => {
            let ir = interact(&str_arg("sessionId")?, json!({ "type": "submit", "selector": str_arg("selector")? }))?;
            run_interact(scout, owner, ir).await?
        }

        "wait" => {
            let mut action = json!({ "type": "wait" });
            for key in ["selector", "url"] {
                if let Some(v) = obj.get(key) {
                    if !v.is_null() {
                        action[key] = v.clone();
                    }
                }
            }
            if let Some(t) = obj.get("text") {
                if !t.is_null() {
                    action["value"] = t.clone();
                }
            }
            if let Some(t) = obj.get("timeoutMs") {
                action["timeoutMs"] = t.clone();
            }
            let ir = interact(&str_arg("sessionId")?, action)?;
            run_interact(scout, owner, ir).await?
        }

        "back" => {
            let ir = interact(&str_arg("sessionId")?, json!({ "type": "back" }))?;
            run_interact(scout, owner, ir).await?
        }

        "forward" => {
            let ir = interact(&str_arg("sessionId")?, json!({ "type": "forward" }))?;
            run_interact(scout, owner, ir).await?
        }

        "scroll" => {
            let mut action = json!({ "type": "scroll" });
            if let Some(v) = obj.get("value") {
                action["value"] = v.clone();
            }
            let ir = interact(&str_arg("sessionId")?, action)?;
            run_interact(scout, owner, ir).await?
        }

        "evaluate" => scout.eval(owner, &str_arg("sessionId")?, str_arg("script")?, abort).await?,

        "page.digest" => {
            let sid = str_arg("sessionId")?;
            let session = scout.sessions.get(owner, &sid)?;
            let worker = scout.worker_of(&sid)?;
            scout
                .workers
                .call(worker, |reply| crate::worker::Cmd::Digest {
                    session: session.id.clone(),
                    reply,
                })
                .await?
        }

        "page.text" => {
            let sel = opt_str("selector");
            let script = match sel {
                Some(s) => format!(
                    "var e = document.querySelector({}); return e ? e.textContent : null;",
                    js_string(&s)
                ),
                None => "return document.body ? document.body.textContent : '';".to_string(),
            };
            let v = eval_value(scout, owner, &str_arg("sessionId")?, &script, abort).await?;
            json!({ "text": v })
        }
        "page.html" => {
            let sel = opt_str("selector");
            let script = match sel {
                Some(s) => format!(
                    "var e = document.querySelector({}); return e ? e.outerHTML : null;",
                    js_string(&s)
                ),
                None => "return document.documentElement.outerHTML;".to_string(),
            };
            let v = eval_value(scout, owner, &str_arg("sessionId")?, &script, abort).await?;
            json!({ "html": v })
        }
        "page.links" => {
            let limit = obj
                .get("limit")
                .and_then(|v| v.as_u64())
                .unwrap_or(200)
                .min(2000);
            let script = format!(
                "var out = [], as = document.querySelectorAll('a'); \
                 for (var i = 0; i < as.length && out.length < {limit}; i++) {{ \
                   out.push({{ text: (as[i].textContent || '').trim(), href: as[i].href, title: as[i].title }}); \
                 }} \
                 return JSON.stringify(out);"
            );
            let raw = eval_value(scout, owner, &str_arg("sessionId")?, &script, abort).await?;
            let links: Value = serde_json::from_str(raw.as_str().unwrap_or("[]"))
                .unwrap_or_else(|_| json!([]));
            json!({ "links": links, "count": links.as_array().map(|a| a.len()).unwrap_or(0) })
        }
        "page.forms" => {
            let script = "var out = [], fs = document.querySelectorAll('form'); \
                 for (var i = 0; i < fs.length; i++) { \
                   var f = fs[i], fields = [], els = f.querySelectorAll('input, textarea, select'); \
                   for (var j = 0; j < els.length; j++) { \
                     fields.push({ tag: els[j].tagName, type: els[j].getAttribute('type'), name: els[j].name, id: els[j].id, required: els[j].hasAttribute('required'), value: els[j].value }); \
                   } \
                   out.push({ action: f.action, method: (f.getAttribute('method') || 'GET').toUpperCase(), id: f.id, fields: fields }); \
                 } \
                 return JSON.stringify(out);";
            let raw = eval_value(scout, owner, &str_arg("sessionId")?, script, abort).await?;
            let forms: Value = serde_json::from_str(raw.as_str().unwrap_or("[]"))
                .unwrap_or_else(|_| json!([]));
            json!({ "forms": forms, "count": forms.as_array().map(|a| a.len()).unwrap_or(0) })
        }
        "page.title" => {
            let script = "return JSON.stringify({ title: document.title, url: location.href });";
            let raw = eval_value(scout, owner, &str_arg("sessionId")?, script, abort).await?;
            serde_json::from_str(raw.as_str().unwrap_or("{}")).unwrap_or_else(|_| json!({}))
        }

        "cookies.get" => {
            let session = scout.sessions.get(owner, &str_arg("sessionId")?)?;
            json!({ "cookies": session.jar.cookies() })
        }
        "cookies.set" => {
            let session = scout.sessions.get(owner, &str_arg("sessionId")?)?;
            let ok = session.jar.set_cookie(&str_arg("url")?, &str_arg("cookie")?);
            if !ok {
                return Err(invalid("cookie was rejected; check its format and domain"));
            }
            json!({ "ok": true })
        }
        "cookies.clear" => {
            let session = scout.sessions.get(owner, &str_arg("sessionId")?)?;
            session.jar.clear();
            json!({ "ok": true })
        }

        "task.submit" => {
            let mut body = json!({ "steps": obj.get("steps").cloned().unwrap_or(json!([])) });
            if let Some(s) = opt_str("sessionId") {
                body["sessionId"] = json!(s);
            }
            for key in ["timeoutMs", "retries", "everyMs", "callbackUrl", "callbackSecret"] {
                if let Some(v) = obj.get(key) {
                    if !v.is_null() {
                        body[key] = v.clone();
                    }
                }
            }
            let tr = validate::task(&body, &scout.cfg)?;
            scout.submit_task(owner, tr)?
        }
        "schedule.get" => scout.schedules.get(owner, &str_arg("scheduleId")?)?,
        "schedule.list" => scout.schedules.list(owner),
        "schedule.delete" => {
            scout.schedules.delete(owner, &str_arg("scheduleId")?)?;
            json!({ "ok": true })
        }
        "task.status" => scout.get_task(owner, &str_arg("taskId")?)?,
        "task.cancel" => scout.cancel_task(owner, &str_arg("taskId")?)?,

        "strategies" => crate::adapters::list_json(),

        other => {
            return Err(invalid(format!(
                "Unknown tool \"{other}\"; call GET /v1/tools for the list"
            )))
        }
    };

    Ok(out)
}

async fn run_interact(
    scout: &Arc<Scout>,
    owner: &str,
    ir: crate::validate::InteractReq,
) -> Result<Value> {
    scout
        .tasks
        .run_inline(None, None, |a| {
            let ir = ir.clone();
            let owner = owner.to_string();
            async move { scout.interact(&owner, ir, &a).await }
        })
        .await
}

/// Run a script and unwrap its value, rejecting the no-return case loudly
/// rather than handing the caller a silent null.
async fn eval_value(
    scout: &Arc<Scout>,
    owner: &str,
    session_id: &str,
    script: &str,
    abort: &Abort,
) -> Result<Value> {
    let out = scout
        .eval(owner, session_id, script.to_string(), abort)
        .await?;
    out.get("value")
        .cloned()
        .filter(|v| !v.is_null())
        .ok_or_else(|| {
            ScoutError::new(
                ErrorCode::CompatibilityError,
                "the page script returned no value",
            )
            .with_retryable(false)
        })
}

/// Escape a string for embedding in generated JavaScript.
pub(crate) fn js_lit(s: &str) -> String {
    js_string(s)
}

fn js_string(s: &str) -> String {
    let escaped = s
        .replace('\\', "\\\\")
        .replace('\'', "\\'")
        .replace('\n', "\\n")
        .replace('\r', "\\r");
    format!("'{escaped}'")
}
