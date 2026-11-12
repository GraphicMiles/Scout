//! Selector parsing, matching and specificity for the subset the engine supports.
//!
//! Supported: type, class, id, `*`, descendant, child, adjacent and general
//! sibling combinators, attribute selectors (`[a]`, `[a=v]`, `[a~=v]`, `[a^=v]`,
//! `[a$=v]`, `[a*=v]`), and the structural pseudo-classes `:root`, `:first-child`,
//! `:last-child`, `:only-child`, `:nth-child()`, `:empty`, plus `:not()` with a
//! simple-selector argument. Anything else is reported as unsupported rather than
//! silently ignored, so a page cannot quietly lose a rule.

use crate::dom::{Document, NodeId};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Combinator {
    Descendant,
    Child,
    Adjacent,
    Sibling,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Simple {
    Universal,
    Type(String),
    Class(String),
    Id(String),
    Attr { name: String, op: AttrOp, value: String },
    Pseudo(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttrOp {
    Exists,
    Equals,
    Includes,
    Prefix,
    Suffix,
    Substring,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Compound {
    pub simples: Vec<Simple>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Selector {
    /// Left to right. `parts[0].0` is always `Descendant` and is unused.
    pub parts: Vec<(Combinator, Compound)>,
    /// (ids, classes, types) — the specificity triple.
    pub specificity: (u32, u32, u32),
}

impl fmt::Display for Selector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, (comb, compound)) in self.parts.iter().enumerate() {
            if i > 0 {
                match comb {
                    Combinator::Descendant => write!(f, " ")?,
                    Combinator::Child => write!(f, " > ")?,
                    Combinator::Adjacent => write!(f, " + ")?,
                    Combinator::Sibling => write!(f, " ~ ")?,
                }
            }
            for (j, s) in compound.simples.iter().enumerate() {
                if j > 0 {
                    // no separator: a compound is written as one token
                }
                match s {
                    Simple::Universal => write!(f, "*")?,
                    Simple::Type(t) => write!(f, "{}", t)?,
                    Simple::Class(c) => write!(f, ".{}", c)?,
                    Simple::Id(i) => write!(f, "#{}", i)?,
                    Simple::Attr { name, op, value } => {
                        let o = match op {
                            AttrOp::Exists => String::new(),
                            AttrOp::Equals => "=".into(),
                            AttrOp::Includes => "~=".into(),
                            AttrOp::Prefix => "^=".into(),
                            AttrOp::Suffix => "$=".into(),
                            AttrOp::Substring => "*=".into(),
                        };
                        if *op == AttrOp::Exists {
                            write!(f, "[{}]", name)?;
                        } else {
                            write!(f, "[{}{}\"{}\"]", name, o, value)?;
                        }
                    }
                    Simple::Pseudo(p) => write!(f, ":{}", p)?,
                }
            }
        }
        Ok(())
    }
}

/// A selector that could not be parsed, with the reason.
#[derive(Debug, Clone)]
pub struct Unsupported {
    pub selector: String,
    pub reason: String,
}

/// Parse a comma-separated selector list.
pub fn parse_selector_list(input: &str) -> (Vec<Selector>, Vec<Unsupported>) {
    let mut ok = Vec::new();
    let mut bad = Vec::new();
    for piece in split_top_level(input, ',') {
        let text = piece.trim();
        if text.is_empty() {
            continue;
        }
        match parse_selector(text) {
            Ok(sel) => ok.push(sel),
            Err(reason) => bad.push(Unsupported {
                selector: text.to_string(),
                reason,
            }),
        }
    }
    (ok, bad)
}

fn split_top_level(input: &str, sep: char) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut current = String::new();
    let mut in_string: Option<char> = None;
    for ch in input.chars() {
        if let Some(q) = in_string {
            current.push(ch);
            if ch == q {
                in_string = None;
            }
            continue;
        }
        match ch {
            '"' | '\'' => {
                in_string = Some(ch);
                current.push(ch);
            }
            '(' | '[' => {
                depth += 1;
                current.push(ch);
            }
            ')' | ']' => {
                depth -= 1;
                current.push(ch);
            }
            c if c == sep && depth == 0 => {
                out.push(std::mem::take(&mut current));
            }
            c => current.push(c),
        }
    }
    out.push(current);
    out
}

pub fn parse_selector(input: &str) -> Result<Selector, String> {
    let mut parts: Vec<(Combinator, Compound)> = Vec::new();
    let mut specificity = (0u32, 0u32, 0u32);
    let mut pending = Combinator::Descendant;
    let mut rest = input.trim();

    while !rest.is_empty() {
        // One compound: simple selectors written with no space between them.
        let mut simples: Vec<Simple> = Vec::new();
        loop {
            let (consumed, simple) = parse_simple(rest)?;
            if consumed == 0 {
                break;
            }
            match &simple {
                Simple::Id(_) => specificity.0 += 1,
                Simple::Class(_) | Simple::Attr { .. } | Simple::Pseudo(_) => specificity.1 += 1,
                Simple::Type(_) => specificity.2 += 1,
                Simple::Universal => {}
            }
            simples.push(simple);
            rest = &rest[consumed..];
            // `p.special`, `a:hover`, `[x="y"].z` stay one compound; a space or a
            // combinator starts the next one.
            if !rest.starts_with(|c: char| c == '.' || c == '#' || c == '[' || c == ':') {
                break;
            }
        }
        if simples.is_empty() {
            return Err(format!("unexpected input at {:?}", &rest[..rest.len().min(16)]));
        }
        parts.push((pending, Compound { simples }));

        rest = rest.trim_start();
        if rest.is_empty() {
            break;
        }
        pending = if let Some(r) = rest.strip_prefix('>') {
            rest = r;
            Combinator::Child
        } else if let Some(r) = rest.strip_prefix('+') {
            rest = r;
            Combinator::Adjacent
        } else if let Some(r) = rest.strip_prefix('~') {
            rest = r;
            Combinator::Sibling
        } else {
            Combinator::Descendant
        };
        rest = rest.trim_start();
    }

    if parts.is_empty() {
        return Err("empty selector".to_string());
    }
    Ok(Selector { parts, specificity })
}

/// Parse one simple selector at the start of `s`; returns bytes consumed.
fn parse_simple(s: &str) -> Result<(usize, Simple), String> {
    let bytes = s.as_bytes();
    if bytes.is_empty() {
        return Err("empty".into());
    }
    match bytes[0] {
        b'*' => Ok((1, Simple::Universal)),
        b'.' => {
            let (name, len) = read_ident(&s[1..])?;
            Ok((1 + len, Simple::Class(name)))
        }
        b'#' => {
            let (name, len) = read_ident(&s[1..])?;
            Ok((1 + len, Simple::Id(name)))
        }
        b'[' => {
            let close = s.find(']').ok_or("unterminated attribute selector")?;
            let body = &s[1..close];
            let (name, op, value) = parse_attr(body)?;
            Ok((close + 1, Simple::Attr { name, op, value }))
        }
        b':' => {
            let after = &s[1..];
            if let Some(rest) = after.strip_prefix(':') {
                // Pseudo-elements are not rendered as boxes; accept and ignore.
                let (name, len) = read_ident(rest)?;
                return Ok((2 + len, Simple::Pseudo(format!("::{}", name))));
            }
            let (name, len) = read_ident(after)?;
            let mut consumed = 1 + len;
            let mut full = name.clone();
            if after[len..].starts_with('(') {
                let rest = &after[len..];
                let close = rest.find(')').ok_or("unterminated pseudo-class argument")?;
                full = format!("{}{}", name, &rest[..=close]);
                consumed += close + 1;
            }
            check_pseudo(&full)?;
            Ok((consumed, Simple::Pseudo(full)))
        }
        c if c.is_ascii_alphabetic() || c == b'_' || c == b'-' => {
            let (name, len) = read_ident(s)?;
            Ok((len, Simple::Type(name.to_ascii_lowercase())))
        }
        _ => Err(format!("unexpected character {:?}", s.chars().next().unwrap())),
    }
}

fn check_pseudo(name: &str) -> Result<(), String> {
    let base = name.split('(').next().unwrap_or(name).to_ascii_lowercase();
    const SUPPORTED: &[&str] = &[
        "root",
        "first-child",
        "last-child",
        "only-child",
        "empty",
        "not",
        "nth-child",
        "nth-last-child",
        "first-of-type",
        "last-of-type",
        "only-of-type",
        "nth-of-type",
        "nth-last-of-type",
        "checked",
        "disabled",
        "enabled",
        "required",
        "read-only",
        "placeholder-shown",
        "lang",
        // Interactive states cannot apply to a still frame: a rule keyed on them
        // is parsed, never matched, and must not be reported as a limitation.
        "hover",
        "focus",
        "focus-visible",
        "focus-within",
        "active",
        "visited",
        "link",
        "target",
        "any-link",
    ];
    if name.starts_with("::") || SUPPORTED.contains(&base.as_str()) {
        return Ok(());
    }
    Err(format!("unsupported pseudo-class :{}", base))
}

fn parse_attr(body: &str) -> Result<(String, AttrOp, String), String> {
    let body = body.trim();
    for (token, op) in [("~=", AttrOp::Includes), ("^=", AttrOp::Prefix), ("$=", AttrOp::Suffix), ("*=", AttrOp::Substring), ("=", AttrOp::Equals)] {
        if let Some(pos) = body.find(token) {
            let name = body[..pos].trim().to_ascii_lowercase();
            let raw = body[pos + token.len()..].trim();
            let value = raw.trim_matches(|c| c == '"' || c == '\'').to_string();
            return Ok((name, op, value));
        }
    }
    Ok((body.to_ascii_lowercase(), AttrOp::Exists, String::new()))
}

fn read_ident(s: &str) -> Result<(String, usize), String> {
    let mut out = String::new();
    let mut chars = s.char_indices();
    let mut consumed = 0;
    while let Some((i, ch)) = chars.next() {
        let ok = ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' || ch == '\\' || !ch.is_ascii();
        if !ok {
            consumed = i;
            break;
        }
        out.push(ch);
        consumed = i + ch.len_utf8();
    }
    if out.is_empty() {
        return Err("expected an identifier".into());
    }
    Ok((out, consumed))
}

// ---------------------------------------------------------------- matching

pub fn matches(doc: &Document, node: NodeId, selector: &Selector) -> bool {
    matches_from(doc, node, selector, selector.parts.len() - 1)
}

fn matches_from(doc: &Document, node: NodeId, selector: &Selector, index: usize) -> bool {
    let (_, compound) = &selector.parts[index];
    if !matches_compound(doc, node, compound) {
        return false;
    }
    if index == 0 {
        return true;
    }
    let combinator = selector.parts[index].0;
    match combinator {
        Combinator::Child => match doc.nodes[node].parent {
            Some(parent) => matches_from(doc, parent, selector, index - 1),
            None => false,
        },
        Combinator::Descendant => {
            let mut current = doc.nodes[node].parent;
            while let Some(ancestor) = current {
                if matches_from(doc, ancestor, selector, index - 1) {
                    return true;
                }
                current = doc.nodes[ancestor].parent;
            }
            false
        }
        Combinator::Adjacent => match previous_element_sibling(doc, node) {
            Some(prev) => matches_from(doc, prev, selector, index - 1),
            None => false,
        },
        Combinator::Sibling => {
            let mut current = previous_element_sibling(doc, node);
            while let Some(prev) = current {
                if matches_from(doc, prev, selector, index - 1) {
                    return true;
                }
                current = previous_element_sibling(doc, prev);
            }
            false
        }
    }
}

pub fn matches_compound(doc: &Document, node: NodeId, compound: &Compound) -> bool {
    let el = &doc.nodes[node];
    if !el.is_element() {
        return false;
    }
    compound.simples.iter().all(|simple| matches_simple(doc, node, simple))
}

fn matches_simple(doc: &Document, node: NodeId, simple: &Simple) -> bool {
    let el = &doc.nodes[node];
    match simple {
        Simple::Universal => true,
        Simple::Type(t) => el.name == *t,
        Simple::Class(c) => el.has_class(c),
        Simple::Id(i) => el.attr("id") == Some(i.as_str()),
        Simple::Attr { name, op, value } => match (el.attr(name), op) {
            (None, _) => false,
            (Some(_), AttrOp::Exists) => true,
            (Some(actual), op) => match op {
                AttrOp::Equals => actual == value,
                AttrOp::Includes => actual.split_ascii_whitespace().any(|t| t == value),
                AttrOp::Prefix => actual.starts_with(value.as_str()),
                AttrOp::Suffix => actual.ends_with(value.as_str()),
                AttrOp::Substring => actual.contains(value.as_str()),
                AttrOp::Exists => true,
            },
        },
        Simple::Pseudo(name) => matches_pseudo(doc, node, name),
    }
}

fn matches_pseudo(doc: &Document, node: NodeId, name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    if lower.starts_with("::") {
        // Pseudo-elements never match the element itself for layout purposes.
        return false;
    }
    let (base, arg) = match lower.find('(') {
        Some(pos) => (&lower[..pos], Some(lower[pos + 1..].trim_end_matches(')').trim().to_string())),
        None => (lower.as_str(), None),
    };
    match base {
        "root" => doc.nodes[node].parent.map(|p| doc.nodes[p].kind == super::super::dom::Kind::Document).unwrap_or(false),
        "first-child" => element_siblings(doc, node)
            .map(|s| s.first() == Some(&node))
            .unwrap_or(false),
        "last-child" => element_siblings(doc, node)
            .map(|s| s.last() == Some(&node))
            .unwrap_or(false),
        "only-child" => element_siblings(doc, node).map(|s| s.len() == 1).unwrap_or(false),
        "empty" => doc.nodes[node].children.iter().all(|c| {
            let child = &doc.nodes[*c];
            child.kind == super::super::dom::Kind::Comment
                || (child.kind == super::super::dom::Kind::Text && child.text.trim().is_empty())
        }),
        "nth-child" => match arg.and_then(|a| parse_nth(&a)) {
            Some((a, b)) => element_siblings(doc, node)
                .and_then(|s| s.iter().position(|c| *c == node))
                .map(|idx| {
                    let n = idx as i32 + 1;
                    match a {
                        0 => n == b,
                        _ => (n - b) % a == 0 && (n - b) / a >= 0,
                    }
                })
                .unwrap_or(false),
            None => false,
        },
        "first-of-type" | "last-of-type" | "only-of-type" | "nth-of-type" | "nth-last-of-type" => {
            let Some(siblings) = self_siblings_of_type(doc, node) else { return false };
            let position = match siblings.iter().position(|c| *c == node) {
                Some(position) => position as i32 + 1,
                None => return false,
            };
            let total = siblings.len() as i32;
            match base {
                "first-of-type" => position == 1,
                "last-of-type" => position == total,
                "only-of-type" => total == 1,
                "nth-of-type" => match arg.and_then(|a| parse_nth(&a)) {
                    Some((a, b)) => {
                        let n = position;
                        a == 0 && n == b || a != 0 && (n - b) % a == 0 && (n - b) / a >= 0
                    }
                    None => false,
                },
                "nth-last-of-type" => match arg.and_then(|a| parse_nth(&a)) {
                    Some((a, b)) => {
                        let n = total - position + 1;
                        a == 0 && n == b || a != 0 && (n - b) % a == 0 && (n - b) / a >= 0
                    }
                    None => false,
                },
                _ => false,
            }
        }
        "nth-last-child" => match arg.and_then(|a| parse_nth(&a)) {
            Some((a, b)) => element_siblings(doc, node)
                .and_then(|s| s.iter().position(|c| *c == node).map(|i| (i, s.len())))
                .map(|(idx, len)| {
                    let n = (len - idx) as i32;
                    a == 0 && n == b || a != 0 && (n - b) % a == 0 && (n - b) / a >= 0
                })
                .unwrap_or(false),
            None => false,
        },
        // Form state lives in the attributes a static page ships with.
        "checked" => {
            let el = &doc.nodes[node];
            el.attr("checked").is_some()
                || (el.attr("selected").is_some() && el.name == "option")
        }
        "disabled" => doc.nodes[node].attr("disabled").is_some(),
        "enabled" => doc.nodes[node].attr("disabled").is_none(),
        "required" => doc.nodes[node].attr("required").is_some(),
        "read-only" => doc.nodes[node].attr("readonly").is_some(),
        "placeholder-shown" => {
            doc.nodes[node].attr("value").map(|v| v.is_empty()).unwrap_or(false)
                && doc.nodes[node].attr("placeholder").is_some()
        }
        "lang" => match arg {
            Some(wanted) => {
                let mut current = Some(node);
                while let Some(id) = current {
                    if let Some(lang) = doc.nodes[id].attr("lang").or_else(|| doc.nodes[id].attr("xml:lang")) {
                        let lang = lang.to_ascii_lowercase();
                        return lang == wanted || lang.starts_with(&format!("{}-", wanted));
                    }
                    current = doc.nodes[id].parent;
                }
                false
            }
            None => false,
        },
        // Interactive states never apply to a still frame.
        "hover" | "focus" | "focus-visible" | "focus-within" | "active" | "visited" | "target" => {
            false
        }
        "link" | "any-link" => {
            matches!(doc.nodes[node].name.as_str(), "a" | "area") && doc.nodes[node].attr("href").is_some()
        }
        "not" => match arg {
            Some(inner) => match parse_selector(&inner) {
                Ok(sel) => !matches(doc, node, &sel),
                Err(_) => false,
            },
            None => false,
        },
        _ => false,
    }
}

fn parse_nth(arg: &str) -> Option<(i32, i32)> {
    let arg = arg.trim().replace(' ', "");
    if arg == "odd" {
        return Some((2, 1));
    }
    if arg == "even" {
        return Some((2, 0));
    }
    if let Some(pos) = arg.find('n') {
        let a_part = &arg[..pos];
        let a = match a_part {
            "" | "+" => 1,
            "-" => -1,
            other => other.parse().ok()?,
        };
        let rest = &arg[pos + 1..];
        let b = if rest.is_empty() {
            0
        } else {
            rest.parse().ok()?
        };
        Some((a, b))
    } else {
        let b = arg.parse().ok()?;
        Some((0, b))
    }
}

/// Element siblings that share this node's tag name, in document order.
fn self_siblings_of_type(doc: &Document, node: NodeId) -> Option<Vec<NodeId>> {
    let name = doc.nodes[node].name.clone();
    element_siblings(doc, node).map(|siblings| {
        siblings
            .into_iter()
            .filter(|id| doc.nodes[*id].name == name)
            .collect()
    })
}

fn element_siblings(doc: &Document, node: NodeId) -> Option<Vec<NodeId>> {
    let parent = doc.nodes[node].parent?;
    Some(
        doc.nodes[parent]
            .children
            .iter()
            .copied()
            .filter(|c| doc.nodes[*c].is_element())
            .collect(),
    )
}

fn previous_element_sibling(doc: &Document, node: NodeId) -> Option<NodeId> {
    let parent = doc.nodes[node].parent?;
    let siblings = &doc.nodes[parent].children;
    let pos = siblings.iter().position(|c| *c == node)?;
    siblings[..pos]
        .iter()
        .rev()
        .find(|c| doc.nodes[**c].is_element())
        .copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compound_selectors_stay_one_compound() {
        let sel = parse_selector("p.special").unwrap();
        assert_eq!(sel.parts.len(), 1, "p.special is one compound, not `p .special`");
        assert_eq!(sel.parts[0].1.simples.len(), 2);
        assert_eq!(sel.specificity, (0, 1, 1));
    }

    #[test]
    fn class_beats_type_regardless_of_order() {
        let class = parse_selector("p.special").unwrap();
        let plain = parse_selector("p").unwrap();
        assert!(class.specificity > plain.specificity);
    }

    #[test]
    fn combinators_are_read() {
        assert_eq!(parse_selector("div p").unwrap().parts.len(), 2);
        assert_eq!(parse_selector("div > p").unwrap().parts.len(), 2);
        assert_eq!(parse_selector("div > p").unwrap().parts[1].0, Combinator::Child);
        assert_eq!(parse_selector("li + li").unwrap().parts[1].0, Combinator::Adjacent);
        assert_eq!(parse_selector("h1 ~ p").unwrap().parts[1].0, Combinator::Sibling);
        assert_eq!(parse_selector("#a .b .c").unwrap().parts.len(), 3);
    }

    #[test]
    fn specificity_is_the_css_triple() {
        assert_eq!(parse_selector("#x .y div").unwrap().specificity, (1, 1, 1));
        assert_eq!(parse_selector("*").unwrap().specificity, (0, 0, 0));
        assert_eq!(parse_selector("a[href]").unwrap().specificity, (0, 1, 1));
    }

    #[test]
    fn pseudo_classes_parse_without_leaking_into_the_parent_compound() {
        if let Ok(sel) = parse_selector("li:first-child") {
            assert_eq!(sel.parts.len(), 1);
        }
        if let Ok(sel) = parse_selector("li:nth-child(2n+1)") {
            assert_eq!(sel.parts.len(), 1);
        }
    }
}
