//! Font loading and text measurement.
//!
//! Measurement is the part of layout that must agree with a browser to the pixel,
//! so the engine does not guess: it reads the same font file the browser used and
//! sums the same advances, with kerning.

use crate::raster::{self, GlyphMetrics};
use std::collections::HashMap;
use std::sync::Arc;

/// Where the engine looks for the default font. `SCOUT_RENDER_FONT` overrides it;
/// otherwise the common Linux, macOS and Windows paths are tried in order.
const DEFAULT_FONT_CANDIDATES: &[&str] = &[
    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    "/usr/share/fonts/dejavu/DejaVuSans.ttf",
    "/Library/Fonts/DejaVuSans.ttf",
    "C:\\Windows\\Fonts\\DejaVuSans.ttf",
    "/System/Library/Fonts/Supplemental/Arial.ttf",
    "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
];

/// Build a face from a font file. `rustybuzz::Face` borrows its data, so the bytes
/// are leaked once: a process loads a handful of fonts and keeps them for its
/// lifetime, which is exactly what a leaked `'static` slice models. fontdue parses
/// its own copy, so the file is read once and split.
/// Every face is loaded once per process. The bytes are leaked to give HarfBuzz
/// its `'static` borrow, so loading them per render would leak a font file per
/// render — which is exactly what embedding this engine in a long-lived server
/// cannot afford. A cached face is cheap to clone (an `Arc`).
fn load_face(path: &str, family: String, weight: u16) -> Option<Arc<Face>> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<HashMap<(String, u16), Arc<Face>>>> =
        std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
    let key = (path.to_string(), weight);
    if let Some(hit) = cache.lock().ok()?.get(&key) {
        return Some(hit.clone());
    }
    let face = Arc::new(load_face_uncached(path, family, weight)?);
    if let Ok(mut map) = cache.lock() {
        map.insert(key, face.clone());
    }
    Some(face)
}

fn load_face_uncached(path: &str, family: String, weight: u16) -> Option<Face> {
    let bytes = std::fs::read(path).ok()?;
    let owned: &'static [u8] = Box::leak(bytes.into_boxed_slice());
    // HarfBuzz's defaults are a browser's defaults: kerning and standard ligatures
    // are on, and no features need to be requested explicitly.
    let shaper = rustybuzz::Face::from_slice(owned, 0)?;
    Some(Face {
        data: FaceData::Static(shaper),
        family,
        weight,
    })
}

/// Which loaded face a CSS family name means. The engine ships three real faces
/// (sans, serif, mono); web font names are mapped onto them the way a browser
/// falls back when the named font is missing.
fn slot_for(name: &str) -> &'static str {
    let name = name.trim_matches(|c| c == '\'' || c == '"');
    let monospace = [
        "monospace", "mono", "courier", "courier new", "consolas", "menlo", "monaco",
        "dejavu sans mono", "sfmono-regular", "roboto mono", "source code pro", "fira code",
        "ubuntu mono", "liberation mono", "cascadia code", "jetbrains mono",
    ];
    let serif = [
        "serif", "times", "times new roman", "georgia", "garamond", "palatino", "cambria",
        "book antiqua", "baskerville", "caslon", "minion pro", "charter", "iowan old style",
        "liberation serif", "dejavu serif", "tex gyre termes", "computer modern", "ui-serif",
        "apple garamond", "hoefler text", "source serif pro", "merriweather", "lora",
    ];
    let lower = name.to_ascii_lowercase();
    if monospace.contains(&lower.as_str()) {
        "monospace"
    } else if serif.contains(&lower.as_str()) {
        "serif"
    } else {
        "default"
    }
}

/// Right-to-left scripts, by block: Hebrew, Arabic and their presentation forms.
fn is_rtl(text: &str) -> bool {
    text.chars().any(|c| {
        matches!(c as u32,
            0x0590..=0x05FF | 0x0600..=0x06FF | 0x0700..=0x074F | 0x0750..=0x077F
            | 0x0780..=0x07BF | 0x08A0..=0x08FF | 0xFB1D..=0xFB4F | 0xFB50..=0xFDFF
            | 0xFE70..=0xFEFF)
    })
}

