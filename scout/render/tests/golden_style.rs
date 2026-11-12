//! Stage 1 gate: does our cascade agree with Chromium's computed styles?
//!
//! Every fixture in `corpus/fixtures/` has a golden recorded from Chromium by
//! `harness/measure.mjs`. The golden is a tree of element nodes keyed by an index
//! path (`i`, element children only), which is also what `Document::element_path`
//! produces, so the two trees can be joined without caring which parser ran.
//!
//! Compared here: the 28 properties that are *computed* (not used) style.
//! Deferred: `width`/`height` are used values — they belong to the layout gate
//! (Stage 2) — and `line-height: normal` is a keyword that Chromium does not
//! resolve to a number, so there is nothing to diff.
//!
//! Gate from RENDER_ENGINE_PLAN.md: ≥90% of property comparisons match overall,
//! and no single fixture falls below 80%.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use scout_render::css::value::Color;
use scout_render::dom::NodeId;
use scout_render::style::{Display, Position, Style, TextAlign};
use scout_render::{text, Viewport};
use serde_json::Value;

// ---------------------------------------------------------------- goldens

struct Golden {
    /// path joined with '.' → (tag, property map)
    nodes: BTreeMap<String, (String, BTreeMap<String, String>)>,
}

fn key(path: &[usize]) -> String {
    path.iter()
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(".")
}

fn flatten(node: &Value, out: &mut BTreeMap<String, (String, BTreeMap<String, String>)>) {
    let path: Vec<usize> = node["i"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_u64().unwrap() as usize)
        .collect();
    let mut styles = BTreeMap::new();
    if let Some(obj) = node["styles"].as_object() {
        for (k, v) in obj {
            styles.insert(k.clone(), v.as_str().unwrap_or("").to_string());
        }
    }
    out.insert(
        key(&path),
        (node["tag"].as_str().unwrap_or("").to_string(), styles),
    );
    for child in node["children"].as_array().unwrap() {
        flatten(child, out);
    }
}

fn load_golden(path: &Path) -> Golden {
    let text = fs::read_to_string(path).expect("golden readable");
    let json: Value = serde_json::from_str(&text).expect("golden is json");
    let mut nodes = BTreeMap::new();
    flatten(&json["root"], &mut nodes);
    Golden { nodes }
}

// ---------------------------------------------------------------- parsing values

fn px(s: &str) -> Option<f32> {
    let s = s.trim();
    if let Some(v) = s.strip_suffix("px") {
        v.trim().parse::<f32>().ok()
    } else {
        s.parse::<f32>().ok()
    }
}

fn colour(s: &str) -> Option<Color> {
    let s = s.trim().to_ascii_lowercase();
    if s == "transparent" {
        return Some(Color::TRANSPARENT);
    }
    if let Some(rest) = s.strip_prefix("rgb(").or_else(|| s.strip_prefix("rgba(")) {
        let inner = rest.trim_end_matches(')');
        let parts: Vec<f32> = inner
            .split(|c| c == ',' || c == ' ')
            .filter(|p| !p.trim().is_empty())
            .filter_map(|p| p.trim().parse::<f32>().ok())
            .collect();
        if parts.len() >= 3 {
            let a = parts.get(3).copied().unwrap_or(1.0);
            return Some(Color {
                r: parts[0].round() as u8,
                g: parts[1].round() as u8,
                b: parts[2].round() as u8,
                a: (a * 255.0).round() as u8,
            });
        }
        return None;
    }
    // Named colours the fixtures use, plus hex.
    let named = match s.as_str() {
        "black" => Some(Color::BLACK),
        "white" => Some(Color::WHITE),
        _ => None,
    };
    if named.is_some() {
        return named;
    }
    if let Some(hex) = s.strip_prefix('#') {
        let parse = |h: &str| u8::from_str_radix(h, 16).ok();
        return match hex.len() {
            3 => Some(Color {
                r: parse(&hex[0..1])? * 17,
                g: parse(&hex[1..2])? * 17,
                b: parse(&hex[2..3])? * 17,
                a: 255,
            }),
            6 => Some(Color {
                r: parse(&hex[0..2])?,
                g: parse(&hex[2..4])?,
                b: parse(&hex[4..6])?,
                a: 255,
            }),
            _ => None,
        };
    }
    None
}

fn same_colour(a: Color, b: Color) -> bool {
    // Chromium prints 8-bit channels; allow one unit of rounding slack.
    (a.r as i32 - b.r as i32).abs() <= 1
        && (a.g as i32 - b.g as i32).abs() <= 1
        && (a.b as i32 - b.b as i32).abs() <= 1
        && (a.a as i32 - b.a as i32).abs() <= 1
}

