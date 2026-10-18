//! The loaded-document model held by a session.

use scraper::Html;
use serde_json::Value;
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageKind {
    Html,
    Json,
}

/// One loaded document.
///
/// `form_state` is owned by the page rather than the session, so navigating away
/// drops any typed values along with the DOM they belong to. That is the same
/// lifetime guarantee the reference implementation got from a WeakMap, made
/// explicit here. Values are a list per element because a `<select multiple>`
/// legitimately holds several.
#[derive(Debug)]
pub struct Page {
    pub url: String,
    pub status: u16,
    pub content_type: String,
    pub bytes: usize,
    pub kind: PageKind,
    pub doc: Option<Html>,
    pub json: Option<Value>,
    pub warnings: Vec<Value>,
    pub form_state: HashMap<String, Vec<String>>,
    pub redirects: Vec<Value>,
    /// Web Storage for the page's session: `{local: {...}, session: {...}}`.
    /// Carried across navigations, because that is what a browser does; both
    /// die with the session.
    pub storage: Value,
    /// Files staged on `input[type=file]` controls, keyed by the control's
    /// canonical selector, exactly like `form_state`.
    pub files: HashMap<String, StoredFile>,
    /// URLs this session visited, in order, with the current position. The
    /// back and forward actions walk it exactly like a browser.
    pub history: Vec<String>,
    pub history_pos: usize,
    /// The page's own script bodies in document order, kept so a later event
    /// pass (a scroll, a key press) can re-run them: listeners die with the
    /// context that registered them, so the scripts must run again before the
    /// event is dispatched into the page.
    pub scripts: Vec<crate::js::Script>,
}

/// A file staged on an `input[type=file]` control by an `upload` action.
#[derive(Debug, Clone)]
pub struct StoredFile {
    pub filename: String,
    pub bytes: Vec<u8>,
}

impl Page {
    pub fn html(
        url: String,
        status: u16,
        content_type: String,
        bytes: usize,
        doc: Html,
        warnings: Vec<Value>,
        redirects: Vec<Value>,
    ) -> Self {
        Self {
            url,
            status,
            content_type,
            bytes,
            kind: PageKind::Html,
            doc: Some(doc),
            json: None,
            warnings,
            form_state: HashMap::new(),
            redirects,
            scripts: Vec::new(),
            storage: serde_json::json!({ "local": {}, "session": {} }),
            files: HashMap::new(),
            history: Vec::new(),
            history_pos: 0,
        }
    }

    pub fn json_page(
        url: String,
        status: u16,
        content_type: String,
        bytes: usize,
        json: Value,
        warnings: Vec<Value>,
        redirects: Vec<Value>,
    ) -> Self {
        Self {
            url,
            status,
            content_type,
            bytes,
            kind: PageKind::Json,
            doc: None,
            json: Some(json),
            warnings,
            scripts: Vec::new(),
            form_state: HashMap::new(),
            redirects,
            storage: serde_json::json!({ "local": {}, "session": {} }),
            files: HashMap::new(),
            history: Vec::new(),
            history_pos: 0,
        }
    }

    pub fn title(&self) -> Option<String> {
        self.doc.as_ref().and_then(crate::html::title)
    }

