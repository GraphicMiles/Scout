//! Scout Render: HTML + CSS to geometry, without a browser.
//!
//! The engine has four stages, each independently testable against the goldens in
//! `corpus/goldens/` (recorded from Chromium):
//!
//!   parse  → `dom::Document`
//!   style  → `style::ComputedStyles`   (cascade, inheritance, computed values)
//!   layout → `layout::LayoutTree`      (block, inline, flex)
//!   paint  → `paint::Raster`           (display list, tiny-skia, PNG)
//!
//! Anything the engine does not support is reported, never silently dropped.

pub mod css;
pub mod dom;
pub mod flex;
pub mod image;
pub mod layout;
pub mod paint;
pub mod raster;
pub mod resource;
pub mod style;
pub mod text;

pub use dom::{Document, Kind, NodeId};
pub use style::Display;
pub use layout::{layout, LayoutTree, Rect};
pub use paint::{paint, Canvas};
pub use style::{compute_styles, ComputedStyles, Style};

/// A viewport the page is laid out in.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Viewport {
    pub width: f32,
    pub height: f32,
}

impl Default for Viewport {
    fn default() -> Self {
        Viewport { width: 800.0, height: 600.0 }
    }
}

/// Parse a document and compute every element's style, with no external
/// resources: only `<style>` elements are read. This is what tests and goldens
/// use, and it can never touch the network.
pub fn style_document(
    html: &str,
    viewport: Viewport,
    fonts: &mut text::FontContext,
) -> (Document, ComputedStyles) {
    let (doc, computed, _) = style_document_with(html, "", viewport, fonts, &resource::Offline);
    (doc, computed)
}

/// Parse a document and compute styles, loading linked stylesheets through
/// `loader`. `base` is the page's own address — a URL or a filesystem path —
/// against which relative `href`s are resolved.
///
/// The third return value names every resource that failed to load, so callers
/// can report it rather than render an unstyled page in silence.
pub fn style_document_with(
    html: &str,
    base: &str,
    viewport: Viewport,
    fonts: &mut text::FontContext,
    loader: &dyn resource::ResourceLoader,
) -> (Document, ComputedStyles, Vec<(String, String)>) {
    let doc = Document::parse(html);
    let mut sheets = Vec::new();
    // Document order matters: a `<style>` after a `<link>` must win, and the
    // other way round. Walk the elements once and collect in sequence.
    let mut linked = std::collections::HashMap::new();
    let (loaded, mut failures) = resource::load_stylesheets(&doc, base, loader);
    for sheet in &loaded {
        if let Some(css) = &sheet.css {
            linked.insert(sheet.href.clone(), css.clone());
        }
    }
    let mut pending: Vec<String> = doc.linked_stylesheets.clone();
    for id in doc.elements() {
        let node = &doc.nodes[id];
        match node.name.as_str() {
            "style" => {
                let css: String = node
                    .children
                    .iter()
                    .filter_map(|c| match doc.nodes[*c].kind {
                        dom::Kind::Text => Some(doc.nodes[*c].text.as_str()),
                        _ => None,
                    })
                    .collect();
                if !css.trim().is_empty() {
                    sheets.push(css::Stylesheet::parse_with_base(&css, base));
                }
            }
            "link" => {
                if let Some(href) = node.attr("href") {
                    if let Some(index) = pending.iter().position(|h| h == href) {
                        if let Some(css) = linked.get(href) {
                            let sheet_base = resource::resolve(base, href);
                            sheets.push(css::Stylesheet::parse_with_base(css, &sheet_base));
                        }
                        pending.remove(index);
                    }
                }
            }
            _ => {}
        }
    }
    // `@font-face` rules become loadable faces before any style is computed: a
    // page that ships its own font must be measured with that font, not with a
    // substitute that is 10% wider.
    let mut web_fonts: Vec<text::WebFont> = Vec::new();
    for sheet in &sheets {
        for face in &sheet.fonts {
            let urls: Vec<String> = face
                .srcs
                .iter()
                .filter(|src| !src.starts_with("local("))
                .map(|src| resource::resolve(&sheet.base, src))
                .collect();
            // A family is often declared several times — one rule per file format
            // (eot, woff2, woff, ttf). They describe the same face, so the URLs are
            // merged in declaration order and the loader takes the first one it can
            // decode, which is exactly the browser's src-list rule.
            if let Some(existing) = web_fonts.iter_mut().find(|f| {
                f.family.eq_ignore_ascii_case(&face.family) && f.weight == face.weight
            }) {
                existing.urls.extend(urls);
                continue;
            }
            web_fonts.push(text::WebFont {
                family: face.family.clone(),
                urls,
                weight: face.weight,
                italic: face.italic,
            });
        }
    }
    fonts.set_web_fonts(web_fonts);

    let computed = compute_styles(&doc, &sheets, fonts, viewport.width, text::root_font_size());
    failures.retain(|(href, _)| !linked.contains_key(href));
    (doc, computed, failures)
}

