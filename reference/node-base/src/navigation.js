import { ScoutError } from './errors.js';
import { guardedFetch } from './security.js';
import { parseHtml, text } from './html.js';
import { querySelector, querySelectorAll } from './selector.js';

const RETRY_STATUS = new Set([500, 502, 503, 504, 408]);

function jsWarnings(page) {
  if (page.kind !== 'html') return [];
  const body = querySelector(page.doc, 'body') ?? page.doc;
  const visible = text(body).length;
  const scripts = querySelectorAll(page.doc, 'script:not([type*="json"])').length;
  const noscript = querySelector(page.doc, 'noscript');
  const needsJs = (scripts > 0 && visible < 200) || (noscript && /javascript/i.test(text(noscript)) && visible < 500);
  return needsJs ? [{
    code: 'js_required_likely',
    message: 'Page appears to render its content with JavaScript. Scout core does not execute JavaScript, so extraction may be empty or incomplete.',
  }] : [];
}

/** Load a URL into the session. Replaces (and releases) the previous page. */
export async function doNavigate(cfg, session, { url, method = 'GET', body, headers, timeoutMs }, signal) {
  const t0 = Date.now();
  const res = await guardedFetch(cfg, {
    url, method, body, headers, jar: session.jar, signal,
    timeoutMs: Math.min(timeoutMs ?? cfg.navTimeoutMs, cfg.maxNavTimeoutMs),
  });
  if (res.status >= 400) {
    const restricted = res.status === 401 || res.status === 403 || res.status === 429;
    throw new ScoutError('navigation_failed', `Server responded with HTTP ${res.status}` +
      (restricted ? '; access is restricted and Scout does not bypass authentication or anti-bot controls' : ''), {
      retryable: RETRY_STATUS.has(res.status), details: { status: res.status, url: res.url },
    });
  }
  const page = { url: res.url, status: res.status, contentType: res.contentType, bytes: res.bytes, kind: 'html', doc: null, json: null, formState: new WeakMap() };
  if (res.contentType.includes('json')) {
    try { page.json = JSON.parse(res.body); page.kind = 'json'; }
    catch { throw new ScoutError('unsupported_content', 'Response declared JSON but could not be parsed'); }
  } else {
    page.doc = parseHtml(res.body, { maxNodes: cfg.maxDomNodes });
  }
  session.setPage(page);
  return {
    url: page.url, status: page.status, contentType: page.contentType, bytes: page.bytes,
    title: page.doc ? text(querySelector(page.doc, 'title') ?? { type: 'text', text: '' }) : null,
    redirects: res.redirects, warnings: jsWarnings(page), durationMs: Date.now() - t0,
  };
}

export function pageSummary(page) {
  return { url: page.url, status: page.status, title: page.doc ? text(querySelector(page.doc, 'title') ?? { type: 'text', text: '' }) : null };
}