/// One positioned glyph from the shaper.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ShapedGlyph {
    pub glyph_index: u16,
    /// Which loaded face drew it. A glyph id only means anything inside the face
    /// that produced it, and a run can span faces when a script falls back.
    pub face: usize,
    /// Pen offset from the start of the run, in CSS pixels.
    pub x: f32,
    /// Vertical offset (positive is up).
    pub y: f32,
    pub advance: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FontMetrics {
    pub ascent: f32,
    pub descent: f32,
    pub line_gap: f32,
}

impl FontMetrics {
    /// CSS `line-height: normal` for this font at this size.
    pub fn normal_line_height(&self) -> f32 {
        self.ascent - self.descent + self.line_gap
    }
}

/// Advance width of `0` per pixel of font size, for the default face. `ch` is
/// defined as that width, and layout arithmetic needs it without a font handle.
static CH_RATIO: std::sync::OnceLock<f32> = std::sync::OnceLock::new();

/// The `ch` unit's resolution factor for the default face (about 0.55em here).
pub fn ch_ratio() -> f32 {
    *CH_RATIO.get().unwrap_or(&0.55)
}

/// A `@font-face` rule the page declared, already resolved to absolute URLs.
#[derive(Debug, Clone)]
pub struct WebFont {
    pub family: String,
    pub urls: Vec<String>,
    pub weight: u16,
    pub italic: bool,
}

