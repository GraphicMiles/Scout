//! Painting: geometry → pixels.
//!
//! The painter walks the document in tree order, which is also paint order for the
//! subset supported here (no stacking contexts, no z-index): a box paints its own
//! background and borders, then its children, then its text. Text is rasterised by
//! fontdue and composited by `tiny-skia`, so antialiasing matches the shapes a
//! browser draws rather than snapping to pixel grids.

use tiny_skia::{Color as SkColor, FillRule, Paint, PathBuilder, Pixmap, Rect as SkRect, Transform};

use crate::css::value::{self, Color, Corners};
use crate::dom::{Document, Kind, NodeId};
use crate::layout::{LayoutTree, Rect};
use crate::style::{ComputedStyles, Display, Position, Style};
use crate::text::FontContext;

/// Contrast applied to glyph coverage before compositing. 1.0 is linear; browsers
/// render text slightly darker than raw coverage, which this matches.

pub struct Options {
    pub width: u32,
    pub height: u32,
    /// Paint the canvas white first, like a screenshot of a page.
    pub background: Color,
}

impl Default for Options {
    fn default() -> Self {
        Options { width: 800, height: 600, background: Color::WHITE }
    }
}

pub struct Canvas {
    pub pixmap: Pixmap,
}

impl Canvas {
    pub fn width(&self) -> u32 {
        self.pixmap.width()
    }

    pub fn height(&self) -> u32 {
        self.pixmap.height()
    }

    /// PNG bytes, 8-bit RGBA.
    pub fn to_png(&self) -> Result<Vec<u8>, String> {
        let mut out = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut out, self.pixmap.width(), self.pixmap.height());
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
            // tiny-skia stores premultiplied RGBA; PNG wants straight alpha.
            let mut data = self.pixmap.data().to_vec();
            for px in data.chunks_exact_mut(4) {
                let a = px[3] as u32;
                if a != 0 && a != 255 {
                    for c in &mut px[..3] {
                        *c = ((*c as u32 * 255 + a / 2) / a).min(255) as u8;
                    }
                }
            }
            writer.write_image_data(&data).map_err(|e| e.to_string())?;
        }
        Ok(out)
    }
}

/// Paint a laid-out document.
pub fn paint(
    doc: &Document,
    styles: &ComputedStyles,
    layout: &LayoutTree,
    fonts: &mut FontContext,
    options: Options,
) -> Canvas {
    let mut pixmap = Pixmap::new(options.width.max(1), options.height.max(1))
        .unwrap_or_else(|| Pixmap::new(1, 1).unwrap());
    pixmap.fill(SkColor::from_rgba8(
        options.background.r,
        options.background.g,
        options.background.b,
        255,
    ));

    let mut painter = Painter { doc, styles, layout, fonts, pixmap };

    // The root's background propagates to the canvas; everything else paints in
    // tree order.
    if let Some(root) = doc.elements().into_iter().next() {
        painter.paint_backgrounds(root, true);
    }
    if let Some(body) = Some(doc.body()) {
        painter.paint_box(body, true);
    }
    painter.paint_text();
    painter.paint_markers();

    Canvas { pixmap: painter.pixmap }
}

struct Painter<'a> {
    doc: &'a Document,
    styles: &'a ComputedStyles,
    layout: &'a LayoutTree,
    fonts: &'a mut FontContext,
    pixmap: Pixmap,
}

impl<'a> Painter<'a> {
    fn style(&self, id: NodeId) -> Option<&Style> {
        self.styles.styles.get(&id)
    }

    fn visible(&self, id: NodeId) -> bool {
        match self.style(id) {
            Some(s) => s.display != Display::None,
            None => false,
        }
    }

    /// The page background comes from `html`, or from `body` when `html` has none.
    fn paint_backgrounds(&mut self, root: NodeId, top_level: bool) {
        if !top_level {
            return;
        }
        let html_bg = self.style(root).map(|s| s.background_color).unwrap_or(Color::TRANSPARENT);
        if html_bg.a > 0 {
            self.fill(Rect::new(0.0, 0.0, self.pixmap.width() as f32, self.pixmap.height() as f32), html_bg);
        }
    }

