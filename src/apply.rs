//! Telegram posts -> jobs, and the "How to apply" extraction (links, e-mail, Telegram, WhatsApp, phone).
use crate::http::netloc;
use crate::lex::Lexicon;
use crate::lexdata as d;
use crate::model::{ApplyMethod, Job, RawPost};
use crate::rx::Rx;
use crate::text::normalize;
use crate::util::trunc;
use serde_json::Value;
use std::collections::HashSet;
use std::sync::LazyLock;
use unicode_general_category::{get_general_category as gc, GeneralCategory as G};

pub static EMAIL_RE: LazyLock<Rx> = LazyLock::new(|| Rx::lit(d::EMAIL_RE));
pub static URL_RE: LazyLock<Rx> = LazyLock::new(|| Rx::lit_ci(d::URL_RE));
static HANDLE_RE: LazyLock<Rx> = LazyLock::new(|| Rx::lit(d::HANDLE_RE));
static PHONE_CUE: LazyLock<Rx> = LazyLock::new(|| Rx::lit_ci(d::PHONE_CUE));
static PHONE_RE: LazyLock<Rx> = LazyLock::new(|| Rx::lit(d::PHONE_RE));
static NOT_APPLY: LazyLock<Rx> = LazyLock::new(|| Rx::lit_ci(d::NOT_APPLY));
static LINK_BOOST: LazyLock<Rx> = LazyLock::new(|| Rx::lit_ci(d::LINK_BOOST_WORDS));
static COMPANY_RE: LazyLock<Rx> = LazyLock::new(|| Rx::lit(&format!("(?im){}", d::COMPANY_RE)));
static LOC_RE: LazyLock<Rx> = LazyLock::new(|| Rx::lit(&format!("(?im){}", d::LOC_RE)));
static APPLY_STRONG: LazyLock<Rx> = LazyLock::new(|| Rx::lit_ci(d::APPLY_STRONG));
static APPLY_BROAD: LazyLock<Rx> = LazyLock::new(|| Rx::lit_ci(d::APPLY_BROAD));
static JOIN_NOISE: LazyLock<Rx> = LazyLock::new(|| Rx::lit_ci(d::JOIN_NOISE));
static NOISE_CHARS: LazyLock<Rx> = LazyLock::new(|| Rx::lit(r"[#*_`~>•▪●◆■□▶◀]+"));
static WS: LazyLock<Rx> = LazyLock::new(|| Rx::lit(r"\s+"));
static WORD2: LazyLock<Rx> = LazyLock::new(|| Rx::lit(r"[^\W\d_]{2,}"));
static LEAD_SCHEME: LazyLock<Rx> = LazyLock::new(|| Rx::lit(r"^(?:mailto:|https?://(?:www\.)?)"));
static LINKEDIN_JOBS: LazyLock<Rx> = LazyLock::new(|| Rx::lit(r"/(?:comm/)?jobs/"));
static NON_DIGIT: LazyLock<Rx> = LazyLock::new(|| Rx::lit(r"\D"));
static CHAN_PREFIX: LazyLock<Rx> = LazyLock::new(|| Rx::lit_ci(r"^https?://(?:t|telegram)\.me/(?:s/)?"));

pub fn clean_url(u: &str) -> String {
    let u = u.trim_end_matches(['.', ',', ';', ':', '!', '?', ')', ']', '}', '"', '\'']);
    if u.to_lowercase().starts_with("www.") {
        format!("https://{}", u)
    } else {
        u.to_string()
    }
}

pub enum Channel {
    Name(String),
    Id(i64),
}

pub fn normalize_channel(raw: &Value) -> Option<Channel> {
    if let Some(n) = raw.as_i64() {
        return if n == 0 { None } else { Some(Channel::Id(n)) };
    }
    let s = raw.as_str()?.trim().to_string();
    let s = CHAN_PREFIX.replace_all(&s, "");
    let s = s.trim_start_matches('@').trim_matches('/').trim().to_string();
    let s = s.split('/').next().unwrap_or("").split('?').next().unwrap_or("").to_string();
    if s.is_empty() {
        return None;
    }
    let digits = s.strip_prefix('-').unwrap_or(&s);
    if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
        return s.parse().ok().map(Channel::Id);
    }
    Some(Channel::Name(s))
}

fn clean_title_line(line: &str) -> String {
    let kept: String = line.chars().filter(|&ch| !matches!(gc(ch), G::OtherSymbol | G::ModifierSymbol | G::Surrogate | G::Format | G::NonspacingMark) || ch == '\u{200c}').collect();
    let out = NOISE_CHARS.replace_all(&kept, " ");
    let out = WS.replace_all(&out, " ");
    out.trim().trim_matches([' ', '-', '–', '—', ':', '|', '.']).to_string()
}

