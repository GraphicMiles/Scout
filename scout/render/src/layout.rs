//! Block and inline layout: HTML + computed styles → geometry.
//!
//! The normal flow here is the CSS 2.1 model restricted to what Scout needs:
//! block boxes stack vertically, inline content is split into lines greedily, and
//! vertical margins collapse between siblings and through a parent's edges.
//! Everything is in CSS pixels with the origin at the top-left of the viewport.
//!
//! Flex containers and floats are separate passes in `flex.rs` and `float.rs`; this
//! module owns the geometry of a document's normal flow.

use std::collections::HashMap;

use crate::css::value::{Color, Length};
use crate::dom::{Document, Kind, NodeId};
use crate::style::{
    BoxSizing, Clear, ComputedStyles, Display, Edges, FlexDirection, Float, Position, Style, TextAlign,
    WhiteSpace,
};
use crate::text::FontContext;
use crate::Viewport;

/// A rectangle in CSS pixels, relative to the viewport origin.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Rect {
    pub fn new(x: f32, y: f32, width: f32, height: f32) -> Rect {
        Rect { x, y, width, height }
    }

    pub fn right(&self) -> f32 {
        self.x + self.width
    }

    pub fn bottom(&self) -> f32 {
        self.y + self.height
    }

    pub fn union(&self, other: &Rect) -> Rect {
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        Rect {
            x,
            y,
            width: self.right().max(other.right()) - x,
            height: self.bottom().max(other.bottom()) - y,
        }
    }
}

/// One positioned piece of text. Paint turns these into glyphs.
#[derive(Debug, Clone)]
pub struct TextRun {
    pub node: NodeId,
    pub text: String,
    pub rect: Rect,
    pub color: Color,
    pub font_size: f32,
    pub font_weight: u16,
    pub italic: bool,
    pub family: String,
    pub underline: bool,
}

/// A line box, recorded so the text stage can be gated on line breaks.
#[derive(Debug, Clone, Copy)]
pub struct Line {
    pub owner: NodeId,
    pub rect: Rect,
}

#[derive(Debug, Default)]
pub struct LayoutTree {
    /// Border boxes, keyed by node.
    pub rects: HashMap<NodeId, Rect>,
    /// Text fragments in paint order.
    pub text: Vec<TextRun>,
    /// Line boxes in paint order.
    pub lines: Vec<Line>,
    /// The height the document occupies, for sizing a screenshot.
    pub document_height: f32,
    /// The width the document occupies: a body wider than the viewport wins.
    pub document_width: f32,
}

impl LayoutTree {
    pub fn rect(&self, id: NodeId) -> Option<Rect> {
        self.rects.get(&id).copied()
    }
}

// ---------------------------------------------------------------- lengths

// The viewport the current layout is being solved in. `vw`/`vh` need it, and
// threading a viewport through every length resolution would touch every call
// site for one rarely-used unit family.
thread_local! {
    /// The viewport the current layout is being solved in. Kept per-thread so
    /// concurrent server renders with different viewports never race on `vw`/`vh`.
    static VIEWPORT: std::cell::Cell<(f32, f32)> = const { std::cell::Cell::new((800.0, 600.0)) };
}

fn viewport_size() -> (f32, f32) {
    VIEWPORT.with(|v| v.get())
}

/// Set the viewport for viewport-relative units. Called once per layout.
pub fn set_viewport(width: f32, height: f32) {
    VIEWPORT.with(|v| v.set((width, height)));
}

fn width_key(cb_width: f32) -> u32 {
    if !cb_width.is_finite() || cb_width >= 1_000_000.0 {
        u32::MAX
    } else if cb_width <= 0.0 {
        0
    } else {
        (cb_width * 4.0).round() as u32
    }
}

pub fn resolve_len(len: Length, font_size: f32, root: f32, percent_base: f32) -> Option<f32> {
    match len {
        Length::Px(v) => Some(v),
        Length::Em(v) => Some(v * font_size),
        Length::Rem(v) => Some(v * root),
        Length::Percent(v) => Some(v / 100.0 * percent_base),
        Length::Ch(v) => Some(v * crate::text::ch_ratio() * font_size),
        Length::Vw(v) => Some(v / 100.0 * viewport_size().0),
        Length::Vh(v) => Some(v / 100.0 * viewport_size().1),
        Length::VMin(v) => {
            let (w, h) = viewport_size();
            Some(v / 100.0 * w.min(h))
        }
        Length::VMax(v) => {
            let (w, h) = viewport_size();
            Some(v / 100.0 * w.max(h))
        }
        _ => None,
    }
}

pub fn resolve_edges(e: Edges<Length>, font_size: f32, root: f32, percent_base: f32) -> Edges<f32> {
    Edges {
        top: resolve_len(e.top, font_size, root, percent_base).unwrap_or(0.0),
        right: resolve_len(e.right, font_size, root, percent_base).unwrap_or(0.0),
        bottom: resolve_len(e.bottom, font_size, root, percent_base).unwrap_or(0.0),
        left: resolve_len(e.left, font_size, root, percent_base).unwrap_or(0.0),
    }
}

fn is_auto(len: Length) -> bool {
    matches!(len, Length::Keyword | Length::Invalid)
}

/// The result of laying out one block box.
pub(crate) struct Block {
    pub(crate) rect: Rect,
    pub(crate) margin_bottom: f32,
    /// A box with no content whose top and bottom margins meet.
    pub(crate) collapse_through: bool,
}

pub(crate) struct Ctx<'a> {
    pub(crate) doc: &'a Document,
    pub(crate) styles: &'a ComputedStyles,
    pub(crate) fonts: &'a mut FontContext,
    out: LayoutTree,
    root_font_size: f32,
    viewport: Viewport,
    depth: usize,
    /// Floats placed inside a given containing block, consulted by line breaking.
    floats: Vec<(NodeId, Float, Rect)>,
    /// Boxes whose geometry was solved elsewhere (taffy) and must be respected.
    pub(crate) forced: HashMap<NodeId, (f32, f32, f32, f32)>,
    /// Boxes that size to their content instead of filling their container:
    /// inline-blocks and flex items, both of which are shrink-to-fit.
    pub(crate) shrink_fit: std::collections::HashSet<NodeId>,
    /// Out-of-flow boxes, each with the positioned ancestor that contains it
    /// (`None` = the initial containing block, i.e. the viewport).
    absolutes: Vec<(NodeId, Option<NodeId>)>,
    /// The nearest positioned ancestor while laying out.
    positioned_ancestor: Option<NodeId>,
    /// `img` intrinsic sizes, read once per node.
    image_sizes: HashMap<NodeId, Option<(f32, f32)>>,
    /// Memoised measurements to prevent exponential blowup on nested flex/table containers.
    measure_cache: HashMap<(NodeId, u32), (f32, f32)>,
    max_content_cache: HashMap<(NodeId, u32), f32>,
    min_content_cache: HashMap<(NodeId, u32), f32>,
}

static FALLBACK_STYLE: std::sync::OnceLock<Style> = std::sync::OnceLock::new();

pub fn layout(
    doc: &Document,
    styles: &ComputedStyles,
    fonts: &mut FontContext,
    viewport: Viewport,
) -> LayoutTree {
    set_viewport(viewport.width, viewport.height);
    let mut ctx = Ctx {
        doc,
        styles,
        fonts,
        out: LayoutTree::default(),
        root_font_size: crate::text::root_font_size(),
        viewport,
        depth: 0,
        floats: Vec::new(),
        forced: HashMap::new(),
        shrink_fit: std::collections::HashSet::new(),
        absolutes: Vec::new(),
        positioned_ancestor: None,
        image_sizes: HashMap::new(),
        measure_cache: HashMap::new(),
        max_content_cache: HashMap::new(),
        min_content_cache: HashMap::new(),
    };

    let root = match doc.elements().into_iter().next() {
        Some(id) => id,
        None => return LayoutTree::default(),
    };

    // Absolutely positioned boxes are out of flow, so they are placed once the
    // document's normal flow has settled. Placing one can queue more.
    ctx.layout_block(root, 0.0, viewport.width, 0.0, 0.0, false);
    for _ in 0..64 {
        if ctx.absolutes.is_empty() {
            break;
        }
        let queued = std::mem::take(&mut ctx.absolutes);
        for (id, cb) in queued {
            let block = match cb.and_then(|c| ctx.out.rects.get(&c).copied()) {
                Some(rect) => {
                    let style = ctx.style(cb.unwrap()).clone();
                    Rect::new(
                        rect.x + style.border_width.left,
                        rect.y + style.border_width.top,
                        rect.width - style.border_width.left - style.border_width.right,
                        rect.height - style.border_width.top - style.border_width.bottom,
                    )
                }
                None => Rect::new(0.0, 0.0, viewport.width, viewport.height),
            };
            ctx.layout_absolute(id, block);
        }
    }
    let block = ctx
        .out
        .rects
        .get(&root)
        .map(|r| Block { rect: *r, margin_bottom: 0.0, collapse_through: false })
        .unwrap_or(Block { rect: Rect::new(0.0, 0.0, viewport.width, 0.0), margin_bottom: 0.0, collapse_through: false });
    ctx.out.document_height = block.rect.bottom().max(viewport.height);
    // A page can be wider than the viewport (a fixed-width body, a long line):
    // the document is as wide as anything that actually got placed.
    let widest = ctx
        .out
        .rects
        .values()
        .map(|r| r.right())
        .fold(0.0f32, f32::max);
    ctx.out.document_width = block.rect.right().max(widest).max(viewport.width);
    ctx.out
}

