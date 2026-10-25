//! The filter pipeline: language, entry-level, location, remote, de-duplication and link checks.
use crate::difflib;
use crate::http::{netloc, Http};
use crate::lex::{detect_language, extract_experience, lang_label, window, Lexicon};
use crate::lexdata as d;
use crate::model::{ApplyMethod, Dup, Job};
use crate::rx::Rx;
use crate::text::{jbool, jf64, ji64, jlist, normalize};
use crate::util::trunc;
use serde_json::Value;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::LazyLock;

static LANG_REQ_CUE: LazyLock<Rx> = LazyLock::new(|| Rx::lit_ci(d::LANG_REQ_CUE));
static LANG_OPT_CUE: LazyLock<Rx> = LazyLock::new(|| Rx::lit_ci(d::LANG_OPT_CUE));
static REMOTE_YES: LazyLock<Rx> = LazyLock::new(|| Rx::lit_ci(d::REMOTE_YES));
static REMOTE_NO: LazyLock<Rx> = LazyLock::new(|| Rx::lit_ci(d::REMOTE_NO));
static WS: LazyLock<Rx> = LazyLock::new(|| Rx::lit(r"\s+"));
static TOKENS: LazyLock<Rx> = LazyLock::new(|| Rx::lit(r"[^\W_]+"));
static SEP_RUN: LazyLock<Rx> = LazyLock::new(|| Rx::lit(r"[\s\-_]+"));

pub fn check_language(job: &mut Job, lex: &Lexicon, f: &Value) -> (bool, String) {
    let acceptable: HashSet<String> = match f.get("acceptable_languages") {
        Some(Value::Array(_)) => jlist(f, "acceptable_languages").into_iter().collect(),
        _ => ["en", "fa", "tr"].iter().map(|s| s.to_string()).collect(),
    };
    let text = format!("{}\n{}", job.title, job.description);
    job.language = detect_language(&text);
    let norm = normalize(&text);
    let mut required: BTreeSet<String> = BTreeSet::new();
    let mut mentioned: BTreeSet<String> = BTreeSet::new();
    for (code, pat) in &lex.lang_patterns {
        for m in pat.find_iter(&norm) {
            mentioned.insert(code.to_string());
            let w = window(&norm, m.start(), m.end(), 60, 60);
            if LANG_REQ_CUE.is_match(w) && !LANG_OPT_CUE.is_match(w) {
                required.insert(code.to_string());
            }
        }
    }
    job.languages_mentioned = mentioned.into_iter().collect();
    if !acceptable.contains(&job.language) && job.language != "unknown" {
        return (false, format!("posting language is {}", lang_label(&job.language)));
    }
    let other: Vec<&String> = required.iter().filter(|c| !acceptable.contains(*c)).collect();
    if !other.is_empty() && !required.iter().any(|c| acceptable.contains(c)) {
        let names: Vec<String> = other.iter().map(|c| lang_label(c).to_string()).collect();
        return (false, format!("requires {} (no English/Persian/Turkish option)", names.join("/")));
    }
    (true, String::new())
}

fn first_match(pat: &Rx, texts: &[String], scope: Option<usize>) -> Option<String> {
    for t in texts {
        let hay: String = match scope {
            Some(n) => trunc(t, n),
            None => t.clone(),
        };
        if let Some(m) = pat.find(&hay) {
            return Some(WS.replace_all(m.as_str(), " ").trim().to_string());
        }
    }
    None
}

