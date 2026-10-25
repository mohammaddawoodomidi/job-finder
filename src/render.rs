//! Optional browser rendering for sites whose jobs are built by JavaScript (config: "render": true).
//! Uses the Microsoft Edge every Windows 10/11 PC already has (or Chrome / Chromium) through the DevTools protocol.
use headless_chrome::{Browser, Tab};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Debug)]
pub enum RenderErr {
    /// No browser could be started (the message tells the user how to fix it).
    Unavailable(String),
    Other(String),
}
impl std::fmt::Display for RenderErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RenderErr::Unavailable(s) | RenderErr::Other(s) => write!(f, "{}", s),
        }
    }
}

/// Every Chromium-based browser found, best first: a path from `browser_path` in the settings or JOBFINDER_BROWSER,
/// then Chrome, Brave, other Chromium browsers, and Microsoft Edge last (it is optional).
pub fn find_browsers() -> Vec<PathBuf> {
    let mut c: Vec<PathBuf> = Vec::new();
    if let Ok(p) = std::env::var("JOBFINDER_BROWSER") {
        c.push(PathBuf::from(p));
    }
    if let Some(p) = crate::config::read_config_raw().ok().and_then(|v| v.get("browser_path").and_then(|x| x.as_str().map(str::to_string))).filter(|p| !p.trim().is_empty()) {
        c.push(PathBuf::from(p.trim()));
    }
    if cfg!(windows) {
        let roots: Vec<PathBuf> = ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"].iter().filter_map(|v| std::env::var_os(v)).map(PathBuf::from).collect();
        for rel in [r"Google\Chrome\Application\chrome.exe", r"BraveSoftware\Brave-Browser\Application\brave.exe", r"BraveSoftware\Brave-Browser-Beta\Application\brave.exe", r"Chromium\Application\chrome.exe", r"Vivaldi\Application\vivaldi.exe", r"Microsoft\Edge\Application\msedge.exe"] {
            for r in &roots {
                c.push(r.join(rel));
            }
        }
    } else {
        for n in ["google-chrome", "google-chrome-stable", "brave-browser", "brave", "chromium", "chromium-browser", "microsoft-edge"] {
            for dir in std::env::var("PATH").unwrap_or_default().split(':') {
                c.push(PathBuf::from(dir).join(n));
            }
        }
        if let Ok(rd) = std::fs::read_dir("/opt/pw-browsers") {
            for e in rd.flatten() {
                let n = e.file_name().to_string_lossy().to_string();
                if n.starts_with("chromium") && !n.contains("headless") {
                    c.push(e.path().join("chrome-linux/chrome"));
                    c.push(e.path().join("chrome-linux64/chrome"));
                }
            }
        }
    }
    let mut seen = std::collections::HashSet::new();
    c.into_iter().filter(|p| p.is_file() && seen.insert(p.clone())).collect()
}

pub fn find_browser() -> Option<PathBuf> {
    find_browsers().into_iter().next()
}

/// Every browser process we started: (pid, profile folder). Used to kill them all when the app is closed.
static LIVE: Mutex<Vec<(u32, PathBuf)>> = Mutex::new(Vec::new());

fn kill_tree(pid: u32) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let _ = std::process::Command::new("taskkill").args(["/PID", &pid.to_string(), "/T", "/F"]).creation_flags(0x0800_0000).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status();
    }
    #[cfg(not(windows))]
    {
        let _ = std::process::Command::new("pkill").args(["-9", "-P", &pid.to_string()]).status();
        let _ = std::process::Command::new("kill").args(["-9", &pid.to_string()]).status();
    }
}

/// Kills every browser we started (with all its sub-processes) and deletes their temporary profiles.
pub fn kill_all_browsers() {
    let list: Vec<(u32, PathBuf)> = std::mem::take(&mut *LIVE.lock().unwrap());
    for (pid, _) in &list {
        kill_tree(*pid);
    }
    for (_, dir) in &list {
        for _ in 0..3 {
            if std::fs::remove_dir_all(dir).is_ok() || !dir.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(150));
        }
    }
}

pub struct Renderer {
    browser: Browser,
    ua: String,
    timeout: Duration,
    pages: Mutex<HashMap<u32, Arc<Tab>>>,
    next: AtomicU32,
    gate: Mutex<()>, // one browser operation at a time, like the original's single browser thread
    child: Mutex<Option<std::process::Child>>,
    profile: PathBuf,
    scratch: Mutex<Option<Arc<Tab>>>, // one tab reused for all single-page fetches (opening a tab costs ~0.5 s)
}

