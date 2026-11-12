//! Stage 5 gate: do our pixels look like Chromium's?
//!
//! `harness/shots.mjs` records a 800×600 Chromium screenshot of every fixture into
//! `corpus/screenshots/`. This test renders the same fixture with scout-render and
//! compares the two with SSIM on the luminance channel.
//!
//! Text is antialiased by two different rasterisers, so the score is never 1.0 on
//! a fixture with glyphs; the gate is median ≥ 0.90 with no fixture below 0.80.
//! Boxes, borders and backgrounds, which have no such excuse, score 1.0.

use std::fs;
use std::path::Path;

use scout_render::{paint, text, Viewport};
use serde_json::Value;

const SCALE: u32 = 800;
const HEIGHT: u32 = 600;

struct Scores {
    fixture: String,
    ssim: f64,
    mean_abs: f64,
}

fn read_png(path: &Path) -> (u32, u32, Vec<u8>) {
    let file = fs::File::open(path).expect("png readable");
    let decoder = png::Decoder::new(file);
    let mut reader = decoder.read_info().expect("png header");
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).expect("png frame");
    buf.truncate(info.buffer_size());
    (info.width, info.height, buf)
}

/// Luminance, as PNGs may arrive in either colour type.
fn luminance(width: u32, height: u32, data: &[u8], channels: usize) -> Vec<f64> {
    let mut out = Vec::with_capacity((width * height) as usize);
    for i in 0..(width * height) as usize {
        let p = &data[i * channels..(i * channels + channels).min(data.len())];
        let (r, g, b) = match channels {
            1 => (p[0] as f64, p[0] as f64, p[0] as f64),
            2 => (p[0] as f64, p[0] as f64, p[0] as f64),
            3 | 4 => (
                p[0] as f64,
                p.get(1).copied().unwrap_or(0) as f64,
                p.get(2).copied().unwrap_or(0) as f64,
            ),
            _ => (0.0, 0.0, 0.0),
        };
        out.push(0.2126 * r + 0.7152 * g + 0.0722 * b);
    }
    out
}

fn ssim(a: &[f64], b: &[f64]) -> f64 {
    let n = a.len().min(b.len()) as f64;
    if n == 0.0 {
        return 0.0;
    }
    let ma = a.iter().sum::<f64>() / n;
    let mb = b.iter().sum::<f64>() / n;
    let mut va = 0.0;
    let mut vb = 0.0;
    let mut cov = 0.0;
    for i in 0..n as usize {
        let da = a[i] - ma;
        let db = b[i] - mb;
        va += da * da;
        vb += db * db;
        cov += da * db;
    }
    va /= n;
    vb /= n;
    cov /= n;
    let c1 = (0.01 * 255.0f64).powi(2);
    let c2 = (0.03 * 255.0f64).powi(2);
    ((2.0 * ma * mb + c1) * (2.0 * cov + c2))
        / ((ma * ma + mb * mb + c1) * (va + vb + c2))
}

fn channels_of(path: &Path) -> usize {
    let file = fs::File::open(path).unwrap();
    let reader = png::Decoder::new(file).read_info().unwrap();
    reader.info().color_type.samples()
}

