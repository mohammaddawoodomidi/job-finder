//! The local web server the page talks to (127.0.0.1 only, Host + X-Requested-With checks).
use crate::cache::{cache_size_kb, clear_page_cache};
use crate::config::*;
use crate::http::Http;
use crate::probe::{probe_channel, probe_site};
use crate::scan::{run_scan, ScanErr, ScanOpts};
use crate::util::*;
use regex::Regex;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::io::Read;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Instant, SystemTime};
use tiny_http::{Header, Method, Request, Response, Server};

const INDEX_HTML: &str = include_str!("../ui/index.html");
const MAX_BODY: u64 = 2_000_000;
const OPEN_URL_SCHEMES: [&str; 4] = ["http", "https", "mailto", "tel"];
static FILE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(?:jobs|rejected|duplicates)_\d{8}_\d{6}\.(?:html|csv|json)$").unwrap());
static STAMP_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^jobs_(\d{8}_\d{6})\.json$").unwrap());

pub static DESKTOP: AtomicBool = AtomicBool::new(false);
pub static QUIT: AtomicBool = AtomicBool::new(false);
static LAST_SEEN: AtomicU64 = AtomicU64::new(0); // last request from the page (seconds since start)
static BYE_AT: AtomicU64 = AtomicU64::new(0); // page said it is closing (0 = never)
static T0: LazyLock<Instant> = LazyLock::new(Instant::now);
fn secs() -> u64 {
    T0.elapsed().as_secs() + 1
}

/// Stops everything and ends the process at once: running scan, every browser we started, the server. Nothing stays behind.
pub fn shutdown_all() -> ! {
    cancel_scan();
    QUIT.store(true, Ordering::Relaxed);
    crate::render::kill_all_browsers();
    let _ = std::fs::remove_dir_all(base().join("browser-profile"));
    std::process::exit(0)
}

/// Browser-fallback mode has no window of ours to watch: when the page's window is closed (it says goodbye, or stops
/// answering the heartbeat) the program quits by itself.
pub fn start_watchdog() {
    LAST_SEEN.store(secs(), Ordering::Relaxed);
    std::thread::spawn(|| loop {
        std::thread::sleep(std::time::Duration::from_millis(500));
        let now = secs();
        let bye = BYE_AT.load(Ordering::Relaxed);
        let seen = LAST_SEEN.load(Ordering::Relaxed);
        if (bye > 0 && now > bye + 4 && seen <= bye) || now > seen + 180 {
            shutdown_all();
        }
    });
}

struct Inner {
    status: &'static str, // idle | running | done | error | cancelled
    result: Option<Value>,
    error: String,
    started: Option<Instant>,
    finished: Option<Instant>,
    progress: Value,
    cancel: Arc<AtomicBool>,
    gen: u64, // which scan owns the state: a stopped scan winding down must not overwrite a newer one
}
static STATE: LazyLock<Mutex<Inner>> = LazyLock::new(|| {
    Mutex::new(Inner { status: "idle", result: None, error: String::new(), started: None, finished: None, progress: json!({"stage":"","done":0,"total":0,"label":""}), cancel: Arc::new(AtomicBool::new(false)), gen: 0 })
});

pub fn cancel_scan() {
    STATE.lock().unwrap().cancel.store(true, Ordering::Relaxed);
}

/// Stop button: the page changes to "stopped" at once; the worker threads notice the flag and wind down in the background.
fn stop_scan() {
    let mut s = STATE.lock().unwrap();
    s.cancel.store(true, Ordering::Relaxed);
    if s.status == "running" {
        s.status = "cancelled";
        s.finished = Some(Instant::now());
        s.gen += 1; // the old scan's final update is now ignored
        drop(s);
        add_line("Stopped. Nothing was saved this time.".into());
    }
}

