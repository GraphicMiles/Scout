//! YouTube channel adapter.
//!
//! Reads a channel's videos page using two independent sources, preferring the
//! richer one and falling back when it is absent:
//!
//!   1. `ytInitialData` — the JSON blob YouTube embeds in the served HTML.
//!   2. The server-rendered DOM (`a[href*="/watch?v="]`), which survives layout
//!      changes that move the JSON around.
//!
//! Only the first page of results is read. YouTube paginates with continuation
//! requests, which are not made, so a channel with more videos than one page
//! returns a truncated list and says so in `warnings`.

use crate::error::{ErrorCode, Result, ScoutError};
use crate::html;
use crate::page::Page;
use crate::selector;
use crate::util::resolve_url;
use serde_json::{json, Map, Value};
use std::collections::HashSet;

const MAX_JSON_BYTES: usize = 8 * 1024 * 1024;

fn script_texts(page: &Page) -> Vec<String> {
    let Some(doc) = page.doc.as_ref() else {
        return Vec::new();
    };
    let Ok(sel) = selector::compile("script") else {
        return Vec::new();
    };
    doc.select(&sel)
        .map(|s| html::raw_text(&*s))
        .filter(|t| !t.is_empty() && t.len() <= MAX_JSON_BYTES)
        .collect()
}

/// Locate and parse `ytInitialData` from any inline script.
pub fn parse_initial_data(page: &Page) -> Option<Value> {
    for text in script_texts(page) {
        let trimmed = text.trim();
        if trimmed.starts_with('{') && trimmed.contains("\"videoRenderer\"") || trimmed.contains("ytInitialData") {
            // Either a raw JSON blob or an assignment wrapping one.
            let candidate = if trimmed.starts_with('{') {
                Some(trimmed.to_string())
            } else {
                extract_balanced_object(&text)
            };
            if let Some(json_str) = candidate {
                if let Ok(v) = serde_json::from_str::<Value>(&json_str) {
                    return Some(v);
                }
            }
        }
    }
    None
}

/// Pull the first balanced `{...}` object out of a script body.
fn extract_balanced_object(src: &str) -> Option<String> {
    let start = src.find('{')?;
    let bytes = src.as_bytes();
    let mut depth = 0i32;
    let mut in_str = false;
    let mut escaped = false;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        let c = b as char;
        if in_str {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_str = false;
            }
            continue;
        }
        match c {
            '"' => in_str = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(src[start..=i].to_string());
                }
            }
            _ => {}
        }
    }
    None
}

/// Recursively find every occurrence of `key` in a JSON tree.
fn find_all<'a>(value: &'a Value, key: &str, out: &mut Vec<&'a Value>) {
    match value {
        Value::Object(map) => {
            for (k, v) in map {
                if k == key {
                    out.push(v);
                }
                find_all(v, key, out);
            }
        }
        Value::Array(arr) => {
            for v in arr {
                find_all(v, key, out);
            }
        }
        _ => {}
    }
}

fn text_of(v: &Value) -> Option<String> {
    // Three shapes appear across YouTube's markup generations:
    //   {"simpleText": "..."} | {"runs":[{"text":"..."}]} | {"content": "..."}
    if let Some(s) = v.get("simpleText").and_then(|x| x.as_str()) {
        return Some(s.to_string());
    }
    if let Some(s) = v.get("content").and_then(|x| x.as_str()) {
        return Some(s.to_string());
    }
    if let Some(runs) = v.get("runs").and_then(|x| x.as_array()) {
        let joined: String = runs
            .iter()
            .filter_map(|r| r.get("text").and_then(|t| t.as_str()))
            .collect();
        if !joined.is_empty() {
            return Some(joined);
        }
    }
    if let Some(s) = v.as_str() {
        return Some(s.to_string());
    }
    None
}

fn best_thumbnail(v: &Value) -> Option<String> {
    let thumbs = v
        .get("thumbnail")
        .and_then(|t| t.get("thumbnails"))
        .and_then(|t| t.as_array())?;
    thumbs
        .last()
        .and_then(|t| t.get("url"))
        .and_then(|u| u.as_str())
        .map(|s| s.to_string())
}