impl<'a> Ctx<'a> {
    pub(crate) fn style(&self, id: NodeId) -> &Style {
        self.styles
            .styles
            .get(&id)
            .unwrap_or_else(|| FALLBACK_STYLE.get_or_init(Style::default))
    }

    // ------------------------------------------------------------ block

    /// Lay out a block-level box. `y_cursor` is the top of the box's margin area;
    /// the returned `margin_top` says how much of it was spent before the border box.
    /// `y_cursor` is the bottom of the previous box; `prev_mb` is the bottom margin
    /// it left pending, which collapses with this box's top margin. `suppress_mt`
    /// means the top margin was already spent above the parent (first-child escape).
    pub(crate) fn layout_block(
        &mut self,
        id: NodeId,
        cb_x: f32,
        cb_width: f32,
        y_cursor: f32,
        prev_mb: f32,
        suppress_mt: bool,
    ) -> Block {
        self.depth += 1;
        if self.depth > 300 {
            self.depth -= 1;
            return Block {
                rect: Rect::new(cb_x, y_cursor, 0.0, 0.0),
                margin_bottom: 0.0,
                collapse_through: true,
            };
        }

        let style = self.style(id).clone();
        let fs = style.font_size;
        let root = self.root_font_size;

        // A positioned element becomes the containing block for its descendants.
        let saved_ancestor = self.positioned_ancestor;
        let positioned = style.position != Position::Static;
        if positioned {
            self.positioned_ancestor = Some(id);
        }
        let mark_rects = self.out.rects.len();
        let mark_text = self.out.text.len();

        let intrinsic = match self.doc.nodes[id].name.as_str() {
            "img" => self.intrinsic_size(id),
            "svg" => self.declared_size(id),
            _ => None,
        };

        let margin = resolve_edges(style.margin, fs, root, cb_width);
        let padding = resolve_edges(style.padding, fs, root, cb_width);
        let border = style.border_width;

        let bh = padding.left + padding.right + border.left + border.right;

        // A box whose size was solved by a flex container keeps that geometry.
        if let Some((fx, fy, fw, fh)) = self.forced.remove(&id) {
            let content_width = (fw - bh).max(0.0);
            let content_x = fx + border.left + padding.left;
            let content_y = fy + border.top + padding.top;
            // A flex container keeps being a flex container however it got its size:
            // its children are flex items, not block children.
            let (used, last_mb, has_content, ends_with_line) =
                if style.display == Display::Flex {
                    let definite = if fh > 0.0 {
                        Some((fh - padding.top - padding.bottom - border.top - border.bottom).max(0.0))
                    } else {
                        None
                    };
                    let h = self.layout_flex(id, content_x, content_y, content_width, definite);
                    (h, 0.0, h > 0.0, false)
                } else if style.display == Display::Table {
                    let h = self.layout_table(id, content_x, content_y, content_width);
                    (h, 0.0, h > 0.0, false)
                } else {
                    self.layout_children(id, &style, content_x, content_y, content_width)
                };
            // A forced height of zero means "auto": the box is as tall as its content.
            let height = if fh > 0.0 {
                fh
            } else {
                used + padding.top + padding.bottom + border.top + border.bottom
            };
            let rect = Rect::new(fx, fy, fw, height);
            self.out.rects.insert(id, rect);
            self.positioned_ancestor = saved_ancestor;
            self.depth -= 1;
            return Block {
                rect,
                margin_bottom: 0.0,
                collapse_through: !has_content && !ends_with_line && (last_mb > 0.0),
            };
        }

        // ---- width
        let auto_width = is_auto(style.width);
        let content_width = if auto_width && intrinsic.is_some() {
            intrinsic.unwrap().0
        } else if auto_width {
            let available = (cb_width - margin.left - margin.right - bh).max(0.0);
            // An inline-block is shrink-to-fit by definition; the set also covers
            // boxes being measured for a flex container.
            if style.display == Display::InlineBlock
                || style.display == Display::Table
                || self.shrink_fit.contains(&id)
            {
                // Shrink-to-fit: min(max-content, available), both border-box widths.
                let max_border = self.max_content_width(id, cb_width);
                let available_border = (available + bh).max(0.0);
                (max_border.min(available_border) - bh).max(0.0)
            } else {
                available
            }
        } else {
            let mut w = resolve_len(style.width, fs, root, cb_width).unwrap_or(0.0);
            if style.box_sizing == BoxSizing::BorderBox {
                w = (w - bh).max(0.0);
            }
            w.max(0.0)
        };
        let mut content_width = content_width;
        let limit = |len: Length| resolve_len(len, fs, root, cb_width);
        if let Some(max) = limit(style.max_width) {
            let max = if style.box_sizing == BoxSizing::BorderBox { (max - bh).max(0.0) } else { max };
            content_width = content_width.min(max);
        }
        if let Some(min) = limit(style.min_width) {
            let min = if style.box_sizing == BoxSizing::BorderBox { (min - bh).max(0.0) } else { min };
            content_width = content_width.max(min);
        }
        let border_box_width = content_width + bh;

        // ---- horizontal position, including auto-margin centring
        let free = cb_width - border_box_width;
        // `margin: auto` takes up the slack on either side. A `width: auto` box
        // normally fills its container (free space is zero, so nothing happens),
        // but `max-width` can leave the same slack for the margins to absorb.
        let auto_left = matches!(style.margin.left, Length::Keyword | Length::Invalid);
        let auto_right = matches!(style.margin.right, Length::Keyword | Length::Invalid);
        let (ml, _mr) = match (auto_left, auto_right) {
            (true, true) => (free.max(0.0) / 2.0, free.max(0.0) / 2.0),
            (true, false) => (free.max(0.0), margin.right),
            (false, true) => (margin.left, free.max(0.0)),
            _ => (margin.left, margin.right),
        };
        let x = cb_x + ml;

        // ---- top margin, collapsing through the first child if it can escape.
        // The root element is the document's edge: a child's margin cannot escape
        // past it, so `<body>`'s default 8px margin pushes `body` down rather than
        // moving the whole page.
        let can_collapse_top = self.depth > 1 && border.top == 0.0 && padding.top == 0.0;
        let first_child_mt = if can_collapse_top && !suppress_mt {
            self.first_in_flow_block_child(id, content_width)
        } else {
            None
        };
        // A child's margin that escapes through an open top edge moves the parent's
        // border box down even when the parent's own margin was already spent — but
        // only once: a box whose top margin was already counted by an ancestor must
        // not count the same group again.
        let escaped = first_child_mt.map(|c| (c - margin.top).max(0.0)).unwrap_or(0.0);
        let own_gap = if suppress_mt { prev_mb } else { prev_mb.max(margin.top) };
        let y = y_cursor + own_gap + escaped;
        let content_x = x + border.left + padding.left;
        let content_y = y + border.top + padding.top;

        // ---- children
        let (content_height_used, last_mb, has_content, ends_with_line) = if style.display
            == Display::Flex
        {
            let definite = if is_auto(style.height) {
                None
            } else {
                Some(
                    resolve_len(style.height, fs, root, self.viewport.height)
                        .unwrap_or(0.0)
                        .max(0.0),
                )
            };
            let h = self.layout_flex(id, content_x, content_y, content_width, definite);
            (h, 0.0, h > 0.0, false)
        } else if style.display == Display::Table {
            let h = self.layout_table(id, content_x, content_y, content_width);
            (h, 0.0, h > 0.0, false)
        } else {
            self.layout_children(id, &style, content_x, content_y, content_width)
        };
        // ---- the last child's bottom margin escapes when nothing stops it. The
        // root is the document's edge, so it keeps that margin in its own height.
        let is_root = self.depth == 1;
        let can_collapse_bottom = !is_root
            && is_auto(style.height)
            && border.bottom == 0.0
            && padding.bottom == 0.0
            && !ends_with_line;

        let mut content_height = if is_auto(style.height) && intrinsic.is_some() {
            intrinsic.unwrap().1
        } else if is_auto(style.height) {
            content_height_used + if can_collapse_bottom { 0.0 } else { last_mb }
        } else {
            let mut h = resolve_len(style.height, fs, root, self.viewport.height).unwrap_or(0.0);
            if style.box_sizing == BoxSizing::BorderBox {
                h = (h - padding.top - padding.bottom - border.top - border.bottom).max(0.0);
            }
            h.max(0.0)
        };
        content_height = content_height.max(0.0);

        let effective_mb = if can_collapse_bottom {
            margin.bottom.max(last_mb)
        } else {
            margin.bottom
        };

        let rect = Rect::new(
            x,
            y,
            border_box_width,
            content_height + padding.top + padding.bottom + border.top + border.bottom,
        );
        self.out.rects.insert(id, rect);

        let collapse_through = !has_content
            && rect.height == 0.0
            && border.top == 0.0
            && border.bottom == 0.0
            && padding.top == 0.0
            && padding.bottom == 0.0;

        // `position: relative` lays the box out in flow, then shifts it and
        // everything it contains by the offsets.
        let mut rect = rect;
        if style.position == Position::Relative {
            let dx = offset_px(style.left, fs, root, cb_width)
                .or_else(|| offset_px(style.right, fs, root, cb_width).map(|v| -v))
                .unwrap_or(0.0);
            let dy = offset_px(style.top, fs, root, cb_width)
                .or_else(|| offset_px(style.bottom, fs, root, cb_width).map(|v| -v))
                .unwrap_or(0.0);
            if dx != 0.0 || dy != 0.0 {
                self.shift_subtree(id, mark_rects, mark_text, dx, dy);
                rect.x += dx;
                rect.y += dy;
            }
        }
        self.positioned_ancestor = saved_ancestor;

        self.depth -= 1;
        Block {
            rect,
            margin_bottom: effective_mb,
            collapse_through,
        }
    }

