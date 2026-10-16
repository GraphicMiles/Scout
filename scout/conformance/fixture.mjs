/**
 * The fixture site from the Node implementation's `test/helpers.js`, ported
 * without changing a single response body. Keeping it byte-identical is what
 * makes the suite a conformance suite: the same inputs the Node runtime was
 * judged on are served to the Rust runtime.
 *
 * Only the process plumbing changed (no imports from Scout internals).
 */
import http from 'node:http';
import zlib from 'node:zlib';

export const KEY = 'test-key-0123456789abcdef';
export const OTHER_KEY = 'other-key-0123456789abcdef';

export const PRODUCTS = [
  { name: 'Alpha Lamp', price: '$1,299.50', amount: 1299.5, date: '2026-03-01', path: '/p/alpha' },
  { name: 'Beta Desk', price: '€45', amount: 45, date: '2026-03-02', path: '/p/beta' },
  { name: 'Gamma Chair', price: '₦15,000', amount: 15000, date: '2026-03-03', path: '/p/gamma' },
];

export const productsHtml = () => `<!doctype html><html lang="en"><head><title>Shop &amp; Co</title>
<meta name="description" content="Test shop"><meta property="og:title" content="Shop OG"><link rel="canonical" href="/products">
<script type="application/ld+json">{"@type":"ItemList","n":3}</script></head><body><h1>Products</h1><ul>
${PRODUCTS.map((p) => `<li class="item"><h2 class="name">${p.name}</h2><span class="price">${p.price}</span><time datetime="${p.date}">d</time><a class="link" href="${p.path}">view</a></li>`).join('\n')}
</ul></body></html>`;

export const ytHtml = (videos, { withMeta = true } = {}) => {
  const data = {
    metadata: withMeta ? { channelMetadataRenderer: { title: 'Test Channel', externalId: 'UCabc123', channelUrl: 'https://www.youtube.com/channel/UCabc123', vanityChannelUrl: 'http://www.youtube.com/@testchan', description: 'A "quoted" } brace { description' } } : {},
    contents: { tabs: [{ content: { items: videos.map((v) => ({ richItemRenderer: { content: { videoRenderer: { videoId: v.id, title: { runs: [{ text: v.title }] }, publishedTimeText: { simpleText: v.ago }, viewCountText: { simpleText: '10 views' }, lengthText: { simpleText: '1:00' } } } } })) } }] },
  };
  return `<html><head><title>x</title></head><body><script nonce="a">var ytInitialData = ${JSON.stringify(data)};</script></body></html>`;
};

const form = `<html><head><title>Form</title></head><body>
<form id="f" action="/submit" method="post"><input name="q" type="text" value="default"><input type="hidden" name="token" value="t1">
<input type="password" name="pw"><input type="checkbox" name="agree" value="yes"><textarea name="note">hello</textarea>
<select name="color"><option value="red">Red</option><option value="blue">Blue</option></select>
<button type="submit" name="go" value="1">Go</button><button type="button" id="js">JS</button></form>
<form action="/echo-get" method="get"><input name="s"><input type="submit" value="Search"></form>
<a id="next" href="/p/alpha">next</a><a id="js-link" href="javascript:void(0)">x</a><div id="onclick" onclick="x()">c</div>
<form action="/submit" method="post" enctype="multipart/form-data"><input type="file" name="f"></form></body></html>`;

