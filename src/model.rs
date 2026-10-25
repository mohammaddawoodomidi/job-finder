use chrono::{DateTime, Utc};

#[derive(Clone, Debug, Default)]
pub struct ApplyMethod {
    pub kind: String, // link | email | telegram | whatsapp | phone
    pub label: String,
    pub value: String,
    pub href: String,
    pub status: String,
    pub from: String, // source that offered it (merged duplicates)
}

#[derive(Clone, Debug, Default)]
pub struct Dup {
    pub source: String,
    pub url: String,
    pub title: String,
    pub reason: String,
}

#[derive(Clone, Debug, Default)]
pub struct Job {
    pub source: String,
    pub source_type: String, // rss | website | telegram
    pub title: String,
    pub description: String,
    pub company: String,
    pub location: String,
    pub posted_at: Option<DateTime<Utc>>,
    pub source_url: String,
    pub apply_url: String,
    pub apply_kind: String, // direct | listing
    pub remote: Option<bool>,
    pub job_type: String,
    pub hints: Vec<String>,
    pub candidate_links: Vec<String>,
    pub language: String,
    pub languages_mentioned: Vec<String>,
    pub title_en: String,
    pub summary_en: String,
    pub entry_reason: String,
    pub years_required: Option<i64>,
    pub link_status: String,
    pub final_url: String,
    pub apply_methods: Vec<ApplyMethod>,
    pub apply_text: String,
    pub apply_text_en: String,
    pub page_text: String,    // the job page's text including side panels (where fields like "Gender" often sit)
    pub gender_checked: bool, // the job page was read and checked for a gender requirement
    pub needs_render: bool,   // this source's pages are built by JavaScript
    pub full_text: String,    // the job page's whole text (requirements / submission / gender wording)
    pub requirements: String, // brief: what the applicant needs
    pub submission: String,   // brief: submission guideline
    pub apply_link: String,   // the working link to the original posting
    pub duplicates: Vec<Dup>,
    pub page_fetched: bool, // the job's own page was downloaded OK
    pub from_cache: bool,   // ... but it came from the cache
    pub is_new: bool,
}

impl Job {
    pub fn new(source: &str, source_type: &str, title: &str) -> Job {
        Job { source: source.to_string(), source_type: source_type.to_string(), title: title.to_string(), ..Default::default() }
    }
}

#[derive(Clone, Debug)]
pub struct RawPost {
    pub channel: String,
    pub text: String,
    pub links: Vec<String>,
    pub posted_at: Option<DateTime<Utc>>,
    pub permalink: String,
}

#[derive(Clone, Debug, Default)]
pub struct SourceStat {
    pub name: String,
    pub found: usize,
    pub seconds: f64,
    pub error: String,
}
