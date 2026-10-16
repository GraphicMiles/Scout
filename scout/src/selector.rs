//! CSS selector compilation.
//!
//! Selectors are compiled by the `selectors` crate (the engine behind Servo and
//! rust-selectors) rather than a hand-written matcher, so the supported grammar
//! is a superset of the reference implementation's subset. Compilation failures
//! surface as `invalid_request` with the offending selector, never as a silent
//! empty match.

use crate::error::{Result, ScoutError};
use scraper::{Html, Selector};
use std::collections::HashMap;
use std::sync::Mutex;

pub const MAX_SELECTOR_LEN: usize = 512;
const CACHE_CAP: usize = 512;

static CACHE: Mutex<Option<HashMap<String, Selector>>> = Mutex::new(None);

pub fn compile(sel: &str) -> Result<Selector> {
    if sel.trim().is_empty() {
        return Err(ScoutError::invalid("Selector must be a non-empty string")
            .with_details(serde_json::json!({ "field": "selector" })));
    }
    if sel.len() > MAX_SELECTOR_LEN {
        return Err(ScoutError::invalid(format!(
            "selector is longer than {MAX_SELECTOR_LEN} characters"
        ))
        .with_details(serde_json::json!({ "field": "selector" })));
    }

    {
        let guard = CACHE.lock().unwrap();
        if let Some(map) = guard.as_ref() {
            if let Some(s) = map.get(sel) {
                return Ok(s.clone());
            }
        }
    }

    // The selector parser accepts some unterminated input (for example `[foo`),
    // reinterpreting it as a different, valid selector. Reject structurally
    // malformed selectors up front so a typo fails loudly instead of quietly
    // matching the wrong elements.
    if let Some(why) = unbalanced(sel) {
        return Err(
            ScoutError::invalid(format!("Invalid CSS selector \"{sel}\": {why}"))
                .with_details(serde_json::json!({ "field": "selector" })),
        );
    }

    let parsed = Selector::parse(sel).map_err(|e| {
        ScoutError::invalid(format!("Invalid CSS selector \"{sel}\": {e:?}"))
            .with_details(serde_json::json!({ "field": "selector" }))
    })?;

    let mut guard = CACHE.lock().unwrap();
    let map = guard.get_or_insert_with(HashMap::new);
    if map.len() >= CACHE_CAP {
        map.clear();
    }
    map.insert(sel.to_string(), parsed.clone());
    Ok(parsed)
}

/// Check bracket, parenthesis and quote balance outside of quoted strings.
/// Returns a description of the first problem found.
fn unbalanced(sel: &str) -> Option<String> {
    let mut square = 0i32;
    let mut paren = 0i32;
    let mut quote: Option<char> = None;
    let bytes: Vec<char> = sel.chars().collect();
    let mut i = 0;

    while i < bytes.len() {
        let c = bytes[i];
        if let Some(q) = quote {
            if c == '\\' {
                i += 2;
                continue;
            }
            if c == q {
                quote = None;
            }
        } else {
            match c {
                '\'' | '"' => quote = Some(c),
                '[' => square += 1,
                ']' => {
                    square -= 1;
                    if square < 0 {
                        return Some("unexpected \"]\"".into());
                    }
                }
                '(' => paren += 1,
                ')' => {
                    paren -= 1;
                    if paren < 0 {
                        return Some("unexpected \")\"".into());
                    }
                }
                _ => {}
            }
        }
        i += 1;
    }

    if quote.is_some() {
        return Some("unterminated quoted string".into());
    }
    if square > 0 {
        return Some("unterminated attribute selector".into());
    }
    if paren > 0 {
        return Some("unclosed parenthesis".into());
    }
    None
}

/// First matching element in document order.
pub fn select_one<'a>(doc: &'a Html, sel: &Selector) -> Option<scraper::ElementRef<'a>> {
    doc.select(sel).next()
}

/// The `index`-th match, in document order.
pub fn select_one_indexed<'a>(
    doc: &'a Html,
    sel: &Selector,
    index: usize,
) -> Option<scraper::ElementRef<'a>> {
    doc.select(sel).nth(index)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_empty_and_oversized() {
        assert!(compile("").is_err());
        assert!(compile("   ").is_err());
        assert!(compile(&"a".repeat(600)).is_err());
    }

    #[test]
    fn rejects_malformed() {
        assert!(compile(">>>").is_err());
        assert!(compile("..x").is_err());
        assert!(compile("[unclosed").is_err());
    }

    #[test]
    fn accepts_rich_selectors() {
        for ok in [
            "div",
            "*",
            "#id",
            ".cls",
            "a[href]",
            "a[href^='https']",
            "li:nth-child(2n+1)",
            "li:first-child",
            "p:not(.x)",
            "ul > li + li",
            "a, b",
            "input[type='text' i]",
        ] {
            assert!(compile(ok).is_ok(), "{ok} should compile");
        }
    }

    #[test]
    fn selects_from_document() {
        let doc = Html::parse_document("<ul><li>a</li><li class='b'>b</li></ul>");
        let sel = compile("li.b").unwrap();
        let hits: Vec<_> = doc.select(&sel).collect();
        assert_eq!(hits.len(), 1);
    }
}