fn start_scan(opts: &Value) -> Result<(), String> {
    let (cancel, gen) = {
        let mut s = STATE.lock().unwrap();
        if s.status == "running" {
            return Err("A scan is already running.".into());
        }
        s.status = "running";
        s.result = None;
        s.error.clear();
        s.started = Some(Instant::now());
        s.finished = None;
        s.progress = json!({"stage":"start","done":0,"total":0,"label":""});
        s.cancel = Arc::new(AtomicBool::new(false));
        s.gen += 1;
        (s.cancel.clone(), s.gen)
    };
    clear_log();
    let days = match opts.get("days") {
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::String(t)) => t.trim().parse::<f64>().ok(),
        _ => None,
    };
    let only: HashSet<String> = opts.get("only").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty()).collect()).unwrap_or_default();
    let so = ScanOpts {
        days,
        verify: if opts.get("verify") == Some(&Value::Bool(false)) { Some(false) } else { None },
        only: if only.is_empty() { None } else { Some(only) },
        debug: opts.get("debug").and_then(Value::as_bool).unwrap_or(false),
        work_mode: opts.get("work_mode").and_then(Value::as_str).map(str::to_string),
        force_refresh: opts.get("force_refresh").and_then(Value::as_bool).unwrap_or(false),
    };
    std::thread::Builder::new()
        .name("scan".into())
        .spawn(move || {
            let on_progress = |stage: &str, done: usize, total: usize, label: &str| {
                let mut s = STATE.lock().unwrap();
                if s.gen == gen {
                    s.progress = json!({"stage": stage, "done": done, "total": total, "label": label});
                }
            };
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_scan(&config_path(), so, cancel, &on_progress)));
            let mut s = STATE.lock().unwrap();
            if s.gen != gen {
                return; // stopped (and possibly replaced) while it was winding down
            }
            match r {
                Ok(Ok(res)) => {
                    s.result = Some(res);
                    s.status = "done";
                }
                Ok(Err(ScanErr::Cancelled)) => {
                    s.status = "cancelled";
                    drop(s);
                    add_line("Stopped. Nothing was saved this time.".into());
                    STATE.lock().unwrap().finished = Some(Instant::now());
                    return;
                }
                Ok(Err(ScanErr::Failed(e))) => {
                    error!("Scan failed: {}", e);
                    s.status = "error";
                    s.error = format!("Error: {}", e);
                }
                Err(_) => {
                    error!("Scan failed: internal error");
                    s.status = "error";
                    s.error = "Error: internal error (see logs/run.log)".into();
                }
            }
            s.finished = Some(Instant::now());
        })
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// The theme chosen last time (light until the user picks something else).
fn saved_theme() -> String {
    read_config_raw().ok().and_then(|c| c.get("theme").and_then(Value::as_str).map(str::to_string)).filter(|t| matches!(t.as_str(), "light" | "dark" | "auto")).unwrap_or_else(|| "light".into())
}

fn list_reports() -> Vec<Value> {
    let d = out_dir();
    let mut names: Vec<(String, String)> = vec![];
    if let Ok(rd) = std::fs::read_dir(&d) {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            if let Some(m) = STAMP_RE.captures(&n) {
                names.push((n.clone(), m[1].to_string()));
            }
        }
    }
    names.sort_by(|a, b| b.0.cmp(&a.0));
    names
        .into_iter()
        .take(50)
        .map(|(n, stamp)| {
            let (h, c) = (format!("jobs_{}.html", stamp), format!("jobs_{}.csv", stamp));
            let mt = std::fs::metadata(d.join(&n)).and_then(|m| m.modified()).unwrap_or(SystemTime::now());
            let t: chrono::DateTime<chrono::Local> = mt.into();
            json!({"name": n, "html": if d.join(&h).exists() { h } else { String::new() }, "csv": if d.join(&c).exists() { c } else { String::new() }, "time": t.format("%Y-%m-%d %H:%M").to_string()})
        })
        .collect()
}

fn environment_info(cfg: &Value) -> Value {
    let needs: Vec<Value> = cfg.get("websites").and_then(Value::as_array).map(|a| a.iter().filter(|s| s.get("enabled").and_then(Value::as_bool).unwrap_or(true) && s.get("render").map_or(false, |r| !matches!(r, Value::Null | Value::Bool(false)))).filter_map(|s| s.get("name").cloned()).collect()).unwrap_or_default();
    let kb = cache_path(cfg).map(|p| cache_size_kb(&p)).unwrap_or(0);
    json!({"playwright": crate::render::find_browser().is_some(), "browser_sites": needs, "cache_kb": kb, "desktop": DESKTOP.load(Ordering::Relaxed), "version": VERSION, "data_dir": base().display().to_string()})
}