    /// Move every box and text run laid out for `id`'s subtree since `mark`.
    fn shift_subtree(&mut self, id: NodeId, mark_rects: usize, mark_text: usize, dx: f32, dy: f32) {
        let mut members: std::collections::HashSet<NodeId> = std::collections::HashSet::new();
        let mut stack = vec![id];
        while let Some(node) = stack.pop() {
            members.insert(node);
            for child in &self.doc.nodes[node].children {
                stack.push(*child);
            }
        }
        let keys: Vec<NodeId> = self.out.rects.keys().copied().collect();
        let _ = mark_rects;
        for key in keys {
            if members.contains(&key) {
                if let Some(rect) = self.out.rects.get_mut(&key) {
                    rect.x += dx;
                    rect.y += dy;
                }
            }
        }
        let _ = mark_text;
        let node_members = members.clone();
        for run in self.out.text.iter_mut() {
            if node_members.contains(&run.node) {
                run.rect.x += dx;
                run.rect.y += dy;
            }
        }
        for line in self.out.lines.iter_mut() {
            if node_members.contains(&line.owner) {
                line.rect.x += dx;
                line.rect.y += dy;
            }
        }
    }

    /// Place one out-of-flow box against its containing block.
    fn layout_absolute(&mut self, id: NodeId, cb: Rect) {
        let style = self.style(id).clone();
        let fs = style.font_size;
        let root = self.root_font_size;
        let margin = resolve_edges(style.margin, fs, root, cb.width);
        let padding = resolve_edges(style.padding, fs, root, cb.width);
        let border = style.border_width;
        let bh = padding.left + padding.right + border.left + border.right;
        let bv = padding.top + padding.bottom + border.top + border.bottom;

        // Width: an explicit width wins; both offsets set and no width means the box
        // is stretched between them; otherwise shrink-to-fit.
        let left = offset_px(style.left, fs, root, cb.width);
        let right = offset_px(style.right, fs, root, cb.width);
        let width = match (style.width, left, right) {
            (Length::Keyword, Some(l), Some(r)) => Some((cb.width - l - r - margin.left - margin.right).max(0.0)),
            (Length::Keyword | Length::Invalid, _, _) => {
                let max = self.max_content_width(id, cb.width);
                let min = self.min_content_width(id, cb.width);
                Some(max.min(cb.width - margin.left - margin.right).max(min))
            }
            (other, _, _) => {
                let mut w = resolve_len(other, fs, root, cb.width).unwrap_or(0.0);
                if style.box_sizing == BoxSizing::BorderBox {
                    w = (w - bh).max(0.0);
                }
                Some(w)
            }
        };

        let top = offset_px(style.top, fs, root, cb.height);
        let bottom = offset_px(style.bottom, fs, root, cb.height);
        let height = match (style.height, top, bottom) {
            (Length::Keyword, Some(t), Some(b)) => Some((cb.height - t - b - margin.top - margin.bottom).max(0.0)),
            (Length::Keyword | Length::Invalid, _, _) => None,
            (other, _, _) => {
                let mut h = resolve_len(other, fs, root, cb.height).unwrap_or(0.0);
                if style.box_sizing == BoxSizing::BorderBox {
                    h = (h - bv).max(0.0);
                }
                Some(h)
            }
        };

        let border_box_w = width.unwrap_or(0.0) + bh;
        let x = match (left, right) {
            (Some(l), _) => cb.x + l + margin.left,
            (None, Some(r)) => cb.right() - r - margin.right - border_box_w,
            _ => cb.x + margin.left,
        };
        let y = match (top, bottom) {
            (Some(t), _) => cb.y + t + margin.top,
            (None, Some(b)) => cb.bottom() - b - margin.bottom - height.unwrap_or(0.0) - bv,
            _ => cb.y + margin.top,
        };

        let width = width.unwrap_or(0.0);
        // Lay the box out at its measured size, then let height fall out of content.
        self.shrink_fit.insert(id);
        let block = self.layout_block_at(id, x, y, width);
        self.shrink_fit.remove(&id);
        let mut rect = block.rect;
        if let Some(h) = height {
            rect.height = h + bv;
            self.out.rects.insert(id, rect);
        }
    }

    /// Lay out a table: rows of cells, with columns sized the CSS 2.1 auto way.
    ///
    /// Each column starts at the widest minimum content of its cells, then grows
    /// toward its maximum content as long as the table has room. A cell that
    /// cannot fit is laid out narrow and its text wraps, exactly as a browser
    /// would do. Returns the height the rows used.
    fn layout_table(&mut self, id: NodeId, x: f32, y: f32, available: f32) -> f32 {
        // Rows may be wrapped in row groups, or sit directly in the table.
        let mut rows = self.table_rows(id);
        // A table of bare cells (or rows that are really rows) still gets laid out
        // in one implicit row rather than vanishing.
        if rows.is_empty() {
            rows.push(id);
        }

        struct Cell {
            node: NodeId,
            column: usize,
            span: usize,
            min: f32,
            max: f32,
        }

        let mut cells: Vec<Cell> = Vec::new();
        let mut columns = 0usize;
        for row in &rows {
            let mut column = 0usize;
            for cell in self.doc.nodes[*row].children.clone() {
                let style = self.style(cell).clone();
                if style.display != Display::TableCell {
                    continue;
                }
                let span = self
                    .doc
                    .nodes[cell]
                    .attr("colspan")
                    .and_then(|s| s.trim().parse::<usize>().ok())
                    .filter(|s| (1..=64).contains(s))
                    .unwrap_or(1);
                let min = self.min_content_width(cell, available);
                let max = self.max_content_width(cell, available);
                cells.push(Cell { node: cell, column, span, min: min.max(1.0), max: max.max(min) });
                column += span;
                columns = columns.max(column);
            }
        }
        if cells.is_empty() || columns == 0 {
            return 0.0;
        }

        // Column widths: the widest minimum, then share the slack toward maximum.
        let mut widths = vec![0.0f32; columns];
        let mut maximums = vec![0.0f32; columns];
        for cell in &cells {
            let share = cell.span as f32;
            let min_share = cell.min / share;
            let max_share = cell.max / share;
            for index in cell.column..cell.column + cell.span {
                widths[index] = widths[index].max(min_share);
                maximums[index] = maximums[index].max(max_share);
            }
        }
        let min_total: f32 = widths.iter().sum();
        let max_total: f32 = maximums.iter().sum();
        if max_total > min_total {
            let room = (available - min_total).max(0.0);
            let want: f32 = maximums.iter().zip(&widths).map(|(m, w)| (m - w).max(0.0)).sum();
            let scale = if want > 0.0 { (room / want).min(1.0) } else { 0.0 };
            for index in 0..columns {
                widths[index] += ((maximums[index] - widths[index]).max(0.0)) * scale;
            }
        }
        // Nothing fits: shrink every column proportionally instead of overflowing.
        let total: f32 = widths.iter().sum();
        if total > available && total > 0.0 {
            let scale = available / total;
            for width in widths.iter_mut() {
                *width *= scale;
            }
        }

        let mut cursor_y = y;
        let mut row_rects: Vec<(NodeId, Rect)> = Vec::new();
        for row in &rows {
            let mut cursor_x = x;
            let mut column = 0usize;
            let mut row_height: f32 = 0.0;
            let mut row_cells: Vec<NodeId> = Vec::new();
            for cell in self.doc.nodes[*row].children.clone() {
                let style = self.style(cell).clone();
                if style.display != Display::TableCell {
                    continue;
                }
                let span = cells
                    .iter()
                    .find(|c| c.node == cell)
                    .map(|c| c.span)
                    .unwrap_or(1);
                let width: f32 = widths[column..(column + span).min(columns)].iter().sum();
                column += span;
                let padding = resolve_edges(style.padding, style.font_size, self.root_font_size, width);
                let inset = padding.left + padding.right + style.border_width.left
                    + style.border_width.right;
                let content_width = (width - inset).max(0.0);
                let block = self.layout_block_at(cell, cursor_x, cursor_y, content_width);
                // A cell paints its own background; it also fills the row height.
                let mut rect = self.out.rects.get(&cell).copied().unwrap_or(block.rect);
                rect.x = cursor_x;
                rect.y = cursor_y;
                rect.width = width;
                self.out.rects.insert(cell, rect);
                row_height = row_height.max(rect.height);
                row_cells.push(cell);
                cursor_x += width;
            }
            // Stretch short cells to the row: borders and backgrounds line up.
            for cell in row_cells {
                if let Some(rect) = self.out.rects.get(&cell).copied() {
                    if rect.height < row_height {
                        self.out.rects.insert(cell, Rect::new(rect.x, rect.y, rect.width, row_height));
                    }
                }
            }
            let rect = Rect::new(x, cursor_y, widths.iter().sum::<f32>(), row_height);
            self.out.rects.insert(*row, rect);
            row_rects.push((*row, rect));
            cursor_y += row_height;
        }
        cursor_y - y
    }