    /// A token-cheap page summary for agents: what the page is about and what
    /// can be done on it, without shipping the whole document. Everything is
    /// capped so a huge page cannot blow the context window.
    pub fn digest(&self) -> serde_json::Value {
        use serde_json::json;
        let Some(doc) = self.doc.as_ref() else {
            return serde_json::Value::Null;
        };
        let base = url::Url::parse(&self.url).ok();

        let headings = |tags: &[&str]| -> serde_json::Value {
            let mut out: Vec<String> = Vec::new();
            for tag in tags {
                if let Ok(sel) = crate::selector::compile(tag) {
                    for el in doc.select(&sel) {
                        let t = el.text().collect::<String>().split_whitespace().collect::<Vec<_>>().join(" ");
                        let t = t.chars().take(120).collect::<String>();
                        if !t.is_empty() && !out.contains(&t) {
                            out.push(t);
                        }
                        if out.len() >= 5 {
                            break;
                        }
                    }
                }
                if out.len() >= 5 {
                    break;
                }
            }
            json!(out)
        };

        let mut links: Vec<String> = Vec::new();
        if let Ok(sel) = crate::selector::compile("a[href]") {
            for el in doc.select(&sel) {
                let href = el.value().attr("href").unwrap_or_default();
                let abs = base
                    .as_ref()
                    .and_then(|b| b.join(href).ok())
                    .map(|u| u.to_string())
                    .unwrap_or_else(|| href.to_string());
                let label = el.text().collect::<String>().split_whitespace().collect::<Vec<_>>().join(" ");
                let label = label.chars().take(60).collect::<String>();
                let entry = if label.is_empty() {
                    abs
                } else {
                    format!("{label} -> {abs}")
                };
                if !links.contains(&entry) {
                    links.push(entry);
                }
                if links.len() >= 12 {
                    break;
                }
            }
        }

        let mut forms: Vec<serde_json::Value> = Vec::new();
        if let Ok(sel) = crate::selector::compile("form") {
            for el in doc.select(&sel) {
                let action = el.value().attr("action").unwrap_or("");
                let action = base
                    .as_ref()
                    .and_then(|b| b.join(action).ok())
                    .map(|u| u.to_string())
                    .unwrap_or_else(|| action.to_string());
                let method = el.value().attr("method").unwrap_or("GET").to_ascii_uppercase();
                let mut fields: Vec<String> = Vec::new();
                if let Ok(field_sel) = crate::selector::compile("input, select, textarea") {
                    for f in el.select(&field_sel) {
                        if let Some(name) = f.value().attr("name") {
                            let kind = f.value().name();
                            if !fields.contains(&name.to_string()) {
                                fields.push(format!("{kind}[name={name}]"));
                            }
                        }
                        if fields.len() >= 12 {
                            break;
                        }
                    }
                }
                forms.push(json!({ "action": action, "method": method, "fields": fields }));
                if forms.len() >= 5 {
                    break;
                }
            }
        }

        let mut buttons: Vec<String> = Vec::new();
        if let Ok(sel) = crate::selector::compile("button, input[type=submit], input[type=button]") {
            for el in doc.select(&sel) {
                let label = el
                    .text()
                    .collect::<String>()
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ");
                let label = if label.is_empty() {
                    el.value().attr("value").unwrap_or_default().to_string()
                } else {
                    label
                };
                let label = label.chars().take(60).collect::<String>();
                if !label.is_empty() && !buttons.contains(&label) {
                    buttons.push(label);
                }
                if buttons.len() >= 12 {
                    break;
                }
            }
        }

        json!({
            "title": self.title(),
            "url": self.url,
            "headings": {
                "h1": headings(&["h1"]),
                "h2": headings(&["h2"]),
                "h3": headings(&["h3"]),
            },
            "links": links,
            "forms": forms,
            "buttons": buttons,
        })
    }
}

/// The target of `<meta http-equiv="refresh" content="N; url=U">`, resolved
/// against the page URL. Attribute names and the "refresh" value are matched
/// case-insensitively, single or double quotes are accepted, and a plain
/// `content="N"` (reload the same page) is ignored: reloading buys a caller
/// nothing and risks a loop.
pub fn meta_refresh(html: &str, base: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let mut at = 0usize;
    while let Some(start) = lower[at..].find("<meta") {
        let start = at + start;
        let end = lower[start..].find('>').map(|e| start + e)?;
        let tag = &html[start..end];
        at = end;
        let attrs = parse_attrs(tag);
        let equiv = attrs
            .iter()
            .find(|(k, _)| k == "http-equiv")
            .map(|(_, v)| v.to_ascii_lowercase())?;
        if equiv != "refresh" {
            continue;
        }
        let content = attrs
            .iter()
            .find(|(k, _)| k == "content")
            .map(|(_, v)| v.clone())?;
        // content = delay [ ";" [ url= ] target ]
        let after_delay = content.split(';').nth(1)?;
        let target = after_delay.trim();
        let target = target
            .strip_prefix("url=")
            .or_else(|| target.strip_prefix("URL="))
            .unwrap_or(target)
            .trim()
            .trim_matches(|c| c == '\'' || c == '"');
        if target.is_empty() {
            return None;
        }
        return url::Url::parse(base)
            .and_then(|b| b.join(target))
            .ok()
            .map(|u| u.to_string());
    }
    None
}