thread_local! {
    static PENDING_WEB_FONTS: std::cell::RefCell<Vec<WebFont>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Record `@font-face` rules on the current thread for legacy callers; prefer
/// [`FontContext::set_web_fonts`] so faces stay scoped to a single context.
pub fn register_web_fonts(fonts: Vec<WebFont>) {
    PENDING_WEB_FONTS.with(|slot| {
        let mut list = slot.borrow_mut();
        list.clear();
        list.extend(fonts);
    });
}

const MAX_FONT_BYTES: usize = 16 * 1024 * 1024;
const MAX_WOFF_TABLES: usize = 128;
const MAX_SFNT_BYTES: usize = 32 * 1024 * 1024;

/// Turn a font file into a face without leaking heap memory (`Box::leak`).
fn face_from_bytes(bytes: Vec<u8>, family: &str, weight: u16) -> Option<Face> {
    if bytes.len() > MAX_FONT_BYTES {
        return None;
    }
    let bytes = if bytes.starts_with(b"wOF2") {
        let decoded = woff2_patched::convert_woff2_to_ttf(&mut std::io::Cursor::new(bytes)).ok()?;
        if decoded.len() > MAX_SFNT_BYTES {
            return None;
        }
        decoded
    } else if bytes.starts_with(b"wOFF") {
        decode_woff(&bytes)?
    } else {
        bytes
    };
    if bytes.len() > MAX_SFNT_BYTES {
        return None;
    }
    // Validate once up front that HarfBuzz/ttf-parser accepts the table directory.
    let _ = rustybuzz::Face::from_slice(&bytes, 0)?;
    Some(Face {
        data: FaceData::Owned(bytes),
        family: family.to_string(),
        weight,
    })
}

/// Rebuild the sfnt table directory of a WOFF 1.0 file with strict bounds so
/// untrusted `orig_len` headers can never trigger an allocator abort (`SIGABRT`).
fn decode_woff(bytes: &[u8]) -> Option<Vec<u8>> {
    use std::io::Read;
    if bytes.len() < 44 || bytes.len() > MAX_FONT_BYTES {
        return None;
    }
    let u16_at = |i: usize| u16::from_be_bytes([bytes[i], bytes[i + 1]]) as usize;
    let u32_at = |i: usize| {
        u32::from_be_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]) as usize
    };
    let flavor = u32_at(4);
    let num_tables = u16_at(12);
    if num_tables == 0 || num_tables > MAX_WOFF_TABLES {
        return None;
    }
    let dir_end = 44usize.checked_add(num_tables.checked_mul(20)?)?;
    if dir_end > bytes.len() {
        return None;
    }
    let mut total_orig = 0usize;
    let mut tables: Vec<(u32, Vec<u8>)> = Vec::with_capacity(num_tables);
    for i in 0..num_tables {
        let rec = 44 + i * 20;
        let tag = u32_at(rec) as u32;
        let offset = u32_at(rec + 4);
        let comp_len = u32_at(rec + 8);
        let orig_len = u32_at(rec + 12);
        let end = offset.checked_add(comp_len)?;
        if end > bytes.len() || comp_len > orig_len || orig_len > MAX_FONT_BYTES {
            return None;
        }
        total_orig = total_orig.checked_add(orig_len)?;
        if total_orig > MAX_SFNT_BYTES {
            return None;
        }
        let raw = &bytes[offset..end];
        let data = if comp_len < orig_len {
            let mut out = Vec::with_capacity(orig_len.min(64 * 1024));
            let limit = (orig_len as u64).saturating_add(1);
            flate2::read::ZlibDecoder::new(raw)
                .take(limit)
                .read_to_end(&mut out)
                .ok()?;
            if out.len() != orig_len {
                return None;
            }
            out
        } else {
            raw.to_vec()
        };
        tables.push((tag, data));
    }
    // sfnt: offset table, then one 16-byte directory entry per table.
    let mut out = Vec::new();
    out.extend_from_slice(&(flavor as u32).to_be_bytes());
    out.extend_from_slice(&(num_tables as u16).to_be_bytes());
    let search = 16usize;
    let mut entry_selector = 0u16;
    while (1usize << (entry_selector + 1)) <= num_tables.max(1) {
        entry_selector += 1;
    }
    let search_range = (1usize << entry_selector) * search;
    out.extend_from_slice(&(search_range as u16).to_be_bytes());
    out.extend_from_slice(&(entry_selector as u16).to_be_bytes());
    out.extend_from_slice(&((num_tables * 16).saturating_sub(search_range) as u16).to_be_bytes());
    let mut offset = 12 + num_tables * 16;
    let mut body = Vec::new();
    for (tag, data) in &tables {
        let mut padded = data.clone();
        while padded.len() % 4 != 0 {
            padded.push(0);
        }
        // A WOFF table entry's checksum is over the uncompressed data.
        let mut checksum = 0u32;
        for chunk in padded.chunks(4) {
            let mut word = [0u8; 4];
            word.copy_from_slice(chunk);
            checksum = checksum.wrapping_add(u32::from_be_bytes(word));
        }
        out.extend_from_slice(&tag.to_be_bytes());
        out.extend_from_slice(&checksum.to_be_bytes());
        out.extend_from_slice(&(offset as u32).to_be_bytes());
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        body.extend_from_slice(&padded);
        offset += padded.len();
    }
    out.extend_from_slice(&body);
    Some(out)
}

pub struct FontContext {
    faces: HashMap<(String, u16), Arc<Face>>,
    default: Option<Arc<Face>>,
    #[allow(dead_code)]
    root_font_size: f32,
    missing: Vec<String>,
    loaded_from: Option<String>,
    /// Faces used for characters the CSS family cannot draw.
    fallbacks: Vec<Arc<Face>>,
    /// Scripts whose faces exist on disk but are not loaded yet, in order.
    pending: Vec<&'static str>,
    /// Rasterised glyphs, keyed by face index, glyph id and size in 1/64 px.
    raster_cache: HashMap<(usize, u16, u32), Arc<(GlyphMetrics, Vec<u8>)>>,
    /// `@font-face` declarations and loaded faces scoped strictly to this render.
    web_fonts: Vec<WebFont>,
    web_cache: HashMap<String, Option<Arc<Face>>>,
}

