//! Decoding images, for `img` and `background-image: url(...)`.
//!
//! The engine is offline by design, so an image resolves to a local file or a
//! `data:` URI — never to a network request. Decoded bitmaps are cached by source
//! string, because a background repeated on twenty cards must be decoded once.

use std::collections::HashMap;

use zune_jpeg as jpeg;

/// Where an image came from, once resolved.
pub enum Source {
    /// A path on disk.
    File(std::path::PathBuf),
    /// Base64 bytes from a `data:image/...;base64,` URI.
    Data(Vec<u8>),
}

/// Where relative `src`/`url()` values point, and whether remote images may be
/// downloaded. Set once by the caller: a library user gets no I/O it did not ask
/// for, while the CLI points this at the page it is rendering.
pub struct Environment {
    /// Directory (or page URL) that relative paths resolve against.
    pub base: String,
    /// Whether `http(s)` images may be fetched (via the CLI's downloader).
    pub network: bool,
    /// Where downloaded images are cached.
    pub cache: std::path::PathBuf,
}

/// Somewhere to get remote bytes from. The CLI installs one that shells out to
/// `curl`; an embedder (Scout) installs one that goes through its own SSRF
/// policy, cookie jar and byte budget instead. A library user who installs
/// nothing gets an engine that never opens a socket.
pub type Fetcher = std::sync::Arc<dyn Fn(&str) -> Option<Vec<u8>> + Send + Sync>;

static FETCHER: std::sync::OnceLock<std::sync::Mutex<Option<Fetcher>>> = std::sync::OnceLock::new();

fn fetcher_slot() -> &'static std::sync::Mutex<Option<Fetcher>> {
    FETCHER.get_or_init(|| std::sync::Mutex::new(None))
}

/// Install (or clear) the remote fetcher used for `http(s)` images and fonts.
pub fn set_fetcher(fetcher: Option<Fetcher>) {
    if let Ok(mut slot) = fetcher_slot().lock() {
        *slot = fetcher;
    }
}

fn current_fetcher() -> Option<Fetcher> {
    fetcher_slot().lock().ok()?.clone()
}

static ENVIRONMENT: std::sync::OnceLock<std::sync::Mutex<Environment>> = std::sync::OnceLock::new();

fn environment() -> &'static std::sync::Mutex<Environment> {
    ENVIRONMENT.get_or_init(|| {
        std::sync::Mutex::new(Environment {
            base: String::new(),
            network: false,
            cache: std::env::temp_dir().join("scout-render-cache"),
        })
    })
}

/// Point the image loader at the page being rendered.
pub fn configure(base: &str, network: bool, cache: std::path::PathBuf) {
    if let Ok(mut env) = environment().lock() {
        env.base = base.to_string();
        env.network = network;
        env.cache = cache;
    }
}

/// Resolve a `src`/`url()` value to a local source, relative to the page.
pub fn resolve(src: &str) -> Option<Source> {
    let src = src.trim();
    if let Some(rest) = src.strip_prefix("data:") {
        let (meta, payload) = rest.split_once(',')?;
        if meta.ends_with(";base64") {
            return Some(Source::Data(base64_decode(payload)?));
        }
        return Some(Source::Data(payload.as_bytes().to_vec()));
    }
    if src.starts_with("http://") || src.starts_with("https://") || src.starts_with("//") {
        return remote(src);
    }
    let path = if src.starts_with("file://") || std::path::Path::new(src).is_absolute() {
        std::path::PathBuf::from(src.trim_start_matches("file://"))
    } else {
        let base = environment().lock().ok()?.base.clone();
        if base.is_empty() {
            std::path::PathBuf::from(src)
        } else {
            std::path::PathBuf::from(crate::resource::resolve(&base, src))
        }
    };
    if path.exists() {
        Some(Source::File(path))
    } else {
        None
    }
}

