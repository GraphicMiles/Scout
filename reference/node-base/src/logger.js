const LEVELS = { debug: 10, info: 20, warn: 30, error: 40, silent: 99 };
const SENSITIVE = /authorization|cookie|password|passwd|token|secret|api[-_]?key|credential/i;

export function redact(v, depth = 0) {
  if (v == null || depth > 4) return v;
  if (typeof v === 'string') return v.length > 500 ? v.slice(0, 500) + '…' : v;
  if (Array.isArray(v)) return v.slice(0, 20).map((x) => redact(x, depth + 1));
  if (typeof v === 'object') {
    const o = {};
    for (const [k, x] of Object.entries(v)) o[k] = SENSITIVE.test(k) ? '[redacted]' : redact(x, depth + 1);
    return o;
  }
  return v;
}

export function createLogger(level = 'info') {
  const min = LEVELS[level] ?? LEVELS.info;
  const emit = (lvl, msg, fields) => {
    if (LEVELS[lvl] < min) return;
    process.stdout.write(JSON.stringify({ ...redact(fields ?? {}), t: new Date().toISOString(), level: lvl, msg }) + '\n');
  };
  return {
    debug: (m, f) => emit('debug', m, f),
    info: (m, f) => emit('info', m, f),
    warn: (m, f) => emit('warn', m, f),
    error: (m, f) => emit('error', m, f),
  };
}
