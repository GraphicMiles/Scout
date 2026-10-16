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
    /// The page's own script bodies in document order, kept so a later event
    /// pass (a scroll, a key press) can re-run them: listeners die with the
    /// context that registered them, so the scripts must run again before the
    /// event is dispatched into the page.
    pub scripts: Vec<String>,
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
        }
    }

    pub fn title(&self) -> Option<String> {
        self.doc.as_ref().and_then(crate::html::title)
    }
}
