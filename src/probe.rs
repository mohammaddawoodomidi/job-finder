//! "Add a website / channel": detect how to read a site and check a Telegram channel before anything is saved.
use crate::apply::{normalize_channel, Channel};
use crate::http::{netloc, Http};
use crate::lexdata::JOBLIKE;
use crate::rx::{escape, Rx};
use crate::sources::{extract_jobpostings, parse_feed, urljoin};
use crate::text::{jstr, text_stripped};
use scraper::{Html, Selector};
use serde_json::{json, Value};
use std::sync::LazyLock;

const DATE_REL: &str = r"(\d+\s*(?:minutes?|hours?|days?|weeks?|months?)\s*ago)";
static JOBLIKE_RX: LazyLock<Rx> = LazyLock::new(|| Rx::lit_ci(JOBLIKE));

pub fn probe_channel(http: &Http, tg: &Value, raw: &Value) -> Value {
    let ch = match normalize_channel(raw) {
        Some(Channel::Name(n)) if Rx::lit(r"^[A-Za-z][A-Za-z0-9_]{3,31}$").is_match(&n) => n,
        _ => return json!({"ok": false, "error": "That does not look like a channel name. Use a link like https://t.me/name or @name."}),
    };
    let base = { let b = jstr(tg, "web_preview_base"); if b.is_empty() { "https://t.me/s/".to_string() } else { b } };
    let Some(r) = http.get(&format!("{}{}", base, ch)) else {
        return json!({"ok": false, "error": "Could not reach Telegram. Check your internet connection."});
    };
    if r.status != 200 {
        return json!({"ok": false, "error": format!("Telegram answered HTTP {}: the channel does not exist or is not public.", r.status)});
    }
    let soup = Html::parse_document(&r.text());
    let n = soup.select(&Selector::parse("div.tgme_widget_message").unwrap()).count();
    if n == 0 {
        return json!({"ok": false, "error": "This channel has no public web preview (private channel, or preview switched off)."});
    }
    let title = soup.select(&Selector::parse(".tgme_channel_info_header_title").unwrap()).next().map(|t| text_stripped(t)).filter(|t| !t.is_empty()).unwrap_or_else(|| ch.clone());
    json!({"ok": true, "channel": ch, "title": title, "posts": n})
}

/// Finds the repeated kind of link that points at individual jobs: /jobs/details/123, /job/some-title ...
fn guess_listing(soup: &Html, page_url: &str) -> Option<Value> {
    let own = netloc(page_url).to_lowercase();
    let own = own.strip_prefix("www.").unwrap_or(&own).to_string();
    let a_sel = Selector::parse("a[href]").unwrap();
    struct G {
        prefix: String,
        hrefs: Vec<String>,
        titles: Vec<String>,
    }
    let mut groups: Vec<G> = Vec::new();
    for a in soup.select(&a_sel) {
        let href = a.value().attr("href").unwrap_or("").trim();
        if href.is_empty() || ["#", "javascript:", "mailto:", "tel:"].iter().any(|p| href.starts_with(p)) {
            continue;
        }
        let joined = urljoin(page_url, href);
        let Ok(u) = url::Url::parse(&joined) else { continue };
        let nl = netloc(&joined).to_lowercase();
        if nl.strip_prefix("www.").unwrap_or(&nl) != own || u.path().trim_matches('/').is_empty() {
            continue;
        }
        let segs: Vec<&str> = u.path().split('/').filter(|s| !s.is_empty()).collect();
        let prefix = if segs.len() > 1 { format!("/{}/", segs[..segs.len() - 1].join("/")) } else { "/".to_string() };
        let text = text_stripped(a);
        let n = text.chars().count();
        if !(4..=160).contains(&n) {
            continue;
        }
        let idx = match groups.iter().position(|g| g.prefix == prefix) {
            Some(i) => i,
            None => {
                groups.push(G { prefix: prefix.clone(), hrefs: vec![], titles: vec![] });
                groups.len() - 1
            }
        };
        let g = &mut groups[idx];
        if !g.hrefs.iter().any(|h| h == u.path()) {
            g.hrefs.push(u.path().to_string());
            g.titles.push(text);
        }
    }
    let mut best: Option<(&G, usize)> = None;
    for g in &groups {
        let n = g.hrefs.len();
        if g.prefix == "/" || n < 3 {
            continue;
        }
        let score = n * if JOBLIKE_RX.is_match(&g.prefix) { 3 } else { 1 };
        if best.map_or(true, |(_, s)| score > s) {
            best = Some((g, score));
        }
    }
    let (g, _) = best?;
    let mut entry = json!({
        "type": "html", "item_selector": format!("a[href*=\"{}\"]", g.prefix), "item_href_regex": format!("{}[^/?#]+/?$", escape(&g.prefix)),
        "card": true, "fetch_details": true, "stop_when_older": true,
        "detail_date_regex": r"(?:posted|published|date)\D{0,20}(\d{4}-\d{2}-\d{2})",
    });
    let page_text: String = soup.root_element().text().collect::<Vec<_>>().join(" ");
    if Rx::lit_ci(DATE_REL).is_match(&page_text) {
        entry["date_regex"] = json!(DATE_REL);
    }
    let p2 = Rx::lit(r"[?&]page=2\b");
    if let Some(h) = soup.select(&a_sel).filter_map(|x| x.value().attr("href")).find(|h| p2.is_match(h)) {
        // the list has a "page 2" link: remember its pattern so later pages are read too
        entry["page_url_template"] = json!(Rx::lit(r"([?&]page=)2\b").replace_all(&urljoin(page_url, h), "${1}{page}").replace("${1}", "")).clone();
        let joined = urljoin(page_url, h);
        let tmpl = match joined.find("page=2") {
            Some(i) => format!("{}page={{page}}{}", &joined[..i], &joined[i + 6..]),
            None => joined,
        };
        entry["page_url_template"] = json!(tmpl);
        entry["max_pages"] = json!(5);
    }
    Some(json!({"entry": entry, "titles": g.titles.iter().take(6).collect::<Vec<_>>(), "count": g.hrefs.len()}))
}

