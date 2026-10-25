//! Polite HTTP client: per-host throttling, retries, robots.txt, circuit breaker, link probing.
use crate::cache::ScrapeCache;
use crate::text::{jbool, jf64, ji64, jstr};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use ureq::ResponseExt;

pub struct Resp {
    pub status: u16,
    pub body: Vec<u8>,
    pub url: String,
    pub content_type: String,
    pub retry_after: String,
}

impl Resp {
    /// The body as text: charset from the header, a BOM or a <meta> tag, else UTF-8.
    pub fn text(&self) -> String {
        decode_bytes(&self.body, &self.content_type)
    }
    pub fn json(&self) -> Option<Value> {
        serde_json::from_slice(&self.body).ok()
    }
}

pub fn decode_bytes(body: &[u8], content_type: &str) -> String {
    let mut label: Option<String> = None;
    if let Some(i) = content_type.to_lowercase().find("charset=") {
        let rest = &content_type[i + 8..];
        label = Some(rest.split(';').next().unwrap_or("").trim().trim_matches('"').to_string());
    }
    if label.is_none() {
        if let Some((enc, _)) = encoding_rs::Encoding::for_bom(body) {
            return enc.decode(body).0.into_owned();
        }
        let head = String::from_utf8_lossy(&body[..body.len().min(4096)]).to_lowercase();
        if let Some(i) = head.find("charset=") {
            let rest = &head[i + 8..];
            let l: String = rest.trim_start_matches(['"', '\'']).chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect();
            if !l.is_empty() {
                label = Some(l);
            }
        }
    }
    let enc = label.and_then(|l| encoding_rs::Encoding::for_label(l.as_bytes())).unwrap_or(encoding_rs::UTF_8);
    enc.decode(body).0.into_owned()
}

/// `urlparse(url).netloc` (lower-case host plus an explicit port).
pub fn netloc(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(u) => match (u.host_str(), u.port()) {
            (Some(h), Some(p)) => format!("{}:{}", h, p),
            (Some(h), None) => h.to_string(),
            _ => String::new(),
        },
        Err(_) => String::new(),
    }
}

// ------------------------------------------------------------------------------------------------ robots.txt
#[derive(Default)]
pub struct Robots {
    groups: Vec<(Vec<String>, Vec<(bool, String)>)>, // (agents, [(allow, pattern)])
}

impl Robots {
    pub fn parse(text: &str) -> Robots {
        let mut groups: Vec<(Vec<String>, Vec<(bool, String)>)> = Vec::new();
        let mut in_agents = false;
        for raw in text.lines() {
            let line = raw.split('#').next().unwrap_or("").trim();
            let Some((field, value)) = line.split_once(':') else { continue };
            let (field, value) = (field.trim().to_lowercase(), value.trim());
            match field.as_str() {
                "user-agent" => {
                    if !in_agents || groups.is_empty() {
                        groups.push((Vec::new(), Vec::new()));
                    }
                    groups.last_mut().unwrap().0.push(value.to_lowercase());
                    in_agents = true;
                }
                "allow" | "disallow" => {
                    in_agents = false;
                    if let Some(g) = groups.last_mut() {
                        if !value.is_empty() {
                            g.1.push((field == "allow", value.to_string()));
                        }
                    }
                }
                _ => {}
            }
        }
        Robots { groups }
    }

    pub fn disallow_all() -> Robots {
        Robots { groups: vec![(vec!["*".into()], vec![(false, "/".into())])] }
    }

    fn pattern_len(pattern: &str, path: &str) -> Option<usize> {
        // '*' = any run of characters, a trailing '$' = end of the address
        let (pat, anchored) = match pattern.strip_suffix('$') {
            Some(p) => (p, true),
            None => (pattern, false),
        };
        let parts: Vec<&str> = pat.split('*').collect();
        let mut pos = 0usize;
        for (i, part) in parts.iter().enumerate() {
            if i == 0 {
                if !path.starts_with(part) {
                    return None;
                }
                pos = part.len();
            } else if let Some(k) = path[pos..].find(part) {
                pos += k + part.len();
            } else {
                return None;
            }
        }
        if anchored && parts.len() == 1 && path != pat {
            return None;
        }
        if anchored && parts.len() > 1 && !path.ends_with(parts[parts.len() - 1]) {
            return None;
        }
        Some(pattern.len())
    }

