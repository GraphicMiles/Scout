//! CSS parsing for the property subset the engine lays out and paints.
//!
//! The parser is deliberately small and explicit: it reads rules, declarations and
//! at-rules it understands, and records everything it does not understand instead
//! of dropping it silently. A cascade that quietly loses a `!important` rule is
//! worse than one that reports the rule it skipped.

pub mod selector;
pub mod value;

use selector::{parse_selector_list, Selector, Unsupported};

#[derive(Debug, Clone, PartialEq)]
pub struct Declaration {
    pub property: String,
    pub value: String,
    pub important: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct MediaQuery {
    pub min_width: Option<f32>,
    pub max_width: Option<f32>,
    /// Every condition in the prelude was understood. A query this engine cannot
    /// evaluate must not be treated as unconditional: that is how a page's
    /// `prefers-color-scheme: dark` block ends up painting over its light theme.
    pub understood: bool,
    /// A condition we know to be false for a still, light, wide viewport.
    pub impossible: bool,
}

/// What the engine pretends to be, in media-feature terms: a light-coloured,
/// wide, pointer-driven screen with no preference for reduced motion. These are
/// the browser defaults Chromium screenshots with, so both sides agree.
impl MediaQuery {
    pub fn matches(&self, viewport_width: f32) -> bool {
        if self.impossible || !self.understood && self.is_conditional() {
            return false;
        }
        if let Some(min) = self.min_width {
            if viewport_width < min {
                return false;
            }
        }
        if let Some(max) = self.max_width {
            if viewport_width > max {
                return false;
            }
        }
        true
    }

    fn is_conditional(&self) -> bool {
        self.min_width.is_some() || self.max_width.is_some() || !self.understood
    }

    /// Intersect two conditions, as nesting `@media` blocks does.
    pub fn and(&self, other: &MediaQuery) -> MediaQuery {
        MediaQuery {
            min_width: match (self.min_width, other.min_width) {
                (Some(a), Some(b)) => Some(a.max(b)),
                (a, b) => a.or(b),
            },
            max_width: match (self.max_width, other.max_width) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            },
            understood: self.understood && other.understood,
            impossible: self.impossible || other.impossible,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Rule {
    pub selectors: Vec<Selector>,
    pub declarations: Vec<Declaration>,
    pub media: Option<MediaQuery>,
    /// Position in the stylesheet, for the cascade's final tie-break.
    pub order: usize,
}

/// One `@font-face` rule: a family name, the files that can supply it, and the
/// weight and style it stands for.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FontFace {
    pub family: String,
    pub srcs: Vec<String>,
    pub weight: u16,
    pub italic: bool,
}

#[derive(Default)]
pub struct Stylesheet {
    pub rules: Vec<Rule>,
    pub unsupported: Vec<Unsupported>,
    pub at_rules_skipped: Vec<String>,
    /// `@font-face` rules, in document order.
    pub fonts: Vec<FontFace>,
    /// The sheet's own address, so relative `url()`s in `@font-face` resolve the
    /// way they do for the page.
    pub base: String,
}

impl Stylesheet {
    pub fn parse(css: &str) -> Stylesheet {
        Self::parse_with_base(css, "")
    }

    /// Parse a sheet that was loaded from `base` (a URL or a filesystem path).
    pub fn parse_with_base(css: &str, base: &str) -> Stylesheet {
        let mut sheet = Stylesheet { base: base.to_string(), ..Default::default() };
        let cleaned = strip_comments(css);
        sheet.parse_block(&cleaned, None);
        sheet
    }

    pub fn extend(&mut self, other: Stylesheet) {
        let base = self.rules.len();
        for (i, mut rule) in other.rules.into_iter().enumerate() {
            rule.order = base + i;
            self.rules.push(rule);
        }
        self.unsupported.extend(other.unsupported);
        self.at_rules_skipped.extend(other.at_rules_skipped);
        self.fonts.extend(other.fonts);
    }

