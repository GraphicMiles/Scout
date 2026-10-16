//! Embedded JavaScript execution (QuickJS via `rquickjs`).
//!
//! ## Scope of this increment
//!
//! The engine is real: scripts run in QuickJS with a memory cap, a stack cap and
//! an interrupt-based wall-clock timeout, and `console` output is captured.
//!
//! The browser compatibility layer is a *first* increment. Before a script runs,
//! the parsed document is injected as a JSON tree, and a read-only DOM API is
//! implemented over it in JavaScript. That gives scripts a genuine view of the
//! page (selectors, text, attributes, traversal) without hand-writing a DOM in
//! Rust.
//!
//! What it deliberately does **not** do yet: no layout, no style resolution, no
//! mutation of the real document, no event loop, and no network from inside the
//! sandbox (`fetch`/`XMLHttpRequest` are absent rather than stubbed with a lie).
//! A page whose content is produced by such APIs still reports
//! `js_required_likely` rather than returning fabricated data.

use crate::error::{ErrorCode, Result, ScoutError};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

/// A DOM snapshot handed to the sandbox.
pub fn dom_snapshot(doc: &scraper::Html) -> Value {
    use scraper::Node;
    use serde_json::Map;

    fn walk(node: &ego_tree::NodeRef<Node>, budget: &mut usize) -> Value {
        if *budget == 0 {
            return Value::Null;
        }
        match node.value() {
            Node::Text(t) => {
                *budget -= 1;
                json!({ "t": 3, "v": t.text.to_string() })
            }
            Node::Element(e) => {
                *budget -= 1;
                let mut attrs = Map::new();
                for (k, v) in e.attrs() {
                    attrs.insert(k.to_string(), json!(v));
                }
                let mut children = Vec::new();
                for c in node.children() {
                    let cv = walk(&c, budget);
                    if !cv.is_null() {
                        children.push(cv);
                    }
                }
                json!({
                    "t": 1,
                    "n": e.name(),
                    "a": Value::Object(attrs),
                    "c": children,
                })
            }
            _ => {
                // Document/fragment: descend, but emit nothing for the node itself.
                let mut children = Vec::new();
                for c in node.children() {
                    let cv = walk(&c, budget);
                    if !cv.is_null() {
                        children.push(cv);
                    }
                }
                if children.is_empty() {
                    Value::Null
                } else {
                    json!({ "t": 1, "n": "#root", "a": {}, "c": children })
                }
            }
        }
    }

    let mut budget = 20_000usize;
    walk(&doc.tree.root(), &mut budget)
}

