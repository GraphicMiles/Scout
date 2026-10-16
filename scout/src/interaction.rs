//! Interaction: click, fill, select, check, press and form submission.
//!
//! ## What interaction means here
//!
//! Scout does not run an event loop, so a click cannot dispatch a DOM event to a
//! JavaScript handler. What it *can* do honestly is perform the navigation a
//! click would cause — following a link, or submitting the form a control
//! belongs to — and report plainly when an action would have required script.
//!
//! Typed values are recorded in the page's form state rather than written into
//! the document, which keeps the parsed tree immutable and makes "values are
//! dropped on navigation" a property of ownership instead of a cleanup step.

use crate::config::Config;
use crate::error::{ErrorCode, Result, ScoutError};
use crate::html;
use crate::page::Page;
use crate::selector;
use crate::util::resolve_url;
use scraper::{ElementRef, Node};
use serde_json::{json, Value};

/// The action types Node accepts. Anything else is a caller error rather than
/// an unimplemented feature, so it is refused with `invalid_request`.
const ACTION_TYPES: &[&str] = &[
    "click", "fill", "select", "check", "uncheck", "press", "submit", "scroll",
    "upload",
];

#[derive(Debug, Clone)]
pub struct Action {
    pub kind: String,
    pub selector: Option<String>,
    /// The action's values: exactly one for `fill`, one or more for `select`.
    pub values: Vec<String>,
    pub key: Option<String>,
    /// File contents for `upload`, base64 encoded.
    pub content_base64: Option<String>,
    /// Which match to act on when the selector matches several elements.
    pub index: Option<u64>,
    /// Per-action navigation budget, clamped by the caller.
    pub timeout_ms: Option<u64>,
}

impl Action {
    /// Node's `parseAction`: unknown fields, unknown types, a missing selector
    /// and per-type value rules are all caller errors, reported by name.
    pub fn from_json(v: &Value) -> Result<Self> {
        let bad = |msg: String, field: &str| {
            ScoutError::invalid(msg).with_details(json!({ "field": field }))
        };
        let o = v
            .as_object()
            .ok_or_else(|| bad("action must be an object".into(), "action"))?;
        for k in o.keys() {
            if !["type", "selector", "index", "value", "key", "contentBase64", "timeoutMs"].contains(&k.as_str()) {
                return Err(bad(format!("Unknown field \"{k}\" in action"), k));
            }
        }
        let kind = o
            .get("type")
            .and_then(|t| t.as_str())
            .ok_or_else(|| bad("action.type is required".into(), "type"))?
            .to_string();
        if !ACTION_TYPES.contains(&kind.as_str()) {
            return Err(bad(
                format!("action.type must be one of {}", ACTION_TYPES.join(", ")),
                "type",
            ));
        }
        let selector = match o.get("selector") {
            None | Some(Value::Null) => None,
            Some(Value::String(sel)) => {
                crate::validate::check_selector(sel)?;
                Some(sel.clone())
            }
            Some(_) => {
                return Err(bad(
                    "\"selector\" must be a non-empty string up to 512 characters".into(),
                    "selector",
                ))
            }
        };
        if selector.is_none() && kind != "scroll" {
            return Err(bad("\"selector\" is required".into(), "selector"));
        }
        let values = match kind.as_str() {
            "fill" => match o.get("value") {
                Some(Value::String(val)) if val.chars().count() <= 10_000 => vec![val.clone()],
                _ => {
                    return Err(bad(
                        "\"value\" must be a string up to 10000 characters".into(),
                        "value",
                    ))
                }
            },
            "select" => {
                let mut list: Vec<String> = Vec::new();
                match o.get("value") {
                    Some(Value::String(val)) => list.push(val.clone()),
                    Some(Value::Array(items)) => {
                        for item in items {
                            match item.as_str() {
                                Some(x) => list.push(x.to_string()),
                                None => {
                                    return Err(bad(
                                        "\"value\" must be a string or array of up to 50 strings"
                                            .into(),
                                        "value",
                                    ))
                                }
                            }
                        }
                    }
                    _ => {}
                }
                if list.is_empty() || list.len() > 50 || list.iter().any(|x| x.chars().count() > 512)
                {
                    return Err(bad(
                        "\"value\" must be a string or array of up to 50 strings".into(),
                        "value",
                    ));
                }
                list
            }
            // Pixels to scroll by; optional, defaulting to 800 in the caller.
            "scroll" => match o.get("value") {
                None | Some(Value::Null) => Vec::new(),
                Some(Value::String(v))
                    if v.chars().count() <= 9 && v.chars().all(|c| c.is_ascii_digit()) =>
                {
                    vec![v.clone()]
                }
                Some(Value::Number(n)) if n.as_u64().map(|u| u < 1_000_000_000).unwrap_or(false) => {
                    vec![n.to_string()]
                }
                _ => {
                    return Err(bad(
                        "\"value\" for scroll must be a non-negative number of pixels".into(),
                        "value",
                    ))
                }
            },
            // The file's name; contents arrive separately as base64.
            "upload" => match o.get("value") {
                Some(Value::String(val)) if is_safe_filename(val) => vec![val.clone()],
                _ => {
                    return Err(bad(
                        "\"value\" for upload must be a file name: 1 to 256 characters, no path separators"
                            .into(),
                        "value",
                    ))
                }
            },
            _ => Vec::new(),
        };
        let index = match o.get("index") {
            None | Some(Value::Null) => None,
            Some(Value::Number(n)) => match n.as_u64() {
                Some(i) if i <= 1000 => Some(i),
                _ => {
                    return Err(bad(
                        "\"index\" must be an integer between 0 and 1000".into(),
                        "index",
                    ))
                }
            },
            Some(_) => {
                return Err(bad(
                    "\"index\" must be an integer between 0 and 1000".into(),
                    "index",
                ))
            }
        };
        let key = match o.get("key") {
            None | Some(Value::Null) => None,
            Some(Value::String(k)) if !k.is_empty() && k.chars().count() <= 32 => Some(k.clone()),
            Some(_) => {
                return Err(bad(
                    "\"key\" must be a non-empty string up to 32 characters".into(),
                    "key",
                ))
            }
        };
        let content_base64 = match o.get("contentBase64") {
            None | Some(Value::Null) => None,
            // A hard ceiling independent of config: the decoded size is
            // checked again, against the configured cap, when the file is
            // staged. 16 MB of base64 covers every sane `max_upload_bytes`.
            Some(Value::String(s)) if !s.is_empty() && s.len() <= 16 * 1024 * 1024 => {
                Some(s.clone())
            }
            Some(_) => {
                return Err(bad(
                    "\"contentBase64\" must be a non-empty base64 string up to 16MB".into(),
                    "contentBase64",
                ))
            }
        };
        let timeout_ms = match o.get("timeoutMs") {
            None | Some(Value::Null) => None,
            Some(Value::Number(n)) => match n.as_u64() {
                Some(t) if (100..=30_000).contains(&t) => Some(t),
                _ => {
                    return Err(bad(
                        "\"timeoutMs\" must be an integer between 100 and 30000".into(),
                        "timeoutMs",
                    ))
                }
            },
            Some(_) => {
                return Err(bad(
                    "\"timeoutMs\" must be an integer between 100 and 30000".into(),
                    "timeoutMs",
                ))
            }
        };
        Ok(Action {
            kind,
            selector,
            values,
            key,
            content_base64,
            index,
            timeout_ms,
        })
    }
}

