//! Multilingual text normalisation, keyword patterns, dates, HTML -> text.
use crate::rx::{escape, Rx};
use chrono::{DateTime, Duration, NaiveDate, NaiveDateTime, TimeZone, Utc};
use scraper::{ElementRef, Html, Node};
use serde_json::Value;
use std::sync::LazyLock;
use unicode_normalization::UnicodeNormalization;

pub fn utcnow() -> DateTime<Utc> {
    Utc::now()
}

// ------------------------------------------------------------------------------------------------ normalisation
fn map_char(c: char) -> Option<char> {
    // Persian / Arabic-Indic digits -> ASCII, then letter variants unified; None = drop the character
    Some(match c {
        '۰' | '٠' => '0',
        '۱' | '١' => '1',
        '۲' | '٢' => '2',
        '۳' | '٣' => '3',
        '۴' | '٤' => '4',
        '۵' | '٥' => '5',
        '۶' | '٦' => '6',
        '۷' | '٧' => '7',
        '۸' | '٨' => '8',
        '۹' | '٩' => '9',
        'ي' | 'ى' => 'ی',
        'ك' => 'ک',
        'ۀ' | 'ة' => 'ه',
        'ı' => 'i',
        '\u{200c}' => ' ',
        '\u{064b}'..='\u{065f}' | '\u{0670}' | '\u{0640}' | '\u{200d}' | '\u{200e}' | '\u{200f}' | '\u{0307}' => return None,
        c => c,
    })
}

/// Lower-case, unify Arabic/Persian letter variants, Persian digits -> ASCII, ZWNJ -> space, remove diacritics,
/// fold Turkish dotless i.
pub fn normalize(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    let lowered: String = text.nfkc().collect::<String>().to_lowercase().replace('\u{2019}', "'");
    let mut out = String::with_capacity(lowered.len());
    let mut prev_space = false;
    for c in lowered.chars() {
        let Some(c) = map_char(c) else { continue };
        // runs of blanks (but not newlines) collapse to one space
        if matches!(c, ' ' | '\t' | '\r' | '\u{0c}' | '\u{0b}') {
            if !prev_space {
                out.push(' ');
            }
            prev_space = true;
        } else {
            out.push(c);
            prev_space = false;
        }
    }
    out
}

fn kw_regex(keyword: &str, prefix_only: bool) -> String {
    let parts: Vec<String> = normalize(keyword).split_whitespace().map(escape).collect();
    let body = parts.join(r"[\s\-_]*");
    // prefix_only -> allows suffixes (Persian plurals, Turkish agglutination)
    if prefix_only {
        format!(r"(?<!\w){}", body)
    } else {
        format!(r"\b{}\b", body)
    }
}

pub fn compile_keywords(en: &[&str], fa: &[&str], tr: &[&str], extra: &[String], raw: &[&str]) -> Rx {
    let mut pats: Vec<String> = en.iter().map(|k| kw_regex(k, false)).collect();
    pats.extend(fa.iter().chain(tr.iter()).map(|k| kw_regex(k, true)));
    pats.extend(extra.iter().map(|k| kw_regex(k, false)));
    pats.extend(raw.iter().map(|s| s.to_string())); // ready-made regular expressions
    Rx::lit_ci(&pats.join("|"))
}

// ------------------------------------------------------------------------------------------------ dates
fn unit_seconds(u: &str) -> Option<i64> {
    Some(match u {
        "min" | "minute" | "dakika" | "دقیقه" => 60,
        "hour" | "hr" | "saat" | "ساعت" => 3600,
        "day" | "gün" | "gun" | "روز" => 86400,
        "week" | "hafta" | "هفته" => 604800,
        "month" | "ay" | "ماه" => 2592000,
        _ => return None,
    })
}

static REL_RE: LazyLock<Rx> = LazyLock::new(|| Rx::lit_ci(crate::lexdata::REL_RE));

