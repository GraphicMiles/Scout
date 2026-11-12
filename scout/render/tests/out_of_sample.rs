//! An out-of-sample page: written after the engine passed its own corpus, to see
//! whether the rules generalise or were fitted to the fixtures.
//!
//! `corpus/extra/page.html` uses flex with `flex: 1` cards, inline-block tags with
//! margins, justified text, a nav with inline link margins and a list. Its boxes and
//! screenshot were recorded from Chromium by `harness/rects.mjs` and
//! `harness/demo-shot.mjs`. Nothing in the engine was tuned against this file
//! without fixing the general rule behind the difference, so it stays a real test.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use scout_render::{paint, text, Viewport};
use serde_json::Value;

const WIDTH: u32 = 800;
const HEIGHT: u32 = 600;

fn fixture(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("corpus/extra")
        .join(name)
}

#[test]
fn extra_page_boxes_match_chromium() {
    let html = fs::read_to_string(fixture("page.html")).expect("page.html");
    let golden: Value = serde_json::from_str(&fs::read_to_string(fixture("page.rects.json")).unwrap())
        .expect("page.rects.json");

    let mut expected: BTreeMap<String, ([f32; 4], String)> = BTreeMap::new();
    for row in golden.as_array().unwrap() {
        let rect = [
            row["x"].as_f64().unwrap() as f32,
            row["y"].as_f64().unwrap() as f32,
            row["w"].as_f64().unwrap() as f32,
            row["h"].as_f64().unwrap() as f32,
        ];
        expected.insert(
            row["path"].as_str().unwrap().to_string(),
            (rect, row["tag"].as_str().unwrap().to_string()),
        );
    }

    let mut fonts = text::FontContext::new(text::root_font_size());
    let (doc, _styles, tree) =
        scout_render::render(&html, Viewport { width: WIDTH as f32, height: HEIGHT as f32 }, &mut fonts);

    let mut checked = 0;
    let mut within = 0;
    let mut worst = 0.0f32;
    let mut lines = String::new();
    for id in doc.elements() {
        let path = doc
            .element_path(id)
            .iter()
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(".");
        let Some((want, tag)) = expected.get(&path) else { continue };
        let got = tree.rect(id).unwrap_or_default();
        let got = [got.x, got.y, got.width, got.height];
        let error = (0..4).map(|i| (got[i] - want[i]).abs()).fold(0.0f32, f32::max);
        checked += 1;
        worst = worst.max(error);
        if error <= 2.0 {
            within += 1;
        } else {
            lines.push_str(&format!(
                "  {} <{}> want {:?} got {:?}\n",
                path, tag, want, got
            ));
        }
    }

    println!(
        "out-of-sample: {}/{} boxes within ±2px, worst {:.2}px\n{}",
        within, checked, worst, lines
    );
    assert!(checked >= 30, "the page should have at least 30 boxes, got {}", checked);
    // Every box, not most of them: a page of ordinary markup has no excuse for a
    // single grossly misplaced box.
    assert_eq!(within, checked, "out-of-sample boxes drifted:\n{}", lines);
}

#[test]
fn extra_page_pixels_match_chromium() {
    let html = fs::read_to_string(fixture("page.html")).unwrap();
    let mut fonts = text::FontContext::new(text::root_font_size());
    let (doc, styles, tree) =
        scout_render::render(&html, Viewport { width: WIDTH as f32, height: HEIGHT as f32 }, &mut fonts);

    let shot = fixture("page.png");
    if !shot.exists() {
        eprintln!("skipping: corpus/extra/page.png is missing — run harness/demo-shot.mjs");
        return;
    }
    let height = tree.document_height.ceil() as u32;

    let canvas = paint::paint(
        &doc,
        &styles,
        &tree,
        &mut fonts,
        paint::Options { width: WIDTH, height, ..Default::default() },
    );

    let file = fs::File::open(&shot).unwrap();
    let mut reader = png::Decoder::new(file).read_info().unwrap();
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).unwrap();
    let channels = info.color_type.samples();

    let w = WIDTH.min(info.width) as usize;
    let h = height.min(info.height) as usize;
    let ours = canvas.pixmap.data();

    let mut sum_a = 0.0;
    let mut sum_b = 0.0;
    let mut n = 0.0;
    let mut va = 0.0;
    let mut vb = 0.0;
    let mut cov = 0.0;
    let lum_a: Vec<f64> = (0..w * h)
        .map(|i| {
            let p = &ours[i * 4..(i * 4 + 3).min(ours.len())];
            if p.len() == 3 { 0.2126 * p[0] as f64 + 0.7152 * p[1] as f64 + 0.0722 * p[2] as f64 } else { 0.0 }
        })
        .collect();
    let lum_b: Vec<f64> = (0..w * h)
        .map(|i| {
            let row = i / w;
            let col = i % w;
            let idx = (row * info.width as usize + col) * channels;
            let p = &buf[idx..(idx + 3).min(buf.len())];
            if p.len() == 3 { 0.2126 * p[0] as f64 + 0.7152 * p[1] as f64 + 0.0722 * p[2] as f64 } else { 0.0 }
        })
        .collect();

    for i in 0..lum_a.len() {
        sum_a += lum_a[i];
        sum_b += lum_b[i];
    }
    let ma = sum_a / lum_a.len() as f64;
    let mb = sum_b / lum_b.len() as f64;
    for i in 0..lum_a.len() {
        let da = lum_a[i] - ma;
        let db = lum_b[i] - mb;
        va += da * da;
        vb += db * db;
        cov += da * db;
    }
    n += lum_a.len() as f64;
    va /= n;
    vb /= n;
    cov /= n;
    let c1 = (0.01 * 255.0f64).powi(2);
    let c2 = (0.03 * 255.0f64).powi(2);
    let ssim = ((2.0 * ma * mb + c1) * (2.0 * cov + c2)) / ((ma * ma + mb * mb + c1) * (va + vb + c2));
    println!("out-of-sample page SSIM {:.3} (whole page, same width)", ssim);
    assert!(ssim >= 0.90, "out-of-sample SSIM {:.3} < 0.90", ssim);
}