/// A remote image, downloaded once into the cache when the caller allowed it.
fn remote(src: &str) -> Option<Source> {
    let (network, cache) = {
        let env = environment().lock().ok()?;
        (env.network, env.cache.clone())
    };
    if !network {
        return None;
    }
    let url = if src.starts_with("//") { format!("https:{}", src) } else { src.to_string() };
    // An embedder's fetcher owns the policy, so it is asked first and the curl
    // path below is only the CLI's stand-in for one.
    if let Some(fetch) = current_fetcher() {
        return fetch(&url).map(Source::Data);
    }
    // Cache by URL hash: no filename games, no collisions between hosts.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in url.bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let extension = url
        .rsplit('.')
        .next()
        .filter(|e| matches!(*e, "png" | "jpg" | "jpeg" | "gif" | "webp" | "svg" | "avif" | "bmp" | "woff2" | "woff" | "ttf" | "otf" | "ttc"))
        .unwrap_or("bin");
    std::fs::create_dir_all(&cache).ok()?;
    let path = cache.join(format!("{:016x}.{}", hash, extension));
    if !path.exists() {
        let status = std::process::Command::new("curl")
            .args([
                "-sSL",
                "--max-time",
                "10",
                "--max-filesize",
                "8388608",
                "-A",
                "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126 Safari/537.36",
                "-o",
            ])
            .arg(&path)
            .arg(&url)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .ok()?;
        if !status.success() {
            let _ = std::fs::remove_file(&path);
            return None;
        }
    }
    Some(Source::File(path))
}

/// Bytes at an absolute URL or path, through the same cache the image loader uses.
pub fn bytes_at(url: &str) -> Option<Vec<u8>> {
    let source = resolve(url)?;
    bytes_of(&source)
}

fn bytes_of(source: &Source) -> Option<Vec<u8>> {
    match source {
        Source::File(path) => std::fs::read(path).ok(),
        Source::Data(data) => Some(data.clone()),
    }
}

/// Width and height in pixels, without keeping the decoded pixels.
pub fn intrinsic_size(src: &str) -> Option<(f32, f32)> {
    let source = resolve(src)?;
    let head = head_of(&source, 64 * 1024)?;
    let all = if is_svg(&head) { bytes_of(&source) } else { None };
    if let Some(bytes) = all {
        return svg_size(&bytes);
    }
    header_size(&head).map(|(w, h)| (w as f32, h as f32))
}

/// The first `limit` bytes of a source. Sizing an image needs its header, not
/// its pixels: reading a whole photograph to answer "how wide is it" is how a
/// layout pass ends up holding a page's worth of image data.
fn head_of(source: &Source, limit: usize) -> Option<Vec<u8>> {
    match source {
        Source::Data(data) => Some(data[..data.len().min(limit)].to_vec()),
        Source::File(path) => {
            use std::io::Read;
            let mut file = std::fs::File::open(path).ok()?;
            let mut buffer = vec![0; limit];
            let mut filled = 0;
            while filled < limit {
                match file.read(&mut buffer[filled..]) {
                    Ok(0) => break,
                    Ok(n) => filled += n,
                    Err(_) => break,
                }
            }
            buffer.truncate(filled);
            Some(buffer)
        }
    }
}

/// Decoded RGBA8 pixels, cached per process.
pub fn decode(src: &str) -> Option<std::sync::Arc<Bitmap>> {
    decode_for(src, None)
}