struct Video {
    id: String,
    title: Option<String>,
    published: Option<String>,
    duration: Option<String>,
    views: Option<String>,
    thumbnail: Option<String>,
}

fn video_from_renderer(r: &Value) -> Option<Video> {
    let id = r.get("videoId").and_then(|v| v.as_str())?.to_string();
    Some(Video {
        id,
        title: r.get("title").and_then(text_of),
        published: r.get("publishedTimeText").and_then(text_of),
        duration: r.get("lengthText").and_then(text_of),
        views: r
            .get("viewCountText")
            .and_then(text_of)
            .or_else(|| r.get("shortViewCountText").and_then(text_of)),
        thumbnail: best_thumbnail(r),
    })
}

/// Read a `lockupViewModel`, the markup YouTube serves for channel video grids.
fn video_from_lockup(l: &Value) -> Option<Video> {
    let id = l.get("contentId").and_then(|x| x.as_str())?.to_string();

    let meta = l
        .get("metadata")
        .and_then(|m| m.get("lockupMetadataViewModel"));
    let title = meta
        .and_then(|m| m.get("title"))
        .and_then(text_of);

    // `metadataRows[0].metadataParts` carries view count then age. The trailing
    // icon names the part, and is more reliable than positional order.
    let mut views = None;
    let mut published = None;
    if let Some(parts) = meta
        .and_then(|m| m.get("metadata"))
        .and_then(|m| m.get("contentMetadataViewModel"))
        .and_then(|m| m.get("metadataRows"))
        .and_then(|r| r.as_array())
        .and_then(|rows| rows.first())
        .and_then(|row| row.get("metadataParts"))
        .and_then(|p| p.as_array())
    {
        for part in parts {
            let text = part.get("text").and_then(text_of);
            let icon = part
                .get("leadingIcon")
                .and_then(|i| i.get("name"))
                .and_then(|n| n.as_str());
            if icon == Some("PLAY_ARROW_OUTLINED") {
                views = text;
            } else if published.is_none() {
                published = text;
            }
        }
    }

    // Duration lives in a thumbnail overlay badge.
    let mut badges = Vec::new();
    find_all(l, "thumbnailBadgeViewModel", &mut badges);
    let duration = badges
        .iter()
        .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
        .find(|t| t.contains(':') && t.chars().all(|c| c.is_ascii_digit() || c == ':'))
        .map(|t| t.to_string());

    // Pick the largest thumbnail source rather than the first.
    let thumbnail = l
        .get("contentImage")
        .and_then(|c| c.get("thumbnailViewModel"))
        .and_then(|t| t.get("image"))
        .and_then(|i| i.get("sources"))
        .and_then(|s| s.as_array())
        .and_then(|arr| {
            arr.iter()
                .max_by_key(|s| s.get("width").and_then(|w| w.as_u64()).unwrap_or(0))
        })
        .and_then(|s| s.get("url"))
        .and_then(|u| u.as_str())
        .map(|s| s.to_string())
        .or_else(|| best_thumbnail(l));

    Some(Video {
        id,
        title,
        published,
        duration,
        views,
        thumbnail,
    })
}

