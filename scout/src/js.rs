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
      get href() { return attrs.href || ''; },
      set href(v) { attrs.href = String(v); },
      get src() { return attrs.src || ''; },
      set src(v) { attrs.src = String(v); },
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

/// What running a page's scripts produced.
pub struct PageRunOutcome {
    /// The mutated document, serialised back to HTML. `None` when execution was
    /// disabled or nothing ran.
    pub html: Option<String>,
    pub logs: Vec<String>,
    pub scripts_run: usize,
    pub scripts_failed: usize,
    pub timers_run: u32,
    pub timers_pending: u32,
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
    timeout: Duration,
    max_memory: usize,
    timer_budget: u32,
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
        }

        // Deferred writes are common, so drain the virtual clock after the
        // synchronous scripts have all run.
        // Marshalled to JSON rather than poked at as a JS object: the drain
        // result is small, and this cannot fail silently on an API mismatch.
        let timers: Option<String> = ctx
            .eval::<String, _>(
                format!("JSON.stringify(__scout_drain_timers({}))", timer_budget).as_str(),
            )
            .ok();
        timer_counts.set(parse_timer_result(timers.as_deref()));

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
        let d = doc(html);
        let snap = dom_snapshot(&d);
        run_page(
            &scripts.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            &snap,
            "https://example.com/",
            "Scout/0.1",
            Duration::from_millis(2_000),
            64 * 1024 * 1024,
            64,
        )
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
