#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]
#[macro_use]
mod util;
mod apply;
mod assets;
mod cache;
mod config;
mod difflib;
mod filter;
mod gender;
mod gui;
mod http;
mod info;
mod lex;
mod lexdata;
mod model;
mod output;
mod pages;
mod probe;
mod render;
mod rx;
mod scan;
mod server;
mod sources;
mod text;
mod translate;

use std::sync::atomic::Ordering;
use util::VERSION;

fn run(args: &[String]) -> i32 {
    if !gui::acquire_single_instance() {
        gui::message_box("Job Finder is already running. Look for its window in the taskbar.", false);
        return 0;
    }
    let srv = match server::create_server(0) {
        Ok(s) => s,
        Err(e) => {
            gui::message_box(&format!("Job Finder could not start:\n{}\n\nSettings file: {}", e, util::config_path().display()), true);
            return 1;
        }
    };
    gui::kill_children_with_me();
    let _ = std::fs::remove_dir_all(util::base().join("browser-profile")); // leftovers of a crashed run
    let url = srv.url();
    info!("Job Finder {} started at {} (data folder: {})", VERSION, url, util::base().display());
    server::DESKTOP.store(true, Ordering::Relaxed);
    let forced_browser = args.iter().any(|a| a == "--browser");
    let native = if forced_browser { Err("--browser was given".to_string()) } else { gui::show_window(&url) };
    if let Err(e) = native {
        warn!("Native window unavailable ({}); using the browser fallback.", e);
        server::DESKTOP.store(false, Ordering::Relaxed); // the page then shows its own Quit button
        gui::open_fallback_ui(&url);
        server::start_watchdog();
        while !server::QUIT.load(Ordering::Relaxed) {
            std::thread::sleep(std::time::Duration::from_millis(300));
        }
    }
    // the window was closed (or Quit pressed): stop the scan, kill browsers, exit - immediately
    server::shutdown_all()
}

/// Starts the server in a scratch data folder and checks the program's own pieces. Exit 0 = healthy.
fn selftest(out_file: Option<&str>) -> i32 {
    use serde_json::json;
    let mut res = serde_json::Map::new();
    res.insert("version".into(), json!(VERSION));
    let mut ok = true;
    let tmp = std::env::temp_dir().join(format!("jobfinder-selftest-{}", std::process::id()));
    util::set_base(tmp.clone());
    util::set_quiet(true);
    match server::create_server(0) {
        Ok(srv) => {
            let get = |p: &str| ureq::get(format!("http://127.0.0.1:{}{}", srv.port, p)).call().ok().and_then(|mut r| r.body_mut().read_to_string().ok());
            let page = get("/").unwrap_or_default();
            let cfg: serde_json::Value = get("/api/config").and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
            let p_ok = page.contains("Job Finder");
            let c_ok = cfg["config"]["websites"].as_array().map_or(false, |a| !a.is_empty());
            res.insert("page served".into(), json!(if p_ok { "ok" } else { "FAILED: unexpected page" }));
            res.insert("default settings".into(), json!(if c_ok { "ok" } else { "FAILED: no websites in default settings" }));
            ok = p_ok && c_ok;
            srv.shutdown();
        }
        Err(e) => {
            res.insert("server".into(), json!(format!("FAILED: {}", e)));
            ok = false;
        }
    }
    res.insert("browser for JavaScript sites".into(), json!(render::find_browser().map(|p| p.display().to_string()).unwrap_or_else(|| "not found (optional)".into())));
    res.insert("healthy".into(), json!(ok));
    let text = serde_json::to_string_pretty(&res).unwrap_or_default();
    match out_file {
        Some(f) => {
            let _ = std::fs::write(f, text);
        }
        None => println!("{}", text),
    }
    let _ = std::fs::remove_dir_all(tmp);
    if ok { 0 } else { 1 }
}

/// Hidden: runs one scan without a window (used by tests): --scan [days]
fn headless_scan(args: &[String]) -> i32 {
    util::init_logging();
    let days = args.get(1).and_then(|d| d.parse::<f64>().ok());
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let opts = scan::ScanOpts { days, ..Default::default() };
    match scan::run_scan(&util::config_path(), opts, cancel, &|_, _, _, _| {}) {
        Ok(v) => {
            println!("{}", v);
            0
        }
        Err(scan::ScanErr::Cancelled) => 1,
        Err(scan::ScanErr::Failed(e)) => {
            eprintln!("{}", e);
            1
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let r = std::panic::catch_unwind(|| {
        if let Some(i) = args.iter().position(|a| a == "--selftest") {
            return selftest(args.get(i + 1).map(String::as_str));
        }
        if let Some(i) = args.iter().position(|a| a == "--scan") {
            return headless_scan(&args[i..]);
        }
        run(&args)
    });
    let code = r.unwrap_or_else(|_| {
        let log = util::base().join("logs").join("crash.log");
        let _ = std::fs::create_dir_all(log.parent().unwrap());
        let _ = std::fs::OpenOptions::new().create(true).append(true).open(&log).and_then(|mut f| {
            use std::io::Write;
            writeln!(f, "--- Job Finder {} ---\nunexpected internal error (panic)", VERSION)
        });
        gui::message_box(&format!("Job Finder hit an unexpected error.\nDetails were saved to:\n{}", log.display()), true);
        1
    });
    std::process::exit(code);
}