    /// Lay a box out with its border box pinned at (x, y) and a given content width.
    fn layout_block_at(&mut self, id: NodeId, x: f32, y: f32, content_width: f32) -> Block {
        let style = self.style(id).clone();
        let padding = resolve_edges(style.padding, style.font_size, self.root_font_size, content_width);
        let border = style.border_width;
        let bh = padding.left + padding.right + border.left + border.right;
        // A forced height of zero asks `layout_block` to size the box to its content.
        self.forced.insert(id, (x, y, content_width + bh, 0.0));
        self.layout_block(id, x, content_width, y, 0.0, true)
    }

    /// Lay out a block container's children. Returns (height used inside the
    /// content box, bottom margin of the last child, whether anything was placed,
    /// whether the content ends with a line box — which stops bottom-margin escape).
    fn layout_children(
        &mut self,
        id: NodeId,
        style: &Style,
        content_x: f32,
        content_y: f32,
        content_width: f32,
    ) -> (f32, f32, bool, bool) {
        let children: Vec<NodeId> = self.doc.nodes[id]
            .children
            .iter()
            .copied()
            .filter(|c| {
                let n = &self.doc.nodes[*c];
                match n.kind {
                    Kind::Element => true,
                    // Whitespace-only text still separates two inline boxes; a run
                    // that holds nothing else produces no line of its own.
                    Kind::Text => true,
                    _ => false,
                }
            })
            .collect();

        let mut cursor = content_y;
        let mut last_mb = 0.0f32;
        let mut pending: Option<f32> = Some(0.0);
        let mut has_content = false;
        let mut ends_with_line = false;
        // Matching `layout_block`: at the root, a child's margin stays inside the
        // document's box instead of escaping out of the page.
        let can_collapse_top = self.depth > 1
            && style.border_width.top == 0.0
            && resolve_len(style.padding.top, style.font_size, self.root_font_size, content_width)
                .unwrap_or(0.0)
                == 0.0;

        let mut i = 0usize;
        while i < children.len() {
            let child = children[i];

            // Out-of-flow children are queued first, whatever their display, and
            // then skipped by the flow below.
            if self.is_out_of_flow(child) && self.style(child).display != Display::None {
                let cb = if self.style(child).position == Position::Fixed {
                    None
                } else {
                    self.positioned_ancestor
                };
                self.absolutes.push((child, cb));
                i += 1;
                continue;
            }

            let inline_level = self.is_inline_level(child);

            if inline_level {
                let start = i;
                while i < children.len()
                    && self.is_inline_level(children[i])
                    && !self.is_out_of_flow(children[i])
                {
                    i += 1;
                }
                let run = &children[start..i];
                // A run of nothing but whitespace produces no line and must not
                // consume a pending margin.
                let blank = run.iter().all(|c| {
                    let n = &self.doc.nodes[*c];
                    n.kind == Kind::Text && n.text.trim().is_empty()
                });
                if blank {
                    continue;
                }
                if let Some(owed) = pending.take() {
                    cursor += owed;
                }
                let height = self.layout_inline(id, run, style, content_x, cursor, content_width);
                cursor += height;
                last_mb = 0.0;
                if height > 0.0 {
                    has_content = true;
                    ends_with_line = true;
                }
                pending = Some(0.0);
                continue;
            }

            i += 1;

            // A block child ends any run of inline content; that is handled by the
            // ordering above, so `pending` simply carries on.
            let child_style = self.style(child).clone();
            if child_style.display == Display::None {
                continue;
            }
            let child_margin = resolve_edges(
                child_style.margin,
                child_style.font_size,
                self.root_font_size,
                content_width,
            );

            // `clear` drops the box below the floats on that side.
            if child_style.clear != Clear::None {
                let bottom = self
                    .floats
                    .iter()
                    .filter(|(c, side, _)| *c == id && clears(child_style.clear, *side))
                    .map(|(_, _, r)| r.bottom())
                    .fold(cursor, f32::max);
                if bottom > cursor {
                    cursor = bottom;
                    pending = Some(0.0);
                }
            }



            // A float is taken out of the flow: it is laid out where it sits and the
            // cursor does not move past it.
            if child_style.float != Float::None {
                let (w, _h) = self.measure_box(child, content_width);
                let x = match child_style.float {
                    Float::Right => content_x + content_width - w,
                    _ => content_x,
                };
                let block = self.layout_block(child, x, content_width, cursor, 0.0, true);
                let rect = self.out.rects.get(&child).copied().unwrap_or(block.rect);
                // Text is pushed aside by the float's *margin* box, not its border box.
                let band = Rect::new(
                    rect.x - child_margin.left,
                    rect.y - child_margin.top,
                    rect.width + child_margin.left + child_margin.right,
                    rect.height + child_margin.top + child_margin.bottom,
                );
                self.floats.push((id, child_style.float, band));
                continue;
            }

            // The child's top margin collapses with whatever the previous box left
            // pending. For the first child of a parent whose top edge is open, the
            // margin has already been spent above the parent's border box.
            // The parent's top edge is open until something with height is placed,
            // so every leading child (including empty spacers) has its top margin
            // already spent above the parent.
            let escrow = can_collapse_top && !has_content;
            // While that group is open, its total has already been counted above
            // this box: neither the pending margin nor the child's own applies.
            let prev_mb = if escrow { 0.0 } else { pending.take().unwrap_or(0.0) };
            ends_with_line = false;

            let before = cursor;
            let block = self.layout_block(child, content_x, content_width, cursor, prev_mb, escrow);
            // A box that collapses through (an empty paragraph used as a spacer)
            // adds no height of its own: its margins join the pending group.
            cursor = if block.collapse_through { before } else { block.rect.bottom() };
            last_mb = block.margin_bottom;
            if !block.collapse_through {
                has_content = true;
            }
            pending = Some(match (block.collapse_through, pending) {
                (true, Some(owed)) => owed.max(block.margin_bottom),
                _ => block.margin_bottom,
            });
        }

        // Floats only affect lines inside this block.
        self.floats.retain(|(container, _, _)| *container != id);

        (cursor - content_y, last_mb, has_content, ends_with_line)
    }

