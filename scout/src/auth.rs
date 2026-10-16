//! Signing in with credentials the operator supplies.
//!
//! ## What this is
//!
//! A session that belongs to someone who has an account on the target: the
//! username and password they typed, submitted through their own login form,
//! over their own connection, into a cookie jar this session owns. Everything
//! downstream — navigation, extraction, `fetch` from page scripts, the
//! `request` tool — then carries that session the way a browser would.
//!
//! ## What this is not
//!
//! There is no credential stuffing, no CAPTCHA solving, no challenge
//! answering, no rate-limit evasion and no guessing. A login that a human
//! could not complete with these credentials fails here too, and the failure
//! is reported with the evidence rather than retried in the hope it works the
//! second time. Whether a given account, target and purpose is authorised is
//! the operator's decision; this module only makes an authorised login
//! possible to automate, and says plainly what happened.

use crate::abort::Abort;
use crate::error::Result;
use crate::scout::Scout;
use crate::session::Session;
use serde_json::{json, Value};
use std::sync::Arc;

/// Find the password field, the field above it, and the form that owns them.
///
/// Indices are into the document's `input` elements, because that is what the
/// interaction layer can address: a generated selector is guesswork, whereas
/// "the third input on the page" is exactly what was looked at.
const DISCOVER: &str = r##"
return (function () {
  var all = document.querySelectorAll('input');
  var pw = -1, user = -1, i, t;
  for (i = 0; i < all.length; i++) {
    t = String(all[i].getAttribute('type') || 'text').toLowerCase();
    if (t === 'password' && pw < 0) pw = i;
    else if ((t === 'text' || t === 'email' || t === 'tel') && pw < 0 && user < 0) user = i;
  }
  if (pw < 0) return { found: false, inputs: all.length };
  var pwEl = all[pw];
  var form = pwEl, guard = 0;
  while (form && String(form.nodeName || '').toLowerCase() !== 'form' && guard++ < 20) {
    form = form.parentNode;
  }
  return {
    found: true,
    passwordIndex: pw,
    usernameIndex: user,
    usernameName: user >= 0 ? String(all[user].getAttribute('name') || '') : '',
    passwordName: String(pwEl.getAttribute('name') || ''),
    formMethod: form ? String(form.getAttribute('method') || 'GET').toUpperCase() : 'GET',
    formAction: form ? String(form.getAttribute('action') || '') : '',
    inputs: all.length
  };
})();
"##;

/// What the landing page looks like afterwards: did the wall come back, and
/// where did the session end up?
const VERIFY: &str = r##"
return (function () {
  var all = document.querySelectorAll('input');
  var passwords = 0, i, t;
  for (i = 0; i < all.length; i++) {
    t = String(all[i].getAttribute('type') || 'text').toLowerCase();
    if (t === 'password') passwords++;
  }
  var body = document.body ? String(document.body.textContent || '') : '';
  return {
    url: location.href,
    title: String(document.title || ''),
    passwordFields: passwords,
    inputs: all.length,
    text: body.replace(/\s+/g, ' ').slice(0, 400)
  };
})();
"##;

