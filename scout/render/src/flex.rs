//! Flex containers, delegated to `taffy`.
//!
//! Taffy solves the flexible lengths; this module only translates our computed
//! style into taffy's, then hands each child's solved box back to the block
//! engine so its own contents are laid out by the same code as everything else.

use taffy::prelude::*;
use taffy::{AvailableSpace, Dimension, LengthPercentage, LengthPercentageAuto, TaffyTree};

use crate::css::value::Length;
use crate::dom::NodeId;
use crate::layout::{resolve_len, Ctx};
use crate::style::{AlignItems, BoxSizing, FlexDirection, JustifyContent, Style};

fn dim(len: Length, font_size: f32, root: f32, base: f32) -> Dimension {
    match len {
        Length::Percent(v) => Dimension::percent(v / 100.0),
        _ => match resolve_len(len, font_size, root, base) {
            Some(v) => Dimension::length(v),
            None => Dimension::auto(),
        },
    }
}

fn lpa(len: Length, font_size: f32, root: f32, base: f32) -> LengthPercentageAuto {
    match len {
        Length::Percent(v) => LengthPercentageAuto::percent(v / 100.0),
        _ => match resolve_len(len, font_size, root, base) {
            Some(v) => LengthPercentageAuto::length(v),
            None => LengthPercentageAuto::auto(),
        },
    }
}

fn lp(len: Length, font_size: f32, root: f32, base: f32) -> LengthPercentage {
    match len {
        Length::Percent(v) => LengthPercentage::percent(v / 100.0),
        _ => LengthPercentage::length(resolve_len(len, font_size, root, base).unwrap_or(0.0)),
    }
}

fn child_style(style: &Style) -> Style2 {
    let fs = style.font_size;
    let root = 16.0;
    let base = 0.0;
    let mut out = Style2::default();
    out.size = Size {
        width: dim(style.width, fs, root, base),
        height: dim(style.height, fs, root, base),
    };
    out.min_size = Size {
        width: lpa(style.min_width, fs, root, base),
        height: lpa(style.min_height, fs, root, base),
    };
    out.max_size = Size {
        width: lpa(style.max_width, fs, root, base),
        height: lpa(style.max_height, fs, root, base),
    };
    out.margin = Rect {
        left: lpa(style.margin.left, fs, root, base),
        right: lpa(style.margin.right, fs, root, base),
        top: lpa(style.margin.top, fs, root, base),
        bottom: lpa(style.margin.bottom, fs, root, base),
    };
    out.padding = Rect {
        left: lp(style.padding.left, fs, root, base),
        right: lp(style.padding.right, fs, root, base),
        top: lp(style.padding.top, fs, root, base),
        bottom: lp(style.padding.bottom, fs, root, base),
    };
    out.border = Rect {
        left: LengthPercentage::length(style.border_width.left),
        right: LengthPercentage::length(style.border_width.right),
        top: LengthPercentage::length(style.border_width.top),
        bottom: LengthPercentage::length(style.border_width.bottom),
    };
    out.flex_grow = style.flex_grow;
    out.flex_shrink = style.flex_shrink;
    out.flex_basis = match style.flex_basis {
        Length::Keyword => Dimension::auto(),
        other => dim(other, fs, root, base),
    };
    out.box_sizing = match style.box_sizing {
        BoxSizing::BorderBox => taffy::BoxSizing::BorderBox,
        BoxSizing::ContentBox => taffy::BoxSizing::ContentBox,
    };
    out
}

type Style2 = taffy::Style;

/// taffy takes `size` and `flex_basis` in the element's own box sizing, while the
/// engine measures border boxes; convert before handing a number over.
fn to_style_box(style: &Style, border_box: f32, horizontal: bool) -> f32 {
    if style.box_sizing == BoxSizing::BorderBox {
        return border_box;
    }
    let pad = if horizontal {
        style.padding.left.resolve(style.font_size, 16.0).unwrap_or(0.0)
            + style.padding.right.resolve(style.font_size, 16.0).unwrap_or(0.0)
            + style.border_width.left
            + style.border_width.right
    } else {
        style.padding.top.resolve(style.font_size, 16.0).unwrap_or(0.0)
            + style.padding.bottom.resolve(style.font_size, 16.0).unwrap_or(0.0)
            + style.border_width.top
            + style.border_width.bottom
    };
    (border_box - pad).max(0.0)
}

