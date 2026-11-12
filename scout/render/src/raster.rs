//! Glyph rasterisation.
//!
//! Outlines are read one glyph at a time and filled here, rather than by handing
//! the whole font file to a rasteriser that parses every outline it contains. A
//! text face costs ~19 MB that way and a CJK collection 314 MB, which a server
//! that renders pages for a living cannot pay per screenshot. One glyph's edges
//! are a few kilobytes.
//!
//! The fill is analytic in x and supersampled in y: each sub-scanline finds the
//! edges it crosses, sorts them, and adds exact horizontal coverage. That is
//! smooth enough to compare with a browser's antialiasing and cheap enough to do
//! for every glyph in a page.

use ttf_parser::{Face, GlyphId, OutlineBuilder};

/// Where a glyph's bitmap sits relative to the pen position and the baseline.
/// Device pixels, y up, the same convention as `fontdue::Metrics`.
#[derive(Debug, Clone, PartialEq)]
pub struct GlyphMetrics {
    /// Left edge, from the pen position.
    pub xmin: i32,
    /// Bottom edge, from the baseline (positive is up).
    pub ymin: i32,
    pub width: usize,
    pub height: usize,
    pub advance_width: f32,
}

/// Vertical samples per pixel row.
const SUB_ROWS: u32 = 8;

struct Edge {
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
}

/// Collects an outline as line segments in device pixels.
struct Flattener {
    edges: Vec<Edge>,
    start: (f32, f32),
    current: (f32, f32),
    scale: f32,
}

impl Flattener {
    fn point(&self, x: f32, y: f32) -> (f32, f32) {
        (x * self.scale, y * self.scale)
    }

    fn edge(&mut self, to: (f32, f32)) {
        if to != self.current {
            self.edges.push(Edge {
                x0: self.current.0,
                y0: self.current.1,
                x1: to.0,
                y1: to.1,
            });
            self.current = to;
        }
    }

    /// Segments for a curve, chosen from its length in device pixels so a large
    /// glyph is not drawn with a coarse polygon.
    fn steps(&self, length: f32) -> usize {
        ((length / 1.5).ceil() as usize).clamp(2, 24)
    }
}

impl OutlineBuilder for Flattener {
    fn move_to(&mut self, x: f32, y: f32) {
        self.finish();
        self.current = self.point(x, y);
        self.start = self.current;
    }

    fn line_to(&mut self, x: f32, y: f32) {
        let to = self.point(x, y);
        self.edge(to);
    }

    fn quad_to(&mut self, cx: f32, cy: f32, x: f32, y: f32) {
        let c = self.point(cx, cy);
        let to = self.point(x, y);
        let from = self.current;
        let length = (c.0 - from.0).hypot(c.1 - from.1) + (to.0 - c.0).hypot(to.1 - c.1);
        let n = self.steps(length);
        for i in 1..=n {
            let t = i as f32 / n as f32;
            let u = 1.0 - t;
            let p = (
                u * u * from.0 + 2.0 * u * t * c.0 + t * t * to.0,
                u * u * from.1 + 2.0 * u * t * c.1 + t * t * to.1,
            );
            self.edge(p);
        }
    }

    fn curve_to(&mut self, cx1: f32, cy1: f32, cx2: f32, cy2: f32, x: f32, y: f32) {
        let c1 = self.point(cx1, cy1);
        let c2 = self.point(cx2, cy2);
        let to = self.point(x, y);
        let from = self.current;
        let length = (c1.0 - from.0).hypot(c1.1 - from.1)
            + (c2.0 - c1.0).hypot(c2.1 - c1.1)
            + (to.0 - c2.0).hypot(to.1 - c2.1);
        let n = self.steps(length);
        for i in 1..=n {
            let t = i as f32 / n as f32;
            let u = 1.0 - t;
            let p = (
                u * u * u * from.0 + 3.0 * u * u * t * c1.0 + 3.0 * u * t * t * c2.0 + t * t * t * to.0,
                u * u * u * from.1 + 3.0 * u * u * t * c1.1 + 3.0 * u * t * t * c2.1 + t * t * t * to.1,
            );
            self.edge(p);
        }
    }

    fn close(&mut self) {
        self.finish();
    }
}

impl Flattener {
    /// Close the contour: an open outline is still a filled shape.
    fn finish(&mut self) {
        if self.current != self.start {
            let start = self.start;
            self.edge(start);
        }
    }
}

