//! Outreach on LinkedIn: who to reach at each company the user applies to, hiring posts worth
//! a note, and the drafts the user approves before Maverick types them. Nothing here sends.
//!
//! A check never blocks an application: "None, apply cold" is a normal verdict.

use std::cmp::Reverse;
use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::Path;
use std::sync::Arc;

use jiff::Timestamp;
use jiff::civil::Date;
use rusqlite::{OptionalExtension as _, params};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::bridge::Bridge;
use crate::description::{self, Fetched};
use crate::jobs::{HiringPost, Outreach, Posting, Stage, posting_address, stored_enum};
use crate::linkedin::{
    self, Connection, INMAIL_BODY_LIMIT, INMAIL_SUBJECT_LIMIT, NOTE_LIMIT, Person, Post, PosterCheck, Reader, Slot,
    Stop, company_key, company_slug, people_address, people_page, post_search_address, posts_page, same_company,
};
use crate::providers::{self, Job};
use crate::score::{self, APPLY_READY, Decision, Profile, Reading};
use crate::store::{Store, StoreError, stored};

/// The most People-page lookups one check run makes, apply-ready postings first.
pub const LOOKUP_CAP: usize = 10;
/// The most new hiring-post drafts in one Pacific day.
pub const DRAFTS_PER_DAY: usize = 10;
/// The most posts one sweep reads with the model.
pub const POSTS_READ: usize = 20;
/// LinkedIn content searches for hiring posts. Each one is one page load.
pub const SEARCHES: &[&str] = &[
    "\"hiring\" \"new grad\" \"software engineer\"",
    "\"we're hiring\" backend OR infrastructure engineer",
    "\"hiring\" \"ML infrastructure\" OR \"AI infrastructure\" engineer",
];

stored_enum! {
    /// What the outreach check found for one posting.
    Verdict {
        Connection => "connection",
        TeamMember => "team_member",
        HiringManager => "hiring_manager",
        Deferred => "deferred",
        Cold => "cold",
    }
}

impl Verdict {
    pub fn label(self) -> &'static str {
        match self {
            Self::Connection => "Connection found",
            Self::TeamMember => "Team member found",
            Self::HiringManager => "Hiring manager named",
            Self::Deferred => "People lookup deferred",
            Self::Cold => "None, apply cold",
        }
    }

    pub fn found_people(self) -> bool {
        matches!(self, Self::Connection | Self::TeamMember | Self::HiringManager)
    }
}

/// The outreach check for one posting.
#[derive(Clone, Debug, PartialEq)]
pub struct Check {
    pub posting_link: String,
    pub verdict: Verdict,
    pub team: Option<String>,
    /// One or two people, best first.
    pub people: Vec<Person>,
    pub note: Option<String>,
    pub checked_at: Timestamp,
}

impl Check {
    /// The verdict as the tracker writes it, with the people named.
    pub fn summary(&self) -> String {
        let mut line = match (self.verdict, &self.team) {
            (Verdict::Deferred, Some(team)) => format!("Team named ({team}), people lookup deferred"),
            (verdict, _) => verdict.label().to_owned(),
        };
        let people: Vec<String> = self
            .people
            .iter()
            .map(|person| {
                let mut said = person.name.clone();
                if !person.title.is_empty() {
                    let _ = write!(said, ", {}", person.title);
                }
                if let Some(degree) = &person.degree {
                    let _ = write!(said, " ({degree})");
                }
                said
            })
            .collect();
        if !people.is_empty() {
            let _ = write!(line, ": {}", people.join("; "));
        }
        line
    }
}

/// What a draft is about.
#[derive(Clone, Debug, PartialEq)]
pub enum About {
    /// A posting the user applies to, by its link.
    Posting(String),
    /// A hiring post, by its address.
    Post(String),
}

impl About {
    fn kind(&self) -> &'static str {
        match self {
            Self::Posting(_) => "posting",
            Self::Post(_) => "post",
        }
    }

    pub fn link(&self) -> &str {
        match self {
            Self::Posting(link) | Self::Post(link) => link,
        }
    }
}

