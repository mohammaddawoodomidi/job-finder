//! Makes sure EVERY job's own page has been opened and read before the gender check (and before anything is shown):
//! jobs from RSS feeds, structured data or list-only sources never had their page read; pages that failed once get
//! one more try. The whole page text (side panels included) is kept - "Gender: Female" often sits next to the main text.
use crate::model::Job;
use crate::sources::{extract_jobpostings, fetch_soup, ld_to_job, Ctx};
use crate::text::{element_to_text, text_stripped};
use crate::util::trunc;
use scraper::Selector;
use serde_json::json;

fn key(job: &Job) -> String {
    crate::cache::ScrapeCache::detail_key(&json!({"name": job.source, "kind": "job-page-2"}), &job.source_url)
}

/// Reads the job's page. Returns true when the text of the page is now known.
pub fn read_job_page(ctx: &Ctx, job: &mut Job) -> bool {
    if !job.page_text.is_empty() {
        return true;
    }
    if job.source_type == "telegram" || !(job.source_url.starts_with("http://") || job.source_url.starts_with("https://")) {
        return false;
    }
    let cache = ctx.http.cache.as_ref();
    let ck = key(job);
    if let Some(snap) = cache.and_then(|c| c.get_detail(&ck)) {
        let g = |k: &str| snap.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
        job.page_text = g("page_text");
        if job.full_text.is_empty() {
            job.full_text = g("full_text");
        }
        if !job.page_text.is_empty() {
            return true;
        }
    }
    let site = json!({"render": job.needs_render, "wait_ms": 1500, "detail_wait_ms": 1500});
    let Ok(Some(soup)) = fetch_soup(ctx, &job.source_url, Some(&site), true) else { return false };
    let body = Selector::parse("body").unwrap();
    let text = soup.select(&body).next().map(|b| trunc(&text_stripped(b), 40000)).unwrap_or_default();
    let main_text = soup.select(&body).next().map(|b| trunc(&element_to_text(b), 20000)).unwrap_or_default();
    if text.is_empty() {
        return false;
    }
    job.page_text = text.clone();
    if job.full_text.is_empty() {
        job.full_text = main_text.clone();
    }
    if let Some(ld) = extract_jobpostings(&soup).first() {
        let l = ld_to_job(ld, &job.source_url, &job.source);
        if job.description.chars().count() < 200 && !l.description.is_empty() {
            job.description = l.description; // the feed only had a teaser
        }
    }
    job.page_fetched = true;
    if let Some(c) = cache {
        c.put_detail(&ck, &json!({"page_text": job.page_text, "full_text": job.full_text}));
    }
    true
}