    /// Lay a box out into a throwaway tree to learn its size.
    pub(crate) fn measure_box(&mut self, id: NodeId, cb_width: f32) -> (f32, f32) {
        let key = (id, width_key(cb_width));
        if let Some(hit) = self.measure_cache.get(&key) {
            return *hit;
        }
        let saved = std::mem::take(&mut self.out);
        let saved_floats = std::mem::take(&mut self.floats);
        // A measurement must not leak the geometry a flex or table pass handed
        // its children: a stale `forced` entry would make the next measurement
        // of that box report the size it had last time instead of its content.
        let saved_forced = std::mem::take(&mut self.forced);
        let saved_absolutes = std::mem::take(&mut self.absolutes);
        let depth = self.depth;
        let added = self.shrink_fit.insert(id);
        let block = self.layout_block(id, 0.0, cb_width, 0.0, 0.0, true);
        if added {
            self.shrink_fit.remove(&id);
        }
        self.out = saved;
        self.floats = saved_floats;
        self.forced = saved_forced;
        self.absolutes = saved_absolutes;
        self.depth = depth;
        let out = (block.rect.width, block.rect.height);
        self.measure_cache.insert(key, out);
        out
    }

    /// The width a table's columns need. Maximum: the widest row, cells side by
    /// side at their max content. Minimum: the widest cell, since columns cannot
    /// overlap. `None` when this box is not a table.
    fn table_intrinsic_width(
        &mut self,
        id: NodeId,
        style: &Style,
        cb_width: f32,
        max: bool,
    ) -> Option<f32> {
        if style.display != Display::Table {
            return None;
        }
        let _root = self.root_font_size;
        let mut widest_row = 0.0f32;
        let mut widest_cell = 0.0f32;
        for row in self.table_rows(id) {
            let mut row_width = 0.0f32;
            for cell in self.doc.nodes[row].children.clone() {
                let cs = self.style(cell).clone();
                if cs.display != Display::TableCell {
                    continue;
                }
                let span = self
                    .doc
                    .nodes[cell]
                    .attr("colspan")
                    .and_then(|s| s.trim().parse::<usize>().ok())
                    .filter(|s| (1..=64).contains(s))
                    .unwrap_or(1) as f32;
                let content = if max {
                    self.max_content_width(cell, cb_width)
                } else {
                    self.min_content_width(cell, cb_width)
                };
                row_width += content;
                widest_cell = widest_cell.max(content / span);
            }
            widest_row = widest_row.max(row_width);
        }
        if max {
            Some(widest_row)
        } else {
            Some(widest_cell * 1.0)
        }
    }

    /// A table's rows, unwrapping row groups.
    fn table_rows(&self, id: NodeId) -> Vec<NodeId> {
        let mut rows = Vec::new();
        for child in &self.doc.nodes[id].children {
            match self.style(*child).display {
                Display::TableRow => rows.push(*child),
                Display::TableRowGroup => {
                    for row in &self.doc.nodes[*child].children {
                        if self.style(*row).display == Display::TableRow {
                            rows.push(*row);
                        }
                    }
                }
                _ => {}
            }
        }
        rows
    }

    /// The width a row flex container's items need, laid end to end. `None` when
    /// this box is not a non-wrapping row flex container, so the ordinary
    /// block/inline content measurement applies.
    fn flex_intrinsic_width(
        &mut self,
        id: NodeId,
        style: &Style,
        cb_width: f32,
        max: bool,
    ) -> Option<f32> {
        if style.display != Display::Flex
            || !matches!(style.flex_direction, FlexDirection::Row | FlexDirection::RowReverse)
        {
            return None;
        }
        let root = self.root_font_size;
        let mut total = 0.0f32;
        let mut widest = 0.0f32;
        let mut items = 0usize;
        for child in self.doc.nodes[id].children.clone() {
            let cs = self.style(child).clone();
            if cs.display == Display::None {
                continue;
            }
            if self.doc.nodes[child].kind != Kind::Element {
                continue;
            }
            let margin = resolve_edges(cs.margin, cs.font_size, root, cb_width);
            let inner = match cs.width {
                Length::Keyword | Length::Invalid => match cs.flex_basis {
                    // `flex-basis` replaces the content size on the main axis.
                    Length::Keyword | Length::Invalid => {
                        if max {
                            self.max_content_width(child, cb_width)
                        } else {
                            self.min_content_width(child, cb_width)
                        }
                    }
                    other => resolve_len(other, cs.font_size, root, cb_width).unwrap_or(0.0),
                },
                other => resolve_len(other, cs.font_size, root, cb_width).unwrap_or(0.0),
            };
            let outer = inner + margin.left + margin.right;
            total += outer;
            widest = widest.max(outer);
            items += 1;
        }
        if !max && style.flex_wrap {
            // A wrapping row may put every item on a line of its own, so its
            // min-content width is its widest item, not the sum of all of them.
            return Some(widest);
        }
        if items > 1 {
            total += style.column_gap * (items - 1) as f32;
        }
        Some(total)
    }

    /// The widest this box's content can be without wrapping, plus its own
    /// horizontal padding and borders. Text measurement is the engine's own, so a
    /// shrink-to-fit box is as wide as its line, not as wide as its container.
    pub(crate) fn max_content_width(&mut self, id: NodeId, cb_width: f32) -> f32 {
        let key = (id, width_key(cb_width));
        if let Some(hit) = self.max_content_cache.get(&key) {
            return *hit;
        }
        let style = self.style(id).clone();
        let fs = style.font_size;
        let root = self.root_font_size;
        let padding = resolve_edges(style.padding, fs, root, cb_width);
        let border = style.border_width;
        let own = padding.left + padding.right + border.left + border.right;

        // A row flex container is as wide as its items laid end to end — that is
        // what makes a nav bar hug its links instead of collapsing to the widest
        // one, and it is how the container gets a sensible `flex-basis`.
        if let Some(intrinsic) = self.flex_intrinsic_width(id, &style, cb_width, true) {
            let res = intrinsic + own;
            self.max_content_cache.insert(key, res);
            return res;
        }
        if let Some(intrinsic) = self.table_intrinsic_width(id, &style, cb_width, true) {
            let res = intrinsic + own;
            self.max_content_cache.insert(key, res);
            return res;
        }

        let mut widest: f32 = 0.0;
        let mut run: f32 = 0.0;
        for child in self.doc.nodes[id].children.clone() {
            let node = &self.doc.nodes[child];
            match node.kind {
                Kind::Text => {
                    // A whitespace-only node is still a space between two inline
                    // boxes, but on its own it has no width.
                    let text = node.text.clone();
                    run += self.unwrapped_text_width(&text, &style);
                }
                Kind::Element => {
                    let cs = self.style(child).clone();
                    if cs.display == Display::None {
                        continue;
                    }
                    // `<br>` is a forced break: the run before it cannot grow.
                    if node.name == "br" {
                        widest = widest.max(run);
                        run = 0.0;
                        continue;
                    }
                    let margin = resolve_edges(cs.margin, cs.font_size, root, cb_width);
                    match cs.display {
                        Display::Block | Display::Flex | Display::ListItem => {
                            // A block starts a new line: what came before cannot grow.
                            widest = widest.max(run);
                            run = 0.0;
                            let inner = match cs.width {
                                Length::Keyword | Length::Invalid => {
                                    self.max_content_width(child, f32::MAX / 8.0)
                                }
                                other => resolve_len(other, cs.font_size, root, cb_width)
                                    .unwrap_or(0.0),
                            };
                            widest = widest.max(inner + margin.left + margin.right);
                        }
                        _ => {
                            // Inline-level siblings share one line, so they add up.
                            run += self.max_content_width(child, f32::MAX / 8.0)
                                + margin.left
                                + margin.right;
                            widest = widest.max(run);
                        }
                    }
                }
                _ => {}
            }
        }
        let res = widest.max(run).max(0.0) + own;
        self.max_content_cache.insert(key, res);
        res
    }

