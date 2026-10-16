const STATUS = {
  invalid_request: 400, unauthorized: 401, forbidden: 403, blocked_destination: 403,
  not_found: 404, method_not_allowed: 405, conflict: 409, cancelled: 409,
  payload_too_large: 413, unsupported_content: 415,
  element_not_found: 422, compatibility_error: 422, rate_limited: 429,
  unsupported_capability: 501, navigation_failed: 502, response_too_large: 502,
  queue_full: 503, too_many_sessions: 503, overloaded: 503, shutting_down: 503, resource_limit: 503,
  navigation_timeout: 504, task_timeout: 504, internal: 500,
};

export class ScoutError extends Error {
  constructor(code, message, opts = {}) {
    super(message);
    this.name = 'ScoutError';
    this.code = code;
    this.status = opts.status ?? STATUS[code] ?? 500;
    this.retryable = opts.retryable ?? false;
    this.details = opts.details;
    this.retryAfterSec = opts.retryAfterSec;
  }
  toJSON() {
    const error = { code: this.code, message: this.message, retryable: this.retryable };
    if (this.details !== undefined) error.details = this.details;
    return { error };
  }
}

/** Unknown errors never leak their message to callers. */
export function toScoutError(e) {
  return e instanceof ScoutError ? e : new ScoutError('internal', 'Internal error');
}

export function abortError(signal) {
  const r = signal?.reason;
  return r instanceof ScoutError ? r : new ScoutError('cancelled', 'Operation cancelled');
}

export const sleep = (ms, signal) =>
  new Promise((resolve, reject) => {
    if (signal?.aborted) return reject(abortError(signal));
    const t = setTimeout(() => { signal?.removeEventListener('abort', onAbort); resolve(); }, ms);
    const onAbort = () => { clearTimeout(t); reject(abortError(signal)); };
    signal?.addEventListener('abort', onAbort, { once: true });
  });