/// A file name a browser would accept: not empty, not absurdly long, and not
/// smuggling a path. Browsers keep only the final component anyway.
fn is_safe_filename(name: &str) -> bool {
    !name.is_empty()
        && name.chars().count() <= 256
        && !name.contains(['/', '\\', '\0'])
        && name != "."
        && name != ".."
}

/// Standard base64 decode, no padding required, whitespace ignored. Returns
/// `None` on any character outside the alphabet instead of failing half way.
fn base64_decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some((c - b'A') as u32),
            b'a'..=b'z' => Some((c - b'a' + 26) as u32),
            b'0'..=b'9' => Some((c - b'0' + 52) as u32),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let cleaned: Vec<u8> = s
        .bytes()
        .filter(|b| !b.is_ascii_whitespace() && *b != b'=')
        .collect();
    let mut out = Vec::with_capacity(cleaned.len() / 4 * 3);
    for chunk in cleaned.chunks(4) {
        let n = chunk.iter().try_fold(0u32, |acc, &c| val(c).map(|v| (acc << 6) | v))?;
        match chunk.len() {
            4 => out.extend_from_slice(&[(n >> 16) as u8, (n >> 8) as u8, n as u8]),
            3 => out.extend_from_slice(&[(n >> 10) as u8, (n >> 2) as u8]),
            2 => out.push((n >> 4) as u8),
            _ => return None,
        }
    }
    Some(out)
}

/// What the caller should do next, if anything.
#[derive(Debug, Clone)]
pub enum Effect {
    /// Nothing to fetch; the action updated local state only.
    None { detail: Value },
    /// Perform this request and load the result into the session.
    Navigate {
        url: String,
        method: String,
        /// The request body as raw bytes: form posts are text, multipart
        /// posts carry file bytes that are not valid UTF-8.
        body: Option<Vec<u8>>,
        content_type: Option<String>,
        /// The caller's per-action budget for this navigation, if it gave one.
        timeout_ms: Option<u64>,
    },
}

impl Effect {
    pub fn to_json(&self) -> Value {
        match self {
            Effect::None { detail } => json!({ "effect": "none", "detail": detail }),
            Effect::Navigate {
                url,
                method,
                body,
                content_type,
                ..
            } => json!({
                "effect": "navigate",
                "url": url,
                "method": method,
                "bodyBytes": body.as_ref().map(|b| b.len()),
                "contentType": content_type,
            }),
        }
    }
}

/// Actions that a document-only runtime cannot perform. Reported by name so the
/// caller learns precisely what is missing instead of receiving a generic error.
/// (Scroll left this list: it is now delivered to the page's scripts as a
/// scroll event, which is what lazy loading listens for.)
const UNSUPPORTED_ACTIONS: &[(&str, &str)] = &[];
pub fn apply(page: &mut Page, action: &Action, cfg: &Config) -> Result<Effect> {
    if let Some((_, why)) = UNSUPPORTED_ACTIONS.iter().find(|(k, _)| *k == action.kind) {
        return Err(ScoutError::unsupported(*why).with_details(json!({ "action": action.kind })));
    }
    const KNOWN: &[&str] = &["fill", "select", "check", "uncheck", "press", "submit", "click", "upload"];
    if !KNOWN.contains(&action.kind.as_str()) {
        return Err(ScoutError::unsupported(format!(
            "Interaction \"{}\" is not supported",
            action.kind
        ))
        .with_details(json!({ "action": action.kind, "supported": KNOWN })));
    }
    if page.kind != crate::page::PageKind::Html {
        return Err(ScoutError::new(
            ErrorCode::CompatibilityError,
            "Interaction needs an HTML page",
        ));
    }
    let selector_str = action
        .selector
        .as_deref()
        .ok_or_else(|| ScoutError::invalid("action.selector is required"))?;

    match action.kind.as_str() {
        "fill" => fill(page, action, selector_str),
        "select" => select(page, action, selector_str),
        "check" => set_checked(page, action, selector_str, true),
        "uncheck" => set_checked(page, action, selector_str, false),
        "press" => press(page, action, selector_str),
        "submit" => submit_form(page, action, selector_str, cfg),
        "click" => click(page, action, selector_str, cfg),
        "upload" => upload(page, action, selector_str, cfg),
        other => Err(ScoutError::unsupported(format!(
            "Interaction \"{other}\" is not supported"
        ))
        .with_details(json!({ "action": other }))),
    }
}

fn doc(page: &Page) -> Result<&scraper::Html> {
    page.doc
        .as_ref()
        .ok_or_else(|| ScoutError::new(ErrorCode::CompatibilityError, "Interaction needs an HTML page"))
}

/// Find the element an action refers to. `index` selects among matches, so a
/// selector that matches several elements is addressed the same way the Node
/// runtime addressed it.
fn find<'a>(page: &'a Page, sel: &str, index: u64) -> Result<ElementRef<'a>> {
    let compiled = selector::compile(sel)?;
    selector::select_one_indexed(doc(page)?, &compiled, index as usize).ok_or_else(|| {
        let at = if index > 0 { format!(" at index {index}") } else { String::new() };
        ScoutError::new(
            ErrorCode::ElementNotFound,
            format!("No element matches selector \"{sel}\"{at}"),
        )
        .with_retryable(false)
        .with_details(json!({ "selector": sel, "index": index }))
    })
}

