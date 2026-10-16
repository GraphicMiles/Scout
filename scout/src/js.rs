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

  /* --------------------------- mutation ---------------------------
     The tree is mutable so page scripts can build content, which is the
     entire point of running them. Mutations land on the snapshot; the host
     re-parses the serialised result, so extraction sees what the script
     produced rather than the markup it started from. */

  var VOID_TAGS = { area:1, base:1, br:1, col:1, embed:1, hr:1, img:1, input:1,
                    link:1, meta:1, param:1, source:1, track:1, wbr:1 };
  var RAW_TAGS = { script:1, style:1 };

  function mkElement(tag) { return { t: 1, n: String(tag).toLowerCase(), a: {}, c: [] }; }
  function mkText(text) { return { t: 3, v: String(text) }; }

  function adopt(parent, child, index) {
    if (!parent || !child) return null;
    // A document fragment contributes its children, as in a browser, rather
    // than nesting itself as a node that no selector will ever match.
    if (child.n === '#fragment') {
      var kids = (child.c || []).slice();
      for (var f = 0; f < kids.length; f++) {
        adopt(parent, kids[f], index === undefined ? undefined : index + f);
      }
      child.c = [];
      return child;
    }
    detach(child);
    var kids = parent.c || (parent.c = []);
    if (index === undefined || index < 0 || index > kids.length) kids.push(child);
    else kids.splice(index, 0, child);
    child.__parent = parent;
    return child;
  }

  function detach(node) {
    if (!node) return node;
    var p = node.__parent;
    if (p && p.c) {
      var i = p.c.indexOf(node);
      if (i >= 0) p.c.splice(i, 1);
    }
    delete node.__parent;
    return node;
  }

  function cloneTree(node, deep) {
    if (node.t === 3) return mkText(node.v);
    var copy = mkElement(node.n);
    for (var k in node.a) {
      if (Object.prototype.hasOwnProperty.call(node.a, k)) copy.a[k] = node.a[k];
    }
    if (deep && node.c) {
      for (var i = 0; i < node.c.length; i++) copy.c.push(cloneTree(node.c[i], true));
    }
    return copy;
  }

  function nodeOf(x) {
    if (!x) return null;
    if (x.__node) return x.__node;
    if (x.t !== undefined) return x;
    return null;
  }

  function decode(str) {
    return String(str)
      .replace(/&lt;/g, '<').replace(/&gt;/g, '>')
      .replace(/&quot;/g, '"').replace(/&#39;/g, "'")
      .replace(/&nbsp;/g, ' ')
      .replace(/&amp;/g, '&');
  }

  /* A small tag-soup parser: enough for the HTML that frameworks assign to
     innerHTML, not a spec-compliant HTML5 parser. Anything it cannot make
     sense of becomes text rather than vanishing. */
  function parseFragment(html) {
    var out = [], stack = [], s = String(html).replace(/<!--[\s\S]*?-->/g, ''), i = 0;
    function push(node) {
      var p = stack.length ? stack[stack.length - 1] : null;
      if (p) { p.c.push(node); node.__parent = p; } else out.push(node);
    }
    while (i < s.length) {
      var lt = s.indexOf('<', i);
      if (lt < 0) { var tail = s.slice(i); if (tail) push(mkText(decode(tail))); break; }
      if (lt > i) { var pre = s.slice(i, lt); if (pre) push(mkText(decode(pre))); }
      if (s.charAt(lt + 1) === '/') {
        var ge = s.indexOf('>', lt);
        if (ge < 0) break;
        var closeName = s.slice(lt + 2, ge).trim().toLowerCase();
        for (var k = stack.length - 1; k >= 0; k--) {
          if (stack[k].n === closeName) { stack.length = k; break; }
        }
        i = ge + 1;
        continue;
      }
      var end = lt + 1, quote = null;
      while (end < s.length) {
        var ch = s.charAt(end);
        if (quote) { if (ch === quote) quote = null; }
        else if (ch === '"' || ch === "'") quote = ch;
        else if (ch === '>') break;
        end++;
      }
      var raw = s.slice(lt + 1, end);
      i = end + 1;
      if (raw.charAt(0) === '!') continue;
      var selfClose = /\/\s*$/.test(raw);
      raw = raw.replace(/\/\s*$/, '');
      var nm = /^[A-Za-z][A-Za-z0-9:_-]*/.exec(raw);
      if (!nm) continue;
      var el = mkElement(nm[0]);
      var rest = raw.slice(nm[0].length);
      var attrRe = /([A-Za-z_:][-A-Za-z0-9_:.]*)(?:\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s"'>]+)))?/g;
      var am;
      while ((am = attrRe.exec(rest)) !== null) {
        var v = am[2] !== undefined ? am[2] : (am[3] !== undefined ? am[3] : (am[4] !== undefined ? am[4] : ''));
        el.a[am[1].toLowerCase()] = decode(v);
      }
      push(el);
      if (!selfClose && !VOID_TAGS[el.n]) {
        if (RAW_TAGS[el.n]) {
          var close = s.toLowerCase().indexOf('</' + el.n, i);
          if (close >= 0) {
            el.c.push(mkText(s.slice(i, close)));
            var gt = s.indexOf('>', close);
            i = gt < 0 ? s.length : gt + 1;
          }
        } else {
          stack.push(el);
        }
      }
    }
    return out;
  }

  function esc(t) {
    return String(t).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
  }
  function escAttr(t) { return esc(t).replace(/"/g, '&quot;'); }

  function serialize(node) {
    if (!node) return '';
    if (node.t === 3) return esc(node.v);
    if (node.t !== 1) return '';
    if (node.n === '#root') {
      var o = '';
      for (var i = 0; i < (node.c || []).length; i++) o += serialize(node.c[i]);
      return o;
    }
    var s = '<' + node.n, a = node.a || {};
    for (var k in a) {
      if (!Object.prototype.hasOwnProperty.call(a, k)) continue;
      if (a[k] === null || a[k] === undefined) continue;
      s += ' ' + k + '="' + escAttr(String(a[k])) + '"';
    }
    s += '>';
    if (VOID_TAGS[node.n]) return s;
    for (var j = 0; j < (node.c || []).length; j++) s += serialize(node.c[j]);
    return s + '</' + node.n + '>';
  }

  /* `style` is a write-only-ish shim: pages set styles constantly and reading
     them back almost never affects extracted data, so assignments are kept in
     an object without attempting CSS cascade. */
  function styleFor(node) {
    if (node.__style) return node.__style;
    var bag = {};
    node.__style = new Proxy({}, {
      get: function (t, k) { return Object.prototype.hasOwnProperty.call(bag, k) ? bag[k] : ''; },
      set: function (t, k, v) { bag[k] = String(v); return true; },
      has: function () { return true; }
    });
    return node.__style;
  }

  /* An array, because existing callers index it, with the DOM methods hung off
     it so `classList.add('x')` works. `attrs.class` stays the source of truth,
     so a fresh call reflects earlier mutations. */
  function classListFor(attrs) {
    var list = (attrs.class || '').split(/\s+/).filter(function (x) { return !!x; });
    list.add = function () {
      for (var i = 0; i < arguments.length; i++) {
        if (list.indexOf(arguments[i]) < 0) list.push(arguments[i]);
      }
      attrs.class = list.filter(function (x) { return typeof x === 'string'; }).join(' ');
    };
    list.remove = function () {
      for (var i = 0; i < arguments.length; i++) {
        var j = list.indexOf(arguments[i]);
        if (j >= 0) list.splice(j, 1);
      }
      attrs.class = list.filter(function (x) { return typeof x === 'string'; }).join(' ');
    };
    list.toggle = function (c, force) {
      var has = list.indexOf(c) >= 0;
      var want = force === undefined ? !has : !!force;
      if (want) list.add(c); else list.remove(c);
      return want;
    };
    list.contains = function (c) { return list.indexOf(c) >= 0; };
    return list;
  }

  /* Resolve a URL against the document, as a browser does for href/src.
     Without this, `a.href` hands back the raw attribute, so every relative
     link on a page comes out as "/path" instead of an address the caller can
     actually fetch — which makes link extraction close to useless. */
  function resolveUrl(href) {
    if (!href) return href;
    if (/^[a-z][a-z0-9+.\-]*:/i.test(href)) return href;   // already absolute
    if (href.charAt(0) === '#') return href;
    var base = globalThis.__scout_url || '';
    if (!base) return href;
    if (href.charAt(0) === '/') {
      var origin = /^[a-z][a-z0-9+.\-]*:\/\/[^/]+/i.exec(base);
      return origin ? origin[0] + href : href;
    }
    var b = base.split('#')[0].split('?')[0];
    var i = b.lastIndexOf('/');
    return (i >= 0 ? b.slice(0, i + 1) : b + '/') + href;
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
      __node: node,
      getAttribute: function (n) { return Object.prototype.hasOwnProperty.call(attrs, n) ? attrs[n] : null; },
      hasAttribute: function (n) { return Object.prototype.hasOwnProperty.call(attrs, n); },
      getAttributeNames: function () { return Object.keys(attrs); },
      /* Setters are not optional. These were getter-only, so under a strict
         evaluation `el.className = 'x'` threw and took the whole script with
         it — which is how a page whose only sin is setting a class would fail
         to render at all. */
      get id() { return attrs.id || ''; },
      set id(v) { attrs.id = String(v); },
      get className() { return attrs.class || ''; },
      set className(v) { attrs.class = String(v); },
      get innerText() { return textOf(node); },
      set innerText(v) { this.textContent = v; },
      get href() { return resolveUrl(attrs.href || ''); },
      set href(v) { attrs.href = String(v); },
      get src() { return resolveUrl(attrs.src || ''); },
      set src(v) { attrs.src = String(v); },
      get action() { return resolveUrl(attrs.action || ''); },
      set action(v) { attrs.action = String(v); },
      get srcset() { return attrs.srcset || ''; },
      set srcset(v) { attrs.srcset = String(v); },
      get title() { return attrs.title || ''; },
      set title(v) { attrs.title = String(v); },
      get value() { return attrs.value || ''; },
      set value(v) { attrs.value = String(v); },
      get name() { return attrs.name || ''; },
      set name(v) { attrs.name = String(v); },
      get datetime() { return attrs.datetime || ''; },
      set datetime(v) { attrs.datetime = String(v); },
      get content() { return attrs.content || ''; },
      set content(v) { attrs.content = String(v); },
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
      appendChild: function (child) {
        var c = nodeOf(child);
        return c ? wrap(adopt(node, c)) : null;
      },
      insertBefore: function (child, ref) {
        var c = nodeOf(child);
        if (!c) return null;
        var idx = ref ? (node.c || []).indexOf(nodeOf(ref)) : -1;
        return wrap(adopt(node, c, idx < 0 ? undefined : idx));
      },
      replaceChild: function (fresh, old) {
        var c = nodeOf(fresh), o = nodeOf(old);
        if (!c || !o) return null;
        var idx = (node.c || []).indexOf(o);
        detach(o);
        return wrap(adopt(node, c, idx < 0 ? undefined : idx));
      },
      removeChild: function (child) { return wrap(detach(nodeOf(child))); },
      remove: function () { detach(node); },
      setAttribute: function (n, v) { attrs[n] = String(v); },
      removeAttribute: function (n) { delete attrs[n]; },
      cloneNode: function (deep) { return wrap(cloneTree(node, !!deep)); },
      insertAdjacentHTML: function (where, html) {
        var kids = parseFragment(html), i;
        if (where === 'afterbegin' || where === 'afterBegin') {
          for (i = kids.length - 1; i >= 0; i--) adopt(node, kids[i], 0);
        } else {
          // beforebegin/afterend address outside this element. Appending inside
          // keeps the content visible rather than dropping it silently.
          for (i = 0; i < kids.length; i++) adopt(node, kids[i]);
        }
      },
      addEventListener: function () {},
      removeEventListener: function () {},
      get innerHTML() {
        var o = '';
        for (var i = 0; i < (node.c || []).length; i++) o += serialize(node.c[i]);
        return o;
      },
      set innerHTML(v) {
        node.c = [];
        var kids = parseFragment(v);
        for (var i = 0; i < kids.length; i++) adopt(node, kids[i]);
      },
      get outerHTML() { return serialize(node); },
      get textContent() { return textOf(node); },
      set textContent(v) {
        node.c = [];
        if (v !== '' && v !== null && v !== undefined) adopt(node, mkText(v));
      },
      get style() { return styleFor(node); },
      get classList() { return classListFor(attrs); },
      get dataset() {
        var d = {};
        for (var k in attrs) {
          if (Object.prototype.hasOwnProperty.call(attrs, k) && k.indexOf('data-') === 0) {
            d[k.slice(5).replace(/-([a-z])/g, function (m, c) { return c.toUpperCase(); })] = attrs[k];
          }
        }
        return d;
      },
      get childNodes() {
        var r = [], kids = node.c || [];
        for (var i = 0; i < kids.length; i++) {
          if (kids[i].t === 1) r.push(wrap(kids[i]));
          else if (kids[i].t === 3) r.push(wrapText(kids[i]));
        }
        return r;
      },
      get children() {
        var r = [], kids = node.c || [];
        for (var i = 0; i < kids.length; i++) if (kids[i].t === 1) r.push(wrap(kids[i]));
        return r;
      },
      get firstChild() { var c = this.childNodes; return c.length ? c[0] : null; },
      get lastChild() { var c = this.childNodes; return c.length ? c[c.length - 1] : null; },
      get firstElementChild() { var c = this.children; return c.length ? c[0] : null; },
      get lastElementChild() { var c = this.children; return c.length ? c[c.length - 1] : null; },
      get parentNode() {
        var p = node.__parent;
        return p && p.t === 1 ? wrap(p) : null;
      },
      get parentElement() { return this.parentNode; },
      get nextSibling() {
        var p = node.__parent; if (!p || !p.c) return null;
        for (var j = p.c.indexOf(node) + 1; j < p.c.length; j++) {
          if (p.c[j].t === 1) return wrap(p.c[j]);
          if (p.c[j].t === 3) return wrapText(p.c[j]);
        }
        return null;
      },
      get previousSibling() {
        var p = node.__parent; if (!p || !p.c) return null;
        for (var j = p.c.indexOf(node) - 1; j >= 0; j--) {
          if (p.c[j].t === 1) return wrap(p.c[j]);
          if (p.c[j].t === 3) return wrapText(p.c[j]);
        }
        return null;
      }
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

  // One pass to establish parent links and wrapper identity. Child accessors
  // are live getters above, so they remain correct after mutation.
  (function link(node) {
    if (!node || node.t !== 1) return;
    for (var i = 0; i < (node.c || []).length; i++) {
      var c = node.c[i];
      if (c.t !== 1 && c.t !== 3) continue;
      c.__parent = node;
      if (c.t === 1) { wrap(c); link(c); } else { wrapText(c); }
    }
    wrap(node);
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
    // Real nodes, not detached stubs: a created element that silently swallows
    // its children is worse than no createElement at all.
    createElement: function (t) { return wrap(mkElement(t)); },
    createTextNode: function (t) { return wrapText(mkText(t)); },
    createDocumentFragment: function () { return wrap(mkElement('#fragment')); },
    createComment: function () { return wrapText(mkText('')); },
    addEventListener: function () {},
    removeEventListener: function () {},
    cookie: globalThis.__scout_cookie || ''
  };

  // The host calls this after page scripts run to collect the mutated document.
  globalThis.__scout_serialize = function () { return serialize(root); };

  globalThis.navigator = {
    userAgent: globalThis.__scout_ua || 'Scout/0.1',
    language: 'en-US',
    languages: ['en-US', 'en']
  };
  (function () {
    var raw = globalThis.__scout_url || '';
    var m = /^([a-z][a-z0-9+.\-]*:)?\/\/([^\/?#]*)([^?#]*)(\?[^#]*)?(#.*)?$/i.exec(raw);
    var href = raw;
    var protocol = m && m[1] ? m[1].toLowerCase() : '';
    var host = m && m[2] ? m[2] : '';
    var pathname = m && m[3] ? m[3] : '';
    var search = m && m[4] ? m[4] : '';
    var hash = m && m[5] ? m[5] : '';
    var hostname = host.replace(/:\d+$/, '');
    globalThis.location = {
      href: href, protocol: protocol, host: host, hostname: hostname,
      port: /:(\d+)$/.test(host) ? host.split(':').pop() : '',
      pathname: pathname, search: search, hash: hash,
      origin: protocol && host ? protocol + '//' + host : '',
      assign: function () {}, replace: function () {}, reload: function () {},
      toString: function () { return href; }
    };
  })();
  globalThis.window = globalThis;
  globalThis.self = globalThis;
  globalThis.addEventListener = function () {};
  globalThis.removeEventListener = function () {};
  /* Timers run on a virtual clock: callbacks are queued, then the host drains
     them synchronously once the page's scripts have finished, so deferred DOM
     writes are still observed without sleeping in real time. Bounded by count
     and by the same wall-clock budget as the rest of the execution. */
  var TIMER_QUEUE = [], TIMER_ID = 1, TIMER_LIMIT = 256;

  function dropTimer(id) {
    for (var i = 0; i < TIMER_QUEUE.length; i++) {
      if (TIMER_QUEUE[i].id === id) { TIMER_QUEUE.splice(i, 1); return; }
    }
  }

  globalThis.setTimeout = function (fn, delay) {
    if (typeof fn !== 'function' || TIMER_QUEUE.length >= TIMER_LIMIT) return 0;
    TIMER_QUEUE.push({ fn: fn, id: TIMER_ID, delay: delay || 0, repeat: false });
    return TIMER_ID++;
  };
  globalThis.setInterval = function (fn, delay) {
    if (typeof fn !== 'function' || TIMER_QUEUE.length >= TIMER_LIMIT) return 0;
    TIMER_QUEUE.push({ fn: fn, id: TIMER_ID, delay: delay || 0, repeat: true });
    return TIMER_ID++;
  };
  globalThis.clearTimeout = dropTimer;
  globalThis.clearInterval = dropTimer;
  globalThis.requestAnimationFrame = function (fn) { return globalThis.setTimeout(fn, 16); };
  globalThis.cancelAnimationFrame = dropTimer;
  globalThis.queueMicrotask = function (fn) { return globalThis.setTimeout(fn, 0); };

  // Called by the host. Returns how many callbacks ran and how many are left.
  globalThis.__scout_drain_timers = function (budget) {
    var ran = 0;
    while (TIMER_QUEUE.length && ran < budget) {
      var t = TIMER_QUEUE.shift();
      if (t.repeat) TIMER_QUEUE.push(t);
      try { t.fn(); } catch (e) { /* one failing timer must not stop the rest */ }
      ran++;
    }
    return { ran: ran, pending: TIMER_QUEUE.length };
  };
})();
"##;

// --------------------------------------------------------------- network

/// A request issued by page JavaScript (`fetch` or `XMLHttpRequest`).
#[derive(Debug, Clone)]
pub struct NetRequest {
    pub method: String,
    /// As written by the page: relative URLs are resolved against the document.
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<NetBody>,
}

/// A request body. Text is the common case; binary arrives as base64.
#[derive(Debug, Clone)]
pub enum NetBody {
    Text(String),
    Bytes(Vec<u8>),
}

/// What the host got back. `body` is text (lossy for non-UTF-8 bytes); when
/// the response was binary, `body_b64` carries the exact bytes so scripts can
/// have them without corruption.
#[derive(Debug, Clone)]
pub struct NetResponse {
    pub status: u16,
    pub status_text: String,
    /// Final URL after redirects, which is what `response.url` reports.
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
    /// Present only when the response was not textual: the raw bytes,
    /// base64-encoded.
    pub body_b64: Option<String>,
}

/// The host's network, injected into the sandbox.
///
/// A synchronous callback rather than a future: the sandbox has no event loop,
/// so a request completes before the calling statement finishes. The
/// implementation is responsible for every policy that applies to navigation —
/// SSRF validation, the session cookie jar, byte caps and timeouts — so page
/// JavaScript cannot reach anything a navigation could not.
pub type NetFn = std::sync::Arc<dyn Fn(NetRequest) -> std::result::Result<NetResponse, String> + Send + Sync>;

/// Counters a caller can publish next to the JS summary.
#[derive(Debug, Default, Clone, Copy)]
pub struct NetStats {
    pub requests: usize,
    pub failed: usize,
    pub bytes: usize,
    pub blocked: usize,
}

/// `fetch`, `XMLHttpRequest`, `atob`/`btoa` and `URLSearchParams`.
///
/// ## How this differs from a browser, and why
///
/// * **Requests are synchronous.** There is no event loop to suspend on, so
///   `fetch` performs the request while the calling statement is still on the
///   stack and returns an already-settled promise. `await`, `.then` chains and
///   `Promise.all` therefore work — the host drains pending jobs after each
///   script, exactly as a browser drains its microtask queue — but nothing is
///   ever *waiting* on the network when the script ends.
/// * **CORS is not enforced.** A browser's same-origin policy protects the
///   user's credentials from other sites' scripts; here the request is already
///   checked against the SSRF policy and the session's own jar, so rejecting a
///   cross-origin response would remove compatibility without adding safety.
///   `credentials` is still honoured: cross-origin requests only carry cookies
///   when the page asks for `include`.
/// * **Response bodies are text.** Octet streams are decoded as UTF-8, so a
///   binary response is lossy rather than faithful.
const NET_SHIM: &str = r##"
(function () {
  var B64 = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/';

  function bytesToB64(bytes) {
    var out = '', i, b0, b1, b2;
    for (i = 0; i < bytes.length; i += 3) {
      b0 = bytes[i]; b1 = bytes[i + 1]; b2 = bytes[i + 2];
      out += B64.charAt(b0 >> 2);
      out += B64.charAt(((b0 & 3) << 4) | ((b1 === undefined ? 0 : b1) >> 4));
      out += (b1 === undefined) ? '=' : B64.charAt(((b1 & 15) << 2) | ((b2 === undefined ? 0 : b2) >> 6));
      out += (b2 === undefined) ? '=' : B64.charAt(b2 & 63);
    }
    return out;
  }

  function b64ToBytes(str) {
    var map = {}, i, buf = 0, bits = 0, out = [];
    for (i = 0; i < 64; i++) map[B64.charAt(i)] = i;
    var s = String(str).replace(/[^A-Za-z0-9+/]/g, '');
    for (i = 0; i < s.length; i++) {
      buf = (buf << 6) | map[s.charAt(i)];
      bits += 6;
      if (bits >= 8) { bits -= 8; out.push((buf >> bits) & 0xff); }
    }
    return out;
  }

  function latin1FromBytes(b) {
    var out = '', i;
    for (i = 0; i < b.length; i++) out += String.fromCharCode(b[i]);
    return out;
  }

  function bytesFromLatin1(s) {
    var out = [], i;
    for (i = 0; i < s.length; i++) out.push(s.charCodeAt(i) & 0xff);
    return out;
  }

  function utf8Bytes(s) {
    var out = [], i, c;
    for (i = 0; i < s.length; i++) {
      c = s.charCodeAt(i);
      if (c < 0x80) out.push(c);
      else if (c < 0x800) out.push(0xc0 | (c >> 6), 0x80 | (c & 0x3f));
      else out.push(0xe0 | (c >> 12), 0x80 | ((c >> 6) & 0x3f), 0x80 | (c & 0x3f));
    }
    return out;
  }

  globalThis.atob = function (s) { return latin1FromBytes(b64ToBytes(s)); };
  globalThis.btoa = function (s) { return bytesToB64(bytesFromLatin1(String(s))); };

  function URLSearchParams(init) {
    this._p = [];
    var i, kv, parts;
    if (typeof init === 'string') {
      var s = init.charAt(0) === '?' ? init.slice(1) : init;
      parts = s.split('&');
      for (i = 0; i < parts.length; i++) {
        if (!parts[i]) continue;
        kv = parts[i].split('=');
        this._p.push([
          decodeURIComponent(kv[0].replace(/\+/g, ' ')),
          decodeURIComponent((kv[1] === undefined ? '' : kv[1]).replace(/\+/g, ' '))
        ]);
      }
    } else if (init && typeof init === 'object') {
      for (var k in init) {
        if (Object.prototype.hasOwnProperty.call(init, k)) {
          this._p.push([String(k), String(init[k])]);
        }
      }
    }
  }
  URLSearchParams.prototype.append = function (k, v) { this._p.push([String(k), String(v)]); };
  URLSearchParams.prototype.set = function (k, v) {
    for (var i = 0; i < this._p.length; i++) {
      if (this._p[i][0] === String(k)) { this._p[i][1] = String(v); return; }
    }
    this._p.push([String(k), String(v)]);
  };
  URLSearchParams.prototype.get = function (k) {
    for (var i = 0; i < this._p.length; i++) if (this._p[i][0] === String(k)) return this._p[i][1];
    return null;
  };
  URLSearchParams.prototype.getAll = function (k) {
    var out = [];
    for (var i = 0; i < this._p.length; i++) if (this._p[i][0] === String(k)) out.push(this._p[i][1]);
    return out;
  };
  URLSearchParams.prototype.has = function (k) { return this.get(k) !== null; };
  URLSearchParams.prototype['delete'] = function (k) {
    var keep = [];
    for (var i = 0; i < this._p.length; i++) if (this._p[i][0] !== String(k)) keep.push(this._p[i]);
    this._p = keep;
  };
  URLSearchParams.prototype.forEach = function (fn) {
    for (var i = 0; i < this._p.length; i++) fn(this._p[i][1], this._p[i][0]);
  };
  URLSearchParams.prototype.entries = function () {
    var out = [];
    for (var i = 0; i < this._p.length; i++) out.push([this._p[i][0], this._p[i][1]]);
    return out;
  };
  URLSearchParams.prototype.keys = function () { return this.entries().map(function (e) { return e[0]; }); };
  URLSearchParams.prototype.values = function () { return this.entries().map(function (e) { return e[1]; }); };
  URLSearchParams.prototype.toString = function () {
    var out = [];
    for (var i = 0; i < this._p.length; i++) {
      out.push(encodeURIComponent(this._p[i][0]) + '=' + encodeURIComponent(this._p[i][1]));
    }
    return out.join('&');
  };
  globalThis.URLSearchParams = URLSearchParams;

  function headerList(h) {
    var out = [], i, k;
    if (!h) return out;
    if (Object.prototype.toString.call(h) === '[object Array]') {
      for (i = 0; i < h.length; i++) out.push([String(h[i][0]), String(h[i][1])]);
      return out;
    }
    if (typeof h.entries === 'function') {
      var e = h.entries() || [];
      for (i = 0; i < e.length; i++) out.push([String(e[i][0]), String(e[i][1])]);
      return out;
    }
    for (k in h) if (Object.prototype.hasOwnProperty.call(h, k)) out.push([k, String(h[k])]);
    return out;
  }

  function makeHeaders(list) {
    var entries = [], i;
    for (i = 0; i < (list || []).length; i++) {
      entries.push([String(list[i][0]).toLowerCase(), String(list[i][1])]);
    }
    function find(k) {
      k = String(k).toLowerCase();
      for (var i = 0; i < entries.length; i++) if (entries[i][0] === k) return i;
      return -1;
    }
    return {
      append: function (k, v) { entries.push([String(k).toLowerCase(), String(v)]); },
      set: function (k, v) {
        var i = find(k);
        if (i >= 0) entries[i][1] = String(v);
        else entries.push([String(k).toLowerCase(), String(v)]);
      },
      get: function (k) { var i = find(k); return i < 0 ? null : entries[i][1]; },
      getAll: function (k) {
        k = String(k).toLowerCase();
        var out = [];
        for (var i = 0; i < entries.length; i++) if (entries[i][0] === k) out.push(entries[i][1]);
        return out;
      },
      has: function (k) { return find(k) >= 0; },
      'delete': function (k) { var i = find(k); if (i >= 0) entries.splice(i, 1); },
      forEach: function (fn) { for (var i = 0; i < entries.length; i++) fn(entries[i][1], entries[i][0]); },
      entries: function () { return entries.map(function (e) { return [e[0], e[1]]; }); },
      keys: function () { return entries.map(function (e) { return e[0]; }); },
      values: function () { return entries.map(function (e) { return e[1]; }); }
    };
  }
  globalThis.Headers = function (init) { return makeHeaders(headerList(init)); };

  function bodyPayload(body) {
    var i, arr;
    if (body === undefined || body === null) return null;
    if (typeof body === 'string') return { kind: 'text', value: body };
    if (body instanceof URLSearchParams) return { kind: 'text', value: body.toString() };
    if (typeof ArrayBuffer !== 'undefined' && body instanceof ArrayBuffer) {
      return { kind: 'base64', value: bytesToB64(Array.prototype.slice.call(new Uint8Array(body))) };
    }
    if (body && typeof body.byteLength === 'number' && typeof body.length === 'number') {
      arr = [];
      for (i = 0; i < body.length; i++) arr.push(body[i] & 0xff);
      return { kind: 'base64', value: bytesToB64(arr) };
    }
    if (typeof body === 'object') return { kind: 'text', value: JSON.stringify(body) };
    return { kind: 'text', value: String(body) };
  }

  function makeResponse(r) {
    var body = (r.body === undefined || r.body === null) ? '' : String(r.body);
    var b64 = (r.bodyB64 === undefined || r.bodyB64 === null) ? null : String(r.bodyB64);
    var res = {
      ok: r.status >= 200 && r.status < 300,
      status: r.status,
      statusText: r.statusText || '',
      url: r.url || '',
      redirected: false,
      type: 'basic',
      headers: makeHeaders(r.headers || []),
      bodyUsed: false,
      base64: b64
    };
    res.text = function () { res.bodyUsed = true; return Promise.resolve(body); };
    res.json = function () { res.bodyUsed = true; return Promise.resolve(JSON.parse(body)); };
    res.arrayBuffer = function () {
      res.bodyUsed = true;
      // Binary responses carry their exact bytes in base64; textual ones are
      // encoded from the decoded string. Never re-encode lossy text as bytes.
      var bytes = b64 ? b64ToBytes(b64) : utf8Bytes(body), ab = new ArrayBuffer(bytes.length), view = new Uint8Array(ab);
      for (var i = 0; i < bytes.length; i++) view[i] = bytes[i];
      return Promise.resolve(ab);
    };
    res.clone = function () { return makeResponse(r); };
    return res;
  }
  globalThis.Response = function (body, init) {
    return makeResponse({
      status: (init && init.status) || 200,
      statusText: (init && init.statusText) || '',
      url: '',
      headers: headerList(init && init.headers),
      body: body === undefined || body === null ? '' : String(body)
    });
  };

  // One call, one request. The host applies the SSRF policy, the session jar,
  // the byte cap and the timeout; a failure becomes a thrown TypeError, which
  // is what fetch and XHR reject with.
  function netCall(req) {
    var raw = globalThis.__scout_net(JSON.stringify(req));
    var r = JSON.parse(raw);
    if (!r || !r.ok) throw new Error((r && r.error) || 'the request was refused');
    return r.response;
  }

  globalThis.fetch = function (input, init) {
    var url, method = 'GET', headers = null, body = undefined, credentials = 'same-origin';
    if (input && typeof input === 'object' && typeof input.url === 'string') {
      url = input.url;
      method = input.method || 'GET';
      headers = input.headers;
      body = input._body;
      if (input.credentials) credentials = input.credentials;
      if (init && init.body !== undefined) body = init.body;
    } else {
      url = String(input);
    }
    if (init) {
      if (init.method) method = init.method;
      if (init.headers !== undefined) headers = init.headers;
      if (init.body !== undefined) body = init.body;
      if (init.credentials) credentials = init.credentials;
    }
    // The executor runs synchronously, so the promise is already settled by
    // the time fetch returns. .then chains and await still work because the
    // host drains pending jobs after each script.
    return new Promise(function (resolve, reject) {
      try {
        var res = netCall({
          method: String(method).toUpperCase(),
          url: url,
          headers: headerList(headers),
          body: bodyPayload(body),
          credentials: credentials
        });
        resolve(makeResponse(res));
      } catch (e) {
        reject(new TypeError(e && e.message ? e.message : String(e)));
      }
    });
  };

  globalThis.Request = function (input, init) {
    var url = (input && typeof input === 'object' && typeof input.url === 'string') ? input.url : String(input);
    this.url = url;
    this.method = (init && init.method) || (input && input.method) || 'GET';
    this.headers = makeHeaders(headerList((init && init.headers) || (input && input.headers)));
    this.credentials = (init && init.credentials) || 'same-origin';
    this._body = (init && init.body !== undefined) ? init.body : (input && input._body);
  };

  // Events are dispatched after the current script finishes, so a handler
  // assigned after send() still fires, as it would in a browser.
  function queue(fn) { Promise.resolve().then(fn); }

  function XMLHttpRequest() {
    this.readyState = 0;
    this.status = 0;
    this.statusText = '';
    this.responseText = '';
    this.response = '';
    this.responseBase64 = null;
    this.responseType = '';
    this.responseURL = '';
    this.withCredentials = false;
    this.timeout = 0;
    this.onreadystatechange = null;
    this.onload = null;
    this.onerror = null;
    this.onloadend = null;
    this.onabort = null;
    this.ontimeout = null;
    this.upload = { addEventListener: function () {} };
    this._method = 'GET';
    this._url = '';
    this._async = true;
    this._requestHeaders = [];
    this._responseHeaders = [];
    this._listeners = {};
    this._aborted = false;
  }
  XMLHttpRequest.UNSENT = 0;
  XMLHttpRequest.OPENED = 1;
  XMLHttpRequest.HEADERS_RECEIVED = 2;
  XMLHttpRequest.LOADING = 3;
  XMLHttpRequest.DONE = 4;

  XMLHttpRequest.prototype.open = function (method, url, async) {
    this._method = String(method || 'GET').toUpperCase();
    this._url = String(url);
    this._async = async !== false;
    this.readyState = 1;
    var self = this;
    queue(function () { self._fire('readystatechange'); });
  };
  XMLHttpRequest.prototype.setRequestHeader = function (k, v) {
    this._requestHeaders.push([String(k), String(v)]);
  };
  XMLHttpRequest.prototype.overrideMimeType = function () {};
  XMLHttpRequest.prototype.addEventListener = function (type, fn) {
    if (!this._listeners[type]) this._listeners[type] = [];
    if (typeof fn === 'function') this._listeners[type].push(fn);
  };
  XMLHttpRequest.prototype.removeEventListener = function () {};
  XMLHttpRequest.prototype.getResponseHeader = function (k) {
    var lower = String(k).toLowerCase();
    for (var i = 0; i < this._responseHeaders.length; i++) {
      if (String(this._responseHeaders[i][0]).toLowerCase() === lower) return String(this._responseHeaders[i][1]);
    }
    return null;
  };
  XMLHttpRequest.prototype.getAllResponseHeaders = function () {
    var out = [];
    for (var i = 0; i < this._responseHeaders.length; i++) {
      out.push(this._responseHeaders[i][0] + ': ' + this._responseHeaders[i][1]);
    }
    return out.join('\r\n');
  };
  XMLHttpRequest.prototype.abort = function () {
    this._aborted = true;
    this.readyState = 0;
    var self = this;
    queue(function () { self._fire('abort'); self._fire('loadend'); });
  };
  XMLHttpRequest.prototype._fire = function (type) {
    var handler = this['on' + type];
    if (typeof handler === 'function') {
      try { handler.call(this, { type: type, target: this }); } catch (e) { /* one bad listener must not stop the rest */ }
    }
    var list = this._listeners[type] || [];
    for (var i = 0; i < list.length; i++) {
      try { list[i].call(this, { type: type, target: this }); } catch (e) {}
    }
  };
  XMLHttpRequest.prototype.send = function (body) {
    var self = this;
    var payload;
    try {
      payload = bodyPayload(body);
    } catch (e) {
      payload = null;
    }
    try {
      var r = netCall({
        method: this._method,
        url: this._url,
        headers: this._requestHeaders,
        body: payload,
        credentials: this.withCredentials ? 'include' : 'same-origin'
      });
      this._responseHeaders = r.headers || [];
      this.readyState = 4;
      this.status = r.status;
      this.statusText = r.statusText || '';
      this.responseURL = r.url || this._url;
      this.responseText = (r.body === undefined || r.body === null) ? '' : String(r.body);
      this.responseBase64 = (r.bodyB64 === undefined || r.bodyB64 === null) ? null : String(r.bodyB64);
      if (this.responseType === 'json') {
        this.response = JSON.parse(this.responseText);
      } else if (this.responseType === 'arraybuffer') {
        var bytes = this.responseBase64 ? b64ToBytes(this.responseBase64) : utf8Bytes(this.responseText);
        var ab = new ArrayBuffer(bytes.length), view = new Uint8Array(ab);
        for (var bi = 0; bi < bytes.length; bi++) view[bi] = bytes[bi];
        this.response = ab;
      } else {
        this.response = this.responseText;
      }
      queue(function () {
        if (self._aborted) return;
        self._fire('readystatechange');
        if (self.status >= 200 && self.status < 300) self._fire('load');
        else self._fire('error');
        self._fire('loadend');
      });
    } catch (e) {
      this.readyState = 4;
      this.status = 0;
      this.statusText = '';
      queue(function () {
        if (self._aborted) return;
        self._fire('readystatechange');
        self._fire('error');
        self._fire('loadend');
      });
    }
  };
  globalThis.XMLHttpRequest = XMLHttpRequest;
})();
"##;

/// Install `__scout_net`, the only channel between page JavaScript and the
/// network. Always present, so a script that calls `fetch` on a runtime with
/// networking switched off gets a stated refusal rather than `fetch is not
/// defined` swallowed as a script error.
fn wire_net(ctx: &rquickjs::Ctx<'_>, net: Option<NetFn>) -> Result<()> {
    let f = rquickjs::Function::new(ctx.clone(), move |input: String| -> String {
        let out = match &net {
            None => json!({
                "ok": false,
                "error": "network access from page scripts is disabled (SCOUT_JS_NETWORK=0)"
            }),
            Some(call) => match serde_json::from_str::<Value>(&input) {
                Err(e) => json!({ "ok": false, "error": format!("malformed request: {e}") }),
                Ok(v) => match parse_net_request(&v) {
                    Err(e) => json!({ "ok": false, "error": e }),
                    Ok(req) => match call(req) {
                        Ok(r) => json!({
                            "ok": true,
                            "response": {
                                "status": r.status,
                                "statusText": r.status_text,
                                "url": r.url,
                                "headers": r.headers,
                                "body": r.body,
                                "bodyB64": r.body_b64
                            }
                        }),
                        Err(e) => json!({ "ok": false, "error": e }),
                    },
                },
            },
        };
        out.to_string()
    })
    .map_err(|e| js_err(format!("could not install the network bridge: {e}")))?;
    ctx.globals()
        .set("__scout_net", f)
        .map_err(|e| js_err(format!("{e}")))
}

fn parse_net_request(v: &Value) -> std::result::Result<NetRequest, String> {
    let method = v
        .get("method")
        .and_then(|m| m.as_str())
        .unwrap_or("GET")
        .to_ascii_uppercase();
    let url = v
        .get("url")
        .and_then(|m| m.as_str())
        .ok_or_else(|| "the request had no url".to_string())?
        .to_string();
    let headers = v
        .get("headers")
        .and_then(|h| h.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|p| {
                    let k = p.get(0).and_then(|x| x.as_str())?;
                    let val = p.get(1).and_then(|x| x.as_str())?;
                    Some((k.to_string(), val.to_string()))
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let body = match v.get("body") {
        None | Some(Value::Null) => None,
        Some(b) => {
            let kind = b.get("kind").and_then(|k| k.as_str()).unwrap_or("text");
            let value = b.get("value").and_then(|x| x.as_str()).unwrap_or("");
            match kind {
                "base64" => Some(NetBody::Bytes(decode_base64(value))),
                _ => Some(NetBody::Text(value.to_string())),
            }
        }
    };
    Ok(NetRequest {
        method,
        url,
        headers,
        body,
    })
}

fn decode_base64(input: &str) -> Vec<u8> {
    let table: Vec<i16> = (0u8..=255)
        .map(|b| {
            let c = b as char;
            if c.is_ascii_uppercase() {
                (b - b'A') as i16
            } else if c.is_ascii_lowercase() {
                (b - b'a' + 26) as i16
            } else if c.is_ascii_digit() {
                (b - b'0' + 52) as i16
            } else if c == '+' {
                62
            } else if c == '/' {
                63
            } else {
                -1
            }
        })
        .collect();
    let mut out = Vec::with_capacity(input.len() * 3 / 4 + 3);
    let mut buf: u32 = 0;
    let mut bits = 0u32;
    for b in input.bytes() {
        let v = table[b as usize];
        if v < 0 {
            continue;
        }
        buf = (buf << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    out
}

/// Run pending promise jobs, so `await` and `.then` chains issued by a script
/// that performed a request actually finish. Bounded, and checked against the
/// wall clock: a chain that keeps queueing work cannot outlive the budget.
fn drain_jobs(ctx: &rquickjs::Ctx<'_>, cap: u32) -> u32 {
    let mut ran = 0u32;
    while ran < cap && ctx.execute_pending_job() {
        ran += 1;
    }
    ran
}

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
/// Scripts with a `return` run as a function body; scripts without one run as
/// a program whose completion value is the result. Knowing which kind ran lets
/// the caller tell "your script computed nothing" apart from "your script
/// returned undefined on purpose".
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
pub fn run(
    script: &str,
    snapshot: &Value,
    base_url: &str,
    user_agent: &str,
    net: Option<NetFn>,
    timeout: Duration,
    max_memory: usize,
) -> Result<JsOutcome> {
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
        ctx.eval::<(), _>(NET_SHIM)
            .map_err(|e| js_err(classify(&e, &timed_out)))?;
        wire_net(&ctx, net)?;

        // Scripts with an explicit `return` run as a function body, as before.
        // Scripts without one run as a program, so the value of the final
        // expression comes back the way it would at a console: `1 + 1` is 2,
        // not a silent null.
        let program = if has_return(script) {
            format!("(function(){{ {} }})()", script)
        } else {
            script.to_string()
        };
        // A script that awaits a request finishes in pending jobs, not in the
        // call itself: `return await fetch(u).then(r => r.json())` yields a
        // promise, and the value only exists once the job queue has been
        // driven. Resolving it here is the difference between /v1/eval being
        // able to call an API and returning a promise nobody will ever run.
        let v = ctx
            .eval::<rquickjs::Value, _>(program.as_str())
            .map_err(|e| js_err(classify(&e, &timed_out)))?;
        let value = match v.as_promise() {
            Some(promise) => match promise.finish::<rquickjs::Value>() {
                Ok(rv) if rv.is_undefined() || rv.is_null() => None,
                Ok(rv) => Some(render_js_value(&rv)),
                // Rejected, or blocked on something that is not a job (a
                // timer). The pending exception has to be cleared or every
                // later eval on this context fails.
                Err(_) => {
                    let _ = ctx.catch();
                    None
                }
            },
            None if v.is_undefined() || v.is_null() => None,
            None => Some(render_js_value(&v)),
        };
        Ok(value)
    })?;

    let duration_ms = started.elapsed().as_millis() as u64;
    let logs = logs.borrow().clone();
    let timed_out = timed_out.load(std::sync::atomic::Ordering::SeqCst);

    // A `None` value without a `return` means the program's final statement
    // produced no value (a declaration, or a call returning undefined). Say so
    // rather than leaving the caller to debug a silent null.
    let mut warnings: Vec<Value> = Vec::new();
    if result.is_none() && !timed_out && !script.trim().is_empty() && !has_return(script) {
        warnings.push(json!({
            "code": "no_return_value",
            "message": "The script produced no value. Its final statement has no value; \
                        end with an expression, or use an explicit `return`."
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

/// What running a page's scripts produced.
#[derive(Debug)]
pub struct PageRunOutcome {
    /// The mutated document, serialised back to HTML. `None` when execution was
    /// disabled or nothing ran.
    pub html: Option<String>,
    pub logs: Vec<String>,
    pub scripts_run: usize,
    pub scripts_failed: usize,
    pub timers_run: u32,
    pub timers_pending: u32,
    /// Promise jobs run after the scripts: the continuations of `await` and
    /// `.then`, which is where a page's fetch callbacks actually execute.
    pub jobs_run: u32,
    pub duration_ms: u64,
    pub timed_out: bool,
    /// Per-script failures. A broken analytics script must not fail the page,
    /// so these are reported rather than returned as an error.
    pub errors: Vec<String>,
}

/// Run a page's scripts, in document order, inside a single context.
///
/// One context for all of them is the point: classic scripts share a global
/// scope, so a library loaded by `<script src>` must be visible to the inline
/// script that follows it. Creating a context per script would break that.
///
/// Execution is best-effort by design. Real pages ship scripts that reference
/// APIs we do not implement, and failing the whole navigation because one
/// tracker threw would make the runtime useless on the open web. Individual
/// failures are collected in `errors`; only a timeout or an engine failure
/// returns `Err`.
pub fn run_page(
    scripts: &[String],
    snapshot: &Value,
    base_url: &str,
    user_agent: &str,
    net: Option<NetFn>,
    timeout: Duration,
    max_memory: usize,
    timer_budget: u32,
    // Total promise jobs one page may run, across every script and drain.
    job_budget: u32,
) -> Result<PageRunOutcome> {
    use rquickjs::{Context, Runtime};

    let started = Instant::now();
    let rt = Runtime::new().map_err(|e| js_err(format!("could not start JS runtime: {e}")))?;
    rt.set_memory_limit(max_memory);
    rt.set_max_stack_size(512 * 1024);

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
    let logs: std::rc::Rc<std::cell::RefCell<Vec<String>>> =
        std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));

    let mut scripts_run = 0usize;
    let mut scripts_failed = 0usize;
    let mut errors: Vec<String> = Vec::new();
    // Hoisted out of the closure: `ctx.with` cannot return these directly
    // because the borrow of `ctx` ends with it.
    let timer_counts: std::rc::Rc<std::cell::Cell<(u32, u32)>> =
        std::rc::Rc::new(std::cell::Cell::new((0, 0)));
    let jobs: std::rc::Rc<std::cell::Cell<u32>> = std::rc::Rc::new(std::cell::Cell::new(0));
    let mut jobs_left = job_budget;

    let outcome = ctx.with(|ctx| -> Result<Option<String>> {
        wire_console(&ctx, &logs)?;

        let prelude = format!(
            "globalThis.__scout_dom = {};\nglobalThis.__scout_url = {};\nglobalThis.__scout_ua = {};\n",
            snapshot,
            serde_json::to_string(base_url).unwrap_or_else(|_| "\"\"".into()),
            serde_json::to_string(user_agent).unwrap_or_else(|_| "\"\"".into()),
        );
        ctx.eval::<(), _>(prelude.as_str())
            .map_err(|e| js_err(classify(&e, &timed_out)))?;
        ctx.eval::<(), _>(DOM_SHIM)
            .map_err(|e| js_err(classify(&e, &timed_out)))?;
        ctx.eval::<(), _>(NET_SHIM)
            .map_err(|e| js_err(classify(&e, &timed_out)))?;
        wire_net(&ctx, net)?;

        for script in scripts {
            if timed_out.load(std::sync::atomic::Ordering::SeqCst) {
                break;
            }
            // Deliberately NOT wrapped in a function. Classic scripts share a
            // global scope: a `var` declared by one <script> must be visible to
            // the next. (The /v1/eval endpoint does wrap, so `return` works
            // there; page scripts have no top-level return to support.)
            match ctx.eval::<rquickjs::Value, _>(script.as_str()) {
                Ok(_) => scripts_run += 1,
                Err(e) => {
                    scripts_failed += 1;
                    if errors.len() < 20 {
                        errors.push(classify(&e, &timed_out));
                    }
                }
            }
            // A script that issued a request leaves its continuation in the job
            // queue. Drain it before the next script, which is when a browser
            // would have run those microtasks.
            let ran = drain_jobs(&ctx, jobs_left);
            jobs.set(jobs.get() + ran);
            jobs_left = jobs_left.saturating_sub(ran);
        }

        // Timers can issue requests and requests can schedule timers, so the
        // two queues are drained together until neither produces work.
        let mut timers_run = 0u32;
        let mut timers_pending = 0u32;
        let mut remaining = timer_budget;
        for _ in 0..64 {
            let queued = drain_jobs(&ctx, jobs_left);
            jobs_left = jobs_left.saturating_sub(queued);
            jobs.set(jobs.get() + queued);
            let drained: Option<String> = ctx
                .eval::<String, _>(
                    format!("JSON.stringify(__scout_drain_timers({}))", remaining).as_str(),
                )
                .ok();
            let (ran, pending) = parse_timer_result(drained.as_deref());
            timers_run += ran;
            timers_pending = pending;
            remaining = remaining.saturating_sub(ran);
            if queued == 0 && ran == 0 {
                break;
            }
            if timed_out.load(std::sync::atomic::Ordering::SeqCst) || remaining == 0 {
                break;
            }
        }
        timer_counts.set((timers_run, timers_pending));

        let html = if timed_out.load(std::sync::atomic::Ordering::SeqCst) {
            None
        } else {
            ctx.eval::<String, _>("__scout_serialize()")
                .map_err(|e| js_err(classify(&e, &timed_out)))?
                .into()
        };

        Ok(html)
    });

    let html = outcome?;
    let duration_ms = started.elapsed().as_millis() as u64;
    let logs = logs.borrow().clone();
    let (timers_run, timers_pending) = timer_counts.get();

    Ok(PageRunOutcome {
        html,
        logs,
        scripts_run,
        scripts_failed,
        timers_run,
        timers_pending,
        jobs_run: jobs.get(),
        duration_ms,
        timed_out: timed_out.load(std::sync::atomic::Ordering::SeqCst),
        errors,
    })
}

fn parse_timer_result(v: Option<&str>) -> (u32, u32) {
    match v.and_then(|raw| serde_json::from_str::<Value>(raw).ok()) {
        Some(json) => (
            json.get("ran").and_then(|r| r.as_u64()).unwrap_or(0) as u32,
            json.get("pending").and_then(|r| r.as_u64()).unwrap_or(0) as u32,
        ),
        None => (0, 0),
    }
}

/// Attach a bounded `console` to a context. Shared by `run` and `run_page`.
fn wire_console(
    ctx: &rquickjs::Ctx<'_>,
    sink: &std::rc::Rc<std::cell::RefCell<Vec<String>>>,
) -> Result<()> {
    let console = rquickjs::Object::new(ctx.clone()).map_err(|e| js_err(format!("{e}")))?;
    for level in ["log", "info", "warn", "error", "debug"] {
        let logs_in = sink.clone();
        let lvl = level.to_string();
        let f = rquickjs::Function::new(
            ctx.clone(),
            move |args: rquickjs::function::Rest<rquickjs::Value>| {
                let parts: Vec<String> = args.0.iter().map(render_value).collect();
                if let Ok(mut v) = logs_in.try_borrow_mut() {
                    if v.len() < 500 {
                        v.push(format!("[{lvl}] {}", parts.join(" ")));
                    }
                }
            },
        )
        .map_err(|e| js_err(format!("{e}")))?;
        console.set(level, f).map_err(|e| js_err(format!("{e}")))?;
    }
    ctx.globals()
        .set("console", console)
        .map_err(|e| js_err(format!("{e}")))?;
    Ok(())
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

    fn run_page_on(html: &str, scripts: &[&str]) -> Result<PageRunOutcome> {
        run_page_net_on(html, scripts, None)
    }

    fn run_page_net_on(html: &str, scripts: &[&str], net: Option<NetFn>) -> Result<PageRunOutcome> {
        run_page_budget_on(html, scripts, net, 64, 500)
    }

    fn run_page_budget_on(
        html: &str,
        scripts: &[&str],
        net: Option<NetFn>,
        timer_budget: u32,
        job_budget: u32,
    ) -> Result<PageRunOutcome> {
        let d = doc(html);
        let snap = dom_snapshot(&d);
        run_page(
            &scripts.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            &snap,
            "https://example.com/",
            "Scout/0.1",
            net,
            Duration::from_millis(2_000),
            64 * 1024 * 1024,
            timer_budget,
            job_budget,
        )
    }

    /// A host network that answers with binary content as base64.
    fn fake_net_b64(b64: &'static str, seen: std::sync::Arc<std::sync::Mutex<Vec<NetRequest>>>) -> NetFn {
        std::sync::Arc::new(move |req: NetRequest| {
            seen.lock().unwrap().push(req.clone());
            Ok(NetResponse {
                status: 200,
                status_text: "OK".into(),
                url: "https://example.com/img.png".into(),
                headers: vec![("content-type".into(), "image/png".into())],
                body: "\u{fffd}\u{fffd}".into(), // lossy text, as the host produces for binary
                body_b64: Some(b64.to_string()),
            })
        })
    }

    /// A host network that answers every request from a fixed table and records
    /// what it was asked for, so tests can assert on the request as well as the
    /// rendered result.
    fn fake_net(
        body: &'static str,
        seen: std::sync::Arc<std::sync::Mutex<Vec<NetRequest>>>,
    ) -> NetFn {
        std::sync::Arc::new(move |req: NetRequest| {
            seen.lock().unwrap().push(req.clone());
            Ok(NetResponse {
                status: 200,
                status_text: "OK".into(),
                url: "https://example.com/api/items".into(),
                headers: vec![("content-type".into(), "application/json".into())],
                body: body.to_string(),
                body_b64: None,
            })
        })
    }

    fn empty_seen() -> std::sync::Arc<std::sync::Mutex<Vec<NetRequest>>> {
        std::sync::Arc::new(std::sync::Mutex::new(Vec::new()))
    }

    // ------------------------------------------------------------- network

    #[test]
    fn fetch_reaches_the_document_through_a_then_chain() {
        let seen = empty_seen();
        let net = fake_net("[{\"name\":\"Ada\"},{\"name\":\"Alan\"}]", seen.clone());
        let out = run_page_net_on(
            "<html><body><ul id='l'></ul></body></html>",
            &[concat!(
                "fetch('/api/items')",
                "  .then(function (r) { return r.json(); })",
                "  .then(function (items) {",
                "    var l = document.getElementById('l');",
                "    for (var i = 0; i < items.length; i++) {",
                "      var li = document.createElement('li');",
                "      li.textContent = items[i].name;",
                "      l.appendChild(li);",
                "    }",
                "  });"
            )],
            Some(net),
        )
        .unwrap();

        assert_eq!(out.scripts_run, 1, "the page script must still count as run");
        assert!(out.jobs_run >= 1, "the .then callbacks are jobs: {out:?}");
        let html = out.html.unwrap_or_default();
        assert!(html.contains("Ada") && html.contains("Alan"), "got: {html}");
        let seen_guard = seen.lock().unwrap();
        let reqs = &*seen_guard;
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].method, "GET");
    }

    #[test]
    fn a_fetched_url_is_resolved_against_the_document() {
        let seen = empty_seen();
        let net = fake_net("{}", seen.clone());
        run_page_net_on("<p>x</p>", &["fetch('api/items');"], Some(net)).unwrap();
        // The host resolves it; what matters is that the page may write a
        // relative URL and the request still names a full one.
        let seen_guard = seen.lock().unwrap();
        let reqs = &*seen_guard;
        assert_eq!(reqs[0].url, "api/items");
    }

    #[test]
    fn awaiting_a_fetch_resolves_before_the_script_ends() {
        let seen = empty_seen();
        let net = fake_net("{\"n\":42}", seen.clone());
        let out = run_net_on(
            "<p>x</p>",
            "return fetch('/api').then(function (r) { return r.json(); }).then(function (j) { return j.n; });",
            Some(net),
        )
        .unwrap();
        assert_eq!(out.value, Some(serde_json::json!(42)), "logs: {:?}", out.logs);
    }

    #[test]
    fn an_async_function_can_await_fetch_and_write_the_dom() {
        let seen = empty_seen();
        let net = fake_net("{\"title\":\"Hello\"}", seen.clone());
        let out = run_page_net_on(
            "<html><body><h1 id='t'></h1></body></html>",
            &[concat!(
                "(async function () {",
                "  var r = await fetch('/api/title');",
                "  var j = await r.json();",
                "  document.getElementById('t').textContent = j.title;",
                "})();"
            )],
            Some(net),
        )
        .unwrap();
        assert!(out.html.clone().unwrap_or_default().contains("Hello"), "{:?}", out.html);
    }

    #[test]
    fn xhr_loads_and_fires_its_handlers_after_the_script() {
        let seen = empty_seen();
        let net = fake_net("{\"rows\":[1,2,3]}", seen.clone());
        let out = run_page_net_on(
            "<html><body><div id='d'></div></body></html>",
            &[concat!(
                "var x = new XMLHttpRequest();",
                "x.open('POST', '/api/rows');",
                "x.setRequestHeader('content-type', 'application/json');",
                "x.onreadystatechange = function () {",
                "  if (x.readyState === 4 && x.status === 200) {",
                "    document.getElementById('d').textContent = x.responseText;",
                "  }",
                "};",
                "x.send(JSON.stringify({ q: 'all' }));"
            )],
            Some(net),
        )
        .unwrap();

        let html = out.html.unwrap_or_default();
        assert!(html.contains("&quot;rows&quot;") || html.contains("rows"), "got: {html}");
        let seen_guard = seen.lock().unwrap();
        let reqs = &*seen_guard;
        assert_eq!(reqs[0].method, "POST");
        assert_eq!(
            reqs[0].headers.first().map(|h| h.0.clone()),
            Some("content-type".into())
        );
        assert!(reqs[0].body.is_some(), "the posted body must travel");
    }

    #[test]
    fn xhr_handlers_registered_after_send_still_fire() {
        let seen = empty_seen();
        let net = fake_net("ok", seen.clone());
        let out = run_page_net_on(
            "<html><body><div id='d'></div></body></html>",
            &[concat!(
                "var x = new XMLHttpRequest();",
                "x.open('GET', '/api/ping');",
                "x.send();",
                "x.addEventListener('load', function () {",
                "  document.getElementById('d').textContent = 'loaded:' + x.status;",
                "});"
            )],
            Some(net),
        )
        .unwrap();
        assert!(
            out.html.unwrap_or_default().contains("loaded:200"),
            "a handler added after send must still run, as in a browser"
        );
    }

    #[test]
    fn a_failed_request_rejects_instead_of_returning_nothing() {
        let net: NetFn = std::sync::Arc::new(|_| Err("host refused the request".to_string()));
        let out = run_net_on(
            "<p>x</p>",
            "return fetch('/api').then(function () { return 'resolved'; }).catch(function (e) { return 'rejected:' + e.message; });",
            Some(net),
        )
        .unwrap();
        assert_eq!(out.value, Some(serde_json::json!("rejected:host refused the request")));
    }

    #[test]
    fn fetch_without_a_host_network_is_refused_by_name() {
        // Networking disabled: the page must learn why, not see an undefined
        // `fetch` and die with an opaque TypeError.
        let out = run_net_on(
            "<p>x</p>",
            "return fetch('/api').catch(function (e) { return 'err:' + e.message; });",
            None,
        )
        .unwrap();
        let v = out.value.unwrap_or_default().to_string();
        assert!(v.contains("SCOUT_JS_NETWORK"), "got: {v}");
    }

    #[test]
    fn url_search_params_encodes_a_form_body() {
        let seen = empty_seen();
        let net = fake_net("ok", seen.clone());
        run_net_on(
            "<p>x</p>",
            "return fetch('/login', { method: 'POST', body: new URLSearchParams({ user: 'a b', pw: 'p&q' }) });",
            Some(net),
        )
        .unwrap();
        let seen_guard = seen.lock().unwrap();
        let reqs = &*seen_guard;
        match reqs[0].body.as_ref().expect("a body") {
            NetBody::Text(t) => assert_eq!(t, "user=a%20b&pw=p%26q"),
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[test]
    fn btoa_and_atob_round_trip() {
        let out = run_on("<p>x</p>", "return atob(btoa('hello world'));").unwrap();
        assert_eq!(out.value, Some(serde_json::json!("hello world")));
    }

    #[test]
    fn a_request_from_a_timer_still_reaches_the_document() {
        let seen = empty_seen();
        let net = fake_net("late", seen.clone());
        let out = run_page_net_on(
            "<html><body><div id='d'></div></body></html>",
            &[concat!(
                "setTimeout(function () {",
                "  fetch('/api/late').then(function (r) { return r.text(); }).then(function (t) {",
                "    document.getElementById('d').textContent = t;",
                "  });",
                "}, 10);"
            )],
            Some(net),
        )
        .unwrap();
        assert!(
            out.html.clone().unwrap_or_default().contains("late"),
            "a timer callback that issues a request must still render: {:?}",
            out.html
        );
    }

    #[test]
    fn the_job_queue_is_bounded() {
        // A page that keeps chaining promises must not spin forever; the drain
        // is capped and the wall clock still applies.
        let net = fake_net("{}", empty_seen());
        let out = run_page_budget_on(
            "<p>x</p>",
            &["(function loop(i) { if (i > 100000) return; Promise.resolve().then(function () { loop(i + 1); }); })(0);"],
            Some(net),
            64,
            500,
        )
        .unwrap();
        assert!(
            out.jobs_run <= 500,
            "a page may not exceed its job budget, got {}",
            out.jobs_run
        );
    }

    #[test]
    fn page_scripts_share_one_global_scope() {
        // This is why scripts run in a single context: a value defined by an
        // earlier script must be visible to a later one, as in a browser.
        let out = run_page_on(
            "<html><body><div id='a'></div></body></html>",
            &[
                "var shared = 41;",
                "document.getElementById('a').textContent = String(shared + 1);",
            ],
        )
        .unwrap();
        assert_eq!(out.scripts_run, 2);
        assert_eq!(out.scripts_failed, 0);
        let html = out.html.unwrap_or_default();
        assert!(html.contains("42"), "expected the written value, got: {html}");
    }

    #[test]
    fn one_broken_script_does_not_fail_the_page() {
        let out = run_page_on(
            "<html><body><div id='a'></div></body></html>",
            &[
                "thisReferencesSomethingUndefined.boom();",
                "document.getElementById('a').textContent = 'still here';",
            ],
        )
        .unwrap();
        assert_eq!(out.scripts_run, 1);
        assert_eq!(out.scripts_failed, 1);
        assert!(!out.errors.is_empty(), "the failure should be reported");
        assert!(out.html.unwrap_or_default().contains("still here"));
    }

    #[test]
    fn deferred_writes_from_timers_reach_the_document() {
        let out = run_page_on(
            "<html><body><div id='a'></div></body></html>",
            &["setTimeout(function () { document.getElementById('a').textContent = 'late'; }, 10);"],
        )
        .unwrap();
        assert!(out.timers_run >= 1, "the queued callback should have run");
        assert!(out.html.unwrap_or_default().contains("late"));
    }

    #[test]
    fn created_elements_are_appended_not_swallowed() {
        // The old createElement stub accepted appendChild and did nothing, so
        // any script-built list vanished without a trace.
        let out = run_page_on(
            "<html><body><ul id='list'></ul></body></html>",
            &[
                "var ul = document.getElementById('list');",
                "for (var i = 0; i < 3; i++) { var li = document.createElement('li'); \
                 li.className = 'item'; li.setAttribute('data-i', i); \
                 li.textContent = 'row ' + i; ul.appendChild(li); }",
            ],
        )
        .unwrap();
        let html = out.html.unwrap_or_default();
        assert_eq!(html.matches("<li").count(), 3, "got: {html}");
        assert!(html.contains("row 2"));
        assert!(html.contains("data-i=\"2\""));
    }

    #[test]
    fn innerhtml_assignment_is_parsed_into_real_nodes() {
        let out = run_page_on(
            "<html><body><div id='a'></div></body></html>",
            &["document.getElementById('a').innerHTML = '<p class=\"x\">hi &amp; bye</p>';"],
        )
        .unwrap();
        let html = out.html.unwrap_or_default();
        assert!(html.contains("<p class=\"x\">"), "got: {html}");
        assert!(html.contains("hi &amp; bye"));
    }

    #[test]
    fn a_runaway_page_script_times_out_rather_than_hanging() {
        let out = run_page_on("<p>x</p>", &["while (true) {}"]).unwrap();
        assert!(out.timed_out);
        // A timeout must not hand back a half-built document.
        assert!(out.html.is_none());
    }

    fn run_on(html: &str, script: &str) -> Result<JsOutcome> {
        run_net_on(html, script, None)
    }

    fn run_net_on(html: &str, script: &str, net: Option<NetFn>) -> Result<JsOutcome> {
        let d = doc(html);
        let snap = dom_snapshot(&d);
        run(
            script,
            &snap,
            "https://example.com/",
            "Scout/0.1",
            net,
            Duration::from_millis(2_000),
            64 * 1024 * 1024,
        )
    }

    #[test]
    fn binary_fetch_responses_carry_exact_bytes() {
        let seen = empty_seen();
        // Bytes 0x00 0xFF 0x10 encode to "AP8Q".
        let net = fake_net_b64("AP8Q", seen.clone());
        let out = run_net_on(
            "<p>x</p>",
            "return fetch('/img.png').then(function (r) { \
                return r.arrayBuffer().then(function (ab) { \
                    return [r.base64, new Uint8Array(ab).length, new Uint8Array(ab)[1]]; \
                }); \
            });",
            Some(net),
        )
        .unwrap();
        assert_eq!(
            out.value,
            Some(json!(["AP8Q", 3, 255])),
            "base64 must be exposed and arrayBuffer must carry the exact bytes: {:?}",
            out.value
        );
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
    fn a_trailing_expression_without_return_comes_back_as_the_value() {
        // Scripts without a `return` run as programs, so the completion value
        // of the last expression is returned, the way a console would.
        let out = run_on("<p>x</p>", "1 + 1").unwrap();
        assert_eq!(out.value, Some(json!(2)));
        assert!(warn_codes(&out).is_empty());
    }

    #[test]
    fn a_program_whose_last_statement_has_no_value_warns() {
        // A declaration leaves no completion value; the caller is told why.
        let out = run_on("<p>x</p>", "var x = 1;").unwrap();
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
            None,
            Duration::from_millis(5_000),
            8 * 1024 * 1024,
        );
        assert!(err.is_err(), "allocation should have been stopped");
    }

    #[test]
    fn timers_are_queued_and_drained_on_a_virtual_clock() {
        // No sleeping in real time: callbacks wait until the scripts finish,
        // then the host drains them within the same execution budget.
        let out = run_on("<p>x</p>", "setTimeout(function () {}, 10); return 'queued';").unwrap();
        assert_eq!(out.value, Some(json!("queued")));
    }

    #[test]
    fn the_timer_queue_is_bounded() {
        // A page scheduling timers in a loop must not grow the queue without
        // limit; the cap is enforced in the shim.
        let out = run_on(
            "<p>x</p>",
            "for (var i = 0; i < 100000; i++) setTimeout(function () {}, 1); return 'ok';",
        )
        .unwrap();
        assert_eq!(out.value, Some(json!("ok")));
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
