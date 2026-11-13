use std::sync::Arc;
use std::time::{Duration, Instant};

fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((n >> 18) & 63) as usize] as char);
        out.push(TABLE[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 { TABLE[((n >> 6) & 63) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { TABLE[(n & 63) as usize] as char } else { '=' });
    }
    out
}

#[test]
fn local_files_blocked_by_default_and_in_render_scope() {
    let probe = std::env::temp_dir().join("scout_render_probe_secret.svg");
    std::fs::write(
        &probe,
        br#"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16"><rect width="16" height="16" fill="red"/></svg>"#,
    )
    .unwrap();
    let file_url = format!("file://{}", probe.display());
    let abs_path = probe.display().to_string();

    // Default library mode (no CLI configure): local files are strictly off.
    assert!(scout_render::image::resolve(&file_url).is_none());
    assert!(scout_render::image::resolve(&abs_path).is_none());
    assert!(scout_render::image::resolve("Cargo.toml").is_none());

    let seen_urls = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let seen_clone = seen_urls.clone();
    let fetcher: scout_render::image::Fetcher = Arc::new(move |url: &str| {
        seen_clone.lock().unwrap().push(url.to_string());
        if url == "https://app.example/assets/ok.svg" {
            Some(
                br#"<svg xmlns="http://www.w3.org/2000/svg" width="12" height="12"><rect width="12" height="12" fill="green"/></svg>"#
                    .to_vec(),
            )
        } else {
            None
        }
    });

    scout_render::image::with_render_scope("https://app.example/dir/page.html", fetcher, 1024, || {
        // `file://` is rejected outright; `/tmp/...` and `Cargo.toml` never read local disk
        // (even though both exist on disk!) and instead resolve against the HTTPS origin.
        assert!(scout_render::image::resolve(&file_url).is_none());
        assert!(scout_render::image::decode_for(&abs_path, None).is_none());
        assert!(scout_render::image::decode_for("Cargo.toml", None).is_none());
        assert!(scout_render::image::decode_for("/assets/ok.svg", None).is_some());
        // Budget exhaustion blocks excessive inline data: URIs too.
        let big_data = format!("data:text/plain,{}", "a".repeat(2048));
        assert!(scout_render::image::resolve(&big_data).is_none());
    });
    let urls = seen_urls.lock().unwrap().clone();
    assert!(!urls.iter().any(|u| u.starts_with("file:")));
    assert!(urls.iter().all(|u| u.starts_with("https://app.example/")));
    let _ = std::fs::remove_file(&probe);
}

#[test]
fn malicious_woff_orig_len_does_not_oom_or_abort() {
    // WOFF1 header claiming 1 table with orig_len = u32::MAX (4 GiB) in 64 bytes.
    let mut woff = vec![0u8; 64];
    woff[0..4].copy_from_slice(b"wOFF");
    woff[4..8].copy_from_slice(&0x0001_0000u32.to_be_bytes());
    let woff_len = woff.len() as u32;
    woff[8..12].copy_from_slice(&woff_len.to_be_bytes());
    woff[12..14].copy_from_slice(&1u16.to_be_bytes());
    woff[16..20].copy_from_slice(&u32::MAX.to_be_bytes());
    woff[44..48].copy_from_slice(b"head");
    woff[48..52].copy_from_slice(&60u32.to_be_bytes());
    woff[52..56].copy_from_slice(&4u32.to_be_bytes());
    woff[56..60].copy_from_slice(&u32::MAX.to_be_bytes());

    let data_url = format!("data:font/woff;base64,{}", base64_encode(&woff));
    let mut fonts = scout_render::text::FontContext::new(16.0);
    fonts.set_web_fonts(vec![scout_render::text::WebFont {
        family: "evilfont".to_string(),
        urls: vec![data_url],
        weight: 400,
        italic: false,
    }]);
    let w = fonts.measure("evilfont", 400, 16.0, "hello");
    assert!(w.is_finite() && w > 0.0);
}

#[test]
fn extreme_font_size_and_shadow_blur_do_not_panic_or_hang() {
    let html = r#"<!doctype html>
    <style>
      .huge { font-size: 999999999999px; box-shadow: 0 0 1e30px rgba(0, 0, 0, 0.5); }
    </style>
    <div class="huge">Wg</div>"#;
    let viewport = scout_render::Viewport { width: 400.0, height: 300.0 };
    let mut fonts = scout_render::text::FontContext::new(16.0);
    let started = Instant::now();
    let (doc, styles, tree) = scout_render::render(html, viewport, &mut fonts);
    let canvas = scout_render::paint(
        &doc,
        &styles,
        &tree,
        &mut fonts,
        scout_render::paint::Options {
            width: 400,
            height: 300,
            ..Default::default()
        },
    );
    assert_eq!(canvas.width(), 400);
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
fn css_var_billion_laughs_cycles_and_forward_refs() {
    let mut vars = String::from(":root { --v0: 10px 10px; --cycle-a: var(--cycle-b, 12px); --cycle-b: var(--cycle-a, 14px); ");
    for i in 1..=28 {
        vars.push_str(&format!(
            "--v{i}: var(--v{prev}) var(--v{prev}) var(--v{prev}) var(--v{prev}); ",
            prev = i - 1
        ));
    }
    vars.push_str("}");
    let html = format!(
        r#"<!doctype html><style>{vars}
        #fwd {{ color: var(--brand); --brand: rgb(12, 34, 56); font-size: var(--cycle-a); margin: var(--v28); }}
        </style><div id="fwd">ok</div>"#
    );
    let viewport = scout_render::Viewport { width: 800.0, height: 600.0 };
    let mut fonts = scout_render::text::FontContext::new(16.0);
    let started = Instant::now();
    let (doc, styles, _tree) = scout_render::render(&html, viewport, &mut fonts);
    assert!(started.elapsed() < Duration::from_secs(1));
    let id = doc
        .nodes
        .iter()
        .position(|n| n.attr("id") == Some("fwd"))
        .expect("node #fwd");
    assert_eq!(styles.styles[&id].color, scout_render::css::Color::rgb(12, 34, 56));
}

#[test]
fn deeply_nested_flexbox_is_memoized() {
    let mut html = String::from("<!doctype html>");
    for _ in 0..30 {
        html.push_str("<div style='display:flex'>");
    }
    html.push_str("<span>leaf text inside nested flex</span>");
    for _ in 0..30 {
        html.push_str("</div>");
    }
    let viewport = scout_render::Viewport { width: 800.0, height: 600.0 };
    let mut fonts = scout_render::text::FontContext::new(16.0);
    let started = Instant::now();
    let (_doc, _styles, tree) = scout_render::render(&html, viewport, &mut fonts);
    assert!(!tree.rects.is_empty());
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "nested flex took {:?}",
        started.elapsed()
    );
}

#[test]
fn deep_dom_does_not_overflow_2mb_worker_stack() {
    let handle = std::thread::Builder::new()
        .stack_size(2 * 1024 * 1024)
        .spawn(|| {
            let depth = 5_000;
            let mut html = String::with_capacity(depth * 11 + 32);
            for _ in 0..depth {
                html.push_str("<div>");
            }
            html.push_str("bottom");
            for _ in 0..depth {
                html.push_str("</div>");
            }
            let viewport = scout_render::Viewport { width: 640.0, height: 480.0 };
            let mut fonts = scout_render::text::FontContext::new(16.0);
            let (doc, styles, tree) = scout_render::render(&html, viewport, &mut fonts);
            let _canvas = scout_render::paint(
                &doc,
                &styles,
                &tree,
                &mut fonts,
                scout_render::paint::Options {
                    width: 320,
                    height: 240,
                    ..Default::default()
                },
            );
        })
        .unwrap();
    handle.join().expect("2 MiB worker thread should not overflow stack");
}

#[test]
fn oversized_canvas_and_svg_dimensions_are_clamped() {
    let (cw, ch) = scout_render::paint::clamp_canvas_dimensions(800, 10_000_000);
    assert!(cw <= scout_render::paint::MAX_CANVAS_DIM);
    assert!(ch <= scout_render::paint::MAX_CANVAS_DIM);
    assert!(u64::from(cw) * u64::from(ch) <= scout_render::paint::MAX_CANVAS_PIXELS);

    let svg = r#"data:image/svg+xml,<svg xmlns="http://www.w3.org/2000/svg" width="50000" height="50000"><rect width="50000" height="50000" fill="blue"/></svg>"#;
    let bmp = scout_render::image::decode_for(svg, None).expect("scaled svg");
    assert!(bmp.width <= 2048 && bmp.height <= 2048);
}

#[test]
fn snapshot_json_stays_valid_when_first_boxes_are_offscreen_or_non_finite() {
    let html = r#"<!doctype html>
    <div style="margin-top: -500px; height: 50px"><a href="/off">off</a></div>
    <div style="margin-top: 600px; height: 40px"><a href="/on">on</a></div>"#;
    let viewport = scout_render::Viewport { width: 800.0, height: 600.0 };
    let mut fonts = scout_render::text::FontContext::new(16.0);
    let (doc, styles, mut tree) = scout_render::render(html, viewport, &mut fonts);
    if let Some((_k, rect)) = tree.rects.iter_mut().next() {
        rect.width = f32::NAN;
    }
    let json_text =
        scout_render::snapshot_json(&doc, &styles, &tree, viewport, &mut fonts, true, 0);
    let parsed: serde_json::Value =
        serde_json::from_str(&json_text).expect("snapshot_json must emit valid JSON");
    assert!(parsed["elements"].is_array());
}

#[test]
fn multibyte_invalid_css_colors_and_selectors_never_panic() {
    assert!(scout_render::css::value::parse_color("#€").is_none());
    assert!(scout_render::css::value::parse_color("rgb) (").is_none());
    assert!(scout_render::css::value::BackgroundImage::parse("linear-gradient) (").is_none());
    let sheet = scout_render::css::Stylesheet::parse(
        "@€€€€€€€€€€€€€€€€ { color: #€; } ??€€€€€€€€ { width: 10px; }",
    );
    assert!(sheet.rules.is_empty());
}

#[test]
fn svg_pattern_pixmap_bomb_is_rejected_without_aborting_process() {
    // Reproduces the live `redis.io` 73.4 GB `resvg::path::render_pattern_pixmap`
    // OOM abort: a modest 200x200 SVG whose `<pattern>` tile scales to 135494x135494.
    let html = r#"<!doctype html>
    <html><body>
      <h1>Before rogue pattern SVG</h1>
      <svg width="200" height="200" viewBox="0 0 200 200">
        <defs>
          <pattern id="huge" patternUnits="userSpaceOnUse" width="135494" height="135494">
            <rect width="100" height="100" fill="red"/>
          </pattern>
        </defs>
        <rect width="200" height="200" fill="url(#huge)"/>
      </svg>
      <p>After rogue pattern SVG</p>
    </body></html>"#;
    let viewport = scout_render::Viewport { width: 800.0, height: 600.0 };
    let mut fonts = scout_render::text::FontContext::new(16.0);
    let (doc, styles, tree) = scout_render::render(html, viewport, &mut fonts);
    let canvas = scout_render::paint(
        &doc,
        &styles,
        &tree,
        &mut fonts,
        scout_render::paint::Options {
            width: 800,
            height: 600,
            ..Default::default()
        },
    );
    let png = canvas.to_png().expect("png");
    assert!(png.starts_with(&[0x89, b'P', b'N', b'G']));
}

