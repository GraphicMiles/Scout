import { querySelector, querySelectorAll } from './selector.js';
import { text, innerHTML, outerHTML, rawText } from './html.js';
import { resolveUrl } from './util.js';

export function baseUrlOf(doc, pageUrl) {
  const b = querySelector(doc, 'base[href]');
  return (b && resolveUrl(b.attrs.href, pageUrl)) || pageUrl;
}

const SYMBOLS = { '$': 'USD', '€': 'EUR', '£': 'GBP', '₦': 'NGN', '¥': 'JPY', '₹': 'INR' };
export function parsePrice(raw) {
  // US-style separators only ("1,234.50"); European "1.234,50" is not interpreted.
  const m = /(USD|EUR|GBP|NGN|JPY|INR|[$€£₦¥₹])?\s*(\d{1,3}(?:[,\s]\d{3})+(?:\.\d+)?|\d+(?:\.\d+)?)\s*(USD|EUR|GBP|NGN|JPY|INR)?/.exec(raw);
  if (!m) return null;
  const cur = m[1] ?? m[3] ?? null;
  return { amount: Number(m[2].replace(/[,\s]/g, '')), currency: cur ? SYMBOLS[cur] ?? cur : null, raw: m[0].trim() };
}
function parseNumber(raw) {
  const m = /-?\d[\d,]*(?:\.\d+)?/.exec(raw);
  return m ? Number(m[0].replace(/,/g, '')) : null;
}
function parseDate(raw) {
  const t = Date.parse(raw);
  return Number.isFinite(t) ? new Date(t).toISOString() : null;
}

export function readValue(el, spec, base) {
  let attr = spec.attr;
  if (!attr) attr = spec.type === 'url' ? (el.attrs.href !== undefined ? 'href' : el.attrs.src !== undefined ? 'src' : 'text')
    : spec.type === 'date' && el.attrs.datetime !== undefined ? 'datetime' : 'text';
  let raw;
  if (attr === 'text') raw = text(el);
  else if (attr === 'html') raw = innerHTML(el);
  else if (attr === 'outerHtml') raw = outerHTML(el);
  else raw = el.attrs[attr] ?? null;
  if (raw == null) return null;
  switch (spec.type) {
    case 'url': return resolveUrl(raw, base);
    case 'number': return parseNumber(raw);
    case 'price': return parsePrice(raw);
    case 'date': return parseDate(raw);
    default: return raw;
  }
}

/** Schema: { item?: css, fields: { name: { selector?, attr?, type?, multiple? } } } (already validated). */
export function extractBySchema(doc, schema, { base, limit }) {
  const warnings = [];
  const read = (scope) => {
    const rec = {};
    for (const [k, f] of Object.entries(schema.fields)) {
      if (f.multiple) {
        const els = f.selector ? querySelectorAll(scope, f.selector, 200) : [scope];
        rec[k] = els.map((e) => readValue(e, f, base)).filter((v) => v != null);
      } else {
        const el = f.selector ? querySelector(scope, f.selector) : scope;
        rec[k] = el ? readValue(el, f, base) : null;
      }
    }
    return rec;
  };
  let data;
  if (schema.item) {
    const items = querySelectorAll(doc, schema.item, limit);
    if (!items.length) warnings.push({ code: 'no_match', message: `Item selector "${schema.item}" matched nothing` });
    data = items.map(read);
  } else data = read(doc);
  const rows = Array.isArray(data) ? data : [data];
  if (rows.length) {
    for (const k of Object.keys(schema.fields)) {
      if (rows.every((r) => r[k] == null || (Array.isArray(r[k]) && !r[k].length))) {
        warnings.push({ code: 'field_empty', message: `Field "${k}" produced no value in any record`, field: k });
      }
    }
  }
  return { data, warnings };
}

function genericPage({ page, limit }) {
  if (page.kind === 'json') return { data: { json: page.json } };
  const doc = page.doc;
  const base = baseUrlOf(doc, page.url);
  const meta = (sel) => { const e = querySelector(doc, sel); return e ? e.attrs.content ?? null : null; };
  const openGraph = {};
  for (const m of querySelectorAll(doc, 'meta[property^="og:"]', 40)) openGraph[m.attrs.property.slice(3)] = m.attrs.content ?? null;
  const jsonLd = [];
  for (const s of querySelectorAll(doc, 'script[type="application/ld+json"]', 10)) {
    const raw = rawText(s);
    if (raw.length > 200000) continue;
    try { jsonLd.push(JSON.parse(raw)); } catch { /* ignore malformed block */ }
  }
  const links = [];
  for (const a of querySelectorAll(doc, 'a[href]')) {
    const url = resolveUrl(a.attrs.href, base);
    if (url && /^https?:/.test(url)) links.push({ text: text(a), url });
    if (links.length >= limit) break;
  }
  return {
    data: {
      title: text(querySelector(doc, 'title') ?? { type: 'text', text: '' }) || null,
      description: meta('meta[name="description"]'),
      canonical: querySelector(doc, 'link[rel="canonical"]')?.attrs.href ? resolveUrl(querySelector(doc, 'link[rel="canonical"]').attrs.href, base) : null,
      lang: querySelector(doc, 'html')?.attrs.lang ?? null,
      openGraph,
      headings: querySelectorAll(doc, 'h1, h2, h3', limit).map((h) => ({ level: Number(h.tag[1]), text: text(h) })),
      links,
      times: querySelectorAll(doc, 'time[datetime]', limit).map((t) => ({ datetime: t.attrs.datetime, text: text(t) })),
      jsonLd,
    },
  };
}

export function selectorsStrategy() {
  return {
    name: 'selectors', status: 'verified-fixtures', defaultLimit: 50,
    description: 'Extract fields using CSS selectors from a caller-supplied schema.',
    run({ page, limit, schema }) {
      if (page.kind !== 'html') return { data: null, warnings: [{ code: 'not_html', message: 'Selector extraction needs an HTML page' }] };
      return extractBySchema(page.doc, schema, { base: baseUrlOf(page.doc, page.url), limit });
    },
  };
}

export function genericPageStrategy() {
  return {
    name: 'generic-page', status: 'verified-fixtures', defaultLimit: 50,
    description: 'Title, description, canonical URL, Open Graph, headings, links, <time> elements and JSON-LD.',
    run: genericPage,
  };
}