    fn paint_box(&mut self, id: NodeId, is_body: bool) {
        let Some(rect) = self.layout.rect(id) else {
            return;
        };
        let Some(style) = self.style(id).cloned() else {
            return;
        };
        if style.display == Display::None {
            return;
        }

        // A negative-z-index child paints behind its parent's own background,
        // which is what makes a "glow" or "backdrop" panel peek out from around
        // the box it belongs to.
        let mut behind: Vec<(i32, NodeId)> = Vec::new();
        for child in &self.doc.nodes[id].children {
            if self.doc.nodes[*child].kind != Kind::Element || !self.visible(*child) {
                continue;
            }
            if let Some(child_style) = self.style(*child) {
                if child_style.z_index < 0 && child_style.position != Position::Static {
                    behind.push((child_style.z_index, *child));
                }
            }
        }
        behind.sort_by_key(|(z, _)| *z);
        for (_, child) in behind {
            self.paint_box(child, false);
        }

        self.box_background(rect, &style, is_body);
        self.borders(rect, &style);
        self.painted_image(id, rect, &style);

        // Children paint in tree order, except positioned ones, which form a small
        // stacking order of their own: negative z-index first, then the rest in
        // z-index order. That is enough structure for the common sticky header and
        // dropdown without building real stacking contexts.
        let mut flow: Vec<NodeId> = Vec::new();
        let mut positioned: Vec<(i32, NodeId)> = Vec::new();
        // (collected before painting so negative-z children can go under the box)
        for child in &self.doc.nodes[id].children {
            if self.doc.nodes[*child].kind != Kind::Element || !self.visible(*child) {
                continue;
            }
            let child_style = self.style(*child);
            match child_style.map(|s| s.position) {
                Some(Position::Absolute) | Some(Position::Fixed) | Some(Position::Relative) => {
                    positioned.push((child_style.map(|s| s.z_index).unwrap_or(0), *child));
                }
                _ => flow.push(*child),
            }
        }
        // Negative z-index boxes paint behind everything in the flow; the rest
        // paint above it in z order. (A full stacking-context model would also
        // place them relative to the nearest context, but this covers the badge
        // and backdrop patterns pages actually use.)
        positioned.sort_by_key(|(z, _)| *z);
        for child in flow {
            self.paint_box(child, false);
        }
        for (z, child) in positioned.iter().filter(|(z, _)| *z >= 0) {
            let _ = z;
            self.paint_box(*child, false);
        }
    }

    /// Background colour, gradient, shadow and image for one box.
    fn box_background(&mut self, rect: Rect, style: &Style, is_body: bool) {
        let radius = self.radii(rect, style);

        if let Some(shadow) = style.shadow {
            if !shadow.inset {
                self.draw_shadow(rect, radius, shadow);
            }
        }

        // `body`'s background is promoted to the canvas when `html` has none.
        let body_promotes = is_body
            && self
                .doc
                .elements()
                .into_iter()
                .next()
                .and_then(|r| self.style(r))
                .map(|s| s.background_color.a == 0)
                .unwrap_or(true);
        if body_promotes {
            let canvas = Rect::new(0.0, 0.0, self.pixmap.width() as f32, self.pixmap.height() as f32);
            // The canvas starts from `html`'s background; `body`'s paints over it.
            // A page that inverts its colours puts the white on `html` and the dark
            // on `body`, so skipping the first one here would flood the frame.
            if let Some(root) = self.doc.elements().into_iter().next() {
                if let Some(root_style) = self.style(root).cloned() {
                    if root_style.background_color.a > 0 {
                        self.fill(canvas, root_style.background_color);
                    }
                }
            }
            if style.background_color.a > 0 {
                self.fill(canvas, style.background_color);
            }
            if let Some(image) = &style.background_image {
                self.background_image(canvas, image, radius);
            }
            return;
        }

        if style.background_color.a > 0 {
            self.fill_rounded(rect, radius, style.background_color);
        }
        if let Some(image) = &style.background_image {
            self.background_image(rect, image, radius);
        }
    }