/// The read-only DOM surface exposed to scripts, implemented in JavaScript on
/// top of the injected tree.
const DOM_SHIM: &str = r##"
(function () {
  var root = globalThis.__scout_dom || { t: 1, n: "#root", a: {}, c: [] };

  /* ---------------- selector engine (JS side) ----------------
     Supports: groups (,), descendant and child combinators, and compound
     selectors of tag / #id / .class / [attr] with =, ~=, |=, ^=, $=, *=.
     Anything it cannot parse returns no matches rather than matching wrongly,
     and the host-side engine remains available for exact CSS semantics. */

  function parseCompound(text) {
    var c = { tag: null, id: null, classes: [], attrs: [] };
    var re = /([#.]?[A-Za-z0-9_\-\u00a0-\uffff]+|\*)|\[([^\]]*)\]/g;
    var m, any = false;
    while ((m = re.exec(text)) !== null) {
      any = true;
      if (m[2] !== undefined) {
        var body = m[2], op = null, val = null;
        var am = /^([\w:.\-]+)\s*(?:([~|^$*]?=)\s*"?([^"\]]*)"?)?\s*$/.exec(body);
        if (!am) return null;
        if (am[2]) { op = am[2]; val = am[3]; }
        c.attrs.push([am[1], op, val]);
      } else {
        var tok = m[1];
        // A bare `*` is the universal selector: it constrains nothing, but it
        // must not abort the parse (leaving c.tag null matches any element).
        if (tok === '*') continue;
        if (tok.charAt(0) === '#') c.id = tok.slice(1);
        else if (tok.charAt(0) === '.') c.classes.push(tok.slice(1));
        else c.tag = tok.toLowerCase();
      }
    }
    return any ? c : null;
  }

  function parseSelector(sel) {
    var groups = [];
    var rawGroups = String(sel).split(',');
    for (var g = 0; g < rawGroups.length; g++) {
      var text = rawGroups[g].trim();
      if (!text) return null;
      var parts = [], buf = '', comb = null, pendingComb = null;
      for (var i = 0; i < text.length; i++) {
        var ch = text.charAt(i);
        if (ch === ' ' || ch === '\t' || ch === '\n') {
          if (buf) { parts.push({ comb: pendingComb || (parts.length ? ' ' : null), text: buf }); buf = ''; pendingComb = null; }
          continue;
        }
        if (ch === '>') {
          if (buf) { parts.push({ comb: pendingComb || (parts.length ? ' ' : null), text: buf }); buf = ''; }
          pendingComb = '>';
          continue;
        }
        buf += ch;
      }
      if (buf) parts.push({ comb: pendingComb || (parts.length ? ' ' : null), text: buf });
      if (!parts.length) return null;
      var compiled = [];
      for (var p = 0; p < parts.length; p++) {
        var comp = parseCompound(parts[p].text);
        if (!comp) return null;
        compiled.push({ comb: parts[p].comb, compound: comp });
      }
      groups.push(compiled);
    }
    return groups.length ? groups : null;
  }

  function matchCompound(node, c) {
    if (node.t !== 1) return false;
    var name = (node.n || '').toLowerCase();
    var attrs = node.a || {};
    if (c.tag && c.tag !== '*' && name !== c.tag) return false;
    if (c.id !== null && attrs.id !== c.id) return false;
    for (var i = 0; i < c.classes.length; i++) {
      var cl = (attrs.class || '').split(/\s+/);
      if (cl.indexOf(c.classes[i]) < 0) return false;
    }
    for (var a = 0; a < c.attrs.length; a++) {
      var spec = c.attrs[a], nm = spec[0], op = spec[1], want = spec[2];
      if (!Object.prototype.hasOwnProperty.call(attrs, nm)) return false;
      if (!op) continue;
      var have = String(attrs[nm]);
      want = String(want == null ? '' : want);
      if (op === '=' && have !== want) return false;
      if (op === '~=' && !want) return false;
      if (op === '~=' && have.split(/\s+/).indexOf(want) < 0) return false;
      if (op === '|=' && have !== want && have.indexOf(want + '-') !== 0) return false;
      if (op === '^=' && (!want || have.indexOf(want) !== 0)) return false;
      if (op === '$=' && (!want || have.slice(-want.length) !== want)) return false;
      if (op === '*=' && (!want || have.indexOf(want) < 0)) return false;
    }
    return true;
  }

  function matchGroup(node, parts, idx) {
    if (!matchCompound(node, parts[idx].compound)) return false;
    if (idx === 0) return true;
    var comb = parts[idx].comb;
    if (comb === '>') {
      var p = node.__parent;
      return !!p && p.t === 1 && matchGroup(p, parts, idx - 1);
    }
    if (comb === ' ') {
      var q = node.__parent;
      while (q && q.t === 1) {
        if (matchGroup(q, parts, idx - 1)) return true;
        q = q.__parent;
      }
      return false;
    }
    return false;
  }

  var SEL_CACHE = {};
  function matches(node, sel) {
    var groups = SEL_CACHE[sel];
    if (groups === undefined) {
      groups = parseSelector(sel);
      SEL_CACHE[sel] = groups;
    }
    if (!groups) return false;
    for (var i = 0; i < groups.length; i++) {
      if (matchGroup(node, groups[i], groups[i].length - 1)) return true;
    }
    return false;
  }

  /* ---------------------------- DOM API ---------------------------- */

  function textOf(node) {
    if (!node) return '';
    if (node.t === 3) return node.v;
    var out = '';
    for (var i = 0; i < (node.c || []).length; i++) out += textOf(node.c[i]);
    return out;
  }

  function collect(node, out) {
    if (!node || node.t !== 1) return;
    out.push(node);
    for (var i = 0; i < (node.c || []).length; i++) collect(node.c[i], out);
  }

  function allElements() {
    var out = [];
    collect(root, out);
    return out;
  }

  function wrap(node) {
    if (!node) return null;
    if (node.__el) return node.__el;
    var attrs = node.a || {};
    var el = {
      tagName: (node.n || '').toUpperCase(),
      nodeName: (node.n || '').toUpperCase(),
      nodeType: 1,
      attributes: attrs,
      children: [],
      parentNode: null,
      __node: node,
      getAttribute: function (n) { return Object.prototype.hasOwnProperty.call(attrs, n) ? attrs[n] : null; },
      hasAttribute: function (n) { return Object.prototype.hasOwnProperty.call(attrs, n); },
      getAttributeNames: function () { return Object.keys(attrs); },
      get id() { return attrs.id || ''; },
      get className() { return attrs.class || ''; },
      get classList() { return (attrs.class || '').split(/\s+/).filter(function (x) { return !!x; }); },
      get textContent() { return textOf(node); },
      get innerText() { return textOf(node); },
      get href() { return attrs.href || ''; },
      get src() { return attrs.src || ''; },
      get srcset() { return attrs.srcset || ''; },
      get title() { return attrs.title || ''; },
      get value() { return attrs.value || ''; },
      get name() { return attrs.name || ''; },
      get datetime() { return attrs.datetime || ''; },
      get content() { return attrs.content || ''; },
      matches: function (s) { return matches(node, s); },
      closest: function (s) {
        var q = node;
        while (q && q.t === 1) { if (matches(q, s)) return wrap(q); q = q.__parent; }
        return null;
      },
      contains: function (other) {
        var target = other && other.__node;
        if (!target) return false;
        var q = target;
        while (q) { if (q === node) return true; q = q.__parent; }
        return false;
      },
      querySelector: function (s) {
        var all = [];
        for (var i = 0; i < (node.c || []).length; i++) collect(node.c[i], all);
        for (var j = 0; j < all.length; j++) if (matches(all[j], s)) return wrap(all[j]);
        return null;
      },
      querySelectorAll: function (s) {
        var all = [], res = [];
        for (var i = 0; i < (node.c || []).length; i++) collect(node.c[i], all);
        for (var j = 0; j < all.length; j++) if (matches(all[j], s)) res.push(wrap(all[j]));
        return res;
      },
      getElementsByTagName: function (t) {
        var all = [], res = [];
        collect(node, all);
        for (var j = 0; j < all.length; j++) if ((all[j].n || '').toLowerCase() === String(t).toLowerCase()) res.push(wrap(all[j]));
        return res;
      },
      getElementsByClassName: function (c) {
        var all = [], res = [];
        collect(node, all);
        for (var j = 0; j < all.length; j++) {
          if ((all[j].a.class || '').split(/\s+/).indexOf(c) >= 0) res.push(wrap(all[j]));
        }
        return res;
      },
      appendChild: function () { throw new Error('Scout: DOM mutation is not supported'); },
      removeChild: function () { throw new Error('Scout: DOM mutation is not supported'); },
      setAttribute: function () { throw new Error('Scout: DOM mutation is not supported'); },
      addEventListener: function () {},
      removeEventListener: function () {},
      cloneNode: function () { throw new Error('Scout: DOM mutation is not supported'); }
    };
    node.__el = el;
    return el;
  }

  function wrapText(node) {
    if (node.__el) return node.__el;
    var e = {
      nodeType: 3,
      nodeName: '#text',
      data: node.v,
      nodeValue: node.v,
      textContent: node.v,
      parentNode: null
    };
    node.__el = e;
    return e;
  }

  (function link(node) {
    if (!node || node.t !== 1) return;
    var kids = [], all = [];
    for (var i = 0; i < (node.c || []).length; i++) {
      var c = node.c[i];
      var wc = c.t === 1 ? wrap(c) : (c.t === 3 ? wrapText(c) : null);
      if (!wc) continue;
      c.__parent = node;
      all.push(wc);
      if (c.t === 1) { kids.push(wc); link(c); }
    }
    var w = wrap(node);
    w.children = kids;          // elements only
    w.childNodes = all;         // elements and text, in document order
    w.firstChild = all.length ? all[0] : null;
    w.lastChild = all.length ? all[all.length - 1] : null;
    for (var k = 0; k < all.length; k++) all[k].parentNode = w;
  })(root);

  var rootEl = wrap(root);   // synthetic container holding the whole document

  function firstElementNamed(name) {
    var all = allElements();
    for (var i = 0; i < all.length; i++) {
      if ((all[i].n || '').toLowerCase() === name) return all[i].__el || wrap(all[i]);
    }
    return null;
  }

  var htmlEl = firstElementNamed('html') || rootEl;

  globalThis.document = {
    // The <html> element, not Scout's synthetic container for the document.
    get documentElement() { return htmlEl; },
    // Null when the document genuinely has no such element, as in a browser.
    get body() { return firstElementNamed('body'); },
    get head() { return firstElementNamed('head'); },
    get title() {
      var all = allElements();
      for (var i = 0; i < all.length; i++) {
        if ((all[i].n || '').toLowerCase() === 'title') return textOf(all[i]);
      }
      return '';
    },
    get readyState() { return 'complete'; },
    // Document-level queries root at the container so that they include <html>,
    // matching document.querySelector('html') in a browser.
    querySelector: function (s) { return rootEl.querySelector(s); },
    querySelectorAll: function (s) { return rootEl.querySelectorAll(s); },
    getElementById: function (id) { return rootEl.querySelector('#' + id); },
    getElementsByTagName: function (t) { return rootEl.getElementsByTagName(t); },
    getElementsByClassName: function (c) { return rootEl.getElementsByClassName(c); },
    createElement: function (t) { return { tagName: String(t).toUpperCase(), children: [], attributes: {}, appendChild: function () {} }; },
    addEventListener: function () {},
    removeEventListener: function () {},
    cookie: ''
  };

  globalThis.navigator = {
    userAgent: globalThis.__scout_ua || 'Scout/0.1',
    language: 'en-US',
    languages: ['en-US', 'en']
  };
  globalThis.location = globalThis.location || {
    href: globalThis.__scout_url || '',
    toString: function () { return globalThis.__scout_url || ''; }
  };
  globalThis.window = globalThis;
  globalThis.self = globalThis;
  globalThis.addEventListener = function () {};
  globalThis.removeEventListener = function () {};
  globalThis.setTimeout = function () { throw new Error('Scout: timers are not available in the sandbox'); };
  globalThis.setInterval = function () { throw new Error('Scout: timers are not available in the sandbox'); };
})();
"##;