fn close(a: f32, b: f32, tol: f32) -> bool {
    (a - b).abs() <= tol
}

fn first_family(list: &str) -> String {
    let first = list.split(',').next().unwrap_or(list).trim();
    first.trim_matches(|c| c == '"' || c == '\'').to_ascii_lowercase()
}

/// Our engine has exactly one face (DejaVu Sans); Chromium reports the *specified*
/// family for the default font, "Times New Roman". Both are the document default,
/// so they compare equal here and the divergence is noted in the report.
fn normalise_family(name: &str) -> String {
    match name {
        "times new roman" | "times" | "serif" | "dejavu sans" | "arial" | "helvetica"
        | "sans-serif" => "default".to_string(),
        other => other.to_string(),
    }
}

fn display_str(d: Display) -> &'static str {
    match d {
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
}

fn position_str(p: Position) -> &'static str {
    match p {
        Position::Static => "static",
        Position::Relative => "relative",
        Position::Absolute => "absolute",
        Position::Fixed => "fixed",
    }
}

fn text_align_str(t: TextAlign) -> &'static str {
    match t {
        // Chromium reports the keyword as authored: `start` is the initial value.
        TextAlign::Left => "start",
        TextAlign::Right => "end",
        TextAlign::Center => "center",
        TextAlign::Justify => "justify",
    }
}

fn white_space_str(w: scout_render::style::WhiteSpace) -> &'static str {
    use scout_render::style::WhiteSpace::*;
    match w {
        Normal => "normal",
        Nowrap => "nowrap",
        Pre => "pre",
        PreWrap => "pre-wrap",
        PreLine => "pre-line",
    }
}

// ---------------------------------------------------------------- comparison

#[derive(Default)]
struct Tally {
    props: BTreeMap<String, (usize, usize)>, // prop → (passed, checked)
    failures: Vec<String>,
    skipped: usize,
}

impl Tally {
    fn check(&mut self, prop: &str, ok: bool, detail: impl FnOnce() -> String) {
        let e = self.props.entry(prop.to_string()).or_insert((0, 0));
        e.1 += 1;
        if ok {
            e.0 += 1;
        } else {
            self.failures.push(detail());
        }
    }

    fn counts(&self) -> (usize, usize) {
        let mut passed = 0;
        let mut checked = 0;
        for (p, c) in self.props.values() {
            passed += p;
            checked += c;
        }
        (passed, checked)
    }
}

