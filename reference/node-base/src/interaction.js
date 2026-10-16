import { ScoutError } from './errors.js';
import { doNavigate, pageSummary } from './navigation.js';
import { querySelectorAll, closest } from './selector.js';
import { descendants, text, rawText } from './html.js';
import { baseUrlOf } from './extract.js';
import { resolveUrl } from './util.js';

const NO_JS = 'Scout core does not execute JavaScript, so this element has no supported default action';
const unsupported = (msg) => new ScoutError('unsupported_capability', msg);

function requirePage(session) {
  const p = session.page;
  if (!p || p.kind !== 'html') throw new ScoutError('conflict', 'Session has no loaded HTML page; navigate first');
  return p;
}
function find(page, action) {
  const idx = action.index ?? 0;
  const els = querySelectorAll(page.doc, action.selector, idx + 1);
  if (els.length <= idx) throw new ScoutError('element_not_found', `No element matches selector "${action.selector}"${idx ? ` at index ${idx}` : ''}`, { details: { selector: action.selector, index: idx } });
  return els[idx];
}
const st = (page, el) => page.formState.get(el) ?? {};
const setSt = (page, el, patch) => page.formState.set(el, { ...st(page, el), ...patch });
const inputType = (el) => (el.attrs.type || 'text').toLowerCase();

function referer(page, target) {
  try { return new URL(target).origin === new URL(page.url).origin ? page.url : new URL(page.url).origin + '/'; } catch { return undefined; }
}

function optionValue(o) { return o.attrs.value ?? text(o); }

function collectFormData(page, form, submitter) {
  const pairs = [];
  for (const el of descendants(form)) {
    if (el.attrs.disabled !== undefined) continue;
    const name = el.attrs.name;
    const s = st(page, el);
    if (el.tag === 'input') {
      const type = inputType(el);
      if (['submit', 'image', 'button', 'reset'].includes(type)) { if (el === submitter && name) pairs.push([name, el.attrs.value ?? '']); continue; }
      if (type === 'file' || !name) continue;
      if (type === 'checkbox' || type === 'radio') {
        if (s.checked ?? el.attrs.checked !== undefined) pairs.push([name, el.attrs.value ?? 'on']);
        continue;
      }
      pairs.push([name, s.value ?? el.attrs.value ?? '']);
    } else if (el.tag === 'textarea') {
      if (name) pairs.push([name, s.value ?? rawText(el).replace(/^\r?\n/, '')]);
    } else if (el.tag === 'select') {
      if (!name) continue;
      const opts = [...descendants(el)].filter((o) => o.tag === 'option' && o.attrs.disabled === undefined);
      const multiple = el.attrs.multiple !== undefined;
      let chosen;
      if (s.value !== undefined) { const want = [].concat(s.value); chosen = opts.filter((o) => want.includes(optionValue(o))); }
      else { chosen = opts.filter((o) => o.attrs.selected !== undefined); if (!chosen.length && !multiple && opts.length) chosen = [opts[0]]; }
      for (const o of multiple ? chosen : chosen.slice(0, 1)) pairs.push([name, optionValue(o)]);
    } else if (el.tag === 'button' && el === submitter && name) pairs.push([name, el.attrs.value ?? '']);
  }
  return pairs;
}

async function submitForm(cfg, session, form, submitter, signal, timeoutMs) {
  const page = session.page;
  const enctype = (submitter?.attrs.formenctype ?? form.attrs.enctype ?? 'application/x-www-form-urlencoded').toLowerCase();
  if (enctype !== 'application/x-www-form-urlencoded') throw unsupported(`Form encoding "${enctype}" is not supported (only application/x-www-form-urlencoded)`);
  let method = (submitter?.attrs.formmethod ?? form.attrs.method ?? 'get').toUpperCase();
  if (method === 'DIALOG') throw unsupported('method=dialog forms are not supported');
  if (method !== 'POST') method = 'GET';
  const actionAttr = submitter?.attrs.formaction ?? form.attrs.action;
  if (actionAttr && /^\s*javascript:/i.test(actionAttr)) throw unsupported('javascript: form actions require JavaScript execution');
  const target = actionAttr ? resolveUrl(actionAttr, baseUrlOf(page.doc, page.url)) : page.url;
  if (!target) throw new ScoutError('invalid_request', 'Form action is not a valid URL');
  const qs = new URLSearchParams(collectFormData(page, form, submitter)).toString();
  const headers = { referer: referer(page, target) };
  if (method === 'GET') {
    const u = new URL(target); u.search = qs;
    return doNavigate(cfg, session, { url: u.href, headers, timeoutMs }, signal);
  }
  return doNavigate(cfg, session, { url: target, method: 'POST', body: qs, headers: { ...headers, 'content-type': 'application/x-www-form-urlencoded' }, timeoutMs }, signal);
}