/// Parse, style and lay out a document, loading its linked stylesheets through
/// `loader`. `base` is the page's own address, used for relative references.
pub fn render_with(
    html: &str,
    base: &str,
    viewport: Viewport,
    fonts: &mut text::FontContext,
    loader: &dyn resource::ResourceLoader,
) -> (Document, ComputedStyles, LayoutTree, Vec<(String, String)>) {
    let (doc, computed, failures) = style_document_with(html, base, viewport, fonts, loader);
    let tree = layout(&doc, &computed, fonts, viewport);
    (doc, computed, tree, failures)
}

/// Parse, style and lay out a document in one call.
pub fn render(html: &str, viewport: Viewport, fonts: &mut text::FontContext) -> (Document, ComputedStyles, LayoutTree) {
    let (doc, computed) = style_document(html, viewport, fonts);
    let tree = layout(&doc, &computed, fonts, viewport);
    (doc, computed, tree)
}

fn finite(v: f32) -> f32 {
    if v.is_finite() { v } else { 0.0 }
}

pub fn snapshot_json(
    doc: &Document,
    styles: &ComputedStyles,
    tree: &LayoutTree,
    viewport: Viewport,
    fonts: &mut text::FontContext,
    pretty: bool,
    elapsed_ms: u128,
) -> String {
    let (nl, ind) = if pretty { ("\n", "  ") } else { ("", "") };
    let mut out = String::new();
    out.push_str(&format!(
        "{{{nl}{ind}\"engine\": \"scout-render {}\",{nl}",
        env!("CARGO_PKG_VERSION")
    ));
    out.push_str(&format!(
        "{ind}\"viewport\": {{\"width\": {}, \"height\": {}}},{nl}",
        finite(viewport.width),
        finite(viewport.height)
    ));
    out.push_str(&format!(
        "{ind}\"document\": {{\"height\": {:.2}}},{nl}",
        finite(tree.document_height)
    ));
    // The engine's own clock, not the shell's: this is how long parse, style,
    // layout and (for a screenshot) paint actually took.
    out.push_str(&format!("{ind}\"elapsed_ms\": {},{nl}", elapsed_ms));
    out.push_str(&format!(
        "{ind}\"font\": \"{}\",{nl}",
        escape(fonts.source().unwrap_or("built-in fallback"))
    ));
    out.push_str(&format!("{ind}\"elements\": ["));
    let elements = doc.elements();
    let mut emitted = 0usize;
    for id in &elements {
        let Some(rect) = tree.rect(*id) else { continue };
        let node = &doc.nodes[*id];
        let style = styles.styles.get(id);
        let text = visible_text(doc, *id);
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        let display = style.map(|s| s.display).unwrap_or(Display::Block);
        if display == Display::None
            || !rect.width.is_finite()
            || !rect.height.is_finite()
            || rect.width <= 0.0
            || rect.height <= 0.0
        {
            continue;
        }
        if emitted > 0 {
            out.push_str(&format!(",{nl}"));
        }
        emitted += 1;
        out.push_str(&format!("{ind}{ind}{{"));
        out.push_str(&format!("\"path\": \"{}\",", escape(&doc.css_path(*id))));
        out.push_str(&format!("\"tag\": \"{}\",", escape(&node.name)));
        if let Some(id_attr) = node.attr("id") {
            out.push_str(&format!("\"id\": \"{}\",", escape(id_attr)));
        }
        if let Some(class) = node.attr("class") {
            out.push_str(&format!("\"class\": \"{}\",", escape(class)));
        }
        out.push_str(&format!(
            "\"rect\": {{\"x\": {:.2}, \"y\": {:.2}, \"width\": {:.2}, \"height\": {:.2}}},",
            finite(rect.x),
            finite(rect.y),
            finite(rect.width),
            finite(rect.height)
        ));
        if !text.is_empty() {
            out.push_str(&format!("\"text\": \"{}\",", escape(&truncate(&text, 120))));
        }
        out.push_str("\"styles\": {");
        if let Some(style) = style {
            for (i, (k, v)) in style_props(style).iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(&format!("\"{}\": \"{}\"", k, escape(v)));
            }
        }
        out.push('}');
        out.push('}');
    }
    out.push_str(&format!("{nl}{ind}],{nl}"));
    out.push_str(&format!(
        "{ind}\"unsupported\": [{}]{nl}}}{nl}",
        styles
            .unsupported
            .iter()
            .map(|u| format!("\"{}\"", escape(u)))
            .collect::<Vec<_>>()
            .join(", ")
    ));
    out
}

fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// Text an agent would see: markup and scripts inside a document do not count.
pub fn visible_text(doc: &Document, id: NodeId) -> String {
    let mut out = String::new();
    let mut stack = vec![id];
    while let Some(node) = stack.pop() {
        let n = &doc.nodes[node];
        match n.kind {
            Kind::Text => out.push_str(&n.text),
            Kind::Element => {
                if matches!(n.name.as_str(), "head" | "style" | "script" | "title" | "meta") {
                    continue;
                }
                for child in n.children.iter().rev() {
                    stack.push(*child);
                }
            }
            _ => {}
        }
    }
    out
}

/// First `limit` characters of `s`, never splitting a multi-byte character.
pub fn truncate(s: &str, limit: usize) -> &str {
    match s.char_indices().nth(limit) {
        Some((index, _)) => &s[..index],
        None => s,
    }
}

fn style_props(style: &Style) -> Vec<(&'static str, String)> {
    let px = |v: f32| format!("{:.2}", finite(v));
    let mut out = vec![
        (
            "display",
            match style.display {
                Display::None => "none",
                Display::Block => "block",
                Display::Inline => "inline",
                Display::InlineBlock => "inline-block",
                Display::Flex => "flex",
                Display::ListItem => "list-item",
                Display::Table => "table",
                Display::TableRowGroup => "table-row-group",
                Display::TableRow => "table-row",
                Display::TableCell => "table-cell",
            }
            .to_string(),
        ),
        ("color", style.color.to_css()),
        ("background-color", style.background_color.to_css()),
        ("font-family", style.font_family.clone()),
        ("font-size", px(style.font_size)),
        ("font-weight", style.font_weight.to_string()),
        ("line-height", px(style.line_height)),
        ("margin-top", px(style.margin.top.resolve(style.font_size, 16.0).unwrap_or(0.0))),
        ("margin-left", px(style.margin.left.resolve(style.font_size, 16.0).unwrap_or(0.0))),
        (
            "padding-top",
            px(style.padding.top.resolve(style.font_size, 16.0).unwrap_or(0.0)),
        ),
        (
            "padding-left",
            px(style.padding.left.resolve(style.font_size, 16.0).unwrap_or(0.0)),
        ),
        ("border-top-width", px(style.border_width.top)),
        ("border-left-width", px(style.border_width.left)),
    ];
    out.push(("visibility", (if style.visibility_visible { "visible" } else { "hidden" }).to_string()));
    out
}

