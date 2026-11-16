# scout-render

The layout and paint engine behind Scout's `page.layout()` and
`page.screenshot()`. It turns HTML and CSS into boxes and pixels without a
browser: no download, no GPU, no JavaScript engine in the loop.

It is a workspace member of Scout, not a separate project. The engine is a
library (`scout_render`) plus a small CLI for inspecting a page on its own.

## Command line

```bash
cargo build --release
./target/release/scout-render snapshot   page.html -o page.json --width 1280 --height 800
./target/release/scout-render screenshot page.html -o page.png  --width 1280 --height 800 --full-page
```

`snapshot` prints every element's box, tag, text and CSS path, which is the JSON
shape an agent needs in order to act on a node. `<file.html>` may also be an
`http(s)` URL; `--base <url>` resolves a saved page's relative assets against its
origin and `--offline` keeps the render on local disk. Fonts come from
`SCOUT_RENDER_FONT`, or the first DejaVu Sans on the system, with serif, mono and
script fallbacks loaded only when a character needs them.

## Library

```rust
let mut fonts = scout_render::text::FontContext::new(16.0);
let (doc, styles, tree) =
    scout_render::render(&html, scout_render::Viewport { width: 1280.0, height: 800.0 }, &mut fonts);
let canvas = scout_render::paint::paint(&doc, &styles, &tree, &mut fonts, Default::default());
```

`render_with` takes a `ResourceLoader` and returns the assets it could not load,
so an embedder supplies its own fetching policy - Scout routes stylesheets,
images and fonts through its guarded fetcher and its cookie jar.

## What it draws

Block, inline, inline-block, flex and table layout; the CSS 2.1 box model
including margin collapsing, floats and positioning; backgrounds, gradients,
shadows, borders, `border-radius`, images, inline SVG and `data:` URIs;
`@font-face` with WOFF, WOFF2, TTF and OTF; and text shaped by HarfBuzz, so
Arabic, Hebrew, Devanagari and CJK use the same shaping and kerning a browser
does.

Not drawn: canvas, video, WebGL, filters and 3D transforms. `display: grid` is
not laid out as a grid: its children become blocks in document order. Stripe,
with 105 grid containers, shows the cost - 3% of its boxes land within 2 px of
Chromium's, against 12.2% across the benchmark as a whole.

## Measured

- **Geometry, 20 live sites, 11,935 matched boxes** against Chromium: 0.3% of
  boxes within 2 px page-absolute, 12.2% within 2 px of their parent-relative
  position. `bench/results.md`.
- **Pixels, 16 fixtures** against Chromium screenshots: median SSIM 0.993, worst
  0.889 (`tests/golden_paint.rs`).
- **CSS, 4,708 fixture properties** computed identically; **layout**, 171/171
  boxes within 2 px.
- Memory: 7 MB RSS to lay out example.com, 15 MB for a 1280×800 screenshot;
  14 MB and 23 MB for the 23,000-pixel Stripe page; 45 MB and 49 MB for
  Wikipedia. The engine adds 2.1 MB to Scout's binary (5.8 MB without it,
  7.9 MB with it).

## Checks

```bash
cargo test -p scout-render                     # style, layout and paint gates
bench/run.sh && node bench/chrome.mjs && python3 bench/compare.py   # the 20-site run
```

The benchmark fetches each page once to `bench/sites/`, renders that exact file
with `--base`, and has Chromium screenshot the same bytes, so neither side is
measured on markup the other never saw. `bench/results/` is generated.

A build without the engine is a supported configuration: `cargo build --release
--no-default-features` drops the crate, the two tools and the `/v1/screenshot`
route, and saves 2.1 MB.

```
src/dom.rs      parsing, tree, CSS paths
src/css/        tokeniser, selectors, cascade, values
src/style.rs    computed styles, media queries, shorthands
src/layout.rs   block, inline, flex, table, floats, text runs
src/text.rs     font loading, fallbacks, HarfBuzz shaping
src/raster.rs   outline rasterisation to coverage bitmaps
src/paint.rs    boxes, backgrounds, borders, images, SVG, glyphs
src/resource.rs stylesheets and images, through an embedder-supplied loader
```
