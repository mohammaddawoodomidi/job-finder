//! Readers for every source type: RSS/Atom, JSON-LD pages, HTML list pages (optionally browser-rendered),
//! JSON APIs and Telegram channel previews.
use crate::apply::{clean_url, telegram_job_from_post};
use crate::http::{netloc, Http};
use crate::lex::Lexicon;
use crate::model::{Job, RawPost};
use crate::render::{RenderErr, RenderHost, Renderer};
use crate::rx::Rx;
use crate::text::*;
use crate::util::trunc;
use chrono::{DateTime, Duration, Utc};
use scraper::{ElementRef, Html, Selector};
use serde_json::Value;
use std::collections::HashSet;
use std::sync::Arc;

pub struct Ctx {
    pub http: Http,
    pub prefilter: Option<Rx>,
    pub render: RenderHost,
}

pub type SrcResult = Result<Vec<Job>, String>;
pub type SiteFn = fn(&Ctx, &Value, DateTime<Utc>) -> SrcResult;

pub fn site_fn(kind: &str) -> Option<SiteFn> {
    match kind {
        "rss" => Some(src_site_rss),
        "jsonld" => Some(src_site_jsonld),
        "html" => Some(src_site_html),
        "json_api" => Some(src_site_json),
        _ => None,
    }
}
pub const SITE_TYPES: [&str; 4] = ["rss", "jsonld", "html", "json_api"];

enum SErr {
    Render(RenderErr),
    Msg(String),
}
impl From<String> for SErr {
    fn from(s: String) -> Self {
        SErr::Msg(s)
    }
}
impl From<RenderErr> for SErr {
    fn from(e: RenderErr) -> Self {
        SErr::Render(e)
    }
}

// ------------------------------------------------------------------------------------------------ helpers
pub fn urljoin(base: &str, rel: &str) -> String {
    match url::Url::parse(base) {
        Ok(b) => b.join(rel).map(|u| u.to_string()).unwrap_or_else(|_| rel.to_string()),
        Err(_) => rel.to_string(),
    }
}

/// Python's html.unescape for titles.
pub fn html_unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let doc = Html::parse_fragment(s);
    doc.root_element().text().collect::<String>()
}

/// JSON that may contain raw control characters inside strings (Python's `strict=False`).
fn lenient_json(raw: &str) -> Option<Value> {
    if let Ok(v) = serde_json::from_str(raw) {
        return Some(v);
    }
    let mut out = String::with_capacity(raw.len());
    let (mut in_str, mut esc) = (false, false);
    for c in raw.chars() {
        if in_str {
            if esc {
                esc = false;
                out.push(c);
            } else if c == '\\' {
                esc = true;
                out.push(c);
            } else if c == '"' {
                in_str = false;
                out.push(c);
            } else if (c as u32) < 0x20 {
                out.push_str(&format!("\\u{:04x}", c as u32));
            } else {
                out.push(c);
            }
        } else {
            if c == '"' {
                in_str = true;
            }
            out.push(c);
        }
    }
    serde_json::from_str(&out).ok()
}

fn sel(s: &str) -> Result<Selector, String> {
    Selector::parse(s).map_err(|e| format!("bad CSS selector {:?}: {}", s, e))
}
fn opt_sel(site: &Value, key: &str) -> Result<Option<Selector>, String> {
    let s = jstr(site, key);
    if s.is_empty() { Ok(None) } else { sel(&s).map(Some) }
}
fn opt_rx(site: &Value, key: &str, ci: bool) -> Result<Option<Rx>, String> {
    let s = jstr(site, key);
    if s.is_empty() {
        return Ok(None);
    }
    (if ci { Rx::ci(&s) } else { Rx::new(&s) }).map(Some).map_err(|e| format!("bad regular expression in {:?}: {}", key, e))
}

fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().map_or(false, |x| x != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}
/// Python's `str(x or "")`.
fn str_or_empty(v: Option<&Value>) -> String {
    if truthy(v) { json_str(v) } else { String::new() }
}

