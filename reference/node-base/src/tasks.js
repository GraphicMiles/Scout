import crypto from 'node:crypto';
import { ScoutError, toScoutError, abortError, sleep } from './errors.js';

const TERMINAL = new Set(['succeeded', 'failed', 'cancelled', 'timed_out']);

const view = (t) => ({
  id: t.id, status: t.status, createdAt: new Date(t.createdAt).toISOString(),
  startedAt: t.startedAt ? new Date(t.startedAt).toISOString() : null,
  finishedAt: t.finishedAt ? new Date(t.finishedAt).toISOString() : null,
  result: t.result, error: t.error,
});

/**
 * Bounded FIFO queue + bounded concurrency shared by background tasks and inline (synchronous API) work.
 * Each job gets an AbortSignal; timeouts, cancellation, shutdown and the memory watchdog all abort through it.
 */
export class TaskManager {
  #queue = [];
  #running = new Set();
  #tasks = new Map();
  #jobs = new Map();
  #stopping = false;

  constructor(o) {
    this.concurrency = o.concurrency; this.maxQueued = o.maxQueued;
    this.defaultTimeoutMs = o.defaultTimeoutMs; this.resultTtlMs = o.resultTtlMs;
    this.maxRetained = o.maxRetained ?? 500; this.hooks = o.hooks ?? {}; this.metrics = o.metrics;
    this.timer = setInterval(() => this.#prune(), Math.max(50, Math.min(60000, this.resultTtlMs)));
    this.timer.unref();
  }

  stats() { return { queued: this.#queue.length, running: this.#running.size, retained: this.#tasks.size }; }

  #admit() {
    if (this.#stopping) throw new ScoutError('shutting_down', 'Service is shutting down', { retryable: true });
    this.hooks.admit?.();
    if (this.#running.size >= this.concurrency && this.#queue.length >= this.maxQueued) {
      throw new ScoutError('queue_full', `Queue is full (${this.maxQueued} waiting)`, { retryable: true, retryAfterSec: 2 });
    }
  }

  submit(owner, runner, { timeoutMs } = {}) {
    this.#admit();
    const task = { id: 'tsk_' + crypto.randomBytes(12).toString('hex'), owner, status: 'queued', createdAt: Date.now(), startedAt: null, finishedAt: null, result: null, error: null };
    const job = {
      id: task.id, task, ctl: new AbortController(), timeoutMs: timeoutMs ?? this.defaultTimeoutMs, run: runner,
      settle: (err, out) => {
        if (TERMINAL.has(task.status)) return;
        task.finishedAt = Date.now();
        if (!err) { task.status = 'succeeded'; task.result = out; }
        else {
          const se = toScoutError(err);
          task.status = se.code === 'task_timeout' ? 'timed_out' : se.code === 'cancelled' || se.code === 'shutting_down' ? 'cancelled' : 'failed';
          task.error = se.toJSON().error;
        }
        this.metrics?.inc('scout_tasks_total', { status: task.status });
      },
    };
    this.#tasks.set(task.id, task); this.#jobs.set(task.id, job);
    this.#queue.push(job);
    this.#pump();
    return view(task);
  }

  runInline(owner, fn, { signal, timeoutMs } = {}) {
    return new Promise((resolve, reject) => {
      try { this.#admit(); } catch (e) { return reject(e); }
      if (signal?.aborted) return reject(abortError(signal));
      const job = { owner, ctl: new AbortController(), timeoutMs: timeoutMs ?? this.defaultTimeoutMs, run: fn, settle: (e, out) => (e ? reject(e) : resolve(out)) };
      if (signal) {
        const onAbort = () => {
          const i = this.#queue.indexOf(job);
          if (i >= 0) { this.#queue.splice(i, 1); job.cleanup(); reject(abortError(signal)); }
          else job.ctl.abort(abortError(signal));
        };
        signal.addEventListener('abort', onAbort, { once: true });
        job.cleanup = () => signal.removeEventListener('abort', onAbort);
      }
      this.#queue.push(job);
      this.#pump();
    });
  }

  get(owner, id) {
    const t = this.#tasks.get(id);
    if (!t || t.owner !== owner) throw new ScoutError('not_found', 'Task not found');
    return view(t);
  }

  cancel(owner, id) {
    const t = this.#tasks.get(id);
    if (!t || t.owner !== owner) throw new ScoutError('not_found', 'Task not found');
    if (t.status === 'cancelled') return view(t);
    if (TERMINAL.has(t.status)) throw new ScoutError('conflict', `Task already ${t.status}`);
    const job = this.#jobs.get(id);
    const i = this.#queue.indexOf(job);
    if (i >= 0) { this.#queue.splice(i, 1); this.#jobs.delete(id); }
    t.status = 'cancelled'; t.finishedAt = Date.now();
    t.error = { code: 'cancelled', message: 'Task cancelled by caller', retryable: false };
    this.metrics?.inc('scout_tasks_total', { status: 'cancelled' });
    if (i < 0) job?.ctl.abort(new ScoutError('cancelled', 'Task cancelled by caller'));
    return view(t);
  }

  #pump() {
    while (!this.#stopping && this.#running.size < this.concurrency && this.#queue.length) this.#start(this.#queue.shift());
  }

  async #start(job) {
    this.#running.add(job);
    if (job.task) { job.task.status = 'running'; job.task.startedAt = Date.now(); }
    const ctl = job.ctl;
    const timer = setTimeout(() => ctl.abort(new ScoutError('task_timeout', `Exceeded the ${job.timeoutMs}ms execution limit`, { retryable: true })), job.timeoutMs);
    const unwatch = this.hooks.watch?.(ctl);
    try {
      const out = await job.run(ctl.signal);
      if (ctl.signal.aborted) throw abortError(ctl.signal); // never report success for an aborted job
      job.settle(null, out);
    } catch (e) {
      job.settle(ctl.signal.aborted ? abortError(ctl.signal) : e);
    } finally {
      clearTimeout(timer); unwatch?.(); job.cleanup?.();
      this.#running.delete(job);
      if (job.id) this.#jobs.delete(job.id);
      try { this.hooks.afterJob?.(); } catch { /* best effort */ }
      this.#pump();
    }
  }

  #prune() {
    const now = Date.now();
    for (const [id, t] of this.#tasks) if (TERMINAL.has(t.status) && now - t.finishedAt > this.resultTtlMs) this.#tasks.delete(id);
    if (this.#tasks.size > this.maxRetained) {
      for (const [id, t] of this.#tasks) { if (TERMINAL.has(t.status)) this.#tasks.delete(id); if (this.#tasks.size <= this.maxRetained) break; }
    }
  }

  async shutdown(graceMs = 10000) {
    this.#stopping = true;
    clearInterval(this.timer);
    const err = new ScoutError('shutting_down', 'Service is shutting down', { retryable: true });
    for (const j of this.#queue.splice(0)) { j.cleanup?.(); j.settle(err); }
    const until = Date.now() + graceMs;
    while (this.#running.size && Date.now() < until) await sleep(25);
    for (const j of this.#running) j.ctl.abort(err);
    const hard = Date.now() + 2000;
    while (this.#running.size && Date.now() < hard) await sleep(25);
  }
}