/// Decode a bitmap no larger than `target` needs. A photograph painted into a
/// 320 px card is decoded straight down to that card: a 4000×3000 JPEG is 48 MB
/// held at full size and 0.4 MB at the size it is actually drawn, and a page can
/// have a dozen of them.
pub fn decode_for(src: &str, target: Option<(u32, u32)>) -> Option<std::sync::Arc<Bitmap>> {
    use std::sync::{Arc, Mutex, OnceLock};
    // Bucket the target size so that the same image at nearly the same size is a
    // cache hit rather than a re-decode. The bucket is the painted size doubled:
    // two pixels of source per output pixel still samples cleanly.
    let bucket = target.map(|(w, h)| {
        let wanted = (w.max(1) * 2, h.max(1) * 2);
        (wanted.0.div_ceil(64) * 64, wanted.1.div_ceil(64) * 64)
    });
    type Cache = HashMap<(String, Option<(u32, u32)>), Option<Arc<Bitmap>>>;
    static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let key = (src.to_string(), bucket);
    if let Some(hit) = cache.lock().ok()?.get(&key) {
        return hit.clone();
    }
    let bitmap = (|| {
        let source = resolve(src)?;
        let bytes = bytes_of(&source)?;
        if is_svg(&bytes) {
            return rasterize_svg(&bytes).map(Arc::new);
        }
        let (width, height, pixels) = decode_pixels(&bytes)?;
        let bitmap = Bitmap { width, height, pixels };
        let bitmap = match bucket {
            Some((max_w, max_h)) if bitmap.width > max_w || bitmap.height > max_h => {
                shrink(&bitmap, max_w, max_h)
            }
            _ => bitmap,
        };
        Some(Arc::new(bitmap))
    })();
    let mut guard = cache.lock().ok()?;
    // A screenshot decodes a page's images at a handful of sizes; a long-lived
    // server would otherwise keep every size of every image it ever painted.
    if guard.len() >= 64 {
        guard.clear();
    }
    guard.insert(key, bitmap.clone());
    bitmap
}

/// Box-filter a bitmap down to fit `max_w`×`max_h`, preserving aspect ratio.
fn shrink(bitmap: &Bitmap, max_w: u32, max_h: u32) -> Bitmap {
    let scale = (max_w as f32 / bitmap.width as f32).min(max_h as f32 / bitmap.height as f32);
    let width = ((bitmap.width as f32 * scale).round() as u32).max(1);
    let height = ((bitmap.height as f32 * scale).round() as u32).max(1);
    let mut pixels = vec![0u8; (width * height * 4) as usize];
    for y in 0..height {
        let y0 = (y * bitmap.height / height) as usize;
        let y1 = (((y + 1) * bitmap.height / height) as usize).max(y0 + 1);
        for x in 0..width {
            let x0 = (x * bitmap.width / width) as usize;
            let x1 = (((x + 1) * bitmap.width / width) as usize).max(x0 + 1);
            let mut sum = [0u32; 4];
            let mut count = 0u32;
            for sy in y0..y1 {
                for sx in x0..x1 {
                    let i = (sy * bitmap.width as usize + sx) * 4;
                    for (channel, total) in sum.iter_mut().enumerate() {
                        *total += u32::from(bitmap.pixels[i + channel]);
                    }
                    count += 1;
                }
            }
            let out = ((y * width + x) * 4) as usize;
            for (channel, total) in sum.iter().enumerate() {
                pixels[out + channel] = (total / count.max(1)) as u8;
            }
        }
    }
    Bitmap { width, height, pixels }
}

/// Serialise a node's subtree back to markup. An inline `<svg>` is already a
/// vector document inside the HTML, so its own markup is the most faithful
/// thing to hand the rasteriser.
pub fn markup(doc: &crate::dom::Document, id: crate::dom::NodeId) -> Option<Vec<u8>> {
    fn write(doc: &crate::dom::Document, id: crate::dom::NodeId, root: bool, out: &mut String) {
        let node = &doc.nodes[id];
        match node.kind {
            crate::dom::Kind::Text => {
                out.push_str(&node.text.replace('&', "&amp;").replace('<', "&lt;"));
            }
            crate::dom::Kind::Element => {
                out.push('<');
                out.push_str(&node.name);
                // An `<svg>` written in HTML is parsed into the SVG namespace, but
                // the attribute that names it is not in the markup any more. The
                // rasteriser needs it back to recognise the root.
                if root && node.name == "svg" && node.attr("xmlns").is_none() {
                    out.push_str(" xmlns=\"http://www.w3.org/2000/svg\"");
                }
                for (name, value) in &node.attrs {
                    out.push(' ');
                    out.push_str(name);
                    out.push_str("=\"");
                    out.push_str(&value.replace('&', "&amp;").replace('"', "&quot;"));
                    out.push('"');
                }
                out.push('>');
                for child in &node.children {
                    write(doc, *child, false, out);
                }
                out.push_str("</");
                out.push_str(&node.name);
                out.push('>');
            }
            _ => {}
        }
    }
    let mut out = String::new();
    write(doc, id, true, &mut out);
    if out.is_empty() {
        None
    } else {
        Some(out.into_bytes())
    }
}

