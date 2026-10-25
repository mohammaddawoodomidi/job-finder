//! Detects postings that only women may apply to (English, Persian/Dari, Pashto, Arabic, Turkish).
//!
//! Design: a posting is dropped only when a *clear* "women only" statement is found AND nothing in the text
//! shows that men can apply too. When in doubt the job stays in the list - hiding a job a man could have taken is
//! worse than showing one extra.
//!
//! Kept (not "women only"): "female preferred", "women are encouraged to apply", "male and female", "both genders",
//! "regardless of gender", "not only women", "services for women only" (about clients, not staff), "Women's Rights Officer",
//! and postings that list several positions of which some are for men ("driver: male only; nurse: female only").
use crate::rx::Rx;
use crate::text::normalize;
use std::sync::LazyLock;

#[derive(Clone, Copy, PartialEq)]
enum Tier {
    /// Wording that is about the applicant by itself (needs only the soft / negation / "male too" checks).
    Applicant,
    /// Short phrases such as "women only" that could also describe a service or a place: needs job words nearby.
    Bare,
    /// "Men cannot apply" - already says men are out.
    MaleOut,
}

/// What must NOT follow the match (checked in code, so the patterns stay on the fast regex engine).
#[derive(Clone, Copy, PartialEq)]
enum Tail {
    None,
    /// "women and men", "female/male", "women's ..."
    Male,
    /// Persian "زن / مرد", "زن و مرد"
    Fa,
}

struct Pat {
    rx: Rx,
    tier: Tier,
    tail: Tail,
}
fn p(tier: Tier, pat: &str) -> Pat {
    Pat { rx: Rx::new(&format!("(?i){}", pat)).unwrap_or_else(|e| panic!("bad gender pattern {}: {}", pat, e)), tier, tail: Tail::None }
}
fn pt(tier: Tier, tail: Tail, pat: &str) -> Pat {
    Pat { tail, ..p(tier, pat) }
}
static TAIL_MALE: LazyLock<Rx> = LazyLock::new(|| Rx::lit_ci(r"^\s*(?:and|or|&|/|,|\+|plus|as\s+well\s+as)\s*(?:males?|men|man|m\b)"));
static TAIL_POSS: LazyLock<Rx> = LazyLock::new(|| Rx::lit_ci(r"^'s"));
static TAIL_FA: LazyLock<Rx> = LazyLock::new(|| Rx::lit_ci(r"^\s*[/،,]|^\s+(?:و|یا)\s"));
fn tail_blocks(t: Tail, after: &str) -> bool {
    match t {
        Tail::None => false,
        Tail::Male => TAIL_MALE.is_match(after) || TAIL_POSS.is_match(after),
        Tail::Fa => TAIL_FA.is_match(after),
    }
}

const F: &str = r"(?:females?|women|woman|ladies|lady)";