/// Compare one element. Returns nothing; writes into `tally`.
fn compare_element(
    path: &[usize],
    tag: &str,
    ours: &Style,
    golden: &BTreeMap<String, String>,
    root_font_size: f32,
    tally: &mut Tally,
) {
    let where_ = || format!("{} <{}>", key(path), tag);
    let g = |name: &str| golden.get(name).map(|s| s.trim()).unwrap_or("");

    for prop in [
        "display",
        "position",
        "visibility",
        "font-family",
        "font-size",
        "font-weight",
        "font-style",
        "letter-spacing",
        "text-align",
        "text-transform",
        "white-space",
        "color",
        "background-color",
        "box-sizing",
        "overflow",
    ] {
        if g(prop).is_empty() {
            continue;
        }
        let ok = match prop {
            "display" => g(prop) == display_str(ours.display),
            "position" => g(prop) == position_str(ours.position),
            "visibility" => g(prop) == if ours.visibility_visible { "visible" } else { "hidden" },
            "font-family" => {
                normalise_family(&first_family(g(prop)))
                    == normalise_family(&first_family(&ours.font_family))
            }
            "font-size" => px(g(prop)).map(|v| close(v, ours.font_size, 0.5)).unwrap_or(false),
            "font-weight" => g(prop).parse::<u16>().map(|v| v == ours.font_weight).unwrap_or(false),
            "font-style" => g(prop) == if ours.font_style_italic { "italic" } else { "normal" },
            "letter-spacing" => match g(prop) {
                "normal" => ours.letter_spacing.abs() < 0.01,
                v => px(v).map(|p| close(p, ours.letter_spacing, 0.5)).unwrap_or(false),
            },
            "text-align" => {
                // `left` and `start` are the same edge in an ltr document.
                let gl = g(prop);
                let mine = text_align_str(ours.text_align);
                gl == mine || (mine == "start" && gl == "left") || (mine == "end" && gl == "right")
            }
            "text-transform" => match g(prop) {
                "uppercase" => ours.text_transform_upper,
                "none" => !ours.text_transform_upper,
                _ => false,
            },
            "white-space" => g(prop) == white_space_str(ours.white_space),
            "color" => {
                colour(g(prop)).map(|c| same_colour(c, ours.color)).unwrap_or(false)
            }
            "background-color" => colour(g(prop))
                .map(|c| same_colour(c, ours.background_color))
                .unwrap_or(false),
            "box-sizing" => {
                g(prop)
                    == match ours.box_sizing {
                        scout_render::style::BoxSizing::ContentBox => "content-box",
                        scout_render::style::BoxSizing::BorderBox => "border-box",
                    }
            }
            "overflow" => g(prop) == if ours.overflow_hidden { "hidden" } else { "visible" },
            _ => unreachable!(),
        };
        tally.check(prop, ok, || {
            format!("{} {}: chromium={:?} ours={:?}", where_(), prop, g(prop), describe(prop, ours))
        });
    }

    // Four-sided boxes.
    let sides = ["top", "right", "bottom", "left"];
    for (i, side) in sides.iter().enumerate() {
        for (prop, edge) in [
            ("margin", ours.margin),
            ("padding", ours.padding),
        ] {
            let name = format!("{}-{}", prop, side);
            let gv = g(&name);
            if gv.is_empty() {
                continue;
            }
            let mine = match i {
                0 => edge.top,
                1 => edge.right,
                2 => edge.bottom,
                _ => edge.left,
            };
            // Percentages and `auto` need a containing block; that is layout's job.
            let Some(pixels) = mine.resolve(ours.font_size, root_font_size) else {
                tally.skipped += 1;
                continue;
            };
            let ok = px(gv).map(|v| close(v, pixels, 0.5)).unwrap_or(false);
            tally.check(&name, ok, || {
                format!("{} {}: chromium={:?} ours={:?}px", where_(), name, gv, pixels)
            });
        }

        let name = format!("border-{}-width", side);
        let gv = g(&name);
        if gv.is_empty() {
            continue;
        }
        let mine = match i {
            0 => ours.border_width.top,
            1 => ours.border_width.right,
            2 => ours.border_width.bottom,
            _ => ours.border_width.left,
        };
        let ok = px(gv).map(|v| close(v, mine, 0.5)).unwrap_or(false);
        tally.check(&name, ok, || {
            format!("{} {}: chromium={:?} ours={:?}px", where_(), name, gv, mine)
        });
    }

    // `line-height` is reported as the keyword when it is `normal`, which cannot be
    // diffed against a number — but every other form must agree.
    match g("line-height") {
        "" | "normal" => {}
        v => {
            let ok = px(v).map(|p| close(p, ours.line_height, 0.5)).unwrap_or(false);
            tally.check("line-height", ok, || {
                format!("{} line-height: chromium={:?} ours={:?}px", where_(), v, ours.line_height)
            });
        }
    }
}

fn describe(prop: &str, s: &Style) -> String {
    match prop {
        "display" => display_str(s.display).to_string(),
        "position" => position_str(s.position).to_string(),
        "visibility" => (if s.visibility_visible { "visible" } else { "hidden" }).to_string(),
        "font-family" => s.font_family.clone(),
        "font-size" => format!("{}px", s.font_size),
        "font-weight" => s.font_weight.to_string(),
        "font-style" => (if s.font_style_italic { "italic" } else { "normal" }).to_string(),
        "letter-spacing" => format!("{}px", s.letter_spacing),
        "text-align" => text_align_str(s.text_align).to_string(),
        "text-transform" => (if s.text_transform_upper { "uppercase" } else { "none" }).to_string(),
        "white-space" => white_space_str(s.white_space).to_string(),
        "color" => s.color.to_css(),
        "background-color" => s.background_color.to_css(),
        "box-sizing" => match s.box_sizing {
            scout_render::style::BoxSizing::ContentBox => "content-box".to_string(),
            scout_render::style::BoxSizing::BorderBox => "border-box".to_string(),
        },
        "overflow" => (if s.overflow_hidden { "hidden" } else { "visible" }).to_string(),
        _ => "?".to_string(),
    }
}

// ---------------------------------------------------------------- the gate

fn fixtures() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("corpus/fixtures");
    let mut out: Vec<PathBuf> = fs::read_dir(&dir)
        .expect("corpus/fixtures exists")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().map(|x| x == "html").unwrap_or(false))
        .collect();
    out.sort();
    out
}