/// '17 hours ago' / '3 saat önce' / '۲ روز پیش' / 'just now' / 'yesterday' -> datetime.
fn parse_relative(s: &str) -> Option<DateTime<Utc>> {
    let t = normalize(s.trim());
    let t = t.trim();
    match t {
        "just now" | "now" | "az önce" => return Some(utcnow()),
        "yesterday" | "dün" => return Some(utcnow() - Duration::days(1)),
        _ => {}
    }
    let c = REL_RE.captures(t)?;
    let n_txt = c.get(1)?.as_str();
    let n: i64 = if n_txt.chars().all(|ch| ch.is_ascii_digit()) { n_txt.parse().ok()? } else { 1 };
    let unit_txt = c.get(2)?.as_str();
    let unit = unit_seconds(unit_txt.trim_end_matches('s')).or_else(|| unit_seconds(unit_txt))?;
    Some(utcnow() - Duration::seconds(n.checked_mul(unit)?))
}

pub fn parse_epoch(v: f64) -> Option<DateTime<Utc>> {
    let v = if v > 1e12 { v / 1000.0 } else { v }; // milliseconds
    if !v.is_finite() {
        return None;
    }
    let secs = v.floor();
    let nanos = ((v - secs) * 1e9) as u32;
    Utc.timestamp_opt(secs as i64, nanos).single()
}

fn naive_utc(n: NaiveDateTime) -> DateTime<Utc> {
    Utc.from_utc_datetime(&n)
}

/// Parse many date formats -> UTC datetime (or None).
pub fn parse_dt_str(value: &str) -> Option<DateTime<Utc>> {
    let s = value.trim();
    if s.is_empty() {
        return None;
    }
    if s.chars().all(|c| c.is_ascii_digit()) {
        return s.parse::<f64>().ok().and_then(parse_epoch);
    }
    if let Some(rel) = parse_relative(s) {
        return Some(rel);
    }
    if let Ok(d) = DateTime::parse_from_rfc2822(s) {
        return Some(d.with_timezone(&Utc)); // RSS
    }
    let iso = s.replace('Z', "+00:00");
    for f in ["%Y-%m-%dT%H:%M:%S%.f%:z", "%Y-%m-%dT%H:%M:%S%.f%z", "%Y-%m-%d %H:%M:%S%.f%:z", "%Y-%m-%d %H:%M:%S%.f%z", "%Y-%m-%dT%H:%M%:z"] {
        if let Ok(d) = DateTime::parse_from_str(&iso, f) {
            return Some(d.with_timezone(&Utc));
        }
    }
    for f in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%dT%H:%M", "%Y-%m-%d %H:%M"] {
        if let Ok(n) = NaiveDateTime::parse_from_str(s, f) {
            return Some(naive_utc(n));
        }
    }
    for f in ["%Y-%m-%d", "%d/%m/%Y", "%d.%m.%Y", "%B %d, %Y", "%b %d, %Y", "%d %B %Y", "%d %b %Y", "%d-%b-%Y"] {
        if let Ok(d) = NaiveDate::parse_from_str(s, f) {
            return d.and_hms_opt(0, 0, 0).map(naive_utc);
        }
    }
    None
}

pub fn parse_dt(v: &Value) -> Option<DateTime<Utc>> {
    match v {
        Value::Null => None,
        Value::Number(n) => n.as_f64().and_then(parse_epoch),
        Value::String(s) => parse_dt_str(s),
        _ => None,
    }
}

// ------------------------------------------------------------------------------------------------ HTML -> text
const BLOCK_TAGS: [&str; 8] = ["p", "li", "div", "h1", "h2", "h3", "h4", "tr"];

fn walk_text(el: ElementRef, out: &mut String, br_as_newline: bool, block_newlines: bool) {
    for c in el.children() {
        match c.value() {
            Node::Text(t) => out.push_str(t),
            Node::Element(e) => {
                let name = e.name();
                if matches!(name, "script" | "style" | "template") {
                    continue;
                }
                if name == "br" && br_as_newline {
                    out.push('\n');
                    continue;
                }
                if let Some(child) = ElementRef::wrap(c) {
                    walk_text(child, out, br_as_newline, block_newlines);
                    if block_newlines && BLOCK_TAGS.contains(&name) {
                        out.push('\n');
                    }
                }
            }
            _ => {}
        }
    }
}

