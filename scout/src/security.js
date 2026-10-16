import net from 'node:net';
import dns from 'node:dns';
import http from 'node:http';
import https from 'node:https';
import zlib from 'node:zlib';
import { ScoutError, abortError } from './errors.js';

// ---------- destination policy (SSRF) ----------
const blocked = new net.BlockList();
for (const [a, p] of [
  ['0.0.0.0', 8], ['10.0.0.0', 8], ['100.64.0.0', 10], ['127.0.0.0', 8], ['169.254.0.0', 16], ['172.16.0.0', 12],
  ['192.0.0.0', 24], ['192.0.2.0', 24], ['192.88.99.0', 24], ['192.168.0.0', 16], ['198.18.0.0', 15],
  ['198.51.100.0', 24], ['203.0.113.0', 24], ['224.0.0.0', 4], ['240.0.0.0', 4],
]) blocked.addSubnet(a, p, 'ipv4');
for (const [a, p] of [
  ['::', 128], ['::1', 128], ['64:ff9b::', 96], ['100::', 64], ['2001::', 32],
  ['2001:db8::', 32], ['2002::', 16], ['fc00::', 7], ['fe80::', 10], ['fec0::', 10], ['ff00::', 8],
]) blocked.addSubnet(a, p, 'ipv6');

// IPv4-mapped IPv6 (::ffff:a.b.c.d) is never a legitimate DNS answer. Kept in its own list because
// BlockList.check() also matches plain IPv4 addresses against ::ffff:0:0/96 rules.
const mapped = new net.BlockList();
mapped.addSubnet('::ffff:0:0', 96, 'ipv6');

export function isBlockedIp(ip) {
  if (net.isIPv4(ip)) return blocked.check(ip, 'ipv4');
  if (net.isIPv6(ip)) return blocked.check(ip, 'ipv6') || mapped.check(ip, 'ipv6');
  return true;
}

const deny = (msg) => new ScoutError('blocked_destination', msg);
const INTERNAL_SUFFIXES = ['.localhost', '.local', '.internal', '.localdomain', '.lan', '.home.arpa'];

export function validateUrl(raw, cfg) {
  let u;
  try { u = new URL(raw); } catch { throw new ScoutError('invalid_request', 'URL is not valid', { details: { field: 'url' } }); }
  if (u.protocol !== 'http:' && u.protocol !== 'https:') throw deny(`Scheme "${u.protocol}" is not permitted`);
  if (u.username || u.password) throw deny('URLs with embedded credentials are not permitted');
  const host = u.hostname.replace(/^\[|\]$/g, '').toLowerCase().replace(/\.$/, '');
  if (!cfg.privateAllowlist.includes(host)) {
    const port = Number(u.port || (u.protocol === 'https:' ? 443 : 80));
    if (!cfg.allowedPorts.includes(port)) throw deny(`Port ${port} is not permitted`);
    if (host === 'localhost' || host === 'metadata.google.internal' || INTERNAL_SUFFIXES.some((s) => host.endsWith(s))) throw deny('Host is not permitted');
    if (net.isIP(host)) { if (isBlockedIp(host)) throw deny('Destination address is in a blocked range'); }
    else if (!host.includes('.')) throw deny('Single-label hostnames are not permitted');
  }
  if (cfg.allowedDomains.length && !cfg.allowedDomains.some((d) => host === d || host.endsWith('.' + d))) throw deny('Host is not in the allowed domain list');
  return u;
}

/** dns lookup that validates every resolved address at connect time (defeats DNS rebinding). */
export function makeLookup(cfg) {
  const resolve = cfg.resolver ?? dns.lookup;
  return (hostname, options, cb) => {
    if (typeof options === 'function') { cb = options; options = {}; }
    resolve(hostname, { ...options, all: true }, (err, addrs) => {
      if (err) return cb(err);
      const allow = cfg.privateAllowlist.includes(String(hostname).toLowerCase());
      if (!allow) for (const a of addrs) if (isBlockedIp(a.address)) return cb(deny('Hostname resolves to a blocked address'));
      if (options.all) return cb(null, addrs);
      cb(null, addrs[0].address, addrs[0].family);
    });
  };
}

// ---------- guarded HTTP fetch ----------
const REDIRECT = new Set([301, 302, 303, 307, 308]);
const textual = (ct) => !ct || ct.startsWith('text/') || ct.includes('json') || ct.includes('xml');

function mapNetError(e) {
  if (e instanceof ScoutError) return e;
  const code = e?.code;
  if (code === 'ENOTFOUND') return new ScoutError('navigation_failed', 'DNS lookup failed for the requested host', { details: { cause: code } });
  if (code === 'ECONNREFUSED') return new ScoutError('navigation_failed', 'Connection refused', { details: { cause: code } });
  if (typeof code === 'string' && /CERT|TLS|SSL|HOSTNAME_MISMATCH/.test(code)) return new ScoutError('navigation_failed', 'TLS verification failed', { details: { cause: code } });
  return new ScoutError('navigation_failed', 'Network error while fetching the page', { retryable: true, details: { cause: code ?? e?.name } });
}