impl Drop for Renderer {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.lock().unwrap().take() {
            let pid = c.id();
            kill_tree(pid);
            let _ = c.kill();
            let _ = c.wait();
            LIVE.lock().unwrap().retain(|(p, _)| *p != pid);
        }
        for _ in 0..5 {
            if std::fs::remove_dir_all(&self.profile).is_ok() || !self.profile.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(200)); // Windows keeps files locked for a moment
        }
    }
}

/// Starts the browser ourselves and asks it for a free debugging port (`--remote-debugging-port=0`); it writes the
/// address into `DevToolsActivePort` in its profile folder. This works with Edge, which (unlike Chrome) does not print
/// the address to the console - the browser library waited for that line 11 times, ~30 s each.
fn launch(path: &std::path::Path, block_media: bool, headless_arg: &str) -> Result<(std::process::Child, String, PathBuf), String> {
    let profile = crate::util::base().join("browser-profile").join(format!("run-{}-{}", std::process::id(), crate::util::now_ts() as u64));
    std::fs::create_dir_all(&profile).map_err(|e| format!("cannot create {}: {}", profile.display(), e))?;
    let mut cmd = std::process::Command::new(path);
    cmd.arg(headless_arg)
        .arg("--remote-debugging-port=0")
        .arg("--remote-allow-origins=*")
        .arg(format!("--user-data-dir={}", profile.display()))
        .args(["--no-first-run", "--no-default-browser-check", "--disable-extensions", "--disable-background-networking", "--disable-sync", "--disable-default-apps", "--disable-features=msEdgeStartupBoost,msEdgeSidebarV2,Translate", "--disable-popup-blocking", "--disable-dev-shm-usage", "--lang=en-US", "--window-size=1280,1600", "about:blank"]);
    if block_media {
        cmd.args(["--blink-settings=imagesEnabled=false", "--mute-audio"]);
    }
    #[cfg(unix)]
    if unsafe { libc_geteuid() } == 0 {
        cmd.arg("--no-sandbox"); // Chromium refuses to run as root otherwise (only relevant outside Windows)
    }
    cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let mut child = cmd.spawn().map_err(|e| format!("{}", e))?;
    LIVE.lock().unwrap().push((child.id(), profile.clone()));
    let file = profile.join("DevToolsActivePort");
    let deadline = std::time::Instant::now() + Duration::from_secs(25);
    let mut exited_at: Option<std::time::Instant> = None;
    loop {
        if let Ok(txt) = std::fs::read_to_string(&file) {
            let mut it = txt.lines();
            if let (Some(port), Some(p)) = (it.next(), it.next()) {
                if port.trim().parse::<u16>().is_ok() && p.starts_with('/') {
                    return Ok((child, format!("ws://127.0.0.1:{}{}", port.trim(), p.trim()), profile));
                }
            }
        }
        if let Ok(Some(st)) = child.try_wait() {
            // Edge/Chrome may hand the work to another process and exit with code 0: keep looking for the port file a few more seconds
            if !st.success() || exited_at.get_or_insert_with(std::time::Instant::now).elapsed() > Duration::from_secs(6) {
                kill_tree(child.id());
                LIVE.lock().unwrap().retain(|(p, _)| *p != child.id());
                let _ = std::fs::remove_dir_all(&profile);
                return Err(format!("the browser exited right after starting ({})", st));
            }
        }
        if std::time::Instant::now() > deadline {
            kill_tree(child.id());
            let _ = child.kill();
            let _ = child.wait();
            LIVE.lock().unwrap().retain(|(p, _)| *p != child.id());
            let _ = std::fs::remove_dir_all(&profile);
            return Err("the browser did not report its debugging address within 25 seconds".into());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(unix)]
extern "C" {
    #[link_name = "geteuid"]
    fn libc_geteuid() -> u32;
}

const NEXT_JS: &str = r#"(function(sel){
 function vis(e){return e && e.offsetParent!==null && !e.disabled;}
 var css=['a[rel="next"]','[aria-label*="next" i]:not([disabled])','li.next a','[class*="pagination" i] [class*="next" i]'];
 if(sel) css=[sel];
 for(var i=0;i<css.length;i++){try{var e=document.querySelector(css[i]);if(vis(e)){e.click();return true;}}catch(x){}}
 if(sel) return false;
 var texts=['load more','show more','next','›','»'];
 var els=[].slice.call(document.querySelectorAll('a,button'));
 for(var t=0;t<texts.length;t++){for(var k=0;k<els.length;k++){var e=els[k];if(vis(e)&&(e.textContent||'').trim().toLowerCase().indexOf(texts[t])>=0){e.click();return true;}}}
 return false;})"#;
