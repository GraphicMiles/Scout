import { ScoutError } from './errors.js';

// Tolerant, bounded HTML tokenizer + tree builder. NOT a full HTML5 tree-construction implementation:
// no foster parenting, no adoption agency, no implied html/head/body. Swap for html5ever/lol_html in a Rust core.

const VOID = new Set(['area', 'base', 'br', 'col', 'embed', 'hr', 'img', 'input', 'link', 'meta', 'param', 'source', 'track', 'wbr']);
const RAW = new Set(['script', 'style']);
const RCDATA = new Set(['textarea', 'title']);
const P_CLOSERS = new Set('address article aside blockquote details div dl fieldset figcaption figure footer form h1 h2 h3 h4 h5 h6 header hr main nav ol p pre section table ul'.split(' '));
const IMPLIED = {
  li: ['li'], dt: ['dt', 'dd'], dd: ['dt', 'dd'], tr: ['tr', 'td', 'th'], td: ['td', 'th'], th: ['td', 'th'],
  thead: ['tr', 'td', 'th', 'tbody', 'tfoot'], tbody: ['thead', 'tbody', 'tfoot', 'tr', 'td', 'th'],
  tfoot: ['thead', 'tbody', 'tr', 'td', 'th'], option: ['option'], optgroup: ['option', 'optgroup'],
};
const SKIP_TEXT = new Set(['script', 'style', 'template']);
const BLOCK = new Set('address article aside blockquote br dd details div dl dt fieldset figcaption figure footer form h1 h2 h3 h4 h5 h6 header hr li main nav ol p pre section table tbody td tfoot th thead tr ul'.split(' '));

const NAMED = {
  amp: '&', lt: '<', gt: '>', quot: '"', apos: "'", nbsp: '\u00a0', copy: '©', reg: '®', trade: '™', hellip: '…',
  mdash: '—', ndash: '–', lsquo: '‘', rsquo: '’', ldquo: '“', rdquo: '”', bull: '•', middot: '·', euro: '€',
  pound: '£', yen: '¥', cent: '¢', times: '×', laquo: '«', raquo: '»', deg: '°',
};

export function decodeEntities(s) {
  if (!s.includes('&')) return s;
  return s.replace(/&(?:#(\d{1,7})|#[xX]([0-9a-fA-F]{1,6})|([a-zA-Z][a-zA-Z0-9]{1,8}));/g, (m, d, h, name) => {
    if (name) return NAMED[name] ?? m;
    const cp = d ? parseInt(d, 10) : parseInt(h, 16);
    if (cp === 0 || cp > 0x10ffff || (cp >= 0xd800 && cp <= 0xdfff)) return '\ufffd';
    return String.fromCodePoint(cp);
  });
}