pub fn parse_feed(content: &[u8]) -> Result<Vec<Vec<(String, String)>>, String> {
    use quick_xml::events::Event;
    use quick_xml::Reader;
    let mut rd = Reader::from_reader(content);
    let mut buf = Vec::new();
    let mut items: Vec<Vec<(String, String)>> = Vec::new();
    let mut depth = 0usize;
    let mut item_depth: Option<usize> = None;
    let mut cur: Option<Vec<(String, String)>> = None;
    struct Child {
        name: String,
        text: String,
        href: String,
        rel: String,
        depth: usize,
        saw_child: bool,
    }
    let mut child: Option<Child> = None;
    let local = |raw: &str| -> String { raw.rsplit(':').next().unwrap_or("").to_string() };
    fn finish(cur: &mut Vec<(String, String)>, c: Child) {
        if c.name == "link" {
            let href = if !c.href.is_empty() { c.href.clone() } else { c.text.trim().to_string() };
            let rel = if c.rel.is_empty() { "alternate".to_string() } else { c.rel.clone() };
            if !href.is_empty() {
                match cur.iter_mut().find(|(k, _)| k == "link") {
                    Some(e) => {
                        if rel == "alternate" {
                            e.1 = href;
                        }
                    }
                    None => cur.push(("link".into(), href)),
                }
            }
        } else if !cur.iter().any(|(k, _)| *k == c.name) {
            cur.push((c.name, c.text.trim().to_string()));
        }
    }
    let attr = |e: &quick_xml::events::BytesStart, name: &str| -> String {
        e.attributes().flatten().find(|a| a.key.as_ref() == name).map(|a| a.value.to_string()).unwrap_or_default()
    };
    loop {
        match rd.read_event_into(&mut buf) {
            Err(e) => return Err(format!("not a valid feed: {}", e)),
            Ok(Event::Eof) => break,
            Ok(Event::Start(e)) => {
                depth += 1;
                let name = local(e.name().as_ref());
                if item_depth.is_none() && (name == "item" || name == "entry") {
                    item_depth = Some(depth);
                    cur = Some(Vec::new());
                } else if let (Some(id), true) = (item_depth, child.is_none()) {
                    if depth == id + 1 {
                        child = Some(Child { name, text: String::new(), href: attr(&e, "href"), rel: attr(&e, "rel"), depth, saw_child: false });
                    }
                } else if let Some(c) = child.as_mut() {
                    c.saw_child = true;
                }
            }
            Ok(Event::Empty(e)) => {
                let name = local(e.name().as_ref());
                if let (Some(id), None) = (item_depth, child.as_ref()) {
                    if depth + 1 == id + 1 {
                        if let Some(c) = cur.as_mut() {
                            finish(c, Child { name, text: String::new(), href: attr(&e, "href"), rel: attr(&e, "rel"), depth: depth + 1, saw_child: false });
                        }
                    }
                } else if let Some(c) = child.as_mut() {
                    c.saw_child = true;
                }
            }
            Ok(Event::Text(t)) => {
                if let Some(c) = child.as_mut() {
                    if !c.saw_child && depth == c.depth {
                        c.text.push_str(&t.into_inner());
                    }
                }
            }
            Ok(Event::GeneralRef(r)) => {
                if let Some(c) = child.as_mut() {
                    if !c.saw_child && depth == c.depth {
                        let name = r.into_inner().to_string();
                        c.text.push_str(match name.as_str() {
                            "amp" => "&",
                            "lt" => "<",
                            "gt" => ">",
                            "quot" => "\"",
                            "apos" => "'",
                            _ => "",
                        });
                    }
                }
            }
            Ok(Event::CData(t)) => {
                if let Some(c) = child.as_mut() {
                    if !c.saw_child && depth == c.depth {
                        c.text.push_str(&t.into_inner());
                    }
                }
            }
            Ok(Event::End(_)) => {
                if child.as_ref().map_or(false, |c| c.depth == depth) {
                    if let (Some(c), Some(cu)) = (child.take(), cur.as_mut()) {
                        finish(cu, c);
                    }
                }
                if item_depth == Some(depth) {
                    if let Some(c) = cur.take() {
                        items.push(c);
                    }
                    item_depth = None;
                }
                depth = depth.saturating_sub(1);
            }
            _ => {}
        }
        buf.clear();
    }
    Ok(items)
}

fn feed_get<'a>(e: &'a [(String, String)], key: &str) -> &'a str {
    e.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str()).unwrap_or("")
}
fn first<'a>(e: &'a [(String, String)], keys: &[&str]) -> &'a str {
    keys.iter().map(|k| feed_get(e, k)).find(|v| !v.is_empty()).unwrap_or("")
}

// ------------------------------------------------------------------------------------------------ page fetching
pub fn fetch_soup(ctx: &Ctx, url: &str, site: Option<&Value>, detail: bool) -> Result<Option<Html>, RenderErr> {
    let http = &ctx.http;
    if !http.robots_allowed(url) {
        warn!("robots.txt disallows {} - skipped", url);
        return Ok(None);
    }
    if let Some(site) = site.filter(|s| jbool(s, "render", false)) {
        let host = netloc(url);
        if http.is_blocked(&host) {
            return Ok(None);
        }
        http.throttle(url);
        let rend = ctx.render.get(&http.ua, http.block_media)?;
        let (wsel, wms) = if detail {
            (jstr(site, "detail_wait_selector"), ji64(site, "detail_wait_ms", 2000))
        } else {
            (jstr(site, "wait_for_selector"), ji64(site, "wait_ms", 1500))
        };
        return match rend.fetch(url, &wsel, wms.max(0) as u64) {
            Ok(text) => {
                http.note(&host, true);
                Ok(Some(Html::parse_document(&text)))
            }
            Err(RenderErr::Unavailable(m)) => Err(RenderErr::Unavailable(m)),
            Err(e) => {
                warn!("Browser could not load {} -> {}", url, e);
                http.note(&host, false);
                Ok(None)
            }
        };
    }
    match http.get(url) {
        Some(r) if r.status == 200 => Ok(Some(Html::parse_document(&r.text()))),
        _ => Ok(None),
    }
}

