import { ScoutError } from './errors.js';

/** Memory admission control and a watchdog that aborts a task before it can take the container down. */
export class ResourceManager {
  constructor(cfg) { this.cfg = cfg; }
  rssBytes() { return process.memoryUsage.rss(); }

  maybeGc() { if (typeof globalThis.gc === 'function') globalThis.gc(); }

  admit() {
    if (this.rssBytes() <= this.cfg.memorySoftLimitBytes) return;
    this.maybeGc();
    if (this.rssBytes() > this.cfg.memorySoftLimitBytes) {
      throw new ScoutError('overloaded', 'Service is above its memory budget; retry shortly', { retryable: true, retryAfterSec: 5 });
    }
  }

  watch(ctl) {
    const t = setInterval(() => {
      if (this.rssBytes() > this.cfg.memoryHardLimitBytes) {
        ctl.abort(new ScoutError('resource_limit', 'Aborted: memory limit exceeded while executing this task'));
      }
    }, 200);
    t.unref();
    return () => clearInterval(t);
  }

  afterJob() { if (this.rssBytes() > this.cfg.gcAboveBytes) this.maybeGc(); }
}