    fn parse_block(&mut self, input: &str, media: Option<MediaQuery>) {
        let bytes = input.as_bytes();
        let mut i = 0usize;

        while i < bytes.len() {
            // Skip whitespace.
            while i < bytes.len() && (bytes[i] as char).is_ascii_whitespace() {
                i += 1;
            }
            if i >= bytes.len() {
                break;
            }

            if bytes[i] == b'@' {
                let start = i;
                let (_prelude, after) = read_until(input, i, &[b'{', b';']);
                let prelude_text = input[start..after].trim().to_string();
                i = after;
                if i < bytes.len() && bytes[i] == b';' {
                    i += 1;
                    self.at_rules_skipped.push(prelude_text);
                    continue;
                }
                // `{` — find the block.
                let block_end = match matching_brace(bytes, i) {
                    Some(end) => end,
                    None => break,
                };
                let block = &input[i + 1..block_end];
                i = block_end + 1;

                let lower = prelude_text.to_ascii_lowercase();
                if lower.starts_with("@media") {
                    let condition = parse_media(&prelude_text[6..]);
                    let merged = match (&media, condition) {
                        (Some(outer), Some(inner)) => Some(outer.and(&inner)),
                        (Some(outer), None) => Some(outer.clone()),
                        (None, Some(inner)) => Some(inner),
                        (None, None) => None,
                    };
                    if let Some(cond) = &merged {
                        if !cond.matches(f32::INFINITY) && cond.max_width.is_some() {
                            // Conditions that can never match are still parsed; the
                            // cascade filters by viewport at match time.
                        }
                    }
                    self.parse_block(block, merged.or_else(|| media.clone()));
                } else if lower.starts_with("@font-face") {
                    if let Some(face) = parse_font_face(block) {
                        self.fonts.push(face);
                    }
                } else {
                    // @keyframes, @supports, @layer, @import …
                    self.at_rules_skipped.push(prelude_text);
                }
                continue;
            }

            // A style rule: selectors up to `{`, declarations up to `}`.
            let brace = match input[i..].find('{') {
                Some(offset) => i + offset,
                None => break,
            };
            let selector_text = input[i..brace].trim().to_string();
            let block_end = match matching_brace(bytes, brace) {
                Some(end) => end,
                None => break,
            };
            let declarations_text = &input[brace + 1..block_end];
            i = block_end + 1;

            if selector_text.is_empty() {
                continue;
            }
            let (selectors, unsupported) = parse_selector_list(&selector_text);
            self.unsupported.extend(unsupported);
            if selectors.is_empty() {
                continue;
            }
            let declarations = parse_declarations(declarations_text);
            let order = self.rules.len();
            self.rules.push(Rule {
                selectors,
                declarations,
                media: media.clone(),
                order,
            });
        }
    }
}

/// Read one `@font-face` block into a declaration the font loader understands.
fn parse_font_face(block: &str) -> Option<FontFace> {
    let mut face = FontFace { weight: 400, ..Default::default() };
    for decl in parse_declarations(block) {
        let (name, value) = (decl.property.as_str(), decl.value);
        match name {
            "font-family" => {
                face.family = value.trim().trim_matches(|c| c == '\'' || c == '"').to_string();
            }
            "src" => {
                // `url(a.woff2) format("woff2"), url(a.woff) format("woff")` — keep
                // every url in source order; the loader picks the first one it can
                // decode, which is the browser's own rule.
                let mut rest = value.as_str();
                while let Some(start) = rest.find("url(") {
                    let after = &rest[start + 4..];
                    let Some(end) = after.find(')') else { break };
                    let url = after[..end].trim().trim_matches(|c| c == '\'' || c == '"');
                    if !url.is_empty() {
                        face.srcs.push(url.to_string());
                    }
                    rest = &after[end..];
                }
            }
            "font-weight" => {
                // A range (`100 900`) is a variable font: take its first value.
                let first = value.split_whitespace().next().unwrap_or("");
                face.weight = first.parse::<u16>().unwrap_or(400);
            }
            "font-style" => face.italic = value.trim().starts_with("italic"),
            _ => {}
        }
    }
    if face.family.is_empty() || face.srcs.is_empty() {
        return None;
    }
    Some(face)
}

/// Strip `/* … */`, respecting strings.
fn strip_comments(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    let mut in_string: Option<char> = None;
    while let Some(ch) = chars.next() {
        if let Some(q) = in_string {
            out.push(ch);
            if ch == q {
                in_string = None;
            }
            continue;
        }
        if ch == '"' || ch == '\'' {
            in_string = Some(ch);
            out.push(ch);
            continue;
        }
        if ch == '/' && chars.peek() == Some(&'*') {
            chars.next();
            while let Some(c) = chars.next() {
                if c == '*' && chars.peek() == Some(&'/') {
                    chars.next();
                    break;
                }
            }
            out.push(' ');
            continue;
        }
        out.push(ch);
    }
    out
}

/// Read from `start` until one of `stops` at nesting depth zero. Returns the slice
/// and the index of the stop character (or the end).
fn read_until<'a>(input: &'a str, start: usize, stops: &[u8]) -> (&'a str, usize) {
    let bytes = input.as_bytes();
    let mut depth = 0i32;
    let mut i = start;
    let mut in_string: Option<u8> = None;
    while i < bytes.len() {
        let b = bytes[i];
        if let Some(q) = in_string {
            if b == q {
                in_string = None;
            }
            i += 1;
            continue;
        }
        match b {
            b'"' | b'\'' => in_string = Some(b),
            b'(' | b'[' => depth += 1,
            b')' | b']' => depth -= 1,
            _ => {}
        }
        if depth == 0 && stops.contains(&b) {
            return (&input[start..i], i);
        }
        i += 1;
    }
    (&input[start..], bytes.len())
}

fn matching_brace(bytes: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0i32;
    let mut i = open;
    let mut in_string: Option<u8> = None;
    while i < bytes.len() {
        let b = bytes[i];
        if let Some(q) = in_string {
            if b == q {
                in_string = None;
            }
            i += 1;
            continue;
        }
        match b {
            b'"' | b'\'' => in_string = Some(b),
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

pub fn parse_declarations(input: &str) -> Vec<Declaration> {
    let mut out = Vec::new();
    for piece in split_declarations(input) {
        let piece = piece.trim();
        if piece.is_empty() {
            continue;
        }
        let colon = match find_top_level_colon(piece) {
            Some(pos) => pos,
            None => continue,
        };
        let property = piece[..colon].trim().to_ascii_lowercase();
        let mut value = piece[colon + 1..].trim().to_string();
        if property.is_empty() || value.is_empty() {
            continue;
        }
        let mut important = false;
        let lower = value.to_ascii_lowercase();
        if let Some(pos) = lower.rfind("!important") {
            important = true;
            value = value[..pos].trim().to_string();
        }
        out.push(Declaration {
            property,
            value,
            important,
        });
    }
    out
}

fn split_declarations(input: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut current = String::new();
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
            ';' if depth == 0 => out.push(std::mem::take(&mut current)),
            _ => current.push(ch),
        }
    }
    if !current.trim().is_empty() {
        out.push(current);
    }
    out
}

fn find_top_level_colon(input: &str) -> Option<usize> {
    let mut depth = 0i32;
    for (i, ch) in input.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => depth -= 1,
            ':' if depth == 0 => return Some(i),
            _ => {}
        }
    }
    None
}

