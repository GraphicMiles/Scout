import net from 'node:net';

const MAX_COOKIES = 200;
const MAX_SIZE = 4096;
const domainMatch = (host, dom) => host === dom || (host.endsWith('.' + dom) && !net.isIP(host));
const pathMatch = (req, cp) => req === cp || (req.startsWith(cp) && (cp.endsWith('/') || req[cp.length] === '/'));
const defaultPath = (p) => { if (!p || p[0] !== '/') return '/'; const i = p.lastIndexOf('/'); return i <= 0 ? '/' : p.slice(0, i); };

/** Per-session cookie jar. Simplified RFC 6265: no public-suffix list (see README). Never logged. */
export class CookieJar {
  #m = new Map();
  constructor(now = Date.now) { this.now = now; }
  get size() { return this.#m.size; }
  clear() { this.#m.clear(); }

  store(url, setCookie) {
    if (!setCookie) return;
    const u = url instanceof URL ? url : new URL(url);
    const host = u.hostname.replace(/^\[|\]$/g, '').toLowerCase();
    for (const line of Array.isArray(setCookie) ? setCookie : [setCookie]) {
      const c = this.#parse(line, u, host);
      if (!c) continue;
      const key = `${c.domain}|${c.path}|${c.name}`;
      if (c.expires !== null && c.expires <= this.now()) { this.#m.delete(key); continue; }
      if (!this.#m.has(key) && this.#m.size >= MAX_COOKIES) this.#m.delete(this.#m.keys().next().value);
      this.#m.set(key, c);
    }
  }

  #parse(line, u, host) {
    if (typeof line !== 'string' || line.length > MAX_SIZE) return null;
    const [first, ...attrs] = line.split(';');
    const eq = first.indexOf('=');
    if (eq < 1) return null;
    const c = { name: first.slice(0, eq).trim(), value: first.slice(eq + 1).trim(), domain: host, hostOnly: true, path: defaultPath(u.pathname), secure: false, expires: null };
    if (!c.name) return null;
    let maxAge = null;
    for (const a of attrs) {
      const i = a.indexOf('=');
      const k = (i < 0 ? a : a.slice(0, i)).trim().toLowerCase();
      const v = i < 0 ? '' : a.slice(i + 1).trim();
      if (k === 'domain' && v) {
        const d = v.replace(/^\./, '').toLowerCase();
        if (!d.includes('.') || !domainMatch(host, d)) return null; // reject cross-domain and bare-TLD cookies
        c.domain = d; c.hostOnly = false;
      } else if (k === 'path' && v.startsWith('/')) c.path = v;
      else if (k === 'secure') c.secure = true;
      else if (k === 'max-age') { const n = parseInt(v, 10); if (Number.isFinite(n)) maxAge = n; }
      else if (k === 'expires') { const t = Date.parse(v); if (Number.isFinite(t)) c.expires = t; }
    }
    if (maxAge !== null) c.expires = this.now() + maxAge * 1000;
    if (c.secure && u.protocol !== 'https:') return null;
    return c;
  }

  header(url) {
    const u = url instanceof URL ? url : new URL(url);
    const host = u.hostname.replace(/^\[|\]$/g, '').toLowerCase();
    const now = this.now();
    const hits = [];
    for (const [key, c] of this.#m) {
      if (c.expires !== null && c.expires <= now) { this.#m.delete(key); continue; }
      if (!(c.hostOnly ? host === c.domain : domainMatch(host, c.domain))) continue;
      if (!pathMatch(u.pathname || '/', c.path)) continue;
      if (c.secure && u.protocol !== 'https:') continue;
      hits.push(c);
    }
    hits.sort((a, b) => b.path.length - a.path.length);
    return hits.map((c) => `${c.name}=${c.value}`).join('; ');
  }
}
