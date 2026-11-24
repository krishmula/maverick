//! The scoring run: marks repeats and clear misfits from the row alone, then reads each
//! remaining posting's description and asks a model how well it fits the user.
//!
//! The model only reads the posting. The thresholds and hard rules that turn its reading
//! into a decision are code in this file, so they are tested and cannot drift.

use std::fmt::Write as _;
use std::path::Path;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::answers::Answers;
use crate::description::{self, Fetched, Source};
use crate::jobs::{Application, Posting, Sponsorship, Stage, posting_address};
use crate::providers::{self, Job};
use crate::rules::{SeenJobs, prefilter};
use crate::voice::Voice;

/// A score at or above this, with no blocking flag, makes a posting ready to apply to.
pub const APPLY_SCORE: u8 = 75;
/// A score under this skips the posting.
pub const REVIEW_SCORE: u8 = 60;
/// The most descriptions one run reads. The rest stay found for the next run.
pub const RUN_CAP: usize = 30;
/// How many postings are read and judged at the same time.
const AT_ONCE: usize = 4;

/// The resume variants a posting can be matched to.
pub const VARIANTS: &[&str] = &["Backend/Infra", "ML Infra", "Agentic AI"];

/// The start of the notes on a posting that is ready to apply to.
pub const APPLY_READY: &str = "Apply-ready (>=75, no blocking flags) - needs your approval to submit.";

/// What the user is, and is looking for, as the model needs it.
#[derive(Clone, Debug, Default)]
pub struct Profile {
    pub background: Vec<String>,
    pub facts: Vec<String>,
    pub target: Vec<String>,
    /// How the user writes messages. Empty until `with_voice` reads it.
    pub voice: Voice,
}

impl From<&Answers> for Profile {
    fn from(answers: &Answers) -> Self {
        Self {
            background: answers.background.clone(),
            facts: answers.facts.clone(),
            target: answers.target.clone(),
            voice: Voice::default(),
        }
    }
}

impl Profile {
    /// The profile in the answers file, with the voice file beside it when there is one.
    pub fn load(answers: &Path) -> Result<Self, String> {
        let mut profile = Self::from(&Answers::load(answers)?);
        profile.voice = Voice::load(&answers.with_file_name("voice.json"))?;
        Ok(profile)
    }
}

/// What a run does to the found postings before any network call.
#[derive(Debug, Default)]
pub struct Plan {
    /// Postings that repeat another posting or an application, already marked.
    pub duplicates: Vec<Posting>,
    /// Postings the title and location rules skip, already marked.
    pub skipped: Vec<Posting>,
    /// Postings to read and score in this run, at most the cap.
    pub to_score: Vec<Posting>,
    /// Postings left for the next run because of the cap.
    pub waiting: usize,
    /// Postings whose address no source covers. They stay found.
    pub unreadable: usize,
}

/// Sorts the found postings into repeats, rule skips, and the ones to score, oldest first.
pub fn plan(postings: &[Posting], applications: &[Application], cap: usize) -> Plan {
    let mut seen = SeenJobs::default();
    for posting in postings.iter().filter(|posting| posting.stage != Stage::Found) {
        seen.insert(&posting.company, &posting.title);
    }
    for role in applications.iter().filter_map(|sent| sent.role.as_deref()) {
        // A role is "Company - Title" in one cell. The key joins company and title, so this matches.
        seen.insert(role, "");
    }
    let mut plan = Plan::default();
    for posting in postings.iter().filter(|posting| posting.stage == Stage::Found) {
        if seen.insert(&posting.company, &posting.title) {
            plan.duplicates.push(Posting {
                stage: Stage::Duplicate,
                notes: Some("Duplicate - same company and title as an earlier posting or application".to_owned()),
                ..posting.clone()
            });
        } else if let Some(reason) = prefilter(&posting.title, posting.location.as_deref()) {
            plan.skipped.push(Posting {
                stage: Stage::Skipped,
                notes: Some(format!("Skipped - {reason}")),
                ..posting.clone()
            });
        } else if Source::of(&posting_address(&posting.link)).is_none() {
            plan.unreadable += 1;
        } else if plan.to_score.len() < cap {
            plan.to_score.push(posting.clone());
        } else {
            plan.waiting += 1;
        }
    }
    plan
}