/// Rasterise an inline `<svg>` at its intrinsic size, so it paints through the
/// same bitmap path as an `img`. Returns `None` for a document `usvg` rejects.
pub fn inline_svg(markup: &Option<Vec<u8>>) -> Option<std::sync::Arc<Bitmap>> {
    let bytes = markup.as_ref()?;
    rasterize_svg(bytes).map(std::sync::Arc::new)
}

/// Dimensions from the file header alone. A layout pass asks every image for its
/// size; decoding the pixels to answer would cost a photograph's worth of memory
/// per image on a page that only needed geometry.
fn header_size(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G']) && bytes.len() >= 24 {
        let mut size = [0u8; 4];
        size.copy_from_slice(&bytes[16..20]);
        let w = u32::from_be_bytes(size);
        let mut size = [0u8; 4];
        size.copy_from_slice(&bytes[20..24]);
        let h = u32::from_be_bytes(size);
        return (w > 0 && h > 0).then_some((w, h));
    }
    if bytes.starts_with(&[0xFF, 0xD8]) {
        let info = jpeg::JpegDecoder::new(bytes).info()?;
        let (w, h) = (u32::from(info.width), u32::from(info.height));
        return (w > 0 && h > 0).then_some((w, h));
    }
    None
}

/// Width, height and straight RGBA8 pixels of a PNG or JPEG. Both formats go to
/// decoders the binary already links: `png` for the screenshots it writes and
/// `zune-jpeg` for photographs, instead of a general image library and its
/// colour management.
fn decode_pixels(bytes: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        let mut decoder = png::Decoder::new(bytes);
        // Expand palettes and sub-byte greyscale, so every PNG arrives as 8-bit
        // RGB or RGBA and the conversion below is a widening step.
        decoder.set_transformations(png::Transformations::EXPAND);
        let mut reader = decoder.read_info().ok()?;
        let mut buf = vec![0; reader.output_buffer_size()];
        let info = reader.next_frame(&mut buf).ok()?;
        buf.truncate(info.buffer_size());
        let (w, h) = (info.width, info.height);
        let pixels = match info.color_type {
            png::ColorType::Rgba => buf,
            png::ColorType::Rgb => widen(&buf, 3, |p| [p[0], p[1], p[2], 255]),
            png::ColorType::GrayscaleAlpha => widen(&buf, 2, |p| [p[0], p[0], p[0], p[1]]),
            png::ColorType::Grayscale => widen(&buf, 1, |p| [p[0], p[0], p[0], 255]),
            png::ColorType::Indexed => return None,
        };
        return Some((w, h, pixels));
    }
    if bytes.starts_with(&[0xFF, 0xD8]) {
        let mut decoder = jpeg::JpegDecoder::new(bytes);
        let info = decoder.info()?;
        let pixels = decoder.decode().ok()?;
        let (w, h) = (u32::from(info.width), u32::from(info.height));
        let out = match info.components {
            1 => widen(&pixels, 1, |p| [p[0], p[0], p[0], 255]),
            4 => pixels,
            _ => widen(&pixels, 3, |p| [p[0], p[1], p[2], 255]),
        };
        return Some((w, h, out));
    }
    None
}

/// Repack `stride`-byte pixels into RGBA8.
fn widen(bytes: &[u8], stride: usize, to_rgba: impl Fn(&[u8]) -> [u8; 4]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() / stride * 4);
    for pixel in bytes.chunks_exact(stride) {
        out.extend_from_slice(&to_rgba(pixel));
    }
    out
}