// ------------------------------------------------------------------------------------------------ RSS
pub fn src_site_rss(ctx: &Ctx, site: &Value, _cutoff: DateTime<Utc>) -> SrcResult {
    let Some(r) = ctx.http.get(&jstr(site, "url")) else { return Ok(vec![]) };
    if r.status != 200 {
        return Ok(vec![]);
    }
    let name = jstr(site, "name");
    let mut jobs = Vec::new();
    let entries = parse_feed(&r.body)?;
    info!("[{}] feed answered with {} KB and {} item(s).", name, r.body.len() / 1024, entries.len());
    for e in entries {
        let link = feed_get(&e, "link").to_string();
        let mut j = Job::new(&name, "rss", feed_get(&e, "title"));
        j.company = first(&e, &["companyName", "author", "creator"]).to_string();
        j.description = html_to_text(first(&e, &["description", "summary", "encoded", "content"]));
        j.posted_at = parse_dt_str(first(&e, &["pubDate", "published", "updated", "date"]));
        j.source_url = link.clone();
        j.apply_url = link;
        j.apply_kind = "listing".into();
        jobs.push(j);
    }
    Ok(jobs)
}

// ------------------------------------------------------------------------------------------------ JSON-LD
fn walk_ld<'a>(node: &'a Value, out: &mut Vec<&'a Value>) {
    match node {
        Value::Array(a) => a.iter().for_each(|n| walk_ld(n, out)),
        Value::Object(m) => {
            let is_job = match m.get("@type") {
                Some(Value::String(s)) => s == "JobPosting",
                Some(Value::Array(a)) => a.iter().any(|x| x.as_str() == Some("JobPosting")),
                _ => false,
            };
            if is_job {
                out.push(node);
            }
            for key in ["@graph", "itemListElement", "item", "mainEntity"] {
                if let Some(v) = m.get(key) {
                    walk_ld(v, out);
                }
            }
        }
        _ => {}
    }
}

pub fn extract_jobpostings(soup: &Html) -> Vec<Value> {
    let s = Selector::parse(r#"script[type="application/ld+json"]"#).unwrap();
    let mut out = Vec::new();
    for tag in soup.select(&s) {
        let raw: String = tag.text().collect();
        let Some(data) = lenient_json(&raw) else { continue };
        let mut found = Vec::new();
        walk_ld(&data, &mut found);
        out.extend(found.into_iter().cloned());
    }
    out
}

pub fn ld_to_job(ld: &Value, page_url: &str, source: &str) -> Job {
    let company = match ld.get("hiringOrganization") {
        Some(Value::Object(o)) => json_str(o.get("name")),
        Some(other) => str_or_empty(Some(other)),
        None => String::new(),
    };
    let mut loc_parts: Vec<String> = Vec::new();
    let locs: Vec<&Value> = match ld.get("jobLocation") {
        Some(Value::Array(a)) => a.iter().collect(),
        Some(v) => vec![v],
        None => vec![],
    };
    for item in locs {
        if let Some(addr @ Value::Object(_)) = item.get("address") {
            let country = match addr.get("addressCountry") {
                Some(Value::Object(o)) => o.get("name").cloned(),
                other => other.cloned(),
            };
            for x in [addr.get("addressLocality").cloned(), addr.get("addressRegion").cloned(), country] {
                if truthy(x.as_ref()) {
                    loc_parts.push(json_str(x.as_ref()));
                }
            }
        }
    }
    let mut hints: Vec<String> = Vec::new();
    let mut desc = html_to_text(ld.get("description").and_then(Value::as_str).unwrap_or(""));
    match ld.get("experienceRequirements") {
        Some(Value::Object(o)) if !matches!(o.get("monthsOfExperience"), None | Some(Value::Null)) && o.get("monthsOfExperience") != Some(&Value::String(String::new())) => {
            let m = o.get("monthsOfExperience").unwrap();
            let months = match m {
                Value::Number(n) => n.as_f64(),
                Value::String(s) => s.trim().parse::<f64>().ok(),
                _ => None,
            };
            if let Some(mo) = months {
                if mo <= 0.0 {
                    hints.push("no-experience (structured data)".into());
                } else {
                    desc.push_str(&format!("\nExperience requirement: {} years experience", (mo / 12.0).floor() as i64));
                }
            }
        }
        Some(Value::String(s)) => {
            if s.to_lowercase().contains("no requirement") {
                hints.push("no-experience (structured data)".into());
            } else {
                desc.push_str(&format!("\nExperience requirement: {}", s));
            }
        }
        _ => {}
    }
    let url = if truthy(ld.get("url")) { urljoin(page_url, &json_str(ld.get("url"))) } else { page_url.to_string() };
    let mut seen = HashSet::new();
    loc_parts.retain(|p| seen.insert(p.clone()));
    let mut j = Job::new(source, "website", &html_unescape(&json_str(ld.get("title"))));
    j.company = company;
    j.location = loc_parts.join(", ");
    j.description = desc;
    j.posted_at = ld.get("datePosted").and_then(parse_dt);
    j.source_url = url.clone();
    j.apply_url = url;
    j.apply_kind = if truthy(ld.get("directApply")) { "direct" } else { "listing" }.into();
    j.remote = if ld.get("jobLocationType").and_then(Value::as_str) == Some("TELECOMMUTE") { Some(true) } else { None };
    j.hints = hints;
    j.job_type = match ld.get("employmentType") {
        Some(Value::Array(a)) => a.iter().map(|x| json_str(Some(x))).collect::<Vec<_>>().join(", "),
        other => str_or_empty(other),
    };
    j
}

pub fn src_site_jsonld(ctx: &Ctx, site: &Value, _cutoff: DateTime<Utc>) -> SrcResult {
    let name = jstr(site, "name");
    let mut jobs = Vec::new();
    let mut detail_urls: Vec<String> = Vec::new();
    let follow = opt_sel(site, "follow_links_selector")?;
    for page in jlist(site, "pages") {
        let soup = match fetch_soup(ctx, &page, None, false) {
            Ok(Some(s)) => s,
            _ => continue,
        };
        jobs.extend(extract_jobpostings(&soup).iter().map(|ld| ld_to_job(ld, &page, &name)));
        if let Some(fs) = &follow {
            for a in soup.select(fs) {
                if let Some(h) = a.value().attr("href").filter(|h| !h.is_empty()) {
                    detail_urls.push(urljoin(&page, h));
                }
            }
        }
    }
    let mut seen = HashSet::new();
    detail_urls.retain(|u| seen.insert(u.clone()));
    for url in detail_urls.iter().take(ji64(site, "max_detail_pages", 40).max(0) as usize) {
        if let Ok(Some(soup)) = fetch_soup(ctx, url, None, false) {
            for ld in extract_jobpostings(&soup) {
                let mut j = ld_to_job(&ld, url, &name);
                j.page_fetched = j.source_url == *url;
                jobs.push(j);
            }
        }
    }
    Ok(jobs)
}

// ------------------------------------------------------------------------------------------------ HTML list pages
fn detail_text(dsoup: &Html, site: &Value, main: &Selector, body: &Selector) -> String {
    let node = match opt_sel(site, "detail_description_selector") {
        Ok(Some(s)) => dsoup.select(&s).next(),
        _ => dsoup.select(main).next().or_else(|| dsoup.select(body).next()),
    };
    node.map(|n| trunc(&element_to_text(n), 60000)).unwrap_or_default()
}

/// Keep only the part of a page's text between two plain-text markers (no CSS selector needed).
fn cut_text(text: &str, start: &str, end: &str) -> String {
    let mut i = if start.is_empty() { 0 } else { text.find(start).unwrap_or(0) };
    if i > text.len() {
        i = 0;
    }
    if !end.is_empty() {
        if let Some(rel) = text[i + start.len().min(text.len() - i)..].find(end) {
            let j = i + start.len().min(text.len() - i) + rel;
            if j > 0 {
                return text[i..j].to_string();
            }
        }
    }
    text[i..].to_string()
}

fn job_links<'a>(root: ElementRef<'a>, s: &Selector, rx: Option<&Rx>) -> Vec<ElementRef<'a>> {
    root.select(s).filter(|a| rx.map_or(true, |r| r.is_match(a.value().attr("href").unwrap_or("")))).collect()
}