pub fn open_with_os(target: &str) -> Result<(), String> {
    open::that_detached(target).map_err(|e| e.to_string())
}

fn http_for_probes() -> Result<Http, String> {
    let mut cfg = load_config_from_value(&read_config_raw()?);
    cfg["http"]["delay_between_requests_seconds"] = json!(0.3);
    cfg["http"]["max_retries"] = json!(1);
    Ok(Http::new(&cfg))
}

fn hdr(k: &str, v: &str) -> Header {
    Header::from_bytes(k.as_bytes(), v.as_bytes()).unwrap()
}
fn send(req: Request, code: u16, body: Vec<u8>, ctype: &str) {
    let r = Response::from_data(body).with_status_code(code).with_header(hdr("Content-Type", ctype)).with_header(hdr("Cache-Control", "no-store")).with_header(hdr("X-Content-Type-Options", "nosniff"));
    let _ = req.respond(r);
}
fn send_json(req: Request, code: u16, v: Value) {
    send(req, code, v.to_string().into_bytes(), "application/json; charset=utf-8");
}

fn query_param(q: &str, key: &str) -> String {
    url::form_urlencoded::parse(q.as_bytes()).find(|(k, _)| k == key).map(|(_, v)| v.into_owned()).unwrap_or_default()
}

fn handle(mut req: Request, port: u16) {
    let host = req.headers().iter().find(|h| h.field.equiv("Host")).map(|h| h.value.as_str().to_lowercase()).unwrap_or_default();
    if host != format!("127.0.0.1:{}", port) && host != format!("localhost:{}", port) {
        return send_json(req, 403, json!({"error": "bad host"}));
    }
    LAST_SEEN.store(secs(), Ordering::Relaxed);
    let full = req.url().to_string();
    let (path, query) = match full.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (full.clone(), String::new()),
    };
    match req.method().clone() {
        Method::Get => {
            match path.as_str() {
                "/" | "/index.html" => {
                    let theme = saved_theme();
                    let page = INDEX_HTML.replacen("data-theme=\"auto\"", &format!("data-theme=\"{}\"", theme), 1);
                    send(req, 200, page.into_bytes(), "text/html; charset=utf-8")
                }
                "/api/status" => {
                    let since: usize = query_param(&query, "since").parse().unwrap_or(0);
                    let (lines, total) = {
                        let l = LOG.lock().unwrap();
                        let start = since.saturating_sub(l.offset).min(l.lines.len());
                        (l.lines[start..].to_vec(), l.offset + l.lines.len())
                    };
                    let s = STATE.lock().unwrap();
                    let elapsed = s.started.map(|st| s.finished.unwrap_or_else(Instant::now).duration_since(st).as_secs()).unwrap_or(0);
                    let payload = json!({"status": s.status, "lines": lines, "total": total, "result": s.result, "progress": s.progress, "error": s.error, "elapsed": elapsed});
                    drop(s);
                    send_json(req, 200, payload)
                }
                "/api/ping" => send_json(req, 200, json!({"ok": true})),
                "/api/bye" => {
                    BYE_AT.store(secs(), Ordering::Relaxed);
                    send_json(req, 200, json!({"ok": true}))
                }
                "/api/config" => match read_config_raw() {
                    Ok(cfg) => {
                        let env = environment_info(&cfg);
                        send_json(req, 200, json!({"config": cfg, "env": env, "hard_max_days": HARD_MAX_DAYS}))
                    }
                    Err(e) => send_json(req, 500, json!({"error": format!("config.json could not be read: {}", e)})),
                },
                "/api/reports" => send_json(req, 200, json!({"reports": list_reports()})),
                "/api/prefs" => {
                    let v = std::fs::read(base().join("ui-state.json")).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()).filter(Value::is_object).unwrap_or_else(|| json!({}));
                    send_json(req, 200, json!({"prefs": v}))
                }
                "/api/results" => {
                    let name = query_param(&query, "name");
                    let p = out_dir().join(&name);
                    if !STAMP_RE.is_match(&name) || !p.is_file() {
                        return send_json(req, 404, json!({"error": "That result is no longer available."}));
                    }
                    match std::fs::read(&p) {
                        Ok(b) => send(req, 200, b, "application/json; charset=utf-8"),
                        Err(_) => send_json(req, 404, json!({"error": "That result is no longer available."})),
                    }
                }
                _ => send(req, 404, b"Not found".to_vec(), "text/plain; charset=utf-8"),
            }
        }
        Method::Post => {
            let xr = req.headers().iter().find(|h| h.field.equiv("X-Requested-With")).map(|h| h.value.as_str().to_string());
            if xr.as_deref() != Some("jobfinder") {
                return send_json(req, 403, json!({"error": "forbidden"}));
            }
            let mut buf = Vec::new();
            if req.as_reader().take(MAX_BODY + 1).read_to_end(&mut buf).is_err() || buf.len() as u64 > MAX_BODY {
                return send_json(req, 400, json!({"error": "invalid request body"}));
            }
            let body: Value = if buf.is_empty() { json!({}) } else { serde_json::from_slice(&buf).unwrap_or(Value::Null) };
            if !body.is_object() {
                return send_json(req, 400, json!({"error": "invalid request body"}));
            }
            let sget = |k: &str| body.get(k).map(|v| match v { Value::String(s) => s.clone(), Value::Null => String::new(), o => o.to_string() }).unwrap_or_default();
            match path.as_str() {
                "/api/scan" => match start_scan(&body) {
                    Ok(()) => send_json(req, 200, json!({"ok": true, "error": ""})),
                    Err(e) => send_json(req, 409, json!({"ok": false, "error": e})),
                },
                "/api/stop" => {
                    stop_scan();
                    send_json(req, 200, json!({"ok": true}))
                }
                "/api/prefs" => {
                    // what the person last chose on the search page (days, kind, sources, filters, tab ...); stored by the program
                    // because the page's address changes on every start, so the browser itself could not remember it
                    let prefs = body.get("prefs").cloned().unwrap_or(Value::Null);
                    let text = prefs.to_string();
                    if !prefs.is_object() || text.len() > 32_000 {
                        return send_json(req, 400, json!({"ok": false, "error": "invalid preferences"}));
                    }
                    match crate::util::write_atomic(&base().join("ui-state.json"), |p| std::fs::write(p, &text)) {
                        Ok(()) => send_json(req, 200, json!({"ok": true})),
                        Err(e) => send_json(req, 500, json!({"ok": false, "error": format!("Could not save: {}", e)})),
                    }
                }
                "/api/theme" => {
                    let t = sget("theme");
                    if !matches!(t.as_str(), "light" | "dark" | "auto") {
                        return send_json(req, 400, json!({"ok": false, "error": "unknown theme"}));
                    }
                    match read_config_raw() {
                        Ok(mut cfg) => {
                            cfg["theme"] = json!(t);
                            match save_config(&cfg) {
                                Ok(()) => send_json(req, 200, json!({"ok": true})),
                                Err(e) => send_json(req, 500, json!({"ok": false, "error": format!("Could not save: {}", e)})),
                            }
                        }
                        Err(e) => send_json(req, 500, json!({"ok": false, "error": e})),
                    }
                }
                "/api/config" => {
                    let mut cfg = body.get("config").cloned().unwrap_or(Value::Null);
                    if cfg.is_object() && cfg.get("theme").is_none() {
                        if let Some(t) = read_config_raw().ok().and_then(|c| c.get("theme").cloned()) {
                            cfg["theme"] = t; // a settings save must never undo the chosen theme
                        }
                    }
                    let err = validate_config(&cfg);
                    if !err.is_empty() {
                        return send_json(req, 400, json!({"ok": false, "error": err}));
                    }
                    match save_config(&cfg) {
                        Ok(()) => send_json(req, 200, json!({"ok": true})),
                        Err(e) => send_json(req, 500, json!({"ok": false, "error": format!("Could not save: {}", e)})),
                    }
                }
                "/api/delete" => {
                    let name = sget("name");
                    let Some(m) = STAMP_RE.captures(&name) else { return send_json(req, 400, json!({"ok": false, "error": "only results can be deleted"})) };
                    let d = out_dir();
                    for prefix in ["jobs", "rejected", "duplicates"] {
                        for ext in ["json", "html", "csv"] {
                            let _ = std::fs::remove_file(d.join(format!("{}_{}.{}", prefix, &m[1], ext)));
                        }
                    }
                    send_json(req, 200, json!({"ok": true}))
                }
                "/api/clear-cache" => match read_config_raw().ok().and_then(|c| cache_path(&c)) {
                    Some(p) => {
                        clear_page_cache(&p);
                        send_json(req, 200, json!({"ok": true}))
                    }
                    None => send_json(req, 400, json!({"ok": false, "error": "The cache file must be inside the program folder."})),
                },
                "/api/probe-site" | "/api/probe-channel" => {
                    if STATE.lock().unwrap().status == "running" {
                        return send_json(req, 409, json!({"ok": false, "error": "Wait for the running search to finish."}));
                    }
                    let r = std::panic::catch_unwind(|| -> Result<Value, String> {
                        let http = http_for_probes()?;
                        Ok(if path == "/api/probe-site" {
                            probe_site(&http, &sget("url"))
                        } else {
                            let tg = read_config_raw()?.get("telegram").cloned().unwrap_or(json!({}));
                            probe_channel(&http, &tg, body.get("channel").unwrap_or(&Value::Null))
                        })
                    });
                    let v = match r {
                        Ok(Ok(v)) => v,
                        Ok(Err(e)) => json!({"ok": false, "error": format!("Error: {}", e)}),
                        Err(_) => json!({"ok": false, "error": "Error: the check failed unexpectedly"}),
                    };
                    send_json(req, 200, v)
                }
                "/api/open" => {
                    let (kind, name) = (sget("kind"), sget("name"));
                    let target = if kind == "data" {
                        Some(base())
                    } else if kind == "folder" {
                        let d = out_dir();
                        let _ = std::fs::create_dir_all(&d);
                        Some(d)
                    } else if (kind == "report" || kind == "csv") && FILE_RE.is_match(&name) && !name.ends_with(".json") && out_dir().join(&name).is_file() {
                        Some(out_dir().join(&name))
                    } else {
                        None
                    };
                    let Some(t) = target else { return send_json(req, 400, json!({"ok": false, "error": "nothing to open"})) };
                    match open_with_os(&t.to_string_lossy()) {
                        Ok(()) => send_json(req, 200, json!({"ok": true})),
                        Err(e) => send_json(req, 500, json!({"ok": false, "error": format!("Could not open: {}", e)})),
                    }
                }
                "/api/open-url" => {
                    let u = sget("url");
                    let scheme = u.split_once(':').map(|(s, _)| s.to_lowercase()).unwrap_or_default();
                    if u.len() > 2000 || !OPEN_URL_SCHEMES.contains(&scheme.as_str()) {
                        return send_json(req, 400, json!({"ok": false, "error": "unsupported link"}));
                    }
                    let _ = open::that_detached(&u);
                    send_json(req, 200, json!({"ok": true}))
                }
                "/api/quit" => {
                    send_json(req, 200, json!({"ok": true}));
                    cancel_scan();
                    QUIT.store(true, Ordering::Relaxed);
                    std::thread::spawn(|| {
                        std::thread::sleep(std::time::Duration::from_millis(150)); // let the answer reach the page
                        shutdown_all()
                    });
                }
                _ => send_json(req, 404, json!({"error": "unknown endpoint"})),
            }
        }
        _ => send(req, 405, b"Method not allowed".to_vec(), "text/plain; charset=utf-8"),
    }
}

pub struct Running {
    pub port: u16,
    server: Arc<Server>,
}
impl Running {
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}/", self.port)
    }
    pub fn shutdown(&self) {
        cancel_scan();
        self.server.unblock();
    }
}

/// Starts the server on 127.0.0.1 and a free port (0) or the given one; worker threads answer requests.
pub fn create_server(port: u16) -> Result<Running, String> {
    read_config_raw()?; // first run creates the settings file; a broken file is reported
    init_logging();
    let server = Server::http(("127.0.0.1", port)).map_err(|e| e.to_string())?;
    let port = server.server_addr().to_ip().map(|a| a.port()).ok_or("no port")?;
    let server = Arc::new(server);
    for i in 0..8 {
        let s = server.clone();
        std::thread::Builder::new()
            .name(format!("http-{}", i))
            .spawn(move || {
                while let Ok(req) = s.recv() {
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handle(req, port)));
                }
            })
            .map_err(|e| e.to_string())?;
    }
    Ok(Running { port, server })
}