pub fn telegram_job_from_post(p: &RawPost, lex: &Lexicon) -> Option<Job> {
    let text = p.text.trim();
    if text.chars().count() < 40 || !lex.intent.is_match(&normalize(text)) {
        return None;
    }
    let mut title = String::new();
    for line in text.lines() {
        title = clean_title_line(line);
        if title.chars().count() >= 5 {
            break;
        }
    }
    let title = trunc(&title, 140);
    let title = if title.is_empty() { "Telegram job post".to_string() } else { title };
    let mut links: Vec<String> = p.links.iter().map(|u| clean_url(u)).collect();
    links.extend(URL_RE.find_all_str(text).iter().map(|u| clean_url(u)));
    links.extend(EMAIL_RE.find_all_str(text).iter().map(|e| format!("mailto:{}", e)));
    let mut seen = HashSet::new();
    links.retain(|l| seen.insert(l.clone()));
    let grab = |re: &Rx| re.captures(text).and_then(|c| c.get(1)).map(|m| trunc(m.as_str().trim(), 100)).unwrap_or_default();
    let mut j = Job::new(&format!("Telegram: @{}", p.channel), "telegram", &title);
    j.description = text.to_string();
    j.company = grab(&COMPANY_RE);
    j.location = grab(&LOC_RE);
    j.posted_at = p.posted_at;
    j.source_url = p.permalink.clone();
    j.candidate_links = links;
    Some(j)
}

// ------------------------------------------------------------------------------------------------ how to apply
pub fn extract_phones(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in text.lines() {
        let line = EMAIL_RE.replace_all(&URL_RE.replace_all(&normalize(raw), " "), " ");
        let cue = PHONE_CUE.is_match(&line);
        for m in PHONE_RE.find_iter(&line) {
            let num = WS.replace_all(m.as_str(), " ");
            let num = num.trim_matches([' ', '.', '-', '(', ')']).to_string();
            let digits = NON_DIGIT.replace_all(&num, "");
            let n = digits.len();
            if (9..=15).contains(&n) && (cue || num.starts_with('+') || num.starts_with("00")) && !out.contains(&num) {
                out.push(num);
            }
        }
    }
    out
}

/// (kind, score, value) for links that can be used to apply/contact.
fn classify_link(u: &str, own_channel: &str) -> Option<(&'static str, i32, String)> {
    let p = url::Url::parse(u).ok()?;
    if !matches!(p.scheme(), "http" | "https") {
        return None;
    }
    let nl = netloc(u).to_lowercase();
    let host = nl.strip_prefix("www.").unwrap_or(&nl).to_string();
    if host.is_empty() {
        return None;
    }
    if host == "wa.me" || host == "api.whatsapp.com" {
        let src = if host == "wa.me" { p.path().to_string() } else { p.query_pairs().find(|(k, _)| k == "phone").map(|(_, v)| v.into_owned()).unwrap_or_default() };
        let digits = NON_DIGIT.replace_all(&src, "");
        return if digits.is_empty() { None } else { Some(("whatsapp", 3, format!("+{}", digits))) };
    }
    if host == "t.me" || host == "telegram.me" {
        let seg: Vec<&str> = p.path().split('/').filter(|s| !s.is_empty()).collect();
        if seg.len() != 1 || seg[0].starts_with('+') || matches!(seg[0].to_lowercase().as_str(), "joinchat" | "s") || seg[0].to_lowercase() == own_channel {
            return None;
        }
        return Some(("telegram", 3, format!("@{}", seg[0])));
    }
    if d::DENY_HOSTS.iter().any(|dn| host == *dn || host.ends_with(&format!(".{}", dn))) {
        return None;
    }
    if host.ends_with("linkedin.com") && !LINKEDIN_JOBS.is_match(p.path()) {
        return None;
    }
    let mut score = 1;
    if d::ATS_HOSTS.iter().any(|h| host == *h || host.ends_with(&format!(".{}", h))) {
        score += 4;
    }
    if LINK_BOOST.is_match(u) {
        score += 3;
    }
    Some(("link", score, u.to_string()))
}

/// Copy the sentences/lines of the post that explain how to apply (CV by e-mail, form, deadline, ...).
pub fn extract_apply_lines(text: &str, broad: bool, max_lines: usize) -> String {
    let mut picked: Vec<String> = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.chars().count() < 6 {
            continue;
        }
        let n = normalize(line);
        if JOIN_NOISE.is_match(&n) {
            continue;
        }
        let has_contact = URL_RE.is_match(line) || EMAIL_RE.is_match(line) || HANDLE_RE.is_match(line);
        if !(APPLY_STRONG.is_match(&n) || (broad && (APPLY_BROAD.is_match(&n) || has_contact))) {
            continue;
        }
        let rest = HANDLE_RE.replace_all(&EMAIL_RE.replace_all(&URL_RE.replace_all(line, " "), " "), " ");
        if WORD2.find_iter(&rest).len() < 2 {
            continue; // skip lines that are only a bare link/e-mail
        }
        let cleaned: String = line.chars().filter(|&ch| !matches!(gc(ch), G::OtherSymbol | G::ModifierSymbol | G::Surrogate)).collect();
        let cleaned = cleaned.trim().trim_matches([' ', '\t', '-', '–', '—', '•', '▪', '●', '◆', '■', '□', '▶', '◀', '*', '#', '>', '|']);
        let s = trunc(cleaned, 300);
        if !picked.contains(&s) {
            picked.push(s);
        }
        if picked.len() >= max_lines {
            break;
        }
    }
    picked.join("\n")
}