/// (ok, reason). Fills job.years_required.
pub fn evaluate_entry_level(job: &mut Job, lex: &Lexicon, f: &Value) -> (bool, String) {
    let title_n = normalize(&job.title);
    let mut titles = vec![title_n.clone()];
    if !job.title_en.is_empty() {
        titles.push(normalize(&job.title_en));
    }
    for t in &titles {
        if let Some(m) = lex.exclude_title.find(t) {
            return (false, format!("senior/lead/manager title ({})", m.as_str().trim()));
        }
    }
    let mut texts = vec![normalize(&format!("{}\n{}", job.title, job.description))];
    if !job.title_en.is_empty() || !job.summary_en.is_empty() {
        texts.push(normalize(&format!("{}\n{}", job.title_en, job.summary_en)));
    }
    let exps: Vec<(i64, Option<i64>)> = texts.iter().flat_map(|t| extract_experience(t)).collect();
    let years = exps.iter().map(|(lo, _)| *lo).max();
    job.years_required = years;
    let zero_range = exps.iter().any(|(lo, hi)| *lo == 0 && hi.map_or(true, |h| h <= 1));
    let head = title_n.chars().count() + 400;
    let (mut reason, mut explicit) = (String::new(), false);
    if let Some(m) = first_match(&lex.none, &texts, None) {
        reason = format!("No-experience wording: “{}”", m);
        explicit = true;
    } else if let Some(m) = first_match(&lex.entry, &texts, None) {
        reason = format!("Entry-level wording: “{}”", m);
    } else if zero_range {
        reason = "Experience stated as 0 (or 0–1) years".into();
    } else if job.hints.iter().any(|h| h.starts_with("no-experience")) {
        reason = "Structured data says no experience required".into();
    } else if jbool(f, "accept_internships", false) && first_match(&lex.intern, &texts, Some(head)).is_some() {
        let m = first_match(&lex.intern, &texts, Some(head)).unwrap_or_default();
        reason = format!("Internship/trainee role (“{}”)", m);
    } else if jbool(f, "accept_junior_titles", false) {
        let m = first_match(&lex.junior, &texts, Some(head));
        if m.is_some() || job.hints.iter().any(|h| h.to_lowercase().starts_with("level:")) {
            reason = format!("Junior role (“{}”) - check experience wording", m.unwrap_or_else(|| "level hint".into()));
        }
    }
    if reason.is_empty() {
        return (false, "no entry-level / no-experience signal".into());
    }
    if let Some(y) = years {
        if y > ji64(f, "max_min_years_required", 0) && !explicit {
            return (false, format!("asks for {}+ years of experience", y));
        }
    }
    (true, reason)
}

/// Companies the user never wants to see (filters.exclude_companies). Whole-word match on the normalised name,
/// so "acme" hits "ACME Inc." and "Acme-Corp" but not "Acmeville".
pub struct CompanyExcluder(Vec<String>);

fn company_key(s: &str) -> String {
    let n: String = normalize(s).chars().map(|c| if c.is_alphanumeric() { c } else { ' ' }).collect();
    format!(" {} ", n.split_whitespace().collect::<Vec<_>>().join(" "))
}

pub fn company_excluder(f: &Value) -> Option<CompanyExcluder> {
    let keys: Vec<String> = jlist(f, "exclude_companies").iter().map(|k| company_key(k)).filter(|k| k.trim().chars().count() >= 2).collect();
    if keys.is_empty() { None } else { Some(CompanyExcluder(keys)) }
}

impl CompanyExcluder {
    pub fn hit(&self, j: &Job) -> Option<String> {
        if j.company.trim().is_empty() {
            return None;
        }
        let c = company_key(&j.company);
        self.0.iter().find(|k| c.contains(k.as_str())).map(|k| k.trim().to_string())
    }
}

pub fn location_ok(job: &Job, f: &Value) -> bool {
    let keys: Vec<String> = jlist(f, "location_keywords").iter().map(|k| normalize(k)).collect();
    if keys.is_empty() {
        return true;
    }
    let hay = normalize(&format!("{} {}", job.location, job.title));
    if jbool(f, "include_remote_even_if_location_set", true) && (job.remote == Some(true) || hay.contains("remote")) {
        return true;
    }
    keys.iter().any(|k| hay.contains(k.as_str()))
}

/// True for work-from-home jobs (English / Persian / Turkish wording); negations such as "not remote" are ignored.
pub fn detect_remote(job: &Job) -> bool {
    if let Some(r) = job.remote {
        return r;
    }
    let text = REMOTE_NO.replace_all(&normalize(&format!("{} {} {}", job.title, job.location, trunc(&job.description, 2500))), " ");
    REMOTE_YES.is_match(&text)
}