/// Reject a field the runtime cannot faithfully fill, rather than pretending.
fn reject_unfillable(el: &ElementRef, sel: &str) -> Result<()> {
    let ty = el.value().attr("type").unwrap_or("").to_ascii_lowercase();
    if ty == "file" {
        return Err(ScoutError::unsupported(
            "File inputs take the upload action, not fill",
        )
        .with_details(json!({ "selector": sel })));
    }
    Ok(())
}

/// Record a typed value under the element's *canonical* key.
///
/// The caller's selector and the key used at submission time must agree, or a
/// filled field silently submits empty. Both sides go through `element_selector`.
fn record(page: &mut Page, sel: &str, value: String) {
    page.form_state.insert(sel.to_string(), vec![value]);
}

/// Record several values under one key: what a `<select multiple>` produces.
fn record_many(page: &mut Page, sel: &str, values: Vec<String>) {
    page.form_state.insert(sel.to_string(), values);
}

/// The recorded state of a checkbox-style key: touched and non-empty means
/// checked, touched and empty means explicitly unchecked.
fn recorded_checked(v: &[String]) -> bool {
    matches!(v.first(), Some(s) if !s.is_empty())
}

fn upload(page: &mut Page, action: &Action, sel: &str, cfg: &Config) -> Result<Effect> {
    let filename = action
        .values
        .first()
        .cloned()
        .ok_or_else(|| ScoutError::invalid("upload requires \"value\" (the file name)"))?;
    let el = find(page, sel, action.index.unwrap_or(0))?;
    let tag = el.value().name().to_string();
    let ty = el.value().attr("type").unwrap_or("").to_ascii_lowercase();
    if tag != "input" || ty != "file" {
        return Err(ScoutError::unsupported(format!(
            "Upload targets <input type=file>, not a <{tag} type={ty}> element"
        ))
        .with_details(json!({ "selector": sel, "tag": tag, "type": ty })));
    }
    // `is_safe_filename` ran at parse time; the value is unchanged since.
    debug_assert!(is_safe_filename(&filename));

    let b64 = action
        .content_base64
        .as_deref()
        .ok_or_else(|| ScoutError::invalid("upload requires \"contentBase64\""))?;
    // Reject before decoding, so an oversized payload never allocates.
    let decoded_len = b64.len() / 4 * 3;
    if decoded_len > cfg.max_upload_bytes {
        return Err(ScoutError::invalid(format!(
            "Upload is {} bytes; the limit is {}",
            decoded_len, cfg.max_upload_bytes
        ))
        .with_details(json!({ "selector": sel, "limitBytes": cfg.max_upload_bytes })));
    }
    let bytes = base64_decode(b64).ok_or_else(|| {
        ScoutError::invalid("\"contentBase64\" is not valid base64")
            .with_details(json!({ "selector": sel }))
    })?;

    let size = bytes.len();
    page.files.insert(
        element_selector(&el),
        crate::page::StoredFile { filename, bytes },
    );
    Ok(Effect::None {
        detail: json!({ "uploaded": sel, "fileName": action.values[0].as_str(), "sizeBytes": size }),
    })
}

fn fill(page: &mut Page, action: &Action, sel: &str) -> Result<Effect> {
    let value = action
        .values
        .first()
        .cloned()
        .ok_or_else(|| ScoutError::invalid("fill requires \"value\""))?;
    let el = find(page, sel, action.index.unwrap_or(0))?;
    reject_unfillable(&el, sel)?;
    let tag = el.value().name().to_string();
    if !matches!(tag.as_str(), "input" | "textarea" | "select") {
        return Err(ScoutError::unsupported(format!(
            "Cannot fill a <{tag}> element"
        ))
        .with_details(json!({ "selector": sel, "tag": tag })));
    }
    let ty = el.value().attr("type").unwrap_or("text").to_ascii_lowercase();
    if !matches!(
        ty.as_str(),
        "text" | "search" | "email" | "url" | "tel" | "password" | "number" | "hidden" | "textarea" | "select"
    ) && !matches!(tag.as_str(), "textarea" | "select")
    {
        return Err(ScoutError::unsupported(format!(
            "Input type \"{ty}\" cannot be filled"
        ))
        .with_details(json!({ "selector": sel, "type": ty })));
    }
    let value_len = value.len();
    record(page, &element_selector(&el), value);
    Ok(Effect::None {
        detail: json!({ "filled": sel, "valueLength": value_len }),
    })
}

fn select(page: &mut Page, action: &Action, sel: &str) -> Result<Effect> {
    let want = action.values.clone();
    let el = find(page, sel, action.index.unwrap_or(0))?;
    if el.value().name() != "select" {
        return Err(ScoutError::invalid(format!(
            "Element <{}> is not a <select>",
            el.value().name()
        ))
        .with_details(json!({ "selector": sel })));
    }
    // Every requested value must exist as an <option>. Substituting another
    // option would silently submit a choice the caller never made.
    let option_sel = selector::compile("option")?;
    let options: Vec<ElementRef> = el.select(&option_sel).collect();
    let have: Vec<String> = options
        .iter()
        .map(|o| o.value().attr("value").unwrap_or(&html::raw_text(&**o)).to_string())
        .collect();
    let missing: Vec<String> = want.iter().filter(|v| !have.contains(v)).cloned().collect();
    if want.is_empty() || !missing.is_empty() {
        let shown = if missing.is_empty() { want.clone() } else { missing };
        return Err(ScoutError::new(
            ErrorCode::ElementNotFound,
            format!(
                "No <option> with value {}",
                serde_json::to_string(&shown).unwrap_or_default()
            ),
        )
        .with_details(json!({ "selector": sel })));
    }
    // A <select multiple> can hold several values at once; every requested
    // value was verified to exist above, so recording them all is safe. A
    // single-value select still refuses a second value: it would be a choice
    // the page cannot represent.
    if el.value().attr("multiple").is_none() && want.len() > 1 {
        return Err(ScoutError::unsupported(
            "This <select> is not multiple; choose one value per action",
        )
        .with_details(json!({ "selector": sel })));
    }
    record_many(page, &element_selector(&el), want.clone());
    Ok(Effect::None {
        detail: json!({ "selected": sel, "values": want }),
    })
}