    /// The narrowest this box can be before a word has to overflow: the widest
    /// unbreakable run of text, plus its own horizontal padding and borders.
    pub(crate) fn min_content_width(&mut self, id: NodeId, cb_width: f32) -> f32 {
        let key = (id, width_key(cb_width));
        if let Some(hit) = self.min_content_cache.get(&key) {
            return *hit;
        }
        let style = self.style(id).clone();
        let fs = style.font_size;
        let root = self.root_font_size;
        let padding = resolve_edges(style.padding, fs, root, cb_width);
        let border = style.border_width;
        if let Some(intrinsic) = self.flex_intrinsic_width(id, &style, cb_width, false) {
            let res = intrinsic + padding.left + padding.right + border.left + border.right;
            self.min_content_cache.insert(key, res);
            return res;
        }
        if let Some(intrinsic) = self.table_intrinsic_width(id, &style, cb_width, false) {
            let res = intrinsic + padding.left + padding.right + border.left + border.right;
            self.min_content_cache.insert(key, res);
            return res;
        }
        let mut widest: f32 = 0.0;
        let mut run: f32 = 0.0;

        for child in self.doc.nodes[id].children.clone() {
            let node = &self.doc.nodes[child];
            match node.kind {
                Kind::Text => {
                    let text = node.text.clone();
                    for word in text.split_whitespace() {
                        run += self.word_width(word, &style);
                        widest = widest.max(run);
                        run = 0.0;
                        let space = self.fonts.measure(&style.font_family, style.font_weight, style.font_size, " ");
                        widest = widest.max(space);
                    }
                }
                Kind::Element => {
                    let cs = self.style(child).clone();
                    if cs.display == Display::None {
                        continue;
                    }
                    if node.name == "br" {
                        widest = widest.max(run);
                        run = 0.0;
                        continue;
                    }
                    let margin = resolve_edges(cs.margin, cs.font_size, root, cb_width);
                    let inner = match cs.width {
                        Length::Keyword | Length::Invalid => self.min_content_width(child, cb_width),
                        other => resolve_len(other, cs.font_size, root, cb_width).unwrap_or(0.0),
                    };
                    widest = widest.max(run);
                    run = 0.0;
                    widest = widest.max(inner + margin.left + margin.right);
                }
                _ => {}
            }
        }
        let res = widest.max(run) + padding.left + padding.right + border.left + border.right;
        self.min_content_cache.insert(key, res);
        res
    }

    fn word_width(&mut self, word: &str, style: &Style) -> f32 {
        let text = if style.text_transform_upper { word.to_uppercase() } else { word.to_string() };
        self.fonts.measure(&style.font_family, style.font_weight, style.font_size, &text)
    }

    fn unwrapped_text_width(&mut self, text: &str, style: &Style) -> f32 {
        let mut collapsed = String::with_capacity(text.len());
        let mut prev_space = false;
        for ch in text.chars() {
            if ch.is_ascii_whitespace() {
                if !prev_space {
                    collapsed.push(' ');
                    prev_space = true;
                }
            } else {
                collapsed.push(ch);
                prev_space = false;
            }
        }
        if collapsed.is_empty() {
            return 0.0;
        }
        // Whitespace has width here: a space between two inline boxes is what keeps
        // them apart, and dropping it would under-measure every link in a nav.
        let text = if style.text_transform_upper { collapsed.to_uppercase() } else { collapsed };
        self.fonts
            .measure(&style.font_family, style.font_weight, style.font_size, &text)
    }

    /// Out-of-flow boxes are placed against a containing block, never inside a
    /// line — however inline their `display` says they are.
    fn is_out_of_flow(&self, id: NodeId) -> bool {
        matches!(
            self.style(id).position,
            Position::Absolute | Position::Fixed
        )
    }

    fn is_inline_level(&self, id: NodeId) -> bool {
        let node = &self.doc.nodes[id];
        if node.kind == Kind::Text {
            return true;
        }
        if node.kind != Kind::Element {
            return false;
        }
        matches!(self.style(id).display, Display::Inline | Display::InlineBlock)
    }

    /// The top margin of the first in-flow block child, if it can escape upward.
    /// The margin that escapes the top of `id`: its own, or a descendant's when
    /// its top edge is open and it has no content of its own.
    fn collapsed_top_margin(&self, id: NodeId, cb_width: f32, depth: usize) -> Option<f32> {
        if depth > 32 {
            return None;
        }
        let style = self.style(id);
        let margin = resolve_edges(style.margin, style.font_size, self.root_font_size, cb_width).top;
        let best = margin;
        // Only a box with an open top and nothing inline inside passes a margin on.
        let open = style.border_width.top == 0.0
            && is_auto(style.height)
            && is_auto(style.min_height)
            && resolve_len(style.padding.top, style.font_size, self.root_font_size, cb_width)
                .unwrap_or(0.0)
                == 0.0;
        if !open {
            return Some(best);
        }
        for child in &self.doc.nodes[id].children {
            let node = &self.doc.nodes[*child];
            match node.kind {
                Kind::Text if node.text.trim().is_empty() => continue,
                Kind::Text => return Some(best),
                Kind::Element => {
                    let child_style = self.style(*child);
                    if child_style.display == Display::None {
                        continue;
                    }
                    if matches!(
                        child_style.display,
                        Display::Inline | Display::InlineBlock | Display::TableCell | Display::TableRow
                    ) {
                        return Some(best);
                    }
                    return self
                        .collapsed_top_margin(*child, cb_width, depth + 1)
                        .map(|inner| best.max(inner));
                }
                _ => continue,
            }
        }
        Some(best)
    }

    fn first_in_flow_block_child(&self, id: NodeId, cb_width: f32) -> Option<f32> {
        let mut best = 0.0f32;
        for child in &self.doc.nodes[id].children {
            let node = &self.doc.nodes[*child];
            match node.kind {
                Kind::Text if node.text.trim().is_empty() => continue,
                Kind::Text => return None, // inline content stops the collapse
                Kind::Element => {
                    let s = self.style(*child);
                    if s.display == Display::None {
                        continue;
                    }
                    if matches!(s.display, Display::Inline | Display::InlineBlock) {
                        return None;
                    }
                    let margin = resolve_edges(s.margin, s.font_size, self.root_font_size, cb_width);
                    // An empty block collapses through: its margins join the next
                    // block's, so keep looking and take the largest. A box with a
                    // height of its own is not empty, however few children it has.
                    let no_height = is_auto(s.height) && is_auto(s.min_height);
                    let empty = no_height
                        && node.children.iter().all(|c| {
                            let inner = &self.doc.nodes[*c];
                            match inner.kind {
                                Kind::Text => inner.text.trim().is_empty(),
                                Kind::Element => {
                                    matches!(
                                        self.style(*c).display,
                                        Display::None | Display::TableRow | Display::TableCell
                                    )
                                }
                                _ => true,
                            }
                        });
                    if empty {
                        best = best.max(margin.top);
                        continue;
                    }
                    // The child may itself be open at the top, letting its own
                    // first child's margin escape through it and out of this box.
                    let child_style = s.clone();
                    let open = child_style.border_width.top == 0.0
                        && resolve_len(
                            child_style.padding.top,
                            child_style.font_size,
                            self.root_font_size,
                            cb_width,
                        )
                        .unwrap_or(0.0)
                        == 0.0;
                    if open {
                        if let Some(inner) = self.collapsed_top_margin(*child, cb_width, 0) {
                            return Some(best.max(margin.top).max(inner));
                        }
                    }
                    return Some(best.max(margin.top));
                }
                _ => continue,
            }
        }
        None
    }

    // ------------------------------------------------------------ inline

    /// Available horizontal span for a line at `top` with height `h`, avoiding the
    /// floats registered in this containing block.
    fn line_bounds(&self, container: NodeId, x: f32, width: f32, top: f32, h: f32) -> (f32, f32) {
        let mut left = x;
        let mut right = x + width;
        for (c, side, rect) in &self.floats {
            // A float pushes aside every line in its containing block, including
            // lines laid out by descendant blocks.
            let _ = container;
            let _ = c;
            // A float pushes a line aside only where their vertical ranges overlap.
            if rect.bottom() <= top + 0.01 || rect.y >= top + h - 0.01 {
                continue;
            }
            match side {
                Float::Left => left = left.max(rect.right()),
                Float::Right => right = right.min(rect.x),
                Float::None => {}
            }
        }
        (left, right.max(left))
    }