enum FaceData {
    Static(rustybuzz::Face<'static>),
    Owned(Vec<u8>),
}

pub struct Face {
    data: FaceData,
    #[allow(dead_code)]
    family: String,
    #[allow(dead_code)]
    weight: u16,
}

impl Face {
    fn with_shaper<R>(&self, f: impl FnOnce(&rustybuzz::Face<'_>) -> R) -> Option<R> {
        match &self.data {
            FaceData::Static(shaper) => Some(f(shaper)),
              FaceData::Owned(bytes) => {
                let shaper = rustybuzz::Face::from_slice(bytes, 0)?;
                Some(f(&shaper))
            }
        }
    }

    fn has_glyph(&self, ch: char) -> bool {
        self.with_shaper(|s| s.glyph_index(ch).is_some())
            .unwrap_or(false)
    }

    /// Vertical metrics from `hhea`, scaled from font units.
    fn metrics_at(&self, px: f32) -> FontMetrics {
        self.with_shaper(|shaper| {
            let upem = shaper.units_per_em();
            if upem <= 0 {
                return FontMetrics { ascent: px * 0.93, descent: -px * 0.24, line_gap: 0.0 };
            }
            let scale = px / upem as f32;
            FontMetrics {
                ascent: shaper.ascender() as f32 * scale,
                descent: shaper.descender() as f32 * scale,
                line_gap: shaper.line_gap() as f32 * scale,
            }
        })
        .unwrap_or(FontMetrics { ascent: px * 0.93, descent: -px * 0.24, line_gap: 0.0 })
    }

    fn advance_of(&self, ch: char, px: f32) -> f32 {
        self.with_shaper(|shaper| {
            let upem = shaper.units_per_em().max(1) as f32;
            match shaper.glyph_index(ch).and_then(|id| shaper.glyph_hor_advance(id)) {
                Some(units) => units as f32 * px / upem,
                None => 0.0,
            }
        })
        .unwrap_or(0.0)
    }
}

impl Default for FontContext {
    fn default() -> Self {
        Self::new(16.0)
    }
}

impl FontContext {
    pub fn new(root_font_size: f32) -> Self {
        let mut ctx = FontContext {
            faces: HashMap::new(),
            default: None,
            root_font_size,
            missing: Vec::new(),
            loaded_from: None,
            fallbacks: Vec::new(),
            pending: Vec::new(),
            raster_cache: HashMap::new(),
            web_fonts: Vec::new(),
            web_cache: HashMap::new(),
        };
        let candidates: Vec<String> = std::env::var("SCOUT_RENDER_FONT")
            .ok()
            .into_iter()
            .chain(DEFAULT_FONT_CANDIDATES.iter().map(|s| s.to_string()))
            .collect();
        for path in candidates {
            if std::path::Path::new(&path).exists() {
                if let Some(face) = load_face(&path, "default".to_string(), 400) {
                    ctx.faces.insert(("default".to_string(), 400), face.clone());
                    ctx.default = Some(face);
                    ctx.loaded_from = Some(path.clone());
                    let ratio = ctx.default.as_ref().map(|f| f.advance_of('0', 100.0) / 100.0);
                    if let Some(ratio) = ratio {
                        if ratio.is_finite() && ratio > 0.0 {
                            let _ = CH_RATIO.set(ratio);
                        }
                    }
                    // The bold companion, when the same family ships one.
                    let bold_path = path
                        .replace("DejaVuSans.ttf", "DejaVuSans-Bold.ttf")
                        .replace("LiberationSans-Regular.ttf", "LiberationSans-Bold.ttf")
                        .replace("Arial.ttf", "Arial Bold.ttf");
                    if bold_path != path {
                        if std::path::Path::new(&bold_path).exists() {
                            if let Some(bold) = load_face(&bold_path, "default".to_string(), 700) {
                                ctx.faces.insert(("default".to_string(), 700), bold);
                                ctx.loaded_from = Some(format!("{}, {}", path, bold_path));
                            }
                        }
                    }
                    break;
                }
            }
        }
        // A browser does not render every family with one face: a page that asks
        // for a serif gets a serif. Load the serif and mono companions when they
        // are installed, under the CSS generic names, and let `resolve` map the
        // web's usual family names onto them.
        let companions: [(&str, &[&str]); 2] = [
            ("serif", &[
                "/usr/share/fonts/truetype/dejavu/DejaVuSerif.ttf",
                "/usr/share/fonts/dejavu/DejaVuSerif.ttf",
                "/Library/Fonts/DejaVuSerif.ttf",
                "/usr/share/fonts/truetype/liberation/LiberationSerif-Regular.ttf",
                "/System/Library/Fonts/Supplemental/Times New Roman.ttf",
            ]),
            ("monospace", &[
                "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
                "/usr/share/fonts/dejavu/DejaVuSansMono.ttf",
                "/Library/Fonts/DejaVuSansMono.ttf",
                "/usr/share/fonts/truetype/liberation/LiberationMono-Regular.ttf",
                "/System/Library/Fonts/Supplemental/Courier New.ttf",
            ]),
        ];
        for (generic, paths) in companions {
            for path in paths {
                if !std::path::Path::new(path).exists() {
                    continue;
                }
                let Some(face) = load_face(path, generic.to_string(), 400) else { continue };
                ctx.faces.insert((generic.to_string(), 400), face);
                let bold_path = path
                    .replace("DejaVuSerif.ttf", "DejaVuSerif-Bold.ttf")
                    .replace("DejaVuSansMono.ttf", "DejaVuSansMono-Bold.ttf")
                    .replace("LiberationSerif-Regular.ttf", "LiberationSerif-Bold.ttf")
                    .replace("LiberationMono-Regular.ttf", "LiberationMono-Bold.ttf");
                if bold_path.as_str() != *path && std::path::Path::new(&bold_path).exists() {
                    if let Some(bold) = load_face(&bold_path, generic.to_string(), 700) {
                        ctx.faces.insert((generic.to_string(), 700), bold);
                    }
                }
                break;
            }
        }
        // Scripts the Latin faces do not cover. Their files are recorded here and
        // opened the first time a character needs one: shaping English text must
        // not pay for a CJK collection the page never uses.
        const SCRIPT_FONTS: &[&[&str]] = &[
            &[
                "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
                "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc",
                "/usr/share/fonts/opentype/noto/NotoSansCJKsc-Regular.otf",
                "/System/Library/Fonts/PingFang.ttc",
            ],
            &[
                "/usr/share/fonts/truetype/Sarai/Sarai.ttf",
                "/usr/share/fonts/truetype/lohit-devanagari/Lohit-Devanagari.ttf",
                "/usr/share/fonts/truetype/Gargi/Gargi.ttf",
                "/usr/share/fonts/truetype/samyak/Samyak-Devanagari.ttf",
                "/usr/share/fonts/truetype/noto/NotoSansDevanagari-Regular.ttf",
            ],
            &[
                "/usr/share/fonts/truetype/noto/NotoNaskhArabic-Regular.ttf",
                "/usr/share/fonts/truetype/kacst/KacstOne.ttf",
                "/usr/share/fonts/truetype/freefont/FreeSerif.ttf",
            ],
            &[
                "/usr/share/fonts/truetype/noto/NotoSansThai-Regular.ttf",
                "/usr/share/fonts/truetype/tlwg/Loma.ttf",
                "/usr/share/fonts/truetype/freefont/FreeSerif.ttf",
            ],
            &[
                "/usr/share/fonts/truetype/noto/NotoSansHebrew-Regular.ttf",
                "/usr/share/fonts/truetype/freefont/FreeSerif.ttf",
            ],
        ];
        for candidates in SCRIPT_FONTS {
            if let Some(path) = candidates.iter().find(|p| std::path::Path::new(p).exists()) {
                ctx.pending.push(path);
            }
        }
        // Every face already loaded is a fallback candidate, most specific first.
        let mut all: Vec<Arc<Face>> = Vec::new();
        for (_, face) in ctx.faces.iter() {
            if !all.iter().any(|f| Arc::ptr_eq(f, face)) {
                all.push(face.clone());
            }
        }
        ctx.fallbacks = all;
        ctx
    }