/// A note or message for one person, written for the user to approve.
#[derive(Clone, Debug, PartialEq)]
pub struct Draft {
    /// 0 until the store saves it.
    pub id: i64,
    pub about: About,
    pub person: String,
    pub headline: String,
    pub profile_url: String,
    pub slot: Slot,
    /// The InMail subject; `None` for a note or a message.
    pub subject: Option<String>,
    /// The note, the message, or the InMail body.
    pub text: String,
    pub status: Outreach,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

fn people_json(people: &[Person]) -> String {
    serde_json::to_string(people).expect("people are plain JSON")
}

impl Store {
    /// Replaces the stored connections with a fresh export. LinkedIn's export is always complete.
    pub fn replace_connections(&mut self, connections: &[Connection]) -> Result<usize, StoreError> {
        let write = self.db.transaction()?;
        write.execute("DELETE FROM linkedin_connections", [])?;
        {
            let mut insert = write.prepare(
                "INSERT INTO linkedin_connections
                     (first_name, last_name, profile_url, email, company, position, connected_on)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?;
            for connection in connections {
                insert.execute(params![
                    connection.first_name,
                    connection.last_name,
                    connection.profile_url,
                    connection.email,
                    connection.company,
                    connection.position,
                    connection.connected_on.map(|day| day.to_string()),
                ])?;
            }
        }
        write.commit()?;
        Ok(connections.len())
    }

    pub fn connections(&self) -> Result<Vec<Connection>, StoreError> {
        let mut select = self.db.prepare(
            "SELECT first_name, last_name, profile_url, email, company, position, connected_on
             FROM linkedin_connections ORDER BY id",
        )?;
        select
            .query_map([], |row| {
                Ok(Connection {
                    first_name: row.get(0)?,
                    last_name: row.get(1)?,
                    profile_url: row.get(2)?,
                    email: row.get(3)?,
                    company: row.get(4)?,
                    position: row.get(5)?,
                    connected_on: row.get::<_, Option<String>>(6)?.and_then(|day| day.parse().ok()),
                })
            })?
            .collect()
    }

    /// Saves a check, replacing the posting's earlier one.
    pub fn save_check(&self, check: &Check) -> Result<(), StoreError> {
        self.db.execute(
            "INSERT INTO outreach_checks (posting_link, verdict, team, people, note, checked_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT (posting_link) DO UPDATE SET verdict = excluded.verdict, team = excluded.team,
                 people = excluded.people, note = excluded.note, checked_at = excluded.checked_at",
            params![
                check.posting_link,
                check.verdict.as_str(),
                check.team,
                people_json(&check.people),
                check.note,
                check.checked_at.to_string(),
            ],
        )?;
        Ok(())
    }

    /// The LinkedIn page address found for each company, by its reduced name.
    pub fn company_pages(&self) -> Result<std::collections::HashMap<String, String>, StoreError> {
        let mut select = self.db.prepare("SELECT company, slug FROM linkedin_company_pages")?;
        select.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?.collect()
    }

    /// Remembers the company's LinkedIn page, once a People page there showed the right company.
    pub fn save_company_page(&self, company: &str, slug: &str, at: Timestamp) -> Result<(), StoreError> {
        self.db.execute(
            "INSERT INTO linkedin_company_pages (company, slug, found_at) VALUES (?1, ?2, ?3)
             ON CONFLICT (company) DO UPDATE SET slug = excluded.slug, found_at = excluded.found_at",
            params![company_key(company), slug, at.to_string()],
        )?;
        Ok(())
    }

    /// Every check, newest first.
    pub fn checks(&self) -> Result<Vec<Check>, StoreError> {
        let mut select = self.db.prepare(
            "SELECT posting_link, verdict, team, people, note, checked_at FROM outreach_checks
             ORDER BY checked_at DESC",
        )?;
        select
            .query_map([], |row| {
                Ok(Check {
                    posting_link: row.get(0)?,
                    verdict: stored(row, 1, Verdict::parse)?,
                    team: row.get(2)?,
                    people: stored(row, 3, |text| serde_json::from_str(text).ok())?,
                    note: row.get(4)?,
                    checked_at: stored(row, 5, |text| text.parse().ok())?,
                })
            })?
            .collect()
    }

    /// Every hiring post, from the tracker and from Maverick's sweeps, newest first.
    pub fn hiring_posts(&self) -> Result<Vec<HiringPost>, StoreError> {
        let mut select = self.db.prepare(
            "SELECT post_link, role, found_at, poster, company, score, variant, outreach, notes
             FROM hiring_posts ORDER BY found_at DESC, id DESC",
        )?;
        select
            .query_map([], |row| {
                Ok(HiringPost {
                    post_link: row.get(0)?,
                    role: row.get(1)?,
                    found_at: stored(row, 2, |text| text.parse().ok())?,
                    poster: row.get(3)?,
                    company: row.get(4)?,
                    score: row.get(5)?,
                    variant: row.get(6)?,
                    outreach: stored(row, 7, Outreach::parse)?,
                    notes: row.get(8)?,
                })
            })?
            .collect()
    }

    /// Adds a hiring post a sweep found. A post the store already holds is left as it is.
    pub fn add_hiring_post(&self, post: &HiringPost) -> Result<bool, StoreError> {
        let added = self.db.execute(
            "INSERT INTO hiring_posts (post_link, role, found_at, poster, company, score, variant, outreach, notes)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT (post_link, role) DO NOTHING",
            params![
                post.post_link,
                post.role,
                post.found_at.to_string(),
                post.poster,
                post.company,
                post.score,
                post.variant,
                post.outreach.as_str(),
                post.notes,
            ],
        )?;
        Ok(added == 1)
    }

    /// Saves a new draft and gives its id. `None` when the person already has a draft about the same thing.
    pub fn add_draft(&self, draft: &Draft) -> Result<Option<i64>, StoreError> {
        let added = self.db.execute(
            "INSERT INTO outreach_drafts
                 (about_kind, about, person, headline, profile_url, slot, text, status, created_at, updated_at, subject)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT (about, profile_url) DO NOTHING",
            params![
                draft.about.kind(),
                draft.about.link(),
                draft.person,
                draft.headline,
                draft.profile_url,
                draft.slot.as_str(),
                draft.text,
                draft.status.as_str(),
                draft.created_at.to_string(),
                draft.updated_at.to_string(),
                draft.subject,
            ],
        )?;
        Ok((added == 1).then(|| self.db.last_insert_rowid()))
    }

    /// Every draft, newest first.
    pub fn drafts(&self) -> Result<Vec<Draft>, StoreError> {
        let mut select = self.db.prepare(
            "SELECT id, about_kind, about, person, headline, profile_url, slot, text, status, created_at, updated_at,
                 subject
             FROM outreach_drafts ORDER BY created_at DESC, id DESC",
        )?;
        select
            .query_map([], |row| {
                let link: String = row.get(2)?;
                Ok(Draft {
                    id: row.get(0)?,
                    about: stored(row, 1, |kind| match kind {
                        "posting" => Some(About::Posting(link.clone())),
                        "post" => Some(About::Post(link.clone())),
                        _ => None,
                    })?,
                    person: row.get(3)?,
                    headline: row.get(4)?,
                    profile_url: row.get(5)?,
                    slot: stored(row, 6, Slot::parse)?,
                    subject: row.get(11)?,
                    text: row.get(7)?,
                    status: stored(row, 8, Outreach::parse)?,
                    created_at: stored(row, 9, |text| text.parse().ok())?,
                    updated_at: stored(row, 10, |text| text.parse().ok())?,
                })
            })?
            .collect()
    }

    pub fn draft(&self, id: i64) -> Result<Option<Draft>, StoreError> {
        Ok(self.drafts()?.into_iter().find(|draft| draft.id == id))
    }

    /// Sets how far a draft has gone. A draft about a hiring post moves the post with it.
    pub fn set_draft_status(&mut self, id: i64, status: Outreach, at: Timestamp) -> Result<(), StoreError> {
        let write = self.db.transaction()?;
        write.execute(
            "UPDATE outreach_drafts SET status = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, status.as_str(), at.to_string()],
        )?;
        let post: Option<String> = write
            .query_row(
                "SELECT about FROM outreach_drafts WHERE id = ?1 AND about_kind = 'post'",
                [id],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(post) = post {
            write.execute(
                "UPDATE hiring_posts SET outreach = ?2 WHERE post_link = ?1",
                params![post, status.as_str()],
            )?;
        }
        write.commit()
    }

    /// Puts a skipped draft, or one typed but not sent, back to waiting for approval, with its text and
    /// slot as they were. Only the draft changes: the posting's check and the hiring post stay as they
    /// are, and the draft keeps the day it was made, so it does not count against the daily cap again.
    pub fn reset_draft(&self, id: i64, at: Timestamp) -> Result<(), String> {
        let said = |error: rusqlite::Error| format!("the database failed: {error}");
        let status: Option<String> = self
            .db
            .query_row("SELECT status FROM outreach_drafts WHERE id = ?1", [id], |row| {
                row.get(0)
            })
            .optional()
            .map_err(said)?;
        let status = status
            .and_then(|status| Outreach::parse(&status))
            .ok_or("the draft is no longer in the database")?;
        if !matches!(status, Outreach::Skipped | Outreach::Typed) {
            return Err(format!(
                "only a skipped or typed draft can be reset; this one is {}",
                status.as_str().replace('_', " ")
            ));
        }
        self.db
            .execute(
                "UPDATE outreach_drafts SET status = 'drafted', updated_at = ?2 WHERE id = ?1",
                params![id, at.to_string()],
            )
            .map_err(said)?;
        Ok(())
    }

    /// Puts a draft in another form, such as a note rewritten as an InMail.
    pub fn set_draft_form(&self, id: i64, slot: Slot, written: &Written, at: Timestamp) -> Result<(), StoreError> {
        self.db.execute(
            "UPDATE outreach_drafts SET slot = ?2, subject = ?3, text = ?4, updated_at = ?5 WHERE id = ?1",
            params![id, slot.as_str(), written.subject, written.text, at.to_string()],
        )?;
        Ok(())
    }

    /// Replaces a draft's text with the user's edit.
    pub fn set_draft_text(&self, id: i64, text: &str, at: Timestamp) -> Result<(), StoreError> {
        self.db.execute(
            "UPDATE outreach_drafts SET text = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, text, at.to_string()],
        )?;
        Ok(())
    }
}

fn is_apply_ready(posting: &Posting) -> bool {
    posting
        .notes
        .as_deref()
        .is_some_and(|notes| notes.starts_with(APPLY_READY))
}

/// The postings a check run looks at, in order: postings to review that have no verdict yet,
/// or whose lookup was deferred, apply-ready first, then by score.
pub fn to_check(postings: &[Posting], checks: &[Check]) -> Vec<Posting> {
    let settled: HashSet<&str> = checks
        .iter()
        .filter(|check| check.verdict != Verdict::Deferred)
        .map(|check| check.posting_link.as_str())
        .collect();
    let mut due: Vec<Posting> = postings
        .iter()
        .filter(|posting| posting.stage == Stage::Review && !settled.contains(posting.link.as_str()))
        .cloned()
        .collect();
    due.sort_by_key(|posting| (!is_apply_ready(posting), Reverse(posting.score)));
    due
}

/// What a posting says about its team and the people behind it.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct Named {
    pub team: Option<String>,
    /// The kind of work in a few words, such as "backend engineering", for a People search.
    pub function: String,
    /// Hiring managers or recruiters the posting names.
    pub people: Vec<NamedPerson>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct NamedPerson {
    pub name: String,
    pub role: String,
}

/// Up to two of the user's connections at the company, those whose position mentions the team first.
pub fn connections_at(company: &str, team: Option<&str>, connections: &[Connection]) -> Vec<Person> {
    let at_company: Vec<&Connection> = connections
        .iter()
        .filter(|connection| same_company(&connection.company, company))
        .collect();
    let team_words = team.map(meaningful_words).unwrap_or_default();
    let on_team = |connection: &&Connection| {
        let position = connection.position.to_lowercase();
        team_words.iter().any(|word| position.contains(word.as_str()))
    };
    let (mut first, rest): (Vec<&Connection>, Vec<&Connection>) = at_company.into_iter().partition(on_team);
    first.extend(rest);
    first
        .into_iter()
        .take(2)
        .map(|connection| Person {
            name: connection.name(),
            title: connection.position.clone(),
            profile_url: connection.profile_url.clone(),
            degree: Some("1st".to_owned()),
        })
        .collect()
}

/// Words that say something about a team or a job, in lower case.
fn meaningful_words(text: &str) -> Vec<String> {
    const FILLER: &[&str] = &["the", "and", "for", "team", "with", "of", "new", "grad"];
    company_key(text)
        .split(' ')
        .filter(|word| word.len() > 2 && !FILLER.contains(word))
        .map(str::to_owned)
        .collect()
}

/// Who to pick from the company's People page: a named person, or else the team or the function.
pub fn people_query(named: &Named) -> String {
    match (named.people.first(), &named.team) {
        (Some(person), _) => person.name.clone(),
        (None, Some(team)) => team.clone(),
        (None, None) if !named.function.trim().is_empty() => named.function.clone(),
        (None, None) => "software engineer".to_owned(),
    }
}

/// What a People-page lookup came back with.
#[derive(Clone, Debug, PartialEq)]
pub enum Lookup {
    Found(Vec<Person>),
    /// The page at the guessed address belongs to another company, or to none.
    WrongPage(String),
    /// The cap was reached, Chrome is not connected, or LinkedIn stopped the run.
    Deferred(String),
}

/// One or two people from a company's People page: the person named, or else engineers whose title
/// names the team, then other engineers, then recruiters. A title with no engineering or
/// recruiting word is never picked, whatever team it names. LinkedIn ignores a search on that page,
/// so the picking happens here, from the people it lists.
pub fn picked(query: &str, named: &Named, people: &[Person]) -> Vec<Person> {
    if let Some(wanted) = named.people.first() {
        let wanted = wanted.name.to_lowercase();
        return people
            .iter()
            .filter(|person| person.name.to_lowercase() == wanted)
            .take(1)
            .cloned()
            .collect();
    }
    const ENGINEERS: &[&str] = &["software", "engineer", "engineering", "sde", "swe", "developer"];
    const RECRUITERS: &[&str] = &["recruiter", "recruiting", "recruitment", "talent"];
    /// Words too common in titles to say which team someone is on.
    const WEAK: &[&str] = &[
        "enterprise",
        "software",
        "engineer",
        "engineering",
        "senior",
        "staff",
        "lead",
        "early",
        "native",
        "technical",
        "general",
        "product",
        "data",
        "platform",
        "systems",
    ];
    let team: Vec<String> = meaningful_words(query)
        .into_iter()
        .filter(|word| !WEAK.contains(&word.as_str()))
        .collect();
    // A role word fits only as itself or its plural, so "Engineered" is not "engineer" and "swe" is
    // not "Sweden". A team word over three letters also fits a word it starts, so "infra" fits
    // "infrastructure". Only the headline's first part says the job; later parts list degrees and skills.
    let has = |person: &Person, sought: &[&str], prefix: bool| {
        let job = person.title.split('|').next().unwrap_or_default();
        job.to_lowercase().split(|c: char| !c.is_alphanumeric()).any(|word| {
            sought.iter().any(|sought| {
                word == *sought
                    || word.strip_suffix('s') == Some(sought)
                    || (prefix && sought.len() > 3 && word.starts_with(sought))
            })
        })
    };
    let team: Vec<&str> = team.iter().map(String::as_str).collect();
    let rank = |person: &Person| match (has(person, ENGINEERS, false), has(person, &team, true)) {
        (true, true) => Some(0),
        (true, false) => Some(1),
        (false, _) if has(person, RECRUITERS, false) => Some(2),
        (false, _) => None,
    };
    let mut ranked: Vec<(u8, &Person)> = people
        .iter()
        .filter_map(|person| rank(person).map(|rank| (rank, person)))
        .collect();
    // Stable, so people of one rank keep the page's order.
    ranked.sort_by_key(|(rank, _)| *rank);
    ranked.into_iter().take(2).map(|(_, person)| person.clone()).collect()
}

/// The verdict, from the three sources in the order the user set: the posting, the connections, the People page.
pub fn settle(named: &Named, linked: Vec<Person>, lookup: Option<Lookup>) -> (Verdict, Vec<Person>, Option<String>) {
    if !linked.is_empty() {
        return (Verdict::Connection, linked, None);
    }
    let note = match &lookup {
        Some(Lookup::WrongPage(why) | Lookup::Deferred(why)) => Some(why.clone()),
        _ => None,
    };
    if !named.people.is_empty() {
        let people = match lookup {
            Some(Lookup::Found(found)) if !found.is_empty() => found,
            // Without a profile link no draft can be offered, so the note says why.
            _ => named
                .people
                .iter()
                .take(2)
                .map(|person| Person {
                    name: person.name.clone(),
                    title: person.role.clone(),
                    profile_url: None,
                    degree: None,
                })
                .collect(),
        };
        let note = if people.iter().any(|person| person.profile_url.is_none()) {
            let missing = format!("no LinkedIn profile link found for {}", people[0].name);
            Some(note.map_or(missing.clone(), |note| format!("{note}; {missing}")))
        } else {
            note
        };
        return (Verdict::HiringManager, people, note);
    }
    match lookup {
        Some(Lookup::Found(found)) if !found.is_empty() => (Verdict::TeamMember, found, None),
        Some(Lookup::Found(_) | Lookup::WrongPage(_)) => (Verdict::Cold, Vec::new(), note),
        Some(Lookup::Deferred(_)) | None => (Verdict::Deferred, Vec::new(), note),
    }
}

const NAMED_INSTRUCTIONS: &str = "You read one job posting. Report only what the posting text says. \
team is the name of the team or group the job is on, as the posting names it, or null when it names none. \
function is the kind of work in two or three words, such as backend engineering or ML infrastructure. \
people lists every hiring manager, recruiter or team lead the posting names as a person, with their role; \
leave it empty when it names nobody. Never guess a name.";

fn named_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "team": { "type": ["string", "null"] },
            "function": { "type": "string" },
            "people": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": { "name": { "type": "string" }, "role": { "type": "string" } },
                    "required": ["name", "role"],
                    "additionalProperties": false,
                },
            },
        },
        "required": ["team", "function", "people"],
        "additionalProperties": false,
    })
}

