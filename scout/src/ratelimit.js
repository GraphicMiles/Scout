/** Token bucket per key. */
export class RateLimiter {
  #m = new Map();
  constructor(perMin) { this.cap = perMin; this.rate = perMin / 60000; }

  #bucket(key, now) {
    let b = this.#m.get(key);
    if (!b) {
      if (this.#m.size >= 10000) this.#evict(now);
      b = { tokens: this.cap, t: now };
      this.#m.set(key, b);
    } else {
      b.tokens = Math.min(this.cap, b.tokens + (now - b.t) * this.rate);
      b.t = now;
    }
    return b;
  }
  take(key, now = Date.now()) {
    const b = this.#bucket(key, now);
    if (b.tokens >= 1) { b.tokens -= 1; return { ok: true }; }
    return { ok: false, retryAfterSec: Math.max(1, Math.ceil((1 - b.tokens) / this.rate / 1000)) };
  }
  peek(key, now = Date.now()) { return this.#bucket(key, now).tokens >= 1; }
  #evict(now) {
    for (const [k, b] of this.#m) if (now - b.t > 120000) this.#m.delete(k);
    if (this.#m.size >= 10000) this.#m.clear();
  }
}