/// Straight RGBA8 pixels.
/// Whether these bytes are an SVG document rather than a bitmap.
pub fn is_svg(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(1024)];
    let text = String::from_utf8_lossy(head);
    text.contains("<svg") || (text.trim_start().starts_with("<?xml") && text.contains("svg"))
}

/// The intrinsic size of an SVG: its `width`/`height` attributes, or the size of
/// its `viewBox`, which is what a browser uses for an `<img>` with no CSS size.
fn svg_size(bytes: &[u8]) -> Option<(f32, f32)> {
    let tree = usvg::Tree::from_data(bytes, &usvg::Options::default()).ok()?;
    let size = tree.size();
    Some((size.width(), size.height()))
}

/// Rasterise an SVG at its intrinsic size, so it can be painted through the same
/// sampling path as a PNG. A vector logo is crisp at any size this way, and —
/// more importantly for layout — an `<img src="logo.svg">` has a real size.
fn rasterize_svg(bytes: &[u8]) -> Option<Bitmap> {
    let tree = usvg::Tree::from_data(bytes, &usvg::Options::default()).ok()?;
    let size = tree.size().to_int_size();
    let (w, h) = (size.width().max(1), size.height().max(1));
    let mut pixmap = tiny_skia::Pixmap::new(w, h)?;
    resvg::render(&tree, tiny_skia::Transform::identity(), &mut pixmap.as_mut());
    // tiny-skia stores premultiplied alpha; the painter wants straight RGBA.
    let mut pixels = Vec::with_capacity((w * h * 4) as usize);
    for px in pixmap.pixels() {
        let c = px.demultiply();
        pixels.extend_from_slice(&[c.red(), c.green(), c.blue(), c.alpha()]);
    }
    Some(Bitmap { width: w, height: h, pixels })
}

pub struct Bitmap {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

impl Bitmap {
    /// Bilinear sample at normalised coordinates.
    pub fn sample(&self, u: f32, v: f32) -> Option<[u8; 4]> {
        if self.width == 0 || self.height == 0 {
            return None;
        }
        let x = (u * self.width as f32 - 0.5).clamp(0.0, (self.width - 1) as f32);
        let y = (v * self.height as f32 - 0.5).clamp(0.0, (self.height - 1) as f32);
        let x0 = x.floor() as u32;
        let y0 = y.floor() as u32;
        let x1 = (x0 + 1).min(self.width - 1);
        let y1 = (y0 + 1).min(self.height - 1);
        let fx = x - x0 as f32;
        let fy = y - y0 as f32;
        let px = |xx: u32, yy: u32| -> [f32; 4] {
            let i = ((yy * self.width + xx) * 4) as usize;
            [
                self.pixels[i] as f32,
                self.pixels[i + 1] as f32,
                self.pixels[i + 2] as f32,
                self.pixels[i + 3] as f32,
            ]
        };
        let a = px(x0, y0);
        let b = px(x1, y0);
        let c = px(x0, y1);
        let d = px(x1, y1);
        let mut out = [0u8; 4];
        for i in 0..4 {
            let top = a[i] * (1.0 - fx) + b[i] * fx;
            let bottom = c[i] * (1.0 - fx) + d[i] * fx;
            out[i] = (top * (1.0 - fy) + bottom * fy).round().clamp(0.0, 255.0) as u8;
        }
        Some(out)
    }
}

fn base64_decode(input: &str) -> Option<Vec<u8>> {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut lookup = [255u8; 256];
    for (i, c) in TABLE.iter().enumerate() {
        lookup[*c as usize] = i as u8;
    }
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let mut buffer = 0u32;
    let mut bits = 0u32;
    for c in input.bytes() {
        if c == b'=' || c.is_ascii_whitespace() {
            continue;
        }
        let value = lookup[c as usize];
        if value == 255 {
            return None;
        }
        buffer = (buffer << 6) | value as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }
    Some(out)
}