fn channel_from_json(root: &Value) -> Map<String, Value> {
    let mut ch = Map::new();

    let mut metas = Vec::new();
    find_all(root, "channelMetadataRenderer", &mut metas);
    if let Some(m) = metas.first() {
        if let Some(v) = m.get("externalId").and_then(|x| x.as_str()) {
            ch.insert("id".into(), json!(v));
        }
        if let Some(v) = m.get("title").and_then(|x| x.as_str()) {
            ch.insert("title".into(), json!(v));
        }
        if let Some(v) = m.get("description").and_then(|x| x.as_str()) {
            ch.insert("description".into(), json!(v));
        }
        if let Some(v) = m
            .get("ownerUrls")
            .and_then(|u| u.as_array())
            .and_then(|a| a.first())
            .and_then(|s| s.as_str())
        {
            ch.insert("url".into(), json!(v));
        }
    }

    let mut headers = Vec::new();
    find_all(root, "c4TabbedHeaderRenderer", &mut headers);
    if let Some(h) = headers.first() {
        if !ch.contains_key("id") {
            if let Some(v) = h.get("channelId").and_then(|x| x.as_str()) {
                ch.insert("id".into(), json!(v));
            }
        }
        if !ch.contains_key("title") {
            if let Some(v) = h.get("title").and_then(|x| x.as_str()) {
                ch.insert("title".into(), json!(v));
            }
        }
        if let Some(v) = h.get("subscriberCountText").and_then(text_of) {
            ch.insert("subscriberText".into(), json!(v));
        }
        if let Some(v) = h.get("videosCountText").and_then(text_of) {
            ch.insert("videoCountText".into(), json!(v));
        }
    }

    // Newer layouts expose the same data under pageHeaderRenderer.
    let mut page_headers = Vec::new();
    find_all(root, "pageHeaderRenderer", &mut page_headers);
    if let Some(h) = page_headers.first() {
        if !ch.contains_key("title") {
            if let Some(v) = h.get("pageTitle").and_then(|x| x.as_str()) {
                ch.insert("title".into(), json!(v));
            }
        }
        if !ch.contains_key("subscriberText") {
            let mut metas = Vec::new();
            find_all(h, "metadata", &mut metas);
            if let Some(m) = metas.first() {
                if let Some(t) = m.get("contentMetadataViewModel").and_then(text_of) {
                    ch.insert("subscriberText".into(), json!(t));
                }
            }
        }
    }

    let mut micros = Vec::new();
    find_all(root, "microformatDataRenderer", &mut micros);
    if let Some(m) = micros.first() {
        if !ch.contains_key("description") {
            if let Some(v) = m.get("description").and_then(|x| x.as_str()) {
                ch.insert("description".into(), json!(v));
            }
        }
        if !ch.contains_key("url") {
            if let Some(v) = m.get("urlCanonical").and_then(|x| x.as_str()) {
                ch.insert("url".into(), json!(v));
            }
        }
    }

    ch
}

fn handle_from_url(url: &str) -> Option<String> {
    let u = url::Url::parse(url).ok()?;
    let mut segs = u.path_segments()?;
    let first = segs.next()?;
    if first.starts_with('@') {
        Some(first.to_string())
    } else if first == "channel" || first == "c" || first == "user" {
        segs.next().map(|s| format!("{first}/{s}"))
    } else {
        Some(first.to_string())
    }
}

/// True when `host` is one of the configured YouTube hosts.
pub fn is_youtube_host(hosts: &[String], host: &str) -> bool {
    let h = host.to_ascii_lowercase();
    hosts.iter().any(|allowed| allowed == &h)
}

/// Validate and normalise a channel URL before it is fetched.
///
/// A channel root (`/@name`, `/channel/UC...`, `/c/...`, `/user/...`) is
/// rewritten to its Videos tab, which is what this strategy reads.
pub fn prepare_url(hosts: &[String], raw: &str) -> Result<String> {
    let mut u = url::Url::parse(raw)
        .map_err(|_| ScoutError::invalid("URL is not valid").with_details(json!({ "field": "url" })))?;
    if !is_youtube_host(hosts, u.host_str().unwrap_or("")) {
        return Err(ScoutError::invalid("This strategy requires a youtube.com channel URL")
            .with_details(json!({ "field": "url" })));
    }
    let path = u.path().trim_end_matches('/').to_string();
    // Mirrors the Node regex: /(@name|channel/UC..|c/name|user/name) with no
    // further path segments — i.e. a channel root, not a tab.
    let one_segment = |prefix: &str| {
        path.strip_prefix(prefix)
            .map(|rest| !rest.is_empty() && !rest.contains('/'))
            .unwrap_or(false)
    };
    let is_channel_root = one_segment("/@")
        || one_segment("/channel/")
        || one_segment("/c/")
        || one_segment("/user/");
    if is_channel_root {
        u.set_path(&format!("{path}/videos"));
    }
    Ok(u.to_string())
}

