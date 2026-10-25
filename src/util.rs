//! Locations, logging and small shared helpers.
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock, RwLock};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const APP_FOLDER: &str = "JobFinder";
pub const HARD_MAX_DAYS: f64 = 7.0;

// ------------------------------------------------------------------------------------------------ locations
/// The folder of the program (.exe).
pub fn app_dir() -> PathBuf {
    std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)).unwrap_or_else(|| PathBuf::from("."))
}

fn is_writable(dir: &Path) -> bool {
    let probe = dir.join(format!(".jf_write_test_{}", std::process::id()));
    match File::create(&probe) {
        Ok(_) => {
            let _ = fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// Where config.json, output/, cache/ and logs/ live: beside the program (portable); if that folder is read-only,
/// the user's own data folder. JOBFINDER_HOME overrides both (tests, special setups).
pub fn default_data_dir() -> PathBuf {
    let p = match std::env::var("JOBFINDER_HOME") {
        Ok(v) if !v.is_empty() => PathBuf::from(v),
        _ => {
            let a = app_dir();
            if is_writable(&a) {
                a
            } else if cfg!(windows) {
                let root = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(|| home().join("AppData").join("Local"));
                root.join(APP_FOLDER)
            } else {
                let root = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).unwrap_or_else(|| home().join(".local").join("share"));
                root.join(APP_FOLDER.to_lowercase())
            }
        }
    };
    let _ = fs::create_dir_all(&p);
    p
}

fn home() -> PathBuf {
    std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))
}

static BASE: OnceLock<RwLock<PathBuf>> = OnceLock::new();
fn base_lock() -> &'static RwLock<PathBuf> {
    BASE.get_or_init(|| RwLock::new(default_data_dir()))
}
pub fn base() -> PathBuf {
    base_lock().read().unwrap().clone()
}
pub fn set_base(p: PathBuf) {
    let _ = fs::create_dir_all(&p);
    *base_lock().write().unwrap() = p;
}
pub fn config_path() -> PathBuf {
    base().join("config.json")
}

/// Writes to `<name>.tmp` first and renames it over the target, so a crash never leaves a half-written file.
pub fn write_atomic<F>(path: &Path, writer: F) -> std::io::Result<()>
where
    F: FnOnce(&Path) -> std::io::Result<()>,
{
    let mut name = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(".tmp");
    let tmp = path.with_file_name(name);
    writer(&tmp)?;
    fs::rename(&tmp, path)
}

// ------------------------------------------------------------------------------------------------ logging
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Level {
    Info,
    Warning,
    Error,
}
impl Level {
    fn name(self) -> &'static str {
        match self {
            Level::Info => "INFO",
            Level::Warning => "WARNING",
            Level::Error => "ERROR",
        }
    }
}

/// The lines the page shows under "Show details".
pub struct LogBuf {
    pub lines: Vec<String>,
    pub offset: usize, // number of lines dropped from the front
}
pub static LOG: Mutex<LogBuf> = Mutex::new(LogBuf { lines: Vec::new(), offset: 0 });
static FILE: Mutex<Option<(PathBuf, File)>> = Mutex::new(None);
static QUIET: AtomicBool = AtomicBool::new(false);

pub fn set_quiet(q: bool) {
    QUIET.store(q, Ordering::Relaxed);
}

pub fn clear_log() {
    let mut l = LOG.lock().unwrap();
    l.lines.clear();
    l.offset = 0;
}

pub fn add_line(line: String) {
    let mut l = LOG.lock().unwrap();
    l.lines.push(line);
    if l.lines.len() > 5000 {
        // keep memory bounded
        l.lines.drain(..1000);
        l.offset += 1000;
    }
}

pub fn init_logging() {
    let dir = base().join("logs");
    let _ = fs::create_dir_all(&dir);
    let path = dir.join("run.log");
    if let Ok(f) = OpenOptions::new().create(true).append(true).open(&path) {
        *FILE.lock().unwrap() = Some((path, f));
    }
}

fn rotate(path: &Path) {
    for i in (1..=3).rev() {
        let from = if i == 1 { path.to_path_buf() } else { PathBuf::from(format!("{}.{}", path.display(), i - 1)) };
        let to = PathBuf::from(format!("{}.{}", path.display(), i));
        if from.exists() {
            let _ = fs::remove_file(&to);
            let _ = fs::rename(&from, &to);
        }
    }
}

pub fn log_at(level: Level, msg: String) {
    let stamp = chrono::Local::now().format("%H:%M:%S");
    let full = format!("{} {:<7} {}", stamp, level.name(), msg);
    // the page shows the first line only for everything below ERROR
    let shown = if level < Level::Error { full.lines().next().unwrap_or("").to_string() } else { full.clone() };
    add_line(shown);
    if let Ok(mut g) = FILE.lock() {
        if let Some((path, f)) = g.as_mut() {
            let _ = writeln!(f, "{}", full);
            if f.metadata().map(|m| m.len() > 1_000_000).unwrap_or(false) {
                let p = path.clone();
                *g = None;
                rotate(&p);
                if let Ok(nf) = OpenOptions::new().create(true).append(true).open(&p) {
                    *g = Some((p, nf));
                }
            }
        }
    }
    if !QUIET.load(Ordering::Relaxed) {
        let _ = writeln!(std::io::stdout(), "{}", full);
    }
}

#[macro_export]
macro_rules! info { ($($a:tt)*) => { $crate::util::log_at($crate::util::Level::Info, format!($($a)*)) } }
#[macro_export]
macro_rules! warn { ($($a:tt)*) => { $crate::util::log_at($crate::util::Level::Warning, format!($($a)*)) } }
#[macro_export]
macro_rules! error { ($($a:tt)*) => { $crate::util::log_at($crate::util::Level::Error, format!($($a)*)) } }

// ------------------------------------------------------------------------------------------------ misc
pub fn sha1_hex(s: &str) -> String {
    use sha1::{Digest, Sha1};
    let mut h = Sha1::new();
    h.update(s.as_bytes());
    h.finalize().iter().map(|b| format!("{:02x}", b)).collect()
}

/// Python's `str[:n]` on characters (never splits a UTF-8 sequence).
pub fn trunc(s: &str, n: usize) -> String {
    match s.char_indices().nth(n) {
        Some((i, _)) => s[..i].to_string(),
        None => s.to_string(),
    }
}

/// Python's `s[a:b]` for characters, clamped.
pub fn char_slice(s: &str, a: usize, b: usize) -> &str {
    let start = s.char_indices().nth(a).map(|(i, _)| i).unwrap_or(s.len());
    let end = s.char_indices().nth(b).map(|(i, _)| i).unwrap_or(s.len());
    if start >= end {
        ""
    } else {
        &s[start..end]
    }
}

/// Rounds like Python's `round(x, 1)`.
pub fn round1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

pub fn now_ts() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}
