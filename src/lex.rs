//! The keyword lexicons, "years of experience" extraction and language detection.
use crate::lexdata as d;
use crate::rx::{escape, Rx};
use crate::text::{compile_keywords, normalize};
use serde_json::Value;
use std::collections::HashSet;
use std::sync::LazyLock;
use unicode_normalization::UnicodeNormalization;

/// `text[max(0, start - before) : end + after]` measured in characters (Python slicing), as a byte slice.
pub fn window(t: &str, start: usize, end: usize, before: usize, after: usize) -> &str {
    let s = t[..start].char_indices().rev().take(before).last().map(|(i, _)| i).unwrap_or(start);
    let e = t[end..].char_indices().take(after).last().map(|(i, c)| end + i + c.len_utf8()).unwrap_or(end);
    &t[s..e]
}

pub struct Lexicon {
    pub none: Rx,
    pub entry: Rx,
    pub intern: Rx,
    pub junior: Rx,
    pub exclude_title: Rx,
    pub intent: Rx,
    pub lang_patterns: Vec<(&'static str, Rx)>,
}

fn kw_prefix(k: &str) -> String {
    format!(r"(?<!\w){}", normalize(k).split_whitespace().map(escape).collect::<Vec<_>>().join(r"[\s\-_]*"))
}

impl Lexicon {
    pub fn new(filters: &Value) -> Lexicon {
        let list = |key: &str| -> Vec<String> {
            filters.get(key).and_then(Value::as_array).map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()).unwrap_or_default()
        };
        let extra = list("extra_include_keywords");
        let none_raw: Vec<&str> = d::NONE_RAW.to_vec();
        let ex_extra: Vec<String> = list("extra_exclude_title_keywords").iter().map(|k| format!(r"\b{}\b", escape(&normalize(k)))).collect();
        let mut pats: Vec<String> = d::EXCL_TITLE_EN.iter().map(|s| s.to_string()).collect();
        pats.extend(d::EXCL_TITLE_FA.iter().chain(d::EXCL_TITLE_TR.iter()).map(|k| kw_prefix(k)));
        pats.extend(ex_extra);
        let mut lang_patterns = Vec::new();
        for (code, names) in d::LANG_NAMES {
            let parts: Vec<String> = names
                .iter()
                .map(|n| {
                    let prefix = n.chars().any(|c| ('\u{0600}'..='\u{06FF}').contains(&c) || "çğşöü".contains(c)) || *code == "tr";
                    let body = normalize(n).split_whitespace().map(escape).collect::<Vec<_>>().join(r"[\s\-_]*");
                    if prefix {
                        format!(r"(?<!\w){}", body)
                    } else {
                        format!(r"\b{}\b", body)
                    }
                })
                .collect();
            lang_patterns.push((*code, Rx::lit_ci(&parts.join("|"))));
        }
        // NONE_RAW is normalised with the same letter-folding as the text it is matched against
        let none_norm: Vec<String> = none_raw.iter().map(|p| normalize(p)).collect();
        let none_refs: Vec<&str> = none_norm.iter().map(String::as_str).collect();
        Lexicon {
            none: compile_keywords(&[], &[], &[], &[], &none_refs),
            entry: compile_keywords(d::ENTRY_EN, d::ENTRY_FA, d::ENTRY_TR, &extra, &[]),
            intern: compile_keywords(d::INTERN_EN, d::INTERN_FA, d::INTERN_TR, &[], &[]),
            junior: compile_keywords(d::JUNIOR_EN, d::JUNIOR_FA, d::JUNIOR_TR, &[], &[]),
            exclude_title: Rx::lit_ci(&pats.join("|")),
            intent: compile_keywords(d::JOB_INTENT_EN, d::JOB_INTENT_FA, d::JOB_INTENT_TR, &[], &[]),
            lang_patterns,
        }
    }
}

// ------------------------------------------------------------------------------------------------ experience in years
static EXP_RANGE: LazyLock<Rx> = LazyLock::new(|| Rx::lit_ci(d::EXP_RANGE));
static EXP_SINGLE: LazyLock<Rx> = LazyLock::new(|| Rx::lit_ci(d::EXP_SINGLE));
static EXP_CUE: LazyLock<Rx> = LazyLock::new(|| Rx::lit_ci(d::EXP_CUE));
static EXP_OPTIONAL: LazyLock<Rx> = LazyLock::new(|| Rx::lit_ci(d::EXP_OPTIONAL));

fn to_int(s: &str) -> i64 {
    if let Ok(n) = s.parse::<i64>() {
        return n;
    }
    match s.to_lowercase().as_str() {
        "zero" => 0,
        "one" => 1,
        "two" => 2,
        "three" => 3,
        "four" => 4,
        "five" => 5,
        "six" => 6,
        "seven" => 7,
        "eight" => 8,
        "nine" => 9,
        "ten" => 10,
        _ => 0,
    }
}

