//! Local SQLite memory shared between runs: job pages (compressed), link checks and "seen" openings.
//! Same file format as the Python edition (details/probes/seen tables, zlib-compressed JSON).
use crate::util::{now_ts, sha1_hex};
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::Compression;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

const DETAIL_KEYS: [&str; 8] = [
    "detail_text_start",
    "detail_text_end",
    "detail_description_selector",
    "detail_date_regex",
    "prefer_detail_date",
    "render",
    "detail_wait_selector",
    "detail_wait_ms",
];
const DEFINITIVE: [&str; 3] = ["verified", "dead", "reachable"]; // a timeout says nothing about the link
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS details (key TEXT PRIMARY KEY, t REAL NOT NULL, data BLOB NOT NULL);
CREATE TABLE IF NOT EXISTS probes (url TEXT PRIMARY KEY, status TEXT NOT NULL, final TEXT NOT NULL, t REAL NOT NULL);
CREATE TABLE IF NOT EXISTS seen (key TEXT PRIMARY KEY, t REAL NOT NULL);
";

fn remove_db_files(path: &Path) {
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let _ = std::fs::remove_file(format!("{}{}", path.display(), suffix));
    }
}

pub struct ScrapeCache {
    db: Mutex<Connection>,
    pub use_pages: bool,
    detail_ttl: f64,
    probe_ttl: f64,
    seen_ttl: f64,
    pub hits: AtomicUsize,
    pub misses: AtomicUsize,
}

fn open(path: &Path) -> rusqlite::Result<Connection> {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let mut attempt = 1;
    loop {
        let r = Connection::open(path).and_then(|db| {
            db.busy_timeout(std::time::Duration::from_secs(30))?;
            let _: String = db.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))?;
            db.execute_batch("PRAGMA synchronous=NORMAL;")?;
            db.execute_batch(SCHEMA)?;
            Ok(db)
        });
        match r {
            Ok(db) => return Ok(db),
            Err(e) => {
                if attempt == 2 {
                    return Err(e);
                }
                warn!("The page cache could not be read; starting a new one.");
                remove_db_files(path);
                attempt += 1;
            }
        }
    }
}

impl ScrapeCache {
    pub fn new(path: &Path, detail_days: f64, probe_hours: f64, use_pages: bool) -> rusqlite::Result<ScrapeCache> {
        let c = ScrapeCache {
            db: Mutex::new(open(path)?),
            use_pages,
            detail_ttl: detail_days * 86400.0,
            probe_ttl: probe_hours * 3600.0,
            seen_ttl: 45.0 * 86400.0,
            hits: AtomicUsize::new(0),
            misses: AtomicUsize::new(0),
        };
        c.prune();
        Ok(c)
    }

    fn prune(&self) {
        let now = now_ts();
        let db = self.db.lock().unwrap();
        for (table, ttl) in [("details", self.detail_ttl), ("probes", self.probe_ttl), ("seen", self.seen_ttl)] {
            let _ = db.execute(&format!("DELETE FROM {} WHERE t < ?1", table), params![now - ttl]);
        }
    }

    /// `site|href|fingerprint of the settings that shape the page's text`
    pub fn detail_key(site: &Value, href: &str) -> String {
        let vals: Vec<Value> = DETAIL_KEYS.iter().map(|k| site.get(*k).cloned().unwrap_or(Value::Null)).collect();
        let fp = sha1_hex(&Value::Array(vals).to_string());
        format!("{}|{}|{}", site.get("name").and_then(Value::as_str).unwrap_or(""), href, &fp[..10])
    }

    pub fn get_detail(&self, key: &str) -> Option<Value> {
        let row: Option<Vec<u8>> = if self.use_pages {
            let db = self.db.lock().unwrap();
            db.query_row("SELECT data FROM details WHERE key = ?1", params![key], |r| r.get(0)).optional().ok().flatten()
        } else {
            None
        };
        let parsed = row.and_then(|blob| {
            let mut s = String::new();
            ZlibDecoder::new(&blob[..]).read_to_string(&mut s).ok()?;
            serde_json::from_str::<Value>(&s).ok()
        });
        match parsed {
            Some(v) => {
                self.hits.fetch_add(1, Ordering::Relaxed);
                Some(v)
            }
            None => {
                self.misses.fetch_add(1, Ordering::Relaxed);
                None
            }
        }
    }