pub fn extract(page: &Page, limit: usize, hosts: &[String]) -> Result<(Value, Vec<Value>)> {
    // A consent interstitial is not something Scout resolves on the caller's
    // behalf; say so rather than reporting an empty channel.
    if let Ok(u) = url::Url::parse(&page.url) {
        let host = u.host_str().unwrap_or("").to_ascii_lowercase();
        if host == "consent.youtube.com" || u.path().starts_with("/consent") {
            return Err(ScoutError::new(
                ErrorCode::CompatibilityError,
                "YouTube served a consent interstitial. Scout does not accept consent flows on the caller's behalf.",
            ));
        }
    }

    let mut warnings = Vec::new();

    let root = parse_initial_data(page);

    // Channel identity: from ytInitialData when available, else page metadata.
    let mut channel = root
        .as_ref()
        .map(channel_from_json)
        .unwrap_or_default();

    if let Some(doc) = page.doc.as_ref() {
        let meta_content = |sel_str: &str| -> Option<String> {
            let sel = selector::compile(sel_str).ok()?;
            selector::select_one(doc, &sel).and_then(|e| e.value().attr("content").map(|s| s.to_string()))
        };
        if !channel.contains_key("title") {
            if let Some(t) = meta_content("meta[property=\"og:title\"]").or_else(|| html::title(doc)) {
                channel.insert("title".into(), json!(t));
            }
        }
        if !channel.contains_key("description") {
            if let Some(d) = meta_content("meta[property=\"og:description\"]")
                .or_else(|| meta_content("meta[name=\"description\"]"))
            {
                channel.insert("description".into(), json!(d));
            }
        }
        if !channel.contains_key("url") {
            if let Some(c) = meta_content("meta[property=\"og:url\"]") {
                channel.insert("url".into(), json!(c));
            }
        }
    }
    if let Some(h) = handle_from_url(&page.url) {
        channel.insert("handle".into(), json!(h));
    }
    channel.insert("sourceUrl".into(), json!(page.url));

    // Videos: prefer ytInitialData, fall back to the DOM.
    let mut videos: Vec<Video> = Vec::new();
    if let Some(root) = root.as_ref() {
        let mut renderers = Vec::new();
        find_all(root, "videoRenderer", &mut renderers);
        find_all(root, "gridVideoRenderer", &mut renderers);
        find_all(root, "richItemRenderer", &mut renderers); // ignored: no videoId

        for r in renderers {
            if let Some(v) = video_from_renderer(r) {
                videos.push(v);
            }
        }
        // `lockupViewModel` is the current video markup on channel pages; it
        // replaced `videoRenderer`, which no longer appears in served HTML.
        let mut lockups = Vec::new();
        find_all(root, "lockupViewModel", &mut lockups);
        for l in lockups {
            if let Some(v) = video_from_lockup(l) {
                videos.push(v);
            }
        }
    }

    if videos.is_empty() {
        if let Some(doc) = page.doc.as_ref() {
            if let Ok(sel) = selector::compile("a[href*=\"/watch?v=\"]") {
                let base = crate::extract::base_url_of(page);
                let mut seen = HashSet::new();
                for a in doc.select(&sel) {
                    let href = a.value().attr("href").unwrap_or("");
                    let Some(abs) = resolve_url(href, &base) else {
                        continue;
                    };
                    let Some(id) = video_id_of(hosts, &abs) else { continue };
                    if !seen.insert(id.clone()) {
                        continue;
                    }
                    let text = html::text(&*a);
                    let title = if !text.is_empty() {
                        Some(text)
                    } else {
                        a.value()
                            .attr("title")
                            .or_else(|| a.value().attr("aria-label"))
                            .map(|s| s.to_string())
                            .filter(|s| !s.is_empty())
                    };
                    videos.push(Video {
                        id,
                        title,
                        published: None,
                        duration: None,
                        views: None,
                        thumbnail: None,
                    });
                }
            }
        }
    }

    // De-duplicate by id, preserving first-seen order.
    let mut seen = HashSet::new();
    videos.retain(|v| seen.insert(v.id.clone()));

    let total = videos.len();
    let truncated = total > limit;
    videos.truncate(limit);

    // Say plainly when the channel identity could not be read, rather than
    // returning an empty object the caller has to interpret.
    if channel.is_empty() {
        warnings.push(json!({
            "code": "channel_missing",
            "message": "Channel metadata was not present in the page data",
        }));
    }
    if videos.is_empty() {
        warnings.push(json!({
            "code": "no_videos",
            "message": "No videos could be read from this page. It may be a consent, sign-in or bot-check page rather than the channel listing.",
        }));
    }
    if truncated {
        warnings.push(json!({
            "code": "continuation_unsupported",
            "message": format!("Only the first page of results is read; {total} videos were present and continuation requests are not made."),
        }));
    }

    let items: Vec<Value> = videos
        .iter()
        .map(|v| {
            json!({
                "videoId": v.id,
                "title": v.title,
                "url": format!("https://www.youtube.com/watch?v={}", v.id),
                "thumbnailUrl": v.thumbnail,
                "publishedText": v.published,
                "duration": v.duration,
                "viewCountText": v.views,
            })
        })
        .collect();

    Ok((
        json!({
            "channel": Value::Object(channel),
            "videos": items,
            "count": items.len(),
        }),
        warnings,
    ))
}