static PATS: LazyLock<Vec<Pat>> = LazyLock::new(|| {
    use Tier::*;
    let f = F;
    vec![
        // ---------------- English
        p(Bare, &format!(r"\b{f}[\s\-]*only\b")),
        pt(Bare, Tail::Male, &format!(r"\bonly\s+(?:for\s+|to\s+|by\s+|the\s+)?(?:qualified\s+|suitable\s+|eligible\s+|interested\s+|capable\s+|experienced\s+)?{f}\b")),
        p(Applicant, &format!(r"\b{f}\s+(?:candidates?|applicants?|staff|professionals?|persons?|individuals?|job\s*seekers?|nationals?|employees?|workers?)\s+(?:only|exclusively)\b")),
        pt(Applicant, Tail::Male, &format!(r"\bexclusively\s+(?:for\s+|to\s+)?{f}\b")),
        p(Applicant, &format!(r"\b{f}\s+exclusively\b")),
        pt(Applicant, Tail::Male, &format!(r"\b(?:restricted|limited|reserved|exclusive|exclusively\s+open)\s+(?:to|for)\s+(?:only\s+)?{f}\b")),
        pt(Applicant, Tail::Male, &format!(r"\b(?:must|should|has\s+to|have\s+to|needs?\s+to|required\s+to|shall)\s+be\s+(?:a\s+)?{f}\b")),
        pt(Applicant, Tail::Male, &format!(r"\b(?:gender|sex)(?:\s+of\s+(?:the\s+)?(?:candidate|applicant|incumbent))?\s*[:=\-–]\s*(?:females?|women|woman|f)\b")),
        pt(Applicant, Tail::Male, &format!(r"\b(?:gender|sex)\s+{f}\b")),
        p(Applicant, &format!(r"\b{f}(?:\s+(?:candidates?|applicants?|staff))?\s+(?:are|is|will\s+be|shall\s+be)\s+only\s+(?:eligible|allowed|considered|accepted|invited|welcome|encouraged|required)\b")),
        p(MaleOut, r"\b(?:males?|men)(?:\s+(?:candidates?|applicants?|persons?|staff|individuals?))?\s+(?:are\s+not|is\s+not|will\s+not|shall\s+not|should\s+not|must\s+not|may\s+not|cannot|can\s+not|can't|can\s+not|do\s+not|don't|need\s+not|needn't|aren't|won't|not)\s+(?:be\s+)?(?:eligible|allowed|permitted|accepted|considered|shortlisted|entertained|apply|applying|required|welcome|encouraged|invited)\b"),
        p(MaleOut, r"\b(?:males?|men)(?:\s+(?:candidates?|applicants?))?\s+(?:are\s+)?(?:excluded|ineligible|prohibited|barred|not\s+eligible)\b"),
        p(MaleOut, r"\bnot\s+open\s+to\s+(?:males?|men)\b"),
        p(MaleOut, r"\bno\s+(?:males?|men)\s+(?:candidates?\s+|applicants?\s+)?(?:allowed|accepted|eligible)\b"),
        // ---------------- English: hiring wording and eligibility fields
        p(Applicant, &format!(r"\b(?:we\s+(?:need|require|want|are\s+(?:looking\s+for|seeking|hiring|recruiting|searching\s+for)|seek|invite|hire|recruit)|(?:hiring|recruiting|seeking|looking\s+for|required|needed|wanted|vacanc(?:y|ies)\s+for|position\s+for|opening\s+for))\s+(?:an?\s+|one\s+|two\s+|three\s+|four\s+|five\s+|\d+\s+)?(?:qualified\s+|dynamic\s+|experienced\s+|motivated\s+|competent\s+|suitable\s+|talented\s+|energetic\s+|young\s+|fresh\s+|junior\s+)*(?:females?|woman|lady|ladies)\b(?!['-])")),
        p(Applicant, &format!(r"\b(?:vacanc(?:y|ies)|positions?|posts?|jobs?|openings?|opportunit(?:y|ies))\s+(?:is\s+|are\s+)?(?:available\s+|open\s+)?(?:for|to)\s+(?:an?\s+)?{f}\s+(?:only\s+)?(?:candidates?|applicants?|staff|professionals?|persons?|individuals?|\w+)\b(?!\s+and\s+(?:male|men))")),
        p(Applicant, &format!(r"\b(?:open|available|applicable|restricted|limited|reserved|designated|dedicated|intended|meant|suitable|specific(?:ally)?)\s+(?:to|for)\s+(?:only\s+)?{f}\s+(?:candidates?|applicants?|professionals?|staff|persons?|individuals?)\b")),
        p(Applicant, &format!(r"\b(?:eligib\w+|who\s+can\s+apply|applicants?|candidates?|required\s+gender|gender\s+required|gender\s+requirement|gender\s+eligibility)\s*[:=\-–]\s*(?:only\s+)?{f}\b")),
        p(Applicant, &format!(r"\b(?:an?|one|two|three|four|five|\d+)\s+(?:qualified\s+|experienced\s+)?female\s+(?:candidates?|applicants?|professionals?|staff|employees?|enumerators?|teachers?|nurses?|doctors?|midwi(?:fe|ves)|trainers?|facilitators?|officers?|assistants?|workers?|supervisors?|coordinators?|managers?|accountants?|engineers?|counsel+ors?|social\s+workers?|receptionists?|secretar(?:y|ies)|cleaners?|cooks?|translators?|interpreters?|monitors?|surveyors?|mobili[sz]ers?|educators?|instructors?|searchers?|guards?|drivers?|data\s+entry)\b")),
        p(Applicant, &format!(r"\b{f}\s+(?:candidates?|applicants?)\s+(?:are\s+)?(?:required|needed|wanted|requested|invited)\b")),
        // ---------------- Persian / Dari (text is normalised: ی ک unified, ZWNJ -> space, marks removed)
        p(Applicant, r"فقط\s*(?:برای\s*|به\s*)?(?:خانم|بانو|زن|زنان|اناث|دختر)"),
        p(Applicant, r"(?:ویژه|مخصوص|مختص|صرفا|منحصرا|اختصاص\s*(?:دارد|یافته)?)\s*(?:به\s*|برای\s*)?(?:خانم|بانو|زنان|زن\s)"),
        p(Applicant, r"(?:خانم|بانو|زنان)(?:\s*ها)?\s*(?:فقط|صرفا)"),
        pt(Applicant, Tail::Fa, r"جنسیت\s*[:：\-]?\s*(?:زن|خانم|بانو|مونث|اناث)"),
        p(MaleOut, r"(?:آقایان|اقایان|مردان)\s*(?:مجاز|محاز|پذیرفته|قبول|مورد\s*قبول)?\s*(?:ن\s*[یی]ست|نمی\s*(?:شوند|توانند)|نمی\s*باشند)"),
        p(Applicant, r"(?:نیازمند|استخدام|نیاز\s*به|کارمند|منشی|معلم|مدیر|حسابدار|کارشناس|مسئول|اپراتور|فروشنده|پرستار|آرایشگر|داکتر|دکتر)\s*(?:یک\s*)?(?:نفر\s*)?(?:خانم|بانو)(?:\s|$|[.،,:)])"),
        p(Applicant, r"(?:داوطلبان|متقاضیان|کاندیدان|نامزدان|کاندیدها)\s*(?:خانم|زن|بانو)(?:\s*ها)?\s*(?:فقط|صرفا|می\s*توانند|نیاز)"),
        // Pashto / Arabic
        p(Applicant, r"یواز[یې]?\s*ښځ"),
        p(Applicant, r"(?:فقط|حصرا)\s*(?:للاناث|للنساء|للإناث)|(?:للاناث|للنساء|للإناث)\s*فقط"),
        // ---------------- Turkish (normalised: dotless i -> i)
        p(Applicant, r"\b(?:sadece|yalnizca|yalniz|salt)\s+(?:bayan|kadin)"),
        p(Applicant, r"\b(?:bayanlar|kadinlar|bayan|kadin)(?:\s+(?:adaylar?|adaylara|basvurular|personel\w*|eleman\w*|calisan\w*))?\s+(?:sadece|yalnizca)\b"),
        p(Applicant, r"\b(?:bayanlara|kadinlara|bayan\s+adaylara)\s+ozel\b"),
        p(Applicant, r"\b(?:bayan|kadin)\s+(?:eleman|personel|calisan|aday|isci)\w*\s+(?:aran|alin|istenm)\w*"),
        p(MaleOut, r"\berkek(?:ler)?\s+(?:adaylar?|basvuru\w*|personel|eleman|calisan)?\s*(?:alinmayacak\w*|alinmamakta\w*|kabul\s+edilme\w*|basvuramaz\w*|basvuru\s+yapamaz\w*|degerlendirilmeyecek\w*)"),
    ]
});