fn set_checked(page: &mut Page, action: &Action, sel: &str, checked: bool) -> Result<Effect> {
    let el = find(page, sel, action.index.unwrap_or(0))?;
    let ty = el.value().attr("type").unwrap_or("").to_ascii_lowercase();
    if el.value().name() != "input" || !matches!(ty.as_str(), "checkbox" | "radio") {
        return Err(ScoutError::invalid("Element is not a checkbox or radio input")
            .with_details(json!({ "selector": sel })));
    }
    if !checked && ty == "radio" {
        return Err(ScoutError::unsupported("Radio buttons cannot be unchecked")
            .with_details(json!({ "selector": sel })));
    }
    record(
        page,
        &element_selector(&el),
        if checked { "on".into() } else { String::new() },
    );
    Ok(Effect::None {
        detail: json!({ "checked": checked, "selector": sel }),
    })
}

fn press(page: &mut Page, action: &Action, sel: &str) -> Result<Effect> {
    let key = action.key.clone().unwrap_or_default();
    if !key.eq_ignore_ascii_case("enter") {
        return Err(ScoutError::unsupported(format!(
            "Key \"{key}\" is not supported; only Enter is implemented"
        ))
        .with_details(json!({ "key": key })));
    }
    let el = find(page, sel, action.index.unwrap_or(0))?;
    let form = closest_form(&el).ok_or_else(|| {
        ScoutError::new(
            ErrorCode::CompatibilityError,
            "Pressing Enter outside a form has no effect Scout can reproduce without script",
        )
    })?;
    let submitter = first_submitter_name(&form);
    build_submission(page, &form, submitter.as_deref(), 0, action.timeout_ms)
}

fn ty_of(el: &ElementRef) -> String {
    match el.value().name() {
        "input" => el.value().attr("type").unwrap_or("text").to_ascii_lowercase(),
        other => other.to_string(),
    }
}

fn click(page: &mut Page, action: &Action, sel: &str, cfg: &Config) -> Result<Effect> {
    let el = find(page, sel, action.index.unwrap_or(0))?;
    let tag = el.value().name().to_string();

    // A link: follow it.
    if matches!(tag.as_str(), "a" | "area") {
        if let Some(href) = el.value().attr("href") {
            let base = crate::extract::base_url_of(page);
            if let Some(url) = resolve_url(href, &base) {
                return Ok(Effect::Navigate {
                    url,
                    method: "GET".into(),
                    body: None,
                    content_type: None,
                    timeout_ms: action.timeout_ms,
                });
            }
            return Err(ScoutError::unsupported(format!(
                "Link target \"{href}\" is not an http(s) URL"
            )));
        }
    }

    // A checkbox or radio: clicking toggles it; a radio always ends checked.
    if tag == "input" && matches!(ty_of(&el).as_str(), "checkbox" | "radio") {
        let is_radio = ty_of(&el) == "radio";
        let was_checked = page
            .form_state
            .get(&element_selector(&el))
            .map(|v| recorded_checked(v))
            .unwrap_or_else(|| el.value().attr("checked").is_some());
        return set_checked(page, action, sel, is_radio || !was_checked);
    }

    // A submit control: submit the form it belongs to.
    let ty = el.value().attr("type").unwrap_or("").to_ascii_lowercase();
    let is_submit = (tag == "button" && !matches!(ty.as_str(), "button" | "reset"))
        || (tag == "input" && matches!(ty.as_str(), "submit" | "image"));
    if is_submit {
        if let Some(form) = closest_form(&el) {
            // The activated control contributes its own name/value.
            let submitter = el.value().attr("name").map(|n| n.to_string());
            return build_submission(page, &form, submitter.as_deref(), 0, action.timeout_ms);
        }
    }

    Err(ScoutError::unsupported(format!(
        "Clicking <{tag}> would require JavaScript event handling, which Scout does not run"
    ))
    .with_details(json!({ "selector": sel, "tag": tag, "maxNavTimeoutMs": cfg.max_nav_timeout_ms })))
}

fn submit_form(
    page: &mut Page,
    action: &Action,
    sel: &str,
    _cfg: &Config,
) -> Result<Effect> {
    let index = action.index.unwrap_or(0);
    let el = find(page, sel, index)?;
    let tag = el.value().name().to_string();
    let form = if tag == "form" { el } else { closest_form(&el).ok_or_else(|| {
        ScoutError::new(ErrorCode::ElementNotFound, "Element is not inside a form")
    })? };
    build_submission(page, &form, None, 0, action.timeout_ms)
}

fn closest_form<'a>(el: &ElementRef<'a>) -> Option<ElementRef<'a>> {
    let mut node = Some(*el);
    while let Some(n) = node {
        if n.value().name() == "form" {
            return Some(n);
        }
        node = n.parent().and_then(ElementRef::wrap);
    }
    None
}

const MAX_FIELDS: usize = 500;

/// The control a browser would treat as the implicit submitter when Enter is
/// pressed: the first <button> (default type submit) or submit/image input.
fn first_submitter_name(form: &ElementRef) -> Option<String> {
    let sel = selector::compile("button, input").ok()?;
    for el in form.select(&sel) {
        let tag = el.value().name();
        let ty = el.value().attr("type").unwrap_or("").to_ascii_lowercase();
        let is_submitter = (tag == "button" && (ty.is_empty() || ty == "submit"))
            || (tag == "input" && matches!(ty.as_str(), "submit" | "image"));
        if is_submitter {
            if let Some(n) = el.value().attr("name") {
                if !n.is_empty() {
                    return Some(n.to_string());
                }
            }
        }
    }
    None
}