    pub fn can_fetch(&self, agent_token: &str, url: &str) -> bool {
        let token = agent_token.to_lowercase();
        let specific: Vec<&(Vec<String>, Vec<(bool, String)>)> =
            self.groups.iter().filter(|g| g.0.iter().any(|a| a != "*" && token.contains(a.as_str()))).collect();
        let chosen: Vec<&(Vec<String>, Vec<(bool, String)>)> =
            if specific.is_empty() { self.groups.iter().filter(|g| g.0.iter().any(|a| a == "*")).collect() } else { specific };
        let path = match url::Url::parse(url) {
            Ok(u) => format!("{}{}", u.path(), u.query().map(|q| format!("?{}", q)).unwrap_or_default()),
            Err(_) => return true,
        };
        let mut best: Option<(usize, bool)> = None;
        for g in chosen {
            for (allow, pat) in &g.1 {
                if let Some(len) = Robots::pattern_len(pat, &path) {
                    best = match best {
                        None => Some((len, *allow)),
                        Some((bl, ba)) if len > bl || (len == bl && *allow && !ba) => Some((len, *allow)),
                        other => other,
                    };
                }
            }
        }
        best.map(|(_, a)| a).unwrap_or(true)
    }
}

// ------------------------------------------------------------------------------------------------ client
pub struct Http {
    agent: ureq::Agent,
    probe_agent: ureq::Agent,
    pub ua: String,
    pub delay: f64,
    respect_robots: bool,
    max_retries: u32,
    t0: Instant,
    last: Mutex<HashMap<String, f64>>,
    robots: Mutex<HashMap<String, Option<Arc<Robots>>>>,
    pub delay_overrides: Mutex<HashMap<String, f64>>,
    fails: Mutex<HashMap<String, u32>>,
    blocked: Mutex<HashSet<String>>,
    pub cancel: Arc<AtomicBool>,
    pub net_ok: AtomicUsize,
    pub net_fail: AtomicUsize,
    pub cache: Option<Arc<ScrapeCache>>,
    pub block_media: bool,
}

type HttpErr = ureq::Error;

impl Http {
    pub fn new(cfg: &Value) -> Http {
        let h = cfg.get("http").cloned().unwrap_or(Value::Null);
        let timeout = jf64(&h, "timeout_seconds", 20.0).max(1.0);
        let contact = {
            let c = jstr(cfg, "contact_email");
            if c.is_empty() { "unknown".to_string() } else { c }
        };
        let ua = format!("EntryLevelJobScraper/1.0 (personal job search; contact: {})", contact);
        let mk = |global: Option<Duration>| -> ureq::Agent {
            let mut b = ureq::Agent::config_builder()
                .http_status_as_error(false)
                .user_agent(ua.clone())
                .max_redirects(10)
                .timeout_connect(Some(Duration::from_secs(10)))
                .timeout_recv_response(Some(Duration::from_secs_f64(timeout)))
                .timeout_recv_body(Some(Duration::from_secs_f64(timeout * 3.0)))
                .max_idle_connections_per_host(8);
            if let Some(g) = global {
                b = b.timeout_global(Some(g));
            }
            ureq::Agent::new_with_config(b.build())
        };
        Http {
            agent: mk(None),
            probe_agent: mk(Some(Duration::from_secs(12))),
            ua,
            delay: jf64(&h, "delay_between_requests_seconds", 1.5),
            respect_robots: jbool(&h, "respect_robots_txt", true),
            max_retries: ji64(&h, "max_retries", 3).clamp(0, 10) as u32,
            t0: Instant::now(),
            last: Mutex::new(HashMap::new()),
            robots: Mutex::new(HashMap::new()),
            delay_overrides: Mutex::new(HashMap::new()),
            fails: Mutex::new(HashMap::new()),
            blocked: Mutex::new(HashSet::new()),
            cancel: Arc::new(AtomicBool::new(false)),
            net_ok: AtomicUsize::new(0),
            net_fail: AtomicUsize::new(0),
            cache: None,
            block_media: jbool(&h, "block_media_in_browser", true),
        }
    }

    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    pub fn is_blocked(&self, host: &str) -> bool {
        self.blocked.lock().unwrap().contains(host)
    }

    pub fn set_delay_override(&self, url: &str, seconds: f64) {
        self.delay_overrides.lock().unwrap().insert(netloc(url), seconds.max(0.2));
    }

    fn sleep_interruptible(&self, secs: f64) {
        let end = Instant::now() + Duration::from_secs_f64(secs.max(0.0));
        while Instant::now() < end && !self.cancelled() {
            std::thread::sleep((end - Instant::now()).min(Duration::from_millis(100)));
        }
    }