/// Reads the posting for its team and the people it names. A posting that cannot be read names nobody.
async fn named_in(posting: &Posting) -> Result<Named, String> {
    let address = posting
        .address
        .clone()
        .unwrap_or_else(|| posting_address(&posting.link));
    let description = match description::fetch(&address).await? {
        Fetched::Open(description) => description,
        Fetched::Closed => return Err("the posting is closed".into()),
    };
    let request = format!(
        "Company: {}\nTitle: {}\n\n{}",
        posting.company, posting.title, description.text
    );
    let reply = providers::run(Job::Scoring, NAMED_INSTRUCTIONS, request, &named_schema()).await?;
    serde_json::from_value(reply).map_err(|error| format!("the model's reply was not in the agreed form: {error}"))
}

/// What one check run did.
#[derive(Debug, Default)]
pub struct CheckRun {
    pub checked: Vec<Check>,
    pub pages_loaded: usize,
    /// Why LinkedIn lookups stopped early, when they did.
    pub stopped: Option<String>,
}

impl CheckRun {
    pub fn summary(&self) -> String {
        let found = self.checked.iter().filter(|check| check.verdict.found_people()).count();
        let deferred = self
            .checked
            .iter()
            .filter(|check| check.verdict == Verdict::Deferred)
            .count();
        let mut line = format!(
            "outreach checked for {} postings ({found} with people, {deferred} deferred, {} LinkedIn pages)",
            self.checked.len(),
            self.pages_loaded
        );
        if let Some(stopped) = &self.stopped {
            let _ = write!(line, "; LinkedIn stopped: {stopped}");
        }
        line
    }
}

