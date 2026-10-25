//! A thin wrapper over two regex engines so the rest of the program can use it like Python's `re`.
//!  - Patterns the fast `regex` crate can compile run there (microseconds per page).
//!  - Patterns that need look-around / back-references run on `fancy-regex` (backtracking, much slower). For those a
//!    "relaxed" copy of the pattern (without the negative look-arounds, which can only widen what matches) is tried
//!    first, so the slow engine only runs on text that has a real chance of matching.
//! A pattern that hits the backtracking limit simply counts as "no match" - it can never hang or crash a scan.
use fancy_regex::{Regex, RegexBuilder};

#[derive(Clone, Debug)]
enum Eng {
    Fast(regex::Regex),
    Slow { f: Regex, pre: Option<regex::Regex> },
}

#[derive(Clone, Debug)]
pub struct Rx(Eng);

/// One match: byte range and text, like `regex::Match`.
#[derive(Clone, Copy, Debug)]
pub struct Match<'t> {
    s: &'t str,
    a: usize,
    b: usize,
}
impl<'t> Match<'t> {
    pub fn start(&self) -> usize {
        self.a
    }
    pub fn end(&self) -> usize {
        self.b
    }
    pub fn as_str(&self) -> &'t str {
        &self.s[self.a..self.b]
    }
}

/// The groups of one match.
#[derive(Debug)]
pub struct Captures<'t> {
    s: &'t str,
    g: Vec<Option<(usize, usize)>>,
}
impl<'t> Captures<'t> {
    pub fn get(&self, i: usize) -> Option<Match<'t>> {
        self.g.get(i).copied().flatten().map(|(a, b)| Match { s: self.s, a, b })
    }
}
impl<'t> std::ops::Index<usize> for Captures<'t> {
    type Output = str;
    fn index(&self, i: usize) -> &str {
        self.get(i).map(|m| m.as_str()).unwrap_or("")
    }
}

const STRIP: [&str; 6] = [r"(?<![\d.])", r"(?<![\w@./])", r"(?<![\w/.:@+\-])", r"(?<!\w)", r"(?!\w)", r"(?!\s+gen)"];

/// Built-in patterns only: `(?<!\w)word` is `\b{start}word` when the word starts with a letter (true for every built-in
/// keyword), and `word(?!\w)` is `word\b{end}`. These forms run on the fast engine instead of the backtracking one.
fn fast(p: &str) -> String {
    p.replace(r"(?<!\w)", r"\b{start}").replace(r"(?!\w)", r"\b{end}").replace(r"\blead\b(?!\s+gen)", r"\blead\b(?:$|[^\s]|\s+(?:$|[^g\s]|g(?:$|[^e])|ge(?:$|[^n])))")
}

impl Rx {
    /// Case-sensitive.
    pub fn new(pattern: &str) -> Result<Rx, String> {
        if let Ok(r) = regex::RegexBuilder::new(pattern).size_limit(256 << 20).dfa_size_limit(256 << 20).build() {
            return Ok(Rx(Eng::Fast(r)));
        }
        let f = RegexBuilder::new(pattern).backtrack_limit(1_000_000).build().map_err(|e| e.to_string())?;
        let mut relaxed = pattern.to_string();
        for s in STRIP {
            relaxed = relaxed.replace(s, "");
        }
        let pre = if relaxed != pattern { regex::RegexBuilder::new(&relaxed).size_limit(256 << 20).dfa_size_limit(256 << 20).build().ok() } else { None };
        Ok(Rx(Eng::Slow { f, pre }))
    }
    /// Case-insensitive (Python's re.IGNORECASE).
    pub fn ci(pattern: &str) -> Result<Rx, String> {
        Rx::new(&format!("(?i){}", pattern))
    }
    /// For built-in patterns that are known to be valid.
    pub fn lit(pattern: &str) -> Rx {
        Rx::new(&fast(pattern)).unwrap_or_else(|e| panic!("built-in pattern {:?} is invalid: {}", pattern, e))
    }
    pub fn lit_ci(pattern: &str) -> Rx {
        Rx::ci(&fast(pattern)).unwrap_or_else(|e| panic!("built-in pattern {:?} is invalid: {}", pattern, e))
    }
    /// The plain look-around engine without any speed-up (used to check that they change nothing).
    #[cfg(test)]
    pub fn reference_ci(pattern: &str) -> Rx {
        let f = RegexBuilder::new(&format!("(?i){}", pattern)).backtrack_limit(1_000_000).build().unwrap();
        Rx(Eng::Slow { f, pre: None })
    }
    fn skip(&self, text: &str) -> bool {
        matches!(&self.0, Eng::Slow { pre: Some(p), .. } if !p.is_match(text))
    }
    pub fn is_match(&self, text: &str) -> bool {
        match &self.0 {
            Eng::Fast(r) => r.is_match(text),
            Eng::Slow { f, .. } => !self.skip(text) && f.is_match(text).unwrap_or(false),
        }
    }
    pub fn find<'t>(&self, text: &'t str) -> Option<Match<'t>> {
        match &self.0 {
            Eng::Fast(r) => r.find(text).map(|m| Match { s: text, a: m.start(), b: m.end() }),
            Eng::Slow { f, .. } => {
                if self.skip(text) {
                    return None;
                }
                f.find(text).ok().flatten().map(|m| Match { s: text, a: m.start(), b: m.end() })
            }
        }
    }
    pub fn captures<'t>(&self, text: &'t str) -> Option<Captures<'t>> {
        match &self.0 {
            Eng::Fast(r) => r.captures(text).map(|c| Captures { s: text, g: c.iter().map(|m| m.map(|m| (m.start(), m.end()))).collect() }),
            Eng::Slow { f, .. } => {
                if self.skip(text) {
                    return None;
                }
                f.captures(text).ok().flatten().map(|c| Captures { s: text, g: c.iter().map(|m| m.map(|m| (m.start(), m.end()))).collect() })
            }
        }
    }
    pub fn captures_all<'t>(&self, text: &'t str) -> Vec<Captures<'t>> {
        match &self.0 {
            Eng::Fast(r) => r.captures_iter(text).map(|c| Captures { s: text, g: c.iter().map(|m| m.map(|m| (m.start(), m.end()))).collect() }).collect(),
            Eng::Slow { f, .. } => {
                let mut out = Vec::new();
                if self.skip(text) {
                    return out;
                }
                for c in f.captures_iter(text) {
                    match c {
                        Ok(c) => out.push(Captures { s: text, g: c.iter().map(|m| m.map(|m| (m.start(), m.end()))).collect() }),
                        Err(_) => break,
                    }
                }
                out
            }
        }
    }
    pub fn find_iter<'t>(&self, text: &'t str) -> Vec<Match<'t>> {
        match &self.0 {
            Eng::Fast(r) => r.find_iter(text).map(|m| Match { s: text, a: m.start(), b: m.end() }).collect(),
            Eng::Slow { f, .. } => {
                let mut out = Vec::new();
                if self.skip(text) {
                    return out;
                }
                for m in f.find_iter(text) {
                    match m {
                        Ok(m) => out.push(Match { s: text, a: m.start(), b: m.end() }),
                        Err(_) => break,
                    }
                }
                out
            }
        }
    }
    /// Python's `re.sub(pattern, repl, text)` with a literal replacement.
    pub fn replace_all(&self, text: &str, repl: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut last = 0;
        for m in self.find_iter(text) {
            out.push_str(&text[last..m.start()]);
            out.push_str(repl);
            last = m.end();
        }
        out.push_str(&text[last..]);
        out
    }
    /// Python's `re.findall` for patterns WITHOUT groups: every whole match.
    pub fn find_all_str(&self, text: &str) -> Vec<String> {
        self.find_iter(text).into_iter().map(|m| m.as_str().to_string()).collect()
    }
}

