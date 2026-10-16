//! Extraction: value parsing, schema-driven fields, and the generic-page strategy.

use crate::html;
use crate::page::{Page, PageKind};
use crate::selector;
use crate::util::resolve_url;
use serde_json::{json, Map, Value};
use scraper::ElementRef;

// ------------------------------------------------------------ value parsing

const SYMBOLS: &[(&str, &str)] = &[
    ("$", "USD"),
    ("€", "EUR"),
    ("£", "GBP"),
    ("₦", "NGN"),
    ("¥", "JPY"),
    ("₹", "INR"),
];

/// Parse a price.
///
/// Handles US-style separators ("1,234.50") only. European style ("1.234,50")
/// is documented as unsupported rather than guessed at, since guessing wrong
/// would silently misreport a price by a factor of 1000.
pub fn parse_price(raw: &str) -> Option<Value> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }

    // Find the first numeric run, allowing comma/space thousands separators.
    let bytes: Vec<char> = s.chars().collect();
    let mut start = None;
    let mut end = 0usize;
    for (i, c) in bytes.iter().enumerate() {
        if c.is_ascii_digit() {
            if start.is_none() {
                start = Some(i);
            }
            end = i + 1;
        } else if start.is_some() && (*c == ',' || *c == ' ' || *c == '.') {
            // continue through separators; a trailing one is trimmed later
        } else if start.is_some() {
            break;
        }
    }
    let start = start?;
    let numeric: String = bytes[start..end].iter().collect();
    let numeric = numeric.trim_end_matches([',', ' ', '.']).to_string();
    if numeric.is_empty() {
        return None;
    }
    let amount: f64 = numeric.replace([',', ' '], "").parse().ok()?;

    let prefix: String = bytes[..start].iter().collect();
    let suffix: String = bytes[end..].iter().collect();

    let currency = SYMBOLS
        .iter()
        .find(|(sym, _)| prefix.contains(sym) || suffix.contains(sym))
        .map(|(_, code)| code.to_string())
        .or_else(|| {
            for code in ["USD", "EUR", "GBP", "NGN", "JPY", "INR"] {
                if prefix.contains(code) || suffix.contains(code) {
                    return Some(code.to_string());
                }
            }
            None
        });

    Some(json!({
        "amount": amount,
        "currency": currency,
        "raw": raw.trim(),
    }))
}

pub fn parse_number(raw: &str) -> Option<f64> {
    let bytes: Vec<char> = raw.chars().collect();
    let mut start = None;
    let mut end = 0usize;
    for (i, c) in bytes.iter().enumerate() {
        let is_startish = c.is_ascii_digit() || ((*c == '-' || *c == '+') && start.is_none());
        if is_startish {
            if start.is_none() {
                start = Some(i);
            }
            end = i + 1;
        } else if start.is_some() && (*c == ',' || *c == '.') {
            // keep consuming
        } else if start.is_some() {
            break;
        }
    }
    let start = start?;
    let numeric: String = bytes[start..end].iter().collect();
    let numeric = numeric.trim_end_matches([',', '.']).replace(',', "");
    numeric.parse::<f64>().ok()
}

pub fn parse_date(raw: &str) -> Option<String> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    // RFC 3339 / ISO 8601 with an explicit offset or Z.
    if let Ok(dt) = chrono_free_parse(s) {
        return Some(dt);
    }
    None
}