/// Runs the outreach check for every posting that needs one. With no `bridge`, or once the
/// lookup cap is spent, People-page lookups are deferred to a later run.
///
/// `only` checks just the postings with those links, whatever their earlier verdicts.
pub async fn check_postings(database: &Path, bridge: Option<Arc<Bridge>>, only: &[String]) -> Result<CheckRun, String> {
    let said = |error: StoreError| format!("the database failed: {error}");
    let (due, deferred, connections, mut pages) = {
        let store = Store::open(database).map_err(said)?;
        let checks = store.checks().map_err(said)?;
        let postings = store.postings().map_err(said)?;
        let due = if only.is_empty() {
            to_check(&postings, &checks)
        } else {
            postings
                .into_iter()
                .filter(|posting| only.contains(&posting.link))
                .collect()
        };
        let deferred: HashSet<String> = checks
            .into_iter()
            .filter(|check| check.verdict == Verdict::Deferred)
            .map(|check| check.posting_link)
            .collect();
        (
            due,
            deferred,
            store.connections().map_err(said)?,
            store.company_pages().map_err(said)?,
        )
    };
    let mut run = CheckRun::default();
    let mut reader: Option<Reader> = None;
    let mut lookups = bridge.map(|bridge| (bridge, LOOKUP_CAP));
    for posting in due {
        // A deferred posting only waits for a People lookup, so without one left reading it again is wasted.
        let can_look_up = matches!(lookups, Some((_, left)) if left > 0);
        if only.is_empty() && deferred.contains(&posting.link) && !can_look_up {
            continue;
        }
        let (named, mut note) = match named_in(&posting).await {
            Ok(named) => (named, None),
            Err(error) => (Named::default(), Some(format!("posting not read: {error}"))),
        };
        let linked = connections_at(&posting.company, named.team.as_deref(), &connections);
        let lookup = if linked.is_empty() {
            Some(match lookups.as_mut() {
                None => Lookup::Deferred("Chrome is not connected".into()),
                Some((_, 0)) => Lookup::Deferred("the run reached its cap of People lookups".into()),
                Some((bridge, left)) => {
                    *left -= 1;
                    let query = people_query(&named);
                    let key = company_key(&posting.company);
                    // Only a slug some page links is kept: a guess can land on another company of the same name.
                    let (slug, linked) = match pages.get(&key) {
                        Some(slug) => (slug.clone(), false),
                        None => match found_page(&posting).await {
                            Some(slug) => (slug, true),
                            None => (company_slug(&posting.company), false),
                        },
                    };
                    match look_up(&mut reader, bridge, &posting.company, &slug).await {
                        Ok(Lookup::Found(people)) => {
                            if linked {
                                Store::open(database)
                                    .and_then(|store| {
                                        store.save_company_page(&posting.company, &slug, Timestamp::now())
                                    })
                                    .map_err(said)?;
                                pages.insert(key, slug);
                            }
                            Lookup::Found(picked(&query, &named, &people))
                        }
                        Ok(other) => other,
                        Err(stop) => {
                            if !matches!(stop, Stop::CapReached) {
                                // A sign-in wall or a failure ends lookups for this run.
                                run.stopped = Some(stop.to_string());
                                lookups = None;
                            }
                            Lookup::Deferred(stop.to_string())
                        }
                    }
                }
            })
        } else {
            None
        };
        let (verdict, people, why) = settle(&named, linked, lookup);
        note = match (note, why) {
            (Some(one), Some(two)) => Some(format!("{one}; {two}")),
            (one, two) => one.or(two),
        };
        let check = Check {
            posting_link: posting.link.clone(),
            verdict,
            team: named.team.clone(),
            people,
            note,
            checked_at: Timestamp::now(),
        };
        Store::open(database)
            .and_then(|store| store.save_check(&check))
            .map_err(said)?;
        run.checked.push(check);
    }
    if let Some(reader) = reader {
        run.pages_loaded = reader.loaded();
        reader.close().await;
    }
    Ok(run)
}

/// Finds the company's LinkedIn page without loading LinkedIn: in the page that listed the posting
/// (Simplify names the company's page), in the posting's own page, or in the company website a
/// posting page names. `None` when none links one.
async fn found_page(posting: &Posting) -> Option<String> {
    let mut pages = vec![posting_address(&posting.link)];
    if let Some(address) = posting.address.clone().filter(|address| !pages.contains(address)) {
        pages.push(address);
    }
    let mut website = None;
    for address in pages {
        let Ok((_, page)) = description::get(&address).await else {
            continue;
        };
        if let Some(slug) = linkedin::company_pages_in(&page).into_iter().next() {
            return Some(slug);
        }
        website = website.or_else(|| linkedin::company_website_in(&page));
    }
    let (_, site) = description::get(&website?).await.ok()?;
    linkedin::company_pages_in(&site).into_iter().next()
}

/// Loads the company's People page at `slug`, opening the reader on the first lookup of the run.
async fn look_up(reader: &mut Option<Reader>, bridge: &Arc<Bridge>, company: &str, slug: &str) -> Result<Lookup, Stop> {
    if reader.is_none() {
        *reader = Some(Reader::open(bridge.clone(), LOOKUP_CAP).await?);
    }
    let reader = reader.as_mut().expect("the reader was just opened");
    let html = reader.load(&people_address(slug), false).await?;
    let page = people_page(&html);
    match page.company {
        Some(name) if same_company(&name, company) => Ok(Lookup::Found(page.people)),
        Some(name) => Ok(Lookup::WrongPage(format!(
            "linkedin.com/company/{slug} is {name}, not {company}"
        ))),
        None => Ok(Lookup::WrongPage(format!(
            "no company page at linkedin.com/company/{slug}"
        ))),
    }
}

/// What the model read in a hiring post.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct PostReading {
    pub hiring: bool,
    pub role: String,
    pub company: String,
    pub software_engineering: bool,
    pub senior_only: bool,
    #[serde(flatten)]
    pub fit: Reading,
}

/// Why a post gets no draft, or `None` when it is worth one. The scoring rules apply as for a posting.
pub fn post_skip(reading: &PostReading) -> Option<String> {
    if !reading.hiring {
        return Some("not a hiring post".into());
    }
    if !reading.software_engineering {
        return Some("not a software engineering role".into());
    }
    if reading.senior_only {
        return Some("senior roles only".into());
    }
    match score::decide(&reading.fit).0 {
        Decision::Skip(reason) => Some(reason),
        Decision::Review | Decision::ApplyReady => None,
    }
}

const POST_INSTRUCTIONS: &str = " This time the posting is a LinkedIn post written by a person. \
hiring is true only when the post announces an open role at a company. \
role is the role's title as the post names it; when it names several, the one that fits the applicant best. \
company is the company that is hiring, from the post, or from the author's headline when the post does not say. \
software_engineering is true for software engineering roles, including backend, infrastructure, ML infrastructure \
and agentic AI work; it is false for sales, design, data analysis, research science and other roles. \
senior_only is true when every role in the post is at senior, staff, principal, lead or manager level.";

fn post_schema() -> Value {
    let mut schema = score::reading_schema();
    let properties = schema["properties"]
        .as_object_mut()
        .expect("the reading schema has properties");
    for (name, kind) in [
        ("hiring", "boolean"),
        ("role", "string"),
        ("company", "string"),
        ("software_engineering", "boolean"),
        ("senior_only", "boolean"),
    ] {
        properties.insert(name.into(), json!({ "type": kind }));
    }
    let required = schema["required"]
        .as_array_mut()
        .expect("the reading schema lists its fields");
    for name in ["hiring", "role", "company", "software_engineering", "senior_only"] {
        required.push(json!(name));
    }
    schema
}

fn profile_text(profile: &Profile) -> String {
    let mut text = String::from("The applicant's resume:\n");
    for line in &profile.background {
        let _ = writeln!(text, "{line}");
    }
    text.push_str("\nFacts about the applicant:\n");
    for fact in &profile.facts {
        let _ = writeln!(text, "- {fact}");
    }
    text
}

async fn read_post(post: &Post, profile: &Profile) -> Result<PostReading, String> {
    let mut request = profile_text(profile);
    request.push_str("\nThe jobs the applicant is looking for:\n");
    for line in &profile.target {
        let _ = writeln!(request, "- {line}");
    }
    let _ = write!(
        request,
        "\nThe post, by {} ({}):\n{}\n",
        post.poster, post.headline, post.text
    );
    let instructions = format!("{}{POST_INSTRUCTIONS}", score::INSTRUCTIONS);
    let reply = providers::run(Job::Scoring, &instructions, request, &post_schema()).await?;
    serde_json::from_value(reply).map_err(|error| format!("the model's reply was not in the agreed form: {error}"))
}

