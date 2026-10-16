import http from 'node:http';
import crypto from 'node:crypto';
import { ScoutError, toScoutError } from './errors.js';
import { Scout } from './scout.js';
import { createLogger } from './logger.js';
import { RateLimiter } from './ratelimit.js';
import * as v from './validate.js';

const sha = (s) => crypto.createHash('sha256').update(s).digest();

export function createServer(scout, cfg, log = createLogger(cfg.logLevel)) {
  const limiter = new RateLimiter(cfg.rateLimitPerMin);
  const failLimiter = new RateLimiter(20); // auth failures per remote address per minute
  const keys = cfg.apiKeys.map((k) => ({ id: 'key_' + sha(k).toString('hex').slice(0, 8), digest: sha(k) }));
  const metricsDigest = cfg.metricsToken ? sha(cfg.metricsToken) : null;
  let shuttingDown = false;

  const bearer = (req) => {
    const h = req.headers.authorization;
    return (typeof h === 'string' && h.startsWith('Bearer ') ? h.slice(7).trim() : req.headers['x-api-key']) || null;
  };
  const unauthorized = () => new ScoutError('unauthorized', 'Missing or invalid API key');
  function authenticate(req) {
    if (cfg.devMode && keys.length === 0) return 'dev';
    const key = bearer(req);
    if (!key || typeof key !== 'string') throw unauthorized();
    const d = sha(key);
    let hit = null;
    for (const k of keys) if (crypto.timingSafeEqual(d, k.digest)) hit = k.id; // no early exit
    if (!hit) throw unauthorized();
    return hit;
  }
  function authMetrics(req) {
    if (metricsDigest) {
      const key = bearer(req);
      if (!key || !crypto.timingSafeEqual(sha(key), metricsDigest)) throw unauthorized();
      return 'metrics';
    }
    return authenticate(req);
  }

  const routes = [
    ['GET', /^\/health$/, 'health', 'public', async () => ({ status: shuttingDown ? 503 : 200, body: { status: shuttingDown ? 'shutting_down' : 'ok', version: cfg.version, uptimeSec: Math.round(process.uptime()) } })],
    ['GET', /^\/metrics$/, 'metrics', 'metrics', async () => ({ status: 200, raw: scout.metrics.render(), contentType: 'text/plain; version=0.0.4' })],
    ['GET', /^\/v1\/strategies$/, 'strategies', 'api', async () => ({ status: 200, body: { strategies: scout.listStrategies() } })],
    ['POST', /^\/v1\/sessions$/, 'sessions.create', 'api', async ({ owner, body }) => { v.parseCreateSession(body); return { status: 201, body: scout.createSession(owner) }; }],
    ['DELETE', /^\/v1\/sessions\/(?<id>[A-Za-z0-9_]+)$/, 'sessions.delete', 'api', async ({ owner, params }) => { scout.destroySession(owner, params.id); return { status: 204 }; }],
    ['POST', /^\/v1\/navigate$/, 'navigate', 'api', async ({ owner, body, signal }) => ({ status: 200, body: await scout.navigate(owner, v.parseNavigate(body, cfg), signal) })],
    ['POST', /^\/v1\/extract$/, 'extract', 'api', async ({ owner, body, signal }) => ({ status: 200, body: await scout.extract(owner, v.parseExtract(body, cfg), signal) })],
    ['POST', /^\/v1\/interact$/, 'interact', 'api', async ({ owner, body, signal }) => ({ status: 200, body: await scout.interact(owner, v.parseInteract(body), signal) })],
    ['POST', /^\/v1\/tasks$/, 'tasks.create', 'api', async ({ owner, body }) => {
      const t = scout.submitTask(owner, v.parseTask(body, cfg));
      return { status: 202, body: t, headers: { location: `/v1/tasks/${t.id}` } };
    }],
    ['GET', /^\/v1\/tasks\/(?<id>[A-Za-z0-9_]+)$/, 'tasks.get', 'api', async ({ owner, params }) => ({ status: 200, body: scout.getTask(owner, params.id) })],
    ['POST', /^\/v1\/tasks\/(?<id>[A-Za-z0-9_]+)\/cancel$/, 'tasks.cancel', 'api', async ({ owner, params }) => ({ status: 200, body: scout.cancelTask(owner, params.id) })],
  ];

  function match(method, path) {
    const allowed = [];
    for (const r of routes) {
      const m = r[1].exec(path);
      if (!m) continue;
      if (r[0] === method) return { route: r, params: m.groups ?? {} };
      allowed.push(r[0]);
    }
    if (allowed.length) throw Object.assign(new ScoutError('method_not_allowed', 'Method not allowed'), { allow: allowed.join(', ') });
    throw new ScoutError('not_found', 'No such endpoint');
  }

  function readJson(req, max) {
    return new Promise((resolve, reject) => {
      const declared = Number(req.headers['content-length']);
      if (declared > max) return reject(new ScoutError('payload_too_large', `Body exceeds ${max} bytes`));
      const chunks = []; let size = 0; let over = false;
      req.on('data', (c) => {
        if (over) return;
        size += c.length;
        if (size > max) { over = true; chunks.length = 0; reject(new ScoutError('payload_too_large', `Body exceeds ${max} bytes`)); }
        else chunks.push(c);
      });
      req.on('end', () => {
        if (over) return;
        const raw = Buffer.concat(chunks).toString('utf8');
        if (!raw.trim()) return resolve({});
        const ct = (req.headers['content-type'] || '').toLowerCase();
        if (!ct.includes('json')) return reject(new ScoutError('invalid_request', 'Content-Type must be application/json'));
        try { resolve(JSON.parse(raw)); } catch { reject(new ScoutError('invalid_request', 'Body is not valid JSON')); }
      });
      req.on('error', () => reject(new ScoutError('cancelled', 'Request stream failed')));
    });
  }

  function send(res, status, body, { headers = {}, raw, contentType } = {}) {
    if (res.writableEnded || res.destroyed) return;
    const payload = raw ?? (body === undefined ? '' : JSON.stringify(body));
    res.writeHead(status, { 'content-type': contentType ?? 'application/json; charset=utf-8', ...(status === 204 ? {} : { 'content-length': Buffer.byteLength(payload) }), ...headers });
    res.end(status === 204 ? undefined : payload);
  }

  const server = http.createServer({ maxHeaderSize: 16 * 1024 }, async (req, res) => {
    const t0 = Date.now();
    const requestId = 'req_' + crypto.randomBytes(6).toString('hex');
    res.setHeader('x-request-id', requestId);
    res.setHeader('cache-control', 'no-store');
    res.setHeader('x-content-type-options', 'nosniff');
    const ac = new AbortController();
    res.on('close', () => { if (!res.writableEnded) ac.abort(new ScoutError('cancelled', 'Client disconnected')); });
    let owner = null; let status = 500; let routeName = 'unmatched';
    try {
      const path = new URL(req.url, 'http://localhost').pathname;
      const { route, params } = match(req.method, path);
      routeName = route[2];
      if (route[3] !== 'public') {
        const ip = req.socket.remoteAddress ?? 'unknown';
        if (!failLimiter.peek(ip)) throw new ScoutError('rate_limited', 'Too many failed authentication attempts', { retryable: true, retryAfterSec: 30 });
        try { owner = route[3] === 'metrics' ? authMetrics(req) : authenticate(req); } catch (e) { failLimiter.take(ip); throw e; }
        if (route[3] === 'api') {
          const rl = limiter.take(owner);
          if (!rl.ok) throw new ScoutError('rate_limited', 'Rate limit exceeded', { retryable: true, retryAfterSec: rl.retryAfterSec });
          if (shuttingDown) throw new ScoutError('shutting_down', 'Service is shutting down', { retryable: true });
        }
      }
      const body = req.method === 'POST' ? await readJson(req, cfg.maxBodyBytes) : undefined;
      const out = await route[4]({ req, owner, body, params, signal: ac.signal });
      status = out.status;
      send(res, out.status, out.body, { headers: out.headers, raw: out.raw, contentType: out.contentType });
    } catch (e) {
      const se = toScoutError(e);
      if (se.code === 'internal') log.error('unhandled error', { requestId, stack: String(e?.stack ?? e) });
      status = se.status;
      const headers = {};
      if (se.retryAfterSec) headers['retry-after'] = String(se.retryAfterSec);
      if (se.code === 'unauthorized') headers['www-authenticate'] = 'Bearer';
      if (e.allow) headers.allow = e.allow;
      if (se.code === 'payload_too_large') { headers.connection = 'close'; res.once('finish', () => req.socket.destroy()); }
      send(res, se.status, se.toJSON(), { headers });
    } finally {
      scout.metrics.inc('scout_http_requests_total', { route: routeName, status });
      log.info('request', { requestId, method: req.method, route: routeName, status, ms: Date.now() - t0, owner });
    }
  });
  server.headersTimeout = 15000;
  server.requestTimeout = 30000;
  server.keepAliveTimeout = 5000;
  server.setShuttingDown = () => { shuttingDown = true; };
  return server;
}

export async function startServer(cfg) {
  const log = createLogger(cfg.logLevel);
  const scout = new Scout(cfg);
  const server = createServer(scout, cfg, log);
  await new Promise((resolve, reject) => { server.once('error', reject); server.listen(cfg.port, cfg.host, resolve); });
  const port = server.address().port;
  const close = async () => {
    server.setShuttingDown();
    server.close();
    server.closeIdleConnections();
    await scout.close(); // finishes in-flight work within the grace period, then aborts and releases everything
    server.closeAllConnections();
  };
  return { server, scout, log, port, url: `http://127.0.0.1:${port}`, close };
}
