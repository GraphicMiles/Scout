import { ScoutError, toScoutError, abortError, sleep } from './errors.js';
import { Metrics } from './metrics.js';
import { ResourceManager } from './resources.js';
import { SessionManager } from './session.js';
import { TaskManager } from './tasks.js';
import { buildStrategies } from './adapters/index.js';
import { doNavigate } from './navigation.js';
import { doInteract } from './interaction.js';

/** Orchestrates navigation, extraction, interaction, sessions and tasks. Transport-agnostic. */
export class Scout {
  constructor(cfg) {
    this.cfg = cfg;
    this.metrics = new Metrics();
    this.resources = new ResourceManager(cfg);
    this.sessions = new SessionManager(cfg);
    this.strategies = buildStrategies(cfg);
    this.tasks = new TaskManager({
      concurrency: cfg.concurrency, maxQueued: cfg.maxQueued, defaultTimeoutMs: cfg.taskTimeoutMs,
      resultTtlMs: cfg.taskResultTtlMs, maxRetained: cfg.maxRetainedTasks, metrics: this.metrics,
      hooks: { admit: () => this.resources.admit(), watch: (c) => this.resources.watch(c), afterJob: () => this.resources.afterJob() },
    });
    this.metrics.gauge('scout_rss_bytes', () => this.resources.rssBytes());
    this.metrics.gauge('scout_sessions', () => this.sessions.size);
    this.metrics.gauge('scout_tasks_queued', () => this.tasks.stats().queued);
    this.metrics.gauge('scout_tasks_running', () => this.tasks.stats().running);
  }

  listStrategies() { return [...this.strategies.values()].map(({ name, description, status, reason }) => ({ name, description, status, ...(reason ? { reason } : {}) })); }

  // ----- sessions -----
  createSession(owner) {
    const s = this.sessions.create(owner);
    return { id: s.id, createdAt: new Date(s.createdAt).toISOString(), expiresInMs: this.cfg.sessionTtlMs };
  }
  destroySession(owner, id) {
    if (!this.sessions.destroy(id, owner)) throw new ScoutError('not_found', 'Session not found');
  }