    /// Corner radii clamped to the box, as CSS requires when they overlap.
    fn radii(&self, rect: Rect, style: &Style) -> Corners<f32> {
        let limit = rect.width.min(rect.height) / 2.0;
        let clamp = |v: f32| v.clamp(0.0, limit);
        Corners {
            top_left: clamp(style.border_radius.top_left),
            top_right: clamp(style.border_radius.top_right),
            bottom_right: clamp(style.border_radius.bottom_right),
            bottom_left: clamp(style.border_radius.bottom_left),
        }
    }

    /// A rounded rectangle path, or a plain one when every corner is square.
    fn rounded_path(rect: Rect, radius: Corners<f32>) -> Option<tiny_skia::Path> {
        if radius.uniform() && radius.top_left <= 0.0 {
            let mut pb = PathBuilder::new();
            pb.push_rect(SkRect::from_xywh(rect.x, rect.y, rect.width, rect.height)?);
            return pb.finish();
        }
        let mut pb = PathBuilder::new();
        let (x0, y0, x1, y1) = (rect.x, rect.y, rect.right(), rect.bottom());
        let (tl, tr, br, bl) = (
            radius.top_left,
            radius.top_right,
            radius.bottom_right,
            radius.bottom_left,
        );
        pb.move_to(x0 + tl, y0);
        pb.line_to(x1 - tr, y0);
        if tr > 0.0 {
            pb.quad_to(x1, y0, x1, y0 + tr);
        }
        pb.line_to(x1, y1 - br);
        if br > 0.0 {
            pb.quad_to(x1, y1, x1 - br, y1);
        }
        pb.line_to(x0 + bl, y1);
        if bl > 0.0 {
            pb.quad_to(x0, y1, x0, y1 - bl);
        }
        pb.line_to(x0, y0 + tl);
        if tl > 0.0 {
            pb.quad_to(x0, y0, x0 + tl, y0);
        }
        pb.close();
        pb.finish()
    }

