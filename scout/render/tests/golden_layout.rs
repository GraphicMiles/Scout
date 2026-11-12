//! Stage 2 gate: does our block layout produce Chromium's boxes?
//!
//! A box passes when all four edges are within 2 CSS pixels of the golden. The
//! fixture list for this stage is the normal-flow corpus; fixtures that need
//! inline-block, floats or flex are scored too but are only asserted from their
//! own stage on, so a regression in a finished stage is never hidden behind an
//! unfinished one.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use scout_render::{text, Viewport};
use serde_json::Value;

const TOLERANCE: f32 = 2.0;

/// The whole corpus. Stage 2 covered normal flow, stage 3 inline-block and floats,
/// stage 4 flex; every fixture is asserted now that all four stages have landed.
const ALL: &[&str] = &[
    "01-block-simple",
    "02-margin-collapse",
    "03-padding-border",
    "04-inline-text",
    "05-text-align",
    "06-nested-widths",
    "07-inline-block",
    "08-fonts",
    "09-whitespace",
    "10-float",
    "11-list",
    "12-flex-row",
    "13-flex-props",
    "14-display-none",
    "15-cascade",
    "16-inherit",
];

fn key(path: &[usize]) -> String {
    path.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(".")
}

fn load_rects(node: &Value, out: &mut BTreeMap<String, (String, [f32; 4])>) {
    let path: Vec<usize> = node["i"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_u64().unwrap() as usize)
        .collect();
    let r = node["rect"].as_array().unwrap();
    let rect = [
        r[0].as_f64().unwrap() as f32,
        r[1].as_f64().unwrap() as f32,
        r[2].as_f64().unwrap() as f32,
        r[3].as_f64().unwrap() as f32,
    ];
    out.insert(
        key(&path),
        (node["tag"].as_str().unwrap_or("").to_string(), rect),
    );
    for child in node["children"].as_array().unwrap() {
        load_rects(child, out);
    }
}

struct Score {
    fixture: String,
    passed: usize,
    total: usize,
    worst: f32,
    failures: Vec<String>,
}

fn score_fixture(name: &str, fonts: &mut text::FontContext) -> Score {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let html = fs::read_to_string(root.join(format!("corpus/fixtures/{}.html", name))).unwrap();
    let golden: Value = serde_json::from_str(
        &fs::read_to_string(root.join(format!("corpus/goldens/{}.json", name))).unwrap(),
    )
    .unwrap();
    let mut expected = BTreeMap::new();
    load_rects(&golden["root"], &mut expected);

    let (doc, _styles, tree) = scout_render::render(&html, Viewport::default(), fonts);

    let mut passed = 0;
    let mut total = 0;
    let mut worst = 0.0f32;
    let mut failures = Vec::new();
    for id in doc.elements() {
        let path = doc.element_path(id);
        let k = key(&path);
        let Some((tag, want)) = expected.get(&k) else {
            continue;
        };
        let got = tree.rect(id).unwrap_or_default();
        let got = [got.x, got.y, got.width, got.height];
        total += 1;
        let error = (0..4)
            .map(|i| (got[i] - want[i]).abs())
            .fold(0.0f32, f32::max);
        worst = worst.max(error);
        if error <= TOLERANCE {
            passed += 1;
        } else {
            failures.push(format!(
                "  {} <{}> want {:?} got {:?} (Δ {:.1})",
                k, tag, want, got, error
            ));
        }
    }
    Score {
        fixture: name.to_string(),
        passed,
        total,
        worst,
        failures,
    }
}

#[test]
fn layout_boxes_match_chromium() {
    let mut fonts = text::FontContext::new(text::root_font_size());
    let mut report = String::from("Stages 2-4 — layout vs Chromium (tolerance ±2px)\n");
    report.push_str("fixture                     boxes   pass    rate   worst\n");

    let mut all_pass = true;
    let mut total_passed = 0;
    let mut total_boxes = 0;

    for name in ALL {
        let s = score_fixture(name, &mut fonts);
        let rate = s.passed as f32 / s.total as f32;
        report.push_str(&format!(
            "{:<26} {:>4}   {:>4}   {:>5.1}%   {:>5.1}px\n",
            s.fixture,
            s.total,
            s.passed,
            rate * 100.0,
            s.worst
        ));
        total_passed += s.passed;
        total_boxes += s.total;
        if rate < 0.95 {
            all_pass = false;
            report.push_str(&format!("  {} below the 95% gate\n", s.fixture));
        }
        for f in s.failures.iter().take(8) {
            report.push_str(f);
            report.push('\n');
        }
    }

    let rate = total_passed as f32 / total_boxes as f32;
    report.push_str(&format!(
        "\noverall {}/{} boxes = {:.2}% (gate ≥95% per fixture)\n",
        total_passed,
        total_boxes,
        rate * 100.0
    ));

    let _ = fs::write(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("target/golden-layout-report.txt"),
        &report,
    );
    println!("{}", report);

    assert!(total_boxes > 0, "no boxes compared — corpus missing");
    assert!(all_pass, "layout gate: a fixture fell below 95% of boxes within ±2px");
}
