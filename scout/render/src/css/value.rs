//! Typed CSS values: lengths, percentages and colours.
//!
//! Lengths keep their units until computed-value time, because whether `em` means
//! the element's own font size or its parent's depends on the property.

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Length {
    Px(f32),
    Em(f32),
    Rem(f32),
    Percent(f32),
    /// The width of the font's `0`, resolved against the current face.
    Ch(f32),
    /// Viewport-relative units; the layout stage supplies the viewport size.
    Vw(f32),
    Vh(f32),
    VMin(f32),
    VMax(f32),
    /// `auto`, `none`, `normal` — the property decides what it means.
    Keyword,
    /// A value the engine does not understand; treated as unset and reported.
    Invalid,
}

impl Default for Length {
    fn default() -> Self {
        Length::Keyword
    }
}

impl Length {
    pub fn resolve(&self, font_size: f32, root_font_size: f32) -> Option<f32> {
        match *self {
            Length::Px(v) => Some(v),
            Length::Em(v) => Some(v * font_size),
            Length::Rem(v) => Some(v * root_font_size),
            _ => None,
        }
    }
}

pub type Percentage = Length;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color {
    pub const BLACK: Color = Color { r: 0, g: 0, b: 0, a: 255 };
    pub const WHITE: Color = Color { r: 255, g: 255, b: 255, a: 255 };
    pub const TRANSPARENT: Color = Color { r: 0, g: 0, b: 0, a: 0 };

    pub fn rgb(r: u8, g: u8, b: u8) -> Color {
        Color { r, g, b, a: 255 }
    }

    pub fn to_css(&self) -> String {
        if self.a == 255 {
            format!("rgb({}, {}, {})", self.r, self.g, self.b)
        } else {
            let alpha = (self.a as f32 / 255.0 * 1000.0).round() / 1000.0;
            format!("rgba({}, {}, {}, {})", self.r, self.g, self.b, alpha)
        }
    }

    pub fn is_transparent(&self) -> bool {
        self.a == 0
    }
}

/// Parse a px/em/rem/%/unitless length. `0` is a length with no unit.
pub fn parse_length(input: &str) -> Length {
    let s = input.trim().to_ascii_lowercase();
    if s.is_empty() {
        return Length::Invalid;
    }
    if let Some(v) = parse_px(&s) {
        return Length::Px(v);
    }
    if let Some(rest) = s.strip_suffix("em") {
        if let Ok(v) = rest.trim().parse::<f32>() {
            return Length::Em(v);
        }
    }
    if let Some(rest) = s.strip_suffix("rem") {
        if let Ok(v) = rest.trim().parse::<f32>() {
            return Length::Rem(v);
        }
    }
    if let Some(rest) = s.strip_suffix('%') {
        if let Ok(v) = rest.trim().parse::<f32>() {
            return Length::Percent(v);
        }
    }
    if let Some(rest) = s.strip_suffix("ch") {
        if let Ok(v) = rest.trim().parse::<f32>() {
            return Length::Ch(v);
        }
    }
    // Physical units, at the CSS 96 dpi reference: all the old web used `pt`.
    for (suffix, factor) in [
        ("pt", 96.0 / 72.0),
        ("pc", 16.0),
        ("mm", 96.0 / 25.4),
        ("cm", 96.0 / 2.54),
        ("in", 96.0),
        ("q", 96.0 / 101.6),
    ] {
        if let Some(rest) = s.strip_suffix(suffix) {
            if let Ok(v) = rest.trim().parse::<f32>() {
                return Length::Px(v * factor);
            }
        }
    }

    // Viewport units. `vmin`/`vmax` need the viewport at resolution time.
    for (suffix, unit) in [
        ("vmin", Length::VMin as fn(f32) -> Length),
        ("vmax", (|v| Length::VMax(v)) as fn(f32) -> Length),
        ("vw", (|v| Length::Vw(v)) as fn(f32) -> Length),
        ("vh", (|v| Length::Vh(v)) as fn(f32) -> Length),
    ] {
        if let Some(rest) = s.strip_suffix(suffix) {
            if let Ok(v) = rest.trim().parse::<f32>() {
                return unit(v);
            }
        }
    }
    if s.ends_with("px") {
        return Length::Invalid;
    }
    // A unitless non-zero number is not a length.
    match s.parse::<f32>() {
        Ok(0.0) => Length::Px(0.0),
        _ => Length::Keyword,
    }
}