/// The whole job 'card' around a link: the largest ancestor that still contains only this one job.
fn card_root<'a>(it: ElementRef<'a>, s: &Selector, rx: Option<&Rx>) -> ElementRef<'a> {
    let mut best = it;
    let mut cur = it.parent().and_then(ElementRef::wrap);
    while let Some(c) = cur {
        if matches!(c.value().name(), "body" | "html") {
            break;
        }
        let hrefs: HashSet<Option<&str>> = job_links(c, s, rx).iter().map(|a| a.value().attr("href")).collect();
        if hrefs.len() > 1 {
            break;
        }
        best = c;
        cur = c.parent().and_then(ElementRef::wrap);
    }
    best
}

/// Text of the longest element matching the selector (sites often show a shortened copy first).
fn longest_text(root: ElementRef, s: Option<&Selector>) -> String {
    let Some(s) = s else { return String::new() };
    let mut best = String::new();
    for e in root.select(s) {
        let t = text_stripped(e);
        if t.chars().count() > best.chars().count() {
            best = t;
        }
    }
    best
}

fn detail_snapshot(job: &Job, list_dt: Option<DateTime<Utc>>) -> Value {
    let detail_dt = if job.posted_at != list_dt { job.posted_at } else { None };
    serde_json::json!({
        "description": job.description, "full_text": job.full_text, "page_text_v2": job.page_text, "company": job.company, "location": job.location, "hints": job.hints,
        "apply_url": job.apply_url, "apply_kind": job.apply_kind,
        "detail_posted_at": detail_dt.map(|d| d.to_rfc3339()),
    })
}

/// Replays detail_snapshot() with exactly the precedence the live code uses (list values win, except where noted).
fn restore_detail(job: &mut Job, snap: &Value, site: &Value) {
    let s = |k: &str| snap.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let d = s("description");
    if !d.is_empty() {
        job.description = d;
    }
    job.full_text = s("full_text");
    job.page_text = s("page_text_v2");
    if job.company.is_empty() {
        job.company = s("company");
    }
    if job.location.is_empty() {
        job.location = s("location");
    }
    for h in snap.get("hints").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
        if !job.hints.iter().any(|x| x == h) {
            job.hints.push(h.to_string());
        }
    }
    let u = s("apply_url");
    if !u.is_empty() {
        job.apply_url = u;
    }
    let k = s("apply_kind");
    if !k.is_empty() {
        job.apply_kind = k;
    }
    if let Some(d) = snap.get("detail_posted_at").and_then(parse_dt) {
        if job.posted_at.is_none() || jbool(site, "prefer_detail_date", false) {
            job.posted_at = Some(d);
        }
    }
    job.page_fetched = true;
    job.from_cache = true;
}

