//! Settings: defaults, merging, validation and the files on disk.
use crate::sources::SITE_TYPES;
use crate::text::{jbool, jf64};
use crate::util::{base, config_path, HARD_MAX_DAYS};
use serde_json::{json, Map, Value};
use std::path::{Component, Path, PathBuf};

/// Written on first start if config.json is missing (identical to the shipped config.json).
pub const DEFAULT_USER_CONFIG_JSON: &str = include_str!("../config.json");

pub fn default_config() -> Value {
    json!({
        "_help": "Edit this file. Lines starting with _ are comments and are ignored.",
        "contact_email": "your-email@example.com",
        "max_age_days": 7,
        "parallel_sources": 8,
        "theme": "light",
        "auto_scan_on_start": false,
        "auto_scan_hours": 0,
        "dedupe": {"enabled": true, "title_similarity": 0.85, "text_similarity": 0.7, "max_days_apart": 4,
                   "_help": "The same opening on several sources is shown once. Raise the numbers to merge less, lower them to merge more. --debug writes duplicates_*.csv listing every merge."},
        "output_dir": "output",
        "cache": {"enabled": true, "detail_days": 10, "probe_hours": 24, "file": "cache/scrape_cache.db"},
        "http": {"timeout_seconds": 20, "delay_between_requests_seconds": 1.5, "max_retries": 3, "respect_robots_txt": true, "block_media_in_browser": true},
        "filters": {
            "acceptable_languages": ["en", "fa", "tr"], "accept_internships": true, "accept_junior_titles": true, "max_min_years_required": 0,
            "extra_include_keywords": [], "extra_exclude_title_keywords": [], "exclude_companies": [], "location_keywords": [], "include_remote_even_if_location_set": true,
            "work_mode": "any", "verify_links": true, "exclude_female_only": true
        },
        "translation": {"backend": "none", "argos_auto_install_language_packs": true, "libretranslate_url": "http://localhost:5000", "libretranslate_api_key": "", "cache_file": "cache/translations.json"},
        "websites": [],
        "telegram": {"enabled": true, "mode": "web_preview", "channels": [], "max_pages_per_channel": 8, "web_preview_base": "https://t.me/s/", "api_id": 0, "api_hash": "", "session_name": "job_scraper_session", "max_messages_per_channel": 500, "delay_seconds": null}
    })
}

pub fn deep_merge(base: &Value, over: &Value) -> Value {
    match (base, over) {
        (Value::Object(b), Value::Object(o)) => {
            let mut out: Map<String, Value> = b.clone();
            for (k, v) in o {
                let merged = match (out.get(k), v) {
                    (Some(bv @ Value::Object(_)), Value::Object(_)) => deep_merge(bv, v),
                    _ => v.clone(),
                };
                out.insert(k.clone(), merged);
            }
            Value::Object(out)
        }
        _ => over.clone(),
    }
}

pub fn load_config_from_value(user: &Value) -> Value {
    let mut cfg = deep_merge(&default_config(), user);
    // user-provided "websites" / "telegram.channels" replace defaults entirely
    if let Some(w) = user.get("websites") {
        cfg["websites"] = w.clone();
    }
    if let Some(c) = user.get("telegram").and_then(|t| t.get("channels")) {
        cfg["telegram"]["channels"] = c.clone();
    }
    let d = jf64(&cfg, "max_age_days", 7.0).clamp(0.1, HARD_MAX_DAYS);
    cfg["max_age_days"] = json!(d);
    cfg
}

pub fn read_config_raw() -> Result<Value, String> {
    let p = config_path();
    if !p.exists() {
        if let Some(d) = p.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        std::fs::write(&p, DEFAULT_USER_CONFIG_JSON).map_err(|e| e.to_string())?; // first run: the shipped settings
    }
    let s = std::fs::read_to_string(&p).map_err(|e| e.to_string())?;
    serde_json::from_str(s.trim_start_matches('\u{feff}')).map_err(|e| e.to_string())
}

pub fn load_config(path: &Path) -> Result<Value, String> {
    let s = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let v: Value = serde_json::from_str(s.trim_start_matches('\u{feff}')).map_err(|e| e.to_string())?;
    Ok(load_config_from_value(&v))
}

/// The cache file named in the settings, or None if it would lie outside the program's own folder.
pub fn cache_path(cfg: &Value) -> Option<PathBuf> {
    let name = cfg.get("cache").and_then(|c| c.get("file")).and_then(Value::as_str).filter(|s| !s.is_empty()).unwrap_or("cache/scrape_cache.db");
    let rel = Path::new(name);
    if rel.components().any(|c| matches!(c, Component::ParentDir | Component::RootDir | Component::Prefix(_))) {
        return None;
    }
    Some(base().join(rel))
}

/// An error message, or "" when the config is usable.
pub fn validate_config(cfg: &Value) -> String {
    let Value::Object(m) = cfg else { return "The settings must be a JSON object.".into() };
    match m.get("max_age_days") {
        None => {}
        Some(Value::Number(n)) => {
            if n.as_f64().unwrap_or(0.0) <= 0.0 {
                return "max_age_days must be greater than 0.".into();
            }
        }
        Some(Value::String(s)) if s.trim().parse::<f64>().is_ok() => {
            if s.trim().parse::<f64>().unwrap() <= 0.0 {
                return "max_age_days must be greater than 0.".into();
            }
        }
        Some(_) => return "max_age_days must be a number.".into(),
    }
    for section in ["http", "filters", "dedupe", "translation", "telegram", "cache"] {
        if let Some(v) = m.get(section) {
            if !v.is_object() {
                return format!("\"{}\" must be an object {{ ... }}.", section);
            }
        }
    }
    let mode = cfg.get("filters").and_then(|f| f.get("work_mode")).and_then(Value::as_str).unwrap_or("any");
    if !matches!(mode, "any" | "remote" | "onsite") {
        return "filters.work_mode must be any, remote or onsite.".into();
    }
    if cache_path(cfg).is_none() {
        return "cache.file must be a path inside the program folder.".into();
    }
    match m.get("websites") {
        None => {}
        Some(Value::Array(sites)) => {
            for s in sites {
                if !s.is_object() || s.get("name").and_then(Value::as_str).map_or(true, str::is_empty) {
                    return "Every entry in \"websites\" needs a \"name\".".into();
                }
            }
            for s in sites {
                let ty = s.get("type").and_then(Value::as_str).unwrap_or("");
                if jbool(s, "enabled", true) && !SITE_TYPES.contains(&ty) {
                    return format!("Website {:?}: \"type\" must be one of {}.", s["name"].as_str().unwrap_or(""), SITE_TYPES.join(", "));
                }
            }
        }
        Some(_) => return "Every entry in \"websites\" needs a \"name\".".into(),
    }
    if let Some(tg) = m.get("telegram") {
        if let Some(ch) = tg.get("channels") {
            if !ch.is_array() {
                return "telegram.channels must be a list.".into();
            }
        }
    }
    String::new()
}

pub fn save_config(cfg: &Value) -> std::io::Result<()> {
    let p = config_path();
    if p.exists() {
        let _ = std::fs::copy(&p, p.with_extension("json.bak"));
    }
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(cfg).unwrap_or_default())?;
    std::fs::rename(&tmp, &p)
}

pub fn out_dir() -> PathBuf {
    let name = read_config_raw().ok().and_then(|c| c.get("output_dir").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string)).unwrap_or_else(|| "output".into());
    base().join(name)
}