    /// Load the next unloaded script face, or return false when none are left.
    fn load_next_pending(&mut self) -> bool {
        while let Some(path) = self.pending.first().copied() {
            self.pending.remove(0);
            if let Some(face) = load_face(path, "fallback".to_string(), 400) {
                self.fallbacks.push(face);
                return true;
            }
        }
        false
    }

    /// Register `@font-face` declarations strictly on this context, avoiding
    /// cross-session global font caches or leaked memory.
    pub fn set_web_fonts(&mut self, fonts: Vec<WebFont>) {
        self.web_fonts = fonts;
        self.web_cache.clear();
    }

    /// A face the page declared through `@font-face`, loaded on first use.
    ///
    /// Weight matching mirrors the browser's: an exact face wins, then 400, then
    /// the nearest declared weight in the same direction.
    fn web_face(&mut self, family: &str, weight: u16) -> Option<Arc<Face>> {
        if self.web_fonts.is_empty() {
            PENDING_WEB_FONTS.with(|slot| {
                let list = slot.borrow();
                if !list.is_empty() {
                    self.web_fonts = list.clone();
                }
            });
        }
        let decls: Vec<WebFont> = self
            .web_fonts
            .iter()
            .filter(|f| f.family.eq_ignore_ascii_case(family))
            .cloned()
            .collect();
        if decls.is_empty() {
            return None;
        }
        let pick = decls
            .iter()
            .find(|f| f.weight == weight)
            .or_else(|| decls.iter().find(|f| f.weight == 400))
            .or_else(|| decls.iter().min_by_key(|f| (f.weight as i32 - weight as i32).abs()));
        let decl = pick?;
        let key = format!("{}|{}|{}", decl.family.to_ascii_lowercase(), decl.weight, decl.italic);
        if let Some(hit) = self.web_cache.get(&key) {
            return hit.clone();
        }
        let mut loaded = None;
        for url in &decl.urls {
            let Some(bytes) = crate::image::bytes_at(url) else { continue };
            if let Some(face) = face_from_bytes(bytes, &decl.family, decl.weight) {
                let arc = Arc::new(face);
                if !self.fallbacks.iter().any(|f| Arc::ptr_eq(f, &arc)) {
                    self.fallbacks.push(arc.clone());
                }
                loaded = Some(arc);
                break;
            }
        }
        self.web_cache.insert(key, loaded.clone());
        loaded
    }

