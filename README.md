# Scout

A browser automation runtime written in Rust, with an authenticated HTTP API.
It fetches a page, parses the DOM, runs the page's JavaScript in a sandboxed
engine, fills forms and returns structured fields.

It is not a Chromium replacement. It is one small binary that starts in
milliseconds and idles around five megabytes, where a real browser idles in the
hundreds.

It also lays pages out and screenshots them: the renderer is part of the same
binary, so a crawler that only needed geometry never pays for a browser.

## Why it exists

Automation gets expensive at the edges. A full browser per worker costs memory
and boot time you cannot afford when you are running a hundred small tasks, and
the tasks are usually ordinary: read this page, click that, give me the fields
back. Scout keeps the useful part of a browser and drops the rest.

## What you can do

- **Drive it like a headless browser** from JavaScript, through the driver
  package.
- **Fetch and parse** a page without rendering a document.
- **Run page JavaScript** in a sandboxed engine, so scripts on the page behave.
- **Fill forms and extract structured data**, which is what most automation is
  actually for.
- **Call it over HTTP** with a bearer key, so a service can use it from any
  language.
- **Lay a page out and screenshot it** - boxes, computed styles and a PNG, with
  no browser involved.
- **Run it on a small machine.** One binary, no browser download; 5.8 MB without
  the renderer, 7.9 MB with it.

## Install

```bash
npm install @graphicmiles/scout
```

The postinstall step downloads the release binary for your platform, so no
compiler is needed.

```js
import { launch } from '@graphicmiles/scout/driver';

const scout = await launch();          // starts the binary on a local port
const page = await scout.newPage();
await page.goto('https://example.com/');
console.log(await page.title());
await scout.close();
```

From source, or on a platform without a release binary (Rust 1.88 or newer):

```bash
cd scout
cargo build --release
SCOUT_BIN=./target/release/scout npm install github:GraphicMiles/Scout
```

## Rendering

`page.layout()` returns every element's box, tag, text and computed styles;
`page.screenshot()` returns a PNG. Both run the engine in `scout/render`, a CSS
layout and paint implementation with no browser, no downloads and no GPU.

```js
const layout = await page.layout();          // { elements, document, viewport }
const png = await page.screenshot();         // PNG bytes, or { encoding: 'base64' }
```

Over HTTP: `POST /v1/tools/call` with `{"name": "page.layout", "arguments":
{sessionId}}`, and `POST /v1/screenshot` with `{ sessionId, width, height,
fullPage }`, which answers with a PNG (`?format=json` for base64).

What it agrees with Chromium on, measured on 20 live sites and 11,935 matched
boxes (see `scout/render/bench/results.md`): 0.3% of boxes land within 2 px of
Chromium's page-absolute position, 12.2% when measured against their parent,
and screenshots score a mean SSIM of 0.682 against Chromium's. On the 16-page
fixture gate, where Chromium's own screenshots are the reference, the median is
0.993.

It draws block, inline, flex and table layout, the CSS 2.1 box model, floats,
positioning, gradients, shadows, background images, SVG, `@font-face`, and text
shaped by HarfBuzz - Latin, Arabic, Hebrew, Devanagari and CJK. Memory stays
flat: laying out example.com peaks at 7 MB of RSS and a 1280×800 screenshot at
15 MB; a 23,000-pixel-tall Stripe page costs 14 MB to lay out and 23 MB to
screenshot, and Wikipedia's front page, the heaviest in the benchmark, 45 and
49 MB. It renders the DOM the session holds, so it sees what page scripts
changed in the document, but it does not run canvas, video or WebGL, and a
`display: grid` container lays its children out as blocks rather than columns.

## What is not here

- No video, canvas or WebGL support.
- The sandbox runs the page's JavaScript, not a full browser security model.
  Treat the content it returns as untrusted input.
- Platform coverage follows the published release binaries. On other targets you
  build from source.

## How it is built

```
scout/          the Rust runtime: HTTP API, DOM, sandbox, extraction
scout/render/   the layout and paint engine, and its benchmark
driver/         the JavaScript client published to npm
package.json, postinstall.mjs   npm packaging and binary download
```

### Checks

Three workflows run on the repository: `ci.yml` for the build and tests,
`release.yml` for the binaries and `npm-publish.yml` for the package.

## Licence

MIT, see `LICENSE`.