/// A number of pixels: the only unit layout arithmetic uses.
pub fn parse_px(input: &str) -> Option<f32> {
    let s = input.trim().to_ascii_lowercase();
    if let Some(rest) = s.strip_suffix("px") {
        return rest.trim().parse::<f32>().ok();
    }
    if s == "0" {
        return Some(0.0);
    }
    None
}

/// A length in a media query, where `em` and `rem` are both relative to the
/// initial font size (16 px) rather than to any element — the spec's rule, and
/// what makes Tachyons-style `@media (min-width: 60em)` breakpoints work.
pub fn parse_media_length(input: &str) -> Option<f32> {
    let s = input.trim().to_ascii_lowercase();
    const INITIAL: f32 = 16.0;
    for (unit, scale) in [("px", 1.0), ("em", INITIAL), ("rem", INITIAL), ("in", 96.0), ("pt", 4.0 / 3.0), ("cm", 96.0 / 2.54), ("mm", 96.0 / 25.4), ("q", 96.0 / 101.6)] {
        if let Some(rest) = s.strip_suffix(unit) {
            // `rem` must not be eaten by `em`; the longest suffix wins because
            // it is matched first in this list.
            return rest.trim().parse::<f32>().ok().map(|v| v * scale);
        }
    }
    if s == "0" {
        return Some(0.0);
    }
    None
}

/// A plain number, for `line-height: 1.5` and `font-weight: 700`.
pub fn parse_number(input: &str) -> Option<f32> {
    input.trim().parse::<f32>().ok()
}

pub fn parse_keyword(input: &str) -> String {
    input.trim().to_ascii_lowercase()
}

pub fn parse_color(input: &str) -> Option<Color> {
    let s = input.trim().to_ascii_lowercase();
    if s.is_empty() {
        return None;
    }
    if let Some(hex) = s.strip_prefix('#') {
        if !hex.is_ascii() {
            return None;
        }
        return match hex.len() {
            3 => {
                let r = u8::from_str_radix(&hex[0..1].repeat(2), 16).ok()?;
                let g = u8::from_str_radix(&hex[1..2].repeat(2), 16).ok()?;
                let b = u8::from_str_radix(&hex[2..3].repeat(2), 16).ok()?;
                Some(Color::rgb(r, g, b))
            }
            4 => {
                let r = u8::from_str_radix(&hex[0..1].repeat(2), 16).ok()?;
                let g = u8::from_str_radix(&hex[1..2].repeat(2), 16).ok()?;
                let b = u8::from_str_radix(&hex[2..3].repeat(2), 16).ok()?;
                let a = u8::from_str_radix(&hex[3..4].repeat(2), 16).ok()?;
                Some(Color { r, g, b, a })
            }
            6 => {
                let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
                let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
                let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
                Some(Color::rgb(r, g, b))
            }
            8 => {
                let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
                let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
                let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
                let a = u8::from_str_radix(&hex[6..8], 16).ok()?;
                Some(Color { r, g, b, a })
            }
            _ => None,
        };
    }
    if s.starts_with("rgb") {
        let open = s.find('(')?;
        let close = s.rfind(')')?;
        let inner = s.get(open + 1..close)?;
        let parts: Vec<&str> = inner.split(&[',', '/'][..]).collect();
        let channel = |p: &str| -> Option<u8> {
            let p = p.trim();
            if let Some(pct) = p.strip_suffix('%') {
                let v: f32 = pct.trim().parse().ok()?;
                return Some((v / 100.0 * 255.0).round().clamp(0.0, 255.0) as u8);
            }
            let v: f32 = p.parse().ok()?;
            Some(v.clamp(0.0, 255.0) as u8)
        };
        if parts.len() < 3 {
            return None;
        }
        let r = channel(parts[0])?;
        let g = channel(parts[1])?;
        let b = channel(parts[2])?;
        let a = match parts.get(3) {
            Some(p) => {
                let raw = p.trim();
                if let Some(pct) = raw.strip_suffix('%') {
                    let v: f32 = pct.trim().parse().ok()?;
                    (v / 100.0 * 255.0).round().clamp(0.0, 255.0) as u8
                } else {
                    let v: f32 = raw.parse().ok()?;
                    (v * 255.0).round().clamp(0.0, 255.0) as u8
                }
            }
            None => 255,
        };
        return Some(Color { r, g, b, a });
    }
    named_color(&s)
}

