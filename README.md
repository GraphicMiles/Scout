# Scout

A browser automation runtime written in Rust, with an authenticated HTTP API.
It fetches a page, parses the DOM, runs the page's JavaScript in a sandboxed
engine, fills forms and returns structured fields.

It is not a Chromium replacement. It is one small binary that starts in
milliseconds and idles around five megabytes, where a real browser idles in the
hundreds.

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
- **Run it on a small machine.** One binary, no browser download.

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

## What is not here

- No rendering and no screenshots. If a site needs a real layout engine to work,
  this is the wrong tool.
- No video, canvas or WebGL support.
- The sandbox runs the page's JavaScript, not a full browser security model.
  Treat the content it returns as untrusted input.
- Platform coverage follows the published release binaries. On other targets you
  build from source.

## How it is built

```
scout/       the Rust runtime: HTTP API, DOM, sandbox, extraction
driver/      the JavaScript client published to npm
package.json, postinstall.mjs   npm packaging and binary download
```

### Checks

Three workflows run on the repository: `ci.yml` for the build and tests,
`release.yml` for the binaries and `npm-publish.yml` for the package.