/// (min_years, max_years_or_None) found next to 'experience' words.
pub fn extract_experience(text_norm: &str) -> Vec<(i64, Option<i64>)> {
    let mut found: Vec<(usize, usize, i64, Option<i64>)> = Vec::new();
    let mut spans: Vec<(usize, usize)> = Vec::new();
    for c in EXP_RANGE.captures_all(text_norm) {
        let m = c.get(0).unwrap();
        spans.push((m.start(), m.end()));
        let g = |i: usize| to_int(c.get(i).map(|m| m.as_str()).unwrap_or("0"));
        found.push((m.start(), m.end(), g(1), Some(g(2))));
    }
    for c in EXP_SINGLE.captures_all(text_norm) {
        let m = c.get(0).unwrap();
        if spans.iter().any(|(s, e)| *s <= m.start() && m.start() < *e) {
            continue;
        }
        found.push((m.start(), m.end(), to_int(c.get(1).map(|m| m.as_str()).unwrap_or("0")), None));
    }
    let mut out = Vec::new();
    for (s, e, lo, hi) in found {
        if !EXP_CUE.is_match(window(text_norm, s, e, 60, 80)) {
            continue;
        }
        if EXP_OPTIONAL.is_match(window(text_norm, s, e, 40, 60)) {
            continue;
        }
        out.push((lo, hi));
    }
    out
}

// ------------------------------------------------------------------------------------------------ language detection
static WORDS: LazyLock<Rx> = LazyLock::new(|| Rx::lit(r"[^\W\d_]+"));
static ARABIC_SCRIPT: LazyLock<Rx> = LazyLock::new(|| Rx::lit(d::ARABIC_SCRIPT));
static AR_STOP: LazyLock<HashSet<&str>> = LazyLock::new(|| d::AR_STOP.iter().copied().collect());
static FA_STOP: LazyLock<HashSet<&str>> = LazyLock::new(|| d::FA_STOP.iter().copied().collect());
static STOP: LazyLock<Vec<(&str, HashSet<&str>)>> = LazyLock::new(|| {
    // same order as the original dictionary: en, tr, de, fr, es
    ["en", "tr", "de", "fr", "es"]
        .iter()
        .map(|code| (*code, d::STOP.iter().find(|(k, _)| k == code).map(|(_, v)| v.iter().copied().collect()).unwrap_or_default()))
        .collect()
});

/// Heuristic: en | fa | tr | ar | ps | de | fr | es | ur | ku | unknown. (Dari and Farsi share script and stop-words: 'fa'.)
pub fn detect_language(text: &str) -> String {
    let t: String = text.nfkc().collect::<String>().to_lowercase().replace("i\u{0307}", "i");
    let words: Vec<String> = WORDS.find_all_str(&t);
    let n_letters: usize = words.iter().map(|w| w.chars().count()).sum();
    if n_letters < 20 {
        return "unknown".into();
    }
    let arabic = ARABIC_SCRIPT.find_iter(&t).len();
    if arabic as f64 / n_letters.max(1) as f64 >= 0.25 {
        let has = |set: &str| t.chars().any(|c| set.contains(c));
        if has(d::PS_ONLY_CHARS) {
            return "ps".into();
        }
        if has(d::UR_ONLY_CHARS) {
            return "ur".into();
        }
        if has(d::KU_ONLY_CHARS) {
            return "ku".into();
        }
        let ar_score = words.iter().filter(|w| AR_STOP.contains(w.as_str())).count()
            + words.iter().filter(|w| w.chars().count() > 3 && w.starts_with("ال")).count()
            + usize::from(t.contains('ة'));
        let fa_score = words.iter().filter(|w| FA_STOP.contains(w.as_str())).count() + if has(d::FA_ONLY_CHARS) { 3 } else { 0 };
        // Persian/Dari is the default for Arabic-script text: only call it Arabic with real evidence
        return if ar_score >= 2 && ar_score > fa_score { "ar" } else { "fa" }.into();
    }
    let mut scores: Vec<(&str, usize)> = STOP.iter().map(|(code, set)| (*code, words.iter().filter(|w| set.contains(w.as_str())).count())).collect();
    let tr_extra: usize = t.chars().filter(|c| matches!(c, 'ğ' | 'ş' | 'ı')).count();
    for s in scores.iter_mut() {
        if s.0 == "tr" {
            s.1 += 2 * tr_extra;
        }
    }
    scores.sort_by(|a, b| b.1.cmp(&a.1)); // stable: ties keep the dictionary order
    let (best, second) = (scores[0], scores[1]);
    if best.1 >= 2 && best.1 as f64 >= 1.5 * second.1.max(1) as f64 {
        return best.0.to_string();
    }
    "unknown".into()
}

pub fn lang_label(code: &str) -> &str {
    match code {
        "en" => "English",
        "fa" => "Persian/Dari",
        "tr" => "Turkish",
        "ar" => "Arabic",
        "ps" => "Pashto",
        "de" => "German",
        "fr" => "French",
        "es" => "Spanish",
        "ur" => "Urdu",
        "ku" => "Kurdish",
        "unknown" => "Unknown",
        other => other,
    }
}