/// Makes a draft safe to type, or says why it cannot be used. Dashes that read as machine-written
/// become hyphens; a connection note must fit LinkedIn's limit; a placeholder means a missing fact.
pub fn check_draft(text: &str, slot: Slot) -> Result<String, String> {
    let mut text = text.trim().to_owned();
    for dash in ['\u{2014}', '\u{2013}'] {
        text = text.replace(&format!(" {dash} "), " - ").replace(dash, " - ");
    }
    let text = text
        .lines()
        .map(|line| {
            line.split(' ')
                .filter(|word| !word.is_empty())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect::<Vec<_>>()
        .join("\n");
    if text.is_empty() {
        return Err("the draft is empty".into());
    }
    if text.contains(['[', ']', '{', '}']) {
        return Err("the draft has a placeholder in brackets, so a fact is missing".into());
    }
    let length = text.chars().count();
    match slot {
        Slot::Note if length > NOTE_LIMIT => Err(format!(
            "the note has {length} characters; a connection note takes at most {NOTE_LIMIT}"
        )),
        Slot::InMail if length > INMAIL_BODY_LIMIT => Err(format!(
            "the InMail has {length} characters; LinkedIn takes at most {INMAIL_BODY_LIMIT}"
        )),
        _ => Ok(text),
    }
}

/// Makes an InMail subject safe to type: one line, no machine dashes, within the limit.
pub fn check_subject(subject: &str) -> Result<String, String> {
    let subject = check_draft(subject, Slot::Message)?.replace('\n', " ");
    let length = subject.chars().count();
    if length > INMAIL_SUBJECT_LIMIT {
        return Err(format!(
            "the subject has {length} characters; keep it to {INMAIL_SUBJECT_LIMIT}"
        ));
    }
    Ok(subject)
}

/// What the model wrote: the text, and a subject for an InMail.
#[derive(Clone, Debug, PartialEq)]
pub struct Written {
    pub subject: Option<String>,
    pub text: String,
}

/// Checks a whole draft for its slot. An InMail needs a subject; nothing else has one.
pub fn check_written(subject: Option<&str>, text: &str, slot: Slot) -> Result<Written, String> {
    let text = check_draft(text, slot)?;
    let subject = match (slot, subject.map(str::trim).filter(|subject| !subject.is_empty())) {
        (Slot::InMail, Some(subject)) => Some(check_subject(subject)?),
        (Slot::InMail, None) => return Err("the InMail has no subject".into()),
        _ => None,
    };
    Ok(Written { subject, text })
}

const DRAFT_INSTRUCTIONS: &str = "You draft one short LinkedIn message from the applicant to one person, \
for the applicant to approve and send. Write in the first person, as the applicant, plainly and specifically. \
Refer to the exact role, team or post it is about. \
Every claim about the applicant must match the resume or the facts exactly: \
the same titles, employers, dates and projects, with nothing inflated. \
Never invent experience, skills, dates, numbers, links or email addresses, \
and never describe work the resume does not list as built work. \
Never use em dashes or en dashes; use commas, hyphens or separate sentences. \
No placeholders and no bullet lists. Put no subject line in the text: \
an InMail's subject goes in subject, and anything else has subject null. \
If the resume and the facts lack something the message needs, text is null and missing says what is missing.";

fn draft_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "subject": { "type": ["string", "null"] },
            "text": { "type": ["string", "null"] },
            "missing": { "type": ["string", "null"] },
        },
        "required": ["subject", "text", "missing"],
        "additionalProperties": false,
    })
}

/// Who a draft is for and what it is about, as the model reads it.
pub struct Ask<'a> {
    pub slot: Slot,
    pub person: &'a str,
    pub headline: &'a str,
    pub context: String,
}

/// What the model is told about the form of a draft in `slot`.
pub fn form_of(slot: Slot) -> String {
    match slot {
        Slot::Note => format!(
            "This is a connection request note: at most {} characters, two or three sentences, no attachment.",
            NOTE_LIMIT - 20
        ),
        Slot::Message => "This is a direct message to a connection: 50 to 110 words. \
                          It may say the resume is attached."
            .to_owned(),
        Slot::InMail => format!(
            "This is a LinkedIn InMail to someone outside the applicant's network: a subject of at most \
             {INMAIL_SUBJECT_LIMIT} characters that names the role, and a body of 80 to 150 words. \
             It may say the resume is attached."
        ),
    }
}

/// Asks the model for a draft and checks it. A draft that fails the check is asked for once more.
pub async fn write_draft(profile: &Profile, ask: &Ask<'_>) -> Result<Written, String> {
    let form = form_of(ask.slot);
    let mut request = profile_text(profile);
    let _ = write!(
        request,
        "\n{form}\nIt goes to {} ({}).\n\n{}\n",
        ask.person, ask.headline, ask.context
    );
    let mut problem = None;
    for _ in 0..2 {
        let asked = match &problem {
            Some(problem) => format!("{request}\nThe last draft could not be used: {problem}. Fix that.\n"),
            None => request.clone(),
        };
        let reply = providers::run(Job::Forms, DRAFT_INSTRUCTIONS, asked, &draft_schema()).await?;
        let text = match (reply["text"].as_str(), reply["missing"].as_str()) {
            (Some(text), _) if !text.trim().is_empty() => text.to_owned(),
            (_, Some(missing)) => return Err(format!("missing to draft: {missing}")),
            _ => return Err("the model wrote no draft".into()),
        };
        match check_written(reply["subject"].as_str(), &text, ask.slot) {
            Ok(written) => return Ok(written),
            Err(why) => problem = Some(why),
        }
    }
    Err(problem.unwrap_or_default())
}

/// The day in Pacific time, which the daily cap counts by.
pub fn pacific_day(at: Timestamp) -> Date {
    at.in_tz("America/Los_Angeles")
        .expect("the time zone database has Pacific time")
        .date()
}

/// How many hiring-post drafts were made on the Pacific day of `now`.
pub fn drafted_today(drafts: &[Draft], now: Timestamp) -> usize {
    let today = pacific_day(now);
    drafts
        .iter()
        .filter(|draft| matches!(draft.about, About::Post(_)) && pacific_day(draft.created_at) == today)
        .count()
}

/// A poster and role reduced so the same post under another LinkedIn id still matches.
pub fn poster_role_key(poster: &str, role: &str) -> String {
    format!("{}|{}", company_key(poster), company_key(role))
}

/// What one sweep did.
#[derive(Debug, Default)]
pub struct SweepRun {
    pub found: usize,
    pub read: usize,
    pub skipped: usize,
    /// Posts worth a note that got none: the poster check failed, or the daily cap was spent.
    pub flagged: usize,
    pub drafted: usize,
    pub pages_loaded: usize,
    pub stopped: Option<String>,
}

impl SweepRun {
    pub fn summary(&self) -> String {
        let mut line = format!(
            "hiring posts: {} found, {} read, {} skipped, {} flagged, {} drafted ({} LinkedIn pages)",
            self.found, self.read, self.skipped, self.flagged, self.drafted, self.pages_loaded
        );
        if let Some(stopped) = &self.stopped {
            let _ = write!(line, "; LinkedIn stopped: {stopped}");
        }
        line
    }
}

/// Searches LinkedIn for hiring posts from the past day, reads each new one in full, and drafts
/// a note for each post worth one, up to the daily cap. Nothing is typed or sent.
pub async fn sweep(database: &Path, profile: &Profile, bridge: Arc<Bridge>) -> Result<SweepRun, String> {
    let said = |error: StoreError| format!("the database failed: {error}");
    let mut run = SweepRun::default();
    let mut reader = Reader::open(bridge, SEARCHES.len())
        .await
        .map_err(|stop| stop.to_string())?;
    let mut posts: Vec<Post> = Vec::new();
    for search in SEARCHES {
        match reader.load(&post_search_address(search), true).await {
            Ok(html) => {
                for post in posts_page(&html) {
                    if !posts.iter().any(|seen| seen.link() == post.link()) {
                        posts.push(post);
                    }
                }
            }
            Err(stop) => {
                run.stopped = Some(stop.to_string());
                break;
            }
        }
    }
    run.pages_loaded = reader.loaded();
    reader.close().await;
    run.found = posts.len();

    let (known_links, mut known_pairs, mut drafted) = {
        let store = Store::open(database).map_err(said)?;
        let known = store.hiring_posts().map_err(said)?;
        let links: HashSet<String> = known.iter().map(|post| post.post_link.clone()).collect();
        let pairs: HashSet<String> = known
            .iter()
            .map(|post| poster_role_key(&post.poster, &post.role))
            .collect();
        (
            links,
            pairs,
            drafted_today(&store.drafts().map_err(said)?, Timestamp::now()),
        )
    };
    let fresh = posts.into_iter().filter(|post| {
        // A page's post has no one to write to, and a cut-short post cannot be judged.
        post.complete && post.poster_url.is_some() && !known_links.contains(&post.link())
    });
    for post in fresh.take(POSTS_READ) {
        let Ok(reading) = read_post(&post, profile).await else {
            continue;
        };
        run.read += 1;
        let pair = poster_role_key(&post.poster, &reading.role);
        if !known_pairs.insert(pair) {
            continue;
        }
        let mut kept = HiringPost {
            post_link: post.link(),
            role: reading.role.clone(),
            found_at: Timestamp::now(),
            poster: post.poster.clone(),
            company: reading.company.clone(),
            score: reading.fit.score,
            variant: reading.fit.variant.clone(),
            outreach: Outreach::NotStarted,
            notes: None,
        };
        let draft = if let Some(reason) = post_skip(&reading) {
            run.skipped += 1;
            kept.outreach = Outreach::Skipped;
            kept.notes = Some(format!("Skipped - {reason}"));
            None
        } else if let Some(flag) = poster_flag(&post, &reading.company) {
            run.flagged += 1;
            kept.notes = Some(flag);
            None
        } else if drafted >= DRAFTS_PER_DAY {
            run.flagged += 1;
            kept.notes = Some(format!(
                "Not drafted: the cap of {DRAFTS_PER_DAY} drafts a day was reached"
            ));
            None
        } else {
            let slot = Slot::for_degree(post.degree.as_deref());
            let ask = Ask {
                slot,
                person: &post.poster,
                headline: &post.headline,
                context: format!(
                    "It is about this LinkedIn post, for the {} role at {}:\n{}",
                    reading.role, reading.company, post.text
                ),
            };
            match write_draft(profile, &ask).await {
                Ok(written) => {
                    drafted += 1;
                    run.drafted += 1;
                    kept.outreach = Outreach::Drafted;
                    kept.notes = Some(format!(
                        "{}, {}: {}",
                        reading.fit.score, reading.fit.variant, reading.fit.reason
                    ));
                    let now = Timestamp::now();
                    Some(Draft {
                        id: 0,
                        about: About::Post(post.link()),
                        person: post.poster.clone(),
                        headline: post.headline.clone(),
                        profile_url: post.poster_url.clone().expect("posts without an author were left out"),
                        slot,
                        subject: written.subject,
                        text: written.text,
                        status: Outreach::Drafted,
                        created_at: now,
                        updated_at: now,
                    })
                }
                Err(why) => {
                    run.flagged += 1;
                    kept.notes = Some(format!("Not drafted: {why}"));
                    None
                }
            }
        };
        let store = Store::open(database).map_err(said)?;
        store.add_hiring_post(&kept).map_err(said)?;
        if let Some(draft) = draft {
            store.add_draft(&draft).map_err(said)?;
        }
    }
    Ok(run)
}