/// Wording that shows men may apply: if present, the posting is kept (e.g. one ad listing several positions).
static MALE_OK: LazyLock<Rx> = LazyLock::new(|| {
    Rx::lit_ci(concat!(
        r"\b(?:males?|men)[\s\-]*only\b|\bonly\s+(?:for\s+|to\s+)?(?:males?|men)\b|\bmale\s+(?:candidates?|applicants?|staff)\s+only\b",
        r"|فقط\s*(?:برای\s*)?(?:آقا|اقا|مرد)|\bsadece\s+erkek|\byalnizca\s+erkek|\berkek\s+(?:eleman|personel|calisan)\w*\s+aran"
    ))
});

/// Two-sided wording right next to a "women" phrase: "male and female", "both", "m/f" ...
static PAIR: LazyLock<Rx> = LazyLock::new(|| {
    Rx::lit_ci(concat!(
        r"\b(?:males?|men|man)\s*(?:and|or|&|/|,|\+|plus|as\s+well\s+as)\s*(?:females?|women|woman|ladies)\b|\b(?:females?|women|woman|ladies)\s*(?:and|or|&|/|,|\+|plus|as\s+well\s+as)\s*(?:males?|men|man)\b",
        r"|\bboth\s+(?:males?|men|genders?|sexes|male|female)\b|\beither\s+(?:gender|sex)\b|\b[mf]\s*/\s*[fm]\b|\(\s*[mf]\s*/\s*[fm]\s*[/)]|\bm\s*/\s*f\s*/\s*[dx]\b|\bmales?\s+and\s+females?\b",
        r"|(?:مرد|آقا|اقا|مذکر)\s*(?:و|یا|/)\s*(?:زن|خانم|بانو)|(?:زن|خانم|بانو)(?:ان|ها)?\s*(?:و|یا|/)\s*(?:مرد|آقا|اقا)|\berkek\s*(?:ve|veya|/)\s*(?:bayan|kadin)|\b(?:bayan|kadin)\s*(?:ve|veya|/)\s*erkek|\bbay\s+ve\s+bayan"
    ))
});
/// "preferably female", "priority to women": soft wording right BEFORE the phrase ...
static SOFT_BEFORE: LazyLock<Rx> = LazyLock::new(|| Rx::lit_ci(r"(?:prefer\w*|priority|ideally|ideal|desirable|especially|mostly|mainly|given\s+to|ترجیح|اولویت|tercih|oncelik)[^.;!?\n]{0,25}$"));
/// ... or right AFTER it: "female only is preferred", "women - a plus".
static SOFT_AFTER: LazyLock<Rx> = LazyLock::new(|| Rx::lit_ci(r"^[^.;!?\n]{0,25}?(?:(?:is|are|will\s+be|would\s+be)\s+)?(?:preferred|preferable|desirable|a\s+plus|an\s+advantage|an\s+asset|favou?red|prioriti[sz]ed|ترجیح|tercih)"));
static NEG_BEFORE: LazyLock<Rx> = LazyLock::new(|| Rx::lit_ci(r"\b(?:not|never|nor|neither|without|no\s+longer|isn't|aren't|doesn't|don't|rather\s+than|instead\s+of)\b|n't\b|نه\s|نیست|degil"));
static NOT_REQUIRED: LazyLock<Rx> = LazyLock::new(|| Rx::lit_ci(r"^\W*\s*(?:is\s+|are\s+)?not\s+(?:a\s+)?(?:required|necessary|mandatory|essential|requirement)"));
static JOB_WORDS: LazyLock<Rx> = LazyLock::new(|| {
    Rx::lit_ci(r"candidate|applicant|appl(?:y|ication)|position|vacanc|\bpost\b|\bjob\b|\brole\b|\bstaff|recruit|\bhir(?:e|ing)\b|eligib|gender|seeking|employ|opening|\bcv\b|resume|shortlist|incumbent|national|professional|submit|متقاضی|داوطلب|کاندید|استخدام|موقعیت|وظیفه|کارمند|aday|basvuru|personel|eleman|calisan|ilan|pozisyon")
});
static SERVICE_AFTER: LazyLock<Rx> = LazyLock::new(|| {
    Rx::lit_ci(r"^[\s,.:;)\-]*(?:shelters?|cent(?:re|er)s?|safe\s*(?:house|space)s?|hostels?|dormitor\w*|gyms?|classe?s?|school|training|courses?|bath\w*|toilets?|clinics?|hospitals?|beneficiar\w*|survivors?|clients?|programs?|programmes?|projects?|services?|spaces?|sessions?|workshops?|groups?|organi[sz]ation|ngo|association|market|bazaar|park|library|university|college)\b")
});
/// Subject areas: "Female Engagement Team", "Female Genital ..." are about women, not a restriction on who applies.
static TOPIC: LazyLock<Rx> = LazyLock::new(|| Rx::lit_ci(r"^\W*(?:engagement|empowerment|protection|genital|rights|participation|entrepreneur\w*|leadership|headed|refugee\w*|beneficiar\w*|survivor\w*|inclusion|economic|livelihood\w*|literacy|health\s+(?:programme|program)|focal)"));

