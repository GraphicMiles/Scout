//! The cascade, inherited values, and computed style.
//!
//! Order of operations, per element:
//!   1. inherit inherited properties from the parent's computed style
//!   2. collect matching declarations and sort by (important, inline, specificity, order)
//!   3. apply them in that order, later declarations winning
//!   4. resolve lengths: `em` against font size, `rem` against the root, `%` kept
//!
//! Percentages are kept unresolved because what they resolve against is a layout
//! question, not a style question.

use crate::css::selector::{matches, Selector};
use crate::css::value::{self, BackgroundImage, Color, Corners, Length, Shadow};
use crate::css::{Declaration, Stylesheet};
use crate::dom::{Document, NodeId};
use crate::text::FontContext;
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Display {
    None,
    Block,
    Inline,
    InlineBlock,
    Flex,
    ListItem,
    /// Table formatting: the four boxes an auto table needs. Borders are not
    /// collapsed and columns are solved the CSS 2.1 way (auto layout).
    Table,
    TableRowGroup,
    TableRow,
    TableCell,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Position {
    Static,
    Relative,
    Absolute,
    Fixed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextAlign {
    Left,
    Right,
    Center,
    Justify,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WhiteSpace {
    Normal,
    Nowrap,
    Pre,
    PreWrap,
    PreLine,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Float {
    None,
    Left,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Clear {
    None,
    Left,
    Right,
    Both,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoxSizing {
    ContentBox,
    BorderBox,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlexDirection {
    Row,
    RowReverse,
    Column,
    ColumnReverse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JustifyContent {
    FlexStart,
    Center,
    FlexEnd,
    SpaceBetween,
    SpaceAround,
    SpaceEvenly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlignItems {
    Stretch,
    FlexStart,
    Center,
    FlexEnd,
    Baseline,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Edges<T> {
    pub top: T,
    pub right: T,
    pub bottom: T,
    pub left: T,
}

impl<T: Copy> Edges<T> {
    pub fn all(v: T) -> Self {
        Edges { top: v, right: v, bottom: v, left: v }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LineHeight {
    /// Depends on the font; resolved to pixels at compute time.
    Normal,
    /// A unitless multiplier. It inherits as the number, not as the pixels, so a
    /// child with a different font size gets a different line box.
    Number(f32),
    Px(f32),
}

impl LineHeight {
    pub fn pixels(&self, normal: f32, font_size: f32) -> f32 {
        match self {
            LineHeight::Normal => normal,
            LineHeight::Number(n) => n * font_size,
            LineHeight::Px(v) => *v,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Style {
    pub display: Display,
    pub position: Position,
    pub visibility_visible: bool,

    pub font_family: String,
    pub font_size: f32,
    pub font_weight: u16,
    pub font_style_italic: bool,
    /// Pixel form, for layout. Recomputed once the font size is final.
    pub line_height: f32,
    /// What inherits: a keyword, a multiplier or a length.
    pub line_height_spec: LineHeight,
    pub letter_spacing: f32,
    pub text_align: TextAlign,
    pub text_transform_upper: bool,
    /// `text-decoration: underline`. Browsers paint it through inline descendants,
    /// so it is inherited here too, and an explicit `none` clears it.
    pub underline: bool,
    pub white_space: WhiteSpace,
    /// `overflow-wrap: break-word` / `word-break: break-all`: long words may be
    /// split between characters to avoid overflow.
    pub word_break: bool,
    pub color: Color,
    pub background_color: Color,

    pub margin: Edges<Length>,
    pub padding: Edges<Length>,
    pub border_width: Edges<f32>,
    pub border_color: Color,
    /// Corner radii, resolved to pixels once the font size is known.
    pub border_radius: Corners<f32>,
    pub shadow: Option<Shadow>,
    pub background_image: Option<BackgroundImage>,
    /// Offsets for positioned boxes; `auto` when the property is unset.
    pub top: Length,
    pub right: Length,
    pub bottom: Length,
    pub left: Length,
    pub z_index: i32,

    pub width: Length,
    pub height: Length,
    pub min_width: Length,
    pub max_width: Length,
    pub min_height: Length,
    pub max_height: Length,
    pub box_sizing: BoxSizing,
    pub overflow_hidden: bool,
    pub opacity: f32,

    pub flex_direction: FlexDirection,
    pub flex_wrap: bool,
    pub flex_grow: f32,
    pub flex_shrink: f32,
    pub flex_basis: Length,
    pub justify_content: JustifyContent,
    pub align_items: AlignItems,
    pub row_gap: f32,
    pub column_gap: f32,

    pub vertical_align: Length,
    /// `vertical-align: top` and friends. Inline-block boxes use it to sit in their
    /// line box; the numeric forms are not used yet.
    pub vertical_align_top: bool,
    pub float: Float,
    pub clear: Clear,

    /// Custom properties, inherited, used to resolve `var()`. Shared until an
    /// element declares one of its own: a page that puts hundreds of variables
    /// on `:root` would otherwise give every element on the page its own copy.
    pub custom: std::sync::Arc<HashMap<String, String>>,
}

impl Default for Style {
    fn default() -> Self {
        Style {
            display: Display::Inline,
            position: Position::Static,
            visibility_visible: true,
            // The initial `font-family` a browser uses here resolves to the
            // installed sans face, and measuring default-styled pages with it is
            // what keeps their line breaks in the same place as Chrome's.
            font_family: "DejaVu Sans".to_string(),
            font_size: 16.0,
            font_weight: 400,
            font_style_italic: false,
            line_height: 19.0,
            line_height_spec: LineHeight::Normal,
            letter_spacing: 0.0,
            text_align: TextAlign::Left,
            text_transform_upper: false,
            underline: false,
            white_space: WhiteSpace::Normal,
            word_break: false,
            color: Color::BLACK,
            background_color: Color::TRANSPARENT,
            margin: Edges::all(Length::Px(0.0)),
            padding: Edges::all(Length::Px(0.0)),
            border_width: Edges::all(0.0),
            border_color: Color::BLACK,
            border_radius: Corners::all(0.0),
            shadow: None,
            background_image: None,
            top: Length::Keyword,
            right: Length::Keyword,
            bottom: Length::Keyword,
            left: Length::Keyword,
            z_index: 0,
            width: Length::Keyword,
            height: Length::Keyword,
            min_width: Length::Px(0.0),
            max_width: Length::Keyword,
            min_height: Length::Px(0.0),
            max_height: Length::Keyword,
            box_sizing: BoxSizing::ContentBox,
            overflow_hidden: false,
            opacity: 1.0,
            flex_direction: FlexDirection::Row,
            flex_wrap: false,
            flex_grow: 0.0,
            flex_shrink: 1.0,
            flex_basis: Length::Keyword,
            justify_content: JustifyContent::FlexStart,
            align_items: AlignItems::Stretch,
            row_gap: 0.0,
            column_gap: 0.0,
            vertical_align: Length::Keyword,
            vertical_align_top: false,
            float: Float::None,
            clear: Clear::None,
            custom: std::sync::Arc::new(HashMap::new()),
        }
    }
}

impl Style {
    /// The initial style of an element whose parent's style is `parent`.
    fn inherit_from(parent: &Style) -> Style {
        let mut style = Style::default();
        // Inherited properties only.
        style.color = parent.color;
        style.font_family = parent.font_family.clone();
        style.font_size = parent.font_size;
        style.font_weight = parent.font_weight;
        style.font_style_italic = parent.font_style_italic;
        style.line_height_spec = parent.line_height_spec;
        style.line_height = parent.line_height;
        style.letter_spacing = parent.letter_spacing;
        style.text_align = parent.text_align;
        style.text_transform_upper = parent.text_transform_upper;
        style.underline = parent.underline;
        style.white_space = parent.white_space;
        style.visibility_visible = parent.visibility_visible;
        style.custom = parent.custom.clone();
        style
    }

    /// The shared map, cloned only when this element is the first to change it.
    fn custom_mut(&mut self) -> &mut HashMap<String, String> {
        std::sync::Arc::make_mut(&mut self.custom)
    }

    /// The user-agent defaults the engine applies before author styles.
    fn user_agent(tag: &str) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = Vec::new();
        let block = matches!(
            tag,
            "html" | "body" | "div" | "p" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "ul" | "ol"
                | "li" | "blockquote" | "pre" | "section" | "article" | "header" | "footer"
                | "main" | "nav" | "aside" | "form" | "figure" | "figcaption" | "hr" | "dl" | "dt"
                | "dd" | "address" | "fieldset"
        );
        if block {
            out.push(("display".into(), "block".into()));
        }
        match tag {
            // The page canvas: browsers give `body` an 8px margin, and a page that
            // relies on it looks cramped without it.
            "body" => {
                out.push(("margin-top".into(), "8px".into()));
                out.push(("margin-right".into(), "8px".into()));
                out.push(("margin-bottom".into(), "8px".into()));
                out.push(("margin-left".into(), "8px".into()));
            }
            "table" => out.push(("display".into(), "table".into())),
            "thead" | "tbody" | "tfoot" => out.push(("display".into(), "table-row-group".into())),
            "tr" => out.push(("display".into(), "table-row".into())),
            "td" | "th" => {
                out.push(("display".into(), "table-cell".into()));
                out.push(("padding".into(), "1px".into()));
                if tag == "th" {
                    out.push(("font-weight".into(), "bold".into()));
                }
            }
            // Chrome, with scripting on, does not lay `<noscript>` content out at
            // all — a page that ships a no-JS fallback would otherwise double in
            // height here.
            "noscript" => out.push(("display".into(), "none".into())),
            "center" => {
                out.push(("display".into(), "block".into()));
                out.push(("text-align".into(), "center".into()));
            }

            _ => {}
        }
        match tag {
            "p" => {
                out.push(("margin-top".into(), "1em".into()));
                out.push(("margin-bottom".into(), "1em".into()));
            }
            "h1" => {
                out.push(("font-size".into(), "2em".into()));
                out.push(("font-weight".into(), "bold".into()));
                out.push(("margin-top".into(), "0.67em".into()));
                out.push(("margin-bottom".into(), "0.67em".into()));
            }
            "h2" => {
                out.push(("font-size".into(), "1.5em".into()));
                out.push(("font-weight".into(), "bold".into()));
                out.push(("margin-top".into(), "0.83em".into()));
                out.push(("margin-bottom".into(), "0.83em".into()));
            }
            "h3" => {
                out.push(("font-size".into(), "1.17em".into()));
                out.push(("font-weight".into(), "bold".into()));
                out.push(("margin-top".into(), "1em".into()));
                out.push(("margin-bottom".into(), "1em".into()));
            }
            // An image is a replaced inline box: it sits in the line like a word.
            // So is an inline `<svg>`, which paints through the same bitmap path.
            "img" | "svg" => out.push(("display".into(), "inline-block".into())),
            "strong" | "b" => out.push(("font-weight".into(), "bold".into())),
            "em" | "i" => out.push(("font-style".into(), "italic".into())),
            "ul" | "ol" => {
                out.push(("margin-top".into(), "1em".into()));
                out.push(("margin-bottom".into(), "1em".into()));
                out.push(("padding-left".into(), "40px".into()));
            }
            "li" => {
                out.push(("display".into(), "list-item".into()));
            }
            "a" => {
                out.push(("color".into(), "rgb(0, 0, 238)".into()));
                out.push(("text-decoration".into(), "underline".into()));
            }
            "head" | "style" | "script" | "meta" | "title" | "link" | "base" => {
                out.push(("display".into(), "none".into()))
            }
            "br" => out.push(("display".into(), "inline".into())),
            _ => {}
        }
        out
    }
}

#[derive(Debug, Clone)]
pub struct ComputedStyles {
    pub styles: HashMap<NodeId, Style>,
    pub unsupported: Vec<String>,
    pub skipped_at_rules: Vec<String>,
}

/// One matched declaration, with its cascade priority.
#[derive(Debug, Clone)]
struct Candidate {
    important: bool,
    inline: bool,
    specificity: (u32, u32, u32),
    order: usize,
    property: String,
    value: String,
}

pub fn compute_styles(
    doc: &Document,
    sheets: &[Stylesheet],
    fonts: &mut FontContext,
    viewport_width: f32,
    root_font_size: f32,
) -> ComputedStyles {
    let mut out = ComputedStyles {
        styles: HashMap::new(),
        unsupported: Vec::new(),
        skipped_at_rules: Vec::new(),
    };
    for sheet in sheets {
        for u in &sheet.unsupported {
            out.unsupported
                .push(format!("{}: {}", u.selector, u.reason));
        }
        out.skipped_at_rules.extend(sheet.at_rules_skipped.clone());
    }

    let mut order = 0usize;
    let mut flat: Vec<(usize, &Selector, &Declaration, Option<&crate::css::MediaQuery>)> = Vec::new();
    for sheet in sheets {
        for rule in &sheet.rules {
            for selector in &rule.selectors {
                for declaration in &rule.declarations {
                    flat.push((order, selector, declaration, rule.media.as_ref()));
                }
                order += 1;
            }
        }
    }

    let mut styles: HashMap<NodeId, Style> = HashMap::new();
    walk(doc, doc.root, None, &flat, fonts, viewport_width, root_font_size, &mut styles, &mut out);

    out.styles = styles;
    out
}

#[allow(clippy::too_many_arguments)]
fn walk(
    doc: &Document,
    node: NodeId,
    parent: Option<NodeId>,
    flat: &[(usize, &Selector, &Declaration, Option<&crate::css::MediaQuery>)],
    fonts: &mut FontContext,
    viewport_width: f32,
    root_font_size: f32,
    out: &mut HashMap<NodeId, Style>,
    report: &mut ComputedStyles,
) {
    let el = &doc.nodes[node];
    if el.kind != crate::dom::Kind::Element {
        // The document node (and stray text/comments at the top) only route the walk
        // down into real elements.
        for child in el.children.clone() {
            walk(doc, child, parent, flat, fonts, viewport_width, root_font_size, out, report);
        }
        return;
    }
    let parent_style = parent.and_then(|p| out.get(&p).cloned());
    let mut style = match &parent_style {
        Some(p) => Style::inherit_from(p),
        None => Style::default(),
    };

    // 1. User-agent defaults.
    for (property, value) in Style::user_agent(&el.name) {
        apply(&mut style, &property, &value, fonts, root_font_size, 0.0);
    }

    // 1b. Presentational attributes: `width`/`height` on an image are CSS
    // declarations in all but name, and pages lean on them to size images before
    // the bytes arrive.
    for (attribute, property) in [("width", "width"), ("height", "height")] {
        if let Some(value) = el.attr(attribute) {
            let value = value.trim();
            let number = value.trim_end_matches("px");
            if let Ok(numeric) = number.parse::<f32>() {
                apply(&mut style, property, &format!("{}px", numeric), fonts, root_font_size, 0.0);
            } else if let Some(percent) = value.strip_suffix('%') {
                if percent.trim().parse::<f32>().is_ok() {
                    apply(
                        &mut style,
                        property,
                        &format!("{}%", percent.trim()),
                        fonts,
                        root_font_size,
                        0.0,
                    );
                }
            }
        }
    }
    // `nowrap` on a cell is the attribute spelling of `white-space: nowrap`.
    if el.attr("nowrap").is_some() {
        apply(&mut style, "white-space", "nowrap", fonts, root_font_size, 0.0);
    }
    // `align="center"` centres a table by auto margins and centres a cell's text.
    if let Some(align) = el.attr("align") {
        let align = align.trim().to_ascii_lowercase();
        match (el.name.as_str(), align.as_str()) {
            ("table", "center") => {
                apply(&mut style, "margin-left", "auto", fonts, root_font_size, 0.0);
                apply(&mut style, "margin-right", "auto", fonts, root_font_size, 0.0);
            }
            ("td" | "th" | "tr", "left" | "center" | "right" | "justify") => {
                apply(
                    &mut style,
                    "text-align",
                    &align,
                    fonts,
                    root_font_size,
                    0.0,
                );
            }
            _ => {}
        }
    }

    // `<font size="2" color=... face=...>` predates CSS and is still all over the
    // web's back catalogue. Size 1–7 maps onto the browser's fixed scale.
    if el.name == "font" {
        const SIZES: [f32; 7] = [10.0, 13.0, 16.0, 18.0, 24.0, 32.0, 48.0];
        if let Some(size) = el.attr("size") {
            let size = size.trim();
            let parsed = if let Some(relative) = size.strip_prefix('+') {
                relative.parse::<usize>().ok().map(|n| 3 + n)
            } else if let Some(relative) = size.strip_prefix('-') {
                relative.parse::<usize>().ok().map(|n| 3usize.saturating_sub(n))
            } else {
                size.parse::<usize>().ok()
            };
            if let Some(index) = parsed.filter(|i| (1..=7).contains(i)) {
                apply(
                    &mut style,
                    "font-size",
                    &format!("{}px", SIZES[index - 1]),
                    fonts,
                    root_font_size,
                    0.0,
                );
            }
        }
        if let Some(colour) = el.attr("color") {
            apply(&mut style, "color", colour, fonts, root_font_size, 0.0);
        }
        if let Some(face) = el.attr("face") {
            apply(&mut style, "font-family", face, fonts, root_font_size, 0.0);
        }
    }
    // Legacy `<center>` centres a table's box by margins, the way browsers do;
    // `text-align` alone would only centre its inline content.
    if el.name == "table"
        && doc.nodes[node]
            .parent
            .map(|p| doc.nodes[p].name == "center")
            .unwrap_or(false)
    {
        apply(&mut style, "margin-left", "auto", fonts, root_font_size, 0.0);
        apply(&mut style, "margin-right", "auto", fonts, root_font_size, 0.0);
    }

    // A table's `border`, `cellpadding` and `cellspacing` attributes are inherited
    // by its cells in every browser. Resolve them against the nearest `<table>`
    // ancestor rather than pretending they are not there.
    if matches!(el.name.as_str(), "td" | "th" | "tr" | "tbody" | "thead" | "tfoot") {
        if let Some(table) = nearest_table(doc, node) {
            let attributes = &doc.nodes[table];
            if attributes.attr("border").map(|b| b.trim() != "0").unwrap_or(false) {
                for side in ["top", "right", "bottom", "left"] {
                    apply(
                        &mut style,
                        &format!("border-{}-width", side),
                        "1px",
                        fonts,
                        root_font_size,
                        0.0,
                    );
                }
            }
            if let Some(padding) = attributes.attr("cellpadding") {
                if padding.trim().parse::<f32>().is_ok() {
                    apply(
                        &mut style,
                        "padding",
                        &format!("{}px", padding.trim()),
                        fonts,
                        root_font_size,
                        0.0,
                    );
                }
            }
        }
    }

    // `bgcolor` is the 1996 spelling of `background-color`.
    if let Some(background) = el.attr("bgcolor") {
        apply(&mut style, "background-color", background, fonts, root_font_size, 0.0);
    }

    // 2. Author declarations, in cascade order.
    let mut candidates: Vec<Candidate> = Vec::new();
    for (order, selector, declaration, media) in flat {
        if let Some(media) = media {
            if !media.matches(viewport_width) {
                continue;
            }
        }
        if !matches(doc, node, selector) {
            continue;
        }
        candidates.push(Candidate {
            important: declaration.important,
            inline: false,
            specificity: selector.specificity,
            order: *order,
            property: declaration.property.clone(),
            value: declaration.value.clone(),
        });
    }
    if let Some(inline) = el.attr("style") {
        for declaration in crate::css::parse_declarations(inline) {
            candidates.push(Candidate {
                important: declaration.important,
                inline: true,
                specificity: (0, 0, 0),
                order: usize::MAX,
                property: declaration.property,
                value: declaration.value,
            });
        }
    }
    candidates.sort_by(|a, b| {
        a.important
            .cmp(&b.important)
            .then(a.inline.cmp(&b.inline))
            .then(a.specificity.cmp(&b.specificity))
            .then(a.order.cmp(&b.order))
    });

    for candidate in &candidates {
        apply(
            &mut style,
            &candidate.property,
            &candidate.value,
            fonts,
            root_font_size,
            1.0,
        );
    }

    // 3. Resolve the parts of computed style that depend on the resolved font.
    let normal_line_height = fonts
        .metrics(&style.font_family, style.font_weight, style.font_size)
        .normal_line_height();
    style.line_height = style
        .line_height_spec
        .pixels(normal_line_height, style.font_size);

    if el.name == "html" {
        let root_size = style.font_size.to_string();
        style.custom_mut().insert("__root_font_size".into(), root_size);
    }

    out.insert(node, style.clone());
    for child in el.children.clone() {
        walk(doc, child, Some(node), flat, fonts, viewport_width, root_font_size, out, report);
    }
}

/// Apply one declaration to a style. Handles shorthands, `var()`, inheritance
/// keywords and unit conversion. Unknown properties are ignored, which is what a
/// browser does too.
fn apply(
    style: &mut Style,
    property: &str,
    raw_value: &str,
    fonts: &mut FontContext,
    root_font_size: f32,
    parent_font_hint: f32,
) {
    let value = resolve_vars(raw_value, style.custom.as_ref());
    let property = property.trim().to_ascii_lowercase();
    let value = value.trim();

    if property.starts_with("--") {
        style.custom_mut().insert(property.clone(), value.to_string());
        return;
    }

    let font_size = if style.font_size > 0.0 { style.font_size } else { 16.0 };
    let px = |v: &str| length_px(v, font_size, root_font_size);

    match property.as_str() {
        "display" => {
            style.display = match value.to_ascii_lowercase().as_str() {
                "none" => Display::None,
                "block" => Display::Block,
                "inline" => Display::Inline,
                "inline-block" => Display::InlineBlock,
                "flex" => Display::Flex,
                "list-item" => Display::ListItem,
                "table" => Display::Table,
                "table-row-group" | "table-header-group" | "table-footer-group" => {
                    Display::TableRowGroup
                }
                "table-row" => Display::TableRow,
                "table-cell" => Display::TableCell,
                // `inline-table` is a table as far as layout is concerned; the
                // inline wrapping is not modelled.
                "inline-table" => Display::Table,
                "inline-flex" => Display::InlineBlock,
                other => {
                    let _ = other;
                    style.display
                }
            }
        }
        "position" => {
            style.position = match value.to_ascii_lowercase().as_str() {
                "relative" => Position::Relative,
                "absolute" => Position::Absolute,
                "fixed" => Position::Fixed,
                _ => Position::Static,
            }
        }
        "visibility" => {
            style.visibility_visible = !value.eq_ignore_ascii_case("hidden");
        }
        "font-family" => style.font_family = value.to_string(),
        "font-size" => {
            let parsed = value::parse_length(value);
            let resolved = match parsed {
                Length::Px(v) => v,
                Length::Em(v) => v * if parent_font_hint > 0.0 { style.font_size } else { 16.0 },
                Length::Rem(v) => v * root_font_size,
                Length::Percent(v) => v / 100.0 * style.font_size,
                _ => match value.to_ascii_lowercase().as_str() {
                    "small" => 13.0,
                    "medium" => 16.0,
                    "large" => 18.0,
                    "x-large" => 24.0,
                    "smaller" => style.font_size * 0.83,
                    "larger" => style.font_size * 1.2,
                    _ => style.font_size,
                },
            };
            style.font_size = resolved.max(0.0);
        }
        "font-weight" => {
            style.font_weight = match value.to_ascii_lowercase().as_str() {
                "bold" | "bolder" => 700,
                "normal" | "lighter" => 400,
                other => other.parse::<u16>().unwrap_or(style.font_weight),
            }
        }
        "font-style" => style.font_style_italic = value.eq_ignore_ascii_case("italic"),
        "line-height" => {
            let lower = value.trim().to_ascii_lowercase();
            style.line_height_spec = match lower.as_str() {
                "normal" | "inherit" | "initial" => LineHeight::Normal,
                _ => {
                    let is_length = lower.ends_with("px")
                        || lower.ends_with("em")
                        || lower.ends_with("rem")
                        || lower.ends_with('%');
                    if is_length {
                        match value::parse_length(&lower) {
                            Length::Px(v) => LineHeight::Px(v),
                            Length::Em(v) => LineHeight::Px(v * style.font_size),
                            Length::Rem(v) => LineHeight::Px(v * root_font_size),
                            Length::Percent(v) => LineHeight::Number(v / 100.0),
                            _ => LineHeight::Normal,
                        }
                    } else {
                        value::parse_number(&lower)
                            .map(LineHeight::Number)
                            .unwrap_or(LineHeight::Normal)
                    }
                }
            };
        }
        "letter-spacing" => {
            style.letter_spacing = px(value).unwrap_or(0.0);
        }
        "text-decoration" | "text-decoration-line" => {
            let lower = value.trim().to_ascii_lowercase();
            if lower.split_whitespace().any(|w| w == "underline") {
                style.underline = true;
            } else if lower.split_whitespace().any(|w| w == "none") {
                style.underline = false;
            }
        }
        "text-align" => {
            style.text_align = match value.to_ascii_lowercase().as_str() {
                "right" => TextAlign::Right,
                "center" => TextAlign::Center,
                "justify" => TextAlign::Justify,
                _ => TextAlign::Left,
            }
        }
        "text-transform" => {
            style.text_transform_upper = value.eq_ignore_ascii_case("uppercase");
        }
        "word-break" | "overflow-wrap" | "word-wrap" => {
            style.word_break = matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "break-all" | "break-word" | "anywhere"
            );
        }
        "white-space" => {
            style.white_space = match value.to_ascii_lowercase().as_str() {
                "nowrap" => WhiteSpace::Nowrap,
                "pre" => WhiteSpace::Pre,
                "pre-wrap" => WhiteSpace::PreWrap,
                "pre-line" => WhiteSpace::PreLine,
                _ => WhiteSpace::Normal,
            }
        }
        "color" => {
            if let Some(c) = value::parse_color(value) {
                style.color = c;
            }
        }
        "background-color" => {
            if let Some(c) = value::parse_color(value) {
                style.background_color = c;
            } else if let Some(c) = background_color_from_shorthand(value) {
                style.background_color = c;
            }
        }
        "background" => {
            if let Some(c) = background_color_from_shorthand(value) {
                style.background_color = c;
            }
        }
        "margin" => {
            let parts = value::split_parts(value);
            if let Some(boxed) = value::expand_box(&parts) {
                style.margin.top = value::parse_length(&boxed[0]);
                style.margin.right = value::parse_length(&boxed[1]);
                style.margin.bottom = value::parse_length(&boxed[2]);
                style.margin.left = value::parse_length(&boxed[3]);
            }
        }
        "margin-top" => style.margin.top = value::parse_length(value),
        "margin-right" => style.margin.right = value::parse_length(value),
        "margin-bottom" => style.margin.bottom = value::parse_length(value),
        "margin-left" => style.margin.left = value::parse_length(value),
        // Logical properties, resolved for the horizontal-tb, left-to-right
        // writing modes that every page in the corpus uses.
        "margin-block" | "margin-inline" => {
            let parts = value::split_parts(value);
            if let Some(boxed) = value::expand_box(&parts) {
                let (start, end) = (value::parse_length(&boxed[0]), value::parse_length(&boxed[1]));
                if property == "margin-inline" {
                    style.margin.left = start;
                    style.margin.right = end;
                } else {
                    style.margin.top = start;
                    style.margin.bottom = end;
                }
            }
        }
        "margin-inline-start" | "margin-inline-end" | "padding-inline-start"
        | "padding-inline-end" | "margin-block-start" | "margin-block-end"
        | "padding-block-start" | "padding-block-end" => {
            let length = value::parse_length(value);
            let (edge, start) = if property.contains("inline") {
                (if property.ends_with("-start") { "left" } else { "right" }, true)
            } else {
                (if property.ends_with("-start") { "top" } else { "bottom" }, true)
            };
            let _ = start;
            let target = if property.starts_with("margin") { &mut style.margin } else { &mut style.padding };
            match edge {
                "left" => target.left = length,
                "right" => target.right = length,
                "top" => target.top = length,
                _ => target.bottom = length,
            }
        }
        "inset" => {
            let parts = value::split_parts(value);
            if let Some(boxed) = value::expand_box(&parts) {
                style.top = value::parse_length(&boxed[0]);
                style.right = value::parse_length(&boxed[1]);
                style.bottom = value::parse_length(&boxed[2]);
                style.left = value::parse_length(&boxed[3]);
            }
        }
        "inset-inline" => {
            let parts = value::split_parts(value);
            if let Some(boxed) = value::expand_box(&parts) {
                style.left = value::parse_length(&boxed[0]);
                style.right = value::parse_length(&boxed[1]);
            }
        }
        "inset-block" => {
            let parts = value::split_parts(value);
            if let Some(boxed) = value::expand_box(&parts) {
                style.top = value::parse_length(&boxed[0]);
                style.bottom = value::parse_length(&boxed[1]);
            }
        }
        "inset-inline-start" => style.left = value::parse_length(value),
        "inset-inline-end" => style.right = value::parse_length(value),
        "inset-block-start" => style.top = value::parse_length(value),
        "inset-block-end" => style.bottom = value::parse_length(value),
        "padding" => {
            let parts = value::split_parts(value);
            if let Some(boxed) = value::expand_box(&parts) {
                style.padding.top = value::parse_length(&boxed[0]);
                style.padding.right = value::parse_length(&boxed[1]);
                style.padding.bottom = value::parse_length(&boxed[2]);
                style.padding.left = value::parse_length(&boxed[3]);
            }
        }
        "padding-top" => style.padding.top = value::parse_length(value),
        "padding-right" => style.padding.right = value::parse_length(value),
        "padding-bottom" => style.padding.bottom = value::parse_length(value),
        "padding-left" => style.padding.left = value::parse_length(value),
        "border" | "border-style" => {
            let width = border_width_from_shorthand(value).unwrap_or(0.0);
            // `border: solid` alone has no width; only a length sets one.
            if width > 0.0 {
                style.border_width = Edges::all(width);
            }
            if let Some(c) = first_colour(value) {
                style.border_color = c;
            }
        }
        "border-radius" => {
            let radii = value::parse_radii(value);
            let resolve = |len: Length| -> f32 {
                match len {
                    Length::Px(v) => v,
                    Length::Em(v) => v * style.font_size,
                    Length::Rem(v) => v * root_font_size,
                    Length::Percent(v) => v,
                    _ => 0.0,
                }
            };
            style.border_radius = Corners {
                top_left: resolve(radii.top_left),
                top_right: resolve(radii.top_right),
                bottom_right: resolve(radii.bottom_right),
                bottom_left: resolve(radii.bottom_left),
            };
        }
        "box-shadow" => style.shadow = value::parse_shadow(value),
        "background-image" => style.background_image = BackgroundImage::parse(value),
        "top" => style.top = value::parse_length(value),
        "right" => style.right = value::parse_length(value),
        "bottom" => style.bottom = value::parse_length(value),
        "left" => style.left = value::parse_length(value),
        "z-index" => {
            style.z_index = value.trim().parse::<i32>().unwrap_or(0);
        }
        "border-color" => {
            if let Some(c) = first_colour(value) {
                style.border_color = c;
            }
        }
        "border-width" => {
            // `border-width` takes one to four lengths, like every box shorthand.
            let parts = value::split_parts(value);
            match value::expand_box(&parts) {
                Some([t, r, b, l]) => {
                    style.border_width = Edges {
                        top: px(&t).unwrap_or(0.0),
                        right: px(&r).unwrap_or(0.0),
                        bottom: px(&b).unwrap_or(0.0),
                        left: px(&l).unwrap_or(0.0),
                    }
                }
                None => style.border_width = Edges::all(border_width_from_shorthand(value).unwrap_or(0.0)),
            }
        }
        "border-top" | "border-right" | "border-bottom" | "border-left" => {
            let width = border_width_from_shorthand(value).unwrap_or(0.0);
            match property.as_str() {
                "border-top" => style.border_width.top = width,
                "border-right" => style.border_width.right = width,
                "border-bottom" => style.border_width.bottom = width,
                "border-left" => style.border_width.left = width,
                _ => {}
            }
        }
        "border-top-width" => style.border_width.top = px(value).unwrap_or(0.0),
        "border-right-width" => style.border_width.right = px(value).unwrap_or(0.0),
        "border-bottom-width" => style.border_width.bottom = px(value).unwrap_or(0.0),
        "border-left-width" => style.border_width.left = px(value).unwrap_or(0.0),
        "width" => style.width = value::parse_length(value),
        "height" => style.height = value::parse_length(value),
        "min-width" => style.min_width = value::parse_length(value),
        "max-width" => style.max_width = value::parse_length(value),
        "min-height" => style.min_height = value::parse_length(value),
        "max-height" => style.max_height = value::parse_length(value),
        "box-sizing" => {
            style.box_sizing = if value.eq_ignore_ascii_case("border-box") {
                BoxSizing::BorderBox
            } else {
                BoxSizing::ContentBox
            }
        }
        "overflow" => style.overflow_hidden = value.to_ascii_lowercase().contains("hidden"),
        "opacity" => style.opacity = value::parse_number(value).unwrap_or(1.0),
        "flex" => {
            let parts = value::split_parts(value);
            if parts.len() == 1 && parts[0].eq_ignore_ascii_case("none") {
                style.flex_grow = 0.0;
                style.flex_shrink = 0.0;
                style.flex_basis = Length::Keyword;
            } else if parts.len() == 1 && parts[0].eq_ignore_ascii_case("auto") {
                style.flex_grow = 1.0;
                style.flex_shrink = 1.0;
                style.flex_basis = Length::Keyword;
            } else {
                let mut nums: Vec<f32> = Vec::new();
                for part in &parts {
                    if let Ok(n) = part.parse::<f32>() {
                        nums.push(n);
                    } else {
                        style.flex_basis = value::parse_length(part);
                    }
                }
                if let Some(g) = nums.first() {
                    style.flex_grow = *g;
                }
                if let Some(s) = nums.get(1) {
                    style.flex_shrink = *s;
                }
                if !nums.is_empty() && !parts.iter().any(|p| !p.trim().parse::<f32>().is_ok()) {
                    // `flex: 1` and `flex: 1 2` both imply a zero basis, which is what
                    // makes two `flex: 1` items end up the same width.
                    style.flex_basis = Length::Percent(0.0);
                }
            }
        }
        "flex-grow" => style.flex_grow = value::parse_number(value).unwrap_or(0.0),
        "flex-shrink" => style.flex_shrink = value::parse_number(value).unwrap_or(1.0),
        "flex-basis" => style.flex_basis = value::parse_length(value),
        "flex-direction" => {
            style.flex_direction = match value.to_ascii_lowercase().as_str() {
                "row-reverse" => FlexDirection::RowReverse,
                "column" => FlexDirection::Column,
                "column-reverse" => FlexDirection::ColumnReverse,
                _ => FlexDirection::Row,
            }
        }
        "flex-wrap" => style.flex_wrap = value.to_ascii_lowercase().contains("wrap"),
        "justify-content" => {
            style.justify_content = match value.to_ascii_lowercase().as_str() {
                "center" => JustifyContent::Center,
                "flex-end" | "end" => JustifyContent::FlexEnd,
                "space-between" => JustifyContent::SpaceBetween,
                "space-around" => JustifyContent::SpaceAround,
                "space-evenly" => JustifyContent::SpaceEvenly,
                _ => JustifyContent::FlexStart,
            }
        }
        "align-items" => {
            style.align_items = match value.to_ascii_lowercase().as_str() {
                "center" => AlignItems::Center,
                "flex-start" | "start" => AlignItems::FlexStart,
                "flex-end" | "end" => AlignItems::FlexEnd,
                "baseline" => AlignItems::Baseline,
                _ => AlignItems::Stretch,
            }
        }
        "gap" | "grid-gap" => {
            let parts = value::split_parts(value);
            let first = parts.first().map(|p| px(p).unwrap_or(0.0)).unwrap_or(0.0);
            let second = parts.get(1).map(|p| px(p).unwrap_or(0.0)).unwrap_or(first);
            style.row_gap = first;
            style.column_gap = second;
        }
        "row-gap" => style.row_gap = px(value).unwrap_or(0.0),
        "column-gap" => style.column_gap = px(value).unwrap_or(0.0),
        "vertical-align" => {
            let lower = value.trim().to_ascii_lowercase();
            style.vertical_align_top = lower == "top" || lower == "text-top";
            style.vertical_align = value::parse_length(value);
        }
        "float" => {
            style.float = match value.trim().to_ascii_lowercase().as_str() {
                "left" => Float::Left,
                "right" => Float::Right,
                _ => Float::None,
            }
        }
        "clear" => {
            style.clear = match value.trim().to_ascii_lowercase().as_str() {
                "left" => Clear::Left,
                "right" => Clear::Right,
                "both" => Clear::Both,
                _ => Clear::None,
            }
        }
        "font" => {
            apply_font_shorthand(style, value, root_font_size, fonts);
        }
        _ => {}
    }
}

fn apply_font_shorthand(style: &mut Style, value: &str, root_font_size: f32, fonts: &mut FontContext) {
    // `font: [style] [weight] size[/line-height] family`
    let parts = value::split_parts(value);
    let mut size_index = None;
    for (i, part) in parts.iter().enumerate() {
        let lower = part.to_ascii_lowercase();
        // The size may arrive glued to the line height: `16px/1.4`.
        let head = lower.split('/').next().unwrap_or("").to_string();
        let is_size = matches!(
            head.as_str(),
            "small" | "medium" | "large" | "x-large" | "smaller" | "larger"
        ) || head.ends_with("px")
            || head.ends_with("em")
            || head.ends_with("rem")
            || head.ends_with('%');
        if is_size {
            size_index = Some(i);
            break;
        }
        if lower == "bold" || lower == "bolder" {
            style.font_weight = 700;
        } else if lower == "normal" {
            // could be style, weight or variant; an explicit size follows
        } else if lower == "italic" {
            style.font_style_italic = true;
        }
    }
    let Some(index) = size_index else {
        return;
    };
    let size_part = &parts[index];
    let (size_text, line_text) = match size_part.split_once('/') {
        Some((s, l)) => (s.to_string(), Some(l.to_string())),
        None => (size_part.clone(), None),
    };
    let size = match value::parse_length(&size_text) {
        Length::Px(v) => v,
        Length::Em(v) => v * style.font_size,
        Length::Rem(v) => v * root_font_size,
        Length::Percent(v) => v / 100.0 * style.font_size,
        _ => style.font_size,
    };
    style.font_size = size;
    style.line_height_spec = match line_text {
        // `16px/1.4` — the multiplier applies to the size just parsed, not the parent's.
        Some(line) => match value::parse_number(&line) {
            Some(n) if !line.ends_with("px") => LineHeight::Number(n),
            _ => value::parse_px(&line).map(LineHeight::Px).unwrap_or(LineHeight::Normal),
        },
        None => LineHeight::Normal,
    };
    let family = parts[index + 1..].join(" ");
    if !family.is_empty() {
        style.font_family = family;
    }
    let _ = fonts;
}

fn length_px(value: &str, font_size: f32, root_font_size: f32) -> Option<f32> {
    match value::parse_length(value) {
        Length::Px(v) => Some(v),
        Length::Em(v) => Some(v * font_size),
        Length::Rem(v) => Some(v * root_font_size),
        Length::Percent(v) => Some(v / 100.0 * font_size),
        _ => None,
    }
}

/// The width from `border: 1px solid black` / `border-width: 2px`.
fn border_width_from_shorthand(value: &str) -> Option<f32> {
    for part in value::split_parts(value) {
        if let Some(px) = value::parse_px(&part) {
            return Some(px);
        }
        let lower = part.to_ascii_lowercase();
        if lower == "thin" {
            return Some(1.0);
        }
        if lower == "medium" {
            return Some(3.0);
        }
        if lower == "thick" {
            return Some(5.0);
        }
    }
    Some(0.0)
}

fn background_color_from_shorthand(value: &str) -> Option<Color> {
    for part in value::split_parts(value) {
        if let Some(color) = value::parse_color(&part) {
            return Some(color);
        }
    }
    None
}

/// Substitute `var(--name)` with the cascade's custom property values.
fn resolve_vars(value: &str, custom: &HashMap<String, String>) -> String {
    if !value.contains("var(") {
        return value.to_string();
    }
    let mut out = String::new();
    let mut rest = value;
    while let Some(pos) = rest.find("var(") {
        out.push_str(&rest[..pos]);
        let after = &rest[pos + 4..];
        let close = match after.find(')') {
            Some(c) => c,
            None => {
                out.push_str(&rest[pos..]);
                return out;
            }
        };
        let inner = &after[..close];
        let (name, fallback) = match inner.split_once(',') {
            Some((n, f)) => (n.trim(), Some(f.trim().to_string())),
            None => (inner.trim(), None),
        };
        match custom.get(name) {
            Some(v) => out.push_str(v),
            None => {
                if let Some(f) = fallback {
                    out.push_str(&f);
                }
            }
        }
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn var_substitution_uses_fallback() {
        let mut custom = HashMap::new();
        custom.insert("--a".to_string(), "red".to_string());
        assert_eq!(resolve_vars("var(--a)", &custom), "red");
        assert_eq!(resolve_vars("var(--missing, blue)", &custom), "blue");
        assert_eq!(resolve_vars("1px solid var(--a)", &custom), "1px solid red");
    }

    #[test]
    fn inheritance_copies_only_inherited_properties() {
        let parent = Style {
            color: Color::rgb(1, 2, 3),
            font_size: 20.0,
            margin: Edges::all(Length::Px(5.0)),
            ..Style::default()
        };
        let child = Style::inherit_from(&parent);
        assert_eq!(child.color, Color::rgb(1, 2, 3));
        assert_eq!(child.font_size, 20.0);
        assert_eq!(child.margin.top, Length::Px(0.0));
    }

    #[test]
    fn user_agent_defaults_make_paragraphs_block() {
        let decls = Style::user_agent("p");
        assert!(decls.iter().any(|(p, v)| p == "display" && v == "block"));
        assert!(decls.iter().any(|(p, _)| p == "margin-top"));
    }
}

/// The first colour in a shorthand like `3px solid #333`.
fn first_colour(value: &str) -> Option<Color> {
    for part in value::split_parts(value) {
        if let Some(c) = value::parse_color(&part) {
            return Some(c);
        }
    }
    None
}

/// The nearest `<table>` above `node`, used for the attributes a table hands down
/// to its cells.
fn nearest_table(doc: &Document, node: NodeId) -> Option<NodeId> {
    let mut current = doc.nodes[node].parent;
    while let Some(id) = current {
        if doc.nodes[id].name == "table" {
            return Some(id);
        }
        current = doc.nodes[id].parent;
    }
    None
}
