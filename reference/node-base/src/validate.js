import { ScoutError } from './errors.js';
import { parseSelector } from './selector.js';

const bad = (m, f) => new ScoutError('invalid_request', m, f ? { details: { field: f } } : undefined);
const isObj = (v) => v !== null && typeof v === 'object' && !Array.isArray(v);
const obj = (v, name = 'body') => { if (!isObj(v)) throw bad(`${name} must be a JSON object`); return v; };
const only = (o, allowed, name = 'body') => { for (const k of Object.keys(o)) if (!allowed.includes(k)) throw bad(`Unknown field "${k}" in ${name}`, k); };
function str(o, k, { req = false, max = 2048 } = {}) {
  const v = o[k];
  if (v === undefined || v === null) { if (req) throw bad(`"${k}" is required`, k); return undefined; }
  if (typeof v !== 'string' || !v.length || v.length > max) throw bad(`"${k}" must be a non-empty string up to ${max} characters`, k);
  return v;
}
function int(o, k, { min = 0, max = Number.MAX_SAFE_INTEGER, def } = {}) {
  const v = o[k];
  if (v === undefined || v === null) return def;
  if (!Number.isInteger(v) || v < min || v > max) throw bad(`"${k}" must be an integer between ${min} and ${max}`, k);
  return v;
}
const SID = /^ses_[0-9a-f]{32}$/;
function sessionId(o, req) {
  const v = str(o, 'sessionId', { req, max: 64 });
  if (v !== undefined && !SID.test(v)) throw bad('"sessionId" is malformed', 'sessionId');
  return v;
}

const TYPES = ['string', 'url', 'number', 'price', 'date'];
function fieldSpec(name, v) {
  if (typeof v === 'string') v = { selector: v };
  if (!isObj(v)) throw bad(`Field "${name}" must be a selector string or an object`, 'schema');
  only(v, ['selector', 'attr', 'type', 'multiple'], `field "${name}"`);
  const selector = str(v, 'selector', { max: 512 });
  if (selector) parseSelector(selector);
  const type = v.type ?? 'string';
  if (!TYPES.includes(type)) throw bad(`Field "${name}": type must be one of ${TYPES.join(', ')}`, 'schema');
  if (v.multiple !== undefined && typeof v.multiple !== 'boolean') throw bad(`Field "${name}": multiple must be boolean`, 'schema');
  return { selector, attr: str(v, 'attr', { max: 64 }), type, multiple: !!v.multiple };
}
export function parseSchema(s) {
  const o = obj(s, 'schema');
  only(o, ['item', 'fields'], 'schema');
  const fields = obj(o.fields, 'schema.fields');
  const names = Object.keys(fields);
  if (!names.length || names.length > 50) throw bad('schema.fields must contain 1 to 50 fields', 'schema');
  const out = {};
  for (const n of names) {
    if (n.length > 64) throw bad('Field names are limited to 64 characters', 'schema');
    out[n] = fieldSpec(n, fields[n]);
  }
  const item = str(o, 'item', { max: 512 });
  if (item) parseSelector(item);
  return { item, fields: out };
}

function extractFields(o, cfg, { sessionAllowed, requireTarget }) {
  const strategy = str(o, 'strategy', { max: 64 });
  const schema = o.schema !== undefined ? parseSchema(o.schema) : undefined;
  const url = str(o, 'url');
  const sid = sessionAllowed ? sessionId(o, false) : undefined;
  if (requireTarget && !url && !sid) throw bad('Provide "url" or "sessionId"');
  if (schema && strategy && strategy !== 'selectors') throw bad('"schema" is only valid with the "selectors" strategy', 'schema');
  if (!schema && strategy === 'selectors') throw bad('The "selectors" strategy requires "schema"', 'schema');
  return { sessionId: sid, url, strategy, schema, limit: int(o, 'limit', { min: 1, max: cfg.maxExtractItems }), timeoutMs: int(o, 'timeoutMs', { min: 100, max: cfg.maxNavTimeoutMs }) };
}

export function parseCreateSession(b) { only(obj(b ?? {}), []); return {}; }
export function parseNavigate(b, cfg) {
  const o = obj(b); only(o, ['sessionId', 'url', 'timeoutMs']);
  return { sessionId: sessionId(o, true), url: str(o, 'url', { req: true }), timeoutMs: int(o, 'timeoutMs', { min: 100, max: cfg.maxNavTimeoutMs }) };
}
export function parseExtract(b, cfg) {
  const o = obj(b); only(o, ['sessionId', 'url', 'strategy', 'schema', 'limit', 'timeoutMs']);
  return extractFields(o, cfg, { sessionAllowed: true, requireTarget: true });
}

const ACTIONS = ['click', 'fill', 'select', 'check', 'uncheck', 'press', 'submit', 'scroll'];
export function parseAction(a) {
  const o = obj(a, 'action'); only(o, ['type', 'selector', 'index', 'value', 'key', 'timeoutMs'], 'action');
  if (!ACTIONS.includes(o.type)) throw bad(`action.type must be one of ${ACTIONS.join(', ')}`, 'type');
  const selector = str(o, 'selector', { req: o.type !== 'scroll', max: 512 });
  if (selector) parseSelector(selector);
  let value = o.value;
  if (o.type === 'fill') { if (typeof value !== 'string' || value.length > 10000) throw bad('"value" must be a string up to 10000 characters', 'value'); }
  else if (o.type === 'select') {
    const list = [].concat(value ?? []);
    if (!list.length || list.length > 50 || list.some((x) => typeof x !== 'string' || x.length > 512)) throw bad('"value" must be a string or array of up to 50 strings', 'value');
  }
  return { type: o.type, selector, index: int(o, 'index', { min: 0, max: 1000 }), value, key: str(o, 'key', { max: 32 }), timeoutMs: int(o, 'timeoutMs', { min: 100, max: 30000 }) };
}
export function parseInteract(b) {
  const o = obj(b); only(o, ['sessionId', 'action']);
  return { sessionId: sessionId(o, true), action: parseAction(o.action) };
}

export function parseTask(b, cfg) {
  const o = obj(b); only(o, ['sessionId', 'steps', 'timeoutMs', 'retries']);
  if (!Array.isArray(o.steps) || !o.steps.length || o.steps.length > 20) throw bad('"steps" must be an array of 1 to 20 steps', 'steps');
  const steps = o.steps.map((s, i) => {
    const so = obj(s, `steps[${i}]`);
    switch (so.op) {
      case 'navigate': only(so, ['op', 'url', 'timeoutMs'], `steps[${i}]`); return { op: 'navigate', url: str(so, 'url', { req: true }), timeoutMs: int(so, 'timeoutMs', { min: 100, max: cfg.maxNavTimeoutMs }) };
      case 'extract': only(so, ['op', 'url', 'strategy', 'schema', 'limit', 'timeoutMs'], `steps[${i}]`); return { op: 'extract', ...extractFields(so, cfg, { sessionAllowed: false, requireTarget: false }) };
      case 'interact': only(so, ['op', 'action'], `steps[${i}]`); return { op: 'interact', action: parseAction(so.action) };
      default: throw bad(`steps[${i}].op must be "navigate", "extract" or "interact"`, 'steps');
    }
  });
  return {
    sessionId: sessionId(o, false), steps,
    timeoutMs: int(o, 'timeoutMs', { min: 1000, max: cfg.maxTaskTimeoutMs }),
    retries: int(o, 'retries', { min: 0, max: cfg.maxRetries, def: Math.min(1, cfg.maxRetries) }),
  };
}