    /// Waits so that one host is never hit faster than its delay (several threads share the schedule).
    pub fn throttle(&self, url: &str) {
        let host = netloc(url);
        let now = self.t0.elapsed().as_secs_f64();
        let slot = {
            let delay = self.delay_overrides.lock().unwrap().get(&host).copied().unwrap_or(self.delay);
            let mut last = self.last.lock().unwrap();
            let slot = now.max(last.get(&host).copied().unwrap_or(0.0) + delay);
            last.insert(host, slot);
            slot
        };
        if slot > now {
            self.sleep_interruptible(slot - now);
        }
    }

    /// Circuit breaker: after 5 failures in a row, stop wasting time on that host for this run.
    pub fn note(&self, host: &str, ok: bool) {
        if ok {
            self.net_ok.fetch_add(1, Ordering::Relaxed);
            self.fails.lock().unwrap().insert(host.to_string(), 0);
            return;
        }
        self.net_fail.fetch_add(1, Ordering::Relaxed);
        let n = {
            let mut f = self.fails.lock().unwrap();
            let e = f.entry(host.to_string()).or_insert(0);
            *e += 1;
            *e
        };
        if n >= 5 && self.blocked.lock().unwrap().insert(host.to_string()) {
            error!("Too many failed requests to {} - giving up on it for this run (check your internet, or the site may be blocking the script).", host);
        }
    }

    fn do_request(&self, agent: &ureq::Agent, method: &str, url: &str, headers: &[(String, String)], body: Option<&Value>) -> Result<Resp, HttpErr> {
        let mut resp = match (method, body) {
            ("POST", Some(b)) => {
                let mut r = agent.post(url);
                for (k, v) in headers {
                    r = r.header(k, v);
                }
                r.header("Accept-Language", "en,fa;q=0.8,tr;q=0.6").send_json(b)?
            }
            ("POST", None) => agent.post(url).header("Accept-Language", "en,fa;q=0.8,tr;q=0.6").send_empty()?,
            ("HEAD", _) => {
                let mut r = agent.head(url);
                for (k, v) in headers {
                    r = r.header(k, v);
                }
                r.header("Accept-Language", "en,fa;q=0.8,tr;q=0.6").call()?
            }
            _ => {
                let mut r = agent.get(url);
                for (k, v) in headers {
                    r = r.header(k, v);
                }
                r.header("Accept-Language", "en,fa;q=0.8,tr;q=0.6").call()?
            }
        };
        let status = resp.status().as_u16();
        let final_url = resp.get_uri().to_string();
        let content_type = resp.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
        let retry_after = resp.headers().get("retry-after").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
        let body = if method == "HEAD" { Vec::new() } else { resp.body_mut().with_config().limit(40 * 1024 * 1024).read_to_vec()? };
        Ok(Resp { status, body, url: final_url, content_type, retry_after })
    }

    /// One GET with urllib3-style retries (connection / read errors and HTTP 500/502/503/504).
    fn get_retrying(&self, url: &str, headers: &[(String, String)]) -> Result<Resp, String> {
        let mut used = 0u32;
        loop {
            if self.cancelled() {
                return Err("cancelled".into());
            }
            match self.do_request(&self.agent, "GET", url, headers, None) {
                Ok(r) if matches!(r.status, 500 | 502 | 503 | 504) => {
                    if used >= self.max_retries {
                        return Err(format!("RetryError (HTTP {})", r.status));
                    }
                }
                Ok(r) => return Ok(r),
                Err(e) => {
                    let transient = matches!(e, ureq::Error::Io(_) | ureq::Error::Timeout(_) | ureq::Error::ConnectionFailed | ureq::Error::HostNotFound);
                    if !transient || used >= self.max_retries.min(2) {
                        return Err(error_kind(&e).to_string());
                    }
                }
            }
            used += 1;
            let backoff = if used <= 1 { 0.0 } else { (2f64).powi(used as i32 - 1).min(120.0) };
            self.sleep_interruptible(backoff);
        }
    }

    pub fn get(&self, url: &str) -> Option<Resp> {
        self.get_with(url, &[])
    }