    /// The first face that can draw `ch`, after the one CSS asked for.
    fn face_for(&mut self, primary: &Arc<Face>, ch: char) -> Option<Arc<Face>> {
        if primary.has_glyph(ch) {
            return Some(primary.clone());
        }
        if let Some(face) = self.fallbacks.iter().find(|f| f.has_glyph(ch)) {
            return Some(face.clone());
        }
        // Not covered by anything loaded: open the remaining script faces one at
        // a time until one can draw this character, and cache the miss by leaving
        // loaded faces at the front of the search for every later call.
        while self.load_next_pending() {
            if let Some(face) = self.fallbacks.iter().find(|f| f.has_glyph(ch)) {
                return Some(face.clone());
            }
        }
        None
    }

    /// Characters no loaded face can draw, so a caller can report them instead of
    /// shipping a page full of empty boxes in silence.
    pub fn uncovered(&mut self, family: &str, weight: u16, text: &str) -> Vec<char> {
        let Some(primary) = self.resolve(family, weight) else {
            return Vec::new();
        };
        let mut missing: Vec<char> = Vec::new();
        for ch in text.chars() {
            if ch.is_whitespace() || ch.is_control() {
                continue;
            }
            if self.face_for(&primary, ch).is_none() && !missing.contains(&ch) {
                missing.push(ch);
            }
        }
        missing
    }

    pub fn has_font(&self) -> bool {
        self.default.is_some()
    }

    pub fn source(&self) -> Option<&str> {
        self.loaded_from.as_deref()
    }