const SIG_JS: &str = "document.body.innerText.length + ':' + document.querySelectorAll('a').length";

impl Renderer {
    pub fn new(ua: &str, timeout_ms: u64, block_media: bool) -> Result<Renderer, RenderErr> {
        let browsers = find_browsers();
        if browsers.is_empty() {
            return Err(RenderErr::Unavailable("This site needs a browser, but none was found. Install Google Chrome or Brave (Microsoft Edge also works), or set \"browser_path\" in config.json to its program file.".into()));
        }
        let mut problems: Vec<String> = Vec::new();
        for path in &browsers {
            for headless in ["--headless=new", "--headless"] {
                let (mut child, ws, profile) = match launch(path, block_media, headless) {
                    Ok(x) => x,
                    Err(m) => {
                        problems.push(format!("{} ({}): {}", path.display(), headless, m));
                        continue;
                    }
                };
                match Browser::connect_with_timeout(ws, Duration::from_secs(6 * 3600)) {
                    Ok(browser) => {
                        info!("Using the browser {}", path.display());
                        return Ok(Renderer { browser, ua: ua.to_string(), timeout: Duration::from_millis(timeout_ms), pages: Mutex::new(HashMap::new()), next: AtomicU32::new(0), gate: Mutex::new(()), child: Mutex::new(Some(child)), profile, scratch: Mutex::new(None) });
                    }
                    Err(e) => {
                        kill_tree(child.id());
                        let _ = child.kill();
                        let _ = child.wait();
                        LIVE.lock().unwrap().retain(|(p, _)| *p != child.id());
                        let _ = std::fs::remove_dir_all(&profile);
                        problems.push(format!("{} ({}): started but could not be controlled: {}", path.display(), headless, e.to_string().lines().next().unwrap_or("")));
                    }
                }
            }
        }
        Err(RenderErr::Unavailable(format!("No browser could be started for this site. Tried: {}. Close other windows of that browser, or set \"browser_path\" in config.json to Chrome or Brave.", problems.join(" | "))))
    }

    fn new_tab(&self) -> Result<Arc<Tab>, RenderErr> {
        let tab = self.browser.new_tab().map_err(|e| RenderErr::Other(e.to_string()))?;
        tab.set_default_timeout(self.timeout);
        let _ = tab.set_user_agent(&self.ua, Some("en-US,en"), None);
        Ok(tab)
    }

    /// Polls `document.readyState` (much quicker than the library's own wait, which sleeps ~1 s per page).
    fn wait_ready(&self, tab: &Tab) {
        let t0 = std::time::Instant::now();
        while t0.elapsed() < self.timeout {
            let done = tab.evaluate("document.readyState", false).ok().and_then(|o| o.value).map_or(false, |v| v.as_str() == Some("complete"));
            if done {
                break;
            }
            std::thread::sleep(Duration::from_millis(40));
        }
    }

    fn load(&self, tab: &Tab, url: &str, wait_selector: &str, wait_ms: u64) -> Result<(), RenderErr> {
        tab.navigate_to(url).map_err(|e| RenderErr::Other(e.to_string()))?;
        self.wait_ready(tab); // some sites never settle: that is fine, the timeout ends the wait
        let mut found = false;
        if !wait_selector.is_empty() {
            found = tab.wait_for_element_with_custom_timeout(wait_selector, self.timeout).is_ok();
        }
        if wait_ms > 0 {
            Renderer::settle(tab, wait_ms, if found { 3 } else { 6 });
        }
        Ok(())
    }

    fn goto(&self, url: &str, wait_selector: &str, wait_ms: u64) -> Result<Arc<Tab>, RenderErr> {
        let tab = self.new_tab()?;
        self.load(&tab, url, wait_selector, wait_ms)?;
        Ok(tab)
    }