/// Parse the date formats that appear in machine-readable markup.
///
/// Deliberately narrow: this reads the `datetime` attribute and common ISO
/// forms. Free-text dates ("yesterday", "2 days ago") in a locale that is not
/// known are returned as `None` rather than guessed.
fn chrono_free_parse(s: &str) -> Result<String, ()> {
    let (date_part, rest) = split_date(s).ok_or(())?;
    let (y, m, d) = date_part;

    // Validate the calendar date.
    if !(1..=12).contains(&m) || d < 1 || d > 31 || y < 1 {
        return Err(());
    }
    let dim = days_in_month(y, m);
    if d > dim {
        return Err(());
    }

    let (mut hh, mut mm, mut ss) = (0u32, 0u32, 0u32);
    let mut offset_minutes: i32 = 0;
    if let Some(rest) = rest {
        let rest = rest.trim_start_matches(['T', 't', ' ']);
        if !rest.is_empty() {
            let (time_str, tz_str) = split_time_tz(rest);
            let parts: Vec<&str> = time_str.split(':').collect();
            if parts.len() >= 2 {
                hh = parts[0].parse().map_err(|_| ())?;
                mm = parts[1].parse().map_err(|_| ())?;
                if parts.len() >= 3 {
                    let sec = parts[2].split('.').next().unwrap_or("0");
                    ss = sec.parse().map_err(|_| ())?;
                }
            }
            if hh > 23 || mm > 59 || ss > 60 {
                return Err(());
            }
            if let Some(tz) = tz_str {
                if tz == "Z" || tz == "z" {
                    offset_minutes = 0;
                } else if let Some(sign_end) = tz.find(['+', '-']) {
                    let sign = if tz.as_bytes()[sign_end] == b'-' { -1 } else { 1 };
                    let body = &tz[sign_end + 1..];
                    let (oh, om) = match body.split_once(':') {
                        Some((a, b)) => (a, b),
                        None => {
                            if body.len() == 4 {
                                (&body[..2], &body[2..])
                            } else {
                                (body, "0")
                            }
                        }
                    };
                    let oh: i32 = oh.parse().map_err(|_| ())?;
                    let om: i32 = om.parse().map_err(|_| ())?;
                    offset_minutes = sign * (oh * 60 + om);
                }
            }
        }
    }

    // Convert to UTC and emit a canonical ISO-8601 string. Days-from-civil below
    // is Howard Hinnant's algorithm, matching the logger's formatter.
    let utc_seconds = days_from_civil(y, m, d) * 86_400 + (hh as i64) * 3600 + (mm as i64) * 60 + ss as i64
        - (offset_minutes as i64) * 60;
    Ok(format_iso(utc_seconds))
}

fn split_date(s: &str) -> Option<((i64, u32, u32), Option<&str>)> {
    let b = s.as_bytes();
    if b.len() >= 10 && b[4] == b'-' && b[7] == b'-' {
        let y = s.get(0..4)?.parse().ok()?;
        let m = s.get(5..7)?.parse().ok()?;
        let d = s.get(8..10)?.parse().ok()?;
        return Some(((y, m, d), s.get(10..)));
    }
    // YYYY/MM/DD
    if b.len() >= 10 && b[4] == b'/' && b[7] == b'/' {
        let y = s.get(0..4)?.parse().ok()?;
        let m = s.get(5..7)?.parse().ok()?;
        let d = s.get(8..10)?.parse().ok()?;
        return Some(((y, m, d), s.get(10..)));
    }
    None
}

fn split_time_tz(rest: &str) -> (&str, Option<&str>) {
    match rest.find(['Z', 'z']) {
        Some(i) => (&rest[..i], Some(&rest[i..])),
        None => {
            // Find an offset sign after the time portion.
            if let Some(i) = rest.char_indices().skip(1).find(|(_, c)| *c == '+' || *c == '-').map(|(i, _)| i) {
                (&rest[..i], Some(&rest[i..]))
            } else {
                (rest, None)
            }
        }
    }
}

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 } as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn format_iso(unix_seconds: i64) -> String {
    let days = unix_seconds.div_euclid(86_400);
    let tod = unix_seconds.rem_euclid(86_400);
    let (h, mi, s) = (tod / 3600, (tod % 3600) / 60, tod % 60);

    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}.000Z")
}

// -------------------------------------------------------------- field specs

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueType {
    Text,
    Url,
    Number,
    Price,
    Date,
    Html,
    OuterHtml,
}

impl ValueType {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "text" => ValueType::Text,
            "url" => ValueType::Url,
            "number" => ValueType::Number,
            "price" => ValueType::Price,
            "date" => ValueType::Date,
            "html" => ValueType::Html,
            "outerHtml" => ValueType::OuterHtml,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone)]
pub struct FieldSpec {
    pub selector: Option<String>,
    pub attr: Option<String>,
    pub value_type: ValueType,
    pub multiple: bool,
}

#[derive(Debug, Clone)]
pub struct Schema {
    pub item: Option<String>,
    pub fields: Vec<(String, FieldSpec)>,
}