pub fn canonical_url(u: &str) -> String {
    let Ok(p) = url::Url::parse(u) else { return u.to_string() };
    let q: Vec<(String, String)> = p
        .query_pairs()
        .filter(|(k, v)| !v.is_empty() && !d::TRACKING_PARAMS.iter().any(|t| k.to_lowercase().starts_with(t)))
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    let nl = netloc(u).to_lowercase();
    let host = nl.strip_prefix("www.").unwrap_or(&nl);
    let mut out = format!("{}://{}{}", p.scheme().to_lowercase(), host, p.path().trim_end_matches('/'));
    if !q.is_empty() {
        let qs = url::form_urlencoded::Serializer::new(String::new()).extend_pairs(q.iter().map(|(k, v)| (k.as_str(), v.as_str()))).finish();
        out.push('?');
        out.push_str(&qs);
    }
    out
}

// ------------------------------------------------------------------------------------------------ de-duplication
static TITLE_NOISE: LazyLock<HashSet<&'static str>> = LazyLock::new(|| d::DUP_TITLE_NOISE.iter().copied().collect());
static COMPANY_NOISE: LazyLock<HashSet<&'static str>> = LazyLock::new(|| d::DUP_COMPANY_NOISE.iter().copied().collect());
static PLACE_LOOKUP: LazyLock<HashMap<String, &'static str>> = LazyLock::new(|| {
    let mut m = HashMap::new();
    for (canon, vs) in d::PLACES {
        for v in *vs {
            m.insert(normalize(v).replace('-', " "), *canon);
        }
    }
    m
});
static PLACE_RE: LazyLock<Rx> = LazyLock::new(|| {
    let all: Vec<&str> = d::PLACES.iter().flat_map(|(_, vs)| vs.iter().copied()).collect();
    let ascii: Vec<&str> = all.iter().copied().filter(|v| v.is_ascii()).collect();
    let other: Vec<&str> = all.iter().copied().filter(|v| !v.is_ascii()).collect();
    crate::text::compile_keywords(&ascii, &other, &[], &[], &[])
});

fn dup_tokens(text: &str, noise: Option<&HashSet<&str>>, min_len: usize) -> Vec<String> {
    TOKENS
        .find_all_str(&normalize(text))
        .into_iter()
        .filter(|t| t.chars().count() >= min_len && noise.map_or(true, |n| !n.contains(t.as_str())))
        .collect()
}

fn dup_places(text: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    for m in PLACE_RE.find_iter(&normalize(text)) {
        let key = SEP_RUN.replace_all(m.as_str(), " ").trim().to_string();
        if let Some(c) = PLACE_LOOKUP.get(&key) {
            out.insert(c.to_string());
        }
    }
    out
}

struct DupFeat {
    ts: f64,
    source: String,
    titles: Vec<(HashSet<String>, String)>,
    title_tokens: HashSet<String>,
    comp: HashSet<String>,
    comp_s: String,
    head: HashSet<String>,
    body: HashSet<String>,
    places: HashSet<String>,
    ext: HashSet<String>,
    own: String,
}

fn strip_www(h: &str) -> &str {
    h.strip_prefix("www.").unwrap_or(h)
}

