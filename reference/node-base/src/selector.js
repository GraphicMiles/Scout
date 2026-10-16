import { ScoutError } from './errors.js';
import { descendants } from './html.js';

// CSS selector subset: type, *, #id, .class, [attr], [attr=v] (~= |= ^= $= *=, optional i flag),
// :first-child :last-child :only-child :nth-child(an+b|odd|even) :not(...), combinators (space > + ~), groups (,).
// Anything else raises invalid_request rather than silently matching nothing.

const IDENT = /[\w\u00a0-\uffff-]+/y;
const ATTR = /\[\s*([\w:.-]+)\s*(?:([~|^$*]?=)\s*(?:"([^"]*)"|'([^']*)'|([^\s\]]+))\s*(i)?\s*)?\]/y;
const cache = new Map();

const err = (sel, msg) => { throw new ScoutError('invalid_request', `Invalid CSS selector "${sel}": ${msg}`, { details: { field: 'selector' } }); };

function parseNth(arg) {
  if (arg == null) return null;
  const s = arg.trim().toLowerCase().replace(/\s+/g, '');
  if (s === 'odd') return [2, 1];
  if (s === 'even') return [2, 0];
  if (/^[+-]?\d+$/.test(s)) return [0, parseInt(s, 10)];
  const m = /^([+-]?\d*)n([+-]\d+)?$/.exec(s);
  if (!m) return null;
  const a = m[1] === '' || m[1] === '+' ? 1 : m[1] === '-' ? -1 : parseInt(m[1], 10);
  return [a, m[2] ? parseInt(m[2], 10) : 0];
}

function buildPseudo(sel, name, arg) {
  switch (name) {
    case 'first-child': case 'last-child': case 'only-child':
      if (arg !== null) err(sel, `:${name} takes no argument`);
      return { name };
    case 'nth-child': {
      const ab = parseNth(arg);
      if (!ab) err(sel, 'bad :nth-child argument');
      return { name, a: ab[0], b: ab[1] };
    }
    case 'not':
      if (!arg || !arg.trim()) err(sel, ':not() needs an argument');
      return { name, groups: parseSelector(arg) };
    default:
      return err(sel, `pseudo-class :${name} is not supported`);
  }
}

export function parseSelector(sel) {
  if (typeof sel !== 'string' || !sel.trim()) throw new ScoutError('invalid_request', 'Selector must be a non-empty string', { details: { field: 'selector' } });
  if (sel.length > 512) err(sel.slice(0, 40) + '…', 'selector is longer than 512 characters');
  const hit = cache.get(sel);
  if (hit) return hit;

  const n = sel.length;
  let i = 0;
  const groups = [];
  let parts = [];
  let comb = null;
  let cur = null;
  const flush = () => {
    if (cur) { parts.push({ comb: parts.length ? comb || ' ' : null, compound: cur }); cur = null; comb = null; }
  };
  const ident = () => {
    IDENT.lastIndex = i;
    const m = IDENT.exec(sel);
    if (!m) err(sel, `expected an identifier at position ${i}`);
    i += m[0].length;
    return m[0];
  };
  const skipWs = () => { while (i < n && /\s/.test(sel[i])) i++; };

  while (i < n) {
    const c = sel[i];
    if (c === ',') {
      flush();
      if (!parts.length) err(sel, 'empty selector in list');
      groups.push(parts); parts = []; comb = null; i++; skipWs();
      continue;
    }
    if (/\s/.test(c)) {
      flush(); skipWs();
      if (parts.length && i < n && sel[i] !== ',' && !'>+~'.includes(sel[i])) comb = ' ';
      continue;
    }
    if (c === '>' || c === '+' || c === '~') {
      flush();
      if (!parts.length) err(sel, 'dangling combinator');
      comb = c; i++; skipWs();
      continue;
    }
    if (!cur) cur = { tag: null, id: null, classes: [], attrs: [], pseudos: [] };
    if (c === '*') { cur.tag = '*'; i++; }
    else if (c === '#') { i++; cur.id = ident(); }
    else if (c === '.') { i++; cur.classes.push(ident()); }
    else if (c === '[') {
      ATTR.lastIndex = i;
      const m = ATTR.exec(sel);
      if (!m) err(sel, `bad attribute selector at position ${i}`);
      cur.attrs.push({ name: m[1].toLowerCase(), op: m[2] || null, value: m[3] ?? m[4] ?? m[5] ?? '', ci: !!m[6] });
      i = ATTR.lastIndex;
    } else if (c === ':') {
      i++;
      if (sel[i] === ':') err(sel, 'pseudo-elements are not supported');
      const name = ident().toLowerCase();
      let arg = null;
      if (sel[i] === '(') {
        let depth = 1; let j = i + 1;
        while (j < n && depth) { if (sel[j] === '(') depth++; else if (sel[j] === ')') depth--; j++; }
        if (depth) err(sel, 'unclosed parenthesis');
        arg = sel.slice(i + 1, j - 1); i = j;
      }
      cur.pseudos.push(buildPseudo(sel, name, arg));
    } else if (/[\w\u00a0-\uffff-]/.test(c)) {
      if (cur.tag || cur.id || cur.classes.length || cur.attrs.length || cur.pseudos.length) err(sel, `unexpected "${c}" at position ${i}`);
      cur.tag = ident().toLowerCase();
    } else err(sel, `unexpected "${c}" at position ${i}`);
  }
  flush();
  if (comb && comb !== ' ') err(sel, 'dangling combinator');
  if (!parts.length) err(sel, 'empty selector');
  groups.push(parts);
  if (cache.size > 200) cache.clear();
  cache.set(sel, groups);
  return groups;
}

const elSiblings = (el) => el.parent.children.filter((c) => c.type === 'element');
const prevEl = (el) => {
  const sib = el.parent.children;
  for (let k = sib.indexOf(el) - 1; k >= 0; k--) if (sib[k].type === 'element') return sib[k];
  return null;
};

function matchNth(a, b, idx) {
  if (a === 0) return idx === b;
  const q = (idx - b) / a;
  return Number.isInteger(q) && q >= 0;
}

function matchPseudo(el, p) {
  switch (p.name) {
    case 'first-child': return elSiblings(el)[0] === el;
    case 'last-child': { const s = elSiblings(el); return s[s.length - 1] === el; }
    case 'only-child': return elSiblings(el).length === 1;
    case 'nth-child': return matchNth(p.a, p.b, elSiblings(el).indexOf(el) + 1);
    case 'not': return !p.groups.some((g) => matchComplex(el, g, g.length - 1));
    default: return false;
  }
}

function matchCompound(el, c) {
  if (c.tag && c.tag !== '*' && el.tag !== c.tag) return false;
  if (c.id !== null && el.attrs.id !== c.id) return false;
  if (c.classes.length) {
    const cl = el.attrs.class;
    if (!cl) return false;
    const set = cl.split(/\s+/);
    for (const k of c.classes) if (!set.includes(k)) return false;
  }
  for (const a of c.attrs) {
    let v = el.attrs[a.name];
    if (v === undefined) return false;
    if (!a.op) continue;
    let w = a.value;
    if (a.ci) { v = v.toLowerCase(); w = w.toLowerCase(); }
    switch (a.op) {
      case '=': if (v !== w) return false; break;
      case '~=': if (!w || !v.split(/\s+/).includes(w)) return false; break;
      case '|=': if (v !== w && !v.startsWith(w + '-')) return false; break;
      case '^=': if (!w || !v.startsWith(w)) return false; break;
      case '$=': if (!w || !v.endsWith(w)) return false; break;
      case '*=': if (!w || !v.includes(w)) return false; break;
    }
  }
  for (const p of c.pseudos) if (!matchPseudo(el, p)) return false;
  return true;
}

function matchComplex(el, parts, idx) {
  if (!matchCompound(el, parts[idx].compound)) return false;
  if (idx === 0) return true;
  const comb = parts[idx].comb;
  if (comb === '>') { const p = el.parent; return !!p && p.type === 'element' && matchComplex(p, parts, idx - 1); }
  if (comb === ' ') {
    for (let p = el.parent; p && p.type === 'element'; p = p.parent) if (matchComplex(p, parts, idx - 1)) return true;
    return false;
  }
  if (comb === '+') { const s = prevEl(el); return !!s && matchComplex(s, parts, idx - 1); }
  for (let s = prevEl(el); s; s = prevEl(s)) if (matchComplex(s, parts, idx - 1)) return true; // '~'
  return false;
}

export function querySelectorAll(root, sel, limit = Infinity) {
  const groups = parseSelector(sel);
  const out = [];
  for (const el of descendants(root)) {
    if (groups.some((g) => matchComplex(el, g, g.length - 1))) {
      out.push(el);
      if (out.length >= limit) break;
    }
  }
  return out;
}
export const querySelector = (root, sel) => querySelectorAll(root, sel, 1)[0] ?? null;

export function closest(el, tag) {
  for (let p = el; p && p.type === 'element'; p = p.parent) if (p.tag === tag) return p;
  return null;
}