#[derive(Debug)]
pub struct JsOutcome {
    pub value: Option<Value>,
    pub logs: Vec<String>,
    pub duration_ms: u64,
    pub timed_out: bool,
    /// Why a `None` value is `None`. Empty unless something is worth reporting.
    pub warnings: Vec<Value>,
}

/// Heuristic: does the script contain an explicit `return`?
///
/// A script runs as a function body, so only an explicit `return` yields a value.
/// Knowing whether one was present lets the caller tell "your script computed
/// nothing" apart from "your script never returned" — the latter is silent today
/// and costs callers real debugging time.
///
/// Deliberately permissive: if `return` appears anywhere, including inside a
/// nested function, this stays quiet. A spurious warning on a script that did
/// return would be worse than a missed warning on one that did not.
fn has_return(script: &str) -> bool {
    script
        .split(|c: char| !c.is_alphanumeric() && c != '_' && c != '$')
        .any(|w| w == "return")
}

/// Run a script against a DOM snapshot in a fresh QuickJS context.
///
/// A new runtime per call is deliberate: it guarantees that no state, prototype
/// pollution or lingering global survives between sessions, and it makes the
/// memory cap trivially enforceable. The cost is engine startup per execution.
pub fn run(script: &str, snapshot: &Value, base_url: &str, user_agent: &str, timeout: Duration, max_memory: usize) -> Result<JsOutcome> {
    use rquickjs::{Context, Runtime};

    let started = Instant::now();
    let rt = Runtime::new().map_err(|e| js_err(format!("could not start JS runtime: {e}")))?;
    rt.set_memory_limit(max_memory);
    rt.set_max_stack_size(512 * 1024);

    // Wall-clock interruption: QuickJS polls this from its own interpreter loop,
    // so a tight `while(true){}` is stopped rather than hanging the worker.
    let deadline = started + timeout;
    let timed_out = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = timed_out.clone();
    rt.set_interrupt_handler(Some(Box::new(move || {
        if Instant::now() >= deadline {
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
            return true;
        }
        false
    })));

    let ctx = Context::full(&rt).map_err(|e| js_err(format!("could not create JS context: {e}")))?;

    let prelude = format!(
        "globalThis.__scout_dom = {};\nglobalThis.__scout_url = {};\nglobalThis.__scout_ua = {};\n",
        snapshot,
        serde_json::to_string(base_url).unwrap_or_else(|_| "\"\"".into()),
        serde_json::to_string(user_agent).unwrap_or_else(|_| "\"\"".into()),
    );

    let logs: std::rc::Rc<std::cell::RefCell<Vec<String>>> = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let logs_in = logs.clone();

    let result = ctx.with(|ctx| -> Result<Option<Value>> {
        // console capture
        let console = rquickjs::Object::new(ctx.clone()).map_err(|e| js_err(format!("{e}")))?;
        for level in ["log", "info", "warn", "error", "debug"] {
            let sink = logs_in.clone();
            let lvl = level.to_string();
            let f = rquickjs::Function::new(ctx.clone(), move |args: rquickjs::function::Rest<rquickjs::Value>| {
                let parts: Vec<String> = args.0.iter().map(render_value).collect();
                if let Ok(mut v) = sink.try_borrow_mut() {
                    if v.len() < 500 {
                        v.push(format!("[{lvl}] {}", parts.join(" ")));
                    }
                }
            })
            .map_err(|e| js_err(format!("{e}")))?;
            console.set(level, f).map_err(|e| js_err(format!("{e}")))?;
        }
        ctx.globals().set("console", console).map_err(|e| js_err(format!("{e}")))?;

        ctx.eval::<(), _>(prelude.as_str())
            .map_err(|e| js_err(classify(&e, &timed_out)))?;
        ctx.eval::<(), _>(DOM_SHIM)
            .map_err(|e| js_err(classify(&e, &timed_out)))?;

        let script = format!("(function(){{ {} }})()", script);
        match ctx.eval::<rquickjs::Value, _>(script.as_str()) {
            Ok(v) => {
                if v.is_undefined() || v.is_null() {
                    Ok(None)
                } else {
                    Ok(Some(render_js_value(&v)))
                }
            }
            Err(e) => Err(js_err(classify(&e, &timed_out))),
        }
    })?;

    let duration_ms = started.elapsed().as_millis() as u64;
    let logs = logs.borrow().clone();
    let timed_out = timed_out.load(std::sync::atomic::Ordering::SeqCst);

    // A `None` value is ambiguous: either the script returned nothing, or it
    // never returned. Say which, rather than leaving the caller to guess.
    let mut warnings: Vec<Value> = Vec::new();
    if result.is_none() && !timed_out && !script.trim().is_empty() && !has_return(script) {
        warnings.push(json!({
            "code": "no_return_value",
            "message": "The script produced no value. Scripts run as a function body, so a value is \
                        only returned by an explicit `return` statement; the value of a trailing \
                        expression is discarded."
        }));
    }

    Ok(JsOutcome {
        value: result,
        logs,
        duration_ms,
        timed_out,
        warnings,
    })
}