pub fn escape(s: &str) -> String {
    fancy_regex::escape(s).into_owned()
}

#[cfg(test)]
mod tests {
    use super::Rx;
    use crate::lexdata as d;

    /// The speed-ups must give exactly the same answers as the plain look-around patterns.
    #[test]
    fn fast_forms_equal_reference() {
        let corpus = [
            "We need a fresh graduate. No experience required. تجربه لازم نیست و بدون سابقه کار. tecrübe aranmaz.",
            "Remote work possible, work from home. Not remote. on-site only. uzaktan çalışma",
            "Minimum 2-3 years of experience; 2.5 years; 10 yrs; 3+ years; سال ۵ تا ۷ سال; 1 yil",
            "Lead generation specialist, team lead, lead, Lead  Gen. lead. (lead) lead gen",
            "کارمند جدید دورکاری، کارآموز، مبتدی، junior accountant, intern, stajyer, yeni mezun",
            "Contact @abc_defgh or +93 700 123 456 / 0700123456; mail me; apply at https://x.example/jobs",
            "",
            "تازه کار, بی تجربه, tecrübesiz deneyimsiz, sifir tecrübe",
        ];
        let mut pats: Vec<&str> = d::NONE_RAW.to_vec();
        pats.extend([d::REMOTE_YES, d::REMOTE_NO, d::EXP_RANGE, d::EXP_SINGLE, d::HANDLE_RE, d::PHONE_RE]);
        for pat in pats {
            let (a, b) = (Rx::lit_ci(pat), Rx::reference_ci(pat));
            for t in corpus {
                let tl = crate::text::normalize(t);
                for t in [t, tl.as_str()] {
                    assert_eq!(a.is_match(t), b.is_match(t), "is_match differs for {:?} on {:?}", pat, t);
                    assert_eq!(a.find_iter(t).iter().map(|m| (m.start(), m.end())).collect::<Vec<_>>(), b.find_iter(t).iter().map(|m| (m.start(), m.end())).collect::<Vec<_>>(), "find_iter differs for {:?} on {:?}", pat, t);
                }
            }
        }
        // the excluded-title alternation with its "lead" special case
        let pats: Vec<String> = d::EXCL_TITLE_EN.iter().map(|s| s.to_string()).collect();
        let joined = pats.join("|");
        let (a, b) = (Rx::lit_ci(&joined), Rx::reference_ci(&joined));
        for t in corpus.iter().chain(["Team Lead", "Lead Generation Manager", "lead gen", "Lead", "Senior Engineer", "Lead Developer"].iter()) {
            assert_eq!(a.is_match(t), b.is_match(t), "exclude differs on {:?}", t);
        }
    }

    #[test]
    fn keyword_first_char_is_word_char() {
        for k in d::ENTRY_FA.iter().chain(d::ENTRY_TR).chain(d::INTERN_FA).chain(d::INTERN_TR).chain(d::JUNIOR_FA).chain(d::JUNIOR_TR).chain(d::JOB_INTENT_FA).chain(d::JOB_INTENT_TR).chain(d::EXCL_TITLE_FA).chain(d::EXCL_TITLE_TR) {
            let n = crate::text::normalize(k);
            let c = n.chars().next().unwrap();
            assert!(c.is_alphanumeric(), "keyword {:?} starts with a non-word character", k);
        }
    }
}