/// `(min-width: 600px) and (max-width: 900px)`. Only width features are read;
/// anything else is ignored, which is the conservative behaviour for a fixed
/// viewport engine.
fn parse_media(prelude: &str) -> Option<MediaQuery> {
    let lower = prelude.to_ascii_lowercase();
    // A media type the engine is not: `@media print` must not apply.
    let typed_out = ["print", "speech"].iter().any(|t| {
        lower.split([',', ' ']).any(|word| word.trim() == *t) && !lower.contains("screen")
    });

    let mut query = MediaQuery { understood: true, ..Default::default() };
    let mut any = false;
    for part in lower.split(')') {
        // Each clause looks like `and (min-width: 600px)`, or occasionally
        // `(min-width : 600px)` — whitespace around the colon is legal CSS and
        // shows up in real stylesheets.
        let inside = part.rsplit('(').next().unwrap_or(part).trim();
        if !part.contains('(') {
            continue;
        }
        any = true;
        let Some((feature, value)) = inside.split_once(':') else {
            // A valueless feature such as `(hover)`.
            match inside.trim() {
                "hover" | "any-hover" | "pointer" | "any-pointer" => {}
                "prefers-reduced-motion" | "prefers-contrast" => {}
                other => {
                    // `(min-width)` with no value is invalid CSS; anything else
                    // unknown must not be assumed true.
                    if !other.is_empty() {
                        query.understood = false;
                    }
                }
            }
            continue;
        };
        let feature = feature.trim();
        let value = value.trim();
        match feature {
            "min-width" => {
                if let Some(v) = value::parse_media_length(value) {
                    query.min_width = Some(v);
                } else {
                    query.understood = false;
                }
            }
            "max-width" => {
                if let Some(v) = value::parse_media_length(value) {
                    query.max_width = Some(v);
                } else {
                    query.understood = false;
                }
            }
            // The engine paints a light page, like a browser with no dark-mode
            // preference of its own.
            "prefers-color-scheme" => match value {
                "light" => {}
                "dark" => query.impossible = true,
                _ => query.understood = false,
            },
            "min-height" | "max-height" | "orientation" | "hover" | "any-hover" | "pointer"
            | "any-pointer" | "prefers-reduced-motion" | "prefers-contrast" | "color"
            | "monochrome" | "resolution" | "min-resolution" | "max-resolution" | "aspect-ratio"
            | "min-aspect-ratio" | "max-aspect-ratio" | "display-mode" | "scripting"
            | "forced-colors" | "prefers-reduced-transparency" | "color-gamut"
            | "dynamic-range" | "update" | "overflow-block" | "overflow-inline" => {
                // Known features whose value a still screenshot satisfies: a wide
                // screen reports `hover: hover`, `pointer: fine`, and so on. A
                // value that would not hold makes the block inapplicable.
                if matches!(value, "none" | "reduce" | "dark" | "coarse") {
                    query.impossible = true;
                }
            }
            _ => query.understood = false,
        }
    }
    if typed_out {
        query.impossible = true;
    }
    if any || typed_out {
        Some(query)
    } else {
        None
    }
}

