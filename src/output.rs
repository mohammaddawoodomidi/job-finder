//! Results: CSV, the HTML report and the compact JSON the app shows.
use crate::assets::{REPORT_CSS, REPORT_JS};
use crate::lex::lang_label;
use crate::model::Job;
use crate::util::{round1, trunc};
use chrono::{DateTime, Local, Utc};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::Path;

const CSV_FIELDS: [&str; 20] = [
    "posted_at_utc", "age_hours", "title", "company", "location", "source", "source_type", "language", "requirements", "submission_guideline",
    "apply_link", "link_status", "also_found_on", "why_matched", "years_required", "remote", "job_type", "title_english", "summary_english", "description",
];

pub fn write_csv(jobs: &[Job], path: &Path, now: DateTime<Utc>) -> std::io::Result<()> {
    use std::io::Write;
    let mut file = std::fs::File::create(path)?;
    file.write_all(b"\xEF\xBB\xBF")?; // BOM => Excel shows Persian correctly
    let mut w = csv::WriterBuilder::new().terminator(csv::Terminator::CRLF).from_writer(file);
    w.write_record(CSV_FIELDS).map_err(std::io::Error::other)?;
    for j in jobs {
        let posted = j.posted_at.unwrap_or(now);
        let rec: Vec<String> = vec![
            posted.format("%Y-%m-%d %H:%M").to_string(),
            format!("{}", round1((now - posted).num_milliseconds() as f64 / 3_600_000.0)),
            j.title.clone(),
            j.company.clone(),
            j.location.clone(),
            j.source.clone(),
            j.source_type.clone(),
            lang_label(&j.language).to_string(),
            if j.requirements.is_empty() { "Not stated in the post".into() } else { j.requirements.clone() },
            if j.submission.is_empty() { "Not stated in the post - use the link".into() } else { j.submission.clone() },
            j.apply_link.clone(),
            j.link_status.clone(),
            j.duplicates.iter().map(|d| format!("{} <{}>", d.source, d.url)).collect::<Vec<_>>().join("; "),
            j.entry_reason.clone(),
            j.years_required.map(|y| y.to_string()).unwrap_or_default(),
            match j.remote { None => String::new(), Some(true) => "True".into(), Some(false) => "False".into() },
            j.job_type.clone(),
            j.title_en.clone(),
            j.summary_en.clone(),
            trunc(&j.description, 3000),
        ];
        w.write_record(&rec).map_err(std::io::Error::other)?;
    }
    w.flush()
}

pub fn rel_time(now: DateTime<Utc>, dt: DateTime<Utc>) -> String {
    let s = (now - dt).num_seconds();
    if s < 3600 {
        return format!("{} min ago", (s / 60).max(1));
    }
    if s < 86400 {
        return format!("{} h ago", s / 3600);
    }
    let d = s / 86400;
    format!("{} day{} ago", d, if d != 1 { "s" } else { "" })
}

/// Python's html.escape(quote=True).
pub fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&#x27;"),
            c => o.push(c),
        }
    }
    o
}

fn short(url: &str) -> String {
    let u = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://")).unwrap_or(url);
    let u = u.strip_prefix("www.").unwrap_or(u);
    if u.chars().count() <= 70 { u.to_string() } else { format!("{}…", trunc(u, 67)) }
}

fn render_how_to_apply(j: &Job) -> String {
    let mut out = String::new();
    let req = if j.requirements.is_empty() { "<p class=\"none\">Not stated in the post.</p>".to_string() } else { format!("<p dir=\"auto\">{}</p>", esc(&j.requirements)) };
    out.push_str(&format!("<div class=\"how\"><h4>What is required of the applicant</h4>{}", req));
    let sub = if j.submission.is_empty() { "<p class=\"none\">Not stated in the post - use the link below.</p>".to_string() } else { format!("<p dir=\"auto\">{}</p>", esc(&j.submission)) };
    out.push_str(&format!("<h4>Submission guideline</h4>{}", sub));
    if !j.apply_link.is_empty() {
        let st = &j.link_status;
        let flag = if st == "verified" || st.starts_with("reachable") {
            "<span class=\"flag ok\">link opens</span>"
        } else if st == "dead" {
            "<span class=\"flag bad\">this link returned an error (404/410) - the ad may have expired</span>"
        } else {
            "<span class=\"flag\">link could not be checked</span>"
        };
        out.push_str(&format!("<h4>Apply / original posting</h4><ul class=\"methods\"><li><a href=\"{}\" target=\"_blank\" rel=\"noopener noreferrer\">{}</a>{}</li></ul>", esc(&j.apply_link), esc(&short(&j.apply_link)), flag));
    }
    out.push_str("</div>");
    out
}