    /// Resolve a CSS family list to a face. Unknown families fall back to the
    /// default face and are recorded, so a snapshot can say the text was measured
    /// with a substitute.
    pub fn resolve(&mut self, families: &str, weight: u16) -> Option<Arc<Face>> {
        // Walk the family list the way a browser does: the first name that maps
        // to a face we have wins, and only when none does is the default used.
        // A family the page loaded through `@font-face` beats any local
        // substitute: the browser measured its text with exactly that file.
        for name in families.split(',') {
            let name = name.trim().trim_matches(|c| c == '\'' || c == '"').to_ascii_lowercase();
            if name.is_empty() {
                continue;
            }
            if let Some(face) = self.web_face(&name, weight) {
                return Some(face);
            }
        }
        let mut chosen: Option<String> = None;
        let mut first_name = String::new();
        for name in families.split(',') {
            let name = name.trim().trim_matches(|c| c == '\'' || c == '"').to_ascii_lowercase();
            if name.is_empty() {
                continue;
            }
            if first_name.is_empty() {
                first_name = name.clone();
            }
            let slot = slot_for(&name);
            if self.faces.contains_key(&(slot.to_string(), weight))
                || self.faces.contains_key(&(slot.to_string(), 400))
            {
                chosen = Some(slot.to_string());
                break;
            }
        }
        let slot = chosen.unwrap_or_else(|| "default".to_string());
        let key = (slot.clone(), weight);
        if let Some(face) = self.faces.get(&key) {
            return Some(face.clone());
        }
        // A heavy weight must keep its bold measurements, or laid-out text will
        // not match a browser.
        if weight >= 600 {
            if let Some(bold) = self.faces.get(&(slot.clone(), 700)).cloned() {
                self.faces.insert(key, bold.clone());
                return Some(bold);
            }
            if let Some(bold) = self.faces.get(&("default".to_string(), 700)).cloned() {
                self.faces.insert(key, bold.clone());
                return Some(bold);
            }
        }
        if !first_name.is_empty()
            && !first_name.starts_with("dejavu")
            && !self.missing.contains(&first_name)
        {
            self.missing.push(first_name.clone());
        }
        let face = self
            .faces
            .get(&(slot.clone(), 400))
            .cloned()
            .or_else(|| self.default.clone())?;
        self.faces.insert(key, face.clone());
        Some(face)
    }

    /// Glyph indices, pen positions and the face that owns each glyph.
    pub fn layout_text(
        &mut self,
        family: &str,
        weight: u16,
        px: f32,
        text: &str,
    ) -> Vec<(u16, f32, usize)> {
        self.shape(family, weight, px, text)
            .into_iter()
            .map(|g| (g.glyph_index, g.x, g.face))
            .collect()
    }

    /// Rasterise a glyph with the face that shaped it. Bitmaps are cached by
    /// face, glyph and size: a page repeats its letters many times over.
    pub fn rasterize_glyph_of(
        &mut self,
        face: usize,
        index: u16,
        px: f32,
    ) -> Option<(GlyphMetrics, Vec<u8>)> {
        let key = (face, index, (px * 64.0).round() as u32);
        if let Some(hit) = self.raster_cache.get(&key) {
            return Some(hit.as_ref().clone());
        }
        let target = self.fallbacks.get(face)?.clone();
        let drawn = target.with_shaper(|s| raster::glyph(s, index, px)).flatten();
        if let Some(drawn) = &drawn {
            self.raster_cache.insert(key, Arc::new(drawn.clone()));
        }
        drawn
    }

    /// A glyph's coverage bitmap and metrics at a size.
    pub fn rasterize_glyph(
        &mut self,
        family: &str,
        weight: u16,
        index: u16,
        px: f32,
    ) -> Option<(GlyphMetrics, Vec<u8>)> {
        let face = self.resolve(family, weight)?;
        face.with_shaper(|s| raster::glyph(s, index, px)).flatten()
    }

    pub fn missing_families(&self) -> &[String] {
        &self.missing
    }

    /// The height of the font's content area: ascent + descent. An inline box is
    /// this tall, independent of the line box it sits in.
    pub fn content_height(&mut self, family: &str, weight: u16, px: f32) -> f32 {
        let m = self.metrics(family, weight, px);
        (m.ascent - m.descent).max(0.0)
    }