impl DupFeat {
    fn new(j: &Job) -> DupFeat {
        let ts = j.posted_at.map(|t| t.timestamp() as f64 + t.timestamp_subsec_nanos() as f64 / 1e9).unwrap_or(0.0);
        let mut titles = Vec::new();
        for t in [&j.title, &j.title_en] {
            let toks = dup_tokens(t, Some(&TITLE_NOISE), 2);
            if !toks.is_empty() {
                titles.push((toks.iter().cloned().collect::<HashSet<_>>(), toks.join(" ")));
            }
        }
        let comp = dup_tokens(&j.company.replace('…', " "), Some(&COMPANY_NOISE), 2);
        let comp_s = comp.join(" ");
        let desc = &j.description;
        let title_all: HashSet<String> = titles.iter().flat_map(|(s, _)| s.iter().cloned()).collect();
        let mut head: HashSet<String> = title_all.clone();
        head.extend(dup_tokens(&trunc(desc, 400), None, 2));
        let body: HashSet<String> = dup_tokens(&trunc(desc, 1200), None, 3).into_iter().collect();
        let places = dup_places(&format!("{} {} {}", j.location, j.title, trunc(desc, 500)));
        let own_host = if j.source_url.is_empty() { String::new() } else { strip_www(&netloc(&j.source_url).to_lowercase()).to_string() };
        let own = if j.source_url.is_empty() { String::new() } else { canonical_url(&j.source_url) };
        let mut ext = HashSet::new();
        for m in &j.apply_methods {
            let v = &m.value;
            match m.kind.as_str() {
                "email" => {
                    ext.insert(format!("e:{}", v.to_lowercase()));
                }
                "phone" => {
                    ext.insert(format!("p:{}", v.chars().filter(|c| c.is_ascii_digit()).collect::<String>()));
                }
                "telegram" | "whatsapp" => {
                    ext.insert(format!("{}:{}", &m.kind[..1], v.to_lowercase()));
                }
                "link" if strip_www(&netloc(v).to_lowercase()) != own_host => {
                    ext.insert(format!("l:{}", canonical_url(v)));
                }
                _ => {}
            }
        }
        DupFeat { ts, source: j.source.clone(), titles, title_tokens: title_all, comp: comp.into_iter().collect(), comp_s, head, body, places, ext, own }
    }
}

fn jaccard(a: &HashSet<String>, b: &HashSet<String>) -> f64 {
    let inter = a.intersection(b).count();
    let union = a.len() + b.len() - inter;
    if union == 0 {
        0.0
    } else {
        inter as f64 / union as f64
    }
}

fn title_sim(a: &DupFeat, b: &DupFeat) -> f64 {
    let mut best = 0.0f64;
    for (ta, sa) in &a.titles {
        for (tb, sb) in &b.titles {
            let j = jaccard(ta, tb);
            if j < 0.3 {
                continue;
            }
            best = best.max(j).max(difflib::ratio(sa, sb));
        }
    }
    best
}

fn company_sim(a: &DupFeat, b: &DupFeat) -> Option<f64> {
    if a.comp_s.is_empty() || b.comp_s.is_empty() {
        return None;
    }
    if a.comp_s == b.comp_s {
        return Some(1.0);
    }
    let (short, long) = if a.comp_s.chars().count() <= b.comp_s.chars().count() { (&a.comp_s, &b.comp_s) } else { (&b.comp_s, &a.comp_s) };
    if short.chars().count() >= 6 && long.starts_with(short.as_str()) {
        return Some(0.95); // shortened name: "Afghan Family Guidance Asso…"
    }
    Some(jaccard(&a.comp, &b.comp))
}

fn company_in_head(has: &DupFeat, other: &DupFeat) -> bool {
    !has.comp.is_empty() && has.comp.is_subset(&other.head)
}

#[derive(Clone)]
pub struct DedupeCfg {
    pub enabled: bool,
    pub title_similarity: f64,
    pub text_similarity: f64,
    pub max_days_apart: f64,
}

impl DedupeCfg {
    pub fn from_value(v: Option<&Value>) -> DedupeCfg {
        let null = Value::Null;
        let v = v.unwrap_or(&null);
        DedupeCfg {
            enabled: jbool(v, "enabled", true),
            title_similarity: jf64(v, "title_similarity", 0.85),
            text_similarity: jf64(v, "text_similarity", 0.7),
            max_days_apart: jf64(v, "max_days_apart", 4.0),
        }
    }
}

