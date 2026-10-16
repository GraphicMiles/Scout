import crypto from 'node:crypto';
import { ScoutError } from './errors.js';
import { CookieJar } from './cookies.js';

export class Session {
  #tail = Promise.resolve();
  constructor(id, owner, ephemeral) {
    this.id = id; this.owner = owner; this.ephemeral = ephemeral;
    this.jar = new CookieJar();
    this.page = null;
    this.createdAt = Date.now(); this.lastUsed = this.createdAt;
    this.destroyed = false;
    this.ctl = new AbortController();
  }
  get signal() { return this.ctl.signal; }
  setPage(p) { this.page = p; }

  /** One active operation per session; later callers wait their turn. */
  exclusive(fn) {
    const run = this.#tail.then(() => {
      if (this.destroyed) throw new ScoutError('not_found', 'Session no longer exists');
      this.lastUsed = Date.now();
      return fn();
    });
    this.#tail = run.catch(() => {});
    return run;
  }

  destroy() {
    if (this.destroyed) return;
    this.destroyed = true;
    this.ctl.abort(new ScoutError('cancelled', 'Session destroyed'));
    this.jar.clear();
    this.page = null;
  }
}

export class SessionManager {
  #m = new Map();
  constructor(cfg) {
    this.cfg = cfg;
    this.timer = setInterval(() => this.sweep(), Math.max(50, Math.min(30000, cfg.sessionTtlMs / 2)));
    this.timer.unref();
  }
  get size() { return this.#m.size; }

  create(owner, { ephemeral = false } = {}) {
    if (this.#m.size >= this.cfg.maxSessions) this.sweep();
    if (this.#m.size >= this.cfg.maxSessions) {
      throw new ScoutError('too_many_sessions', `Session limit (${this.cfg.maxSessions}) reached`, { retryable: true, retryAfterSec: 5 });
    }
    const s = new Session('ses_' + crypto.randomBytes(16).toString('hex'), owner, ephemeral);
    this.#m.set(s.id, s);
    return s;
  }

  /** Sessions are bound to the API key that created them; other callers just see not_found. */
  get(owner, id) {
    const s = this.#m.get(id);
    if (!s || s.owner !== owner) throw new ScoutError('not_found', 'Session not found');
    s.lastUsed = Date.now();
    return s;
  }

  destroy(id, owner) {
    const s = this.#m.get(id);
    if (!s || (owner !== undefined && s.owner !== owner)) return false;
    this.#m.delete(id);
    s.destroy();
    return true;
  }

  sweep() {
    const now = Date.now();
    for (const s of [...this.#m.values()]) if (now - s.lastUsed > this.cfg.sessionTtlMs) this.destroy(s.id);
  }

  close() {
    clearInterval(this.timer);
    for (const s of [...this.#m.values()]) this.destroy(s.id);
  }
}