fn video_id_of(hosts: &[String], url: &str) -> Option<String> {
    let u = url::Url::parse(url).ok()?;
    if !is_youtube_host(hosts, u.host_str()?) {
        return None;
    }
    u.query_pairs()
        .find(|(k, _)| k == "v")
        .map(|(_, v)| v.to_string())
        .filter(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_hosts() -> Vec<String> {
        vec!["youtube.com".into(), "www.youtube.com".into(), "m.youtube.com".into()]
    }

    /// Ported from the Node `prepareUrl`: a channel root is aimed at its Videos
    /// tab, and a non-YouTube host is refused before any fetch happens.
    #[test]
    fn prepare_url_rewrites_channel_roots() {
        let h = test_hosts();
        assert_eq!(
            prepare_url(&h, "https://www.youtube.com/@NASA").unwrap(),
            "https://www.youtube.com/@NASA/videos"
        );
        assert_eq!(
            prepare_url(&h, "https://www.youtube.com/@NASA/").unwrap(),
            "https://www.youtube.com/@NASA/videos"
        );
        assert_eq!(
            prepare_url(&h, "https://www.youtube.com/channel/UCLA_DiR1FfKNvjuUpBHmylQ").unwrap(),
            "https://www.youtube.com/channel/UCLA_DiR1FfKNvjuUpBHmylQ/videos"
        );
        // Already a tab, or a deeper path: left alone.
        assert_eq!(
            prepare_url(&h, "https://www.youtube.com/@NASA/videos").unwrap(),
            "https://www.youtube.com/@NASA/videos"
        );
        assert_eq!(
            prepare_url(&h, "https://www.youtube.com/watch?v=abc").unwrap(),
            "https://www.youtube.com/watch?v=abc"
        );
    }

    #[test]
    fn prepare_url_refuses_other_hosts_and_garbage() {
        let h = test_hosts();
        let e = prepare_url(&h, "https://example.com/@NASA").unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidRequest);
        assert!(e.message.contains("youtube.com"));
        // A lookalike host must not pass: membership, not suffix matching.
        assert!(prepare_url(&h, "https://notyoutube.com/@NASA").is_err());
        assert!(prepare_url(&h, "https://youtube.com.evil.test/@NASA").is_err());
        assert_eq!(
            prepare_url(&h, "not a url").unwrap_err().code,
            ErrorCode::InvalidRequest
        );
        // An operator-configured host is accepted (this is how the fixtures run).
        let custom = vec!["127.0.0.1".to_string()];
        assert_eq!(
            prepare_url(&custom, "http://127.0.0.1:8080/@testchan").unwrap(),
            "http://127.0.0.1:8080/@testchan/videos"
        );
    }

    /// A consent interstitial must be reported, never silently treated as an
    /// empty channel — Scout does not accept consent on the caller's behalf.
    #[test]
    fn consent_interstitial_is_an_explicit_error() {
        let src = "<html><body></body></html>";
        let doc = crate::html::parse_html(src, 150_000).unwrap();
        let p = Page::html(
            "https://consent.youtube.com/m?continue=x".into(),
            200,
            "text/html".into(),
            src.len(),
            doc,
            Vec::new(),
            Vec::new(),
        );
        let e = extract(&p, 10, &test_hosts()).unwrap_err();
        assert_eq!(e.code, ErrorCode::CompatibilityError);
        assert!(e.message.contains("consent"));
    }

    fn page_with(html_src: &str) -> Page {
        let doc = crate::html::parse_html(html_src, 150_000).unwrap();
        Page::html(
            "https://www.youtube.com/@examplechannel/videos".into(),
            200,
            "text/html".into(),
            html_src.len(),
            doc,
            Vec::new(),
            Vec::new(),
        )
    }

    fn fixture_with_initial_data() -> String {
        let data = json!({
            "metadata": {
                "channelMetadataRenderer": {
                    "externalId": "UCabc123",
                    "title": "Example Channel",
                    "description": "A channel about examples",
                    "ownerUrls": ["http://www.youtube.com/@examplechannel"]
                }
            },
            "header": {
                "c4TabbedHeaderRenderer": {
                    "channelId": "UCabc123",
                    "title": "Example Channel",
                    "subscriberCountText": { "simpleText": "1.23M subscribers" }
                }
            },
            "contents": {
                "twoColumnBrowseResultsRenderer": {
                    "tabs": [{
                        "tabRenderer": {
                            "content": {
                                "richGridRenderer": {
                                    "contents": [
                                        { "richItemRenderer": { "content": { "videoRenderer": {
                                            "videoId": "vid000000001",
                                            "title": { "runs": [{ "text": "First Video" }] },
                                            "publishedTimeText": { "simpleText": "2 days ago" },
                                            "lengthText": { "simpleText": "10:01" },
                                            "viewCountText": { "simpleText": "1,234 views" },
                                            "thumbnail": { "thumbnails": [{ "url": "https://i.ytimg.com/vi/vid000000001/hq.jpg" }] }
                                        } } } },
                                        { "richItemRenderer": { "content": { "videoRenderer": {
                                            "videoId": "vid000000002",
                                            "title": { "simpleText": "Second Video" },
                                            "publishedTimeText": { "simpleText": "1 week ago" }
                                        } } } },
                                        { "richItemRenderer": { "content": { "videoRenderer": {
                                            "videoId": "vid000000001",
                                            "title": { "simpleText": "First Video (duplicate)" }
                                        } } } }
                                    ]
                                }
                            }
                        }
                    }]
                }
            }
        });
        format!(
            "<html><head><title>Example Channel - YouTube</title></head><body><script>var ytInitialData = {};</script></body></html>",
            data
        )
    }

    #[test]
    fn reads_channel_and_videos_from_initial_data() {
        let p = page_with(&fixture_with_initial_data());
        let (data, warnings) = extract(&p, 10, &test_hosts()).unwrap();
        assert_eq!(data["channel"]["id"], "UCabc123");
        assert_eq!(data["channel"]["title"], "Example Channel");
        assert_eq!(data["channel"]["subscriberText"], "1.23M subscribers");
        assert_eq!(data["count"], 2, "duplicate video ids must collapse");
        assert_eq!(data["videos"][0]["videoId"], "vid000000001");
        assert_eq!(data["videos"][0]["title"], "First Video");
        assert_eq!(data["videos"][0]["publishedText"], "2 days ago");
        assert_eq!(data["videos"][0]["url"], "https://www.youtube.com/watch?v=vid000000001");
        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
    }

    #[test]
    fn falls_back_to_dom_when_initial_data_absent() {
        let html_src = r#"<html><head><title>Chan</title></head><body>
            <a href="/watch?v=aaa111">Video A</a>
            <a href="/watch?v=bbb222"><span>Video B</span></a>
            <a href="/watch?v=aaa111">Video A again</a>
            <a href="/about">About</a>
        </body></html>"#;
        let p = page_with(html_src);
        let (data, _w) = extract(&p, 10, &test_hosts()).unwrap();
        assert_eq!(data["count"], 2);
        assert_eq!(data["videos"][0]["videoId"], "aaa111");
        assert_eq!(data["videos"][1]["videoId"], "bbb222");
        assert_eq!(data["videos"][1]["title"], "Video B");
    }

    #[test]
    fn honours_the_limit_and_warns_about_truncation() {
        let mut items = Vec::new();
        for i in 0..10 {
            items.push(json!({ "richItemRenderer": { "content": { "videoRenderer": {
                "videoId": format!("vid{i:09}"),
                "title": { "simpleText": format!("Video {i}") }
            } } } }));
        }
        let data = json!({ "contents": { "richGridRenderer": { "contents": items } } });
        let html_src = format!("<html><body><script>var ytInitialData = {data};</script></body></html>");
        let p = page_with(&html_src);
        let (out, warnings) = extract(&p, 3, &test_hosts()).unwrap();
        assert_eq!(out["count"], 3);
        assert!(warnings.iter().any(|w| w["code"] == "continuation_unsupported"), "warnings: {warnings:?}");
    }

    #[test]
    fn empty_page_warns_rather_than_inventing_data() {
        let p = page_with("<html><body><p>before you continue to YouTube</p></body></html>");
        let (data, warnings) = extract(&p, 10, &test_hosts()).unwrap();
        assert_eq!(data["count"], 0);
        assert!(data["videos"].as_array().unwrap().is_empty());
        assert!(warnings.iter().any(|w| w["code"] == "no_videos"));
    }

    /// Regression fixture for the `lockupViewModel` markup YouTube serves today.
    /// Captured from a live channel page: `videoRenderer` no longer appears at
    /// all, so this shape is the one that actually runs in production.
    #[test]
    fn reads_current_lockup_markup() {
        let data = json!({
            "metadata": { "channelMetadataRenderer": { "externalId": "UCabc", "title": "Chan" } },
            "contents": {
                "richGridRenderer": { "contents": [ { "richItemRenderer": { "content": {
                    "lockupViewModel": {
                        "contentId": "5b3n-u_0QAI",
                        "contentImage": { "thumbnailViewModel": {
                            "image": { "sources": [
                                { "url": "https://i.ytimg.com/vi/5b3n-u_0QAI/hq720.jpg", "width": 720, "height": 404 },
                                { "url": "https://i.ytimg.com/vi/5b3n-u_0QAI/hqdefault.jpg", "width": 360, "height": 202 }
                            ] },
                            "overlays": [ { "thumbnailBottomOverlayViewModel": { "badges": [
                                { "thumbnailBadgeViewModel": { "text": "20:15" } }
                            ] } } ]
                        } },
                        "metadata": { "lockupMetadataViewModel": {
                            "title": { "content": "Planetary Defenders: This Is Not an Exercise" },
                            "metadata": { "contentMetadataViewModel": { "metadataRows": [ { "metadataParts": [
                                { "text": { "content": "307K" }, "leadingIcon": { "name": "PLAY_ARROW_OUTLINED" } },
                                { "text": { "content": "1d ago" } }
                            ] } ] } }
                        } }
                    }
                } } } ] }
            }
        });
        let html_src = format!("<html><body><script>var ytInitialData = {};</script></body></html>", data);
        let p = page_with(&html_src);
        let (out, warnings) = extract(&p, 10, &test_hosts()).unwrap();

        assert_eq!(out["count"], 1, "warnings: {warnings:?}");
        let v = &out["videos"][0];
        assert_eq!(v["videoId"], "5b3n-u_0QAI");
        assert_eq!(v["title"], "Planetary Defenders: This Is Not an Exercise");
        assert_eq!(v["duration"], "20:15");
        assert_eq!(v["viewCountText"], "307K");
        assert_eq!(v["publishedText"], "1d ago");
        // The larger thumbnail source wins, not the first one listed.
        assert_eq!(v["thumbnailUrl"], "https://i.ytimg.com/vi/5b3n-u_0QAI/hq720.jpg");
    }

    #[test]
    fn extracts_handle_from_url() {
        let p = page_with("<html><body></body></html>");
        let (data, _) = extract(&p, 5, &test_hosts()).unwrap();
        assert_eq!(data["channel"]["handle"], "@examplechannel");
    }

    #[test]
    fn balanced_object_extraction_handles_nested_braces_and_strings() {
        let src = r#"var x = {"a":{"b":"}{"},"c":1}; more"#;
        let got = extract_balanced_object(src).unwrap();
        assert_eq!(got, r#"{"a":{"b":"}{"},"c":1}"#);
    }
}