  // ----- synchronous operations (still run through the shared bounded queue) -----
  navigate(owner, p, signal) {
    return this.#sync(owner, signal, 'navigate', (sig) => this.#inSession(owner, p.sessionId, sig, (s, sg) => this.#navigateRetry(s, p, sg, 0)));
  }
  interact(owner, p, signal) {
    return this.#sync(owner, signal, 'interact', (sig) => this.#inSession(owner, p.sessionId, sig, (s, sg) => doInteract(this.cfg, s, p.action, sg)));
  }
  extract(owner, p, signal) {
    this.#strategy(p); // fail fast, before any network or queue use
    return this.#sync(owner, signal, 'extract', (sig) => {
      if (p.sessionId) return this.#inSession(owner, p.sessionId, sig, (s, sg) => this.#extract(s, p, sg, 0));
      return this.#ephemeral(owner, sig, (s, sg) => this.#extract(s, p, sg, 0));
    });
  }

  // ----- tasks -----
  submitTask(owner, spec) {
    for (const st of spec.steps) if (st.op === 'extract') this.#strategy(st);
    if (spec.sessionId) this.sessions.get(owner, spec.sessionId);
    return this.tasks.submit(owner, (sig) => this.#timed('task', () => this.#runTask(owner, spec, sig)), { timeoutMs: spec.timeoutMs });
  }
  getTask(owner, id) { return this.tasks.get(owner, id); }
  cancelTask(owner, id) { return this.tasks.cancel(owner, id); }

  async close() {
    await this.tasks.shutdown(this.cfg.shutdownGraceMs);
    this.sessions.close();
  }

  // ----- internals -----
  #sync(owner, signal, op, fn) {
    return this.tasks.runInline(owner, (sig) => this.#timed(op, () => fn(sig)), { signal, timeoutMs: this.cfg.requestTimeoutMs });
  }

  async #timed(op, fn) {
    const t0 = performance.now();
    try {
      const r = await fn();
      this.metrics.inc('scout_operations_total', { op, outcome: 'ok' });
      return r;
    } catch (e) {
      this.metrics.inc('scout_operations_total', { op, outcome: e instanceof ScoutError ? e.code : 'internal' });
      throw e;
    } finally {
      this.metrics.observe('scout_operation_seconds', { op }, (performance.now() - t0) / 1000);
    }
  }

  async #inSession(owner, id, sig, fn) {
    const s = this.sessions.get(owner, id);
    const sg = AbortSignal.any([sig, s.signal]);
    return s.exclusive(() => fn(s, sg));
  }

  async #ephemeral(owner, sig, fn) {
    const s = this.sessions.create(owner, { ephemeral: true });
    try {
      return await s.exclusive(() => fn(s, AbortSignal.any([sig, s.signal])));
    } finally {
      this.sessions.destroy(s.id); // always release the DOM, cookies and buffers
    }
  }

  #strategy(p) {
    const name = p.strategy ?? (p.schema ? 'selectors' : 'generic-page');
    const strat = this.strategies.get(name);
    if (!strat) throw new ScoutError('invalid_request', `Unknown strategy "${name}"`, { details: { available: [...this.strategies.keys()] } });
    if (strat.status === 'not_implemented') throw new ScoutError('unsupported_capability', strat.reason, { details: { strategy: name } });
    return strat;
  }

  async #navigateRetry(s, p, sig, retries) {
    for (let attempt = 0; ; attempt++) {
      try {
        return await doNavigate(this.cfg, s, { url: p.url, timeoutMs: p.timeoutMs }, sig);
      } catch (e) {
        if (!(e instanceof ScoutError) || !e.retryable || attempt >= retries) throw e;
        this.metrics.inc('scout_navigation_retries_total');
        await sleep(Math.min(250 * 2 ** attempt, 2000) + Math.random() * 100, sig);
      }
    }
  }

  async #extract(s, p, sig, retries) {
    const t0 = Date.now();
    const strat = this.#strategy(p);
    let nav;
    if (p.url) nav = await this.#navigateRetry(s, { url: strat.prepareUrl ? strat.prepareUrl(p.url) : p.url, timeoutMs: p.timeoutMs }, sig, retries);
    if (!s.page) throw new ScoutError('conflict', 'Session has no loaded page; provide "url" or navigate first');
    const { data, warnings = [] } = strat.run({ page: s.page, limit: p.limit ?? strat.defaultLimit ?? 20, schema: p.schema, cfg: this.cfg });
    return {
      data, warnings: [...(nav?.warnings ?? []).filter((w) => !(strat.embeddedData && w.code === 'js_required_likely')), ...warnings],
      meta: { strategy: strat.name, url: s.page.url, status: s.page.status, durationMs: Date.now() - t0 },
    };
  }

  async #runTask(owner, spec, sig) {
    const own = !spec.sessionId;
    const s = own ? this.sessions.create(owner, { ephemeral: true }) : this.sessions.get(owner, spec.sessionId);
    try {
      return await s.exclusive(async () => {
        const sg = AbortSignal.any([sig, s.signal]);
        const steps = []; let data;
        for (const [index, step] of spec.steps.entries()) {
          if (sg.aborted) throw abortError(sg);
          const t0 = Date.now();
          try {
            let result;
            if (step.op === 'navigate') result = await this.#navigateRetry(s, step, sg, spec.retries);
            else if (step.op === 'extract') { result = await this.#extract(s, step, sg, spec.retries); data = result.data; }
            else result = await doInteract(this.cfg, s, step.action, sg); // interactions are never auto-retried (not idempotent)
            steps.push({ index, op: step.op, ok: true, durationMs: Date.now() - t0, result });
          } catch (e) {
            const se = toScoutError(e);
            throw new ScoutError(se.code, se.message, { retryable: se.retryable, details: { ...se.details, failedStep: index, completedSteps: steps.length } });
          }
        }
        return { steps, data };
      });
    } finally {
      if (own) this.sessions.destroy(s.id);
    }
  }
}