    fn fill_rounded(&mut self, rect: Rect, radius: Corners<f32>, colour: Color) {
        if rect.width <= 0.0 || rect.height <= 0.0 || colour.a == 0 {
            return;
        }
        if radius.uniform() && radius.top_left <= 0.0 {
            self.fill(rect, colour);
            return;
        }
        let Some(path) = Self::rounded_path(rect, radius) else { return };
        let mut paint = Paint::default();
        paint.set_color(SkColor::from_rgba8(colour.r, colour.g, colour.b, colour.a));
        paint.anti_alias = true;
        self.pixmap
            .fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);
    }

    /// `box-shadow`: a blurred, offset copy of the box's rounded outline.
    fn draw_shadow(&mut self, rect: Rect, radius: Corners<f32>, shadow: value::Shadow) {
        let spread = shadow.spread;
        let shadow_rect = Rect::new(
            rect.x + shadow.x - spread,
            rect.y + shadow.y - spread,
            rect.width + spread * 2.0,
            rect.height + spread * 2.0,
        );
        if shadow_rect.width <= 0.0 || shadow_rect.height <= 0.0 {
            return;
        }
        let grow = |r: f32| (r + spread).max(0.0);
        let shadow_radius = Corners {
            top_left: grow(radius.top_left),
            top_right: grow(radius.top_right),
            bottom_right: grow(radius.bottom_right),
            bottom_left: grow(radius.bottom_left),
        };

        if shadow.blur <= 0.5 {
            self.fill_rounded(shadow_rect, shadow_radius, shadow.color);
            return;
        }

        // Draw the outline into an offscreen mask and blur it. Three box passes
        // approximate a Gaussian closely enough at the radii pages actually use.
        let pad = shadow.blur.ceil() as i64 + 2;
        let w = (shadow_rect.width.ceil() as i64 + pad * 2).clamp(1, 4096) as u32;
        let h = (shadow_rect.height.ceil() as i64 + pad * 2).clamp(1, 4096) as u32;
        let Some(mut mask) = Pixmap::new(w, h) else { return };
        let local = Rect::new(
            shadow_rect.x - shadow_rect.x.floor() + pad as f32,
            shadow_rect.y - shadow_rect.y.floor() + pad as f32,
            shadow_rect.width,
            shadow_rect.height,
        );
        let Some(path) = Self::rounded_path(local, shadow_radius) else { return };
        let mut paint = Paint::default();
        paint.set_color(SkColor::from_rgba8(255, 255, 255, 255));
        paint.anti_alias = true;
        mask.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);

        blur_alpha(&mut mask, (shadow.blur / 2.0).round() as usize);

        let origin_x = (shadow_rect.x.floor() - pad as f32).round() as i64;
        let origin_y = (shadow_rect.y.floor() - pad as f32).round() as i64;
        self.composite(&mask, origin_x, origin_y, shadow.color);
    }

    /// `background-image`: a solid gradient or a bitmap, painted inside the box.
    fn background_image(&mut self, rect: Rect, image: &value::BackgroundImage, radius: Corners<f32>) {
        match image {
            value::BackgroundImage::LinearGradient { angle, stops } => {
                self.gradient(rect, radius, *angle, stops);
            }
            value::BackgroundImage::Url(src) => {
                let target = (rect.width.max(1.0) as u32, rect.height.max(1.0) as u32);
                if let Some(bitmap) = crate::image::decode_for(src, Some(target)) {
                    self.draw_bitmap(&bitmap, rect, 1.0);
                }
            }
        }
    }

    fn gradient(&mut self, rect: Rect, radius: Corners<f32>, angle: f32, stops: &[(f32, Color)]) {
        if rect.width <= 0.0 || rect.height <= 0.0 || stops.len() < 2 {
            return;
        }
        // CSS angles run clockwise from "to top"; convert to a direction vector and
        // the line length that covers the box (the CSS gradient line rule).
        let radians = (angle - 90.0).to_radians();
        let (dx, dy) = (radians.cos(), radians.sin());
        let half = (rect.width * dx.abs() + rect.height * dy.abs()) / 2.0;
        let cx = rect.x + rect.width / 2.0;
        let cy = rect.y + rect.height / 2.0;
        let (x0, y0) = (cx - dx * half, cy - dy * half);
        let (x1, y1) = (cx + dx * half, cy + dy * half);

        let mut builder = tiny_skia::LinearGradient::new(
            tiny_skia::Point::from_xy(x0, y0),
            tiny_skia::Point::from_xy(x1, y1),
            stops
                .iter()
                .map(|(position, colour)| tiny_skia::GradientStop::new(
                    (*position / 100.0).clamp(0.0, 1.0),
                    SkColor::from_rgba8(colour.r, colour.g, colour.b, colour.a),
                ))
                .collect(),
            tiny_skia::SpreadMode::Pad,
            Transform::identity(),
        );
        let Some(shader) = builder.take() else { return };
        let Some(path) = Self::rounded_path(rect, radius) else { return };
        let paint = Paint {
            shader,
            anti_alias: true,
            ..Default::default()
        };
        self.pixmap
            .fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);
    }

    /// An `img` element's pixels, drawn into its content box.
    fn painted_image(&mut self, id: NodeId, rect: Rect, style: &Style) {
        let node = &self.doc.nodes[id];
        if node.name == "svg" {
            if let Some(bitmap) = crate::image::inline_svg(&crate::image::markup(self.doc, id)) {
                let inner = self.content_box(rect, style);
                self.draw_bitmap(&bitmap, inner, 1.0);
            }
            return;
        }
        if node.name != "img" {
            return;
        }
        let Some(src) = node.attr("src") else { return };
        let inner = self.content_box(rect, style);
        let target = (inner.width.max(1.0) as u32, inner.height.max(1.0) as u32);
        let Some(bitmap) = crate::image::decode_for(src, Some(target)) else { return };
        self.draw_bitmap(&bitmap, inner, 1.0);
    }

    /// The padding box of a border box: where an `img` or an inline `<svg>`
    /// paints, inside the border and the padding.
    fn content_box(&self, rect: Rect, style: &Style) -> Rect {
        let padding = crate::layout::resolve_edges(
            style.padding,
            style.font_size,
            crate::text::root_font_size(),
            rect.width,
        );
        let border = style.border_width;
        Rect::new(
            rect.x + border.left + padding.left,
            rect.y + border.top + padding.top,
            (rect.width - border.left - border.right - padding.left - padding.right).max(0.0),
            (rect.height - border.top - border.bottom - padding.top - padding.bottom).max(0.0),
        )
    }

    /// Scale a bitmap into `rect`, sampling bilinearly.
    fn draw_bitmap(&mut self, bitmap: &crate::image::Bitmap, rect: Rect, opacity: f32) {
        if rect.width <= 0.0 || rect.height <= 0.0 {
            return;
        }
        let x0 = rect.x.floor().max(0.0) as u32;
        let y0 = rect.y.floor().max(0.0) as u32;
        let x1 = (rect.right().ceil() as i64).clamp(0, self.pixmap.width() as i64) as u32;
        let y1 = (rect.bottom().ceil() as i64).clamp(0, self.pixmap.height() as i64) as u32;
        if x1 <= x0 || y1 <= y0 {
            return;
        }
        let canvas_w = self.pixmap.width();
        for y in y0..y1 {
            let v = (y as f32 + 0.5 - rect.y) / rect.height;
            for x in x0..x1 {
                let u = (x as f32 + 0.5 - rect.x) / rect.width;
                let Some(px) = bitmap.sample(u, v) else { continue };
                let alpha = (px[3] as f32 * opacity).round() as u32;
                if alpha == 0 {
                    continue;
                }
                let idx = ((y * canvas_w + x) * 4) as usize;
                let data = self.pixmap.data_mut();
                let inv = 255 - alpha;
                for (i, c) in [px[0], px[1], px[2]].iter().enumerate() {
                    data[idx + i] = ((*c as u32 * alpha + data[idx + i] as u32 * inv) / 255) as u8;
                }
                data[idx + 3] = (alpha + data[idx + 3] as u32 * inv / 255).min(255) as u8;
            }
        }
    }

    /// Copy an offscreen mask onto the canvas in a solid colour.
    fn composite(&mut self, mask: &Pixmap, origin_x: i64, origin_y: i64, colour: Color) {
        let mw = mask.width() as i64;
        let mh = mask.height() as i64;
        let cw = self.pixmap.width() as i64;
        let ch = self.pixmap.height() as i64;
        let mask_data = mask.data().to_vec();
        for y in 0..mh {
            let dy = origin_y + y;
            if dy < 0 || dy >= ch {
                continue;
            }
            for x in 0..mw {
                let dx = origin_x + x;
                if dx < 0 || dx >= cw {
                    continue;
                }
                let a = mask_data[((y * mw + x) * 4) as usize + 3] as u32 * colour.a as u32 / 255;
                if a == 0 {
                    continue;
                }
                let idx = ((dy * cw + dx) * 4) as usize;
                let data = self.pixmap.data_mut();
                let inv = 255 - a;
                for (i, c) in [colour.r, colour.g, colour.b].iter().enumerate() {
                    data[idx + i] = ((*c as u32 * a + data[idx + i] as u32 * inv) / 255) as u8;
                }
                data[idx + 3] = (a + data[idx + 3] as u32 * inv / 255).min(255) as u8;
            }
        }
    }

    fn borders(&mut self, rect: Rect, style: &Style) {
        let b = style.border_width;
        if b.top <= 0.0 && b.right <= 0.0 && b.bottom <= 0.0 && b.left <= 0.0 {
            return;
        }
        let colour = style.border_color;
        let radius = self.radii(rect, style);
        if radius.top_left > 0.0 || radius.top_right > 0.0 || radius.bottom_right > 0.0 || radius.bottom_left > 0.0 {
            // A rounded border is the ring between the outer and inner rounded
            // rectangles: fill the box, then clear the middle back out.
            self.fill_rounded(rect, radius, colour);
            let inner = Rect::new(
                rect.x + b.left,
                rect.y + b.top,
                (rect.width - b.left - b.right).max(0.0),
                (rect.height - b.top - b.bottom).max(0.0),
            );
            let inner_radius = Corners {
                top_left: (radius.top_left - b.top).max(0.0),
                top_right: (radius.top_right - b.top).max(0.0),
                bottom_right: (radius.bottom_right - b.bottom).max(0.0),
                bottom_left: (radius.bottom_left - b.bottom).max(0.0),
            };
            self.erase_rounded(inner, inner_radius);
            return;
        }
        if b.top > 0.0 {
            self.fill(Rect::new(rect.x, rect.y, rect.width, b.top), colour);
        }
        if b.bottom > 0.0 {
            self.fill(
                Rect::new(rect.x, rect.bottom() - b.bottom, rect.width, b.bottom),
                colour,
            );
        }
        if b.left > 0.0 {
            self.fill(
                Rect::new(rect.x, rect.y + b.top, b.left, rect.height - b.top - b.bottom),
                colour,
            );
        }
        if b.right > 0.0 {
            self.fill(
                Rect::new(rect.right() - b.right, rect.y + b.top, b.right, rect.height - b.top - b.bottom),
                colour,
            );
        }
    }

    /// Clear a rounded area back to transparent, used to hollow out a border.
    fn erase_rounded(&mut self, rect: Rect, radius: Corners<f32>) {
        if rect.width <= 0.0 || rect.height <= 0.0 {
            return;
        }
        let Some(path) = Self::rounded_path(rect, radius) else { return };
        let mut paint = Paint::default();
        paint.set_color(SkColor::from_rgba8(0, 0, 0, 0));
        paint.blend_mode = tiny_skia::BlendMode::Clear;
        paint.anti_alias = true;
        self.pixmap
            .fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);
    }

    fn fill(&mut self, rect: Rect, colour: Color) {
        if rect.width <= 0.0 || rect.height <= 0.0 || colour.a == 0 {
            return;
        }
        let Some(sk) = SkRect::from_xywh(rect.x, rect.y, rect.width, rect.height) else {
            return;
        };
        let mut paint = Paint::default();
        paint.set_color(SkColor::from_rgba8(
            colour.r,
            colour.g,
            colour.b,
            colour.a,
        ));
        paint.anti_alias = false;
        self.pixmap.fill_rect(sk, &paint, Transform::identity(), None);
    }

    /// Draw every text run, glyph by glyph, with the font's own kerning.
    fn paint_text(&mut self) {
        for run in &self.layout.text {
            if run.text.trim().is_empty() {
                continue;
            }
            self.draw_run(&run.family, run.font_weight, run.font_size, &run.text, run.rect.x, run.rect.y, run.rect.height, run.color);
            if run.underline {
                // Underlines sit just below the baseline and are as thick as the
                // font's stroke, which for a 16px face is one pixel.
                let metrics = self.fonts.metrics(&run.family, run.font_weight, run.font_size);
                let content_h = (metrics.ascent - metrics.descent).max(0.0);
                let baseline =
                    run.rect.y + (run.rect.height - content_h).max(0.0) / 2.0 + metrics.ascent;
                let thickness = (run.font_size / 16.0).max(1.0).round();
                let width = self
                    .fonts
                    .measure(&run.family, run.font_weight, run.font_size, &run.text);
                self.fill(
                    Rect::new(run.rect.x, baseline + run.font_size * 0.12, width, thickness),
                    run.color,
                );
            }
        }
    }

    fn draw_run(
        &mut self,
        family: &str,
        weight: u16,
        size: f32,
        text: &str,
        x: f32,
        line_top: f32,
        line_height: f32,
        colour: Color,
    ) {
        let metrics = self.fonts.metrics(family, weight, size);
        let content_h = (metrics.ascent - metrics.descent).max(0.0);
        let baseline = line_top + (line_height - content_h).max(0.0) / 2.0 + metrics.ascent;

        let glyphs = self.fonts.layout_text(family, weight, size, text);
        for (index, offset, face) in glyphs {
            let pen_x = x + offset;
            // A browser positions glyphs at fractional x. Rasterising at the nearest
            // whole pixel and resampling the bitmap by the remainder reproduces that
            // to within a fraction of a pixel, which is what makes text line up.
            let snapped = pen_x.round();
            let fraction = pen_x - snapped;
            let Some((m, bitmap)) = self.fonts.rasterize_glyph_of(face, index, size) else {
                continue;
            };
            if m.width == 0 || m.height == 0 {
                continue;
            }
            let gx = (snapped + m.xmin as f32).round() as i64;
            // Skia snaps the baseline to the device grid and then positions the
            // glyph's top edge on it; rounding the top edge instead would move a
            // whole pixel of every glyph that lands in the upper half of its cell.
            let gy = (baseline - (m.ymin + m.height as i32) as f32).floor() as i64;
            let Some(size_px) = tiny_skia::IntSize::from_wh(m.width as u32, m.height as u32) else {
                continue;
            };
            self.blit_shifted(size_px, &bitmap, gx, gy, fraction, colour);
        }
    }

    /// Composite a coverage bitmap, sampling it `shift` pixels to the right when
    /// `shift` is positive. Linear interpolation between two columns is enough: the
    /// shift never exceeds half a pixel.
    fn blit_shifted(
        &mut self,
        size: tiny_skia::IntSize,
        bitmap: &[u8],
        ox: i64,
        oy: i64,
        shift: f32,
        colour: Color,
    ) {
        if shift.abs() < 0.02 {
            self.blit(size, bitmap, ox, oy, colour);
            return;
        }
        let w = size.width() as usize;
        let h = size.height() as usize;
        if w == 0 || h == 0 {
            return;
        }
        // Sample at column - shift: shifting right means reading a column further left.
        let mut resampled = vec![0u8; w * h];
        for row in 0..h {
            for col in 0..w {
                let source = col as f32 - shift;
                let left = source.floor();
                let weight = source - left;
                let a = |i: i64| -> f32 {
                    if i < 0 || i >= w as i64 {
                        0.0
                    } else {
                        bitmap[row * w + i as usize] as f32
                    }
                };
                resampled[row * w + col] =
                    (a(left as i64) * (1.0 - weight) + a(left as i64 + 1) * weight).round() as u8;
            }
        }
        self.blit(size, &resampled, ox, oy, colour);
    }

    fn blit(&mut self, size: tiny_skia::IntSize, bitmap: &[u8], ox: i64, oy: i64, colour: Color) {
        let w = size.width() as usize;
        let h = size.height() as usize;
        if w == 0 || h == 0 {
            return;
        }
        let canvas_w = self.pixmap.width() as i64;
        let canvas_h = self.pixmap.height() as i64;
        let data = self.pixmap.data_mut();
        for row in 0..h {
            let py = oy + row as i64;
            if py < 0 || py >= canvas_h {
                continue;
            }
            for col in 0..w {
                let px = ox + col as i64;
                if px < 0 || px >= canvas_w {
                    continue;
                }
                // Straight coverage. A contrast curve was measured and made no
                // difference (3.683 → 3.671 summed SSIM over the text fixtures), so
                // the residual is the rasteriser's edge shape, not its darkness.
                let a = bitmap[row * w + col] as u32 * colour.a as u32 / 255;
                if a == 0 {
                    continue;
                }
                let idx = ((py * canvas_w + px) * 4) as usize;
                let inv = 255 - a;
                let dst = &mut data[idx..idx + 4];
                // Blend in premultiplied space, which is what tiny-skia stores.
                for (i, c) in [colour.r, colour.g, colour.b].iter().enumerate() {
                    dst[i] = ((*c as u32 * a + dst[i] as u32 * inv) / 255) as u8;
                }
                dst[3] = (a + dst[3] as u32 * inv / 255).min(255) as u8;
            }
        }
    }

    /// List markers: a bullet to the left of each `list-item` box.
    fn paint_markers(&mut self) {
        let items: Vec<(Rect, Style)> = self
            .doc
            .elements()
            .into_iter()
            .filter(|id| self.style(*id).map(|s| s.display == Display::ListItem).unwrap_or(false))
            .filter_map(|id| Some((self.layout.rect(id)?, self.style(id)?.clone())))
            .collect();

        for (rect, style) in items {
            let width = self.fonts.measure(&style.font_family, style.font_weight, style.font_size, "\u{2022}");
            let x = rect.x - width - 7.0;
            self.draw_run(
                &style.font_family,
                style.font_weight,
                style.font_size,
                "\u{2022}",
                x,
                rect.y,
                style.line_height,
                style.color,
            );
        }
    }
}

