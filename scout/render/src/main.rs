//! scout-render CLI: HTML + CSS to geometry and pixels, without a browser.
//!
//!   scout-render snapshot <file.html> [--width 800] [--height 600]
//!   scout-render screenshot <file.html> -o out.png [--width 800] [--height 600]
//!
//! `snapshot` prints the same JSON shape an agent needs to click a node: every
//! element with its box, its text and a CSS path.

use std::fs;
use std::process::ExitCode;

use scout_render::resource::ResourceLoader;
use scout_render::{paint, text, Viewport};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args[0] == "--help" || args[0] == "-h" {
        usage();
        return ExitCode::SUCCESS;
    }
    if args[0] == "--version" || args[0] == "-V" {
        println!("scout-render {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }

    let command = args[0].clone();
    let mut file: Option<String> = None;
    let mut output: Option<String> = None;
    let mut width = 800.0f32;
    let mut height = 600.0f32;
    let mut full_page = false;
    let mut pretty = true;
    // A saved page can name its own origin, so relative assets still resolve.
    let mut base_override: Option<String> = None;
    let mut offline = false;
    let mut quiet = false;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "-o" | "--output" => {
                i += 1;
                output = args.get(i).cloned();
            }
            "--width" => {
                i += 1;
                width = args.get(i).and_then(|v| v.parse().ok()).unwrap_or(width);
            }
            "--height" => {
                i += 1;
                height = args.get(i).and_then(|v| v.parse().ok()).unwrap_or(height);
            }
            "--full-page" => full_page = true,
            "--base" => {
                i += 1;
                base_override = args.get(i).cloned();
            }
            "--compact" => pretty = false,
            // Render what is on disk only: no stylesheet, font or image is fetched.
            "--offline" => offline = true,
            "--quiet" => quiet = true,
            other if other.starts_with('-') => {
                eprintln!("unknown option {}", other);
                return ExitCode::from(2);
            }
            other => file = Some(other.to_string()),
        }
        i += 1;
    }

    let Some(file) = file else {
        eprintln!("no input file");
        usage();
        return ExitCode::from(2);
    };
    // A URL is fetched first; a path is read. Everything downstream — relative
    // hrefs, relative image srcs — resolves against whichever it was.
    let (html, mut base) = if file.starts_with("http://") || file.starts_with("https://") {
        match fetch(&file) {
            Some(bytes) => (String::from_utf8_lossy(&bytes).into_owned(), file.clone()),
            None => {
                eprintln!("cannot fetch {}", file);
                return ExitCode::from(2);
            }
        }
    } else {
        match fs::read_to_string(&file) {
            Ok(h) => (h, file.clone()),
            Err(e) => {
                eprintln!("cannot read {}: {}", file, e);
                return ExitCode::from(2);
            }
        }
    };

    if let Some(override_base) = base_override {
        base = override_base;
    }
    let viewport = Viewport { width, height };
    let mut fonts = text::FontContext::new(text::root_font_size());
    scout_render::image::configure(
        &base,
        !offline,
        std::env::temp_dir().join("scout-render-images"),
    );
    let loader = CliLoader { network: !offline };
    let started = std::time::Instant::now();
    let (doc, styles, tree, failures) =
        scout_render::render_with(&html, &base, viewport, &mut fonts, &loader);
    let elapsed_ms = started.elapsed().as_millis();
    if !failures.is_empty() && !quiet {
        for (href, why) in failures.iter().take(8) {
            eprintln!("stylesheet {} {}", href, why);
        }
        if failures.len() > 8 {
            eprintln!("… and {} more", failures.len() - 8);
        }
    }

    match command.as_str() {
        "snapshot" => {
            let json = scout_render::snapshot_json(&doc, &styles, &tree, viewport, &mut fonts, pretty, elapsed_ms);
            match output {
                Some(path) => {
                    if let Err(e) = fs::write(&path, json) {
                        eprintln!("cannot write {}: {}", path, e);
                        return ExitCode::from(1);
                    }
                }
                None => println!("{}", json),
            }
            ExitCode::SUCCESS
        }
        "screenshot" => {
            // A full-page capture is the viewport wide and as tall as the
            // document, which is what a browser's full-page screenshot is: content
            // that overflows to the right stays cropped, and one absolutely
            // positioned decoration cannot turn the canvas into a gigabyte.
            let width = width as u32;
            let height = if full_page {
                tree.document_height.max(height).ceil() as u32
            } else {
                height as u32
            };
            // 64 MP is 256 MB of canvas; past that the honest answer is an error,
            // not an allocation the machine may not survive.
            if u64::from(width) * u64::from(height) > 64_000_000 {
                eprintln!(
                    "page needs a {}x{} canvas ({} MP, cap 64 MP); screenshot a viewport instead",
                    width, height, u64::from(width) * u64::from(height) / 1_000_000
                );
                return ExitCode::from(2);
            }
            let canvas = paint::paint(
                &doc,
                &styles,
                &tree,
                &mut fonts,
                paint::Options {
                    width,
                    height,
                    ..Default::default()
                },
            );
            let png = match canvas.to_png() {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("png encoding failed: {}", e);
                    return ExitCode::from(1);
                }
            };
            match output {
                Some(path) => {
                    if let Err(e) = fs::write(&path, &png) {
                        eprintln!("cannot write {}: {}", path, e);
                        return ExitCode::from(1);
                    }
                    eprintln!("wrote {} ({} bytes)", path, png.len());
                }
                None => {
                    use std::io::Write;
                    let _ = std::io::stdout().write_all(&png);
                }
            }
            ExitCode::SUCCESS
        }
        other => {
            eprintln!("unknown command {}", other);
            usage();
            ExitCode::from(2)
        }
    }
}

/// Fetch a URL as bytes, exactly as a browser would present it.
fn fetch(url: &str) -> Option<Vec<u8>> {
    let output = std::process::Command::new("curl")
        .args([
            "-sSL",
            "--max-time",
            "30",
            "--compressed",
            "-A",
            "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126 Safari/537.36",
        ])
        .arg(url)
        .output()
        .ok()?;
    if !output.status.success() || output.stdout.is_empty() {
        return None;
    }
    Some(output.stdout)
}

/// Stylesheets load from disk or, for a live page, over the network. The engine
/// itself stays offline; this is the one place that reaches out.
struct CliLoader {
    network: bool,
}

impl ResourceLoader for CliLoader {
    fn load(&self, url: &str) -> Option<Vec<u8>> {
        if url.starts_with("http://") || url.starts_with("https://") {
            if !self.network {
                return None;
            }
            return fetch(url);
        }
        fs::read(url.trim_start_matches("file://")).ok()
    }
}

fn usage() {
    eprintln!(
        "scout-render — HTML + CSS to geometry and pixels, without a browser

  scout-render snapshot   <file.html> [-o out.json] [--width 800] [--height 600] [--compact]
  scout-render screenshot <file.html> [-o out.png]  [--width 800] [--height 600] [--full-page]

  --base <url>    resolve the page's relative assets against <url> (a saved page)

<file.html> may be a path or an http(s) URL. Linked stylesheets and images load
relative to it; `--offline` keeps the render to the local disk.

Fonts come from SCOUT_RENDER_FONT, or the first DejaVu Sans found on the system."
    );
}
