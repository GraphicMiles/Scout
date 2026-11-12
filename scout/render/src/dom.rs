//! A DOM that both the style engine and the layout engine can walk.
//!
//! Parsing is html5ever, the same parser Scout uses, so a page is recovered from
//! malformed markup the way a browser recovers it. The tree is an arena: nodes are
//! referenced by `NodeId`, which keeps layout free of reference counting and makes
//! an index path cheap to compute.

use html5ever::parse_document;
use html5ever::tendril::TendrilSink;
use markup5ever_rcdom::{Handle, NodeData, RcDom};
use std::collections::HashMap;

pub type NodeId = usize;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    Document,
    Element,
    Text,
    Comment,
}

#[derive(Debug, Clone)]
pub struct Node {
    pub kind: Kind,
    /// Lower-case tag name for elements; `#text` and `#comment` otherwise.
    pub name: String,
    pub attrs: Vec<(String, String)>,
    pub text: String,
    pub parent: Option<NodeId>,
    pub children: Vec<NodeId>,
}

impl Node {
    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn is_element(&self) -> bool {
        self.kind == Kind::Element
    }

    pub fn classes(&self) -> impl Iterator<Item = &str> {
        self.attr("class")
            .unwrap_or("")
            .split_ascii_whitespace()
            .filter(|c| !c.is_empty())
    }

    pub fn has_class(&self, class: &str) -> bool {
        self.classes().any(|c| c == class)
    }

    /// `display: none` or a `<head>`-only element. Style resolution decides the
    /// first; the second is structural.
    pub fn is_rendered_by_default(&self) -> bool {
        !matches!(self.name.as_str(), "head" | "meta" | "title" | "style" | "script" | "link" | "base")
    }
}

#[derive(Debug, Clone)]
pub struct Document {
    pub nodes: Vec<Node>,
    pub root: NodeId,
    /// `<style>` contents in document order.
    pub stylesheets: Vec<String>,
    /// `href` of `<link rel=stylesheet>` in document order.
    pub linked_stylesheets: Vec<String>,
}

impl Document {
    pub fn parse(html: &str) -> Document {
        let dom = parse_document(RcDom::default(), Default::default())
            .from_utf8()
            .read_from(&mut html.as_bytes())
            .expect("parsing a &str cannot fail");

        let mut doc = Document {
            nodes: Vec::new(),
            root: 0,
            stylesheets: Vec::new(),
            linked_stylesheets: Vec::new(),
        };
        let root = doc.import(&dom.document, None);
        doc.root = root;
        doc.collect_stylesheets();
        doc
    }

    fn import(&mut self, handle: &Handle, parent: Option<NodeId>) -> NodeId {
        let (kind, name, attrs, text) = match &handle.data {
            NodeData::Document => (Kind::Document, "#document".to_string(), Vec::new(), String::new()),
            NodeData::Doctype { .. } => (Kind::Comment, "#doctype".to_string(), Vec::new(), String::new()),
            NodeData::Comment { contents } => (Kind::Comment, "#comment".to_string(), Vec::new(), contents.to_string()),
            NodeData::Text { contents } => (
                Kind::Text,
                "#text".to_string(),
                Vec::new(),
                contents.borrow().to_string(),
            ),
            NodeData::Element { name, attrs, .. } => (
                Kind::Element,
                name.local.to_string().to_ascii_lowercase(),
                attrs
                    .borrow()
                    .iter()
                    .map(|a| (a.name.local.to_string().to_ascii_lowercase(), a.value.to_string()))
                    .collect(),
                String::new(),
            ),
            _ => (Kind::Comment, "#other".to_string(), Vec::new(), String::new()),
        };

        let id = self.nodes.len();
        self.nodes.push(Node {
            kind,
            name,
            attrs,
            text,
            parent,
            children: Vec::new(),
        });

        let kids: Vec<Handle> = handle.children.borrow().clone();
        for child in kids {
            let child_id = self.import(&child, Some(id));
            self.nodes[id].children.push(child_id);
        }
        id
    }