/// The reason when a and b are the same opening, else None. Cautious: needs several agreeing signals.
fn same_opening(a: &DupFeat, b: &DupFeat, cfg: &DedupeCfg) -> Option<String> {
    if !a.own.is_empty() && a.own == b.own {
        return Some("same page address".into());
    }
    if (a.ts - b.ts).abs() > cfg.max_days_apart * 86400.0 {
        return None;
    }
    if !a.places.is_empty() && !b.places.is_empty() && a.places.is_disjoint(&b.places) {
        return None; // same title in another province is another vacancy
    }
    if a.source == b.source {
        if let Some(c) = company_sim(a, b) {
            if c < 0.5 {
                return None; // two different employers on one site are two different openings, whatever the wording
            }
        }
    }
    let tsim = title_sim(a, b);
    let body_sim = if a.body.len().min(b.body.len()) >= 25 { jaccard(&a.body, &b.body) } else { 0.0 };
    if (body_sim >= cfg.text_similarity && tsim >= 0.5) || (body_sim >= 0.95 && tsim >= 0.3) {
        return Some(format!("near-identical text ({:.2})", body_sim));
    }
    if a.source == b.source {
        return None; // one source listing two separate pages = two separate vacancies, unless the text is a copy
    }
    let csim = company_sim(a, b);
    let contact = !a.ext.is_disjoint(&b.ext);
    if tsim >= cfg.title_similarity {
        if let Some(c) = csim {
            if c >= 0.8 {
                return Some(format!("same title and company ({:.2})", tsim));
            }
        } else if contact || body_sim >= 0.3 || company_in_head(a, b) || company_in_head(b, a) {
            return Some(format!("same title; company/contact/text agree ({:.2})", tsim));
        }
    }
    if contact && tsim >= 0.5 {
        return Some(format!("same application contact and similar title ({:.2})", tsim));
    }
    // a short website title appearing inside a longer post (typical for Telegram) together with the company name
    for (small, big) in [(a, b), (b, a)] {
        for (ts_, _) in &small.titles {
            if ts_.len() >= 2 && ts_.is_subset(&big.head) && ((!small.comp.is_empty() && small.comp.is_subset(&big.head)) || (small.comp.is_empty() && ts_.len() >= 4)) {
                return Some("title (and company) appear inside the other posting".into());
            }
        }
    }
    None
}

fn dup_quality(j: &Job) -> f64 {
    let mut q = (if j.company.is_empty() { 0.0 } else { 3.0 })
        + (if j.location.is_empty() { 0.0 } else { 2.0 })
        + (j.description.chars().count() as f64 / 500.0).min(3.0)
        + (j.apply_methods.len() as f64).min(3.0);
    if j.source_type != "telegram" {
        q += 2.0;
    }
    if j.page_fetched {
        q += 1.0;
    }
    q
}

/// Every pair that COULD be the same opening (features sorted by time), in ascending order. An inverted index finds
/// them directly instead of testing every pair inside the time window.
fn candidate_pairs(feats: &[DupFeat], cfg: &DedupeCfg) -> Vec<(usize, usize)> {
    let n = feats.len();
    let window = cfg.max_days_apart * 86400.0;
    let ts: Vec<f64> = feats.iter().map(|f| f.ts).collect();
    let bisect_right_ts = |v: f64| ts.partition_point(|&x| x <= v);
    let mut out = Vec::new();
    if cfg.title_similarity <= 0.0 {
        // degenerate setting: every pair in the window may match, so test them all
        for i in 0..n {
            for k in (i + 1)..bisect_right_ts(ts[i] + window) {
                out.push((i, k));
            }
        }
        return out;
    }
    let mut head_post: HashMap<&str, Vec<usize>> = HashMap::new();
    let mut title_post: HashMap<&str, Vec<usize>> = HashMap::new();
    let mut own_groups: HashMap<&str, Vec<usize>> = HashMap::new();
    for (idx, f) in feats.iter().enumerate() {
        for t in &f.head {
            head_post.entry(t.as_str()).or_default().push(idx);
        }
        for t in &f.title_tokens {
            title_post.entry(t.as_str()).or_default().push(idx);
        }
        if !f.own.is_empty() {
            own_groups.entry(f.own.as_str()).or_default().push(idx);
        }
    }
    let later = |posting: &[usize], i: usize, hi: usize| -> Vec<usize> {
        let a = posting.partition_point(|&x| x <= i);
        let b = posting.partition_point(|&x| x < hi);
        if a < b { posting[a..b].to_vec() } else { Vec::new() }
    };
    for (i, f) in feats.iter().enumerate() {
        let hi = bisect_right_ts(ts[i] + window); // first index that is already too far away
        let mut cand: BTreeSet<usize> = BTreeSet::new();
        for t in &f.title_tokens {
            if let Some(p) = head_post.get(t.as_str()) {
                cand.extend(later(p, i, hi)); // my title word appears in the other job's head
            }
        }
        for t in &f.head {
            if let Some(p) = title_post.get(t.as_str()) {
                cand.extend(later(p, i, hi)); // the other job's title word appears in my head
            }
        }
        if !f.own.is_empty() {
            if let Some(g) = own_groups.get(f.own.as_str()) {
                let a = g.partition_point(|&x| x <= i);
                cand.extend(g[a..].iter().copied()); // the same page: merge whatever the dates say
            }
        }
        for k in cand {
            out.push((i, k));
        }
    }
    out
}