fn feed_items(content: &[u8]) -> Vec<Vec<(String, String)>> {
    parse_feed(content).unwrap_or_default()
}

fn titles_of(items: &[Vec<(String, String)>]) -> Vec<String> {
    items.iter().take(6).map(|i| i.iter().find(|(k, _)| k == "title").map(|(_, v)| v.clone()).unwrap_or_default()).collect()
}

/// Best-effort detection of how to read a job website: RSS/Atom feed, JSON-LD job data, or an HTML list.
pub fn probe_site(http: &Http, raw: &str) -> Value {
    let mut url = raw.trim().to_string();
    if url.is_empty() {
        return json!({"ok": false, "error": "Enter the address of the website's job list."});
    }
    if !Rx::lit_ci(r"^https?://").is_match(&url) {
        url = format!("https://{}", url);
    }
    let nl = netloc(&url);
    if nl.is_empty() || !nl.contains('.') {
        return json!({"ok": false, "error": "That does not look like a web address."});
    }
    let name = nl.strip_prefix("www.").unwrap_or(&nl).to_string();
    let base = json!({"name": name, "enabled": true});
    let with = |extra: Value| -> Value {
        let mut e = base.clone();
        for (k, v) in extra.as_object().unwrap() {
            e[k] = v.clone();
        }
        e
    };
    let Some(r) = http.get(&url) else { return json!({"ok": false, "error": "Could not open that address. Check it and your internet connection."}) };
    if r.status >= 400 {
        return json!({"ok": false, "error": format!("The website answered HTTP {} for that address.", r.status)});
    }
    let items = feed_items(&r.body);
    if !items.is_empty() {
        return json!({"ok": true, "kind": "rss", "entry": with(json!({"type": "rss", "url": url})), "titles": titles_of(&items), "count": items.len(),
                      "note": "Found a job feed (RSS/Atom): the most reliable way to read a site."});
    }
    let soup = Html::parse_document(&r.text());
    for link in soup.select(&Selector::parse("link[rel=\"alternate\"]").unwrap()) {
        let ty = link.value().attr("type").unwrap_or("").to_lowercase();
        if (ty.contains("rss") || ty.contains("atom")) && link.value().attr("href").is_some() {
            let feed_url = urljoin(&url, link.value().attr("href").unwrap());
            let items = http.get(&feed_url).filter(|fr| fr.status == 200).map(|fr| feed_items(&fr.body)).unwrap_or_default();
            if !items.is_empty() {
                return json!({"ok": true, "kind": "rss", "entry": with(json!({"type": "rss", "url": feed_url})), "titles": titles_of(&items), "count": items.len(),
                              "note": "Found the site's job feed (RSS/Atom)."});
            }
        }
    }
    let postings = extract_jobpostings(&soup);
    if !postings.is_empty() {
        let titles: Vec<String> = postings.iter().take(6).map(|p| p.get("title").and_then(Value::as_str).unwrap_or("").to_string()).collect();
        return json!({"ok": true, "kind": "jsonld", "entry": with(json!({"type": "jsonld", "pages": [url]})), "titles": titles, "count": postings.len(),
                      "note": "Found structured job data on the page."});
    }
    if let Some(g) = guess_listing(&soup, &url) {
        let mut e = with(json!({"url": url}));
        for (k, v) in g["entry"].as_object().unwrap() {
            e[k] = v.clone();
        }
        return json!({"ok": true, "kind": "html", "entry": e, "titles": g["titles"], "count": g["count"],
                      "note": "Found a list of job links. This is a best guess: after adding it, run a search and check its result in the source table."});
    }
    json!({"ok": false, "error": "No job list was recognised on that page (it may need JavaScript, or the address is not the job list). Try the page that lists all jobs, or add the site under Settings > Advanced."})
}