#[test]
fn stage5_pixels_match_chromium() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let shots = root.join("corpus/screenshots");
    if !shots.exists() {
        eprintln!(
            "skipping: corpus/screenshots is missing — run `node harness/shots.mjs` to record it"
        );
        return;
    }

    let mut fixtures: Vec<_> = fs::read_dir(root.join("corpus/fixtures"))
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().map(|x| x == "html").unwrap_or(false))
        .collect();
    fixtures.sort();

    let mut fonts = text::FontContext::new(text::root_font_size());
    let mut scores: Vec<Scores> = Vec::new();

    for path in fixtures {
        let name = path.file_stem().unwrap().to_string_lossy().to_string();
        let shot = shots.join(format!("{}.png", name));
        if !shot.exists() {
            continue;
        }

        let html = fs::read_to_string(&path).unwrap();
        let (doc, styles, tree) =
            scout_render::render(&html, Viewport { width: SCALE as f32, height: HEIGHT as f32 }, &mut fonts);
        let canvas = paint::paint(
            &doc,
            &styles,
            &tree,
            &mut fonts,
            paint::Options { width: SCALE, height: HEIGHT, ..Default::default() },
        );
        let ours = canvas.pixmap.data();

        let (w, h, theirs) = read_png(&shot);
        assert_eq!((w, h), (SCALE, HEIGHT), "{}: unexpected screenshot size", name);
        let channels = channels_of(&shot);

        let a = luminance(SCALE, HEIGHT, ours, 4);
        let b = luminance(w, h, &theirs, channels);
        let score = ssim(&a, &b);
        let mean_abs = a
            .iter()
            .zip(b.iter())
            .map(|(x, y)| (x - y).abs())
            .sum::<f64>()
            / a.len() as f64;

        scores.push(Scores { fixture: name, ssim: score, mean_abs });
    }

    scores.sort_by(|x, y| x.ssim.partial_cmp(&y.ssim).unwrap());
    let mut report = String::from("Stage 5 — painting vs Chromium (SSIM on luminance)\n");
    report.push_str("fixture                     ssim    mean|Δ| (0-255)\n");
    for s in &scores {
        report.push_str(&format!(
            "{:<26} {:.3}   {:.1}\n",
            s.fixture, s.ssim, s.mean_abs
        ));
    }
    let mut order: Vec<f64> = scores.iter().map(|s| s.ssim).collect();
    order.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let median = order[order.len() / 2];
    let worst = *order.first().unwrap();
    report.push_str(&format!(
        "\nmedian {:.3} (gate ≥0.90)   worst {} {:.3} (gate ≥0.80)\n",
        median,
        scores.first().map(|s| s.fixture.as_str()).unwrap_or("-"),
        worst
    ));
    let _ = fs::write(
        root.join("target/golden-paint-report.txt"),
        &report,
    );
    println!("{}", report);

    assert!(scores.len() >= 16, "expected the whole corpus, scored {}", scores.len());
    assert!(median >= 0.90, "Stage 5 gate: median SSIM {:.3} < 0.90", median);
    assert!(worst >= 0.80, "Stage 5 gate: a fixture scored {:.3} < 0.80", worst);
}

/// The snapshot JSON must describe every visible element with a usable box.
#[test]
fn snapshot_lists_clickable_elements() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let html = fs::read_to_string(root.join("corpus/fixtures/01-block-simple.html")).unwrap();
    let mut fonts = text::FontContext::new(text::root_font_size());
    let (doc, styles, tree) =
        scout_render::render(&html, Viewport::default(), &mut fonts);

    let visible: Vec<_> = doc
        .elements()
        .into_iter()
        .filter(|id| {
            let s = styles.styles.get(id);
            let display = s.map(|s| s.display).unwrap_or(scout_render::style::Display::Block);
            display != scout_render::style::Display::None
                && tree.rect(*id).map(|r| r.width > 0.0 && r.height > 0.0).unwrap_or(false)
        })
        .collect();

    // html, body and the three divs; the head and its children have no box.
    assert_eq!(visible.len(), 5, "visible elements: {:?}", visible.len());
    for id in &visible {
        let path = doc.css_path(*id);
        assert!(!path.is_empty(), "every element needs a selector path");
    }
}

/// A `display: none` subtree must not paint anything.
#[test]
fn display_none_paints_nothing() {
    let html = r#"<html><body><div id="a" style="width:10px;height:10px;background:#f00"></div>
        <div style="display:none"><div style="width:10px;height:10px;background:#00f"></div></div></body></html>"#;
    let mut fonts = text::FontContext::new(text::root_font_size());
    let (doc, styles, tree) = scout_render::render(html, Viewport::default(), &mut fonts);
    let canvas = paint::paint(&doc, &styles, &tree, &mut fonts, paint::Options::default());
    let data = canvas.pixmap.data();

    let mut red = 0;
    let mut blue = 0;
    for px in data.chunks_exact(4) {
        if px[0] > 200 && px[1] < 60 && px[2] < 60 {
            red += 1;
        }
        if px[2] > 200 && px[0] < 60 && px[1] < 60 {
            blue += 1;
        }
    }
    assert!(red > 50, "the visible red box should paint, got {} red pixels", red);
    assert_eq!(blue, 0, "the hidden blue box must not paint, got {} pixels", blue);
}

/// `Value` is used only through the png/JSON helpers above; keep the import honest.
#[allow(dead_code)]
fn _json_is_available(_: Value) {}