pub struct MergeLog {
    pub kept_title: String,
    pub kept_source: String,
    pub removed_title: String,
    pub removed_source: String,
    pub removed_url: String,
    pub reason: String,
}

/// Keeps ONE job per real opening. Returns (kept jobs newest first, log of merges).
pub fn dedupe(mut jobs: Vec<Job>, cfg: &DedupeCfg) -> (Vec<Job>, Vec<MergeLog>) {
    if !cfg.enabled || jobs.len() < 2 {
        return (jobs, Vec::new());
    }
    jobs.sort_by(|a, b| a.posted_at.cmp(&b.posted_at)); // stable, oldest first (features are sorted by time)
    let feats: Vec<DupFeat> = jobs.iter().map(DupFeat::new).collect();
    let mut parent: Vec<usize> = (0..feats.len()).collect();
    let mut why: Vec<(usize, usize, String)> = Vec::new();
    fn find(parent: &mut Vec<usize>, mut i: usize) -> usize {
        while parent[i] != i {
            parent[i] = parent[parent[i]];
            i = parent[i];
        }
        i
    }
    for (i, k) in candidate_pairs(&feats, cfg) {
        if let Some(reason) = same_opening(&feats[i], &feats[k], cfg) {
            let (ri, rk) = (find(&mut parent, i), find(&mut parent, k));
            if ri != rk {
                parent[rk] = ri;
                why.push((i, k, reason));
            }
        }
    }
    let mut order: Vec<usize> = Vec::new();
    let mut groups: HashMap<usize, Vec<usize>> = HashMap::new();
    for i in 0..feats.len() {
        let r = find(&mut parent, i);
        if !groups.contains_key(&r) {
            order.push(r);
        }
        groups.entry(r).or_default().push(i);
    }
    let mut kept: Vec<Job> = Vec::new();
    let mut log: Vec<MergeLog> = Vec::new();
    for root in order {
        let members = &groups[&root];
        if members.len() == 1 {
            kept.push(jobs[members[0]].clone());
            continue;
        }
        let mut ranked = members.clone();
        ranked.sort_by(|&x, &y| {
            dup_quality(&jobs[y]).partial_cmp(&dup_quality(&jobs[x])).unwrap_or(std::cmp::Ordering::Equal).then(feats[x].ts.partial_cmp(&feats[y].ts).unwrap_or(std::cmp::Ordering::Equal))
        });
        let mut rep = jobs[ranked[0]].clone();
        let mut have: HashSet<(String, String)> = rep.apply_methods.iter().map(|m| (m.kind.clone(), m.value.to_lowercase())).collect();
        for &i in &ranked[1..] {
            let o = &jobs[i];
            let reason = why.iter().find(|(x, y, _)| (*x == i || *y == i) && members.contains(x) && members.contains(y)).map(|(_, _, r)| r.clone()).unwrap_or_else(|| "duplicate".into());
            rep.duplicates.push(Dup { source: o.source.clone(), url: o.source_url.clone(), title: o.title.clone(), reason: reason.clone() });
            log.push(MergeLog {
                kept_title: rep.title.clone(),
                kept_source: rep.source.clone(),
                removed_title: o.title.clone(),
                removed_source: o.source.clone(),
                removed_url: o.source_url.clone(),
                reason,
            });
            for m in &o.apply_methods {
                // keep every way to apply that the other copies offered
                if have.insert((m.kind.clone(), m.value.to_lowercase())) {
                    let mut m2: ApplyMethod = m.clone();
                    m2.from = o.source.clone();
                    rep.apply_methods.push(m2);
                }
            }
            macro_rules! fill {
                ($($f:ident),*) => { $( if rep.$f.is_empty() && !o.$f.is_empty() { rep.$f = o.$f.clone(); } )* };
            }
            fill!(company, location, title_en, summary_en, apply_text, apply_text_en, job_type, full_text, requirements, submission);
        }
        kept.push(rep);
    }
    kept.sort_by(|a, b| b.posted_at.cmp(&a.posted_at));
    (kept, log)
}