/// The flag for a post whose author does not clearly work at the hiring company. No draft goes out for it.
fn poster_flag(post: &Post, company: &str) -> Option<String> {
    match linkedin::poster_check(&post.headline, company) {
        PosterCheck::WorksThere => None,
        PosterCheck::Elsewhere => Some(format!(
            "Poster check failed: the headline ({}) names another employer than {company}; no draft",
            post.headline
        )),
        PosterCheck::Unclear => Some(format!(
            "Poster check unclear: the headline ({}) names no employer; no draft",
            post.headline
        )),
    }
}

/// Drafts a note to the first person a check found who has a profile, for the user to approve.
pub async fn draft_for_check(database: &Path, profile: &Profile, posting_link: &str) -> Result<Draft, String> {
    let said = |error: StoreError| format!("the database failed: {error}");
    let (posting, check) = {
        let store = Store::open(database).map_err(said)?;
        let posting = store
            .postings()
            .map_err(said)?
            .into_iter()
            .find(|posting| posting.link == posting_link)
            .ok_or("the posting is no longer in the database")?;
        let check = store
            .checks()
            .map_err(said)?
            .into_iter()
            .find(|check| check.posting_link == posting_link)
            .ok_or("the posting has no outreach check yet")?;
        (posting, check)
    };
    let person = check
        .people
        .iter()
        .find(|person| person.profile_url.is_some())
        .ok_or("nobody the check found has a LinkedIn profile to write to")?;
    let slot = Slot::for_degree(person.degree.as_deref());
    let team = check
        .team
        .as_deref()
        .map(|team| format!(" on the {team} team"))
        .unwrap_or_default();
    let ask = Ask {
        slot,
        person: &person.name,
        headline: &person.title,
        context: format!(
            "The applicant is applying to the {} role{team} at {}. {} works there as {}.",
            posting.title, posting.company, person.name, person.title
        ),
    };
    let written = write_draft(profile, &ask).await?;
    let now = Timestamp::now();
    let mut draft = Draft {
        id: 0,
        about: About::Posting(posting.link.clone()),
        person: person.name.clone(),
        headline: person.title.clone(),
        profile_url: person.profile_url.clone().expect("the person was picked for a profile"),
        slot,
        subject: written.subject,
        text: written.text,
        status: Outreach::Drafted,
        created_at: now,
        updated_at: now,
    };
    let store = Store::open(database).map_err(said)?;
    draft.id = match store.add_draft(&draft).map_err(said)? {
        Some(id) => id,
        None => return Err(format!("{} already has a draft about this posting", person.name)),
    };
    Ok(draft)
}

/// Rewrites a waiting draft to someone outside the network in the other form: a note becomes an
/// InMail and back. The model keeps to the earlier draft's facts and the resume.
pub async fn redraft(database: &Path, profile: &Profile, id: i64) -> Result<Draft, String> {
    let said = |error: StoreError| format!("the database failed: {error}");
    let store = Store::open(database).map_err(said)?;
    let mut draft = store
        .draft(id)
        .map_err(said)?
        .ok_or("the draft is no longer in the database")?;
    if draft.status != Outreach::Drafted {
        return Err("only a draft still waiting for approval can change its form".into());
    }
    let slot = draft
        .slot
        .switched()
        .ok_or("a message to a connection has no other form")?;
    let about = match &draft.about {
        About::Posting(link) => store
            .postings()
            .map_err(said)?
            .into_iter()
            .find(|posting| &posting.link == link)
            .map(|posting| format!("the {} role at {}", posting.title, posting.company)),
        About::Post(link) => store
            .hiring_posts()
            .map_err(said)?
            .into_iter()
            .find(|post| &post.post_link == link)
            .map(|post| {
                format!(
                    "the {} role at {}, which {} posted about",
                    post.role, post.company, draft.person
                )
            }),
    }
    .ok_or("what the draft is about is no longer in the database")?;
    let ask = Ask {
        slot,
        person: &draft.person,
        headline: &draft.headline,
        context: redraft_context(&about, &draft),
    };
    let written = write_draft(profile, &ask).await?;
    let now = Timestamp::now();
    store.set_draft_form(id, slot, &written, now).map_err(said)?;
    draft.slot = slot;
    draft.subject = written.subject;
    draft.text = written.text;
    draft.updated_at = now;
    Ok(draft)
}

/// What the model reads when it rewrites `draft` in another form.
pub fn redraft_context(about: &str, draft: &Draft) -> String {
    format!(
        "The applicant is reaching out about {about}. An earlier draft in another form said:\n{}\n\
         Keep to its facts and add none the resume and the facts do not hold.",
        draft.text
    )
}

