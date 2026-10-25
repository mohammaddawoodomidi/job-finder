//! Optional machine translation of Persian/Turkish posts (only the "libretranslate" backend: your own server).
use crate::http::Http;
use crate::text::{jstr, jbool};
use crate::util::sha1_hex;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

pub struct Translator {
    backend: String,
    cfg: Value,
    cache_path: PathBuf,
    cache: Mutex<HashMap<String, String>>,
}

pub fn chunks(text: &str, size: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut buf = String::new();
    for line in text.lines() {
        if buf.chars().count() + line.chars().count() + 1 > size && !buf.is_empty() {
            out.push(std::mem::take(&mut buf));
        }
        buf.push_str(line);
        buf.push('\n');
        while buf.chars().count() > size {
            let cut = buf.char_indices().nth(size).map(|(i, _)| i).unwrap_or(buf.len());
            out.push(buf[..cut].to_string());
            buf = buf[cut..].to_string();
        }
    }
    if !buf.trim().is_empty() {
        out.push(buf);
    }
    out
}

impl Translator {
    pub fn new(tcfg: &Value, base: &std::path::Path) -> Translator {
        let mut backend = { let b = jstr(tcfg, "backend"); if b.is_empty() { "none".to_string() } else { b } };
        if backend == "argos" {
            warn!("The offline 'argos' translator is not part of this edition. Use 'libretranslate' (your own server) or 'none'. Translation is off.");
            backend = "none".into();
        } else if backend != "none" && backend != "libretranslate" {
            backend = "none".into();
        }
        let rel = { let c = jstr(tcfg, "cache_file"); if c.is_empty() { "cache/translations.json".to_string() } else { c } };
        let cache_path = base.join(rel);
        let mut cache = HashMap::new();
        if backend != "none" {
            if let Ok(s) = std::fs::read_to_string(&cache_path) {
                if let Ok(Value::Object(m)) = serde_json::from_str::<Value>(&s) {
                    for (k, v) in m {
                        if let Some(t) = v.as_str() {
                            cache.insert(k, t.to_string());
                        }
                    }
                }
            }
        }
        Translator { backend, cfg: tcfg.clone(), cache_path, cache: Mutex::new(cache) }
    }

    pub fn enabled(&self) -> bool {
        self.backend != "none"
    }

    pub fn save(&self) {
        if !self.enabled() {
            return;
        }
        if let Some(d) = self.cache_path.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        let m: serde_json::Map<String, Value> = self.cache.lock().unwrap().iter().map(|(k, v)| (k.clone(), Value::String(v.clone()))).collect();
        let _ = std::fs::write(&self.cache_path, Value::Object(m).to_string());
    }

    pub fn to_english(&self, http: &Http, text: &str, src: &str) -> String {
        let text = text.trim();
        if text.is_empty() || !self.enabled() || !(src == "fa" || src == "tr") {
            return String::new();
        }
        let key = sha1_hex(&format!("{}|{}|{}", self.backend, src, text));
        if let Some(v) = self.cache.lock().unwrap().get(&key) {
            return v.clone();
        }
        let out = match self.libre(http, text, src) {
            Ok(o) => o,
            Err(e) => {
                warn!("Translation failed ({}): {}", self.backend, e);
                String::new()
            }
        };
        if !out.is_empty() {
            self.cache.lock().unwrap().insert(key, out.clone());
        }
        out
    }

    fn libre(&self, http: &Http, text: &str, src: &str) -> Result<String, String> {
        let base = jstr(&self.cfg, "libretranslate_url");
        let url = format!("{}/translate", base.trim_end_matches('/'));
        let mut parts = Vec::new();
        for chunk in chunks(text, 1500) {
            let mut payload = json!({"q": chunk, "source": src, "target": "en", "format": "text"});
            let key = jstr(&self.cfg, "libretranslate_api_key");
            if !key.is_empty() {
                payload["api_key"] = Value::String(key);
            }
            let r = http.post_json_raw(&url, &payload)?;
            if r.status != 200 {
                let t = r.text();
                return Err(format!("LibreTranslate HTTP {}: {}", r.status, crate::util::trunc(&t, 120)));
            }
            parts.push(r.json().and_then(|v| v.get("translatedText").and_then(Value::as_str).map(str::to_string)).unwrap_or_default());
        }
        let _ = jbool(&self.cfg, "x", false);
        Ok(parts.join("\n"))
    }
}