/// Collect a form's fields and build the request a browser would send.
///
/// Mirrors the published `collectFormData`: only the activated control
/// contributes its name/value, a checkbox or radio contributes its `value`
/// attribute (or "on") only when the page's state says it is checked, disabled
/// controls are skipped, and a select submits its chosen option values.
fn build_submission(
    page: &Page,
    form: &ElementRef,
    submitter: Option<&str>,
    _depth: usize,
    timeout_ms: Option<u64>,
) -> Result<Effect> {
    let enctype = form
        .value()
        .attr("enctype")
        .unwrap_or("application/x-www-form-urlencoded")
        .to_ascii_lowercase();
    if enctype == "text/plain" {
        return Err(
            ScoutError::unsupported("text/plain form submission is not supported")
                .with_details(json!({ "enctype": enctype })),
        );
    }
    let method = form
        .value()
        .attr("method")
        .unwrap_or("GET")
        .to_ascii_uppercase();

    let mut pairs: Vec<(String, String)> = Vec::new();
    // (field name, canonical selector) for every file input in the form.
    let mut file_fields: Vec<(String, String)> = Vec::new();

    let field_sel = selector::compile("input[name], select[name], textarea[name], button[name]")?;
    for el in form.select(&field_sel).take(MAX_FIELDS) {
        if el.value().attr("disabled").is_some() {
            continue;
        }
        let name = match el.value().attr("name") {
            Some(n) if !n.is_empty() => n.to_string(),
            _ => continue,
        };
        let tag = el.value().name();
        let ty = el.value().attr("type").unwrap_or("").to_ascii_lowercase();
        let canonical = element_selector(&el);

        // Submit controls: only the activated one is part of the submission.
        if (tag == "input" && matches!(ty.as_str(), "submit" | "image" | "button" | "reset"))
            || tag == "button"
        {
            if submitter == Some(name.as_str()) {
                pairs.push((name, el.value().attr("value").unwrap_or("").to_string()));
            }
            continue;
        }
        if tag == "input" && ty == "file" {
            file_fields.push((name, canonical));
            continue;
        }
        if tag == "input" && matches!(ty.as_str(), "checkbox" | "radio") {
            let checked = match page.form_state.get(&canonical) {
                Some(v) => recorded_checked(v),
                None => el.value().attr("checked").is_some(),
            };
            if checked {
                pairs.push((name, el.value().attr("value").unwrap_or("on").to_string()));
            }
            continue;
        }

        match tag {
            "input" => {
                let v = page
                    .form_state
                    .get(&canonical)
                    .and_then(|v| v.first().cloned())
                    .or_else(|| el.value().attr("value").map(|v| v.to_string()))
                    .unwrap_or_default();
                pairs.push((name, v));
            }
            "textarea" => {
                let v = match page.form_state.get(&canonical) {
                    Some(v) => v.first().cloned().unwrap_or_default(),
                    None => {
                        // A newline immediately after <textarea> is dropped by
                        // the HTML parser, so it is not part of the value.
                        let t = html::raw_text(&*el).to_string();
                        t.strip_prefix("\r\n")
                            .or_else(|| t.strip_prefix('\n'))
                            .unwrap_or(&t)
                            .to_string()
                    }
                };
                pairs.push((name, v));
            }
            "select" => {
                let opt_sel = selector::compile("option")?;
                let opts: Vec<ElementRef> = el
                    .select(&opt_sel)
                    .filter(|o| o.value().attr("disabled").is_none())
                    .collect();
                let option_value = |o: &ElementRef| {
                    o.value()
                        .attr("value")
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| html::text(o))
                };
                let multiple = el.value().attr("multiple").is_some();
                let chosen: Vec<String> = match page.form_state.get(&canonical) {
                    Some(want) => opts
                        .iter()
                        .filter(|o| want.contains(&option_value(o)))
                        .map(option_value)
                        .collect(),
                    None => {
                        let selected: Vec<String> = opts
                            .iter()
                            .filter(|o| o.value().attr("selected").is_some())
                            .map(option_value)
                            .collect();
                        if !selected.is_empty() {
                            selected
                        } else if !multiple && !opts.is_empty() {
                            vec![option_value(&opts[0])]
                        } else {
                            Vec::new()
                        }
                    }
                };
                let take: Vec<String> = if multiple {
                    chosen
                } else {
                    chosen.into_iter().take(1).collect()
                };
                for v in take {
                    pairs.push((name.clone(), v));
                }
            }
            _ => {}
        }
    }

    // Files force multipart, exactly like a browser: setting a file on an
    // input upgrades the submission even when the form forgot the enctype.
    let staged: Vec<(&(String, String), Option<&crate::page::StoredFile>)> = file_fields
        .iter()
        .map(|ff| (ff, page.files.get(&ff.1)))
        .collect();
    let use_multipart =
        method == "POST" && (enctype.contains("multipart") || staged.iter().any(|(_, f)| f.is_some()));

    let action_attr = form.value().attr("action").unwrap_or("");
    let base = crate::extract::base_url_of(page);
    let action_url = if action_attr.trim().is_empty() {
        page.url.clone()
    } else {
        resolve_url(action_attr, &base).ok_or_else(|| {
            ScoutError::unsupported("Form action is not an http(s) URL")
                .with_details(json!({ "action": action_attr }))
        })?
    };

    if use_multipart {
        let boundary = multipart_boundary();
        let mut body: Vec<u8> = Vec::new();
        for (k, v) in &pairs {
            body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
            body.extend_from_slice(
                format!("content-disposition: form-data; name=\"{k}\"\r\n\r\n").as_bytes(),
            );
            body.extend_from_slice(v.as_bytes());
            body.extend_from_slice(b"\r\n");
        }
        for (field, staged_file) in &staged {
            let (name, _) = field;
            body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
            match staged_file {
                Some(file) => {
                    body.extend_from_slice(
                        format!(
                            "content-disposition: form-data; name=\"{name}\"; filename=\"{}\"\r\n",
                            file.filename
                        )
                        .as_bytes(),
                    );
                    body.extend_from_slice(
                        b"content-type: application/octet-stream\r\n\r\n",
                    );
                    body.extend_from_slice(&file.bytes);
                }
                // An unfilled file input still gets a part, with an empty
                // name: that is what browsers send.
                None => body.extend_from_slice(
                    format!(
                        "content-disposition: form-data; name=\"{name}\"; filename=\"\"\r\n\r\n"
                    )
                    .as_bytes(),
                ),
            }
            body.extend_from_slice(b"\r\n");
        }
        body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());

        return Ok(Effect::Navigate {
            url: action_url,
            method: "POST".into(),
            body: Some(body),
            content_type: Some(format!("multipart/form-data; boundary={boundary}")),
            timeout_ms,
        });
    }

    let encoded = pairs
        .iter()
        .map(|(k, v)| format!("{}={}", urlencode(k), urlencode(v)))
        .collect::<Vec<_>>()
        .join("&");

    if method == "POST" {
        Ok(Effect::Navigate {
            url: action_url,
            method: "POST".into(),
            body: Some(encoded.into_bytes()),
            content_type: Some("application/x-www-form-urlencoded".into()),
            timeout_ms,
        })
    } else {
        let url = if encoded.is_empty() {
            action_url
        } else if action_url.contains('?') {
            format!("{action_url}&{encoded}")
        } else {
            format!("{action_url}?{encoded}")
        };
        Ok(Effect::Navigate {
            url,
            method: "GET".into(),
            body: None,
            content_type: None,
            timeout_ms,
        })
    }
}