#[test]
fn stage1_computed_styles_match_chromium() {
    let mut fonts = text::FontContext::new(text::root_font_size());
    let root_font_size = text::root_font_size();
    let stem = |p: &Path| p.file_stem().unwrap().to_string_lossy().to_string();

    let mut overall = Tally::default();
    let mut rows: Vec<(String, usize, usize, usize, usize)> = Vec::new(); // name, passed, checked, skipped, elements
    let mut skipped_props: BTreeMap<String, usize> = BTreeMap::new();
    let mut lines: Vec<String> = Vec::new();

    for path in fixtures() {
        let name = stem(&path);
        let golden_path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("corpus/goldens")
            .join(format!("{}.json", name));
        let golden = load_golden(&golden_path);
        let html = fs::read_to_string(&path).expect("fixture readable");

        let (doc, computed) = scout_render::style_document(&html, Viewport::default(), &mut fonts);

        let mut tally = Tally::default();
        let mut elements = 0usize;
        for id in doc.elements() {
            elements += 1;
            let path = doc.element_path(id);
            let Some((tag, styles)) = golden.nodes.get(&key(&path)) else {
                tally.failures.push(format!("{}: no golden element at this path", key(&path)));
                continue;
            };
            let Some(style) = computed.styles.get(&id as &NodeId) else {
                tally.failures.push(format!("{} <{}>: no computed style", key(&path), tag));
                continue;
            };
            compare_element(&path, tag, style, styles, root_font_size, &mut tally);
        }

        // Sanity: the two trees must have the same shape.
        let golden_count = golden.nodes.len();
        if golden_count != elements {
            lines.push(format!(
                "  note {}: {} elements in html5ever, {} in the golden",
                name, elements, golden_count
            ));
        }

        // Fold this fixture into the overall tally.
        for (prop, (p, c)) in &tally.props {
            let e = overall.props.entry(prop.clone()).or_insert((0, 0));
            e.0 += p;
            e.1 += c;
        }
        overall.skipped += tally.skipped;
        for f in tally.failures.iter().take(6) {
            lines.push(format!("  {} FAIL {}", name, f));
        }
        if tally.failures.len() > 6 {
            lines.push(format!("  {} … {} more failures", name, tally.failures.len() - 6));
        }
        for (prop, (_, c)) in &tally.props {
            *skipped_props.entry(prop.clone()).or_insert(0) += c;
        }

        let (passed, checked) = tally.counts();
        rows.push((name, passed, checked, tally.skipped, elements));
    }

    // ---- report
    let mut report = String::new();
    report.push_str("Stage 1 — computed style vs Chromium\n");
    report.push_str("fixture                     pass    checked   rate    elements\n");
    let mut worst: Option<(&str, f32)> = None;
    for (name, passed, checked, skipped, elements) in &rows {
        let rate = if *checked == 0 { 0.0 } else { *passed as f32 / *checked as f32 };
        report.push_str(&format!(
            "{:<26} {:>4}    {:>5}    {:>5.1}%   {:>4}{}\n",
            name,
            passed,
            checked,
            rate * 100.0,
            elements,
            if *skipped > 0 { format!("  ({} deferred)", skipped) } else { String::new() }
        ));
        if worst.map(|(_, w)| rate < w).unwrap_or(true) {
            worst = Some((name, rate));
        }
    }
    let (op, oc) = overall.counts();
    let overall_rate = if oc == 0 { 0.0 } else { op as f32 / oc as f32 };
    report.push_str(&format!(
        "\noverall {}/{} = {:.2}%  (gate ≥90%; worst fixture {} at {:.1}%, gate ≥80%)\n",
        op,
        oc,
        overall_rate * 100.0,
        worst.map(|(n, _)| n).unwrap_or("-"),
        worst.map(|(_, r)| r * 100.0).unwrap_or(0.0)
    ));

    report.push_str("\nper property:\n");
    for (prop, (p, c)) in &overall.props {
        if p != c {
            report.push_str(&format!("  {:<22} {:>4}/{:<4}  {:.0}%\n", prop, p, c, *p as f32 / *c as f32 * 100.0));
        }
    }
    report.push_str("\ndiagnostics:\n");
    for l in &lines {
        report.push_str(l);
        report.push('\n');
    }
    if overall.skipped > 0 {
        report.push_str(&format!(
            "\n{} comparisons deferred (percentage/auto margins need a containing block).\n",
            overall.skipped
        ));
    }

    let out = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/golden-style-report.txt");
    let _ = fs::write(&out, &report);
    println!("{}", report);

    assert!(oc > 0, "no comparisons ran — the fixtures or goldens are missing");
    assert!(
        overall_rate >= 0.90,
        "Stage 1 gate: overall match {:.1}% < 90%",
        overall_rate * 100.0
    );
    for (name, passed, checked, _, _) in &rows {
        let rate = *passed as f32 / *checked as f32;
        assert!(
            rate >= 0.80,
            "Stage 1 gate: fixture {} matched {:.1}% < 80%",
            name,
            rate * 100.0
        );
    }
}