struct RenderPage<'a> {
    rend: Arc<Renderer>,
    sid: u32,
    _p: std::marker::PhantomData<&'a ()>,
}
impl Drop for RenderPage<'_> {
    fn drop(&mut self) {
        self.rend.close(self.sid);
    }
}

pub fn src_site_html(ctx: &Ctx, site: &Value, cutoff: DateTime<Utc>) -> SrcResult {
    match run_html(ctx, site, cutoff) {
        Ok(j) => Ok(j),
        Err(SErr::Render(RenderErr::Unavailable(m))) => {
            error!("[{}] {}", jstr(site, "name"), m);
            Err(m) // shown in the source table instead of a silent "nothing found"
        }
        Err(SErr::Render(e)) => Err(e.to_string()),
        Err(SErr::Msg(m)) => Err(m),
    }
}

fn run_html(ctx: &Ctx, site: &Value, cutoff: DateTime<Utc>) -> Result<Vec<Job>, SErr> {
    let http = &ctx.http;
    let name = jstr(site, "name");
    let tmpl = jstr(site, "page_url_template");
    let n_pages = ji64(site, "max_pages", 1).max(1) as usize;
    let site_url = jstr(site, "url");
    let mut urls = vec![site_url.clone()];
    if !tmpl.is_empty() {
        urls.extend((2..=n_pages).map(|i| tmpl.replace("{page}", &i.to_string())));
    }
    let date_rx = opt_rx(site, "date_regex", true)?;
    let detail_date_rx = opt_rx(site, "detail_date_regex", true)?;
    let href_rx = opt_rx(site, "item_href_regex", false)?;
    let use_card = jbool(site, "card", false);
    let selector_txt = jstr(site, "item_selector");
    if selector_txt.is_empty() {
        return Err("KeyError: 'item_selector'".to_string().into());
    }
    let selector = sel(&selector_txt)?;
    let (title_sel, date_sel, company_sel, location_sel, link_sel) =
        (opt_sel(site, "title_selector")?, opt_sel(site, "date_selector")?, opt_sel(site, "company_selector")?, opt_sel(site, "location_selector")?, opt_sel(site, "link_selector")?);
    let alt_rx = opt_rx(site, "company_from_alt", true)?;
    let apply_rx = Rx::lit_ci(r"\bapply\b|başvur|ثبت نام|ارسال رزومه");
    let (main_sel, body_sel, a_href, img_alt) = (sel("main")?, sel("body")?, sel("a[href]")?, sel("img[alt]")?);
    let fetch_details = jbool(site, "fetch_details", true);
    let now = utcnow();
    let cache = http.cache.as_ref();
    let mut jobs: Vec<Job> = Vec::new();
    let mut seen_hrefs: HashSet<String> = HashSet::new();
    let mut pages_read = 0usize;

    let render_mode = jbool(site, "render", false) && tmpl.is_empty();
    let mut render_page: Option<RenderPage> = None;
    if render_mode {
        if !http.robots_allowed(&site_url) {
            warn!("robots.txt disallows {} - skipped", site_url);
            return Ok(jobs);
        }
        let rend = ctx.render.get(&http.ua, http.block_media)?;
        http.throttle(&site_url);
        let sid = rend.open(&site_url, &jstr(site, "wait_for_selector"), ji64(site, "wait_ms", 1500).max(0) as u64)?;
        render_page = Some(RenderPage { rend, sid, _p: std::marker::PhantomData });
    }
    let total = if render_mode { n_pages } else { urls.len() };

    for page_no in 1..=total {
        // ---- get the page
        let (page_url, soup) = if let Some(rp) = &render_page {
            if page_no > 1 && !rp.rend.advance(rp.sid, &jstr(site, "next_selector"), ji64(site, "wait_ms", 1500).max(0) as u64)? {
                break;
            }
            (site_url.clone(), Html::parse_document(&rp.rend.html(rp.sid)?))
        } else {
            let u = urls[page_no - 1].clone();
            match fetch_soup(ctx, &u, Some(site), false)? {
                Some(s) => (u, s),
                None => break,
            }
        };
        let items = job_links(soup.root_element(), &selector, href_rx.as_ref());
        if items.is_empty() {
            warn!("[{}] selector {:?} matched nothing on {}", name, selector_txt, page_url);
            break;
        }
        info!("[{}] list page {}/{}: {} job links found", name, page_no, urls.len(), items.len());
        let mut page_jobs: Vec<Job> = Vec::new();
        for it in items {
            if http.cancelled() {
                break;
            }
            let is_a = it.value().name() == "a";
            let card = if is_a && (use_card || date_rx.is_some()) { card_root(it, &selector, href_rx.as_ref()) } else { it };
            let root = if use_card { card } else { it };
            let a_el = link_sel.as_ref().and_then(|s| root.select(s).next()).or(if is_a { Some(it) } else { None });
            let href = match a_el.and_then(|a| a.value().attr("href")).filter(|h| !h.is_empty()) {
                Some(h) => urljoin(&page_url, h),
                None => String::new(),
            };
            let title = if title_sel.is_some() {
                longest_text(root, title_sel.as_ref())
            } else if let Some(a) = a_el {
                // the first link is often a shortened copy: take the longest text of the same link
                let raw_href = a.value().attr("href");
                let mut best = text_stripped(a);
                for x in root.select(&a_href) {
                    if x.value().attr("href") == raw_href {
                        let t = text_stripped(x);
                        if t.chars().count() > best.chars().count() {
                            best = t;
                        }
                    }
                }
                best
            } else {
                String::new()
            };
            if title.is_empty() {
                continue;
            }
            let d_el = date_sel.as_ref().and_then(|s| root.select(s).next());
            let mut date_raw: Option<String> = None;
            if let Some(d) = d_el {
                let attr = jstr(site, "date_attribute");
                date_raw = Some(match (!attr.is_empty()).then(|| d.value().attr(&attr)).flatten().filter(|v| !v.is_empty()) {
                    Some(v) => v.to_string(),
                    None => text_stripped(d),
                });
            } else if date_rx.is_some() {
                date_raw = Some(text_stripped(card));
            }
            if let (Some(rx), Some(raw)) = (&date_rx, &date_raw) {
                if !raw.is_empty() {
                    date_raw = rx.captures(raw).and_then(|c| c.get(1)).map(|m| m.as_str().to_string());
                }
            }
            let mut list_dt = date_raw.as_deref().and_then(parse_dt_str);
            if list_dt.map_or(false, |d| d > now + Duration::hours(12)) {
                list_dt = None; // a date in the future is a closing date, not the posting date
            }
            if jbool(site, "require_list_date", false) && list_dt.is_none() {
                continue; // e.g. side-bar lists of other jobs: they have no "x hours ago" next to them
            }
            if !href.is_empty() && !seen_hrefs.insert(href.clone()) {
                continue; // the same job linked twice (title + "more locations" ...)
            }
            let mut company_txt = longest_text(root, company_sel.as_ref());
            if company_txt.is_empty() {
                if let Some(arx) = &alt_rx {
                    for img in root.select(&img_alt) {
                        if let Some(c) = arx.captures(img.value().attr("alt").unwrap_or("")) {
                            company_txt = c.get(1).map(|m| m.as_str().trim().to_string()).unwrap_or_default();
                            break;
                        }
                    }
                }
            }
            let mut job = Job::new(&name, "website", &title);
            job.needs_render = jbool(site, "render", false);
            job.company = company_txt;
            job.location = longest_text(root, location_sel.as_ref());
            job.description = text_stripped(card);
            job.posted_at = list_dt;
            job.source_url = href.clone();
            job.apply_url = href.clone();
            job.apply_kind = "listing".into();
            // don't waste a request on jobs that will be rejected anyway
            let skip_details = list_dt.map_or(false, |d| d < cutoff) || crate::gender::title_is_female_only(&title) || ctx.prefilter.as_ref().map_or(false, |p| p.is_match(&normalize(&title)));
            if fetch_details && !href.is_empty() && !skip_details {
                let ckey = if cache.is_some() { crate::cache::ScrapeCache::detail_key(site, &href) } else { String::new() };
                let snap = cache.and_then(|c| c.get_detail(&ckey)).filter(|sn| sn.get("page_text_v2").is_some()); // entries from older versions lack the full page text: read those pages again
                if let Some(snap) = snap {
                    restore_detail(&mut job, &snap, site); // read in an earlier run: no download needed
                } else {
                    let list_dt_before = job.posted_at;
                    let dsoup = fetch_soup(ctx, &href, Some(site), true)?;
                    pages_read += 1;
                    if pages_read % 5 == 0 {
                        info!("[{}]   ... {} job pages read so far", name, pages_read);
                    }
                    if let Some(dsoup) = &dsoup {
                        job.page_fetched = true;
                        let full_text = detail_text(dsoup, site, &main_sel, &body_sel);
                        job.full_text = trunc(&full_text, 20000);
                        job.page_text = dsoup.select(&body_sel).next().map(|b| trunc(&text_stripped(b), 40000)).unwrap_or_default(); // every text piece separated by a space, so "Gender" and "Female" in neighbouring tags stay two words
                        let lds = extract_jobpostings(dsoup);
                        if let Some(first_ld) = lds.first() {
                            let ldj = ld_to_job(first_ld, &href, &name);
                            if !ldj.description.is_empty() {
                                job.description = ldj.description.clone();
                            }
                            job.posted_at = job.posted_at.or(ldj.posted_at);
                            if job.company.is_empty() {
                                job.company = ldj.company.clone();
                            }
                            if job.location.is_empty() {
                                job.location = ldj.location.clone();
                            }
                            job.hints.extend(ldj.hints.clone());
                            if ldj.apply_kind == "direct" {
                                job.apply_kind = "direct".into();
                            }
                        } else if !full_text.is_empty() {
                            job.description = trunc(&cut_text(&full_text, &jstr(site, "detail_text_start"), &jstr(site, "detail_text_end")), 8000);
                        }
                        if let Some(drx) = &detail_date_rx {
                            let d = drx.captures(&full_text).and_then(|c| c.get(1)).and_then(|m| parse_dt_str(m.as_str()));
                            if let Some(d) = d {
                                if d <= now + Duration::hours(12) && (job.posted_at.is_none() || jbool(site, "prefer_detail_date", false)) {
                                    job.posted_at = Some(d); // the job page's own date, when the list page had none
                                }
                            }
                        }
                        for a in dsoup.select(&a_href) {
                            // an explicit apply button, if any
                            if apply_rx.is_match(&text_stripped(a)) {
                                job.apply_url = urljoin(&href, a.value().attr("href").unwrap_or(""));
                                job.apply_kind = "direct".into();
                                break;
                            }
                        }
                    }
                    if let (Some(c), Some(_)) = (cache, &dsoup) {
                        c.put_detail(&ckey, &detail_snapshot(&job, list_dt_before));
                    }
                }
            }
            page_jobs.push(job);
        }
        let dated: Vec<DateTime<Utc>> = page_jobs.iter().filter_map(|j| j.posted_at).collect();
        jobs.extend(page_jobs);
        if jbool(site, "stop_when_older", false) && !dated.is_empty() && dated.iter().all(|d| *d < cutoff) {
            // newest-first sites: once a whole page is too old, stop paging
            info!("[{}] page {} is entirely older than the window - stopping here", name, page_no);
            break;
        }
    }
    drop(render_page);
    Ok(jobs)
}