// ------------------------------------------------------------------------------------------------ link check
/// Checks the main link of a job. Never removes the job - a broken link is only flagged.
/// A link that was checked within cache.probe_hours is not probed again.
pub fn verify_job_link(http: &Http, job: &mut Job) {
    let Some(pi) = job.apply_methods.iter().position(|m| m.kind == "link") else { return };
    let url = job.apply_methods[pi].value.clone();
    let cache = http.cache.as_ref();
    if job.page_fetched && !job.from_cache && url == job.source_url {
        // we just opened this page ourselves
        job.apply_methods[pi].status = "verified".into();
        job.link_status = "verified".into();
        job.final_url = url.clone();
        if let Some(c) = cache {
            c.put_probe(&url, "verified", &url);
        }
        return;
    }
    if let Some((status, fin)) = cache.and_then(|c| c.get_probe(&url)) {
        job.apply_methods[pi].status = status.clone();
        job.link_status = status;
        job.final_url = if fin.is_empty() { url } else { fin };
        return;
    }
    let (code, fin, err) = http.probe(&url);
    job.final_url = fin.unwrap_or_else(|| url.clone());
    let status = match code {
        None => match err {
            "ssl" => "unverified (SSL problem)".to_string(),
            "connection" => "unverified (could not connect)".to_string(),
            _ => "unverified (timeout)".to_string(),
        }, // a network problem on YOUR side must not look like a dead link
        Some(404) | Some(410) => "dead".into(),
        Some(c) if c < 400 => "verified".into(),
        Some(c @ (401 | 403 | 405 | 429 | 999)) => format!("reachable (site blocks bots: {})", c),
        Some(c) => format!("unverified (HTTP {})", c),
    };
    job.apply_methods[pi].status = status.clone();
    job.link_status = status.clone();
    if let Some(c) = cache {
        c.put_probe(&url, &status, &job.final_url);
    }
}

#[cfg(test)]
mod company_tests {
    use super::*;
    #[test]
    fn excluded_companies() {
        let f = serde_json::json!({"exclude_companies": ["Acme", "Foo Ltd."]});
        let ex = company_excluder(&f).unwrap();
        let mk = |c: &str| { let mut j = Job::new("s", "website", "t"); j.company = c.into(); j };
        assert!(ex.hit(&mk("ACME Inc.")).is_some());
        assert!(ex.hit(&mk("The Acme-Corp")).is_some());
        assert!(ex.hit(&mk("foo ltd")).is_some());
        assert!(ex.hit(&mk("Acmeville")).is_none());
        assert!(ex.hit(&mk("")).is_none());
        assert!(company_excluder(&serde_json::json!({"exclude_companies": []})).is_none());
    }
}