/// Types an approved draft into LinkedIn in the user's Chrome and marks it typed. The user clicks Send.
pub async fn type_approved(database: &Path, bridge: Arc<Bridge>, id: i64) -> Result<(), String> {
    let said = |error: StoreError| format!("the database failed: {error}");
    let draft = Store::open(database)
        .and_then(|store| store.draft(id))
        .map_err(said)?
        .ok_or("the draft is no longer in the database")?;
    let written = check_written(draft.subject.as_deref(), &draft.text, draft.slot)?;
    linkedin::type_draft(
        bridge,
        &draft.profile_url,
        draft.slot,
        written.subject.as_deref(),
        &written.text,
    )
    .await?;
    Store::open(database)
        .and_then(|mut store| store.set_draft_status(id, Outreach::Typed, Timestamp::now()))
        .map_err(said)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::Sponsorship;

    fn posting(link: &str, score: u8, notes: &str) -> Posting {
        Posting {
            link: link.into(),
            found_at: "2026-11-22T17:00:00Z".parse().unwrap(),
            source: "Simplify".into(),
            company: "Example Co".into(),
            title: "Backend Engineer".into(),
            location: None,
            email_subject: None,
            stage: Stage::Review,
            score: Some(score),
            variant: Some("Backend/Infra".into()),
            flags: None,
            notes: Some(notes.into()),
            address: None,
            sponsorship: None,
        }
    }

    fn connection(first: &str, company: &str, position: &str) -> Connection {
        Connection {
            first_name: first.into(),
            last_name: "Sample".into(),
            profile_url: Some(format!("https://www.linkedin.com/in/{}-sample/", first.to_lowercase())),
            email: None,
            company: company.into(),
            position: position.into(),
            connected_on: None,
        }
    }

    fn check(link: &str, verdict: Verdict) -> Check {
        Check {
            posting_link: link.into(),
            verdict,
            team: None,
            people: Vec::new(),
            note: None,
            checked_at: "2026-11-22T18:00:00Z".parse().unwrap(),
        }
    }

    #[test]
    fn apply_ready_postings_are_checked_first_and_settled_ones_are_not_checked_again() {
        let postings = [
            posting("a", 70, "Review - 70"),
            posting("b", 90, &format!("{APPLY_READY} 90")),
            posting("c", 80, &format!("{APPLY_READY} 80")),
            posting("d", 72, "Review - 72"),
            posting("e", 88, "Review - 88"),
            Posting {
                stage: Stage::Skipped,
                ..posting("f", 40, "Skipped - 40")
            },
        ];
        let checks = [check("e", Verdict::Cold), check("d", Verdict::Deferred)];
        let order: Vec<String> = to_check(&postings, &checks)
            .into_iter()
            .map(|posting| posting.link)
            .collect();
        assert_eq!(order, ["b", "c", "d", "a"]);
    }

    #[test]
    fn connections_at_the_company_count_and_the_team_comes_first() {
        let connections = [
            connection("Ada", "Example Co", "Recruiter"),
            connection("Bo", "Other LLC", "Storage Engineer"),
            connection("Cy", "Example Co, Inc.", "Software Engineer, Storage"),
            connection("Di", "Example", "Product Manager"),
        ];
        let found = connections_at("Example Co", Some("Storage team"), &connections);
        let names: Vec<&str> = found.iter().map(|person| person.name.as_str()).collect();
        assert_eq!(names, ["Cy Sample", "Ada Sample"]);
        assert_eq!(found[0].degree.as_deref(), Some("1st"));
        assert!(connections_at("Nowhere Inc", None, &connections).is_empty());
    }

    fn person(name: &str, title: &str) -> Person {
        Person {
            name: name.into(),
            title: title.into(),
            profile_url: Some(format!("https://www.linkedin.com/in/{}/", name.to_lowercase())),
            degree: Some("2nd".into()),
        }
    }

    #[test]
    fn the_verdict_follows_the_order_connection_then_named_then_team() {
        let named = Named {
            team: Some("Storage".into()),
            function: "backend engineering".into(),
            people: Vec::new(),
        };
        let linked = vec![person("Ada", "Engineer")];
        assert_eq!(settle(&named, linked.clone(), None).0, Verdict::Connection);

        let found = Lookup::Found(vec![person("Bo", "Storage Engineer")]);
        let (verdict, people, _) = settle(&named, Vec::new(), Some(found));
        assert_eq!((verdict, people.len()), (Verdict::TeamMember, 1));

        let (verdict, people, note) = settle(&named, Vec::new(), Some(Lookup::Found(Vec::new())));
        assert_eq!((verdict, people.len(), note), (Verdict::Cold, 0, None));

        let deferred = Lookup::Deferred("the run reached its cap of People lookups".into());
        let (verdict, _, note) = settle(&named, Vec::new(), Some(deferred));
        assert_eq!(verdict, Verdict::Deferred);
        assert!(note.unwrap().contains("cap"));

        let wrong = Lookup::WrongPage("no company page at linkedin.com/company/example-co".into());
        assert_eq!(settle(&named, Vec::new(), Some(wrong)).0, Verdict::Cold);

        let manager = Named {
            people: vec![NamedPerson {
                name: "Cy Lead".into(),
                role: "Engineering Manager".into(),
            }],
            ..named
        };
        let (verdict, people, note) = settle(&manager, Vec::new(), Some(Lookup::Found(Vec::new())));
        assert_eq!(verdict, Verdict::HiringManager);
        assert_eq!(people[0].title, "Engineering Manager");
        assert_eq!(people[0].profile_url, None);
        assert_eq!(note.as_deref(), Some("no LinkedIn profile link found for Cy Lead"));
        assert_eq!(people_query(&manager), "Cy Lead");
    }

    #[test]
    fn a_people_search_keeps_titles_that_fit_or_the_person_named() {
        let results = [
            person("Ada", "Account Executive"),
            person("Bo", "Software Engineer, Search"),
            person("Cy", "Storage Platform Lead"),
            person("Di", "Recruiting Partner"),
            person("Ed", "Senior Engineer, Storage"),
        ];
        let named = Named {
            team: Some("Storage".into()),
            ..Named::default()
        };
        let names = |query: &str, named: &Named, people: &[Person]| -> Vec<String> {
            picked(query, named, people)
                .into_iter()
                .map(|person| person.name)
                .collect()
        };
        // An engineer on the team first, then another engineer; Cy names the team but is no engineer.
        assert_eq!(names("Storage", &named, &results), ["Ed", "Bo"]);
        // A weak team word picks no one by itself, "swe" is not "Sweden" and "Engineered" is no engineer.
        let others = [
            person("Ed", "Senior Analyst, Enterprise Cybersecurity"),
            person("Ha", "Supply Chain Analyst at Example Co | MS Systems Engineering"),
            person("Fa", "Sales Lead, Sweden"),
            person("Jo", "Ads, Engineered | Marketing at Example Co"),
            person("Di", "Recruiting Partner"),
            person("Gi", "SWE II"),
        ];
        let enterprise = Named {
            team: Some("Enterprise AI".into()),
            ..Named::default()
        };
        assert_eq!(names("Enterprise AI", &enterprise, &others), ["Gi", "Di"]);
        assert!(names("Enterprise AI", &enterprise, &others[..4]).is_empty());
        let manager = Named {
            people: vec![NamedPerson {
                name: "di".into(),
                role: "Recruiter".into(),
            }],
            ..Named::default()
        };
        assert_eq!(picked("di", &manager, &results)[0].name, "Di");
    }

    #[test]
    fn the_summary_names_the_people_and_a_deferred_team() {
        let mut found = check("a", Verdict::TeamMember);
        found.people = vec![person("Bo", "Storage Engineer")];
        assert_eq!(found.summary(), "Team member found: Bo, Storage Engineer (2nd)");
        let mut deferred = check("b", Verdict::Deferred);
        deferred.team = Some("Storage".into());
        assert_eq!(deferred.summary(), "Team named (Storage), people lookup deferred");
        assert_eq!(check("c", Verdict::Cold).summary(), "None, apply cold");
    }

    fn reading(score: u8) -> PostReading {
        PostReading {
            hiring: true,
            role: "Backend Engineer, New Grad".into(),
            company: "Example Co".into(),
            software_engineering: true,
            senior_only: false,
            fit: Reading {
                score,
                variant: "Backend/Infra".into(),
                reason: "Go services".into(),
                sponsorship: Sponsorship::Unknown,
                citizenship_required: false,
                min_years: None,
                primarily_frontend: false,
                us_location: true,
                advisory: Vec::new(),
            },
        }
    }

    #[test]
    fn posts_follow_the_scoring_rules_and_the_post_rules() {
        assert_eq!(post_skip(&reading(80)), None);
        assert_eq!(post_skip(&reading(65)), None);
        assert!(post_skip(&reading(55)).unwrap().starts_with("55"));
        let gated = PostReading {
            fit: Reading {
                citizenship_required: true,
                ..reading(90).fit
            },
            ..reading(90)
        };
        assert_eq!(post_skip(&gated).unwrap(), "US citizenship or clearance required");
        let senior = PostReading {
            senior_only: true,
            ..reading(90)
        };
        assert_eq!(post_skip(&senior).unwrap(), "senior roles only");
        let sales = PostReading {
            software_engineering: false,
            ..reading(90)
        };
        assert_eq!(post_skip(&sales).unwrap(), "not a software engineering role");
        let schema = post_schema();
        assert!(schema["required"].as_array().unwrap().contains(&json!("senior_only")));
        assert!(
            schema["required"]
                .as_array()
                .unwrap()
                .contains(&json!("citizenship_required"))
        );
    }

    #[test]
    fn a_draft_loses_its_dashes_and_a_note_keeps_to_the_limit() {
        assert_eq!(
            check_draft("  Hi Sam \u{2014} I saw your post.\u{2013}Thanks ", Slot::Note).unwrap(),
            "Hi Sam - I saw your post. - Thanks"
        );
        let long = "word ".repeat(70);
        assert!(check_draft(&long, Slot::Note).unwrap_err().contains("at most 300"));
        assert!(check_draft(&long, Slot::Message).is_ok());
        assert!(check_draft("Hi [Name], hello", Slot::Message).is_err());
        assert!(check_draft("   ", Slot::Message).is_err());
    }

    #[test]
    fn the_daily_cap_counts_post_drafts_by_the_pacific_day() {
        let draft = |about: About, at: &str| Draft {
            id: 1,
            about,
            person: "Sam Poster".into(),
            headline: String::new(),
            profile_url: "https://www.linkedin.com/in/sam/".into(),
            slot: Slot::Note,
            subject: None,
            text: "Hi".into(),
            status: Outreach::Drafted,
            created_at: at.parse().unwrap(),
            updated_at: at.parse().unwrap(),
        };
        let post = || About::Post("https://www.linkedin.com/feed/update/urn:li:activity:1/".into());
        let drafts = [
            // 11 PM Pacific on the 22nd, so the day before.
            draft(post(), "2026-11-23T07:00:00Z"),
            draft(post(), "2026-11-23T08:30:00Z"),
            draft(post(), "2026-11-23T20:00:00Z"),
            draft(About::Posting("x".into()), "2026-11-23T20:00:00Z"),
        ];
        assert_eq!(drafted_today(&drafts, "2026-11-23T21:00:00Z".parse().unwrap()), 2);
        assert_eq!(
            poster_role_key("Sam  Poster", "Backend Engineer (New Grad)"),
            poster_role_key("sam poster", "backend engineer new grad")
        );
    }

    #[test]
    fn a_company_page_found_once_is_kept_by_the_company_name() {
        let scratch = tempfile::tempdir().unwrap();
        let store = Store::open(&scratch.path().join("maverick.db")).unwrap();
        let at: Timestamp = "2026-11-23T17:00:00Z".parse().unwrap();
        store
            .save_company_page("Example Co, Inc.", "example-co-ai", at)
            .unwrap();
        store.save_company_page("Example Co", "example-co-labs", at).unwrap();
        let pages = store.company_pages().unwrap();
        assert_eq!(pages.len(), 1);
        assert_eq!(
            pages.get(&company_key("Example Co")).map(String::as_str),
            Some("example-co-labs")
        );
    }

    #[tokio::test]
    async fn a_deferred_posting_is_not_read_again_when_no_lookup_can_run() {
        let scratch = tempfile::tempdir().unwrap();
        let database = scratch.path().join("maverick.db");
        let mut store = Store::open(&database).unwrap();
        store
            .add_found(&[posting("https://simplify.jobs/p/1", 80, "Review - 80")])
            .unwrap();
        store
            .save_check(&check("https://simplify.jobs/p/1", Verdict::Deferred))
            .unwrap();
        drop(store);

        let run = check_postings(&database, None, &[]).await.unwrap();

        assert_eq!(run.checked, []);
    }

    #[test]
    fn checks_drafts_and_connections_survive_in_the_store() {
        let scratch = tempfile::tempdir().unwrap();
        let mut store = Store::open(&scratch.path().join("maverick.db")).unwrap();
        let connections = [connection("Ada", "Example Co", "Engineer")];
        assert_eq!(store.replace_connections(&connections).unwrap(), 1);
        assert_eq!(store.replace_connections(&connections).unwrap(), 1);
        assert_eq!(store.connections().unwrap(), connections);

        let mut found = check("https://simplify.jobs/p/1", Verdict::TeamMember);
        found.people = vec![person("Bo", "Storage Engineer")];
        store.save_check(&found).unwrap();
        found.verdict = Verdict::Connection;
        store.save_check(&found).unwrap();
        assert_eq!(store.checks().unwrap(), [found]);

        let link = "https://www.linkedin.com/feed/update/urn:li:activity:1/";
        let post = HiringPost {
            post_link: link.into(),
            role: "Backend Engineer".into(),
            found_at: "2026-11-23T17:00:00Z".parse().unwrap(),
            poster: "Sam Poster".into(),
            company: "Example Co".into(),
            score: 82,
            variant: "Backend/Infra".into(),
            outreach: Outreach::Drafted,
            notes: None,
        };
        assert!(store.add_hiring_post(&post).unwrap());
        assert!(!store.add_hiring_post(&post).unwrap());
        let at: Timestamp = "2026-11-23T17:00:00Z".parse().unwrap();
        let draft = Draft {
            id: 0,
            about: About::Post(link.into()),
            person: "Sam Poster".into(),
            headline: "Engineering Manager at Example Co".into(),
            profile_url: "https://www.linkedin.com/in/sam-poster/".into(),
            slot: Slot::Note,
            subject: None,
            text: "Hi Sam, I saw your post.".into(),
            status: Outreach::Drafted,
            created_at: at,
            updated_at: at,
        };
        let id = store.add_draft(&draft).unwrap().unwrap();
        assert_eq!(store.add_draft(&draft).unwrap(), None);
        store.set_draft_text(id, "Hi Sam, thanks for posting.", at).unwrap();
        store.set_draft_status(id, Outreach::Typed, at).unwrap();
        let saved = store.draft(id).unwrap().unwrap();
        assert_eq!(saved.text, "Hi Sam, thanks for posting.");
        assert_eq!(saved.status, Outreach::Typed);
        assert_eq!(store.hiring_posts().unwrap()[0].outreach, Outreach::Typed);

        // Reset touches the draft only: the hiring post stays typed, and the day's count stays one.
        let today = drafted_today(&store.drafts().unwrap(), at);
        store.reset_draft(id, at).unwrap();
        let saved = store.draft(id).unwrap().unwrap();
        assert_eq!(
            (saved.status, saved.text.as_str(), saved.slot),
            (Outreach::Drafted, "Hi Sam, thanks for posting.", Slot::Note)
        );
        assert_eq!(store.hiring_posts().unwrap()[0].outreach, Outreach::Typed);
        assert_eq!(drafted_today(&store.drafts().unwrap(), at), today);
        assert!(
            store
                .reset_draft(id, at)
                .unwrap_err()
                .contains("only a skipped or typed draft")
        );
        store.set_draft_status(id, Outreach::Skipped, at).unwrap();
        store.reset_draft(id, at).unwrap();
        assert_eq!(store.draft(id).unwrap().unwrap().status, Outreach::Drafted);
        store.set_draft_status(id, Outreach::Sent, at).unwrap();
        assert!(store.reset_draft(id, at).is_err());
        assert!(store.reset_draft(999, at).unwrap_err().contains("no longer"));

        let inmail = Written {
            subject: Some("The backend role on Storage".into()),
            text: "Hi Sam, I am applying to the backend role.".into(),
        };
        store.set_draft_form(id, Slot::InMail, &inmail, at).unwrap();
        let saved = store.draft(id).unwrap().unwrap();
        assert_eq!(saved.slot, Slot::InMail);
        assert_eq!(saved.subject.as_deref(), Some("The backend role on Storage"));
        assert_eq!(saved.text, inmail.text);
    }

    #[test]
    fn an_inmail_needs_a_short_subject_and_a_body_linkedin_takes() {
        let body = "word ".repeat(120);
        let written = check_written(Some("  Backend role \u{2014} Storage "), &body, Slot::InMail).unwrap();
        assert_eq!(written.subject.as_deref(), Some("Backend role - Storage"));
        assert_eq!(written.text, body.trim());
        // A body over the note limit is fine in an InMail, but not past LinkedIn's own limit.
        assert!(check_draft(&"a".repeat(INMAIL_BODY_LIMIT), Slot::InMail).is_ok());
        assert!(
            check_draft(&"a".repeat(INMAIL_BODY_LIMIT + 1), Slot::InMail)
                .unwrap_err()
                .contains("at most 1900")
        );
        assert!(
            check_written(None, "Hi Sam.", Slot::InMail)
                .unwrap_err()
                .contains("no subject")
        );
        assert!(check_written(Some("  "), "Hi Sam.", Slot::InMail).is_err());
        let long = "s".repeat(INMAIL_SUBJECT_LIMIT + 1);
        assert!(
            check_written(Some(&long), "Hi Sam.", Slot::InMail)
                .unwrap_err()
                .contains("subject")
        );
        assert!(check_written(Some("[Role] at Example Co"), "Hi Sam.", Slot::InMail).is_err());
        // Only an InMail keeps a subject.
        assert_eq!(check_written(Some("Hi"), "Hi Sam.", Slot::Note).unwrap().subject, None);
    }

    #[test]
    fn a_redraft_asks_for_the_other_form_and_keeps_the_earlier_facts() {
        assert!(form_of(Slot::InMail).contains("subject of at most 200 characters"));
        assert!(form_of(Slot::InMail).contains("80 to 150 words"));
        assert!(form_of(Slot::Note).contains("at most 280 characters"));
        let at: Timestamp = "2026-11-23T17:00:00Z".parse().unwrap();
        let draft = Draft {
            id: 1,
            about: About::Posting("https://example.test/p/1".into()),
            person: "Bo Sample".into(),
            headline: "Engineer".into(),
            profile_url: "https://www.linkedin.com/in/bo-sample/".into(),
            slot: Slot::Note,
            subject: None,
            text: "Hi Bo, I applied to the backend role.".into(),
            status: Outreach::Drafted,
            created_at: at,
            updated_at: at,
        };
        let context = redraft_context("the Backend Engineer role at Example Co", &draft);
        assert!(context.contains("Backend Engineer role at Example Co"));
        assert!(context.contains("Hi Bo, I applied to the backend role."));
        assert_eq!(draft.slot.switched(), Some(Slot::InMail));
    }
}