    fn collect_stylesheets(&mut self) {
        let mut stack = vec![self.root];
        while let Some(id) = stack.pop() {
            let node = self.nodes[id].clone();
            if node.name == "style" {
                let css = node
                    .children
                    .iter()
                    .map(|c| self.nodes[*c].text.clone())
                    .collect::<String>();
                self.stylesheets.push(css);
            } else if node.name == "link"
                && node
                    .attr("rel")
                    .map(|r| r.split_ascii_whitespace().any(|t| t.eq_ignore_ascii_case("stylesheet")))
                    .unwrap_or(false)
            {
                if let Some(href) = node.attr("href") {
                    self.linked_stylesheets.push(href.to_string());
                }
            }
            for child in node.children.iter().rev() {
                stack.push(*child);
            }
        }
    }

    /// The `<body>` element, or the root when a document has none.
    pub fn body(&self) -> NodeId {
        self.nodes
            .iter()
            .position(|n| n.name == "body" && n.is_element())
            .unwrap_or(self.root)
    }

    /// An index path (`[0, 1, 2]`) that is stable across parsers, because it counts
    /// element children only and the tree shape is html5ever's in both.
    pub fn element_path(&self, id: NodeId) -> Vec<usize> {
        let mut path = Vec::new();
        let mut current = Some(id);
        while let Some(nid) = current {
            let node = &self.nodes[nid];
            if let Some(parent) = node.parent {
                let index = self.nodes[parent]
                    .children
                    .iter()
                    .filter(|c| self.nodes[**c].is_element())
                    .position(|c| *c == nid);
                if let Some(i) = index {
                    path.push(i);
                }
            } else {
                // The document node has no index of its own: `html` is `[0]`, not `[0, 0]`.
                break;
            }
            current = node.parent;
        }
        path.reverse();
        path
    }

    /// A unique CSS selector for a node, for snapshots an agent can click with.
    pub fn css_path(&self, id: NodeId) -> String {
        let mut parts: Vec<String> = Vec::new();
        let mut current = Some(id);
        while let Some(nid) = current {
            let node = &self.nodes[nid];
            if !node.is_element() {
                break;
            }
            let mut part = node.name.clone();
            if let Some(id_attr) = node.attr("id") {
                if !id_attr.is_empty() {
                    part = format!("{}#{}", part, id_attr);
                    parts.push(part);
                    break;
                }
            }
            if let Some(parent) = node.parent {
                let same_tag: Vec<NodeId> = self.nodes[parent]
                    .children
                    .iter()
                    .copied()
                    .filter(|c| self.nodes[*c].is_element() && self.nodes[*c].name == node.name)
                    .collect();
                if same_tag.len() > 1 {
                    let nth = same_tag.iter().position(|c| *c == nid).unwrap_or(0) + 1;
                    part = format!("{}:nth-of-type({})", part, nth);
                }
            }
            parts.push(part);
            current = node.parent;
        }
        parts.reverse();
        parts.join(" > ")
    }

    /// Depth-first, elements only, in document order.
    pub fn elements(&self) -> Vec<NodeId> {
        let mut out = Vec::new();
        let mut stack = vec![self.root];
        while let Some(id) = stack.pop() {
            if self.nodes[id].is_element() {
                out.push(id);
            }
            for child in self.nodes[id].children.iter().rev() {
                stack.push(*child);
            }
        }
        out
    }

    /// Text content of a subtree, whitespace untouched.
    pub fn text_content(&self, id: NodeId) -> String {
        let mut out = String::new();
        let mut stack = vec![id];
        while let Some(nid) = stack.pop() {
            let node = &self.nodes[nid];
            if node.kind == Kind::Text {
                out.push_str(&node.text);
            }
            for child in node.children.iter().rev() {
                stack.push(*child);
            }
        }
        out
    }

    /// `id` attribute index, used by the selector matcher's fast path.
    pub fn index_by_id(&self) -> HashMap<String, NodeId> {
        let mut map = HashMap::new();
        for id in self.elements() {
            if let Some(v) = self.nodes[id].attr("id") {
                map.entry(v.to_string()).or_insert(id);
            }
        }
        map
    }
}

/// Collapse whitespace the way `white-space: normal` does.
pub fn collapse_whitespace(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut last_space = true;
    for ch in input.chars() {
        if ch.is_ascii_whitespace() {
            if !last_space {
                out.push(' ');
                last_space = true;
            }
        } else {
            out.push(ch);
            last_space = false;
        }
    }
    out
}