function requestOnce(cfg, u, { method, headers, body, signal, deadline, maxBytes }) {
  return new Promise((resolve, reject) => {
    let settled = false; let req; let timer; let onAbort;
    const finish = (err, val) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      signal?.removeEventListener('abort', onAbort);
      if (err) { try { req?.destroy(); } catch { /* ignore */ } reject(mapNetError(err)); } else resolve(val);
    };
    const remaining = deadline - Date.now();
    if (remaining <= 0) return finish(new ScoutError('navigation_timeout', 'Navigation time budget exhausted', { retryable: true }));
    if (signal?.aborted) return finish(abortError(signal));
    onAbort = () => finish(abortError(signal));
    signal?.addEventListener('abort', onAbort, { once: true });
    timer = setTimeout(() => finish(new ScoutError('navigation_timeout', 'Navigation exceeded its time limit', { retryable: true })), remaining);

    const mod = u.protocol === 'https:' ? https : http;
    req = mod.request({
      protocol: u.protocol, hostname: u.hostname.replace(/^\[|\]$/g, ''), port: u.port || undefined,
      path: u.pathname + u.search, method, headers, lookup: makeLookup(cfg), agent: false,
    }, (res) => {
      const status = res.statusCode;
      if (REDIRECT.has(status) && res.headers.location) { res.resume(); return finish(null, { status, redirect: res.headers.location }); }
      const ct = (res.headers['content-type'] || '').split(';')[0].trim().toLowerCase();
      if (!textual(ct)) return finish(new ScoutError('unsupported_content', `Content type "${ct}" is not downloaded (documents and data only)`, { details: { contentType: ct } }));
      const enc = (res.headers['content-encoding'] || '').toLowerCase();
      let stream = res;
      if (enc === 'gzip' || enc === 'x-gzip') stream = res.pipe(zlib.createGunzip());
      else if (enc === 'deflate') stream = res.pipe(zlib.createInflate());
      else if (enc === 'br') stream = res.pipe(zlib.createBrotliDecompress());
      else if (enc && enc !== 'identity') return finish(new ScoutError('unsupported_content', `Content-Encoding "${enc}" is not supported`));
      const declared = Number(res.headers['content-length']);
      if (!enc && declared > maxBytes) return finish(new ScoutError('response_too_large', `Response exceeds ${maxBytes} bytes`));
      const chunks = []; let size = 0;
      stream.on('data', (c) => {
        size += c.length;
        if (size > maxBytes) { finish(new ScoutError('response_too_large', `Response exceeds ${maxBytes} bytes`)); stream.destroy(); }
        else chunks.push(c);
      });
      stream.on('end', () => finish(null, { status, headers: res.headers, body: Buffer.concat(chunks), contentType: ct, bytes: size }));
      stream.on('error', finish);
      res.on('error', finish);
      res.on('aborted', () => finish(new ScoutError('navigation_failed', 'Connection closed before the response completed', { retryable: true })));
    });
    req.on('error', finish);
    if (body !== undefined) req.write(body);
    req.end();
  });
}

function decodeBody(buf, contentTypeHeader) {
  let charset = /charset=["']?([\w-]+)/i.exec(contentTypeHeader || '')?.[1];
  if (!charset) charset = /<meta[^>]+charset=["']?([\w-]+)/i.exec(buf.subarray(0, 2048).toString('latin1'))?.[1];
  try { return new TextDecoder(charset || 'utf-8').decode(buf); } catch { return new TextDecoder('utf-8').decode(buf); }
}

/**
 * Fetch one document. Every hop (initial URL and each redirect) is validated; connections are
 * validated again at DNS-resolution time. Only textual document types are downloaded.
 */
export async function guardedFetch(cfg, { url, method = 'GET', headers = {}, body, jar, signal, timeoutMs, maxBytes }) {
  const deadline = Date.now() + (timeoutMs ?? cfg.navTimeoutMs);
  const limit = maxBytes ?? cfg.maxResponseBytes;
  let current = validateUrl(url, cfg);
  let m = method; let b = body; const h = { ...headers };
  const redirects = [];
  for (let hop = 0; ; hop++) {
    if (signal?.aborted) throw abortError(signal);
    const reqHeaders = {
      'user-agent': cfg.userAgent,
      accept: 'text/html,application/xhtml+xml,application/json;q=0.9,*/*;q=0.5',
      'accept-encoding': 'gzip, deflate, br',
      'accept-language': 'en-US,en;q=0.8',
      connection: 'close',
      ...h,
    };
    const cookie = jar?.header(current);
    if (cookie) reqHeaders.cookie = cookie;
    if (b !== undefined) reqHeaders['content-length'] = Buffer.byteLength(b);
    const res = await requestOnce(cfg, current, { method: m, headers: reqHeaders, body: b, signal, deadline, maxBytes: limit });
    jar?.store(current, res.headers?.['set-cookie']);
    if (res.redirect !== undefined) {
      if (hop >= cfg.maxRedirects) throw new ScoutError('navigation_failed', `Too many redirects (limit ${cfg.maxRedirects})`, { details: { redirects } });
      let next;
      try { next = new URL(res.redirect, current); } catch { throw new ScoutError('navigation_failed', 'Server sent an invalid redirect location'); }
      try { next = validateUrl(next.href, cfg); } catch (e) {
        if (e instanceof ScoutError && e.code === 'blocked_destination') throw new ScoutError('blocked_destination', `Redirect to ${next.href} blocked: ${e.message}`);
        throw e;
      }
      redirects.push({ from: current.href, to: next.href, status: res.status });
      if (next.origin !== current.origin) { delete h.authorization; delete h.referer; }
      if (res.status === 303 || ((res.status === 301 || res.status === 302) && m === 'POST')) { m = 'GET'; b = undefined; delete h['content-type']; }
      current = next;
      continue;
    }
    const { 'set-cookie': _drop, ...safeHeaders } = res.headers;
    return {
      url: current.href, status: res.status, headers: safeHeaders, contentType: res.contentType,
      body: decodeBody(res.body, res.headers['content-type']), bytes: res.bytes, redirects,
    };
  }
}