fn named_color(name: &str) -> Option<Color> {
    let rgb = match name {
        "transparent" => return Some(Color::TRANSPARENT),
        "black" => (0, 0, 0),
        "white" => (255, 255, 255),
        "red" => (255, 0, 0),
        "green" => (0, 128, 0),
        "blue" => (0, 0, 255),
        "yellow" => (255, 255, 0),
        "cyan" | "aqua" => (0, 255, 255),
        "magenta" | "fuchsia" => (255, 0, 255),
        "gray" | "grey" => (128, 128, 128),
        "silver" => (192, 192, 192),
        "maroon" => (128, 0, 0),
        "olive" => (128, 128, 0),
        "lime" => (0, 255, 0),
        "navy" => (0, 0, 128),
        "purple" => (128, 0, 128),
        "teal" => (0, 128, 128),
        "orange" => (255, 165, 0),
        "currentcolor" => return None,
        _ => return None,
    };
    Some(Color::rgb(rgb.0, rgb.1, rgb.2))
}

/// Split a shorthand value into whitespace-separated parts, respecting functions.
pub fn split_parts(input: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut depth = 0i32;
    let mut in_string: Option<char> = None;
    for ch in input.chars() {
        if let Some(q) = in_string {
            current.push(ch);
            if ch == q {
                in_string = None;
            }
            continue;
        }
        match ch {
            '"' | '\'' => {
                in_string = Some(ch);
                current.push(ch);
            }
            '(' => {
                depth += 1;
                current.push(ch);
            }
            ')' => {
                depth -= 1;
                current.push(ch);
            }
            c if c.is_whitespace() && depth == 0 => {
                if !current.is_empty() {
                    parts.push(std::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }
    if !current.is_empty() {
        parts.push(current);
    }
    parts
}

/// Expand a 1–4 value box shorthand into top, right, bottom, left.
pub fn expand_box(parts: &[String]) -> Option<[String; 4]> {
    match parts.len() {
        1 => Some([parts[0].clone(), parts[0].clone(), parts[0].clone(), parts[0].clone()]),
        2 => Some([
            parts[0].clone(),
            parts[1].clone(),
            parts[0].clone(),
            parts[1].clone(),
        ]),
        3 => Some([
            parts[0].clone(),
            parts[1].clone(),
            parts[2].clone(),
            parts[1].clone(),
        ]),
        4 => Some([
            parts[0].clone(),
            parts[1].clone(),
            parts[2].clone(),
            parts[3].clone(),
        ]),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lengths_keep_units() {
        assert_eq!(parse_length("12px"), Length::Px(12.0));
        assert_eq!(parse_length("1.5em"), Length::Em(1.5));
        assert_eq!(parse_length("2rem"), Length::Rem(2.0));
        assert_eq!(parse_length("50%"), Length::Percent(50.0));
        assert_eq!(parse_length("0"), Length::Px(0.0));
        assert_eq!(parse_length("auto"), Length::Keyword);
    }

    #[test]
    fn colours() {
        assert_eq!(parse_color("#000"), Some(Color::rgb(0, 0, 0)));
        assert_eq!(parse_color("#ffffff"), Some(Color::WHITE));
        assert_eq!(parse_color("rgb(1, 2, 3)"), Some(Color::rgb(1, 2, 3)));
        assert_eq!(parse_color("rgba(1,2,3,0.5)"), Some(Color { r: 1, g: 2, b: 3, a: 128 }));
        assert_eq!(parse_color("red"), Some(Color::rgb(255, 0, 0)));
        assert_eq!(parse_color("transparent"), Some(Color::TRANSPARENT));
    }

    #[test]
    fn box_shorthands() {
        let parts = split_parts("10px 20px");
        assert_eq!(
            expand_box(&parts).unwrap(),
            ["10px".to_string(), "20px".to_string(), "10px".to_string(), "20px".to_string()]
        );
        let three = split_parts("1px 2px 3px");
        assert_eq!(
            expand_box(&three).unwrap(),
            ["1px".to_string(), "2px".to_string(), "3px".to_string(), "2px".to_string()]
        );
    }
}

/// The four corner radii, in the order CSS's `border-radius` shorthand uses:
/// top-left, top-right, bottom-right, bottom-left.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Corners<T> {
    pub top_left: T,
    pub top_right: T,
    pub bottom_right: T,
    pub bottom_left: T,
}

impl<T: Copy> Corners<T> {
    pub fn all(v: T) -> Self {
        Corners { top_left: v, top_right: v, bottom_right: v, bottom_left: v }
    }

    pub fn uniform(&self) -> bool
    where
        T: PartialEq,
    {
        self.top_left == self.top_right
            && self.top_right == self.bottom_right
            && self.bottom_right == self.bottom_left
    }
}

/// Parse `border-radius: 4px` / `4px 8px` / `1px 2px 3px 4px` into lengths.
pub fn parse_radii(value: &str) -> Corners<Length> {
    let parts = split_parts(value);
    let radius = |i: usize| -> Length {
        let part = parts.get(i % parts.len().max(1)).unwrap_or(&parts[0]);
        // `4px / 8px` (elliptical) collapses to its horizontal component: the engine
        // draws circular corners, which is exact whenever both agree.
        let horizontal = part.split('/').next().unwrap_or(part);
        parse_length(horizontal)
    };
    match parts.len() {
        0 => Corners::all(Length::Px(0.0)),
        1 => Corners::all(radius(0)),
        2 => Corners { top_left: radius(0), top_right: radius(1), bottom_right: radius(0), bottom_left: radius(1) },
        3 => Corners { top_left: radius(0), top_right: radius(1), bottom_right: radius(2), bottom_left: radius(1) },
        _ => Corners { top_left: radius(0), top_right: radius(1), bottom_right: radius(2), bottom_left: radius(3) },
    }
}

/// A `box-shadow` layer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Shadow {
    pub x: f32,
    pub y: f32,
    pub blur: f32,
    pub spread: f32,
    pub color: Color,
    /// `inset` shadows are parsed and reported, but not drawn yet.
    pub inset: bool,
}

/// Parse the first `box-shadow` layer: `[inset] x y [blur [spread]] color`.
pub fn parse_shadow(value: &str) -> Option<Shadow> {
    let (first, _rest) = split_layers(value);
    if first.trim().eq_ignore_ascii_case("none") {
        return None;
    }
    let mut numbers: Vec<f32> = Vec::new();
    let mut color = None;
    let mut inset = false;
    for part in split_parts(&first) {
        let lower = part.to_ascii_lowercase();
        if lower == "inset" {
            inset = true;
            continue;
        }
        if let Some(c) = parse_color(&part) {
            color = Some(c);
            continue;
        }
        if let Some(px) = parse_px(&part) {
            numbers.push(px);
        }
    }
    if numbers.len() < 2 {
        return None;
    }
    Some(Shadow {
        x: numbers[0],
        y: numbers[1],
        blur: numbers.get(2).copied().unwrap_or(0.0),
        spread: numbers.get(3).copied().unwrap_or(0.0),
        color: color.unwrap_or(Color { r: 0, g: 0, b: 0, a: 64 }),
        inset,
    })
}

/// Split a comma-separated list at the top level (so `rgba(0,0,0,.2)` stays whole).
pub fn split_layers(value: &str) -> (String, String) {
    let bytes = value.as_bytes();
    let mut depth = 0i32;
    for (i, b) in bytes.iter().enumerate() {
        match b {
            b'(' => depth += 1,
            b')' => depth -= 1,
            b',' if depth == 0 => {
                return (value[..i].to_string(), value[i + 1..].to_string());
            }
            _ => {}
        }
    }
    (value.to_string(), String::new())
}

/// Split on every top-level comma, honouring parentheses and quotes. Gradient
/// colour stops live inside `linear-gradient(...)`, so a naive split would tear
/// the function apart.
pub fn split_commas(input: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut depth = 0i32;
    let mut in_string: Option<char> = None;
    for ch in input.chars() {
        if let Some(q) = in_string {
            current.push(ch);
            if ch == q {
                in_string = None;
            }
            continue;
        }
        match ch {
            '"' | '\'' => {
                in_string = Some(ch);
                current.push(ch);
            }
            '(' => {
                depth += 1;
                current.push(ch);
            }
            ')' => {
                depth -= 1;
                current.push(ch);
            }
            ',' if depth == 0 => {
                parts.push(std::mem::take(&mut current));
            }
            c => current.push(c),
        }
    }
    parts.push(current);
    parts
}

/// One layer of `background-image`, for the subset the engine can draw.
#[derive(Debug, Clone, PartialEq)]
pub enum BackgroundImage {
    /// `linear-gradient(<angle>, <stop> …)`.
    LinearGradient { angle: f32, stops: Vec<(f32, Color)> },
    /// `url(...)`, kept as written; the paint stage resolves it.
    Url(String),
}

impl BackgroundImage {
    /// Parse the first layer of a `background-image` value.
    pub fn parse(value: &str) -> Option<BackgroundImage> {
        let (first, _) = split_layers(value);
        let trimmed = first.trim();
        if trimmed.eq_ignore_ascii_case("none") || trimmed.is_empty() {
            return None;
        }
        if let Some(rest) = trimmed.strip_prefix("url(") {
            let inner = rest.trim_end_matches(')').trim().trim_matches(|c| c == '"' || c == '\'');
            return Some(BackgroundImage::Url(inner.to_string()));
        }
        let lower = trimmed.to_ascii_lowercase();
        if lower.starts_with("linear-gradient(") || lower.starts_with("-webkit-linear-gradient(") {
            let start = trimmed.find('(')? + 1;
            let end = trimmed.rfind(')')?;
            let body = trimmed.get(start..end)?;
            // An optional leading angle, then colour stops.
            let mut angle = 180.0; // `to bottom`
            let mut stops: Vec<(f32, Color)> = Vec::new();
            for (i, part) in split_commas(body).iter().enumerate() {
                let part = part.trim();
                let lower = part.to_ascii_lowercase();
                if i == 0 && (lower.ends_with("deg") || lower.starts_with("to ")) {
                    angle = match lower.trim_end_matches("deg").parse::<f32>() {
                        Ok(v) => v,
                        Err(_) => direction_angle(&lower),
                    };
                    continue;
                }
                // `color [position]`; positions are assigned proportionally afterwards.
                let mut words = part.split_whitespace();
                let Some(first_word) = words.next() else { continue };
                let colour = match parse_color(first_word) {
                    Some(c) => c,
                    None => continue,
                };
                let position = words
                    .next()
                    .and_then(|w| if w.ends_with('%') { w.trim_end_matches('%').parse::<f32>().ok() } else { None });
                stops.push((position.unwrap_or(f32::NAN), colour));
            }
            if stops.len() < 2 {
                return None;
            }
            // Fill in missing positions: first is 0%, last is 100%, the rest spread.
            let n = stops.len();
            for i in 0..n {
                if stops[i].0.is_nan() {
                    let before = (0..i).rev().find(|j| !stops[*j].0.is_nan());
                    let after = (i + 1..n).find(|j| !stops[*j].0.is_nan());
                    let position = match (before, after) {
                        (Some(b), Some(a)) => {
                            let span = (a - b) as f32;
                            stops[b].0 + (stops[i].0.max(0.0) * 0.0) + (i - b) as f32 / span * (stops[a].0 - stops[b].0)
                        }
                        (None, Some(_)) => 0.0,
                        (Some(_), None) => 100.0,
                        (None, None) => i as f32 / (n - 1) as f32 * 100.0,
                    };
                    stops[i].0 = position;
                }
            }
            return Some(BackgroundImage::LinearGradient { angle, stops });
        }
        None
    }
}

fn direction_angle(value: &str) -> f32 {
    match value.trim() {
        "to top" => 0.0,
        "to right" => 90.0,
        "to bottom" => 180.0,
        "to left" => 270.0,
        "to top right" | "to right top" => 45.0,
        "to bottom right" | "to right bottom" => 135.0,
        "to bottom left" | "to left bottom" => 225.0,
        "to top left" | "to left top" => 315.0,
        _ => 180.0,
    }
}