function setChecked(page, el, checked) {
  const type = inputType(el);
  if (type === 'radio') {
    const form = closest(el, 'form') ?? page.doc;
    for (const r of descendants(form)) if (r.tag === 'input' && inputType(r) === 'radio' && r.attrs.name === el.attrs.name && r !== el) setSt(page, r, { checked: false });
    setSt(page, el, { checked: true });
  } else setSt(page, el, { checked });
}

const done = (type, extra = {}) => ({ ok: true, type, navigated: false, ...extra });
const navigated = (type, session, nav) => ({ ok: true, type, navigated: true, navigation: nav, page: pageSummary(session.page) });

export async function doInteract(cfg, session, action, signal) {
  const page = requirePage(session);
  const t = action.type;
  if (t === 'scroll') throw unsupported('Scrolling is not supported: Scout core has no layout engine and cannot trigger lazy loading');
  const el = find(page, action);
  const timeoutMs = action.timeoutMs;

  switch (t) {
    case 'click': {
      const anchor = closest(el, 'a');
      if (anchor && anchor.attrs.href !== undefined) {
        const href = anchor.attrs.href.trim();
        if (/^javascript:/i.test(href)) throw unsupported('javascript: links require JavaScript execution');
        if (href.startsWith('#')) return done(t);
        const target = resolveUrl(href, baseUrlOf(page.doc, page.url));
        if (!target || !/^https?:/.test(target)) throw unsupported(`Links with scheme "${href.split(':')[0]}:" are not followed`);
        return navigated(t, session, await doNavigate(cfg, session, { url: target, headers: { referer: referer(page, target) }, timeoutMs }, signal));
      }
      if (el.tag === 'input' && ['checkbox', 'radio'].includes(inputType(el))) {
        setChecked(page, el, inputType(el) === 'radio' ? true : !(st(page, el).checked ?? el.attrs.checked !== undefined));
        return done(t);
      }
      const btn = closest(el, 'button') ?? (el.tag === 'input' ? el : null);
      const btnType = btn ? (btn.tag === 'button' ? (btn.attrs.type || 'submit') : inputType(btn)).toLowerCase() : null;
      if (btn && (btnType === 'submit' || btnType === 'image')) {
        const form = closest(btn, 'form');
        if (form) return navigated(t, session, await submitForm(cfg, session, form, btn, signal, timeoutMs));
      }
      throw unsupported(NO_JS);
    }
    case 'fill': {
      const type = el.tag === 'input' ? inputType(el) : null;
      if (!(el.tag === 'textarea' || (el.tag === 'input' && !['checkbox', 'radio', 'file', 'submit', 'button', 'image', 'reset'].includes(type)))) {
        throw unsupported(type === 'file' ? 'File inputs are not supported' : `Element <${el.tag}> cannot be filled`);
      }
      setSt(page, el, { value: String(action.value ?? '') }); // value is held in memory only, never echoed or logged
      return done(t, { selector: action.selector });
    }
    case 'select': {
      if (el.tag !== 'select') throw new ScoutError('invalid_request', `Element <${el.tag}> is not a <select>`);
      const want = [].concat(action.value ?? []).map(String);
      const opts = [...descendants(el)].filter((o) => o.tag === 'option');
      const have = new Set(opts.map(optionValue));
      const missing = want.filter((v) => !have.has(v));
      if (!want.length || missing.length) throw new ScoutError('element_not_found', `No <option> with value ${JSON.stringify(missing.length ? missing : want)}`, { details: { selector: action.selector } });
      setSt(page, el, { value: el.attrs.multiple !== undefined ? want : want[0] });
      return done(t);
    }
    case 'check': case 'uncheck': {
      if (el.tag !== 'input' || !['checkbox', 'radio'].includes(inputType(el))) throw new ScoutError('invalid_request', 'Element is not a checkbox or radio input');
      if (t === 'uncheck' && inputType(el) === 'radio') throw unsupported('Radio buttons cannot be unchecked');
      setChecked(page, el, t === 'check');
      return done(t);
    }
    case 'press': {
      if ((action.key ?? '') !== 'Enter') throw unsupported(`Key "${action.key}" is not supported; only Enter on a form field submits`);
      const form = closest(el, 'form');
      if (!form) throw unsupported('Enter only has an effect inside a <form>');
      const submitter = [...descendants(form)].find((b) => (b.tag === 'button' && (b.attrs.type || 'submit') === 'submit') || (b.tag === 'input' && ['submit', 'image'].includes(inputType(b))));
      return navigated(t, session, await submitForm(cfg, session, form, submitter, signal, timeoutMs));
    }
    case 'submit': {
      const form = closest(el, 'form');
      if (!form) throw new ScoutError('invalid_request', 'Selected element is not inside a <form>');
      return navigated(t, session, await submitForm(cfg, session, form, null, signal, timeoutMs));
    }
    default:
      throw new ScoutError('invalid_request', `Unknown action type "${t}"`);
  }
}