/// Rasterise one glyph to an 8-bit coverage bitmap.
///
/// `None` when the glyph has no outline (a space) or the font cannot draw it.
pub fn glyph(face: &Face, glyph: u16, px: f32) -> Option<(GlyphMetrics, Vec<u8>)> {
    let upem = face.units_per_em() as f32;
    if upem <= 0.0 || px <= 0.0 {
        return None;
    }
    let scale = px / upem;
    let mut flattener = Flattener {
        edges: Vec::new(),
        start: (0.0, 0.0),
        current: (0.0, 0.0),
        scale,
    };
    face.outline_glyph(GlyphId(glyph), &mut flattener)?;
    flattener.finish();
    let edges = flattener.edges;

    let (mut min_x, mut min_y) = (f32::MAX, f32::MAX);
    let (mut max_x, mut max_y) = (f32::MIN, f32::MIN);
    for edge in &edges {
        for (x, y) in [(edge.x0, edge.y0), (edge.x1, edge.y1)] {
            min_x = min_x.min(x);
            max_x = max_x.max(x);
            min_y = min_y.min(y);
            max_y = max_y.max(y);
        }
    }
    if min_x > max_x {
        return None;
    }
    let (x0, y0) = (min_x.floor(), min_y.floor());
    let (x1, y1) = (max_x.ceil(), max_y.ceil());
    let width = (x1 - x0) as i32;
    let height = (y1 - y0) as i32;
    if width <= 0 || height <= 0 {
        return None;
    }
    let (w, h) = (width as usize, height as usize);
    let mut coverage = vec![0f32; w * h];

    // Each sub-scanline contributes 1/SUB_ROWS of the row it lands in.
    let weight = 1.0 / SUB_ROWS as f32;
    let mut crossings: Vec<(f32, i32)> = Vec::new();
    for row in 0..h {
        // Outlines are y-up; a bitmap is stored top row first.
        let out_row = h - 1 - row;
        for sub in 0..SUB_ROWS {
            let y = y0 + row as f32 + (sub as f32 + 0.5) / SUB_ROWS as f32;
            crossings.clear();
            for edge in &edges {
                let (ya, yb) = (edge.y0.min(edge.y1), edge.y0.max(edge.y1));
                if y < ya || y >= yb {
                    continue;
                }
                if edge.y0 == edge.y1 {
                    continue;
                }
                let t = (y - edge.y0) / (edge.y1 - edge.y0);
                let x = edge.x0 + (edge.x1 - edge.x0) * t;
                crossings.push((x, if edge.y1 > edge.y0 { 1 } else { -1 }));
            }
            if crossings.len() < 2 {
                continue;
            }
            crossings.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
            let mut winding = 0;
            let mut span_start = 0.0f32;
            for &(x, direction) in &crossings {
                let was = winding;
                winding += direction;
                if was == 0 && winding != 0 {
                    span_start = x;
                } else if was != 0 && winding == 0 {
                    add_span(&mut coverage[out_row * w..(out_row + 1) * w], span_start, x, x0, weight);
                }
            }
        }
    }

    let mut bitmap = vec![0u8; w * h];
    for (out, cover) in bitmap.iter_mut().zip(coverage.iter()) {
        *out = (cover.clamp(0.0, 1.0) * 255.0).round() as u8;
    }

    let advance_width = face
        .glyph_hor_advance(GlyphId(glyph))
        .map(|units| units as f32 * scale)
        .unwrap_or(0.0);
    Some((
        GlyphMetrics {
            xmin: x0 as i32,
            ymin: y0 as i32,
            width: w,
            height: h,
            advance_width,
        },
        bitmap,
    ))
}

/// Add horizontal coverage for the span `[from, to]`, in device pixels, to one
/// row of the accumulation buffer. Partial pixels at each end get their exact
/// share, which is what keeps stems from looking either blurry or stepped.
fn add_span(row: &mut [f32], from: f32, to: f32, origin: f32, weight: f32) {
    let (a, b) = (from - origin, to - origin);
    if b <= a || a >= row.len() as f32 {
        return;
    }
    let a = a.max(0.0);
    let b = b.min(row.len() as f32);
    let first = a.floor() as usize;
    let last = (b.ceil() as usize).min(row.len());
    for (i, cell) in row[first..last].iter_mut().enumerate() {
        let index = first + i;
        let left = (index as f32).max(a);
        let right = ((index + 1) as f32).min(b);
        if right > left {
            *cell += (right - left) * weight;
        }
    }
}