impl Schema {
    /// Parse an already-validated schema document.
    pub fn from_json(v: &Value) -> Result<Self, String> {
        let obj = v.as_object().ok_or("schema must be an object")?;
        let item = match obj.get("item") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) => Some(s.clone()),
            Some(_) => return Err("schema.item must be a string".into()),
        };
        if item.is_none() && obj.get("fields").is_none() {
            return Err("schema must have \"item\" and/or \"fields\"".into());
        }

        let mut fields = Vec::new();
        if let Some(Value::Object(fmap)) = obj.get("fields") {
            for (name, fv) in fmap {
                // The contract allows a field to be a bare selector string as a
                // shorthand for { "selector": "..." }.
                let shorthand;
                let fo = match fv {
                    Value::String(sel) => {
                        shorthand = serde_json::json!({ "selector": sel });
                        shorthand.as_object().unwrap()
                    }
                    Value::Object(o) => o,
                    _ => {
                        return Err(format!(
                            "Field \"{name}\" must be a selector string or an object"
                        ))
                    }
                };
                let selector = match fo.get("selector") {
                    None | Some(Value::Null) => None,
                    Some(Value::String(s)) => Some(s.clone()),
                    Some(_) => return Err(format!("field \"{name}\".selector must be a string")),
                };
                let attr = match fo.get("attr") {
                    None | Some(Value::Null) => None,
                    Some(Value::String(s)) => Some(s.clone()),
                    Some(_) => return Err(format!("field \"{name}\".attr must be a string")),
                };
                let value_type = match fo.get("type") {
                    None | Some(Value::Null) => ValueType::Text,
                    Some(Value::String(s)) => ValueType::parse(s)
                        .ok_or_else(|| format!("field \"{name}\".type \"{s}\" is not supported"))?,
                    Some(_) => return Err(format!("field \"{name}\".type must be a string")),
                };
                let multiple = match fo.get("multiple") {
                    None | Some(Value::Null) => false,
                    Some(Value::Bool(b)) => *b,
                    Some(_) => return Err(format!("field \"{name}\".multiple must be a boolean")),
                };
                fields.push((
                    name.clone(),
                    FieldSpec {
                        selector,
                        attr,
                        value_type,
                        multiple,
                    },
                ));
            }
        }
        fields.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(Schema {
            item,
            fields,
        })
    }
}

pub fn base_url_of(page: &Page) -> String {
    if let Some(doc) = page.doc.as_ref() {
        if let Ok(sel) = selector::compile("base[href]") {
            if let Some(el) = selector::select_one(doc, &sel) {
                if let Some(href) = el.value().attr("href") {
                    if let Some(abs) = resolve_url(href, &page.url) {
                        return abs;
                    }
                }
            }
        }
    }
    page.url.clone()
}

// ----------------------------------------------------------- value reading

pub fn read_value(el: &ElementRef, spec: &FieldSpec, base: &str) -> Option<Value> {
    let attr = spec.attr.clone().unwrap_or_else(|| match spec.value_type {
        ValueType::Url => {
            if el.value().attr("href").is_some() {
                "href".to_string()
            } else if el.value().attr("src").is_some() {
                "src".to_string()
            } else {
                "text".to_string()
            }
        }
        ValueType::Date => {
            if el.value().attr("datetime").is_some() {
                "datetime".to_string()
            } else {
                "text".to_string()
            }
        }
        _ => "text".to_string(),
    });

    let raw: String = match attr.as_str() {
        "text" => html::text(&*el),
        "html" => el.inner_html(),
        "outerHtml" => el.html(),
        other => match el.value().attr(other) {
            Some(v) => v.to_string(),
            None => return None,
        },
    };
    if raw.trim().is_empty() {
        return None;
    }

    Some(match spec.value_type {
        ValueType::Url => match resolve_url(&raw, base) {
            Some(u) => Value::String(u),
            None => return None,
        },
        ValueType::Number => match parse_number(&raw) {
            Some(n) => json!(n),
            None => return None,
        },
        ValueType::Price => parse_price(&raw)?,
        ValueType::Date => match parse_date(&raw) {
            Some(d) => Value::String(d),
            None => return None,
        },
        ValueType::Text | ValueType::Html | ValueType::OuterHtml => Value::String(raw),
    })
}