const CLOSE = /<\/([a-zA-Z][^\s>\/]*)[^>]*>?/y;
const TAG = /<([a-zA-Z][^\s>\/]*)/y;
const ATTR = /([^\s"'<>\/=]+)(?:\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s>]*)))?/y;
const WS = new Set([' ', '\n', '\t', '\r', '\f']);

export function parseHtml(src, limits = {}) {
  const maxNodes = limits.maxNodes ?? 150000;
  const maxDepth = limits.maxDepth ?? 256;
  const root = { type: 'document', tag: '#document', attrs: Object.create(null), children: [], parent: null };
  const stack = [root];
  const n = src.length;
  let nodes = 0;
  let i = 0;
  const top = () => stack[stack.length - 1];
  const bump = () => {
    if (++nodes > maxNodes) throw new ScoutError('response_too_large', `Document exceeds the ${maxNodes} node limit`);
  };
  const addText = (t) => {
    if (!t) return;
    const p = top();
    const last = p.children[p.children.length - 1];
    if (last && last.type === 'text') { last.text += t; return; }
    bump();
    p.children.push({ type: 'text', text: t, parent: p });
  };

  while (i < n) {
    const lt = src.indexOf('<', i);
    if (lt === -1) { addText(decodeEntities(src.slice(i))); break; }
    if (lt > i) addText(decodeEntities(src.slice(i, lt)));
    const c1 = src.charCodeAt(lt + 1);

    if (src.startsWith('<!--', lt)) { const e = src.indexOf('-->', lt + 4); i = e < 0 ? n : e + 3; continue; }
    if (c1 === 33 || c1 === 63) { const e = src.indexOf('>', lt); i = e < 0 ? n : e + 1; continue; }
    if (c1 === 47) {
      CLOSE.lastIndex = lt;
      const m = CLOSE.exec(src);
      if (!m) { const e = src.indexOf('>', lt); i = e < 0 ? n : e + 1; continue; }
      i = CLOSE.lastIndex;
      const name = m[1].toLowerCase();
      let k = stack.length - 1;
      while (k > 0 && stack[k].tag !== name) k--;
      if (k > 0) stack.length = k;
      continue;
    }
    if ((c1 >= 65 && c1 <= 90) || (c1 >= 97 && c1 <= 122)) {
      TAG.lastIndex = lt;
      const tm = TAG.exec(src);
      const tag = tm[1].toLowerCase();
      let j = TAG.lastIndex;
      const attrs = Object.create(null);
      let selfClose = false;
      for (;;) {
        while (j < n && WS.has(src[j])) j++;
        if (j >= n) break;
        if (src[j] === '>') { j++; break; }
        if (src[j] === '/') { if (src[j + 1] === '>') { selfClose = true; j += 2; break; } j++; continue; }
        ATTR.lastIndex = j;
        const am = ATTR.exec(src);
        if (!am) { j++; continue; }
        j = ATTR.lastIndex;
        const name = am[1].toLowerCase();
        if (!(name in attrs)) attrs[name] = decodeEntities(am[2] ?? am[3] ?? am[4] ?? '');
      }
      i = j;

      const implied = IMPLIED[tag];
      if (implied) { while (stack.length > 1 && implied.includes(top().tag)) stack.pop(); }
      else if (P_CLOSERS.has(tag) && top().tag === 'p') stack.pop();

      bump();
      const parent = top();
      const el = { type: 'element', tag, attrs, children: [], parent };
      parent.children.push(el);

      if (RAW.has(tag) || RCDATA.has(tag)) {
        const re = new RegExp('</' + tag + '(?=[\\s/>])', 'ig');
        re.lastIndex = i;
        const m = re.exec(src);
        const end = m ? m.index : n;
        if (end > i) {
          bump();
          const t = src.slice(i, end);
          el.children.push({ type: 'text', text: RCDATA.has(tag) ? decodeEntities(t) : t, parent: el });
        }
        if (m) { const gt = src.indexOf('>', end); i = gt < 0 ? n : gt + 1; } else i = n;
        continue;
      }
      if (!VOID.has(tag) && !selfClose && stack.length < maxDepth) stack.push(el);
      continue;
    }
    addText('<');
    i = lt + 1;
  }
  return root;
}

export function* descendants(root) {
  const st = [];
  for (let i = root.children.length - 1; i >= 0; i--) st.push(root.children[i]);
  while (st.length) {
    const n = st.pop();
    if (n.type !== 'element') continue;
    yield n;
    for (let i = n.children.length - 1; i >= 0; i--) st.push(n.children[i]);
  }
}

/** Visible-ish text with whitespace collapsed; skips script/style/template. */
export function text(node) {
  const out = [];
  const st = [node];
  while (st.length) {
    const n = st.pop();
    if (typeof n === 'string') { out.push(n); continue; }
    if (n.type === 'text') { out.push(n.text); continue; }
    if (n !== node && SKIP_TEXT.has(n.tag)) continue;
    if (BLOCK.has(n.tag)) { out.push(' '); st.push(' '); }
    for (let k = n.children.length - 1; k >= 0; k--) st.push(n.children[k]);
  }
  return out.join('').replace(/\s+/g, ' ').trim();
}

/** Unnormalized concatenation of direct text children (script bodies, textarea values). */
export function rawText(node) {
  let s = '';
  for (const c of node.children) if (c.type === 'text') s += c.text;
  return s;
}

const esc = (s) => s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
const escA = (s) => s.replace(/&/g, '&amp;').replace(/"/g, '&quot;');
function outer(n) {
  if (n.type === 'text') return RAW.has(n.parent?.tag) ? n.text : esc(n.text);
  let s = '<' + n.tag;
  for (const [k, v] of Object.entries(n.attrs)) s += ` ${k}="${escA(v)}"`;
  s += '>';
  if (VOID.has(n.tag)) return s;
  for (const c of n.children) s += outer(c);
  return s + `</${n.tag}>`;
}
export const outerHTML = (n) => outer(n);
export function innerHTML(n) { let s = ''; for (const c of n.children) s += outer(c); return s; }