/// What the model read in one posting.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct Reading {
    pub score: u8,
    pub variant: String,
    pub reason: String,
    #[serde(deserialize_with = "sponsorship")]
    pub sponsorship: Sponsorship,
    pub citizenship_required: bool,
    pub min_years: Option<u8>,
    pub primarily_frontend: bool,
    pub us_location: bool,
    pub advisory: Vec<String>,
}

fn sponsorship<'de, D: serde::Deserializer<'de>>(from: D) -> Result<Sponsorship, D::Error> {
    let text = String::deserialize(from)?;
    Sponsorship::parse(&text).ok_or_else(|| serde::de::Error::custom(format!("unknown sponsorship {text:?}")))
}

#[derive(Clone, Debug, PartialEq)]
pub enum Decision {
    Skip(String),
    Review,
    ApplyReady,
}

/// Turns the model's reading into a decision, with the blocking and advisory flags behind it.
pub fn decide(reading: &Reading) -> (Decision, Vec<String>, Vec<String>) {
    let mut blocking = Vec::new();
    let mut advisory = reading.advisory.clone();
    if reading.sponsorship == Sponsorship::NoSponsorship {
        blocking.push("no sponsorship".to_owned());
    }
    if reading.min_years == Some(4) {
        advisory.insert(0, "4 years required".to_owned());
    }
    let hard_skip = if reading.citizenship_required {
        Some("US citizenship or clearance required".to_owned())
    } else if let Some(years) = reading.min_years.filter(|&years| years >= 5) {
        Some(format!("{years}+ years required"))
    } else if reading.primarily_frontend {
        Some("primarily frontend".to_owned())
    } else if !reading.us_location {
        Some("no location in the United States".to_owned())
    } else if reading.score < REVIEW_SCORE {
        Some(format!("{}, {}: {}", reading.score, reading.variant, reading.reason))
    } else {
        None
    };
    let decision = match hard_skip {
        Some(reason) => Decision::Skip(reason),
        None if reading.score >= APPLY_SCORE && blocking.is_empty() => Decision::ApplyReady,
        None => Decision::Review,
    };
    (decision, blocking, advisory)
}

/// The posting with the decision written into it, in the tracker's wording.
pub fn decided(posting: &Posting, reading: &Reading, address: Option<String>) -> Posting {
    let (decision, blocking, advisory) = decide(reading);
    let verdict = format!("{}, {}: {}", reading.score, reading.variant, reading.reason);
    let (stage, notes) = match decision {
        Decision::Skip(reason) => (Stage::Skipped, format!("Skipped - {reason}")),
        Decision::Review => (Stage::Review, format!("Review - {verdict}")),
        Decision::ApplyReady => (Stage::Review, format!("{APPLY_READY} {verdict}")),
    };
    Posting {
        stage,
        score: Some(reading.score),
        variant: Some(reading.variant.clone()),
        flags: flags(&blocking, &advisory),
        notes: Some(notes),
        address,
        sponsorship: Some(reading.sponsorship),
        ..posting.clone()
    }
}

fn flags(blocking: &[String], advisory: &[String]) -> Option<String> {
    let mut parts = Vec::new();
    if !blocking.is_empty() {
        parts.push(format!("Blocking: {}", blocking.join("; ")));
    }
    if !advisory.is_empty() {
        parts.push(format!("Advisory: {}", advisory.join("; ")));
    }
    (!parts.is_empty()).then(|| parts.join(". "))
}

/// Reads and judges one posting. A failure leaves it found, with the reason in its notes,
/// so the next run tries again.
pub async fn score(posting: Posting, profile: &Profile) -> Posting {
    let address = posting_address(&posting.link);
    let description = match description::fetch(&address).await {
        Ok(Fetched::Open(description)) => description,
        Ok(Fetched::Closed) => {
            return Posting {
                stage: Stage::Skipped,
                flags: Some("Blocking: posting closed".to_owned()),
                notes: Some("Skipped - posting closed".to_owned()),
                ..posting
            };
        }
        Err(error) => return not_scored(posting, &format!("could not read the posting: {error}")),
    };
    match read(&posting, &description, profile).await {
        Ok(reading) => {
            // An address intake already knows is better than one followed from a redirect.
            let address = posting.address.clone().or(description.address);
            decided(&posting, &reading, address)
        }
        Err(error) => not_scored(posting, &error),
    }
}