fn container_style(style: &Style, content_width: f32, content_height: Option<f32>) -> taffy::Style {
    let mut out = taffy::Style::default();
    out.display = taffy::Display::Flex;
    out.flex_direction = match style.flex_direction {
        FlexDirection::Row => taffy::FlexDirection::Row,
        FlexDirection::RowReverse => taffy::FlexDirection::RowReverse,
        FlexDirection::Column => taffy::FlexDirection::Column,
        FlexDirection::ColumnReverse => taffy::FlexDirection::ColumnReverse,
    };
    out.flex_wrap = if style.flex_wrap { taffy::FlexWrap::Wrap } else { taffy::FlexWrap::NoWrap };
    out.justify_content = match style.justify_content {
        JustifyContent::FlexStart => taffy::JustifyContent::FLEX_START,
        JustifyContent::Center => taffy::JustifyContent::CENTER,
        JustifyContent::FlexEnd => taffy::JustifyContent::FLEX_END,
        JustifyContent::SpaceBetween => taffy::JustifyContent::SPACE_BETWEEN,
        JustifyContent::SpaceAround => taffy::JustifyContent::SPACE_AROUND,
        JustifyContent::SpaceEvenly => taffy::JustifyContent::SPACE_EVENLY,
    };
    out.align_items = match style.align_items {
        AlignItems::Stretch => taffy::AlignItems::STRETCH,
        AlignItems::FlexStart => taffy::AlignItems::FLEX_START,
        AlignItems::Center => taffy::AlignItems::CENTER,
        AlignItems::FlexEnd => taffy::AlignItems::FLEX_END,
        AlignItems::Baseline => taffy::AlignItems::STRETCH,
    };
    out.gap = Size {
        width: LengthPercentage::length(style.column_gap),
        height: LengthPercentage::length(style.row_gap),
    };
    // A root node is sized from its style, not from the available space, so the
    // container's own width must be stated explicitly.
    out.size.width = Dimension::length(content_width);
    if let Some(h) = content_height {
        out.size.height = Dimension::length(h);
    }
    out
}