pub async fn login(scout: &Arc<Scout>, owner: &str, args: &Value, abort: &Abort) -> Result<Value> {
    let obj = args.as_object().ok_or_else(|| {
        crate::error::ScoutError::invalid("arguments must be an object")
    })?;
    let str_arg = |key: &str| -> Result<String> {
        obj.get(key)
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| {
                crate::error::ScoutError::invalid(format!("\"{key}\" is required and must be a string"))
            })
    };
    let opt_str = |key: &str| -> Option<String> {
        obj.get(key).and_then(|v| v.as_str()).map(|s| s.to_string())
    };

    let session_id = str_arg("sessionId")?;
    let url = str_arg("url")?;
    let username = str_arg("username")?;
    let password = str_arg("password")?;

    // A cancelled caller must not be signed in on its behalf.
    if abort.token().is_cancelled() {
        return Err(abort.reason());
    }
    let session = scout.sessions.get(owner, &session_id)?;
    let worker = scout.worker_index(&session.id)?;

    let mut steps: Vec<Value> = Vec::new();
    let cookies_before = session.jar.cookies();

    // 1. Load the login page. A site that refuses to serve it has already
    //    answered the question, and its error travels back unchanged.
    let nav = crate::validate::navigate(
        &json!({ "sessionId": session.id, "url": url }),
        &scout.cfg,
    )?;
    let summary = scout
        .tasks
        .run_inline(None, None, |a| {
            let session = session.clone();
            let nav = nav.clone();
            let scout = scout.clone();
            async move { scout.navigate_public(&session, worker, &nav, &a).await }
        })
        .await?;
    steps.push(json!({
        "step": "navigate",
        "url": summary.get("url").cloned().unwrap_or(Value::Null),
        "status": summary.get("status").cloned().unwrap_or(Value::Null),
    }));

    // 2. Locate the form. An explicit selector wins; otherwise the page is
    //    asked which field takes the password.
    let discovered = if opt_str("passwordSelector").is_some() {
        None
    } else {
        let out = eval_page(scout, owner, &session, DISCOVER, abort).await?;
        let found = out.get("found").and_then(|v| v.as_bool()).unwrap_or(false);
        if !found {
            return Ok(json!({
                "looksAuthenticated": false,
                "reason": "no password field on the page; either it is not a sign-in form, \
                           or it is served by script that did not run. Supply \
                           \"passwordSelector\" and \"usernameSelector\" to point at it directly.",
                "steps": steps,
                "page": { "url": summary.get("url").cloned().unwrap_or(Value::Null) },
            }));
        }
        Some(out)
    };

    let password_target = match opt_str("passwordSelector") {
        Some(sel) => json!({ "selector": sel }).with_index(obj.get("passwordIndex")),
        None => json!({ "selector": "input" })
            .with_index(discovered.as_ref().and_then(|d| d.get("passwordIndex"))),
    };

    // 3. Type. Each field is a separate interaction so a failure names the field.
    if let Some(sel) = opt_str("usernameSelector") {
        let target = json!({ "selector": sel }).with_index(obj.get("usernameIndex"));
        act(scout, owner, &session, "fill", target, Some(&username)).await?;
        steps.push(json!({ "step": "fill", "field": "username", "selector": sel }));
    } else if let Some(idx) = discovered
        .as_ref()
        .and_then(|d| d.get("usernameIndex"))
        .and_then(|v| v.as_i64())
        .filter(|i| *i >= 0)
    {
        act(
            scout,
            owner,
            &session,
            "fill",
            json!({ "selector": "input", "index": idx }),
            Some(&username),
        )
        .await?;
        let name = discovered
            .as_ref()
            .and_then(|d| d.get("usernameName"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        steps.push(json!({ "step": "fill", "field": "username", "name": name }));
    }

    act(
        scout,
        owner,
        &session,
        "fill",
        password_target.clone(),
        Some(&password),
    )
    .await?;
    steps.push(json!({ "step": "fill", "field": "password" }));

    // Any other field the caller wants set: a hidden token is usually already
    // in the form, but a "remember me" or a tenant selector is not.
    if let Some(Value::Object(extra)) = obj.get("fields") {
        let mut keys: Vec<&String> = extra.keys().collect();
        keys.sort();
        for key in keys {
            if let Some(v) = extra[key].as_str() {
                act(
                    scout,
                    owner,
                    &session,
                    "fill",
                    json!({ "selector": key.clone() }),
                    Some(v),
                )
                .await?;
                steps.push(json!({ "step": "fill", "field": key }));
            }
        }
    }

    // 4. Submit. This performs the request and loads whatever comes back, so
    //    the session's cookies and its next Referer are already updated.
    let submit_target = match opt_str("submitSelector") {
        Some(sel) => json!({ "selector": sel }),
        None => password_target.clone(),
    };
    let submitted = act(scout, owner, &session, "submit", submit_target, None).await;
    steps.push(json!({ "step": "submit" }));

    let mut error: Option<String> = None;
    match submitted {
        Ok(v) => steps.push(json!({ "step": "navigated", "page": v.get("page").cloned().unwrap_or(Value::Null) })),
        Err(e) => {
            // A rejected login is a result, not a crash: the caller needs to
            // know the password was refused, in what words.
            error = Some(e.message.clone());
            steps.push(json!({ "step": "submitFailed", "error": e.message }));
        }
    }

    // 5. Report the evidence. No guessing: the cookies that appeared, whether
    //    the wall is still standing, and what the landing page says.
    let cookies_after = session.jar.cookies();
    let before_names: Vec<String> = cookies_before
        .iter()
        .filter_map(|c| c.get("name").and_then(|n| n.as_str()).map(|s| s.to_string()))
        .collect();
    let mut added: Vec<String> = Vec::new();
    for c in &cookies_after {
        let name = c.get("name").and_then(|n| n.as_str()).unwrap_or("");
        let is_new = !before_names.iter().any(|b| b == name);
        if is_new && !name.is_empty() {
            added.push(name.to_string());
        }
    }

    let mut landing: Option<Value> = None;
    if error.is_none() {
        match eval_page(scout, owner, &session, VERIFY, abort).await {
            Ok(v) => landing = Some(v),
            Err(e) => landing = Some(json!({ "error": e.message })),
        }
    }

    let password_still_there = landing
        .as_ref()
        .and_then(|l| l.get("passwordFields"))
        .and_then(|v| v.as_i64())
        .unwrap_or(1)
        > 0;
    let looks_authenticated = error.is_none() && !added.is_empty() && !password_still_there;
    let reason = if let Some(e) = &error {
        format!("the sign-in request was rejected: {e}")
    } else if added.is_empty() && password_still_there {
        "the form was submitted but no session cookie was issued and the \
         password field is still on the page: the credentials were not accepted."
            .to_string()
    } else if added.is_empty() {
        "the form was submitted and the wall is gone, but the target issued no \
         cookie: it may use a token in storage or in the URL instead."
            .to_string()
    } else if password_still_there {
        "cookies were issued but the sign-in form is still on the page: check \
         for a second step such as a one-time code or a CAPTCHA. Scout does not \
         complete either."
            .to_string()
    } else {
        "a session cookie was issued and the sign-in form is gone.".to_string()
    };

    Ok(json!({
        "looksAuthenticated": looks_authenticated,
        "reason": reason,
        "url": landing.as_ref().and_then(|l| l.get("url").cloned()).unwrap_or(Value::Null),
        "title": landing.as_ref().and_then(|l| l.get("title").cloned()).unwrap_or(Value::Null),
        "cookies": {
            "before": cookies_before.len(),
            "after": cookies_after.len(),
            "added": added,
        },
        "passwordFieldStillPresent": password_still_there,
        "landing": landing,
        "steps": steps,
    }))
}

/// `{"selector": ...}` plus an optional `index`, which is how the interaction
/// layer addresses one element among several matches.
trait WithIndex {
    fn with_index(self, index: Option<&Value>) -> Value;
}

impl WithIndex for Value {
    fn with_index(mut self, index: Option<&Value>) -> Value {
        if let (Some(map), Some(i)) = (self.as_object_mut(), index.and_then(|v| v.as_i64())) {
            map.insert("index".into(), json!(i));
        }
        self
    }
}

async fn act(
    scout: &Arc<Scout>,
    owner: &str,
    session: &Arc<Session>,
    kind: &str,
    target: Value,
    value: Option<&str>,
) -> Result<Value> {
    let mut action = target;
    if let Some(map) = action.as_object_mut() {
        map.insert("type".into(), json!(kind));
        if let Some(v) = value {
            map.insert("value".into(), json!(v));
        }
    }
    let ir = crate::validate::interact(
        &json!({ "sessionId": session.id, "action": action }),
        &scout.cfg,
    )?;
    scout
        .tasks
        .run_inline(None, None, |a| {
            let ir = ir.clone();
            let owner = owner.to_string();
            let scout = scout.clone();
            async move { scout.interact(&owner, ir, &a).await }
        })
        .await
}

async fn eval_page(
    scout: &Arc<Scout>,
    owner: &str,
    session: &Arc<Session>,
    script: &str,
    abort: &Abort,
) -> Result<Value> {
    let out = scout
        .eval(owner, &session.id, script.to_string(), abort)
        .await?;
    out.get("value")
        .cloned()
        .filter(|v| !v.is_null())
        .ok_or_else(|| {
            crate::error::ScoutError::new(
                crate::error::ErrorCode::CompatibilityError,
                "the page returned no value from the discovery script",
            )
        })
}