/// A boundary that cannot appear in any part: random, like a browser's.
fn multipart_boundary() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let n = nanos ^ (SEQ.fetch_add(1, Ordering::Relaxed) << 48);
    format!("----scout{n:016x}")
}


/// A stable key for an element, used to look up recorded form values.
/// Prefers `name`, then `id`, then a tag+index path.
fn element_selector(el: &ElementRef) -> String {
    if let Some(name) = el.value().attr("name") {
        if !name.is_empty() {
            return format!("[name=\"{}\"]", name.replace('"', "\\\""));
        }
    }
    if let Some(id) = el.value().id() {
        if !id.is_empty() {
            return format!("#{id}");
        }
    }
    el.value().name().to_string()
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            b' ' => out.push('+'),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// Render an element for diagnostics without leaking a full document.
pub fn describe(el: &ElementRef) -> Value {
    json!({
        "tag": el.value().name(),
        "id": el.value().id(),
        "textLength": html::text(&**el).len(),
        "childElements": el.children().filter(|c| matches!(c.value(), Node::Element(_))).count(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page_with(html_src: &str) -> Page {
        let doc = crate::html::parse_html(html_src, 150_000).unwrap();
        Page::html(
            "https://example.com/".into(),
            200,
            "text/html".into(),
            html_src.len(),
            doc,
            Vec::new(),
            Vec::new(),
        )
    }

    fn cfg() -> Config {
        Config::dev()
    }

    #[test]
    fn fill_records_a_value() {
        let mut p = page_with("<form><input name='q'></form>");
        let a = Action::from_json(&json!({ "type": "fill", "selector": "input[name=q]", "value": "hello" })).unwrap();
        let e = apply(&mut p, &a, &cfg()).unwrap();
        assert!(matches!(e, Effect::None { .. }));
        assert_eq!(
            p.form_state.get("[name=\"q\"]").and_then(|v| v.first()).map(|s| s.as_str()),
            Some("hello")
        );
    }

    #[test]
    fn multi_select_records_and_submits_every_value() {
        let mut p = page_with(
            "<form action='/pick' method='get'>\
             <select name='toppings' multiple>\
             <option value='cheese'>cheese</option>\
             <option value='mushrooms'>mushrooms</option>\
             <option value='olives'>olives</option>\
             </select>\
             <button type='submit'>go</button>\
             </form>",
        );
        let a = Action::from_json(&json!({
            "type": "select",
            "selector": "select[name=toppings]",
            "value": ["cheese", "olives"]
        }))
        .unwrap();
        let e = apply(&mut p, &a, &cfg()).unwrap();
        assert!(matches!(e, Effect::None { .. }));
        let stored = p.form_state.get("[name=\"toppings\"]").unwrap();
        assert_eq!(stored, &vec!["cheese".to_string(), "olives".to_string()]);

        // Submitting must carry both values, in the recorded order.
        let submit = Action::from_json(&json!({
            "type": "submit", "selector": "form"
        }))
        .unwrap();
        match apply(&mut p, &submit, &cfg()).unwrap() {
            Effect::Navigate { url, .. } => {
                assert!(url.contains("toppings=cheese"), "url was {url}");
                assert!(url.contains("toppings=olives"), "url was {url}");
            }
            other => panic!("expected navigate, got {other:?}"),
        }
    }

    #[test]
    fn single_select_still_refuses_multiple_values() {
        let mut p = page_with(
            "<form><select name='one'><option value='a'>a</option><option value='b'>b</option></select></form>",
        );
        let a = Action::from_json(&json!({
            "type": "select", "selector": "select[name=one]", "value": ["a", "b"]
        }))
        .unwrap();
        let err = apply(&mut p, &a, &cfg()).unwrap_err();
        assert_eq!(err.code, crate::error::ErrorCode::UnsupportedCapability);
    }

    #[test]
    fn missing_element_is_element_not_found() {
        let mut p = page_with("<form><input name='q'></form>");
        let a = Action::from_json(&json!({ "type": "fill", "selector": "input[name=zzz]", "value": "x" })).unwrap();
        let err = apply(&mut p, &a, &cfg()).unwrap_err();
        assert_eq!(err.code, ErrorCode::ElementNotFound);
    }

    #[test]
    fn file_inputs_are_refused_not_ignored() {
        let mut p = page_with("<form><input type='file' name='f'></form>");
        let a = Action::from_json(&json!({ "type": "fill", "selector": "input[name=f]", "value": "/etc/passwd" })).unwrap();
        let err = apply(&mut p, &a, &cfg()).unwrap_err();
        assert_eq!(err.code, ErrorCode::UnsupportedCapability);
    }

    #[test]
    fn scroll_is_unsupported_by_name() {
        let mut p = page_with("<div id='x'></div>");
        let a = Action::from_json(&json!({ "type": "scroll", "selector": "#x" })).unwrap();
        let err = apply(&mut p, &a, &cfg()).unwrap_err();
        assert_eq!(err.code, ErrorCode::UnsupportedCapability);
    }

    #[test]
    fn keys_other_than_enter_are_refused_by_name() {
        // Keyboard input is Enter-only today. Naming the key in the error keeps
        // this a declared limit rather than a generic failure.
        let mut p = page_with("<form action='/s'><input name='q'></form>");
        let a = Action::from_json(
            &json!({ "type": "press", "selector": "input[name=q]", "key": "Tab" }),
        )
        .unwrap();
        let err = apply(&mut p, &a, &cfg()).unwrap_err();
        assert_eq!(err.code, ErrorCode::UnsupportedCapability);
        assert!(err.message.contains("Tab"), "error should name the key: {}", err.message);
    }

    #[test]
    fn enter_submits_a_get_form_with_query() {
        let mut p = page_with(
            "<form action='/search' method='GET'><input name='q'></form>",
        );
        let a = Action::from_json(&json!({ "type": "fill", "selector": "input[name=q]", "value": "rust lang" })).unwrap();
        apply(&mut p, &a, &cfg()).unwrap();
        let enter = Action::from_json(&json!({ "type": "press", "selector": "input[name=q]", "key": "Enter" })).unwrap();
        match apply(&mut p, &enter, &cfg()).unwrap() {
            Effect::Navigate { url, method, body, .. } => {
                assert_eq!(method, "GET");
                assert!(body.is_none());
                assert_eq!(url, "https://example.com/search?q=rust+lang");
            }
            other => panic!("expected navigate, got {other:?}"),
        }
    }

    #[test]
    fn post_form_produces_an_urlencoded_body() {
        let mut p = page_with("<form action='/login' method='POST'><input name='u'><input name='p' type='password'></form>");
        for (sel, val) in [("input[name=u]", "alice"), ("input[name=p]", "s3cr3t xy")] {
            let a = Action::from_json(&json!({ "type": "fill", "selector": sel, "value": val })).unwrap();
            apply(&mut p, &a, &cfg()).unwrap();
        }
        let submit = Action::from_json(&json!({ "type": "submit", "selector": "form" })).unwrap();
        match apply(&mut p, &submit, &cfg()).unwrap() {
            Effect::Navigate { url, method, body, content_type, .. } => {
                assert_eq!(method, "POST");
                assert_eq!(url, "https://example.com/login");
                let body = String::from_utf8(body.unwrap()).unwrap();
                assert!(body.contains("u=alice"));
                assert!(body.contains("p=s3cr3t+xy"));
                assert_eq!(content_type.as_deref(), Some("application/x-www-form-urlencoded"));
            }
            other => panic!("expected navigate, got {other:?}"),
        }
    }

    #[test]
    fn multipart_forms_submit_as_multipart_with_empty_file_parts() {
        let mut p = page_with(
            "<form action='/u' method='POST' enctype='multipart/form-data'>\
             <input name='a' value='hello'><input type='file' name='f'></form>",
        );
        let fill = Action::from_json(&json!({ "type": "fill", "selector": "input[name=a]", "value": "hello" })).unwrap();
        apply(&mut p, &fill, &cfg()).unwrap();
        let submit = Action::from_json(&json!({ "type": "submit", "selector": "form" })).unwrap();
        match apply(&mut p, &submit, &cfg()).unwrap() {
            Effect::Navigate { url, method, body, content_type, .. } => {
                assert_eq!(method, "POST");
                assert_eq!(url, "https://example.com/u");
                let ct = content_type.unwrap();
                assert!(ct.starts_with("multipart/form-data; boundary="), "ct was {ct}");
                let boundary = ct.split("boundary=").nth(1).unwrap();
                let body = String::from_utf8(body.unwrap()).unwrap();
                // Regular field as a part.
                assert!(
                    body.contains(&format!("content-disposition: form-data; name=\"a\"\r\n\r\nhello\r\n")),
                    "body was {body}"
                );
                // An unfilled file input still sends its part, empty, like a browser.
                assert!(
                    body.contains("content-disposition: form-data; name=\"f\"; filename=\"\"\r\n\r\n\r\n"),
                    "body was {body}"
                );
                assert!(body.ends_with(&format!("--{boundary}--\r\n")));
            }
            other => panic!("expected navigate, got {other:?}"),
        }
    }

    #[test]
    fn staged_files_submit_as_multipart_even_without_enctype() {
        let mut p = page_with(
            "<form action='/u' method='POST'><input name='a'><input type='file' name='f'></form>",
        );
        let upload = Action::from_json(&json!({
            "type": "upload", "selector": "input[name=f]",
            "value": "report.bin", "contentBase64": "AP8Q"
        }))
        .unwrap();
        match apply(&mut p, &upload, &cfg()).unwrap() {
            Effect::None { detail } => {
                assert_eq!(detail["sizeBytes"], 3);
                assert_eq!(detail["fileName"], "report.bin");
            }
            other => panic!("expected none, got {other:?}"),
        }
        let submit = Action::from_json(&json!({ "type": "submit", "selector": "form" })).unwrap();
        match apply(&mut p, &submit, &cfg()).unwrap() {
            Effect::Navigate { body, content_type, .. } => {
                let ct = content_type.unwrap();
                assert!(ct.starts_with("multipart/form-data; boundary="), "ct was {ct}");
                let body = body.unwrap();
                let text = String::from_utf8_lossy(&body);
                assert!(
                    text.contains("content-disposition: form-data; name=\"f\"; filename=\"report.bin\"\r\n"),
                    "body was {text}"
                );
                assert!(text.contains("content-type: application/octet-stream\r\n\r\n"));
                // The exact staged bytes, including non-UTF-8 ones.
                let needle = b"content-type: application/octet-stream\r\n\r\n";
                let at = body
                    .windows(needle.len())
                    .position(|w| w == needle)
                    .expect("file part header");
                assert_eq!(&body[at + needle.len()..at + needle.len() + 3], &[0x00, 0xFF, 0x10]);
            }
            other => panic!("expected navigate, got {other:?}"),
        }
    }

    #[test]
    fn upload_refuses_non_file_inputs_and_bad_base64() {
        let mut p = page_with("<form><input name='a'></form>");
        let wrong = Action::from_json(&json!({
            "type": "upload", "selector": "input[name=a]",
            "value": "x.txt", "contentBase64": "aGk="
        }))
        .unwrap();
        let err = apply(&mut p, &wrong, &cfg()).unwrap_err();
        assert_eq!(err.code, ErrorCode::UnsupportedCapability);

        let mut p = page_with("<form><input type='file' name='f'></form>");
        let bad = Action::from_json(&json!({
            "type": "upload", "selector": "input[name=f]",
            "value": "x.txt", "contentBase64": "not base64 !!"
        }))
        .unwrap();
        let err = apply(&mut p, &bad, &cfg()).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidRequest);

        let mut p = page_with("<form><input type='file' name='f'></form>");
        let err = Action::from_json(&json!({
            "type": "upload", "selector": "input[name=f]",
            "value": "../etc/passwd", "contentBase64": "aGk="
        }))
        .unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidRequest);
    }

    #[test]
    fn clicking_a_link_yields_navigation() {
        let mut p = page_with("<a href='/next' id='n'>next</a>");
        let a = Action::from_json(&json!({ "type": "click", "selector": "#n" })).unwrap();
        match apply(&mut p, &a, &cfg()).unwrap() {
            Effect::Navigate { url, method, .. } => {
                assert_eq!(url, "https://example.com/next");
                assert_eq!(method, "GET");
            }
            other => panic!("expected navigate, got {other:?}"),
        }
    }

    #[test]
    fn clicking_a_plain_div_is_honestly_unsupported() {
        let mut p = page_with("<div id='d'>x</div>");
        let a = Action::from_json(&json!({ "type": "click", "selector": "#d" })).unwrap();
        let err = apply(&mut p, &a, &cfg()).unwrap_err();
        assert_eq!(err.code, ErrorCode::UnsupportedCapability);
    }

    #[test]
    fn unchecked_checkboxes_are_not_submitted() {
        let p = page_with("<form action='/f'><input type='checkbox' name='c'><input name='a' value='1'></form>");
        let form = {
            let sel = selector::compile("form").unwrap();
            selector::select_one(p.doc.as_ref().unwrap(), &sel).unwrap()
        };
        match build_submission(&p, &form, None, 0, None).unwrap() {
            Effect::Navigate { url, .. } => {
                assert!(!url.contains("c="), "unchecked box must be omitted: {url}");
                assert!(url.contains("a=1"));
            }
            other => panic!("expected navigate, got {other:?}"),
        }
    }

    /// The activated control contributes `name=value`; other submit controls
    /// in the same form do not.
    #[test]
    fn only_the_activated_submit_control_is_submitted() {
        let mut p = page_with(
            "<form action='/f' method='post'>\
             <button type='submit' name='go' value='1'>Go</button>\
             <button type='submit' name='other' value='nope'>Other</button></form>",
        );
        let click = Action::from_json(&json!({ "type": "click", "selector": "button[name=go]" })).unwrap();
        match apply(&mut p, &click, &cfg()).unwrap() {
            Effect::Navigate { body, .. } => {
                let body = String::from_utf8(body.unwrap()).unwrap();
                assert!(body.contains("go=1"), "the clicked button submits: {body}");
                assert!(!body.contains("other"), "an unactivated button must not: {body}");
            }
            other => panic!("expected navigate, got {other:?}"),
        }
    }

    /// A checkbox checked by an interaction carries its value attribute.
    #[test]
    fn a_checked_box_submits_its_value_attribute() {
        let mut p = page_with(
            "<form action='/f' method='post'><input type='checkbox' name='agree' value='yes'>\
             <button type='submit' name='go' value='1'>Go</button></form>",
        );
        let check = Action::from_json(&json!({ "type": "check", "selector": "[name=agree]" })).unwrap();
        apply(&mut p, &check, &cfg()).unwrap();
        let submit = Action::from_json(&json!({ "type": "click", "selector": "button[name=go]" })).unwrap();
        match apply(&mut p, &submit, &cfg()).unwrap() {
            Effect::Navigate { body, .. } => {
                let body = String::from_utf8(body.unwrap()).unwrap();
                assert!(body.contains("agree=yes"), "checked box submits its value: {body}");
            }
            other => panic!("expected navigate, got {other:?}"),
        }
    }

    /// A select whose requested value is not an <option> must not silently
    /// submit a different choice.
    #[test]
    fn select_refuses_a_value_that_is_not_an_option() {
        let mut p = page_with(
            "<form action='/f'><select name='color'><option value='red'>Red</option>\
             <option value='blue'>Blue</option></select></form>",
        );
        let bad = Action::from_json(&json!({ "type": "select", "selector": "select", "value": "green" })).unwrap();
        let err = apply(&mut p, &bad, &cfg()).unwrap_err();
        assert_eq!(err.code, ErrorCode::ElementNotFound);
        assert!(err.message.contains("green"), "got: {}", err.message);

        let good = Action::from_json(&json!({ "type": "select", "selector": "select", "value": "blue" })).unwrap();
        apply(&mut p, &good, &cfg()).unwrap();
    }

    /// `index` picks among matches, as the published contract says.
    #[test]
    fn index_selects_among_matches() {
        let mut p = page_with("<a href='/one'>1</a><a href='/two'>2</a>");
        let a = Action::from_json(&json!({ "type": "click", "selector": "a", "index": 1 })).unwrap();
        match apply(&mut p, &a, &cfg()).unwrap() {
            Effect::Navigate { url, .. } => assert_eq!(url, "https://example.com/two"),
            other => panic!("expected navigate, got {other:?}"),
        }
        let beyond = Action::from_json(&json!({ "type": "click", "selector": "a", "index": 5 })).unwrap();
        let err = apply(&mut p, &beyond, &cfg()).unwrap_err();
        assert_eq!(err.code, ErrorCode::ElementNotFound);
    }

    #[test]
    fn uncheck_on_a_radio_is_explicitly_unsupported() {
        let mut p = page_with("<input type='radio' name='r' value='x'>");
        let a = Action::from_json(&json!({ "type": "uncheck", "selector": "input" })).unwrap();
        let err = apply(&mut p, &a, &cfg()).unwrap_err();
        assert_eq!(err.code, ErrorCode::UnsupportedCapability);
        assert!(err.message.contains("Radio"), "got: {}", err.message);
    }

    #[test]
    fn unknown_action_type_is_a_caller_error() {
        // Node's ACTIONS list is closed: anything outside it is invalid_request.
        let err = Action::from_json(&json!({ "type": "teleport" })).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidRequest);
        assert!(err.message.contains("action.type must be one of"), "got: {}", err.message);
    }

    #[test]
    fn scroll_is_a_declared_but_unsupported_capability() {
        let mut p = page_with("<div></div>");
        let a = Action::from_json(&json!({ "type": "scroll" })).unwrap();
        let err = apply(&mut p, &a, &cfg()).unwrap_err();
        assert_eq!(err.code, ErrorCode::UnsupportedCapability);
    }

    #[test]
    fn action_shapes_follow_parse_action() {
        // selector is required except for scroll, and index/key/timeoutMs are bounded.
        assert_eq!(
            Action::from_json(&json!({ "type": "click" })).unwrap_err().code,
            ErrorCode::InvalidRequest
        );
        assert!(Action::from_json(&json!({ "type": "select", "selector": "select" })).is_err());
        let a = Action::from_json(&json!({ "type": "select", "selector": "select", "value": ["blue", "red"] })).unwrap();
        assert_eq!(a.values, vec!["blue".to_string(), "red".to_string()]);
        let a = Action::from_json(&json!({ "type": "fill", "selector": "input", "value": "x" })).unwrap();
        assert_eq!(a.values, vec!["x".to_string()]);
        assert!(Action::from_json(&json!({ "type": "click", "selector": "a", "index": 5000 })).is_err());
        assert!(Action::from_json(&json!({ "type": "click", "selector": "a", "checked": true })).is_err());
        assert!(Action::from_json(&json!({ "type": "click", "selector": "a[href='x" })).is_err());
    }
}