/// Python's `{x:g}`-style number text (7 -> "7", 2.5 -> "2.5").
pub fn fmt_g(x: f64) -> String {
    if x.fract() == 0.0 { format!("{}", x as i64) } else { format!("{}", x) }
}

pub fn build_html(jobs: &[Job], now: DateTime<Utc>, max_age_days: f64, funnel: &[(String, usize)], errors: &[String]) -> String {
    let days_txt = fmt_g(max_age_days);
    let mut sources: Vec<&str> = jobs.iter().map(|j| j.source.as_str()).collect();
    sources.sort();
    sources.dedup();
    let mut langs: Vec<&str> = jobs.iter().map(|j| j.language.as_str()).filter(|l| !l.is_empty()).collect();
    langs.sort();
    langs.dedup();
    let mut groups: BTreeMap<chrono::NaiveDate, Vec<&Job>> = BTreeMap::new();
    for j in jobs {
        if let Some(p) = j.posted_at {
            groups.entry(p.with_timezone(&Local).date_naive()).or_default().push(j);
        }
    }
    let today = now.with_timezone(&Local).date_naive();
    let mut parts: Vec<String> = Vec::new();
    for (d, items) in groups.iter().rev() {
        let delta = (today - *d).num_days();
        let label = if delta == 0 { "Today".to_string() } else if delta == 1 { "Yesterday".to_string() } else { d.format("%A, %d %B").to_string() };
        let mut rows = String::new();
        for j in items {
            let posted = j.posted_at.unwrap_or(now);
            let lt = posted.with_timezone(&Local);
            let rtl = j.language == "fa";
            let mut chips = vec![format!("<li>{}</li>", esc(&j.source)), format!("<li>{}</li>", esc(lang_label(&j.language)))];
            if j.remote == Some(true) {
                chips.push("<li>Remote</li>".into());
            }
            if !j.job_type.is_empty() {
                chips.push(format!("<li>{}</li>", esc(&j.job_type)));
            }
            if j.submission.is_empty() {
                chips.push("<li class=\"warn\">Submission guideline not stated</li>".into());
            }
            if !j.gender_checked {
                chips.push("<li class=\"warn\">Gender requirement not verified (page could not be read)</li>".into());
            }
            if let Some(y) = j.years_required {
                chips.push(format!("<li>Experience mentioned: {}+ yrs</li>", y));
            }
            let mut meta: Vec<String> = Vec::new();
            if !j.company.is_empty() {
                meta.push(format!("<b>{}</b>", esc(&j.company)));
            }
            if !j.location.is_empty() {
                meta.push(esc(&j.location));
            }
            let title_html = if rtl { format!("<h3 dir=\"rtl\" lang=\"fa\">{}</h3>", esc(&j.title)) } else { format!("<h3>{}</h3>", esc(&j.title)) };
            let eng = if !j.title_en.is_empty() || !j.summary_en.is_empty() {
                format!("<details><summary>English translation (machine-translated)</summary><div class=\"body\">{}\n\n{}</div></details>", esc(&j.title_en), esc(&j.summary_en))
            } else {
                String::new()
            };
            let mut body = j.description.trim().to_string();
            if body.chars().count() > 2500 {
                body = format!("{}…", trunc(&body, 2500).trim_end());
            }
            let orig = if body.is_empty() {
                String::new()
            } else {
                format!("<details><summary>{}</summary><div class=\"body\" dir=\"auto\">{}</div></details>", if j.source_type == "telegram" { "Original post" } else { "Description" }, esc(&body))
            };
            let mut view = String::new();
            if !j.duplicates.is_empty() {
                let links: Vec<String> = j
                    .duplicates
                    .iter()
                    .map(|d| if d.url.is_empty() { esc(&d.source) } else { format!("<a href=\"{}\" target=\"_blank\" rel=\"noopener noreferrer\">{}</a>", esc(&d.url), esc(&d.source)) })
                    .collect();
                view.push_str(&format!("<p class=\"orig\">Same opening also posted on: {}</p>", links.join(" · ")));
                chips.push(format!("<li>Also on {} other source{}</li>", j.duplicates.len(), if j.duplicates.len() != 1 { "s" } else { "" }));
            }
            let haystack = [j.title.as_str(), &j.company, &j.location, &j.source, &j.title_en, &trunc(&j.description, 600)].join(" ").to_lowercase();
            rows.push_str(&format!(
                "<article class=\"job\" data-source=\"{}\" data-lang=\"{}\" data-text=\"{}\"><div class=\"when\"><strong>{}</strong>{}</div><div class=\"main\">{}<p class=\"meta\" dir=\"auto\">{}</p><ul class=\"chips\">{}</ul><p class=\"why\">{}</p>{}{}{}{}</div></article>",
                esc(&j.source), esc(&j.language), esc(&haystack), lt.format("%H:%M"), esc(&rel_time(now, posted)), title_html, meta.join(" · "), chips.join(""), esc(&j.entry_reason),
                render_how_to_apply(j), view, eng, orig
            ));
        }
        parts.push(format!("<section class=\"day\"><h2>{}<span>{} role{}</span></h2>{}</section>", label, items.len(), if items.len() != 1 { "s" } else { "" }, rows));
    }
    let found = !parts.is_empty();
    if !found {
        parts.push(format!(
            "<div class=\"empty\"><h2>Not found</h2><p>No job met all requirements (entry-level / no experience, English, Persian/Dari or Turkish, posted in the last {} days).</p><p>The numbers above show where postings were filtered out. Add more websites or Telegram channels in config.json, or run again later.</p></div>",
            days_txt
        ));
    }
    let funnel_html: String = funnel.iter().map(|(l, n)| format!("<span><b>{}</b> {}</span>", n, esc(l))).collect();
    let opt_s: String = sources.iter().map(|s| format!("<option value=\"{}\">{}</option>", esc(s), esc(s))).collect();
    let opt_l: String = langs.iter().map(|l| format!("<option value=\"{}\">{}</option>", esc(l), esc(lang_label(l)))).collect();
    let err_html = if errors.is_empty() { String::new() } else { format!("<p>Sources with problems this run: {}</p>", esc(&errors.join("; "))) };
    let header_sub = if found {
        "No experience required or fresher-friendly, usable in English, Persian/Dari or Turkish. Each job shows what is required, the submission guideline and a working link to the original posting. Newest first."
    } else {
        "Nothing matched this time."
    };
    let bar_html = if found {
        format!(
            "<div class=\"bar\"><div class=\"in\"><input id=\"q\" type=\"search\" placeholder=\"Search title, company, place…\" aria-label=\"Search jobs\"><select id=\"fsrc\" aria-label=\"Filter by source\"><option value=\"\">All sources</option>{}</select><select id=\"flang\" aria-label=\"Filter by language\"><option value=\"\">All languages</option>{}</select><button id=\"theme\" type=\"button\">Light / dark</button><span class=\"shown\" id=\"shown\" aria-live=\"polite\"></span></div></div>",
            opt_s, opt_l
        )
    } else {
        String::new()
    };
    format!(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\n<title>Entry-level jobs · last {d} days</title>\n<style>{css}</style></head><body>\n<header class=\"top\"><h1>Entry-level jobs from the last {d} days</h1>\n<p>{sub}</p>\n<div class=\"funnel\">{fun}</div></header>\n{bar}\n<main>{main}</main>\n<footer>Generated {gen}. \"How to apply\" is copied from each post by software and can miss details: always read the original post, confirm on the employer's own page, and never pay to apply.\nTranslations, where shown, are machine-generated and may be inaccurate.{err}</footer>\n<script>{js}</script></body></html>",
        d = days_txt, css = REPORT_CSS, sub = header_sub, fun = funnel_html, bar = bar_html, main = parts.join(""),
        gen = now.with_timezone(&Local).format("%d %B %Y, %H:%M %Z"), err = err_html, js = REPORT_JS
    )
}

/// The compact form the app's results view uses (long texts are shortened: the CSV/HTML report has them in full).
pub fn job_to_dict(j: &Job, now: DateTime<Utc>) -> Value {
    json!({
        "title": j.title, "title_en": j.title_en, "company": j.company, "location": j.location, "source": j.source,
        "also_on": j.duplicates.iter().map(|d| json!({"source": d.source, "url": d.url})).collect::<Vec<_>>(),
        "posted_at": j.posted_at.map(|p| p.to_rfc3339()).unwrap_or_default(),
        "age_hours": j.posted_at.map(|p| round1((now - p).num_milliseconds() as f64 / 3_600_000.0)),
        "remote": j.remote == Some(true), "is_new": j.is_new, "language": j.language, "why": j.entry_reason, "years": j.years_required,
        "url": if j.apply_link.is_empty() { j.source_url.clone() } else { j.apply_link.clone() }, "link_status": j.link_status,
        "requirements": j.requirements, "submission": j.submission, "gender_checked": j.gender_checked, "description": trunc(&j.description, 1500), "summary_en": trunc(&j.summary_en, 600),
    })
}