    /// Open a page, let its JavaScript run, return the final HTML.
    pub fn fetch(&self, url: &str, wait_selector: &str, wait_ms: u64) -> Result<String, RenderErr> {
        let _g = self.gate.lock().unwrap();
        let tab = {
            let mut sc = self.scratch.lock().unwrap();
            match sc.as_ref() {
                Some(t) => t.clone(),
                None => {
                    let t = self.new_tab()?;
                    *sc = Some(t.clone());
                    t
                }
            }
        };
        let r = self.load(&tab, url, wait_selector, wait_ms).and_then(|_| tab.get_content().map_err(|e| RenderErr::Other(e.to_string())));
        if r.is_err() {
            if let Some(t) = self.scratch.lock().unwrap().take() {
                let _ = t.close(false); // start from a fresh tab next time
            }
        }
        r
    }

    pub fn open(&self, url: &str, wait_selector: &str, wait_ms: u64) -> Result<u32, RenderErr> {
        let _g = self.gate.lock().unwrap();
        let tab = self.goto(url, wait_selector, wait_ms)?;
        let id = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        self.pages.lock().unwrap().insert(id, tab);
        Ok(id)
    }

    fn page(&self, id: u32) -> Result<Arc<Tab>, RenderErr> {
        self.pages.lock().unwrap().get(&id).cloned().ok_or_else(|| RenderErr::Other("page closed".into()))
    }

    pub fn html(&self, id: u32) -> Result<String, RenderErr> {
        let _g = self.gate.lock().unwrap();
        self.page(id)?.get_content().map_err(|e| RenderErr::Other(e.to_string()))
    }

    fn sig(tab: &Tab) -> String {
        tab.evaluate(SIG_JS, false).ok().and_then(|o| o.value).map(|v| v.to_string()).unwrap_or_default()
    }

    /// Waits until the page has stopped changing (0.3-0.6 s without change), at most `max_ms` - instead of always sleeping
    /// the full time, which made every job page cost 2-2.5 s even when it was ready after 0.5 s.
    fn settle(tab: &Tab, max_ms: u64, needed: u32) {
        let t0 = std::time::Instant::now();
        let mut last = Renderer::sig(tab);
        let mut stable = 0;
        while (t0.elapsed().as_millis() as u64) < max_ms {
            std::thread::sleep(Duration::from_millis(100));
            let now = Renderer::sig(tab);
            if now == last {
                stable += 1;
                if stable >= needed {
                    break;
                }
            } else {
                stable = 0;
                last = now;
            }
        }
    }

    /// Click "next" / "load more" (or scroll for infinite lists). True if the page content changed.
    pub fn advance(&self, id: u32, next_selector: &str, wait_ms: u64) -> Result<bool, RenderErr> {
        let _g = self.gate.lock().unwrap();
        let tab = self.page(id)?;
        let before = Renderer::sig(&tab);
        let call = format!("{}({})", NEXT_JS, Value::String(next_selector.to_string()));
        let clicked = tab.evaluate(&call, false).ok().and_then(|o| o.value).and_then(|v| v.as_bool()).unwrap_or(false);
        if !clicked {
            let _ = tab.evaluate("window.scrollTo(0, document.body.scrollHeight)", false);
        }
        // wait for the list to change (at most wait_ms), then let it finish loading
        let t0 = std::time::Instant::now();
        let limit = wait_ms.max(500);
        while (t0.elapsed().as_millis() as u64) < limit && Renderer::sig(&tab) == before {
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = tab.wait_until_navigated();
        Renderer::settle(&tab, 1500, 4);
        Ok(Renderer::sig(&tab) != before)
    }

    pub fn close(&self, id: u32) {
        let _g = self.gate.lock().unwrap();
        if let Some(t) = self.pages.lock().unwrap().remove(&id) {
            let _ = t.close(false);
        }
    }
}

/// Starts the shared browser the first time a site needs it.
#[derive(Default)]
pub struct RenderHost {
    inner: Mutex<Option<Result<Arc<Renderer>, String>>>,
}

impl RenderHost {
    pub fn get(&self, ua: &str, block_media: bool) -> Result<Arc<Renderer>, RenderErr> {
        let mut g = self.inner.lock().unwrap();
        if g.is_none() {
            *g = Some(Renderer::new(ua, 20000, block_media).map(Arc::new).map_err(|e| e.to_string()));
        }
        match g.as_ref().unwrap() {
            Ok(r) => Ok(r.clone()),
            Err(m) => Err(RenderErr::Unavailable(m.clone())),
        }
    }
    pub fn shutdown(&self) {
        *self.inner.lock().unwrap() = None; // dropping the browser closes it
    }
}