/// Split `<meta a="b" c='d' e>` into (key, value) pairs, keys lowercased,
/// values unquoted. Values without quotes are kept verbatim.
fn parse_attrs(tag: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut fields = tag.split_ascii_whitespace().skip(1); // skip "<meta"
    while let Some(field) = fields.next() {
        let Some(eq) = field.find('=') else {
            out.push((field.to_ascii_lowercase(), String::new()));
            continue;
        };
        let key = field[..eq].to_ascii_lowercase();
        let mut value = field[eq + 1..].to_string();
        if (value.starts_with('"') && !value.ends_with('"'))
            || (value.starts_with('\'') && !value.ends_with('\''))
        {
            // The value contains a space and was split: glue it back.
            if let Some((quote, _)) = value.chars().enumerate().find(|(i, c)| {
                *i == 0 && (*c == '"' || *c == '\'')
            }) {
                let close = value.chars().nth(quote).unwrap();
                let mut glued = value.clone();
                for field in fields.by_ref() {
                    glued.push(' ');
                    glued.push_str(field);
                    if field.ends_with(close) {
                        break;
                    }
                }
                value = glued;
            }
        }
        let value = value
            .trim_matches(|c| c == '"' || c == '\'')
            .to_string();
        out.push((key, value));
    }
    out
}


#[cfg(test)]
mod digest_tests {
    use super::*;

    #[test]
    fn digest_summarizes_a_page() {
        let html = r#"<html><head><title>Shop</title></head><body>
        <h1>Widgets</h1><h2>Catalog</h2><h2>Catalog</h2>
        <a href="/p/1">First</a><a href="https://other.example/x">Elsewhere</a>
        <form action="/search" method="post"><input name="q"><select name="c"></select></form>
        <button>Buy</button><input type="submit" value="Go">
        </body></html>"#;
        let doc = crate::html::parse_html(html, 150_000).unwrap();
        let p = Page::html(
            "https://example.com/shop".into(),
            200,
            "text/html".into(),
            html.len(),
            doc,
            Vec::new(),
            Vec::new(),
        );
        let d = p.digest();
        assert_eq!(d["title"], "Shop");
        assert_eq!(d["headings"]["h1"][0], "Widgets");
        let links = d["links"].as_array().unwrap();
        assert_eq!(links.len(), 2);
        assert_eq!(links[0], "First -> https://example.com/p/1");
        assert_eq!(links[1], "Elsewhere -> https://other.example/x");
        let form = &d["forms"][0];
        assert_eq!(form["action"], "https://example.com/search");
        assert_eq!(form["method"], "POST");
        let fields = form["fields"].as_array().unwrap();
        assert!(fields.iter().any(|f| f == "input[name=q]"));
        let buttons = d["buttons"].as_array().unwrap();
        assert!(buttons.contains(&serde_json::json!("Buy")));
        assert!(buttons.contains(&serde_json::json!("Go")));
    }
}

#[cfg(test)]
mod meta_refresh_tests {
    use super::*;

    #[test]
    fn finds_double_quoted_refresh() {
        let html = r#"<html><head><meta http-equiv="refresh" content="0; url=/next.html"></head></html>"#;
        assert_eq!(
            meta_refresh(html, "https://example.com/start"),
            Some("https://example.com/next.html".to_string())
        );
    }

    #[test]
    fn finds_case_and_quote_variants() {
        let html = "<html><head><META HTTP-EQUIV=\"Refresh\" CONTENT='5; url=https://other.example/x'></head></html>";
        assert_eq!(
            meta_refresh(html, "https://example.com/"),
            Some("https://other.example/x".to_string())
        );
    }

    #[test]
    fn absolute_and_relative_targets_resolve() {
        let html = r#"<meta http-equiv="refresh" content="2; url=../up/a?b=1">"#;
        assert_eq!(
            meta_refresh(html, "https://example.com/dir/sub/page"),
            Some("https://example.com/dir/up/a?b=1".to_string())
        );
    }

    #[test]
    fn plain_reload_is_ignored() {
        let html = r#"<meta http-equiv="refresh" content="30">"#;
        assert_eq!(meta_refresh(html, "https://example.com/"), None);
    }

    #[test]
    fn other_meta_tags_are_ignored() {
        let html = r#"<meta charset="utf-8"><meta http-equiv="content-type" content="text/html">"#;
        assert_eq!(meta_refresh(html, "https://example.com/"), None);
    }
}