// ------------------------------------------------------------------- values

pub use value::{Color, Length, Percentage};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_rule() {
        let sheet = Stylesheet::parse("div.a, p { color: red; margin: 10px }");
        assert_eq!(sheet.rules.len(), 1);
        assert_eq!(sheet.rules[0].selectors.len(), 2);
        assert_eq!(sheet.rules[0].declarations.len(), 2);
    }

    #[test]
    fn records_unsupported_selectors_instead_of_dropping_the_rule_silently() {
        let sheet = Stylesheet::parse("a:has(img) { color: red } p { color: blue }");
        assert_eq!(sheet.rules.len(), 1);
        assert_eq!(sheet.unsupported.len(), 1);
        assert_eq!(sheet.unsupported[0].selector, "a:has(img)");
    }

    #[test]
    fn interactive_states_are_kept_but_never_match() {
        // `:hover` is a state, not a limitation: a still frame simply is not hovered.
        let sheet = Stylesheet::parse("a:hover { color: red } p { color: blue }");
        assert_eq!(sheet.rules.len(), 2);
        assert!(sheet.unsupported.is_empty());
    }

    #[test]
    fn importance_survives_parsing() {
        let sheet = Stylesheet::parse("p { color: red !important; }");
        assert!(sheet.rules[0].declarations[0].important);
        assert_eq!(sheet.rules[0].declarations[0].value, "red");
    }

    #[test]
    fn media_queries_carry_their_condition() {
        let sheet = Stylesheet::parse("@media (max-width: 600px) { p { color: red } }");
        assert_eq!(sheet.rules.len(), 1);
        let m = sheet.rules[0].media.as_ref().unwrap();
        assert_eq!(m.max_width, Some(600.0));
        assert!(!m.matches(800.0));
        assert!(m.matches(400.0));
    }

    #[test]
    fn comments_and_strings_are_handled() {
        let sheet = Stylesheet::parse("/* a { } */ p { content: \"a;b\" ; color: red }");
        assert_eq!(sheet.rules.len(), 1);
        assert_eq!(sheet.rules[0].declarations.len(), 2);
    }

    #[test]
    fn font_face_is_parsed_as_a_face_not_a_rule() {
        let sheet = Stylesheet::parse(
            "@font-face { font-family: 'X'; font-weight: 700; src: local(X), url(y.woff2) format('woff2'), url(y.woff) format('woff') } p { color: red }",
        );
        assert_eq!(sheet.rules.len(), 1);
        assert_eq!(sheet.fonts.len(), 1);
        let face = &sheet.fonts[0];
        assert_eq!(face.family, "X");
        assert_eq!(face.weight, 700);
        assert_eq!(face.srcs, vec!["y.woff2".to_string(), "y.woff".to_string()]);
    }
}