    pub fn put_detail(&self, key: &str, snap: &Value) {
        if !self.use_pages {
            return;
        }
        let mut enc = ZlibEncoder::new(Vec::new(), Compression::new(6));
        if enc.write_all(snap.to_string().as_bytes()).is_err() {
            return;
        }
        let Ok(blob) = enc.finish() else { return };
        let _ = self.db.lock().unwrap().execute("INSERT OR REPLACE INTO details(key, t, data) VALUES (?1, ?2, ?3)", params![key, now_ts(), blob]);
    }

    pub fn get_probe(&self, url: &str) -> Option<(String, String)> {
        let row: Option<(String, String, f64)> = self
            .db
            .lock()
            .unwrap()
            .query_row("SELECT status, final, t FROM probes WHERE url = ?1", params![url], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .optional()
            .ok()
            .flatten();
        match row {
            Some((s, f, t)) if now_ts() - t <= self.probe_ttl => Some((s, f)),
            _ => None,
        }
    }

    pub fn put_probe(&self, url: &str, status: &str, fin: &str) {
        if DEFINITIVE.iter().any(|d| status.starts_with(d)) {
            let _ = self.db.lock().unwrap().execute("INSERT OR REPLACE INTO probes(url, status, final, t) VALUES (?1, ?2, ?3, ?4)", params![url, status, fin, now_ts()]);
        }
    }

    /// groups[i] = every address one opening is known by. Returns [true/false]: never listed by an earlier scan?
    /// The very first scan only builds the memory, so nothing is called new then.
    pub fn classify_new(&self, groups: &[Vec<String>]) -> Vec<bool> {
        let now = now_ts();
        let mut db = self.db.lock().unwrap();
        let baseline = db.query_row("SELECT 1 FROM seen LIMIT 1", [], |r| r.get::<_, i64>(0)).optional().ok().flatten().is_none();
        let mut flags = Vec::with_capacity(groups.len());
        for keys in groups {
            let known = keys.iter().any(|k| db.query_row("SELECT 1 FROM seen WHERE key = ?1", params![k], |r| r.get::<_, i64>(0)).optional().ok().flatten().is_some());
            flags.push(if baseline { false } else { !known });
        }
        if let Ok(tx) = db.transaction() {
            for keys in groups {
                for k in keys {
                    let _ = tx.execute("INSERT OR IGNORE INTO seen(key, t) VALUES (?1, ?2)", params![k, now]);
                }
            }
            let _ = tx.commit();
        }
        flags
    }

    pub fn stats(&self) -> (usize, usize) {
        (self.hits.load(Ordering::Relaxed), self.misses.load(Ordering::Relaxed))
    }

    pub fn save(&self) {
        let _ = self.db.lock().unwrap().execute_batch("PRAGMA wal_checkpoint(PASSIVE);");
    }
}

/// Forget remembered job pages and link checks (the list of already-seen openings is kept).
pub fn clear_page_cache(path: &Path) {
    if !path.is_file() {
        return;
    }
    if let Ok(c) = ScrapeCache::new(path, 1.0, 1.0, true) {
        let db = c.db.lock().unwrap();
        let _ = db.execute("DELETE FROM details", []);
        let _ = db.execute("DELETE FROM probes", []);
    }
}

pub fn cache_size_kb(path: &Path) -> u64 {
    let total: u64 = ["", "-wal"].iter().filter_map(|s| std::fs::metadata(PathBuf::from(format!("{}{}", path.display(), s))).ok()).filter(|m| m.is_file()).map(|m| m.len()).sum();
    (total as f64 / 1024.0).round() as u64
}