fn not_scored(posting: Posting, why: &str) -> Posting {
    Posting {
        notes: Some(format!("Not scored yet: {why}")),
        ..posting
    }
}

/// Scores every posting, a few at a time, and calls `done` as each one finishes.
pub async fn score_all(postings: Vec<Posting>, profile: Profile, mut done: impl FnMut(&Posting)) -> Vec<Posting> {
    let profile = Arc::new(profile);
    let limit = Arc::new(Semaphore::new(AT_ONCE));
    let mut running = JoinSet::new();
    for posting in postings {
        let (profile, limit) = (profile.clone(), limit.clone());
        running.spawn(async move {
            let _turn = limit.acquire_owned().await.expect("the semaphore is never closed");
            score(posting, &profile).await
        });
    }
    let mut scored = Vec::new();
    while let Some(finished) = running.join_next().await {
        let posting = finished.expect("a scoring task does not panic");
        done(&posting);
        scored.push(posting);
    }
    scored
}

pub(crate) const INSTRUCTIONS: &str = "You read one job posting and judge how well it fits one applicant. \
You are given the applicant's resume, facts about them, and the jobs they are looking for. \
Score the fit from 0 to 100 against the resume and the target: \
90 to 100 means the level is right and the core of the job is work the resume shows; \
75 to 89 means a good fit with gaps a hiring manager would accept; \
60 to 74 means a partial fit worth a human look; \
under 60 means a poor fit, such as the wrong level, a stack the resume does not show, or a job that is not software engineering. \
The score is about the work and the level only. Do not lower it for sponsorship, citizenship, clearance, location \
or required years: those go in their own fields below and are handled separately. \
Judge like a recruiter screening for a phone call, not like a final interviewer: a job at a target level and in a target kind of work, \
where the resume shows most of the core skills, scores 75 or more. A listed tool or language the resume lacks costs a few points, not a band, \
unless the job is built around it. \
Pick the resume variant whose focus fits the job best: Backend/Infra for backend, platform, distributed systems and cloud work; \
ML Infra for model serving, training infrastructure, GPUs and inference; Agentic AI for building LLM applications and agents. \
In reason, give the deciding match or mismatch in one short sentence. \
Read these facts from the posting text alone, and never guess: \
sponsorship is sponsors only when the posting says it sponsors work visas, no_sponsorship when it says it will not sponsor \
or requires work authorization without sponsorship now or in the future, and unknown otherwise. \
citizenship_required is true when the job requires US citizenship, permanent residency, a security clearance, or ITAR US-person status. \
min_years is the minimum years of professional experience the posting requires, the low end of a range, \
and null when it states none; preferred years do not count. \
primarily_frontend is true when most of the work is user interface work; full-stack work with real backend or infrastructure work is false. \
us_location is true when any listed location is in the United States or the job is remote within the United States, \
and also true when the posting lists no location. \
advisory lists short notes the applicant should know, such as relocation needed, days in the office, \
or a preferred qualification the resume lacks, each in under ten words. \
Leave out anything already covered by the other fields, and leave the list empty when nothing stands out.";

pub(crate) fn reading_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "score": { "type": "integer", "minimum": 0, "maximum": 100 },
            "variant": { "type": "string", "enum": VARIANTS },
            "reason": { "type": "string" },
            "sponsorship": { "type": "string", "enum": Sponsorship::ALL.iter().map(|s| s.as_str()).collect::<Vec<_>>() },
            "citizenship_required": { "type": "boolean" },
            "min_years": { "type": ["integer", "null"], "minimum": 0, "maximum": 30 },
            "primarily_frontend": { "type": "boolean" },
            "us_location": { "type": "boolean" },
            "advisory": { "type": "array", "items": { "type": "string" } },
        },
        "required": [
            "score", "variant", "reason", "sponsorship", "citizenship_required",
            "min_years", "primarily_frontend", "us_location", "advisory",
        ],
        "additionalProperties": false,
    })
}

