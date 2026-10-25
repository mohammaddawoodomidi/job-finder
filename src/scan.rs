//! One scan: read every source in parallel, filter, de-duplicate, check links and write the results.
use crate::apply::{normalize_channel, Channel};
use crate::info::{build_apply_info, resolve_apply_link};
use crate::cache::ScrapeCache;
use crate::config::{cache_path, load_config};
use crate::filter::*;
use crate::gender::female_only;
use crate::http::Http;
use crate::lex::Lexicon;
use crate::model::{Job, SourceStat};
use crate::output::*;
use crate::render::RenderHost;
use crate::sources::*;
use crate::text::*;
use crate::translate::Translator;
use crate::util::{base, sha1_hex, write_atomic, HARD_MAX_DAYS};
use chrono::{Duration, Utc};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

pub type Progress<'a> = &'a (dyn Fn(&str, usize, usize, &str) + Sync);

pub enum ScanErr {
    Cancelled,
    Failed(String),
}
impl From<String> for ScanErr {
    fn from(s: String) -> Self {
        ScanErr::Failed(s)
    }
}

#[derive(Default)]
pub struct ScanOpts {
    pub days: Option<f64>,
    pub verify: Option<bool>,
    pub only: Option<HashSet<String>>,
    pub debug: bool,
    pub work_mode: Option<String>,
    pub force_refresh: bool,
}

type TaskFn<'a> = Box<dyn Fn() -> Result<Vec<Job>, String> + Send + Sync + 'a>;

fn collect(cfg: &Value, ctx: &Ctx, cutoff: chrono::DateTime<Utc>, only: &Option<HashSet<String>>, lex: &Lexicon, progress: Progress) -> (Vec<Job>, Vec<String>, Vec<SourceStat>) {
    let mut tasks: Vec<(String, TaskFn)> = Vec::new();
    if let Some(Value::Array(sites)) = cfg.get("websites") {
        for site in sites {
            if !jbool(site, "enabled", true) {
                continue;
            }
            let name = { let n = jstr(site, "name"); if n.is_empty() { jstr(site, "url") } else { n } };
            if let Some(o) = only {
                if !o.contains(&name.to_lowercase()) {
                    continue;
                }
            }
            let ty = jstr(site, "type");
            let Some(f) = site_fn(&ty) else {
                error!("Unknown website type {:?} for {}", ty, name);
                continue;
            };
            if !matches!(site.get("delay_seconds"), None | Some(Value::Null)) {
                // optional per-site pause between requests
                let first = { let u = jstr(site, "url"); if u.is_empty() { jlist(site, "pages").first().cloned().unwrap_or_default() } else { u } };
                ctx.http.set_delay_override(&first, jf64(site, "delay_seconds", 1.5));
            }
            let site = site.clone();
            tasks.push((name, Box::new(move || f(ctx, &site, cutoff))));
        }
    }
    let tg = cfg.get("telegram").cloned().unwrap_or(Value::Null);
    if jbool(&tg, "enabled", true) && only.as_ref().map_or(true, |o| o.contains("telegram")) {
        let channels: Vec<Channel> = tg.get("channels").and_then(Value::as_array).map(|a| a.iter().filter_map(normalize_channel).collect()).unwrap_or_default();
        if channels.is_empty() {
            warn!("Telegram: no channels configured (add channels in Settings).");
        } else {
            if tg.get("mode").and_then(Value::as_str) == Some("telethon") {
                warn!("Telethon mode is not part of this edition: reading the public web preview instead.");
            }
            if !matches!(tg.get("delay_seconds"), None | Some(Value::Null)) {
                let b = { let b = jstr(&tg, "web_preview_base"); if b.is_empty() { "https://t.me/s/".to_string() } else { b } };
                ctx.http.set_delay_override(&b, jf64(&tg, "delay_seconds", 1.5));
            }
            for ch in channels {
                match ch {
                    Channel::Id(id) => warn!("Channel id {} needs a public username (the web preview works with usernames only).", id),
                    Channel::Name(ch) => {
                        let tg = tg.clone();
                        let label = format!("telegram @{}", ch);
                        tasks.push((
                            label,
                            Box::new(move || {
                                let posts = scrape_telegram_web(&ctx.http, &tg, &ch, cutoff);
                                Ok(telegram_jobs(posts, &format!("@{}", ch), lex))
                            }),
                        ));
                    }
                }
            }
        }
    }
    if tasks.is_empty() {
        warn!("No websites or Telegram channels are enabled. Turn some on in Settings first.");
        return (vec![], vec![], vec![]);
    }
    let total = tasks.len();
    let results: Mutex<Vec<Option<Vec<Job>>>> = Mutex::new((0..total).map(|_| None).collect());
    let stats: Mutex<Vec<SourceStat>> = Mutex::new(Vec::new());
    let errors: Mutex<Vec<String>> = Mutex::new(Vec::new());
    let next = AtomicUsize::new(0);
    let workers = (ji64(cfg, "parallel_sources", 4).max(1) as usize).min(total);
    info!("Reading {} source(s), up to {} at the same time.", total, workers);
    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= total || ctx.http.cancelled() {
                    break;
                }
                let (label, f) = &tasks[i];
                let t0 = std::time::Instant::now();
                info!("Reading {} ...", label);
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f()));
                let (res, err) = match outcome {
                    Ok(Ok(r)) => (r, String::new()),
                    Ok(Err(e)) => (vec![], crate::util::trunc(&format!("Error: {}", e), 120 + 7)),
                    Err(_) => (vec![], "Panic: the reader crashed on unexpected page content".to_string()),
                };
                let secs = t0.elapsed().as_secs_f64();
                if err.is_empty() {
                    info!("{:<34} {:>4} postings ({:.1}s)", label, res.len(), secs);
                } else {
                    error!("Source failed: {}: {}", label, err);
                }
                results.lock().unwrap()[i] = Some(res.clone());
                let mut st = stats.lock().unwrap();
                st.push(SourceStat { name: label.clone(), found: res.len(), seconds: (secs * 10.0).round() / 10.0, error: err.clone() });
                if !err.is_empty() {
                    errors.lock().unwrap().push(format!("{}: {}", label, err));
                }
                progress("sources", st.len(), total, label);
            });
        }
    });
    let jobs: Vec<Job> = results.into_inner().unwrap().into_iter().flatten().flatten().collect(); // stable order: as listed in the settings
    let mut stats = stats.into_inner().unwrap();
    stats.sort_by_key(|s| s.name.to_lowercase());
    (jobs, errors.into_inner().unwrap(), stats)
}