// ------------------------------------------------------------------------------------------------ JSON API
fn with_query(url: &str, params: &[(String, String)]) -> String {
    if params.is_empty() {
        return url.to_string();
    }
    match url::Url::parse(url) {
        Ok(mut u) => {
            u.query_pairs_mut().extend_pairs(params.iter().map(|(k, v)| (k.as_str(), v.as_str())));
            u.to_string()
        }
        Err(_) => url.to_string(),
    }
}

pub fn src_site_json(ctx: &Ctx, site: &Value, _cutoff: DateTime<Utc>) -> SrcResult {
    let http = &ctx.http;
    let url = jstr(site, "url");
    let sid = jstr(site, "json_from_html_script_id");
    if !sid.is_empty() && !http.robots_allowed(&url) {
        warn!("robots.txt disallows {} - skipped", url);
        return Ok(vec![]);
    }
    let method = { let m = jstr(site, "method"); if m.is_empty() { "GET".to_string() } else { m.to_uppercase() } };
    let pparam = jstr(site, "page_param");
    let n_pages = if pparam.is_empty() { 1 } else { ji64(site, "pages", 1).max(1) };
    let start = ji64(site, "page_start", 1);
    let step = ji64(site, "page_step", 1).max(1);
    let fields = site.get("fields").cloned().unwrap_or(Value::Null);
    let headers: Vec<(String, String)> = site.get("headers").and_then(Value::as_object).map(|m| m.iter().map(|(k, v)| (k.clone(), json_str(Some(v)))).collect()).unwrap_or_default();
    let name = jstr(site, "name");
    let script_sel = if sid.is_empty() { None } else { Some(sel(&format!("script[id=\"{}\"]", sid.replace('"', "")))?) };
    let mut jobs = Vec::new();
    let mut n = start;
    while n < start + n_pages * step {
        let mut params: Vec<(String, String)> = site.get("params").and_then(Value::as_object).map(|m| m.iter().map(|(k, v)| (k.clone(), json_str(Some(v)))).collect()).unwrap_or_default();
        let mut body = site.get("json_body").filter(|b| !b.is_null()).cloned();
        if !pparam.is_empty() {
            match (&mut body, method.as_str()) {
                (Some(Value::Object(m)), "POST") => {
                    m.insert(pparam.clone(), Value::from(n));
                }
                _ => {
                    params.retain(|(k, _)| *k != pparam);
                    params.push((pparam.clone(), n.to_string()));
                }
            }
        }
        let full = with_query(&url, &params);
        let mut resp = None;
        for attempt in 0..3u64 {
            let r = if method == "POST" { http.post_json(&full, &body.clone().unwrap_or(Value::Null), &headers) } else { http.get_with(&full, &headers) };
            if let Some(r) = r.filter(|r| r.status == 200) {
                resp = Some(r);
                break;
            }
            if http.cancelled() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1500 * (attempt + 1))); // rate limit or hiccup: wait, then try the same page again
        }
        let Some(r) = resp else {
            if !http.cancelled() {
                warn!("[{}] page {} could not be read after 3 tries - the rest of this source was skipped.", name, n);
            }
            break;
        };
        let data: Option<Value> = if let Some(ss) = &script_sel {
            let soup = Html::parse_document(&r.text());
            soup.select(ss).next().and_then(|t| serde_json::from_str(&t.text().collect::<String>()).ok())
        } else {
            match r.json() {
                Some(v) => Some(v),
                None => break,
            }
        };
        let items: Option<Value> = match (&data, jstr(site, "items_path")) {
            (Some(d), p) if !p.is_empty() => dig(d, &p).cloned(),
            (Some(d), _) => Some(d.clone()),
            _ => None,
        };
        let Some(Value::Array(items)) = items.filter(|i| truthy(Some(i))) else { break };
        for it in &items {
            let g = |k: &str| -> Option<&Value> { fields.get(k).and_then(Value::as_str).filter(|p| !p.is_empty()).and_then(|p| dig(it, p)) };
            let mut u = str_or_empty(g("url"));
            if !u.is_empty() && !u.starts_with("http://") && !u.starts_with("https://") {
                u = urljoin(&url, &u);
            }
            let mut j = Job::new(&name, "website", &str_or_empty(g("title")));
            j.company = str_or_empty(g("company"));
            j.location = str_or_empty(g("location"));
            j.description = html_to_text(&str_or_empty(g("description")));
            j.posted_at = g("date").and_then(parse_dt);
            j.source_url = u.clone();
            let apply = str_or_empty(g("apply_url"));
            j.apply_url = if apply.is_empty() { u.clone() } else { apply.clone() };
            j.apply_kind = if truthy(g("apply_url")) { "direct" } else { "listing" }.into();
            jobs.push(j);
        }
        if !sid.is_empty() {
            break;
        }
        n += step;
    }
    Ok(jobs)
}