static BLANKS: LazyLock<Rx> = LazyLock::new(|| Rx::lit(r"[ \t\u{00a0}]+"));
static BLANK_LINES: LazyLock<Rx> = LazyLock::new(|| Rx::lit(r"\n\s*\n+"));

fn tidy_text(raw: &str) -> String {
    let t = BLANKS.replace_all(raw, " ");
    BLANK_LINES.replace_all(&t, "\n\n").trim().to_string()
}

/// The text of an element as the program's "description": line breaks at <br> and block tags.
pub fn element_to_text(el: ElementRef) -> String {
    let mut out = String::new();
    walk_text(el, &mut out, true, true);
    tidy_text(&out)
}

pub fn html_to_text(raw: &str) -> String {
    if raw.is_empty() {
        return String::new();
    }
    let doc = Html::parse_fragment(raw);
    element_to_text(doc.root_element())
}

/// BeautifulSoup's `get_text(" ", strip=True)`: stripped text pieces joined with one space.
pub fn text_stripped(el: ElementRef) -> String {
    let mut parts: Vec<String> = Vec::new();
    collect_pieces(el, &mut parts);
    parts.join(" ")
}

fn collect_pieces(el: ElementRef, parts: &mut Vec<String>) {
    for c in el.children() {
        match c.value() {
            Node::Text(t) => {
                let s = t.trim();
                if !s.is_empty() {
                    parts.push(s.to_string());
                }
            }
            Node::Element(e) => {
                if matches!(e.name(), "script" | "style" | "template") {
                    continue;
                }
                if let Some(child) = ElementRef::wrap(c) {
                    collect_pieces(child, parts);
                }
            }
            _ => {}
        }
    }
}

/// BeautifulSoup's `get_text()` after every <br> was replaced by a newline (used for Telegram posts).
pub fn text_with_br(el: ElementRef) -> String {
    let mut out = String::new();
    walk_text(el, &mut out, true, false);
    out
}

// ------------------------------------------------------------------------------------------------ JSON helpers
/// Reads a dotted path like 'company.name' or 'data.jobs.0.title'.
pub fn dig<'a>(obj: &'a Value, path: &str) -> Option<&'a Value> {
    let mut cur = obj;
    for part in path.split('.') {
        cur = match cur {
            Value::Array(a) => a.get(part.parse::<usize>().ok()?)?,
            Value::Object(m) => m.get(part)?,
            _ => return None,
        };
        if cur.is_null() {
            return None;
        }
    }
    Some(cur)
}

/// Python-style `str(x)` for a JSON value used as text.
pub fn json_str(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Bool(b)) => (if *b { "True" } else { "False" }).to_string(),
        Some(other) => other.to_string(),
    }
}

/// `cfg[key]` helpers over serde_json values.
pub fn jstr(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}
pub fn jbool(v: &Value, key: &str, default: bool) -> bool {
    match v.get(key) {
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().map(|x| x != 0.0).unwrap_or(default),
        Some(Value::Null) | None => default,
        Some(_) => default,
    }
}
pub fn jf64(v: &Value, key: &str, default: f64) -> f64 {
    match v.get(key) {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(default),
        Some(Value::String(s)) => s.trim().parse().unwrap_or(default),
        _ => default,
    }
}
pub fn ji64(v: &Value, key: &str, default: i64) -> i64 {
    match v.get(key) {
        Some(Value::Number(n)) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)).unwrap_or(default),
        Some(Value::String(s)) => s.trim().parse().unwrap_or(default),
        _ => default,
    }
}
pub fn jlist(v: &Value, key: &str) -> Vec<String> {
    v.get(key).and_then(Value::as_array).map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()).unwrap_or_default()
}