fn request(posting: &Posting, description: &description::Description, profile: &Profile) -> String {
    let mut request = String::from("The applicant's resume:\n");
    for line in &profile.background {
        let _ = writeln!(request, "{line}");
    }
    request.push_str("\nFacts about the applicant:\n");
    for fact in &profile.facts {
        let _ = writeln!(request, "- {fact}");
    }
    request.push_str("\nThe jobs the applicant is looking for:\n");
    for line in &profile.target {
        let _ = writeln!(request, "- {line}");
    }
    let title = if description.title.is_empty() {
        &posting.title
    } else {
        &description.title
    };
    let location = description
        .location
        .as_deref()
        .or(posting.location.as_deref())
        .unwrap_or("not listed");
    let _ = write!(
        request,
        "\nThe posting:\nCompany: {}\nTitle: {title}\nLocation: {location}\n\n{}\n",
        posting.company, description.text
    );
    request
}

async fn read(posting: &Posting, description: &description::Description, profile: &Profile) -> Result<Reading, String> {
    let reply = providers::run(
        Job::Scoring,
        INSTRUCTIONS,
        request(posting, description, profile),
        &reading_schema(),
    )
    .await?;
    serde_json::from_value(reply).map_err(|error| format!("the model's reply was not in the agreed form: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::ApplicationStatus;

    fn posting(link: &str, company: &str, title: &str, stage: Stage) -> Posting {
        Posting {
            link: link.into(),
            found_at: "2026-11-01T00:00:00Z".parse().unwrap(),
            source: "Board".into(),
            company: company.into(),
            title: title.into(),
            location: Some("Austin, Texas, United States".into()),
            email_subject: None,
            stage,
            score: None,
            variant: None,
            flags: None,
            notes: None,
            address: None,
            sponsorship: None,
        }
    }

    fn reading(score: u8) -> Reading {
        Reading {
            score,
            variant: "Backend/Infra".into(),
            reason: "Go services on AWS match the resume".into(),
            sponsorship: Sponsorship::Unknown,
            citizenship_required: false,
            min_years: Some(2),
            primarily_frontend: false,
            us_location: true,
            advisory: vec![],
        }
    }

    const GREENHOUSE: &str = "https://job-boards.greenhouse.io/example/jobs/";

    #[test]
    fn the_plan_marks_repeats_and_rule_skips_and_caps_the_rest() {
        let postings = [
            posting(
                &format!("{GREENHOUSE}1"),
                "Example Co",
                "Backend Engineer",
                Stage::Review,
            ),
            posting(
                &format!("{GREENHOUSE}2"),
                "Example Co",
                "Backend Engineer",
                Stage::Found,
            ),
            posting(
                &format!("{GREENHOUSE}3"),
                "Sample Inc",
                "Staff Software Engineer",
                Stage::Found,
            ),
            posting(
                "https://careers.sample.com/42",
                "Sample Inc",
                "Platform Engineer",
                Stage::Found,
            ),
            posting(
                &format!("{GREENHOUSE}4"),
                "Sample Inc",
                "Software Engineer",
                Stage::Found,
            ),
            posting(
                &format!("{GREENHOUSE}5"),
                "Other LLC",
                "Infrastructure Engineer",
                Stage::Found,
            ),
            posting(&format!("{GREENHOUSE}6"), "Applied Co", "Data Engineer", Stage::Found),
            posting(
                &format!("{GREENHOUSE}7"),
                "Third Co",
                "Software Engineer II",
                Stage::Found,
            ),
        ];
        let applications = [Application {
            sheet_row: Some(2),
            role: Some("Applied Co - Data Engineer".into()),
            link: None,
            applied_on: "2026-10-01".parse().unwrap(),
            status: ApplicationStatus::Pending,
            notes: None,
        }];
        let plan = plan(&postings, &applications, 2);
        let links = |postings: &[Posting]| postings.iter().map(|p| p.link.clone()).collect::<Vec<_>>();
        assert_eq!(
            links(&plan.duplicates),
            [format!("{GREENHOUSE}2"), format!("{GREENHOUSE}6")]
        );
        assert!(plan.duplicates.iter().all(|p| p.stage == Stage::Duplicate));
        assert_eq!(links(&plan.skipped), [format!("{GREENHOUSE}3")]);
        assert_eq!(plan.skipped[0].notes.as_deref(), Some("Skipped - staff-level role"));
        assert_eq!(
            links(&plan.to_score),
            [format!("{GREENHOUSE}4"), format!("{GREENHOUSE}5")]
        );
        assert_eq!((plan.waiting, plan.unreadable), (1, 1));
    }

    #[test]
    fn scores_split_at_the_thresholds() {
        assert_eq!(decide(&reading(75)).0, Decision::ApplyReady);
        assert_eq!(decide(&reading(74)).0, Decision::Review);
        assert_eq!(decide(&reading(60)).0, Decision::Review);
        assert!(matches!(decide(&reading(59)).0, Decision::Skip(_)));
    }

    #[test]
    fn no_sponsorship_blocks_a_high_score_from_being_apply_ready() {
        let reading = Reading {
            sponsorship: Sponsorship::NoSponsorship,
            ..reading(90)
        };
        let (decision, blocking, _) = decide(&reading);
        assert_eq!(decision, Decision::Review);
        assert_eq!(blocking, ["no sponsorship"]);
    }

    #[test]
    fn hard_rules_skip_whatever_the_score() {
        let cases = [
            (
                Reading {
                    citizenship_required: true,
                    ..reading(95)
                },
                "US citizenship or clearance required",
            ),
            (
                Reading {
                    min_years: Some(5),
                    ..reading(95)
                },
                "5+ years required",
            ),
            (
                Reading {
                    primarily_frontend: true,
                    ..reading(95)
                },
                "primarily frontend",
            ),
            (
                Reading {
                    us_location: false,
                    ..reading(95)
                },
                "no location in the United States",
            ),
        ];
        for (reading, reason) in cases {
            assert_eq!(decide(&reading).0, Decision::Skip(reason.into()));
        }
    }

    #[test]
    fn four_years_is_only_advisory() {
        let reading = Reading {
            min_years: Some(4),
            advisory: vec!["relocation needed".into()],
            ..reading(80)
        };
        let (decision, blocking, advisory) = decide(&reading);
        assert_eq!(decision, Decision::ApplyReady);
        assert!(blocking.is_empty());
        assert_eq!(advisory, ["4 years required", "relocation needed"]);
    }

    #[test]
    fn the_decision_is_written_in_the_tracker_wording() {
        let found = posting(
            &format!("{GREENHOUSE}1"),
            "Example Co",
            "Backend Engineer",
            Stage::Found,
        );
        let ready = decided(
            &found,
            &Reading {
                min_years: Some(4),
                ..reading(82)
            },
            Some("https://jobs.example.com/1".into()),
        );
        assert_eq!(ready.stage, Stage::Review);
        assert_eq!(ready.score, Some(82));
        assert_eq!(ready.variant.as_deref(), Some("Backend/Infra"));
        assert_eq!(ready.flags.as_deref(), Some("Advisory: 4 years required"));
        assert_eq!(
            ready.notes.as_deref(),
            Some(
                "Apply-ready (>=75, no blocking flags) - needs your approval to submit. \
                 82, Backend/Infra: Go services on AWS match the resume"
            )
        );
        assert_eq!(ready.address.as_deref(), Some("https://jobs.example.com/1"));
        assert_eq!(ready.sponsorship, Some(Sponsorship::Unknown));

        let blocked = decided(
            &found,
            &Reading {
                sponsorship: Sponsorship::NoSponsorship,
                advisory: vec!["onsite".into()],
                ..reading(70)
            },
            None,
        );
        assert_eq!(
            blocked.flags.as_deref(),
            Some("Blocking: no sponsorship. Advisory: onsite")
        );
        assert!(blocked.notes.unwrap().starts_with("Review - 70, Backend/Infra:"));

        let low = decided(&found, &reading(40), None);
        assert_eq!(low.stage, Stage::Skipped);
        assert_eq!(
            low.notes.as_deref(),
            Some("Skipped - 40, Backend/Infra: Go services on AWS match the resume")
        );
    }

    #[test]
    fn the_model_reply_parses_into_a_reading() {
        let reply = json!({
            "score": 81, "variant": "ML Infra", "reason": "GPU serving matches",
            "sponsorship": "no_sponsorship", "citizenship_required": false, "min_years": null,
            "primarily_frontend": false, "us_location": true, "advisory": ["hybrid"],
        });
        let reading: Reading = serde_json::from_value(reply).unwrap();
        assert_eq!(reading.sponsorship, Sponsorship::NoSponsorship);
        assert_eq!(reading.min_years, None);
        let bad = json!({ "sponsorship": "maybe" });
        assert!(serde_json::from_value::<Reading>(bad).is_err());
    }
}
