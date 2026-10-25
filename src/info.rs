//! What the page shows for each job: a brief list of what the applicant needs, the submission guideline, and one
//! working link to the original posting. (Replaces the old "how to apply" box with e-mails, phones and handles.)
use crate::apply::extract_apply_lines;
use crate::http::Http;
use crate::model::Job;
use crate::rx::Rx;
use crate::util::trunc;
use std::sync::LazyLock;

const STOP: &str = r"responsibilit|\bduties\b|job\s+description|about\s+(?:us|the|our)\b|how\s+to\s+apply|submission|application\s+(?:process|procedure|instructions?|deadline|method)|closing\s+date|\bdeadline\b|\bsalary\b|\bbenefits?\b|contract\s+(?:type|duration|length)|\bbackground\b|terms\s+of|to\s+apply|apply\s+(?:by|now|at|via)|\bcontact\b|vacancy\s+(?:number|code)|duty\s+station|reports?\s+to|general\s+information|\bsummary\b|announcement|وظایف|مسئولیت|نحوه|طریقه|آخرین\s+مهلت|مهلت|حقوق|مزایا|درباره|görev|sorumluluk|başvuru|nasıl";

static REQ_HEAD: LazyLock<Rx> = LazyLock::new(|| {
    Rx::lit_ci(r"(?:^|\n|[\s>])(?:minimum\s+|key\s+|desired\s+|essential\s+|required\s+)?(?:job\s+|candidate\s+)?(?:requirements?|qualifications?(?:\s+and\s+(?:experience|skills))?|skills(?:\s+and\s+(?:experience|qualifications|competencies))?|education(?:\s+and\s+experience)?|eligibility(?:\s+criteria)?|who\s+can\s+apply|candidate\s+profile|person\s+specification|what\s+we\s+(?:are\s+)?looking\s+for|competencies|academic\s+qualifications?|experience\s+and\s+skills)\s*[:\-–—]|(?:^|\n)\s*(?:minimum\s+)?(?:requirements?|qualifications?)\s*\n|شرایط\s*(?:احراز|لازم|متقاضی|داوطلب|کاری)?\s*[:\-–]|مهارت\s*های?\s*(?:لازم|مورد\s*نیاز)\s*[:\-–]?|تحصیلات\s*[:\-–]|ویژگی\s*های?\s*(?:مورد\s*نیاز|لازم)\s*[:\-–]?|aranan\s+nitelikler\s*[:\-–]?|nitelikler\s*[:\-–]|gereksinimler\s*[:\-–]?|başvuru\s+şartları\s*[:\-–]?")
});
static SUB_HEAD: LazyLock<Rx> = LazyLock::new(|| {
    Rx::lit_ci(r"submission\s+(?:guidelines?|instructions?|process|procedure|method|of\s+applications?)|how\s+to\s+apply|application\s+(?:procedure|process|instructions?|method|submission)|(?:^|\n)\s*to\s+apply\b|interested\s+(?:candidates|applicants|persons|individuals)|apply\s+(?:by|via|through|online|at)\b|send\s+(?:your|the)\s+(?:cv|resume|application)|submit\s+(?:your|the)\s+(?:cv|resume|application)|cv\s+submission|نحوه\s*(?:ارسال|درخواست|اپلای)|طریقه\s*(?:ارسال|درخواست)|شیوه\s*(?:ارسال|درخواست)|روش\s*(?:ارسال|درخواست)|آدرس\s*ارسال|علاقه\s*مندان|متقاضیان\s*(?:می\s*توانند|محترم)|ارسال\s*(?:رزومه|سی\s*وی)|شرایط\s*ارسال|nasıl\s+başvurulur|başvuru\s+(?:şekli|yöntemi|için|adresi)|başvurmak\s+isteyen")
});
static STOP_RX: LazyLock<Rx> = LazyLock::new(|| Rx::lit_ci(STOP));
static SUB_STOP: LazyLock<Rx> = LazyLock::new(|| Rx::lit_ci(r"about\s+(?:us|the|our)\b|closing\s+date|related\s+jobs|similar\s+jobs|share\s+this|a\s+ray\s+of\s+hope|all\s+rights\s+reserved|copyright"));
static REQ_CUE: LazyLock<Rx> = LazyLock::new(|| {
    Rx::lit_ci(r"\b(?:degree|bachelor|master|diploma|certificate|graduate|graduation|years?\s+of|experience|fluen\w*|proficien\w*|knowledge|skills?|ability|able\s+to|must|should|required|requirement|qualified|competen\w*|training|computer|ms\s*office|english|dari|pashto|age)\b|لیسانس|فوق\s*لیسانس|دیپلم|تجربه|سابقه|مهارت|تسلط|زبان|دانش|توانایی|مدرک|شرایط|\blisans|mezun|deneyim|tecrübe|bilgi|şart|aranan|yetenek")
});
static BULLET: LazyLock<Rx> = LazyLock::new(|| Rx::lit(r"^[\s•·▪●■◦○\-–—*•\d.)(]+"));
static SENT: LazyLock<Rx> = LazyLock::new(|| Rx::lit(r"[\n•·▪●■]+|[.;!?؟]+\s+"));