    pub fn metrics(&mut self, family: &str, weight: u16, px: f32) -> FontMetrics {
        match self.resolve(family, weight) {
            Some(face) => face.metrics_at(px),
            None => FontMetrics {
                ascent: px * 0.93,
                descent: -px * 0.24,
                line_gap: 0.0,
            },
        }
    }

    /// Shape a run: HarfBuzz positions every glyph, so ligatures, kerning and
    /// script reordering all land where a browser puts them.
    fn shape(&mut self, family: &str, weight: u16, px: f32, text: &str) -> Vec<ShapedGlyph> {
        let Some(primary) = self.resolve(family, weight) else {
            return Vec::new();
        };
        if text.is_empty() {
            return Vec::new();
        }

        // Split the run wherever the face has to change. Shaping a whole string
        // with the wrong face would lose its script's joins and marks, so the
        // break points are per character, not per word.
        let mut segments: Vec<(Arc<Face>, String)> = Vec::new();
        for ch in text.chars() {
            let Some(face) = self.face_for(&primary, ch) else {
                // Nothing can draw it; keep it with the previous segment so the
                // advance still advances and the gap stays visible.
                match segments.last_mut() {
                    Some((_, run)) => run.push(ch),
                    None => segments.push((primary.clone(), ch.to_string())),
                }
                continue;
            };
            match segments.last_mut() {
                Some((current, run)) if Arc::ptr_eq(current, &face) => run.push(ch),
                _ => segments.push((face, ch.to_string())),
            }
        }

        let mut out: Vec<ShapedGlyph> = Vec::new();
        let mut pen = 0.0f32;
        for (face, run) in segments {
            let face_index = self
                .fallbacks
                .iter()
                .position(|f| Arc::ptr_eq(f, &face))
                .unwrap_or(usize::MAX);
            face.with_shaper(|shaper| {
                let mut buffer = rustybuzz::UnicodeBuffer::new();
                buffer.push_str(&run);
                // Right-to-left scripts need the direction set: it is what turns
                // Arabic's contextual forms and reordering on.
                let direction = if is_rtl(&run) {
                    rustybuzz::Direction::RightToLeft
                } else {
                    rustybuzz::Direction::LeftToRight
                };
                buffer.set_direction(direction);
                buffer.guess_segment_properties();
                let scale = px / shaper.units_per_em().max(1) as f32;
                let output = rustybuzz::shape(shaper, &[], buffer);

                for (info, pos) in output.glyph_infos().iter().zip(output.glyph_positions().iter()) {
                    out.push(ShapedGlyph {
                        glyph_index: info.glyph_id as u16,
                        face: face_index,
                        x: pen + pos.x_offset as f32 * scale,
                        y: -(pos.y_offset as f32) * scale,
                        advance: pos.x_advance as f32 * scale,
                    });
                    pen += pos.x_advance as f32 * scale;
                }
            });
        }
        out
    }

    /// Advance width of a shaped run.
    pub fn measure(&mut self, family: &str, weight: u16, px: f32, text: &str) -> f32 {
        let glyphs = self.shape(family, weight, px, text);
        if glyphs.is_empty() {
            return text.chars().count() as f32 * px * 0.5;
        }
        glyphs.iter().map(|g| g.advance).sum()
    }

}

/// Process-wide font size for `rem`.
pub fn root_font_size() -> f32 {
    16.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_font_loads() {
        let ctx = FontContext::new(16.0);
        assert!(ctx.has_font(), "no font found; set SCOUT_RENDER_FONT");
    }

    #[test]
    fn measurement_is_monotonic_in_size() {
        let mut ctx = FontContext::new(16.0);
        let small = ctx.measure("DejaVu Sans", 400, 12.0, "Hello world");
        let large = ctx.measure("DejaVu Sans", 400, 24.0, "Hello world");
        assert!(large > small * 1.9 && large < small * 2.1, "{} vs {}", small, large);
    }

    #[test]
    fn a_space_has_a_width() {
        let mut ctx = FontContext::new(16.0);
        assert!(ctx.measure("DejaVu Sans", 400, 16.0, " ") > 0.0);
    }
}