pub fn extract_by_schema(page: &Page, schema: &Schema, limit: usize) -> (Value, Vec<Value>) {
    let mut warnings = Vec::new();
    let Some(doc) = page.doc.as_ref() else {
        return (
            Value::Null,
            vec![json!({
                "code": "not_html",
                "message": "Selector extraction needs an HTML page",
            })],
        );
    };
    let base = base_url_of(page);

    let read_row = |scope: &ElementRef| -> Map<String, Value> {
        let mut rec = Map::new();
        for (name, spec) in &schema.fields {
            if spec.multiple {
                let mut out = Vec::new();
                if let Some(sel_str) = &spec.selector {
                    if let Ok(sel) = selector::compile(sel_str) {
                        for el in scope.select(&sel).take(200) {
                            if let Some(v) = read_value(&el, spec, &base) {
                                out.push(v);
                            }
                        }
                    }
                } else if let Some(v) = read_value(scope, spec, &base) {
                    out.push(v);
                }
                rec.insert(name.clone(), Value::Array(out));
            } else {
                let v = match &spec.selector {
                    Some(sel_str) => match selector::compile(sel_str) {
                        Ok(sel) => scope
                            .select(&sel)
                            .next()
                            .and_then(|el| read_value(&el, spec, &base)),
                        Err(_) => None,
                    },
                    None => read_value(scope, spec, &base),
                };
                rec.insert(name.clone(), v.unwrap_or(Value::Null));
            }
        }
        rec
    };

    let data = match &schema.item {
        Some(item_sel) => {
            let rows: Vec<Value> = match selector::compile(item_sel) {
                Ok(sel) => doc
                    .select(&sel)
                    .take(limit)
                    .map(|el| Value::Object(read_row(&el)))
                    .collect(),
                Err(e) => {
                    warnings.push(json!({ "code": "invalid_selector", "message": e.message }));
                    Vec::new()
                }
            };
            if rows.is_empty() {
                warnings.push(json!({
                    "code": "no_match",
                    "message": format!("Item selector \"{item_sel}\" matched nothing"),
                }));
            }
            Value::Array(rows)
        }
        None => {
            let root = doc.root_element();
            Value::Object(read_row(&root))
        }
    };

    // Report fields that produced nothing at all, so an empty result is
    // distinguishable from a selector that silently stopped matching.
    if let Value::Array(rows) = &data {
        if !rows.is_empty() {
            for (name, _) in &schema.fields {
                let all_empty = rows.iter().all(|r| {
                    r.get(name)
                        .map(|v| v.is_null() || v.as_array().map(|a| a.is_empty()).unwrap_or(false))
                        .unwrap_or(true)
                });
                if all_empty {
                    warnings.push(json!({
                        "code": "field_empty",
                        "message": format!("Field \"{name}\" produced no value in any record"),
                        "field": name,
                    }));
                }
            }
        }
    }

    (data, warnings)
}

// -------------------------------------------------------- generic-page read