export async function startFixture() {
  const flaky = new Map();
  const timers = new Set();
  const hits = { cookieEcho: [] };
  const server = http.createServer((req, res) => {
    const u = new URL(req.url, 'http://x');
    const html = (body, status = 200, headers = {}) => { res.writeHead(status, { 'content-type': 'text/html; charset=utf-8', ...headers }); res.end(body); };
    switch (u.pathname) {
      case '/': return html('<title>Home</title><body><h1>Hello</h1><a href="/products">Products</a><a href="mailto:a@b.c">mail</a></body>');
      case '/products': return html(productsHtml());
      case '/redirect': return html('', 302, { location: '/products' });
      case '/redirect-loop': return html('', 302, { location: '/redirect-loop' });
      case '/redirect-meta': return html('', 302, { location: 'http://169.254.169.254/latest/meta-data/' });
      case '/redirect-private': return html('', 302, { location: 'http://10.0.0.5/admin' });
      case '/redirect-file': return html('', 302, { location: 'file:///etc/passwd' });
      case '/gzip': { const z = zlib.gzipSync(productsHtml()); res.writeHead(200, { 'content-type': 'text/html', 'content-encoding': 'gzip' }); return res.end(z); }
      case '/huge': { res.writeHead(200, { 'content-type': 'text/html' }); let n = 0; const w = () => { while (n++ < 400) { if (!res.write('a'.repeat(16384))) return res.once('drain', w); } res.end(); }; return w(); }
      case '/image': res.writeHead(200, { 'content-type': 'image/png' }); return res.end(Buffer.alloc(1000));
      case '/slow': { const t = setTimeout(() => html('slow'), 5000); timers.add(t); res.on('close', () => clearTimeout(t)); return; }
      case '/malformed': return html('<div><p>one<p>two<span>unclosed <b>bold</div><ul><li>a<li>b</ul><a href=x.html>link</a><table><tr><td>1<td>2<tr><td>3</table>');
      case '/cookie-set': return html('ok', 200, { 'set-cookie': ['sid=abc123; Path=/; HttpOnly', 'evil=1; Domain=.evil.com', 'tld=1; Domain=com'] });
      case '/cookie-echo': hits.cookieEcho.push(req.headers.cookie ?? ''); res.writeHead(200, { 'content-type': 'application/json' }); return res.end(JSON.stringify({ cookie: req.headers.cookie ?? '' }));
      case '/form': return html(form);
      case '/submit': { let b = ''; req.on('data', (c) => (b += c)); req.on('end', () => html(`<title>Posted</title><pre id="out">${b}</pre><span id="ct">${req.headers['content-type']}</span><span id="ref">${req.headers.referer ?? ''}</span>`)); return; }
      case '/echo-get': return html(`<title>Got</title><pre id="out">${u.search}</pre>`);
      case '/flaky': { const id = u.searchParams.get('id'); const n = (flaky.get(id) ?? 0) + 1; flaky.set(id, n); if (n <= Number(u.searchParams.get('n') ?? 1)) return html('busy', 503); return html('<title>Recovered</title>'); }
      case '/forbidden': return html('no', 403);
      case '/json': res.writeHead(200, { 'content-type': 'application/json' }); return res.end('{"ok":true,"n":[1,2]}');
      case '/js-shell': return html('<html><body><div id="app"></div><script src="/app.js"></script></body></html>');
      case '/large': { const n = Number(u.searchParams.get('items') ?? 5000); let s = '<html><head><title>Large</title></head><body><ul>'; for (let i = 0; i < n; i++) s += `<li class="item"><h2 class="name">Item ${i}</h2><span class="price">$${i}.00</span></li>`; return html(s + '</ul></body></html>'); }
      case '/@testchan/videos': return html(ytHtml(Array.from({ length: 5 }, (_, i) => ({ id: `vid${i}`, title: `Video ${i}`, ago: `${i + 1} days ago` }))));
      case '/@empty/videos': return html('<html><body>nothing here</body></html>');
      default: if (u.pathname.startsWith('/p/')) return html('<title>Product</title>'); return html('not found', 404);
    }
  });
  await new Promise((r) => server.listen(0, '127.0.0.1', r));
  const url = `http://127.0.0.1:${server.address().port}`;
  return { url, hits, close: () => new Promise((r) => { for (const t of timers) clearTimeout(t); server.closeAllConnections(); server.close(r); }) };
}