    pub fn get_with(&self, url: &str, headers: &[(String, String)]) -> Option<Resp> {
        let host = netloc(url);
        if self.is_blocked(&host) || self.cancelled() {
            return None;
        }
        let mut result: Option<Resp> = None;
        for attempt in 1..=2 {
            self.throttle(url);
            match self.get_retrying(url, headers) {
                Ok(r) => {
                    if r.status == 429 && attempt == 1 {
                        // rate limited: wait only if the wait is short
                        if let Ok(wait) = r.retry_after.trim().parse::<f64>() {
                            if wait <= 30.0 {
                                warn!("{} is rate-limiting us - waiting {:.0} s", host, wait);
                                self.sleep_interruptible(wait);
                                continue;
                            }
                        }
                    }
                    result = Some(r);
                    break;
                }
                Err(kind) => {
                    warn!("GET failed {} -> {}", url, kind);
                    self.note(&host, false);
                    return None;
                }
            }
        }
        let r = result?;
        self.note(&host, r.status < 500 && r.status != 429);
        Some(r)
    }

    /// POST with a JSON body (no retries, like the original).
    pub fn post_json(&self, url: &str, body: &Value, headers: &[(String, String)]) -> Option<Resp> {
        let host = netloc(url);
        if self.is_blocked(&host) || self.cancelled() {
            return None;
        }
        self.throttle(url);
        match self.do_request(&self.agent, "POST", url, headers, Some(body)) {
            Ok(r) => {
                self.note(&host, r.status < 500 && r.status != 429);
                Some(r)
            }
            Err(e) => {
                warn!("POST failed {} -> {}", url, error_kind(&e));
                self.note(&host, false);
                None
            }
        }
    }

    /// POST that bypasses throttling and robots (a translation server the user runs themselves). 60 s timeout.
    pub fn post_json_raw(&self, url: &str, body: &Value) -> Result<Resp, String> {
        let agent = {
            let b = ureq::Agent::config_builder().http_status_as_error(false).user_agent(self.ua.clone()).timeout_global(Some(Duration::from_secs(60)));
            ureq::Agent::new_with_config(b.build())
        };
        self.do_request(&agent, "POST", url, &[], Some(body)).map_err(|e| error_kind(&e).to_string())
    }

    fn robots_for(&self, origin: &str) -> Option<Arc<Robots>> {
        if let Some(r) = self.robots.lock().unwrap().get(origin) {
            return r.clone();
        }
        let r = self.get(&format!("{}/robots.txt", origin));
        let rp: Option<Arc<Robots>> = match r {
            None => None, // network failure or no answer: treated as "no robots.txt"
            Some(r) if (400..500).contains(&r.status) => None, // no robots.txt -> everything allowed (RFC 9309)
            Some(r) if r.status >= 500 => Some(Arc::new(Robots::disallow_all())), // server error -> be conservative
            Some(r) => Some(Arc::new(Robots::parse(&r.text()))),
        };
        self.robots.lock().unwrap().insert(origin.to_string(), rp.clone());
        rp
    }

    pub fn robots_allowed(&self, url: &str) -> bool {
        if !self.respect_robots {
            return true;
        }
        let Ok(u) = url::Url::parse(url) else { return true };
        let origin = format!("{}://{}", u.scheme(), netloc(url));
        match self.robots_for(&origin) {
            None => true,
            Some(rp) => rp.can_fetch(self.ua.split('/').next().unwrap_or(""), url),
        }
    }

    /// Check that a link is alive. Returns (status, final_url, error_kind).
    pub fn probe(&self, url: &str) -> (Option<u16>, Option<String>, &'static str) {
        if self.is_blocked(&netloc(url)) || self.cancelled() {
            return (None, None, "connection");
        }
        for method in ["HEAD", "GET"] {
            self.throttle(url);
            let mut req = if method == "HEAD" { self.probe_agent.head(url) } else { self.probe_agent.get(url) };
            req = req.header("Accept-Language", "en,fa;q=0.8,tr;q=0.6");
            match req.call() {
                Ok(r) => {
                    let code = r.status().as_u16();
                    let fin = r.get_uri().to_string();
                    if method == "HEAD" && code >= 400 {
                        continue; // many servers reject HEAD -> retry with GET
                    }
                    return (Some(code), Some(fin), "");
                }
                Err(e) => {
                    return (None, None, error_kind(&e));
                }
            }
        }
        (None, None, "timeout")
    }
}

pub fn error_kind(e: &ureq::Error) -> &'static str {
    match e {
        ureq::Error::Tls(_) | ureq::Error::Rustls(_) | ureq::Error::Pem(_) => "ssl",
        ureq::Error::HostNotFound | ureq::Error::ConnectionFailed => "connection",
        ureq::Error::Io(io) if io.kind() == std::io::ErrorKind::TimedOut => "timeout",
        ureq::Error::Io(_) => "connection",
        _ => "timeout",
    }
}