/// Build a simple path, used by future shapes (rounded corners, clip paths).
#[allow(dead_code)]
fn rect_path(rect: Rect) -> Option<tiny_skia::Path> {
    let mut pb = PathBuilder::new();
    pb.push_rect(SkRect::from_xywh(rect.x, rect.y, rect.width, rect.height)?);
    pb.finish()
}

#[allow(dead_code)]
fn fill_path(pixmap: &mut Pixmap, path: &tiny_skia::Path, colour: Color) {
    let mut paint = Paint::default();
    paint.set_color(SkColor::from_rgba8(colour.r, colour.g, colour.b, colour.a));
    pixmap.fill_path(path, &paint, FillRule::Winding, Transform::identity(), None);
}

/// Separable box blur over the alpha channel, so a shadow gets a soft edge without
/// pulling in an image-processing dependency.
fn blur_alpha(pixmap: &mut Pixmap, radius: usize) {
    if radius == 0 {
        return;
    }
    let w = pixmap.width() as usize;
    let h = pixmap.height() as usize;
    let data = pixmap.data_mut();
    let mut alpha: Vec<f32> = (0..w * h).map(|i| data[i * 4 + 3] as f32).collect();
    let mut scratch = vec![0.0f32; w * h];

    for _ in 0..3 {
        // Horizontal pass.
        for y in 0..h {
            let row = y * w;
            let mut sum = 0.0;
            let mut count = 0.0;
            for x in 0..(radius + 1).min(w) {
                sum += alpha[row + x];
                count += 1.0;
            }
            for x in 0..w {
                scratch[row + x] = sum / count;
                let out = x as i64 - radius as i64;
                let inx = x + radius + 1;
                if out >= 0 {
                    sum -= alpha[row + out as usize];
                    count -= 1.0;
                }
                if inx < w {
                    sum += alpha[row + inx];
                    count += 1.0;
                }
            }
        }
        // Vertical pass.
        for x in 0..w {
            let mut sum = 0.0;
            let mut count = 0.0;
            for y in 0..(radius + 1).min(h) {
                sum += scratch[y * w + x];
                count += 1.0;
            }
            for y in 0..h {
                alpha[y * w + x] = sum / count;
                let out = y as i64 - radius as i64;
                let iny = y + radius + 1;
                if out >= 0 {
                    sum -= scratch[out as usize * w + x];
                    count -= 1.0;
                }
                if iny < h {
                    sum += scratch[iny * w + x];
                    count += 1.0;
                }
            }
        }
    }

    for i in 0..w * h {
        let a = alpha[i].clamp(0.0, 255.0).round() as u8;
        // Premultiplied, as tiny-skia stores: white at full coverage.
        let idx = i * 4;
        data[idx] = a;
        data[idx + 1] = a;
        data[idx + 2] = a;
        data[idx + 3] = a;
    }
}
