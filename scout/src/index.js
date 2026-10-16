import { loadConfig } from './config.js';
import { startServer } from './server.js';

let cfg;
try { cfg = loadConfig(); } catch (e) { console.error(`Scout configuration error: ${e.message}`); process.exit(78); }

const app = await startServer(cfg);
app.log.info('scout listening', { port: app.port, devMode: cfg.devMode, concurrency: cfg.concurrency, maxQueued: cfg.maxQueued });
if (cfg.devMode) app.log.warn('SCOUT_DEV_MODE is on: authentication is disabled when no API keys are set');

let stopping = false;
async function shutdown(sig) {
  if (stopping) return;
  stopping = true;
  app.log.info('shutting down', { signal: sig });
  const hard = setTimeout(() => process.exit(1), cfg.shutdownGraceMs + 5000);
  hard.unref();
  await app.close();
  process.exit(0);
}
process.on('SIGTERM', () => shutdown('SIGTERM'));
process.on('SIGINT', () => shutdown('SIGINT'));
process.on('unhandledRejection', (e) => app.log.error('unhandled rejection', { stack: String(e?.stack ?? e) }));
process.on('uncaughtException', (e) => { app.log.error('uncaught exception', { stack: String(e?.stack ?? e) }); process.exit(1); });
