const MB = 1024 * 1024;
const list = (v) => (v ?? '').split(',').map((s) => s.trim().toLowerCase()).filter(Boolean);

export function loadConfig(env = process.env, overrides = {}) {
  const num = (k, d) => {
    const v = env[k];
    if (v === undefined || v === '') return d;
    const n = Number(v);
    if (!Number.isFinite(n)) throw new Error(`${k} must be a number`);
    return n;
  };
  const cfg = {
    version: '0.1.0',
    host: env.HOST || '0.0.0.0',
    port: num('PORT', 8080),
    devMode: env.SCOUT_DEV_MODE === '1',
    apiKeys: (env.SCOUT_API_KEYS ?? env.SCOUT_API_KEY ?? '').split(',').map((s) => s.trim()).filter(Boolean),
    metricsToken: env.SCOUT_METRICS_TOKEN ?? '',

    // Outbound policy. privateAllowlist is an explicit operator opt-in for internal hosts.
    privateAllowlist: list(env.SCOUT_PRIVATE_ALLOWLIST),
    allowedDomains: list(env.SCOUT_ALLOWED_DOMAINS),
    allowedPorts: list(env.SCOUT_ALLOWED_PORTS || '80,443').map(Number),
    resolver: null, // test hook: dns.lookup-compatible function

    // Limits
    maxBodyBytes: num('SCOUT_MAX_BODY_BYTES', 64 * 1024),
    maxResponseBytes: num('SCOUT_MAX_RESPONSE_BYTES', 2 * MB),
    maxDomNodes: num('SCOUT_MAX_DOM_NODES', 150000),
    maxRedirects: num('SCOUT_MAX_REDIRECTS', 5),
    maxExtractItems: 200,
    navTimeoutMs: num('SCOUT_NAV_TIMEOUT_MS', 15000),
    maxNavTimeoutMs: 30000,
    taskTimeoutMs: num('SCOUT_TASK_TIMEOUT_MS', 60000),
    maxTaskTimeoutMs: 120000,
    requestTimeoutMs: num('SCOUT_REQUEST_TIMEOUT_MS', 60000),
    maxSessions: num('SCOUT_MAX_SESSIONS', 8),
    sessionTtlMs: num('SCOUT_SESSION_TTL_MS', 300000),
    concurrency: num('SCOUT_CONCURRENCY', 1),
    maxQueued: num('SCOUT_MAX_QUEUED', 16),
    taskResultTtlMs: num('SCOUT_TASK_RESULT_TTL_MS', 600000),
    maxRetainedTasks: 500,
    maxRetries: num('SCOUT_MAX_RETRIES', 2),
    rateLimitPerMin: num('SCOUT_RATE_LIMIT_PER_MIN', 120),

    // Memory budget (container is ~512 MB)
    memorySoftLimitBytes: num('SCOUT_MEM_SOFT_MB', 300) * MB,
    memoryHardLimitBytes: num('SCOUT_MEM_HARD_MB', 400) * MB,
    gcAboveBytes: num('SCOUT_GC_ABOVE_MB', 100) * MB,

    shutdownGraceMs: num('SCOUT_SHUTDOWN_GRACE_MS', 10000),
    logLevel: env.SCOUT_LOG_LEVEL || 'info',
    userAgent: env.SCOUT_USER_AGENT || 'Scout/0.1 (automation runtime)',
    youtubeHosts: ['youtube.com', 'www.youtube.com', 'm.youtube.com'],
  };
  Object.assign(cfg, overrides);
  if (!cfg.devMode) {
    if (cfg.apiKeys.length === 0) throw new Error('SCOUT_API_KEYS is required unless SCOUT_DEV_MODE=1');
    if (cfg.apiKeys.some((k) => k.length < 16)) throw new Error('Every API key must be at least 16 characters');
  }
  return cfg;
}