// ---- strict sentence rule: any sentence that talks about female applicants/staff, with no sign that men may apply
static FEM_WORD: LazyLock<Rx> = LazyLock::new(|| Rx::lit_ci(r#"\b(?:females?|women|woman|ladies|lady)\b|\bbayan\w*|\bkadin\w*|(?:^|[\s(\[«"])(?:خانم|بانو|زنان|مونث|اناث)|(?:^|[\s(\[«"])زن(?:ها)?(?:$|[\s)\].،,:؛])|ښځ"#));
static MALE_WORD: LazyLock<Rx> = LazyLock::new(|| Rx::lit_ci(r"\b(?:males?|men|man|boys?|gentlem[ae]n|husbands?)\b|مرد|آقا|اقا|مذکر|\berkek|\bbay\b"));
static SENT_CUE: LazyLock<Rx> = LazyLock::new(|| {
    Rx::lit_ci(r"candidates?|applicants?|\bappl(?:y|ying|ication)|eligib\w*|\bhir(?:e|ing)\b|recruit\w*|\bposition\b|vacanc\w*|\bpost\b|\bjob\b|\brole\b|\bstaff\b|employee|seeking|looking\s+for|\bneed(?:ed|s)?\b|\brequire\w*|\bmust\b|\bshould\s+be\b|\bonly\b|exclusive\w*|open\s+to|reserved|restricted|\bgender\b|\bsex\b|incumbent|enumerator|applicable|متقاضی|داوطلب|کاندید|استخدام|نیازمند|موقعیت|وظیفه|کارمند|فقط|صرفا|ویژه|مخصوص|لازم|شرایط|جنسیت|\baday|basvuru|personel|eleman|calisan|aranmakta|araniyor|sadece|yalnizca|\bilan\b")
});
static SENT_SOFT: LazyLock<Rx> = LazyLock::new(|| {
    Rx::lit_ci(concat!(
        r"encourag|welcome|prefer|priority|advantage|\bplus\b|equal\s+opportunit|diversity|inclusi|regardless|gender[\s-]*(?:neutral|equal|balance|sensitiv|mainstream|based|violence)|empower|\brights?\b|women'?s\b|womens\b|beneficiar|survivor|\bclients?\b|patients?|protect|shelter|\bprojects?\b|\bprogramm?es?\b|\bservices?\b|assist|community|communities|household|entrepreneur|literacy|reproductive|maternal|\bgirls?\b|\bnot\b|n't|\bno\b|\bnor\b|\bnever\b",
        r"|(?:work(?:ing)?|experience|deal(?:ing)?|engag\w+|interact\w*|communicat\w*|train\w*|mobili[sz]\w*|support\w*|reach\w*|help\w*|serv\w*|focus\w*|target\w*)\s+(?:with\s+|for\s+|of\s+|to\s+|on\s+)?(?:the\s+)?(?:local\s+|rural\s+|vulnerable\s+|young\s+|poor\s+)*(?:women|females?)",
        r"|(?:women|females?)\s+(?:and\s+girls|in\s+(?:the\s+)?\w+|headed|who|living|from|affected)|\b(?:mrs|ms|miss|madam)\b",
        r"|تشویق|ترجیح|اولویت|توانمند|حقوق|پروژه|برنامه|خدمات|حمایت|قربانی|بهره\s*مند|مشتری|نه\s|نیست|خانم\s+(?:[^\s،,.]*\s+)?(?:تماس|ایمیل|شماره|مراجعه)|tercih|oncelik|guclendir|\bhak\b|degil"
    ))
});
static SENT_SPLIT: LazyLock<Rx> = LazyLock::new(|| Rx::lit(r"[.;!?\n•|]+"));

fn strict_sentence(full: &str, blunt: bool) -> Option<String> {
    let mut last = 0;
    let mut check = |sent: &str| -> Option<String> {
        let t = sent.trim();
        if t.len() < 4 || !FEM_WORD.is_match(t) || MALE_WORD.is_match(t) {
            return None;
        }
        if blunt {
            // any mention of female applicants/staff counts, except topics, negations and contact names
            if BLUNT_SOFT.is_match(t) {
                return None;
            }
        } else if SENT_SOFT.is_match(t) || !SENT_CUE.is_match(t) {
            return None;
        }
        Some(format!("sentence \"{}\"", crate::util::trunc(t, 90)))
    };
    for m in SENT_SPLIT.find_iter(full) {
        if let Some(r) = check(&full[last..m.start()]) {
            return Some(r);
        }
        last = m.end();
    }
    check(&full[last..])
}

/// For the early title-only check (skips downloading the job page).
pub fn title_is_female_only(title: &str) -> bool {
    title_hit(&normalize(title)).is_some()
}

// ---- title rules
static T_FEMALE: LazyLock<Rx> = LazyLock::new(|| Rx::lit_ci(r"(?:^|[\s(\[\-–—:/,])(?:females?)(?=[\s)\]\-–—:/,.]|$)"));
static T_PAREN: LazyLock<Rx> = LazyLock::new(|| Rx::lit_ci(r"[(\[]\s*(?:women|woman|ladies|lady|female|females|female\s+only|women\s+only)\s*[)\]]|[\-–—:]\s*(?:women|woman|ladies)\s*$|[(\[]\s*(?:خانم|بانو|بانوان|زنان|زن|مونث)\s*[)\]]|\s(?:خانم)\s*$|[(\[]\s*(?:bayan|kadin)\s*[)\]]|\bbayan\s+(?:eleman|personel|calisan|aday|isci|temsilci)\w*"));
static T_BLOCK: LazyLock<Rx> = LazyLock::new(|| Rx::lit_ci(r"\b(?:males?|men|man|both|gender|encourag\w*|prefer\w*|priority|m\s*/\s*f|f\s*/\s*m)\b|مرد|آقا|erkek|\bbay\b"));
static T_LADY: LazyLock<Rx> = LazyLock::new(|| Rx::lit_ci(r"(?:^|[\s(\[\-–—:/,])(?:lady|ladies)(?=[\s)\]\-–—:/,.]|$)|(?:^|\s)خانم(?=\s|$|[)\]\-–—:/،,.])|\bښځین\w*"));

fn floor(s: &str, mut i: usize) -> usize {
    i = i.min(s.len());
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}
fn ceil(s: &str, mut i: usize) -> usize {
    i = i.min(s.len());
    while !s.is_char_boundary(i) {
        i += 1;
    }
    i
}
fn win(s: &str, a: usize, b: usize, before: usize, after: usize) -> (&str, &str) {
    (&s[floor(s, a.saturating_sub(before))..a], &s[b..ceil(s, b + after)])
}

fn title_hit(title: &str) -> Option<String> {
    if title.is_empty() {
        return None;
    }
    if let Some(m) = T_PAREN.find(title) {
        if !Rx::lit_ci(r"\b(?:males?|men)\b|erkek|مرد").is_match(title) {
            return Some(format!("title says \"{}\"", m.as_str().trim()));
        }
    }
    if let Some(m) = T_LADY.find(title) {
        if !T_BLOCK.is_match(title) {
            return Some(format!("title says \"{}\"", m.as_str().trim()));
        }
    }
    if let Some(m) = T_FEMALE.find(title) {
        let rest = &title[m.end()..];
        let others = format!("{}{}", &title[..m.start()], rest);
        if !T_BLOCK.is_match(&others) && !TOPIC.is_match(rest) {
            return Some("title says \"female\"".to_string());
        }
    }
    None
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Mode {
    /// only explicit "women only" style statements
    Explicit,
    /// + any sentence about female applicants without a sign that men may apply (encouraged / preferred wording is kept)
    Balanced,
    /// + "encouraged" / "preferred" wording counts too: ANY mention of female applicants without a male counterpart rejects the job
    Blunt,
}
impl Mode {
    pub fn from_config(v: &str) -> Mode {
        match v.trim().to_lowercase().as_str() {
            "explicit" | "off" | "false" => Mode::Explicit,
            "balanced" => Mode::Balanced,
            _ => Mode::Blunt,
        }
    }
}

/// Where the job's own text ends and site furniture (other jobs, share buttons) begins.
static TAIL_CUT: LazyLock<Rx> = LazyLock::new(|| Rx::lit_ci(r"related\s+jobs|similar\s+jobs|other\s+jobs|more\s+jobs|recommended\s+jobs|you\s+may\s+also\s+like|share\s+this\s+job|latest\s+jobs|مشاغل\s+مشابه|ilgili\s+ilanlar"));

/// Sentence-level wording that makes a female word harmless (topics, negations, contact names).
static BLUNT_SOFT: LazyLock<Rx> = LazyLock::new(|| {
    Rx::lit_ci(concat!(
        r"empower|\brights?\b|women'?s\b|womens\b|beneficiar|survivor|\bclients?\b|patients?|protect|shelter|\bprojects?\b|\bprogramm?es?\b|\bservices?\b|assist|community|communities|household|entrepreneur|literacy|reproductive|maternal|\bnot\b|n't|\bnever\b|\bnor\b|\bno\b",
        r"|(?:work(?:ing)?|experience|deal(?:ing)?|engag\w+|interact\w*|communicat\w*|train\w*|mobili[sz]\w*|support\w*|reach\w*|help\w*|serv\w*|focus\w*|target\w*)\s+(?:with\s+|for\s+|of\s+|to\s+|on\s+)?(?:the\s+)?(?:local\s+|rural\s+|vulnerable\s+|young\s+|poor\s+)*(?:women|females?)",
        r"|(?:women|females?)\s+(?:and\s+girls|in\s+(?:the\s+)?\w+|headed|who|living|from|affected)|\b(?:mrs|ms|miss|madam)\b",
        r"|توانمند|حقوق|پروژه|برنامه|خدمات|حمایت|قربانی|بهره\s*مند|مشتری|نه\s|نیست|خانم\s+(?:[^\s،,.]*\s+)?(?:تماس|ایمیل|شماره|مراجعه)|oncelik|guclendir|\bhak\b|degil"
    ))
});

/// `Some(reason)` when only women can apply to the posting.
pub fn female_only(title: &str, body: &str, mode: Mode) -> Option<String> {
    let t = normalize(title);
    let mut b = normalize(body);
    if let Some(m) = TAIL_CUT.find(&b) {
        b.truncate(m.start()); // other jobs listed under this one are not part of it
    }
    let full = if b.is_empty() { t.clone() } else { format!("{} \n {}", t, b) };
    if full.trim().is_empty() {
        return None;
    }
    if MALE_OK.is_match(&full) {
        return None; // several positions, some for men: the man can apply to those
    }
    let mut reason = title_hit(&t);
    for pat in PATS.iter() {
        if reason.is_some() {
            break;
        }
        for m in pat.rx.find_iter(&full) {
            let (before, after) = win(&full, m.start(), m.end(), 45, 45);
            if tail_blocks(pat.tail, &full[m.end()..]) {
                continue;
            }
            if pat.tier != Tier::MaleOut {
                if SOFT_BEFORE.is_match(&before[floor(before, before.len().saturating_sub(30))..]) || SOFT_AFTER.is_match(&after[..ceil(after, 40)]) {
                    continue; // "preferably female", "priority to women"
                }
                if NEG_BEFORE.is_match(&before[floor(before, before.len().saturating_sub(32))..]) || NOT_REQUIRED.is_match(after) {
                    continue; // "not only women", "women only is not required"
                }
                // only wording in the SAME clause counts: "...only (male and female staff work separately)" does not
                let (b60, a60) = win(&full, m.start(), m.end(), 60, 60);
                let b60 = &b60[b60.rfind(|c| matches!(c, '.' | ';' | '!' | '?' | '\n' | '(' | ')')).map_or(0, |i| i + 1)..];
                let a60 = &a60[..a60.find(|c| matches!(c, '.' | ';' | '!' | '?' | '\n' | '(' | ')')).unwrap_or(a60.len())];
                if PAIR.is_match(&format!("{}{}{}", b60, m.as_str(), a60)) {
                    continue; // "male and female ... women only" is contradictory: keep the job
                }
            }
            if pat.tier == Tier::Bare {
                if m.start() > 400 {
                    let (b120, a120) = win(&full, m.start(), m.end(), 140, 140);
                    if !JOB_WORDS.is_match(&format!("{}{}{}", b120, m.as_str(), a120)) {
                        continue; // e.g. "a women only gym" far from any job wording
                    }
                }
                let (b120, _) = win(&full, m.start(), m.end(), 14, 0);
                let ctx = &full[floor(&full, m.start().saturating_sub(140))..ceil(&full, m.end() + 140)];
                if SERVICE_AFTER.is_match(&full[m.end()..]) || (Rx::lit_ci(r"\b(?:for|to|serving|serve|support|supports|helping)\s*$").is_match(b120) && Rx::lit_ci(r"beneficiar|clients?|survivors?|patients?|students?|learners?|girls").is_match(ctx)) {
                    continue; // about who the service is for, not who is hired
                }
            }
            reason = Some(format!("\"{}\"", crate::util::trunc(m.as_str().trim(), 60)));
            break;
        }
    }
    if reason.is_none() && mode != Mode::Explicit {
        reason = strict_sentence(&full, mode == Mode::Blunt);
    }
    reason.map(|r| format!("female applicants only: {}", r))
}

#[cfg(test)]
mod tests {
    use super::{female_only as fo, Mode};
    fn yes(t: &str, b: &str) {
        assert!(fo(t, b, Mode::Balanced).is_some(), "should be female-only: {:?} / {:?}", t, b);
    }
    fn no(t: &str, b: &str) {
        assert!(fo(t, b, Mode::Balanced).is_none(), "should be open to men: {:?} / {:?} -> {:?}", t, b, fo(t, b, Mode::Balanced));
    }
    #[test]
    fn english_female_only() {
        yes("Admin Assistant", "This position is for female candidates only.");
        yes("Admin Assistant", "Only female applicants will be shortlisted.");
        yes("Teacher", "Women only position. Apply by email.");
        yes("Teacher", "Female-only vacancy in Herat");
        yes("Teacher", "Applicants must be female and hold a degree");
        yes("Teacher", "The candidate should be a woman with a bachelor degree");
        yes("Teacher", "Gender: Female. Location: Kabul");
        yes("Teacher", "Sex: F | Location: Kabul");
        yes("Teacher", "Male candidates are not eligible to apply.");
        yes("Teacher", "Salary 500 USD Gender Female Location Kabul");
        yes("Teacher", "Men cannot apply for this job");
        yes("Teacher", "Male applicants need not apply");
        yes("Teacher", "This post is reserved for women");
        yes("Teacher", "Open to female staff exclusively");
        yes("Female Enumerator", "Collect survey data");
        yes("Social Worker (Female)", "Kabul");
        yes("Nurse - Women", "Kabul");
        yes("Enumerator", "We are hiring females only for this role");
        yes("Enumerator", "Only women are eligible to apply for this position.");
    }
    #[test]
    fn english_open_to_men() {
        no("Admin Assistant", "Female candidates are strongly encouraged to apply.");
        no("Admin Assistant", "Qualified women are encouraged to apply");
        no("Admin Assistant", "Female preferred");
        no("Admin Assistant", "Preference will be given to women");
        no("Admin Assistant", "Preferably female only");
        no("Female Preferred Driver", "Kabul");
        no("Admin Assistant", "Both male and female candidates may apply");
        no("Admin Assistant", "Male/Female candidates");
        no("Admin Assistant (M/F)", "Berlin");
        no("Admin Assistant", "Gender: Male/Female");
        no("Admin Assistant", "Gender: Female/Male");
        no("Admin Assistant", "Gender: Any");
        no("Admin Assistant", "Equal opportunity employer regardless of gender");
        no("Admin Assistant", "Not only women: men are welcome too");
        no("Admin Assistant", "Women only is not required");
        no("Women's Rights Officer", "Kabul");
        no("Female Engagement Team Lead", "Kabul");
        no("Gender and Women Empowerment Officer", "Kabul");
        no("Counselor", "We run a women only shelter and serve survivors");
        no("Trainer", "Training for women only beneficiaries in Kabul");
        no("Admin Assistant", "Working with female beneficiaries only");
        no("Staff", "Driver: male only. Counselor: female only. Apply by email.");
        no("Staff", "Position 1 (male): guard. Position 2: cook, women only. Male candidates only for position 1");
        no("Engineer", "Women and men can apply, women only is not a requirement");
        no("Engineer", "");
    }
    #[test]
    fn strict_cases() {
        yes("Accountant", "Female only. Prior experience preferred. Apply by email.");
        yes("Accountant", "Women only! Fluent English is a plus.");
        yes("Accountant", "We are looking for a female accountant for our Kabul office.");
        yes("Accountant", "We need two qualified female enumerators.");
        yes("Accountant", "This vacancy is open to female candidates.");
        yes("Accountant", "Position open for female applicants with a degree");
        yes("Accountant", "Eligibility: Female");
        yes("Accountant", "Required gender: Female");
        yes("Accountant", "Hiring a female receptionist");
        yes("Female Trainer / Facilitator", "Kabul");
        yes("Lady Health Worker", "Herat");
        yes("Teacher", "Female candidates only (male and female staff work separately in this school).");
        yes("کارمند", "استخدام خانم برای دفتر کابل");
        yes("منشی خانم", "کابل");
        no("Accountant", "Female candidates are encouraged. Prior experience preferred.");
        no("Accountant", "Women are strongly encouraged to apply; Applicants must hold a degree");
        no("Accountant", "Female preferred but men can apply too");
        no("Accountant", "We are looking for an accountant. Women are encouraged to apply.");
        no("Accountant", "Position open for male and female candidates");
    }
    #[test]
    fn strict_sentences() {
        yes("Teacher", "Qualified female candidates must submit a CV and a cover letter.");
        yes("Teacher", "Position: Teacher. Female candidates are required for this post");
        yes("معلم", "متقاضیان خانم شرایط زیر را داشته باشند");
        yes("معلم", "این پست برای بانوان است و کاندیدان باید در کابل باشند");
        yes("Teacher", "The female applicant will report to the school principal");
        no("Teacher", "Experience working with women and girls is an advantage for applicants");
        no("Teacher", "Applicants must have experience working with women");
        no("Teacher", "Women are encouraged to apply as candidates");
        no("معلم", "برای تماس با خانم احمدی ایمیل بفرستید");
        no("Teacher", "The project supports women in Herat; applicants need a degree");
        no("Teacher", "Interested female and male candidates can apply");
    }
    #[test]
    fn blunt_mode() {
        let b = |t: &str, x: &str| fo(t, x, Mode::Blunt).is_some();
        // gender wins over "no experience" / "entry level"
        assert!(b("Junior Clerk", "Entry level, no experience required. Female candidates are encouraged to apply."));
        assert!(b("Junior Clerk", "No experience needed. Women are strongly encouraged to apply"));
        assert!(b("Junior Clerk", "Fresh graduates. Preference will be given to female candidates"));
        assert!(b("Junior Clerk", "Entry level role. Female applicants preferred."));
        assert!(b("کارمند", "بدون سابقه کار. خانم‌ها ترجیح داده می‌شوند"));
        assert!(b("Ofis", "Deneyim aranmaz. Bayan adaylar tercih edilir"));
        assert!(b("Junior Clerk", "Entry level.\nGender: Female\nLocation: Kabul"));
        // two-sided / topic wording stays
        assert!(!b("Junior Clerk", "No experience. Both male and female candidates may apply"));
        assert!(!b("Junior Clerk", "No experience. The project empowers women in Herat"));
        assert!(!b("Junior Clerk", "No experience. Gender: Male/Female"));
        assert!(!b("Junior Clerk", "No experience.\nRelated jobs\nFemale Teacher\nFemale Nurse"));
        assert!(!b("Junior Clerk", "No experience. Contact Ms. Sara for details"));
    }
    #[test]
    fn persian_dari_pashto_arabic() {
        yes("کارمند اداری", "این موقعیت فقط برای خانم‌ها است");
        yes("کارمند اداری", "ویژه بانوان");
        yes("کارمند اداری", "فقط بانوان می‌توانند درخواست دهند");
        yes("کارمند اداری", "جنسیت: زن");
        yes("کارمند اداری", "آقایان مجاز نیستند");
        yes("کارمند اداری", "مردان نمی توانند درخواست دهند");
        yes("کارمند (خانم)", "کابل");
        yes("کارمند اداری", "یوازې ښځو لپاره");
        yes("کارمند اداری", "للنساء فقط");
        no("کارمند اداری", "خانم‌ها و آقایان می‌توانند درخواست دهند");
        no("کارمند اداری", "جنسیت: زن / مرد");
        no("کارمند اداری", "ترجیح با خانم‌ها است");
        no("کارمند اداری", "فقط آقایان");
    }
    #[test]
    fn turkish() {
        yes("Ofis Asistanı", "Sadece bayan adaylar başvurabilir");
        yes("Ofis Asistanı", "Yalnızca kadın adaylar kabul edilir");
        yes("Ofis Asistanı", "Bayan eleman aranıyor");
        yes("Ofis Asistanı", "Erkek adaylar kabul edilmemektedir");
        yes("Bayan Personel", "İstanbul");
        no("Ofis Asistanı", "Bay ve bayan adaylar başvurabilir");
        no("Ofis Asistanı", "Kadın ve erkek adaylar");
        no("Bayan Giyim Satış Danışmanı", "İstanbul");
        no("Ofis Asistanı", "Sadece erkek adaylar");
    }
}
