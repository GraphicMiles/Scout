//! Loading the things a page pulls in: stylesheets, and the images the paint
//! stage asks for.
//!
//! The engine itself never opens a socket. A [`ResourceLoader`] is passed in, so
//! the same build renders a saved page from disk (tests, goldens) and a live URL
//! (the CLI). Anything that cannot be loaded is reported, not guessed.

use std::path::{Path, PathBuf};

/// Somewhere to get bytes from, given an absolute path or URL.
pub trait ResourceLoader {
    /// Bytes for `url`, or `None` when it cannot be loaded.
    fn load(&self, url: &str) -> Option<Vec<u8>>;
}

/// Loads nothing. The default, and what every test uses.
pub struct Offline;

impl ResourceLoader for Offline {
    fn load(&self, _url: &str) -> Option<Vec<u8>> {
        None
    }
}

/// Loads `file:` paths and plain filesystem paths.
pub struct Filesystem;

impl ResourceLoader for Filesystem {
    fn load(&self, url: &str) -> Option<Vec<u8>> {
        let path = url.trim_start_matches("file://");
        std::fs::read(path).ok()
    }
}

/// Join `href` onto the page's own address.
///
/// `base` is either a URL (`https://host/a/b.html`) or a filesystem path
/// (`/tmp/site/index.html`); both are handled because the same code loads a
/// fetched page and a saved one.
pub fn resolve(base: &str, href: &str) -> String {
    let href = href.trim();
    if href.starts_with("data:") || href.starts_with("http://") || href.starts_with("https://") {
        return href.to_string();
    }
    if href.starts_with("//") {
        // Protocol-relative: borrow the page's scheme.
        let scheme = base.split(':').next().unwrap_or("https");
        return format!("{}:{}", scheme, href);
    }
    if base.starts_with("http://") || base.starts_with("https://") {
        return resolve_url(base, href);
    }
    // Filesystem base: treat the page path as a directory prefix.
    let dir = Path::new(base).parent().unwrap_or_else(|| Path::new("."));
    join_path(dir, href)
}

/// Resolve `href` against an absolute URL, including `../` segments.
fn resolve_url(base: &str, href: &str) -> String {
    if href.starts_with('/') {
        let scheme_end = base.find("://").map(|i| i + 3).unwrap_or(0);
        let host_end = base[scheme_end..]
            .find('/')
            .map(|i| i + scheme_end)
            .unwrap_or(base.len());
        return format!("{}{}", &base[..host_end], href);
    }
    // A bare host ("https://example.com") has no path: its base directory is "/".
    let scheme_end = base.find("://").map(|i| i + 3).unwrap_or(0);
    let cut = match base.rfind('/') {
        Some(i) if i >= scheme_end => i + 1,
        _ => base.len() + 1,
    };
    let base = if cut > base.len() { format!("{}/", base) } else { base.to_string() };
    let base = base.as_str();
    let mut segments: Vec<&str> = base[..cut].trim_end_matches('/').split('/').collect();
    for part in href.split('/') {
        match part {
            "." | "" => {}
            ".." => {
                if segments.len() > 3 {
                    segments.pop();
                }
            }
            other => segments.push(other),
        }
    }
    segments.join("/")
}

fn join_path(dir: &Path, href: &str) -> String {
    let mut path = PathBuf::from(dir);
    for part in href.split('/') {
        match part {
            "." | "" => {}
            ".." => {
                path.pop();
            }
            other => path.push(other),
        }
    }
    path.to_string_lossy().into_owned()
}

/// One `<link rel="stylesheet">`, with what came of it.
pub struct LinkedSheet {
    /// The address as written in the document.
    pub href: String,
    /// The absolute address actually requested.
    pub url: String,
    /// The parsed CSS, when it loaded.
    pub css: Option<String>,
}

/// Find every linked stylesheet, in document order, and load it.
///
/// Returns the sheets plus a `(href, reason)` list for the ones that failed, so
/// a caller can report them instead of silently rendering an unstyled page.
pub fn load_stylesheets(
    doc: &crate::dom::Document,
    base: &str,
    loader: &dyn ResourceLoader,
) -> (Vec<LinkedSheet>, Vec<(String, String)>) {
    let mut sheets = Vec::new();
    let mut failures = Vec::new();
    for id in doc.elements() {
        let node = &doc.nodes[id];
        if node.name != "link" {
            continue;
        }
        let rel = node.attr("rel").unwrap_or("").to_ascii_lowercase();
        if !rel.split_whitespace().any(|r| r == "stylesheet") {
            continue;
        }
        // `media` that cannot match is skipped rather than applied.
        if let Some(media) = node.attr("media") {
            let media = media.to_ascii_lowercase();
            if media.contains("print") && !media.contains("screen") {
                continue;
            }
        }
        let Some(href) = node.attr("href") else { continue };
        let url = resolve(base, href);
        match loader.load(&url) {
            Some(bytes) => {
                let css = String::from_utf8_lossy(&bytes).into_owned();
                sheets.push(LinkedSheet { href: href.to_string(), url, css: Some(css) });
            }
            None => {
                failures.push((href.to_string(), "could not be loaded".to_string()));
                sheets.push(LinkedSheet { href: href.to_string(), url, css: None });
            }
        }
    }
    (sheets, failures)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absolute_urls_pass_through() {
        assert_eq!(resolve("https://a.test/x/", "https://b.test/y.css"), "https://b.test/y.css");
    }

    #[test]
    fn relative_urls_resolve_against_the_page() {
        assert_eq!(
            resolve("https://a.test/docs/page.html", "style.css"),
            "https://a.test/docs/style.css"
        );
        assert_eq!(
            resolve("https://a.test/docs/page.html", "../css/app.css"),
            "https://a.test/css/app.css"
        );
        assert_eq!(resolve("https://a.test/docs/page.html", "/root.css"), "https://a.test/root.css");
    }

    #[test]
    fn filesystem_bases_join_like_urls() {
        assert_eq!(resolve("/tmp/site/index.html", "css/a.css"), "/tmp/site/css/a.css");
        assert_eq!(resolve("/tmp/site/docs/index.html", "../a.css"), "/tmp/site/a.css");
    }

    #[test]
    fn protocol_relative_borrows_the_scheme() {
        assert_eq!(resolve("https://a.test/x.html", "//cdn.test/a.css"), "https://cdn.test/a.css");
    }
}