    /// Lay out a run of inline-level nodes into line boxes; returns the height used.
    fn layout_inline(
        &mut self,
        container: NodeId,
        nodes: &[NodeId],
        parent: &Style,
        x: f32,
        y: f32,
        width: f32,
    ) -> f32 {
        let mut tokens: Vec<Token> = Vec::new();
        for id in nodes {
            self.collect_tokens(*id, None, parent, width, &mut tokens);
        }
        if tokens.is_empty() {
            return 0.0;
        }

        let owner = nodes
            .iter()
            .copied()
            .find(|n| self.doc.nodes[*n].kind == Kind::Element)
            .unwrap_or(nodes[0]);

        let no_wrap = matches!(parent.white_space, WhiteSpace::Nowrap | WhiteSpace::Pre);
        if parent.word_break {
            tokens = break_long_words(tokens, width, &mut self.fonts);
        }

        // ---- pass 1: fill lines
        struct LineSpec {
            tokens: Vec<Token>,
            top: f32,
            left: f32,
            right: f32,
            /// Broken by wrapping rather than ending the paragraph: `text-align:
            /// justify` stretches these and leaves the last line alone.
            justify: bool,
        }
        let strut = self.strut_metrics(parent);
        let mut lines: Vec<LineSpec> = Vec::new();
        let mut top = y;
        let (mut left, mut right) = self.line_bounds(container, x, width, top, parent.line_height);
        let mut current: Vec<Token> = Vec::new();
        let mut used = 0.0f32;

        for token in tokens {
            if token.force_break {
                lines.push(LineSpec {
                    tokens: std::mem::take(&mut current),
                    top,
                    left,
                    right,
                    justify: false,
                });
                top += parent.line_height;
                used = 0.0;
                let b = self.line_bounds(container, x, width, top, parent.line_height);
                left = b.0;
                right = b.1;
                continue;
            }
            if token.is_space && current.is_empty() {
                continue;
            }
            if !no_wrap
                && !current.is_empty()
                && used + token.width > (right - left) + 0.005
            {
                let height = line_height_of(&current, strut);
                lines.push(LineSpec {
                    tokens: std::mem::take(&mut current),
                    top,
                    left,
                    right,
                    justify: true,
                });
                top += height;
                used = 0.0;
                let b = self.line_bounds(container, x, width, top, parent.line_height);
                left = b.0;
                right = b.1;
                if token.is_space {
                    continue;
                }
            }
            used += token.width;
            current.push(token);
        }
        if !current.is_empty() {
            lines.push(LineSpec { tokens: current, top, left, right, justify: false });
        }

        // Trailing spaces do not occupy the line.
        for line in &mut lines {
            while line.tokens.last().map(|t| t.is_space).unwrap_or(false) {
                line.tokens.pop();
            }
        }
        lines.retain(|l| !l.tokens.is_empty());
        if lines.is_empty() {
            return 0.0;
        }

        // ---- pass 2: place
        let mut fragments: HashMap<NodeId, Rect> = HashMap::new();
        let mut total = 0.0f32;
        for line in &lines {
            let line_width: f32 = line.tokens.iter().map(|t| t.width).sum();
            let line_height = line_height_of(&line.tokens, strut);
            let available = line.right - line.left;
            let slack = (available - line_width).max(0.0);
            let has_text = line
                .tokens
                .iter()
                .any(|t| !t.text.is_empty() && !t.is_space)
                || line
                    .tokens
                    .iter()
                    .any(|t| t.atomic.is_some() && !t.vertical_top);
            let line_above = line
                .tokens
                .iter()
                .map(|t| t.above)
                .fold(if has_text { strut.0 } else { 0.0 }, f32::max);
            let baseline = line.top + line_above;
            let spaces = line.tokens.iter().filter(|t| t.is_space).count();
            let justify = parent.text_align == TextAlign::Justify && line.justify && spaces > 0;
            let stretch = if justify { slack / spaces as f32 } else { 0.0 };
            let mut cursor = match parent.text_align {
                TextAlign::Center => line.left + slack / 2.0,
                TextAlign::Right => line.left + slack,
                _ => line.left,
            };

            for token in &line.tokens {
                if let Some(atomic) = token.atomic {
                    // An inline-block box: laid out as a block in its line slot, with
                    // its own baseline lined up with the line's.
                    let box_y = if token.vertical_top {
                        line.top
                    } else {
                        (baseline - (token.above - token.margin_top)).max(line.top)
                    };
                    self.layout_block(atomic, cursor, width, box_y, 0.0, true);
                    if let Some(rect) = self.out.rects.get(&atomic).copied() {
                        let entry = fragments.entry(atomic).or_insert(rect);
                        *entry = entry.union(&rect);
                    }
                    cursor += token.width;
                    continue;
                }

                if token.text.is_empty() {
                    // A spacing-only token (an inline element's margin): it moves the
                    // cursor and nothing else.
                    cursor += token.width + if token.is_space { stretch } else { 0.0 };
                    continue;
                }
                let content_h =
                    self.fonts.content_height(&token.family, token.font_weight, token.font_size);
                let text_top = baseline - token.above;
                let rect = Rect::new(cursor, text_top, token.width, line_height);
                let content_rect = Rect::new(
                    cursor,
                    text_top + (line_height - content_h).max(0.0) / 2.0,
                    token.width,
                    content_h,
                );
                cursor += token.width + if token.is_space { stretch } else { 0.0 };
                let entry = fragments.entry(token.node).or_insert(content_rect);
                *entry = entry.union(&content_rect);
                if let Some(owner) = token.owner {
                    let entry = fragments.entry(owner).or_insert(content_rect);
                    *entry = entry.union(&content_rect);
                }
                self.out.text.push(TextRun {
                    node: token.node,
                    text: token.text.clone(),
                    rect,
                    color: token.color,
                    font_size: token.font_size,
                    font_weight: token.font_weight,
                    italic: token.italic,
                    family: token.family.clone(),
                    underline: token.underline,
                });
            }

            self.out.lines.push(Line {
                owner,
                rect: Rect::new(line.left, line.top, line_width.max(0.0), line_height),
            });
            total += line_height;
        }

        for (id, rect) in fragments {
            self.out
                .rects
                .entry(id)
                .and_modify(|r| *r = r.union(&rect))
                .or_insert(rect);
        }

        total
    }

    /// The containing block's own line box: its half-leading, ascent and descent.
    fn strut_metrics(&mut self, style: &Style) -> (f32, f32) {
        let m = self.fonts.metrics(&style.font_family, style.font_weight, style.font_size);
        let content = (m.ascent - m.descent).max(0.0);
        let half = (style.line_height - content) / 2.0;
        (half + m.ascent, half - m.descent)
    }

    fn collect_tokens(
        &mut self,
        id: NodeId,
        inline_owner: Option<NodeId>,
        parent: &Style,
        cb_width: f32,
        out: &mut Vec<Token>,
    ) {
        let node = &self.doc.nodes[id];
        match node.kind {
            Kind::Text => {
                let text = node.text.clone();
                self.push_text_tokens(&text, id, inline_owner, parent, out);
            }
            Kind::Element => {
                let style = self.style(id).clone();
                if style.display == Display::None {
                    return;
                }
                if style.display == Display::InlineBlock {
                    // An atomic inline box: it is sized on its own and occupies one
                    // slot in the line.
                    let (w, h) = self.measure_box(id, cb_width);
                    let margin = resolve_edges(
                        style.margin,
                        style.font_size,
                        self.root_font_size,
                        cb_width,
                    );
                    let m = self.fonts.metrics(&style.font_family, style.font_weight, style.font_size);
                    let content = (m.ascent - m.descent).max(0.0);
                    let half = (style.line_height - content) / 2.0;
                    let offset = style.border_width.top
                        + resolve_len(style.padding.top, style.font_size, self.root_font_size, cb_width)
                            .unwrap_or(0.0)
                        + half
                        + m.ascent;
                    let box_h = h.max(style.line_height);
                    let margin_top =
                        resolve_len(style.margin.top, style.font_size, self.root_font_size, cb_width)
                            .unwrap_or(0.0);
                    let margin_bottom =
                        resolve_len(style.margin.bottom, style.font_size, self.root_font_size, cb_width)
                            .unwrap_or(0.0);
                    // Vertical margins of an inline-block grow the line box, which is
                    // why a tag with `margin-bottom` sits in a taller line.
                    let (above, below) = if style.vertical_align_top {
                        (box_h + margin_top + margin_bottom, 0.0)
                    } else {
                        (
                            (offset.min(box_h)) + margin_top,
                            (box_h - offset).max(0.0) + margin_bottom,
                        )
                    };
                    out.push(Token {
                        node: id,
                        above,
                        below,
                        margin_top,
                        underline: false,
                        owner: Some(id),
                        atomic: Some(id),
                        text: String::new(),
                        width: w + margin.left + margin.right,
                        line_height: h.max(style.line_height),
                        is_space: false,
                        color: style.color,
                        font_size: style.font_size,
                        font_weight: style.font_weight,
                        italic: style.font_style_italic,
                        family: style.font_family.clone(),
                        force_break: false,
                        vertical_top: style.vertical_align_top,
                    });
                    return;
                }
                // Inline elements get a box of their own; block-level ones are
                // handled by `layout_block`, which resets the owner to None.
                let owner = if style.display == Display::Inline {
                    Some(id)
                } else {
                    inline_owner
                };
                if style.display == Display::Inline {
                    let margin = resolve_edges(style.margin, style.font_size, self.root_font_size, cb_width);
                    if margin.left > 0.0 {
                        // The margin sits outside the element's border box, so the
                        // spacer must not widen the element's own fragment.
                        out.push(Token::spacer(id, None, margin.left));
                    }
                    let children: Vec<NodeId> = node.children.clone();
                    for child in &children {
                        self.collect_tokens(*child, owner, &style, cb_width, out);
                    }
                    if margin.right > 0.0 {
                        out.push(Token::spacer(id, None, margin.right));
                    }
                    return;
                }
                let children: Vec<NodeId> = node.children.clone();
                for child in &children {
                    self.collect_tokens(*child, owner, &style, cb_width, out);
                }
            }
            _ => {}
        }
    }