fn squeeze(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Cuts the text to at most `max` characters, ending at a sentence or list-item end when possible.
fn brief(s: &str, max: usize) -> String {
    let t = squeeze(s);
    if t.chars().count() <= max {
        return t;
    }
    let cut: String = t.chars().take(max).collect();
    if let Some(i) = cut.rfind(|c| matches!(c, '.' | ';' | '!' | '?' | '؟')) {
        if i > max / 2 {
            return cut[..=i].to_string();
        }
    }
    format!("{}…", cut.trim_end())
}

/// The text after a heading (found by `head`) up to the next heading (`stop`) or `max_raw` characters.
fn section(text: &str, head: &Rx, stop: &Rx, max_raw: usize) -> Option<String> {
    for m in head.find_iter(text) {
        let rest = &text[m.end()..];
        let limit = crate::util::char_slice(rest, 0, max_raw);
        let end = stop.find(limit).map(|s| s.start()).unwrap_or(limit.len());
        let body = limit[..end].trim();
        if body.chars().count() >= 15 {
            return Some(body.to_string());
        }
    }
    None
}

fn requirements(text: &str) -> String {
    if let Some(sec) = section(text, &REQ_HEAD, &STOP_RX, 900) {
        let items = split_items(&sec);
        let joined = items.join(" • ");
        if !joined.is_empty() {
            return brief(&joined, 450);
        }
    }
    // no heading: pick the sentences that state what the applicant needs
    let picked: Vec<String> = split_items(text).into_iter().filter(|s| REQ_CUE.is_match(s) && s.chars().count() >= 20 && s.chars().count() <= 260).take(4).collect();
    brief(&picked.join(" • "), 450)
}

fn split_items(sec: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut last = 0;
    for m in SENT.find_iter(sec) {
        out.push(sec[last..m.start()].to_string());
        last = m.end();
    }
    out.push(sec[last..].to_string());
    out.into_iter().map(|s| squeeze(&BULLET.replace_all(&s, ""))).filter(|s| s.chars().count() >= 3).collect()
}

fn submission(text: &str) -> String {
    if let Some(sec) = section(text, &SUB_HEAD, &SUB_STOP, 700) {
        return brief(&sec, 500);
    }
    let l = extract_apply_lines(text, false, 4);
    let l = if l.is_empty() { extract_apply_lines(text, true, 3) } else { l };
    brief(&l.replace('\n', " "), 500)
}

/// Fills requirements / submission (and keeps `apply_text` = submission for translation and de-duplication).
pub fn build_apply_info(job: &mut Job) {
    let text = if job.full_text.chars().count() > job.description.chars().count() { job.full_text.clone() } else { job.description.clone() };
    job.requirements = requirements(&text);
    job.submission = submission(&text);
    job.apply_text = job.submission.clone();
}

// ------------------------------------------------------------------------------------------------ the link
fn usable(status: &str) -> bool {
    status.starts_with("verified") || status.starts_with("reachable")
}

/// Status of one address: "verified" | "reachable (site blocks bots: 403)" | "dead" | "unverified (...)".
fn check_url(http: &Http, url: &str) -> (String, String) {
    let cache = http.cache.as_ref();
    if let Some((status, fin)) = cache.and_then(|c| c.get_probe(url)) {
        return (status, if fin.is_empty() { url.to_string() } else { fin });
    }
    let (code, fin, err) = http.probe(url);
    let fin = fin.unwrap_or_else(|| url.to_string());
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
    if let Some(c) = cache {
        c.put_probe(url, &status, &fin);
    }
    (status, fin)
}

/// Picks the link to show: the original posting, else the post's own apply button, else a copy of the posting on
/// another source - the first one that really opens. Sets `apply_link` and `link_status`.
pub fn resolve_apply_link(http: &Http, job: &mut Job, verify: bool) {
    let mut cands: Vec<String> = Vec::new();
    let http_url = |u: &str| u.starts_with("http://") || u.starts_with("https://");
    for u in std::iter::once(job.source_url.clone()).chain((job.apply_kind == "direct").then(|| job.apply_url.clone())).chain(job.duplicates.iter().map(|d| d.url.clone())) {
        if http_url(&u) && !cands.contains(&u) {
            cands.push(u);
        }
    }
    let Some(first) = cands.first().cloned() else {
        job.apply_link.clear();
        job.link_status = "no link".into();
        return;
    };
    if !verify {
        job.apply_link = first;
        job.link_status = "not checked".into();
        return;
    }
    if job.page_fetched && !job.from_cache && cands[0] == job.source_url {
        // we have just opened this very page ourselves
        job.apply_link = first.clone();
        job.link_status = "verified".into();
        if let Some(c) = http.cache.as_ref() {
            c.put_probe(&first, "verified", &first);
        }
        return;
    }
    let mut last = String::new();
    for u in &cands {
        let (status, fin) = check_url(http, u);
        if usable(&status) {
            job.apply_link = if fin.starts_with("http") { fin } else { u.clone() };
            job.link_status = status;
            return;
        }
        last = status;
    }
    job.apply_link = first;
    job.link_status = last; // none opened: shown with a warning
}