// ------------------------------------------------------------------------------------------------ Telegram
pub fn scrape_telegram_web(http: &Http, tg: &Value, channel: &str, cutoff: DateTime<Utc>) -> Vec<RawPost> {
    let base = { let b = jstr(tg, "web_preview_base"); if b.is_empty() { "https://t.me/s/".to_string() } else { b } };
    let max_pages = ji64(tg, "max_pages_per_channel", 8).max(1);
    let (s_msg, s_time, s_date, s_text, s_links, s_btn, s_br) = (
        Selector::parse("div.tgme_widget_message").unwrap(),
        Selector::parse("a.tgme_widget_message_date time").unwrap(),
        Selector::parse("a.tgme_widget_message_date").unwrap(),
        Selector::parse("div.tgme_widget_message_text").unwrap(),
        Selector::parse("a[href]").unwrap(),
        Selector::parse("a.tgme_widget_message_inline_button[href]").unwrap(),
        (),
    );
    let _ = s_br;
    let mut posts = Vec::new();
    let mut before: Option<i64> = None;
    let mut seen: HashSet<i64> = HashSet::new();
    for _ in 0..max_pages {
        if http.cancelled() {
            break;
        }
        let url = format!("{}{}{}", base, channel, before.map(|b| format!("?before={}", b)).unwrap_or_default());
        let r = http.get(&url);
        let Some(r) = r.filter(|r| r.status == 200) else {
            warn!("[telegram @{}] could not load preview", channel);
            break;
        };
        let soup = Html::parse_document(&r.text());
        let wraps: Vec<ElementRef> = soup.select(&s_msg).collect();
        if wraps.is_empty() {
            warn!("[telegram @{}] no messages in preview. Channel may be private or preview disabled.", channel);
            break;
        }
        let (mut min_id, mut oldest): (Option<i64>, Option<DateTime<Utc>>) = (None, None);
        for w in wraps {
            let data_post = w.value().attr("data-post").unwrap_or("");
            let tail = data_post.rsplit('/').next().unwrap_or("");
            let mid = if !tail.is_empty() && tail.chars().all(|c| c.is_ascii_digit()) { tail.parse::<i64>().ok() } else { None };
            if let Some(m) = mid {
                min_id = Some(min_id.map_or(m, |x| x.min(m)));
                if !seen.insert(m) {
                    continue;
                }
            }
            let dt = w.select(&s_time).next().and_then(|t| t.value().attr("datetime")).and_then(parse_dt_str);
            if let Some(d) = dt {
                if oldest.map_or(true, |o| d < o) {
                    oldest = Some(d);
                }
            }
            let permalink = w.select(&s_date).next().and_then(|a| a.value().attr("href")).filter(|h| !h.is_empty()).map(str::to_string).unwrap_or_else(|| format!("https://t.me/{}", data_post));
            let Some(text_el) = w.select(&s_text).next() else { continue };
            let mut links: Vec<String> = text_el.select(&s_links).filter_map(|a| a.value().attr("href")).filter(|h| h.starts_with("http") || h.starts_with("mailto:")).map(str::to_string).collect();
            links.extend(w.select(&s_btn).filter_map(|a| a.value().attr("href")).filter(|h| h.starts_with("http")).map(str::to_string));
            posts.push(RawPost { channel: channel.to_string(), text: text_with_br(text_el), links, posted_at: dt, permalink });
        }
        if min_id.is_none() || oldest.map_or(false, |o| o < cutoff) {
            break;
        }
        before = min_id;
    }
    posts
}

pub fn telegram_jobs(posts: Vec<RawPost>, label: &str, lex: &Lexicon) -> Vec<Job> {
    let n = posts.len();
    let jobs: Vec<Job> = posts.iter().filter_map(|p| telegram_job_from_post(p, lex)).collect();
    info!("telegram {}: {} posts read, {} look like job ads", label, n, jobs.len());
    jobs
}

#[allow(dead_code)]
fn _unused() -> String {
    clean_url("")
}

#[cfg(test)]
mod feed_tests {
    #[test]
    fn himalayas_like_feed_keeps_all_items() {
        let p = std::env::var("HIM_XML").unwrap_or_default();
        if p.is_empty() { return; }
        let b = std::fs::read(p).unwrap();
        let items = super::parse_feed(&b).unwrap();
        assert_eq!(items.len(), 100);
        let t = items[99].iter().find(|(k, _)| k == "title").map(|x| x.1.clone()).unwrap_or_default();
        assert_eq!(t, "Job 99");
    }
}