    fn push_text_tokens(
        &mut self,
        text: &str,
        id: NodeId,
        owner: Option<NodeId>,
        style: &Style,
        out: &mut Vec<Token>,
    ) {
        let preserve = matches!(style.white_space, WhiteSpace::Pre | WhiteSpace::PreWrap);
        let text = if style.text_transform_upper {
            text.to_uppercase()
        } else {
            text.to_string()
        };
        if text.is_empty() {
            return;
        }
        let line_height = style.line_height;
        let metrics = self.fonts.metrics(&style.font_family, style.font_weight, style.font_size);
        let content = (metrics.ascent - metrics.descent).max(0.0);
        // Half-leading is negative when the line is shorter than the font, which is
        // how `line-height: 12px` on 16px text still yields a 12px line box.
        let half_leading = (line_height - content) / 2.0;
        let above = half_leading + metrics.ascent;
        let below = half_leading + (metrics.descent * -1.0);

        let push = |piece: &str, is_space: bool, out: &mut Vec<Token>, ctx: &mut Self| {
            if piece.is_empty() {
                return;
            }
            let width = ctx
                .fonts
                .measure(&style.font_family, style.font_weight, style.font_size, piece);
            out.push(Token {
                node: id,
                above,
                below,
                margin_top: 0.0,
                underline: style.underline,
                owner,
                atomic: None,
                text: piece.to_string(),
                width,
                line_height,
                is_space,
                color: style.color,
                font_size: style.font_size,
                font_weight: style.font_weight,
                italic: style.font_style_italic,
                family: style.font_family.clone(),
                force_break: false,
                vertical_top: false,
            });
        };

        if preserve {
            for (i, line) in text.split('\n').enumerate() {
                if i > 0 {
                    let mut br = Token {
                        node: id,
                        above,
                        below,
                        margin_top: 0.0,
                        underline: false,
                        owner,
                        atomic: None,
                        text: String::new(),
                        width: 0.0,
                        line_height,
                        is_space: false,
                        color: style.color,
                        font_size: style.font_size,
                        font_weight: style.font_weight,
                        italic: style.font_style_italic,
                        family: style.font_family.clone(),
                        force_break: true,
                        vertical_top: false,
                    };
                    br.force_break = true;
                    out.push(br);
                }
                push(line, false, out, self);
            }
            return;
        }

        // Collapse runs of whitespace but keep the outer ones: a leading space still
        // separates this text node from the inline content before it. The line
        // builder is what drops spaces at the start and end of a line.
        let mut collapsed = String::with_capacity(text.len());
        let mut prev_space = false;
        for ch in text.chars() {
            if ch.is_ascii_whitespace() {
                if !prev_space {
                    collapsed.push(' ');
                    prev_space = true;
                }
            } else {
                collapsed.push(ch);
                prev_space = false;
            }
        }
        let parts: Vec<&str> = collapsed.split(' ').collect();
        for (i, word) in parts.iter().enumerate() {
            let previous_empty = i > 0 && parts[i - 1].is_empty();
            let leading = i == 0 && word.is_empty() && parts.len() > 1;
            if leading || (i > 0 && !previous_empty) {
                push(" ", true, out, self);
            }
            push(word, false, out, self);
        }
    }
}

#[derive(Clone)]
struct Token {
    /// The text node the characters came from.
    node: NodeId,
    /// Distance from the token's top edge to the baseline it aligns on, and from
    /// the baseline to its bottom edge. A line box is as tall as the tallest pair.
    above: f32,
    below: f32,
    /// An inline-block's top margin, which `above` includes but the box does not.
    margin_top: f32,
    underline: bool,
    /// The nearest inline element, which needs a fragment box.
    owner: Option<NodeId>,
    /// An inline-block box that is laid out as a block in its line slot.
    atomic: Option<NodeId>,
    text: String,
    width: f32,
    /// The token's line box height, kept for paint and for the record; vertical
    /// placement uses `above`/`below` instead.
    #[allow(dead_code)]
    line_height: f32,
    is_space: bool,
    color: Color,
    font_size: f32,
    font_weight: u16,
    italic: bool,
    family: String,
    force_break: bool,
    vertical_top: bool,
}

/// Split words that cannot fit on a line of `width` into character chunks, the way
/// `overflow-wrap: break-word` does.
fn break_long_words(tokens: Vec<Token>, width: f32, fonts: &mut FontContext) -> Vec<Token> {
    let mut out = Vec::new();
    for token in tokens {
        if token.is_space || token.width <= width || token.text.chars().count() < 2 {
            out.push(token);
            continue;
        }
        let mut piece = String::new();
        let mut piece_width = 0.0f32;
        for ch in token.text.chars() {
            let w = fonts.measure(&token.family, token.font_weight, token.font_size, &ch.to_string());
            if piece_width + w > width && !piece.is_empty() {
                let mut split = token.clone();
                split.text = std::mem::take(&mut piece);
                split.width = piece_width;
                out.push(split);
                piece_width = 0.0;
            }
            piece.push(ch);
            piece_width += w;
        }
        if !piece.is_empty() {
            let mut rest = token.clone();
            rest.text = piece;
            rest.width = piece_width;
            out.push(rest);
        }
    }
    out
}

/// Does a `clear` value ask to move below a float on this side?
fn clears(clear: Clear, side: Float) -> bool {
    matches!(
        (clear, side),
        (Clear::Both, _)
            | (Clear::Left, Float::Left)
            | (Clear::Right, Float::Right)
    )
}

/// The height of a line box: the tallest ascent above the baseline plus the tallest
/// descent below it. The containing block's strut always takes part, which is why a
/// line holding one short inline-block is still a full line tall.
fn line_height_of(tokens: &[Token], strut: (f32, f32)) -> f32 {
    // The strut takes part unless every box on the line is top-aligned: a line of
    // `vertical-align: top` inline-blocks is exactly as tall as those boxes, while a
    // baseline-aligned box still rides on the block's own strut.
    let has_text = tokens.iter().any(|t| !t.text.is_empty() && !t.is_space)
        || tokens
            .iter()
            .any(|t| t.atomic.is_some() && !t.vertical_top);
    let (start_above, start_below) = if has_text { strut } else { (0.0, 0.0) };
    let above = tokens.iter().map(|t| t.above).fold(start_above, f32::max);
    let below = tokens.iter().map(|t| t.below).fold(start_below, f32::max);
    (above + below).max(0.0)
}

impl Token {
    /// A zero-height box for the horizontal margin of an inline element.
    fn spacer(node: NodeId, owner: Option<NodeId>, width: f32) -> Token {
        Token {
            node,
            above: 0.0,
            below: 0.0,
            margin_top: 0.0,
            underline: false,
            owner,
            atomic: None,
            text: String::new(),
            width,
            line_height: 0.0,
            is_space: false,
            color: Color::BLACK,
            font_size: 0.0,
            font_weight: 400,
            italic: false,
            family: String::new(),
            force_break: false,
            vertical_top: false,
        }
    }
}

impl<'a> Ctx<'a> {
    /// An `img`'s intrinsic size, read from its `src` once. This is the one place
    /// layout touches the filesystem; the result is cached on the node.
    /// The `width`/`height` attributes of an inline `<svg>`, which is what a
    /// browser sizes it by. A percentage or a missing attribute leaves the
    /// default 300×150 replaced-element box alone.
    fn declared_size(&self, id: NodeId) -> Option<(f32, f32)> {
        let node = &self.doc.nodes[id];
        let num = |name: &str| -> Option<f32> {
            let raw = node.attr(name)?.trim().trim_end_matches("px").to_string();
            raw.parse::<f32>().ok().filter(|v| *v > 0.0)
        };
        match (num("width"), num("height")) {
            (Some(w), Some(h)) => Some((w, h)),
            _ => None,
        }
    }

    fn intrinsic_size(&mut self, id: NodeId) -> Option<(f32, f32)> {
        if let Some(cached) = self.image_sizes.get(&id) {
            return *cached;
        }
        let size = self
            .doc
            .nodes[id]
            .attr("src")
            .and_then(crate::image::intrinsic_size);
        self.image_sizes.insert(id, size);
        size
    }
}

/// Resolve one of `top`/`right`/`bottom`/`left` against a containing block edge.
fn offset_px(len: Length, font_size: f32, root: f32, base: f32) -> Option<f32> {
    match len {
        Length::Keyword | Length::Invalid => None,
        other => resolve_len(other, font_size, root, base),
    }
}