fn classify(e: &rquickjs::Error, timed_out: &std::sync::atomic::AtomicBool) -> String {
    if timed_out.load(std::sync::atomic::Ordering::SeqCst) {
        return "JavaScript exceeded its time limit".to_string();
    }
    match e {
        rquickjs::Error::Exception => "JavaScript threw an exception".to_string(),
        other => format!("JavaScript error: {other}"),
    }
}

fn js_err(msg: impl Into<String>) -> ScoutError {
    ScoutError::new(ErrorCode::CompatibilityError, msg).with_retryable(false)
}

fn render_value(v: &rquickjs::Value) -> String {
    if v.is_undefined() {
        "undefined".into()
    } else if v.is_null() {
        "null".into()
    } else if let Some(b) = v.as_bool() {
        b.to_string()
    } else if let Some(i) = v.as_int() {
        i.to_string()
    } else if let Some(f) = v.as_float() {
        f.to_string()
    } else if v.is_string() {
        v.as_string().and_then(|s| s.to_string().ok()).unwrap_or_default()
    } else {
        "[object]".into()
    }
}

/// Convert a JS value into JSON. Objects are round-tripped through JSON.stringify
/// so the result is always plain data rather than a live engine handle.
fn render_js_value(v: &rquickjs::Value) -> Value {
    if v.is_null() || v.is_undefined() {
        return Value::Null;
    }
    if let Some(b) = v.as_bool() {
        return json!(b);
    }
    if let Some(i) = v.as_int() {
        return json!(i);
    }
    if let Some(f) = v.as_float() {
        return json!(f);
    }
    if v.is_string() {
        return json!(v.as_string().and_then(|s| s.to_string().ok()).unwrap_or_default());
    }
    // Objects and arrays are round-tripped through JSON.stringify so the caller
    // receives plain data rather than a live engine handle that would tie the
    // result to the context's lifetime.
    let ctx = v.ctx().clone();
    match ctx.json_stringify(v.clone()) {
        Ok(Some(s)) => {
            let raw = s.to_string().unwrap_or_default();
            serde_json::from_str(&raw).unwrap_or(Value::Null)
        }
        _ => Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(html: &str) -> scraper::Html {
        scraper::Html::parse_document(html)
    }

    fn run_on(html: &str, script: &str) -> Result<JsOutcome> {
        let d = doc(html);
        let snap = dom_snapshot(&d);
        run(
            script,
            &snap,
            "https://example.com/",
            "Scout/0.1",
            Duration::from_millis(2_000),
            64 * 1024 * 1024,
        )
    }

    #[test]
    fn executes_and_returns_a_value() {
        let out = run_on("<p>x</p>", "return 6 * 7;").unwrap();
        assert_eq!(out.value, Some(json!(42)));
        assert!(!out.timed_out);
    }

    fn warn_codes(out: &JsOutcome) -> Vec<&str> {
        out.warnings
            .iter()
            .filter_map(|w| w.get("code").and_then(|c| c.as_str()))
            .collect()
    }

    #[test]
    fn a_trailing_expression_without_return_warns_instead_of_failing_silently() {
        // The footgun this warning exists for: `1 + 1` is a valid expression,
        // but as a function body its value is discarded.
        let out = run_on("<p>x</p>", "1 + 1").unwrap();
        assert_eq!(out.value, None);
        assert_eq!(warn_codes(&out), vec!["no_return_value"]);
    }

    #[test]
    fn a_script_that_returns_null_is_not_warned_about() {
        // `null` is a deliberate result, not a missing one.
        let out = run_on("<p>x</p>", "return null;").unwrap();
        assert_eq!(out.value, None);
        assert!(warn_codes(&out).is_empty());
    }

    #[test]
    fn a_successful_return_is_never_warned_about() {
        let out = run_on("<p>x</p>", "return 6 * 7;").unwrap();
        assert_eq!(out.value, Some(json!(42)));
        assert!(warn_codes(&out).is_empty());
    }

    #[test]
    fn the_warning_is_permissive_about_nested_returns() {
        // The value is discarded here too, but `return` appears in the source,
        // so the heuristic stays quiet. A spurious warning is worse than a
        // missed one — this documents that trade-off.
        let out = run_on("<p>x</p>", "function f() { return 1; } f();").unwrap();
        assert!(warn_codes(&out).is_empty());
    }

    #[test]
    fn a_timed_out_script_reports_the_timeout_not_a_missing_value() {
        // A runaway script is an explicit error, not a null value — so there is
        // no ambiguity for the warning to resolve, and none is emitted.
        let err = run_on("<p>x</p>", "while (true) {}").unwrap_err();
        assert_eq!(err.code, ErrorCode::CompatibilityError);
        assert!(err.message.contains("time limit"), "got: {}", err.message);
    }

    #[test]
    fn returns_objects_as_json() {
        let out = run_on("<p>x</p>", "return { a: 1, b: [1,2,3] };").unwrap();
        assert_eq!(out.value, Some(json!({ "a": 1, "b": [1,2,3] })));
    }

    #[test]
    fn captures_console_output() {
        let out = run_on("<p>x</p>", "console.log('hello', 42); return null;").unwrap();
        assert!(out.logs.iter().any(|l| l.contains("hello") && l.contains("42")));
    }

    #[test]
    fn dom_is_readable_from_script() {
        let out = run_on(
            "<html><body><h1>Title</h1><ul><li class='i'>a</li><li class='i'>b</li></ul></body></html>",
            "return document.querySelector('h1').textContent;",
        )
        .unwrap();
        assert_eq!(out.value, Some(json!("Title")));
    }

    #[test]
    fn dom_query_selector_all_counts() {
        let out = run_on(
            "<ul><li class='i'>a</li><li class='i'>b</li><li class='x'>c</li></ul>",
            "return document.querySelectorAll('li.i').length;",
        )
        .unwrap();
        assert_eq!(out.value, Some(json!(2)));
    }

    #[test]
    fn document_title_is_exposed() {
        let out = run_on("<html><head><title>Hi</title></head><body></body></html>", "return document.title;").unwrap();
        assert_eq!(out.value, Some(json!("Hi")));
    }

    /// A tight loop must be interrupted by the wall-clock deadline, not hang.
    #[test]
    fn infinite_loop_is_interrupted() {
        let out = run_on("<p>x</p>", "while(true){}").unwrap_err();
        assert_eq!(out.code, ErrorCode::CompatibilityError);
        assert!(out.message.contains("time limit"), "got: {}", out.message);
    }

    /// Runaway allocation must hit the engine's memory cap instead of the container's.
    #[test]
    fn memory_limit_is_enforced() {
        let d = doc("<p>x</p>");
        let snap = dom_snapshot(&d);
        let err = run(
            "var a=[]; while(true){ a.push(new Array(10000).fill(1)); }",
            &snap,
            "https://example.com/",
            "Scout/0.1",
            Duration::from_millis(5_000),
            8 * 1024 * 1024,
        );
        assert!(err.is_err(), "allocation should have been stopped");
    }

    #[test]
    fn timers_are_reported_as_unavailable() {
        let err = run_on("<p>x</p>", "setTimeout(function(){}, 10);").unwrap_err();
        assert_eq!(err.code, ErrorCode::CompatibilityError);
    }

    #[test]
    fn body_and_head_are_the_real_elements() {
        let out = run_on(
            "<html><head><title>T</title></head><body><p>hi</p></body></html>",
            "return [document.documentElement.tagName, document.head.tagName, document.body.tagName];",
        )
        .unwrap();
        assert_eq!(out.value, Some(json!(["HTML", "HEAD", "BODY"])));
    }

    /// Regression: body used to alias the synthetic document container, so body
    /// text included <title> and <style> content from the head.
    #[test]
    fn body_text_excludes_head_text() {
        let out = run_on(
            "<html><head><title>T</title><style>p{color:red}</style></head><body><p>hi</p></body></html>",
            "return document.body.textContent + '|' + document.head.textContent + '|' + document.body.querySelector('p').tagName;",
        )
        .unwrap();
        assert_eq!(out.value, Some(json!("hi|Tp{color:red}|P")));
    }

    /// Regression: the universal selector failed to parse, so `*` matched nothing.
    #[test]
    fn universal_selector_matches_every_element() {
        let out = run_on(
            "<div><p>a</p><span>b</span></div>",
            "return document.querySelectorAll('*').length + ':' + document.body.querySelectorAll('*').length;",
        )
        .unwrap();
        // Whole document: html, head, body, div, p, span. Inside body: div, p, span.
        assert_eq!(out.value, Some(json!("6:3")));
    }

    #[test]
    fn child_nodes_include_text_and_children_excludes_it() {
        let out = run_on(
            "<body><p>hi</p></body>",
            "var p = document.body.querySelector('p'); return p.childNodes.length + ':' + p.children.length + ':' + p.firstChild.nodeType + ':' + p.firstChild.data;",
        )
        .unwrap();
        assert_eq!(out.value, Some(json!("1:0:3:hi")));
    }

    #[test]
    fn document_queries_include_the_html_element() {
        let out = run_on(
            "<p>x</p>",
            "return document.querySelectorAll('html').length + ':' + document.getElementsByTagName('html').length;",
        )
        .unwrap();
        assert_eq!(out.value, Some(json!("1:1")));
    }

    #[test]
    fn snapshot_prunes_pathological_trees() {
        let big = format!("<div>{}</div>", "<span>x</span>".repeat(30_000));
        let d = doc(&big);
        let snap = dom_snapshot(&d);
        let s = serde_json::to_string(&snap).unwrap();
        assert!(s.len() < 20_000_000, "snapshot should be bounded");
    }
}