fn strip_www(h: &str) -> &str {
    h.strip_prefix("www.").unwrap_or(h)
}

/// Fills job.apply_methods (links / e-mail / Telegram / WhatsApp / phone) and job.apply_text.
pub fn build_how_to_apply(job: &mut Job) {
    let tele = job.source_type == "telegram";
    let own = if tele { job.source.rsplit('@').next().unwrap_or("").to_lowercase() } else { String::new() };
    let text = job.description.clone();
    let mut methods: Vec<ApplyMethod> = Vec::new();
    let mut seen: HashSet<(String, String)> = HashSet::new();
    let mut add = |kind: &str, label: &str, value: &str, href: &str| {
        if seen.insert((kind.to_string(), value.to_lowercase())) {
            methods.push(ApplyMethod { kind: kind.into(), label: label.into(), value: value.into(), href: href.into(), ..Default::default() });
        }
    };

    let mut cands: Vec<String> = job.candidate_links.clone();
    if !tele {
        // a website's description text may contain the real application form / e-mail address
        let own_host = if job.source_url.is_empty() { String::new() } else { strip_www(&netloc(&job.source_url).to_lowercase()).to_string() };
        for u in URL_RE.find_all_str(&text) {
            let u = clean_url(&u);
            if strip_www(&netloc(&u).to_lowercase()) != own_host {
                cands.push(u);
            }
        }
    }
    // ignore addresses that sit in lines like "report misconduct to ..." or "do NOT send your CV to ..."
    let bad_lines: Vec<String> = text.lines().map(normalize).filter(|ln| NOT_APPLY.is_match(ln)).collect();
    let usable = |tok: &str| -> bool {
        let low = tok.to_lowercase();
        let stripped = LEAD_SCHEME.replace_all(&low, "");
        let core = stripped.split('?').next().unwrap_or("").trim_end_matches('/').to_string();
        !bad_lines.iter().any(|bl| bl.contains(&core))
    };
    cands.retain(|u| usable(u));
    if job.apply_url.starts_with("http://") || job.apply_url.starts_with("https://") {
        cands.insert(0, job.apply_url.clone());
    }
    let mut links: Vec<(i32, usize, String)> = Vec::new();
    let mut others: Vec<(&'static str, String, String)> = Vec::new();
    for (idx, u) in cands.iter().enumerate() {
        match classify_link(u, &own) {
            Some(("link", score, _)) => links.push((-score, idx, u.clone())),
            Some((kind, _, value)) => others.push((kind, value, u.clone())),
            None => {}
        }
    }
    links.sort();
    for (negscore, _, u) in links.iter().take(3) {
        let label = if !tele && *u == job.apply_url {
            if job.apply_kind == "listing" { "Apply on the listing page" } else { "Apply link" }
        } else if -negscore >= 4 {
            "Application link"
        } else {
            "Link in the post"
        };
        add("link", label, u, u);
    }
    for (kind, value, u) in others.iter().take(3) {
        add(kind, if *kind == "telegram" { "Message on Telegram" } else { "WhatsApp" }, value, u);
    }
    let mut emails: Vec<String> = cands.iter().filter(|u| u.starts_with("mailto:")).map(|u| u[7..].split('?').next().unwrap_or("").to_string()).collect();
    emails.extend(EMAIL_RE.find_all_str(&text));
    let mut seen_e = HashSet::new();
    let emails: Vec<String> = emails.into_iter().map(|x| x.trim().trim_end_matches(['.', ',', ';']).to_string()).filter(|x| seen_e.insert(x.clone())).collect();
    for e in emails.iter().take(3) {
        let low = e.to_lowercase();
        if usable(e) && !(low.ends_with(".png") || low.ends_with(".jpg") || low.ends_with(".jpeg") || low.ends_with(".gif") || low.ends_with(".webp")) {
            add("email", "Email your CV to", e, &format!("mailto:{}", e));
        }
    }
    let mut seen_h = HashSet::new();
    let handles: Vec<String> = HANDLE_RE.captures_all(&text).iter().filter_map(|c| c.get(1).map(|m| m.as_str().to_string())).filter(|h| seen_h.insert(h.clone())).collect();
    for h in handles.iter().take(3) {
        if h.to_lowercase() != own {
            add("telegram", "Message on Telegram", &format!("@{}", h), &format!("https://t.me/{}", h));
        }
    }
    for num in extract_phones(&text).iter().take(2) {
        let digits = NON_DIGIT.replace_all(num, "");
        let href = if num.starts_with('+') || num.starts_with("00") {
            format!("tel:+{}", if num.starts_with("00") { &digits[2..] } else { &digits[..] })
        } else {
            String::new()
        };
        add("phone", "Phone / WhatsApp", num, &href);
    }
    job.apply_methods = methods;
    job.apply_text = extract_apply_lines(&text, tele, 5);
}