fn job_keys(j: &Job) -> Vec<String> {
    let mut urls = vec![j.source_url.clone()];
    urls.extend(j.duplicates.iter().map(|d| d.url.clone()));
    let keys: Vec<String> = urls.iter().filter(|u| !u.is_empty()).map(|u| canonical_url(u)).collect();
    if keys.is_empty() { vec![sha1_hex(&format!("{}|{}|{}", j.source, j.company, j.title))] } else { keys }
}

/// Reads every enabled source, filters, de-duplicates, and writes jobs_<stamp>.json / .html / .csv into output_dir.
pub fn run_scan(cfg_path: &Path, opts: ScanOpts, cancel: Arc<AtomicBool>, progress: Progress) -> Result<Value, ScanErr> {
    let mut cfg = load_config(cfg_path)?;
    if let Some(d) = opts.days.filter(|d| *d > 0.0) {
        cfg["max_age_days"] = json!(d.clamp(0.1, HARD_MAX_DAYS));
    }
    if opts.verify == Some(false) {
        cfg["filters"]["verify_links"] = json!(false);
    }
    if let Some(m) = opts.work_mode.as_deref().filter(|m| matches!(*m, "any" | "remote" | "onsite")) {
        cfg["filters"]["work_mode"] = json!(m);
    }
    let base_dir = cfg_path.parent().map(Path::to_path_buf).unwrap_or_else(base);
    let out_dir = base_dir.join(cfg.get("output_dir").and_then(Value::as_str).unwrap_or("output"));
    std::fs::create_dir_all(&out_dir).map_err(|e| e.to_string())?;

    let t_start = std::time::Instant::now();
    let now = utcnow();
    let max_days = jf64(&cfg, "max_age_days", 7.0);
    let cutoff = now - Duration::milliseconds((max_days * 86_400_000.0) as i64);
    let f = cfg["filters"].clone();
    let mut http = Http::new(&cfg);
    http.cancel = cancel.clone();
    let cc = cfg["cache"].clone();
    let cpath = cache_path(&cfg).unwrap_or_else(|| base_dir.join("cache/scrape_cache.db"));
    let cache = ScrapeCache::new(&cpath, jf64(&cc, "detail_days", 10.0), jf64(&cc, "probe_hours", 24.0), jbool(&cc, "enabled", true) && !opts.force_refresh)
        .map_err(|e| format!("cache: {}", e))?;
    let cache = Arc::new(cache);
    http.cache = Some(cache.clone());
    let lex = Lexicon::new(&f);
    let ctx = Ctx { http, prefilter: Some(lex.exclude_title.clone()), render: RenderHost::default() };
    let translator = Translator::new(&cfg["translation"], &base_dir);
    info!("Window: {} -> {} (UTC)", cutoff.format("%Y-%m-%d %H:%M"), now.format("%Y-%m-%d %H:%M"));
    let check_cancel = || if ctx.http.cancelled() { Err(ScanErr::Cancelled) } else { Ok(()) };

    let (mut jobs, errors, source_stats) = collect(&cfg, &ctx, cutoff, &opts.only, &lex, progress);
    let t_sources = t_start.elapsed();
    ctx.render.shutdown(); // the browser (if any site needed it) is no longer needed
    cache.save(); // keep what was learned even if the run is stopped later
    let (hits, misses) = cache.stats();
    info!("Page cache: {} job page(s) reused from earlier runs, {} downloaded.", hits, misses);
    check_cancel()?;

    let offline = ctx.http.net_ok.load(Ordering::Relaxed) == 0 && ctx.http.net_fail.load(Ordering::Relaxed) > 0;
    if offline {
        error!("No source answered. Check your internet connection.");
    }
    progress("filter", 0, 1, "");
    let mut tm = [0f64; 4];
    let mut funnel: Vec<(String, usize)> = vec![("collected".into(), jobs.len())];
    let mut rejected: Vec<(String, String, String, String)> = Vec::new();
    let mut reject = |j: &Job, why: &str| rejected.push((j.source.clone(), crate::util::trunc(&j.title, 80), why.to_string(), j.source_url.clone()));

    // ---- 0. companies the user excluded (before anything is opened or downloaded for them)
    if let Some(ex) = crate::filter::company_excluder(&f) {
        let mut keep = Vec::new();
        for j in jobs {
            match ex.hit(&j) {
                Some(w) => reject(&j, &format!("company excluded ({})", w)),
                None => keep.push(j),
            }
        }
        jobs = keep;
        funnel.push(("companies not excluded".into(), jobs.len()));
    }
    // ---- 1. recency window (hard)
    let mut keep = Vec::new();
    for j in jobs {
        match j.posted_at {
            None => reject(&j, "no posting date"),
            Some(p) if p < cutoff => reject(&j, "older than window"),
            Some(p) if p > now + Duration::hours(12) => reject(&j, "date in the future"),
            _ => keep.push(j),
        }
    }
    jobs = keep;
    funnel.push((format!("within {} days", fmt_g(max_days)), jobs.len()));
    // ---- 2. location (optional)
    let mut keep = Vec::new();
    for j in jobs {
        if location_ok(&j, &f) { keep.push(j) } else { reject(&j, "location does not match") }
    }
    jobs = keep;
    // ---- 2b. remote / on-site
    let mode = { let m = jstr(&f, "work_mode"); if m.is_empty() { "any".to_string() } else { m } };
    let mut keep = Vec::new();
    for mut j in jobs {
        let t0 = std::time::Instant::now();
        j.remote = Some(detect_remote(&j));
        tm[0] += t0.elapsed().as_secs_f64();
        if mode == "any" || j.remote == Some(mode == "remote") {
            keep.push(j)
        } else {
            let why = if mode == "remote" { "not a remote job" } else { "remote job (on-site requested)" };
            reject(&j, why);
        }
    }
    jobs = keep;
    if mode != "any" {
        funnel.push((if mode == "remote" { "remote only" } else { "on-site only" }.into(), jobs.len()));
    }
    // ---- 3. language requirement
    let mut keep = Vec::new();
    for mut j in jobs {
        let t0 = std::time::Instant::now();
        let (ok, why) = check_language(&mut j, &lex, &f);
        tm[1] += t0.elapsed().as_secs_f64();
        if ok { keep.push(j) } else { reject(&j, &why) }
    }
    jobs = keep;
    funnel.push(("language OK".into(), jobs.len()));
    // ---- 4. entry-level / no experience (translate Persian/Turkish if needed)
    let mut keep = Vec::new();
    for mut j in jobs {
        let t0 = std::time::Instant::now();
        let (mut ok, mut why) = evaluate_entry_level(&mut j, &lex, &f);
        tm[2] += t0.elapsed().as_secs_f64();
        if !ok && translator.enabled() && (j.language == "fa" || j.language == "tr") && why.starts_with("no entry-level") {
            j.title_en = translator.to_english(&ctx.http, &j.title, &j.language);
            j.summary_en = translator.to_english(&ctx.http, &crate::util::trunc(&j.description, 2500), &j.language);
            (ok, why) = evaluate_entry_level(&mut j, &lex, &f);
        }
        if ok {
            j.entry_reason = why;
            build_apply_info(&mut j); // NEVER a reason to drop a job: nothing found just means "not stated"
            keep.push(j);
        } else {
            reject(&j, &why);
        }
    }
    jobs = keep;
    funnel.push(("entry-level".into(), jobs.len()));
    // ---- 4b. open EVERY remaining job page and check the gender requirement (gender wins over "entry level")
    if jbool(&f, "exclude_female_only", true) {
        let mode = crate::gender::Mode::from_config(&{ let m = jstr(&f, "female_filter_mode"); if m.is_empty() { "blunt".to_string() } else { m } });
        let todo: Vec<&mut Job> = jobs.iter_mut().filter(|j| j.page_text.is_empty() && j.source_type != "telegram").collect();
        let n = todo.len();
        if n > 0 {
            info!("Opening {} job page(s) that were not read yet, to check the gender requirement...", n);
            let queue = Mutex::new(todo.into_iter());
            let done = AtomicUsize::new(0);
            std::thread::scope(|s| {
                for _ in 0..4.min(n) {
                    s.spawn(|| loop {
                        if ctx.http.cancelled() {
                            break;
                        }
                        let next = queue.lock().unwrap().next();
                        let Some(j) = next else { break };
                        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| crate::pages::read_job_page(&ctx, j)));
                        let _ = r;
                        progress("pages", done.fetch_add(1, Ordering::Relaxed) + 1, n, "");
                    });
                }
            });
            check_cancel()?;
        }
        let mut keep = Vec::new();
        let mut unread = 0usize;
        for mut j in jobs {
            let main = format!("{}\n{}", j.description, j.full_text);
            let t0 = std::time::Instant::now();
            let mut fo = female_only(&j.title, &main, mode);
            if fo.is_none() && !j.page_text.is_empty() {
                fo = female_only(&j.title, &j.page_text, crate::gender::Mode::Explicit); // side panels: explicit statements and "Gender: Female" fields
            }
            tm[3] += t0.elapsed().as_secs_f64();
            j.gender_checked = j.source_type == "telegram" || !j.page_text.is_empty();
            if !j.gender_checked {
                unread += 1;
                if unread <= 5 {
                    warn!("  page not readable: {}", crate::util::trunc(&j.source_url, 120));
                }
            }
            match fo {
                Some(why) => reject(&j, &why),
                None => keep.push(j),
            }
        }
        jobs = keep;
        if unread > 0 {
            warn!("{} job(s) could not be opened to check their gender requirement - they are marked in the list.", unread);
        }
        funnel.push(("open to male applicants".into(), jobs.len()));
    }
    let t_filter = t_start.elapsed();
    // ---- 5. sort newest first, de-duplicate
    jobs.sort_by(|a, b| b.posted_at.cmp(&a.posted_at));
    let dcfg = DedupeCfg::from_value(cfg.get("dedupe"));
    let (mut jobs, mut dup_log) = dedupe(jobs, &dcfg);
    // English helper text for accepted Persian/Turkish posts
    for j in jobs.iter_mut() {
        if translator.enabled() && (j.language == "fa" || j.language == "tr") {
            if j.summary_en.is_empty() {
                j.title_en = translator.to_english(&ctx.http, &j.title, &j.language);
                j.summary_en = translator.to_english(&ctx.http, &crate::util::trunc(&j.description, 2500), &j.language);
            }
            if !j.apply_text.is_empty() {
                j.apply_text_en = translator.to_english(&ctx.http, &j.apply_text, &j.language);
            }
        }
    }
    translator.save();
    if translator.enabled() && jobs.iter().any(|j| !j.title_en.is_empty()) {
        // English titles now exist: catch Persian/Turkish <-> English copies
        let (j2, more) = dedupe(jobs, &dcfg);
        jobs = j2;
        dup_log.extend(more);
    }
    if !dup_log.is_empty() {
        info!("Merged {} duplicate posting(s): the same opening was found on several sources and is listed once.", dup_log.len());
    }
    if jbool(&f, "exclude_female_only", true) {
        // second look at the merged postings (text from duplicate copies may have been added)
        let mode = crate::gender::Mode::from_config(&{ let m = jstr(&f, "female_filter_mode"); if m.is_empty() { "blunt".to_string() } else { m } });
        let before = jobs.len();
        jobs.retain(|j| female_only(&j.title, &format!("{}\n{}", j.description, j.full_text), mode).is_none() && (j.page_text.is_empty() || female_only(&j.title, &j.page_text, crate::gender::Mode::Explicit).is_none()));
        if jobs.len() < before {
            info!("Removed {} more women-only job(s) after merging duplicates.", before - jobs.len());
        }
    }
    let t_dedupe = t_start.elapsed();
    funnel.push(("final list".into(), jobs.len()));
    // ---- 5b. which of them are new since the last scan?
    let flags = cache.classify_new(&jobs.iter().map(job_keys).collect::<Vec<_>>());
    for (j, n) in jobs.iter_mut().zip(flags) {
        j.is_new = n;
    }
    progress("filter", 1, 1, "");

    // ---- 6. one working link to the original posting per job (a dead link is flagged, the job is never removed)
    let verify = jbool(&f, "verify_links", true);
    {
        let todo: Vec<&mut Job> = jobs.iter_mut().collect();
        let n = todo.len();
        info!("Checking the link of {} jobs...", n);
        let queue = Mutex::new(todo.into_iter());
        let done = AtomicUsize::new(0);
        std::thread::scope(|s| {
            for _ in 0..6.min(n.max(1)) {
                s.spawn(|| loop {
                    if ctx.http.cancelled() {
                        break;
                    }
                    let next = queue.lock().unwrap().next();
                    let Some(j) = next else { break };
                    resolve_apply_link(&ctx.http, j, verify);
                    progress("links", done.fetch_add(1, Ordering::Relaxed) + 1, n, "");
                });
            }
        });
    }
    let t_links = t_start.elapsed();
    let bad = jobs.iter().filter(|j| j.link_status == "dead" || j.link_status.starts_with("unverified")).count();
    if bad > 0 {
        info!("{} job(s) have a link that could not be confirmed - they are marked in the list.", bad);
    }
    cache.save(); // now including the link checks
    check_cancel()?;

    // ---- 7. write outputs (each file is completed under a temporary name first, so a crash never leaves half a report)
    progress("write", 0, 1, "");
    let stamp = now.with_timezone(&chrono::Local).format("%Y%m%d_%H%M%S").to_string();
    let (html_path, csv_path, json_path) = (out_dir.join(format!("jobs_{}.html", stamp)), out_dir.join(format!("jobs_{}.csv", stamp)), out_dir.join(format!("jobs_{}.json", stamp)));
    write_atomic(&html_path, |p| std::fs::write(p, build_html(&jobs, now, max_days, &funnel, &errors))).map_err(|e| e.to_string())?;
    write_atomic(&csv_path, |p| write_csv(&jobs, p, now)).map_err(|e| e.to_string())?;
    {
        let mut w = csv::WriterBuilder::new().terminator(csv::Terminator::CRLF).from_writer(Vec::new());
        let _ = w.write_record(["kept_title", "kept_source", "removed_title", "removed_source", "removed_url", "reason"]);
        for d in &dup_log {
            let _ = w.write_record([&d.kept_title, &d.kept_source, &d.removed_title, &d.removed_source, &d.removed_url, &d.reason]);
        }
        let mut bytes = b"\xEF\xBB\xBF".to_vec();
        bytes.extend(w.into_inner().unwrap_or_default());
        let _ = std::fs::write(out_dir.join(format!("duplicates_{}.csv", stamp)), bytes);
        let mut w = csv::WriterBuilder::new().terminator(csv::Terminator::CRLF).from_writer(Vec::new());
        let _ = w.write_record(["source", "title", "reason", "url"]);
        for r in &rejected {
            let _ = w.write_record([&r.0, &r.1, &r.2, &r.3]);
        }
        let mut bytes = b"\xEF\xBB\xBF".to_vec();
        bytes.extend(w.into_inner().unwrap_or_default());
        let _ = std::fs::write(out_dir.join(format!("rejected_{}.csv", stamp)), bytes);
    }
    let sources_json: Vec<Value> = source_stats.iter().map(|s| json!({"name": s.name, "found": s.found, "seconds": s.seconds, "error": s.error})).collect();
    let funnel_json: Vec<Value> = funnel.iter().map(|(l, n)| json!([l, n])).collect();
    let (h, m) = cache.stats();
    let summary = json!({
        "html": file_name(&html_path), "csv": file_name(&csv_path), "json": file_name(&json_path), "dir": out_dir.display().to_string(),
        "funnel": funnel_json, "errors": errors, "sources": sources_json, "offline": offline, "count": jobs.len(),
        "new_count": jobs.iter().filter(|j| j.is_new).count(), "work_mode": mode, "cache": {"pages_reused": h, "pages_downloaded": m},
    });
    let payload = json!({
        "version": 1, "created": now.to_rfc3339(), "funnel": summary["funnel"], "errors": summary["errors"], "sources": summary["sources"],
        "offline": summary["offline"], "work_mode": summary["work_mode"], "jobs": jobs.iter().map(|j| job_to_dict(j, now)).collect::<Vec<_>>(),
    });
    // last: its presence = scan complete
    write_atomic(&json_path, |p| std::fs::write(p, payload.to_string())).map_err(|e| e.to_string())?;
    progress("write", 1, 1, "");
    {
        let mut by: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
        for r in &rejected {
            *by.entry(crate::util::trunc(&r.2, 60)).or_default() += 1;
        }
        let mut v: Vec<_> = by.into_iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1));
        if !v.is_empty() {
            info!("Rejected {} posting(s): {}. Full list with addresses: the newest rejected_*.csv in the output folder.", rejected.len(), v.iter().take(6).map(|(k, n)| format!("{} x {}", n, k)).collect::<Vec<_>>().join("; "));
        }
    }
    info!("Filter time: remote {:.1}s, language {:.1}s, entry-level {:.1}s, women-only {:.1}s.", tm[0], tm[1], tm[2], tm[3]);
    info!("Time: reading sources {:.1}s, filtering {:.1}s, duplicates+translation {:.1}s, link check {:.1}s, writing {:.1}s.", t_sources.as_secs_f64(), (t_filter - t_sources).as_secs_f64(), (t_dedupe - t_filter).as_secs_f64(), (t_links - t_dedupe).as_secs_f64(), (t_start.elapsed() - t_links).as_secs_f64());
    Ok(summary)
}

fn file_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()
}