pub fn generic_page(page: &Page, limit: usize) -> (Value, Vec<Value>) {
    if page.kind == PageKind::Json {
        return (
            json!({ "json": page.json.clone().unwrap_or(Value::Null) }),
            Vec::new(),
        );
    }
    let Some(doc) = page.doc.as_ref() else {
        return (Value::Null, Vec::new());
    };
    let base = base_url_of(page);

    let meta = |sel_str: &str| -> Option<String> {
        let sel = selector::compile(sel_str).ok()?;
        selector::select_one(doc, &sel).and_then(|e| e.value().attr("content").map(|s| s.to_string()))
    };

    let mut open_graph = Map::new();
    if let Ok(sel) = selector::compile("meta[property^=\"og:\"]") {
        for m in doc.select(&sel).take(40) {
            if let Some(prop) = m.value().attr("property") {
                if let Some(rest) = prop.strip_prefix("og:") {
                    open_graph.insert(
                        rest.to_string(),
                        m.value().attr("content").map(|s| json!(s)).unwrap_or(Value::Null),
                    );
                }
            }
        }
    }

    let mut json_ld = Vec::new();
    if let Ok(sel) = selector::compile("script[type=\"application/ld+json\"]") {
        for s in doc.select(&sel).take(10) {
            let raw = html::raw_text(&*s);
            if raw.len() > 200_000 {
                continue;
            }
            if let Ok(v) = serde_json::from_str::<Value>(&raw) {
                json_ld.push(v);
            }
        }
    }

    let mut links = Vec::new();
    if let Ok(sel) = selector::compile("a[href]") {
        for a in doc.select(&sel) {
            if links.len() >= limit {
                break;
            }
            let href = a.value().attr("href").unwrap_or("");
            if let Some(u) = resolve_url(href, &base) {
                let t = html::text(&*a);
                links.push(json!({ "text": t, "url": u }));
            }
        }
    }

    let canonical = selector::compile("link[rel=\"canonical\"]")
        .ok()
        .and_then(|sel| selector::select_one(doc, &sel))
        .and_then(|e| e.value().attr("href").map(|s| s.to_string()))
        .and_then(|h| resolve_url(&h, &base));

    let lang = selector::compile("html")
        .ok()
        .and_then(|sel| selector::select_one(doc, &sel))
        .and_then(|e| e.value().attr("lang").map(|s| s.to_string()));

    let mut headings = Vec::new();
    if let Ok(sel) = selector::compile("h1, h2, h3") {
        for h in doc.select(&sel).take(limit) {
            let level = h.value().name().chars().nth(1).and_then(|c| c.to_digit(10));
            headings.push(json!({
                "level": level,
                "text": html::text(&*h),
            }));
        }
    }

    let mut times = Vec::new();
    if let Ok(sel) = selector::compile("time[datetime]") {
        for t in doc.select(&sel).take(limit) {
            times.push(json!({
                "datetime": t.value().attr("datetime"),
                "text": html::text(&*t),
            }));
        }
    }

    let data = json!({
        "title": html::title(doc),
        "description": meta("meta[name=\"description\"]"),
        "canonical": canonical,
        "lang": lang,
        "openGraph": Value::Object(open_graph),
        "headings": headings,
        "links": links,
        "times": times,
        "jsonLd": json_ld,
    });
    (data, Vec::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_prices_with_currency() {
        let p = parse_price("$1,234.50").unwrap();
        assert_eq!(p["amount"], 1234.5);
        assert_eq!(p["currency"], "USD");

        let p = parse_price("€9.99").unwrap();
        assert_eq!(p["amount"], 9.99);
        assert_eq!(p["currency"], "EUR");

        let p = parse_price("NGN 5000").unwrap();
        assert_eq!(p["amount"], 5000.0);
        assert_eq!(p["currency"], "NGN");

        let p = parse_price("12 GBP").unwrap();
        assert_eq!(p["amount"], 12.0);
        assert_eq!(p["currency"], "GBP");

        assert!(parse_price("free").is_none());
        assert!(parse_price("").is_none());
    }

    #[test]
    fn parses_numbers() {
        assert_eq!(parse_number("1,234"), Some(1234.0));
        assert_eq!(parse_number("-12.5 kg"), Some(-12.5));
        assert_eq!(parse_number("abc"), None);
    }

    #[test]
    fn parses_dates() {
        assert_eq!(parse_date("2024-03-05").as_deref(), Some("2024-03-05T00:00:00.000Z"));
        assert_eq!(
            parse_date("2024-03-05T14:30:00Z").as_deref(),
            Some("2024-03-05T14:30:00.000Z")
        );
        assert_eq!(
            parse_date("2024-03-05T14:30:00+02:00").as_deref(),
            Some("2024-03-05T12:30:00.000Z")
        );
        // Rejected rather than guessed.
        assert!(parse_date("2 days ago").is_none());
        assert!(parse_date("not a date").is_none());
        assert!(parse_date("2024-13-01").is_none());
        assert!(parse_date("2023-02-29").is_none());
    }

    #[test]
    fn leap_year_dates() {
        assert_eq!(parse_date("2024-02-29").as_deref(), Some("2024-02-29T00:00:00.000Z"));
    }

    #[test]
    fn schema_parsing_rejects_bad_types() {
        let v = serde_json::json!({ "fields": { "a": { "type": "nonsense" } } });
        assert!(Schema::from_json(&v).is_err());
        let v = serde_json::json!({ "item": "li", "fields": { "a": { "type": "price" } } });
        assert!(Schema::from_json(&v).is_ok());
    }
}
