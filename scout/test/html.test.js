import test from 'node:test';
import assert from 'node:assert/strict';
import { parseHtml, text, decodeEntities, innerHTML } from '../src/html.js';
import { querySelectorAll as qa, querySelector as q } from '../src/selector.js';

test('parses malformed HTML: implied end tags, void elements, unclosed inline', () => {
  const d = parseHtml('<div><p>one<p>two<span>unclosed <b>bold</div><ul><li>a<li>b</ul><br><img src=x><table><tr><td>1<td>2<tr><td>3</table>');
  assert.equal(qa(d, 'p').length, 2);
  assert.equal(qa(d, 'li').length, 2);
  assert.equal(qa(d, 'tr').length, 2);
  assert.equal(qa(d, 'td').length, 3);
  assert.equal(text(q(d, 'div')), 'one twounclosed bold');
  assert.equal(q(d, 'img').attrs.src, 'x');
});
test('raw-text elements do not parse their contents', () => {
  const d = parseHtml('<script>if (a<b) { document.write("<div>x</div>") }</script><style>a>b{}</style><p>after</p>');
  assert.equal(qa(d, 'div').length, 0);
  assert.equal(text(d), 'after');
});
test('entities and attributes', () => {
  assert.equal(decodeEntities('&amp; &lt; &#65; &#x42; &nbsp;&bogus; &#0;'), '& < A B \u00a0&bogus; \ufffd');
  const d = parseHtml(`<a HREF='/x?a=1&amp;b=2' data-x=y class="A  B" disabled>t</a>`);
  const a = q(d, 'a');
  assert.equal(a.attrs.href, '/x?a=1&b=2');
  assert.equal(a.attrs['data-x'], 'y');
  assert.equal(a.attrs.disabled, '');
});
test('tolerates garbage: unterminated tags, stray < and quotes', () => {
  const d = parseHtml('a < b <div class="x>text</div> <p "weird">ok</p> <');
  assert.ok(d.children.length > 0);
  assert.equal(text(q(d, 'p') ?? d).length > 0, true);
});
test('node limit is enforced', () => {
  assert.throws(() => parseHtml('<p>x</p>'.repeat(100), { maxNodes: 50 }), (e) => e.code === 'response_too_large');
});
test('deep nesting is bounded, not fatal', () => {
  const d = parseHtml('<div>'.repeat(5000) + 'deep' + '</div>'.repeat(5000), { maxDepth: 100 });
  assert.equal(text(d), 'deep');
});
test('innerHTML round-trips escaped text', () => {
  const d = parseHtml('<p>a &lt; b &amp; c</p>');
  assert.equal(innerHTML(q(d, 'p')), 'a &lt; b &amp; c');
});

const doc = parseHtml(`<div id="a" class="x y"><ul><li class="i">1</li><li class="i sel">2</li><li class="i">3</li></ul>
<a href="https://e.com/file.pdf" lang="en-US">pdf</a><a href="/rel">rel</a><p>p1</p><p>p2</p></div><span>s</span>`);
test('selectors: combinators, attributes, pseudos, groups', () => {
  assert.equal(qa(doc, 'li').length, 3);
  assert.equal(qa(doc, 'div#a.x.y > ul li.i').length, 3);
  assert.equal(qa(doc, 'li.sel').length, 1);
  assert.equal(qa(doc, 'li:nth-child(odd)').length, 2);
  assert.equal(qa(doc, 'li:nth-child(2n+1)').length, 2);
  assert.equal(qa(doc, 'li:first-child, li:last-child').length, 2);
  assert.equal(qa(doc, 'li:not(.sel)').length, 2);
  assert.equal(qa(doc, 'a[href^="https"]').length, 1);
  assert.equal(qa(doc, 'a[href$=".pdf"]').length, 1);
  assert.equal(qa(doc, 'a[href*="rel"]').length, 1);
  assert.equal(qa(doc, 'a[lang|=en]').length, 1);
  assert.equal(qa(doc, 'p + p').length, 1);
  assert.equal(qa(doc, 'ul ~ p').length, 2);
  assert.equal(qa(doc, 'div span').length, 0);
  assert.equal(qa(doc, '*').length >= 10, true);
  assert.equal(qa(doc, 'li', 2).length, 2);
});
test('unsupported or invalid selectors raise invalid_request instead of matching nothing', () => {
  for (const bad of ['a:hover', 'a::before', '', 'a >', '> a', 'a[', 'li:nth-child(foo)', ':not()', 'a,,b']) {
    assert.throws(() => qa(doc, bad), (e) => e.code === 'invalid_request', bad);
  }
});