impl<'a> Ctx<'a> {
    /// Lay out a flex container's children; returns the height the flex layout used.
    pub(crate) fn layout_flex(
        &mut self,
        id: NodeId,
        content_x: f32,
        content_y: f32,
        content_width: f32,
        content_height: Option<f32>,
    ) -> f32 {
        let style = self.style(id).clone();
        let children: Vec<NodeId> = self.doc.nodes[id]
            .children
            .iter()
            .copied()
            .filter(|c| self.doc.nodes[*c].kind == crate::dom::Kind::Element)
            .filter(|c| self.style(*c).display != crate::style::Display::None)
            .collect();

        // Taffy solves numbers, not text: a leaf with an auto width has no content to
        // size itself from, so every automatic size is measured here first and fed
        // in as an explicit one. Two passes: the first sizes main axes from the
        // max-content width, the second re-measures heights at the widths that
        // actually came back.
        let horizontal = matches!(
            style.flex_direction,
            FlexDirection::Row | FlexDirection::RowReverse
        );
        let mut intrinsic: Vec<(f32, f32)> = Vec::new();
        for child in &children {
            let cs = self.style(*child).clone();
            let auto_width = matches!(cs.width, Length::Keyword | Length::Invalid);
            let w = if auto_width {
                self.max_content_width(*child, content_width).min(content_width)
            } else {
                self.measure_box(*child, content_width).0
            };
            let h = self.measure_box(*child, w.max(1.0)).1;
            intrinsic.push((w, h));
        }

        let mut tree: TaffyTree<()> = TaffyTree::new();
        let mut nodes = Vec::new();
        for (child, (w, h)) in children.iter().zip(intrinsic.iter()) {
            let cs = self.style(*child).clone();
            let mut s = child_style(&cs);
            let auto_width = matches!(cs.width, Length::Keyword | Length::Invalid);
            let auto_height = matches!(cs.height, Length::Keyword | Length::Invalid);
            // `flex-basis: auto` means the item's content size *along the main
            // axis*: the width in a row, the height in a column. Feeding a column
            // container a width as a basis would make every item as tall as it is
            // wide, which is exactly the shape of a broken column layout.
            if matches!(cs.flex_basis, Length::Keyword) {
                if horizontal && auto_width {
                    s.flex_basis = Dimension::length(to_style_box(&cs, *w, true));
                    // `min-width: auto` on a flex item floors it at min-content, which
                    // is what stops a row of text from wrapping into another item's space.
                    let min = self.min_content_width(*child, content_width);
                    s.min_size.width = LengthPercentageAuto::length(min);
                } else if !horizontal && auto_height {
                    s.flex_basis = Dimension::length(to_style_box(&cs, *h, false));
                }
            }
            // A leaf has no content for taffy to size it from, so its measured
            // height is always fed in. `align-items: stretch` then grows it to the
            // line's cross size afterwards, which is what a browser does — and
            // leaving the height auto would make the item zero-tall and collapse
            // the line it was supposed to define.
            // A stretched item in a container with a definite cross size keeps
            // `height: auto` here, so taffy stretches it to the line. Giving it an
            // explicit height would freeze it at its content size instead.
            let stretches = matches!(style.align_items, crate::style::AlignItems::Stretch);
            if auto_height && !(stretches && content_height.is_some()) {
                s.size.height = Dimension::length(to_style_box(&cs, *h, false));
            }
            if let Ok(n) = tree.new_leaf(s) {
                nodes.push(n);
            }
        }
        let root = match tree.new_with_children(container_style(&style, content_width, content_height), &nodes) {
            Ok(r) => r,
            Err(_) => return 0.0,
        };
        let available = Size {
            width: AvailableSpace::Definite(content_width),
            height: match content_height {
                Some(h) => AvailableSpace::Definite(h),
                None => AvailableSpace::MaxContent,
            },
        };
        if tree.compute_layout(root, available).is_err() {
            return 0.0;
        }

        // Re-measure each item at the width flex just gave it, then solve again so
        // a row of auto-height cards is as tall as its tallest card.
        let mut tallest = 0.0f32;
        if horizontal && content_height.is_none() {
            let stretches = matches!(style.align_items, crate::style::AlignItems::Stretch)
                && !style.flex_wrap;
            // First re-measure every auto-height item at the width flex gave it.
            let mut heights: Vec<Option<f32>> = vec![None; children.len()];
            for (i, child) in children.iter().enumerate() {
                let cs = self.style(*child).clone();
                if !matches!(cs.height, Length::Keyword | Length::Invalid) {
                    continue;
                }
                let Ok(l) = tree.layout(nodes[i]) else { continue };
                let width = l.size.width.max(1.0);
                let (_, h) = self.measure_box(*child, width);
                heights[i] = Some(h);
                tallest = tallest.max(h);
            }
            // On a single flex line every stretched item is as tall as the tallest,
            // which is what makes a row of cards line up along the bottom.
            let target = if stretches { tallest } else { 0.0 };
            let mut changed = false;
            for (i, child) in children.iter().enumerate() {
                let Some(h) = heights[i] else { continue };
                let cs = self.style(*child).clone();
                let Ok(l) = tree.layout(nodes[i]) else { continue };
                let height = if stretches { target } else { h };
                if (height - l.size.height).abs() > 0.01 {
                    changed = true;
                }
                let _ = tree.set_style(
                    nodes[i],
                    {
                        let mut s = child_style(&cs);
                        if matches!(cs.width, Length::Keyword | Length::Invalid)
                            && matches!(cs.flex_basis, Length::Keyword)
                        {
                            s.flex_basis = Dimension::length(to_style_box(&cs, intrinsic[i].0, true));
                            s.min_size.width =
                                LengthPercentageAuto::length(self.min_content_width(*child, content_width));
                        }
                        s.size.height = Dimension::length(to_style_box(&cs, height, false));
                        s
                    },
                );
            }
            if changed {
                let _ = tree.compute_layout(root, available);
            }
        }

        let container = match tree.layout(root) {
            Ok(l) => *l,
            Err(_) => return 0.0,
        };
        for (i, child) in children.iter().enumerate() {
            let Ok(l) = tree.layout(nodes[i]) else { continue };
            self.forced.insert(
                *child,
                (
                    content_x + l.location.x,
                    content_y + l.location.y,
                    l.size.width,
                    l.size.height,
                ),
            );
        }

        // Each child is laid out by the block engine at the size taffy solved.
        for child in &children {
            self.layout_block(*child, content_x, content_width, content_y, 0.0, true);
        }

        container.size.height
    }
}
