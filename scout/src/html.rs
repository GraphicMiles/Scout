//! DOM construction and text extraction.
//!
//! Parsing is html5ever (spec-compliant tree construction, including foster
//! parenting and the adoption agency algorithm) rather than a hand-rolled
//! tokenizer. The tree is `ego_tree`, reachable via `Html::tree` for traversal.

use crate::error::{ErrorCode, Result, ScoutError};
use ego_tree::NodeRef;
use scraper::{Html, Node};

/// Tags whose text is never visible.
const SKIP_TEXT: &[&str] = &["script", "style", "template", "head"];

/// Elements that imply a whitespace break between adjacent text runs.
const BLOCK: &[&str] = &[
    "address", "article", "aside", "blockquote", "br", "dd", "details", "div", "dl", "dt",
    "fieldset", "figcaption", "figure", "footer", "form", "h1", "h2", "h3", "h4", "h5", "h6",
    "header", "hr", "li", "main", "nav", "ol", "p", "pre", "section", "table", "tbody", "td",
    "tfoot", "th", "thead", "tr", "ul",
];

pub fn parse_html(src: &str, max_nodes: usize) -> Result<Html> {
    let doc = Html::parse_document(src);
    let count = element_count(&doc);
    if count > max_nodes {
        return Err(ScoutError::new(
            ErrorCode::ResponseTooLarge,
            format!("Document exceeds the {max_nodes} node limit"),
        )
        .with_retryable(false));
    }
    Ok(doc)
}

pub fn element_count(doc: &Html) -> usize {
    doc.tree.nodes().filter(|n| n.value().is_element()).count()
}

/// Concatenate the direct text children of a node, unnormalized.
/// Used for `<script>` bodies and `<textarea>` values.
pub fn raw_text(node: &NodeRef<Node>) -> String {
    let mut s = String::new();
    for child in node.children() {
        if let Node::Text(t) = child.value() {
            s.push_str(&t.text);
        }
    }
    s
}

/// Visible-ish text with runs of whitespace collapsed.
///
/// Block-level boundaries become a single space, and `script`/`style`/`template`
/// subtrees are skipped entirely. The traversal is iterative so a deeply nested
/// document cannot blow the stack.
pub fn text(node: &NodeRef<Node>) -> String {
    let mut out = String::new();
    // Each stack entry is either a node to visit or a literal string to emit.
    enum Item<'a> {
        Node(NodeRef<'a, Node>),
        Str(&'static str),
    }
    let mut stack: Vec<Item> = vec![Item::Node(*node)];
    let root_id = node.id();

    while let Some(item) = stack.pop() {
        match item {
            Item::Str(s) => out.push_str(s),
            Item::Node(n) => match n.value() {
                Node::Text(t) => out.push_str(&t.text),
                Node::Element(e) => {
                    let name = e.name();
                    if n.id() != root_id && SKIP_TEXT.contains(&name) {
                        continue;
                    }
                    // A space goes before the block's content, and the queued
                    // `Str` is popped after its children, so a boundary exists on
                    // both sides and `a<div>b</div>c` does not become "abc".
                    if BLOCK.contains(&name) {
                        out.push(' ');
                        stack.push(Item::Str(" "));
                    }
                    let children: Vec<_> = n.children().collect();
                    for c in children.into_iter().rev() {
                        stack.push(Item::Node(c));
                    }
                }
                // Document and Fragment nodes carry no text of their own but do
                // hold the tree; descend into them rather than skipping.
                _ => {
                    let children: Vec<_> = n.children().collect();
                    for c in children.into_iter().rev() {
                        stack.push(Item::Node(c));
                    }
                }
            },
        }
    }
    collapse_ws(&out)
}

fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_ws = true; // trims leading whitespace
    for ch in s.chars() {
        if ch.is_whitespace() {
            in_ws = true;
        } else {
            if in_ws && !out.is_empty() {
                out.push(' ');
            }
            in_ws = false;
            out.push(ch);
        }
    }
    out
}

/// Text of the first matching element, or empty if absent.
pub fn text_of_first(doc: &Html, selector: &scraper::Selector) -> String {
    doc.select(selector).next().map(|e| e.text().collect::<String>()).unwrap_or_default()
}

/// `document.title`, trimmed. `None` when the document has no `<title>`.
pub fn title(doc: &Html) -> Option<String> {
    let sel = scraper::Selector::parse("title").ok()?;
    let el = doc.select(&sel).next()?;
    let t = text(&*el);
    // scraper's `text()` is a descendant iterator, but re-walking keeps the
    // block/whitespace rules identical to the rest of the extractor.
    let t = if t.is_empty() { el.text().collect::<String>() } else { t };
    let t = t.trim().to_string();
    if t.is_empty() {
        None
    } else {
        Some(t)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(html: &str) -> String {
        let doc = Html::parse_document(html);
        text(&doc.tree.root())
    }

    #[test]
    fn collapses_whitespace_and_separates_blocks() {
        assert_eq!(t("<p>hello   world</p>"), "hello world");
        assert_eq!(t("<div>a</div><div>b</div>"), "a b");
        assert_eq!(t("<li>one</li><li>two</li>"), "one two");
    }

    #[test]
    fn skips_script_and_style() {
        // Inline elements add no separator, matching how a browser renders them
        // on one line; only block boundaries introduce a break.
        assert_eq!(t("<body>keep<script>var x=1</script>drop</body>"), "keepdrop");
        assert_eq!(t("<body>a<p>b</p></body>"), "a b");
        assert_eq!(t("<body><p>x</p><style>.a{}</style><p>y</p></body>"), "x y");
        // noscript fallback content is visible to a client that does not run JS.
        assert_eq!(t("<body><noscript>enable js</noscript></body>"), "enable js");
    }

    #[test]
    fn decodes_entities() {
        assert_eq!(t("<p>a &amp; b &lt;c&gt; &#8364;</p>"), "a & b <c> €");
    }

    #[test]
    fn enforces_node_limit() {
        let big = "<div>".to_string() + &"<span>x</span>".repeat(50) + "</div>";
        assert!(parse_html(&big, 5).is_err());
        assert!(parse_html(&big, 1000).is_ok());
    }

    /// html5ever implements foster parenting, so a stray text node before a
    /// table is hoisted out rather than nested inside it.
    #[test]
    fn handles_malformed_markup() {
        let doc = Html::parse_document("<table>stray<tr><td>cell</td></tr></table>");
        let sel = scraper::Selector::parse("tr").unwrap();
        assert_eq!(doc.select(&sel).count(), 1);
    }

    #[test]
    fn title_extraction() {
        let doc = Html::parse_document("<html><head><title>  Hi  There </title></head></html>");
        assert_eq!(title(&doc).as_deref(), Some("Hi There"));
    }
}
