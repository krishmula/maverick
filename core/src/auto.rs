//! Auto-apply: once the user approves a posting, Maverick submits its form when nothing in it is
//! left for the user, sends a draft to the best two people in the US its outreach check found there, and
//! sends each waiting hiring-post draft, without the user's review. The user turns it on in
//! Connections. Maverick never approves a posting itself.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::bridge::Bridge;
use crate::jobs::{Decision, Outreach, Posting, Stage};
use crate::linkedin::{self, Delivery, Pacing};
use crate::outreach::{self, About, Check, Draft};
use crate::platform::{Platform, address_of};
use crate::score::{APPLY_SCORE, Profile};
use crate::store::{Store, StoreError};

/// How many people at one posting Maverick sends to, best first.
pub const PEOPLE_PER_POSTING: usize = 2;
/// The most drafts sent in one Pacific day, the user's own sends included, to keep LinkedIn calm.
pub const SENDS_PER_DAY: usize = 20;
/// The most drafts sent to people at one company in one Pacific day, so a company with many
/// postings does not hear from Maverick about each of them at once.
pub const SENDS_PER_COMPANY_PER_DAY: usize = 2;
/// The most drafts one hourly outreach run sends.
pub const SENDS_PER_RUN: usize = 4;
/// The random wait between two sends.
pub const BETWEEN_SENDS: Pacing = Pacing {
    least: Duration::from_secs(30),
    most: Duration::from_secs(90),
};
/// After this many sends fail in a row, the run stops: the cause is likely LinkedIn or Chrome, not the person.
const FAILURES_IN_A_ROW: usize = 2;

/// Where the switch is kept: a file next to the database that says `on` or `off`.
fn switch_file(database: &Path) -> Option<PathBuf> {
    Some(database.parent()?.join("auto-apply"))
}

/// Whether auto-apply is on. It is off until the user turns it on.
pub fn is_on(database: &Path) -> bool {
    switch_file(database)
        .and_then(|file| std::fs::read_to_string(file).ok())
        .is_some_and(|saved| saved.trim() == "on")
}

pub fn set_on(database: &Path, on: bool) -> std::io::Result<()> {
    let file = switch_file(database).ok_or_else(|| std::io::Error::other("the database has no directory"))?;
    std::fs::write(file, if on { "on" } else { "off" })
}

/// Which postings auto-apply acts on: those on a chosen platform, at a company not left out.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Scope {
    pub platforms: BTreeSet<Platform>,
    /// Companies auto-apply leaves alone, as the postings name them.
    pub left_out: BTreeSet<String>,
}

/// Until the user chooses, auto-apply covers the platforms Maverick can fill, at every company.
impl Default for Scope {
    fn default() -> Self {
        Self {
            platforms: Platform::ALL
                .into_iter()
                .filter(|platform| platform.fillable())
                .collect(),
            left_out: BTreeSet::new(),
        }
    }
}

impl Scope {
    fn file(database: &Path) -> Option<PathBuf> {
        Some(database.parent()?.join("auto-apply-scope.json"))
    }

    /// The saved scope, or the default when none was saved or it cannot be read.
    pub fn load(database: &Path) -> Self {
        Self::file(database)
            .and_then(|file| std::fs::read_to_string(file).ok())
            .and_then(|saved| serde_json::from_str(&saved).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, database: &Path) -> std::io::Result<()> {
        let file = Self::file(database).ok_or_else(|| std::io::Error::other("the database has no directory"))?;
        std::fs::write(file, serde_json::to_string_pretty(self)?)
    }

    pub fn includes_company(&self, company: &str) -> bool {
        let company = company.trim();
        !self.left_out.iter().any(|out| out.eq_ignore_ascii_case(company))
    }

    pub fn covers(&self, posting: &Posting) -> bool {
        self.platforms.contains(&Platform::of(&address_of(posting))) && self.includes_company(&posting.company)
    }

    pub fn toggle_platform(&mut self, platform: Platform) {
        if !self.platforms.remove(&platform) {
            self.platforms.insert(platform);
        }
    }

    pub fn toggle_company(&mut self, company: &str) {
        let company = company.trim();
        let before = self.left_out.len();
        self.left_out.retain(|out| !out.eq_ignore_ascii_case(company));
        if self.left_out.len() == before {
            self.left_out.insert(company.to_owned());
        }
    }
}

/// The companies with postings scored 75 or more still waiting for a decision, with how many each,
/// most first, then by name. Companies left out are listed too, so they can be brought back.
pub fn companies(postings: &[Posting], decisions: &HashMap<String, Decision>, scope: &Scope) -> Vec<(String, usize)> {
    let mut counts: HashMap<String, (String, usize)> = HashMap::new();
    for posting in postings {
        if scored_enough(posting) && posting.stage == Stage::Review && !decisions.contains_key(&posting.link) {
            let name = posting.company.trim();
            counts
                .entry(name.to_lowercase())
                .or_insert_with(|| (name.to_owned(), 0))
                .1 += 1;
        }
    }
    for out in &scope.left_out {
        counts.entry(out.to_lowercase()).or_insert_with(|| (out.clone(), 0));
    }
    let mut listed: Vec<(String, usize)> = counts.into_values().collect();
    listed.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase())));
    listed
}

fn scored_enough(posting: &Posting) -> bool {
    posting.score.is_some_and(|score| score >= APPLY_SCORE)
}

/// Best score first, then the newest.
fn best_first(postings: &mut [&Posting]) {
    postings.sort_by(|a, b| b.score.cmp(&a.score).then(b.found_at.cmp(&a.found_at)));
}

/// One person to send to now.
#[derive(Clone, Debug, PartialEq)]
pub struct Pick {
    pub about: About,
    pub profile_url: String,
    /// The waiting draft to them, or `None` when one is still to be written about a posting.
    pub draft: Option<i64>,
}

/// The people to send to now. A posting counts once it is scored 75 or more and approved or
/// applied to; its check's best two people with a profile get a draft, while its company has had
/// fewer than two sends today. Every waiting hiring-post
/// draft is sent too, newest first, taking turns with the postings so neither waits on the other.
/// Nobody gets a second draft once one to them was typed, sent or skipped, about anything.
pub fn to_reach(
    postings: &[Posting],
    decisions: &HashMap<String, Decision>,
    checks: &[Check],
    drafts: &[Draft],
    scope: &Scope,
    now: Timestamp,
) -> Vec<Pick> {
    let sent_today = drafts
        .iter()
        .filter(|draft| {
            draft.status == Outreach::Sent && outreach::pacific_day(draft.updated_at) == outreach::pacific_day(now)
        })
        .count();
    let room = SENDS_PER_DAY.saturating_sub(sent_today).min(SENDS_PER_RUN);
    let company_of = |link: &str| {
        postings
            .iter()
            .find(|posting| posting.link == link)
            .map(|posting| posting.company.trim().to_lowercase())
    };
    let mut company_sends: HashMap<String, usize> = HashMap::new();
    for draft in drafts {
        if let About::Posting(link) = &draft.about
            && draft.status == Outreach::Sent
            && outreach::pacific_day(draft.updated_at) == outreach::pacific_day(now)
            && let Some(company) = company_of(link)
        {
            *company_sends.entry(company).or_default() += 1;
        }
    }
    let mut chosen: Vec<&Posting> = postings
        .iter()
        .filter(|posting| {
            let decision = decisions.get(&posting.link);
            scored_enough(posting)
                && scope.covers(posting)
                && (decision == Some(&Decision::Approved) || posting.stage == Stage::Applied && decision.is_none())
        })
        .collect();
    best_first(&mut chosen);
    let reached = |profile: &str| {
        drafts.iter().any(|draft| {
            draft.profile_url == profile && !matches!(draft.status, Outreach::Drafted | Outreach::NotStarted)
        })
    };
    let mut picks: Vec<Pick> = Vec::new();
    for posting in chosen {
        let Some(check) = checks.iter().find(|check| check.posting_link == posting.link) else {
            continue;
        };
        if !check.verdict.found_people() {
            continue;
        }
        let people = check
            .people
            .iter()
            .filter_map(|person| person.profile_url.as_deref())
            .take(PEOPLE_PER_POSTING);
        let company = posting.company.trim().to_lowercase();
        for profile in people {
            if reached(profile) || picks.iter().any(|pick: &Pick| pick.profile_url == profile) {
                continue;
            }
            let sends = company_sends.entry(company.clone()).or_default();
            if *sends >= SENDS_PER_COMPANY_PER_DAY {
                break;
            }
            *sends += 1;
            let waiting = drafts.iter().find(|draft| {
                draft.profile_url == profile
                    && draft.status == Outreach::Drafted
                    && draft.about == About::Posting(posting.link.clone())
            });
            picks.push(Pick {
                about: About::Posting(posting.link.clone()),
                profile_url: profile.to_owned(),
                draft: waiting.map(|draft| draft.id),
            });
        }
    }
    let mut posts: Vec<&Draft> = drafts
        .iter()
        .filter(|draft| matches!(draft.about, About::Post(_)) && draft.status == Outreach::Drafted)
        .collect();
    posts.sort_by_key(|draft| std::cmp::Reverse(draft.created_at));
    let mut post_picks: Vec<Pick> = Vec::new();
    for draft in posts {
        let taken = |pick: &Pick| pick.profile_url == draft.profile_url;
        if reached(&draft.profile_url) || picks.iter().any(taken) || post_picks.iter().any(taken) {
            continue;
        }
        post_picks.push(Pick {
            about: draft.about.clone(),
            profile_url: draft.profile_url.clone(),
            draft: Some(draft.id),
        });
    }
    let (mut postings, mut posts) = (picks.into_iter(), post_picks.into_iter());
    let mut turns = Vec::new();
    while turns.len() < room {
        match (postings.next(), posts.next()) {
            (None, None) => break,
            (one, other) => turns.extend(one.into_iter().chain(other)),
        }
    }
    turns.truncate(room);
    turns
}

/// What one auto-apply outreach run did.
#[derive(Debug, Default)]
pub struct ReachRun {
    /// The people a draft went to.
    pub sent: Vec<String>,
    /// The people not sent to because their profile is outside the US, with where it says they are.
    /// Their draft is skipped.
    pub abroad: Vec<String>,
    /// Each person whose draft could not be sent, and why. Their draft is skipped; Reset brings it back.
    pub failed: Vec<(String, String)>,
    /// Why the run stopped early, when it did.
    pub stopped: Option<String>,
}

impl ReachRun {
    pub fn summary(&self) -> String {
        let mut line = format!("auto-apply sent {}", self.sent.len());
        if !self.sent.is_empty() {
            line.push_str(&format!(" ({})", self.sent.join(", ")));
        }
        if !self.abroad.is_empty() {
            line.push_str(&format!("; not in the US: {}", self.abroad.join(", ")));
        }
        for (person, why) in &self.failed {
            line.push_str(&format!("; {person} not sent: {why}"));
        }
        if let Some(stopped) = &self.stopped {
            line.push_str(&format!("; stopped: {stopped}"));
        }
        line
    }
}

/// Writes and sends a draft to each person `to_reach` picks, one at a time with a random wait between.
/// A draft that fails before Send is skipped. Send pressed without the page showing the draft sent
/// leaves it typed, with its tab open, and stops the run, so nothing is sent twice.
pub async fn reach(
    database: &Path,
    profile: &Profile,
    bridge: Arc<Bridge>,
    resume: Option<&Path>,
) -> Result<ReachRun, String> {
    let said = |error: StoreError| format!("the database failed: {error}");
    let picks = {
        let store = Store::open(database).map_err(said)?;
        to_reach(
            &store.postings().map_err(said)?,
            &store.decisions().map_err(said)?,
            &store.checks().map_err(said)?,
            &store.drafts().map_err(said)?,
            &Scope::load(database),
            Timestamp::now(),
        )
    };
    let mut run = ReachRun::default();
    let mut failures = 0;
    for (index, pick) in picks.iter().enumerate() {
        if index > 0 {
            tokio::time::sleep(BETWEEN_SENDS.wait(fastrand::f64())).await;
        }
        let draft = match (pick.draft, &pick.about) {
            (Some(id), _) => Store::open(database)
                .and_then(|store| store.draft(id))
                .map_err(said)?
                .ok_or("a draft left the database during the run")?,
            (None, About::Post(_)) => return Err("a hiring post was picked without its draft".into()),
            (None, About::Posting(link)) => {
                match outreach::draft_for_check(database, profile, link, &pick.profile_url).await {
                    Ok(draft) => draft,
                    Err(why) => {
                        run.failed.push((pick.profile_url.clone(), format!("no draft: {why}")));
                        failures += 1;
                        if failures >= FAILURES_IN_A_ROW {
                            run.stopped = Some("drafts failed twice in a row".into());
                            break;
                        }
                        continue;
                    }
                }
            }
        };
        let sent = match outreach::check_written(draft.subject.as_deref(), &draft.text, draft.slot) {
            Ok(written) => {
                linkedin::send_draft(
                    bridge.clone(),
                    &draft.profile_url,
                    draft.slot,
                    written.subject.as_deref(),
                    &written.text,
                    resume,
                )
                .await
            }
            Err(why) => Err(why),
        };
        let now = Timestamp::now();
        let mut store = Store::open(database).map_err(said)?;
        match sent {
            Ok((typed, delivery)) => {
                if let Some(credits) = typed.credits {
                    store.save_inmail_credits(credits, now).map_err(said)?;
                }
                failures = 0;
                match delivery {
                    Delivery::Sent => {
                        store.set_draft_status(draft.id, Outreach::Sent, now).map_err(said)?;
                        run.sent.push(draft.person.clone());
                    }
                    Delivery::OutsideUs(location) => {
                        store.set_draft_status(draft.id, Outreach::Skipped, now).map_err(said)?;
                        run.abroad.push(format!("{} ({location})", draft.person));
                    }
                    Delivery::Unconfirmed(why) => {
                        store.set_draft_status(draft.id, Outreach::Typed, now).map_err(said)?;
                        run.stopped = Some(format!("{}: {why}", draft.person));
                        break;
                    }
                }
            }
            Err(why) => {
                store.set_draft_status(draft.id, Outreach::Skipped, now).map_err(said)?;
                run.failed.push((draft.person.clone(), why));
                failures += 1;
                if failures >= FAILURES_IN_A_ROW {
                    run.stopped = Some("sends failed twice in a row".into());
                    break;
                }
            }
        }
    }
    Ok(run)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linkedin::{Person, Slot};
    use crate::outreach::Verdict;

    fn posting(link: &str, score: u8, stage: Stage) -> Posting {
        Posting {
            link: link.into(),
            found_at: "2026-11-22T17:00:00Z".parse().unwrap(),
            source: "Simplify".into(),
            company: "Example Co".into(),
            title: "Backend Engineer".into(),
            location: None,
            email_subject: None,
            stage,
            score: Some(score),
            variant: None,
            flags: None,
            notes: None,
            address: None,
            sponsorship: None,
        }
    }

    fn person(handle: &str) -> Person {
        Person {
            name: handle.to_uppercase(),
            title: "Engineer".into(),
            profile_url: Some(profile(handle)),
            degree: Some("2nd".into()),
        }
    }

    fn profile(handle: &str) -> String {
        format!("https://www.linkedin.com/in/{handle}/")
    }

    fn check(link: &str, people: &[&str]) -> Check {
        Check {
            posting_link: link.into(),
            verdict: Verdict::TeamMember,
            team: None,
            people: people.iter().map(|handle| person(handle)).collect(),
            note: None,
            checked_at: "2026-11-22T18:00:00Z".parse().unwrap(),
        }
    }

    fn draft(id: i64, link: &str, handle: &str, status: Outreach, at: &str) -> Draft {
        Draft {
            id,
            about: About::Posting(link.into()),
            person: handle.to_uppercase(),
            headline: "Engineer".into(),
            profile_url: profile(handle),
            slot: Slot::Note,
            subject: None,
            text: "Hi".into(),
            status,
            created_at: at.parse().unwrap(),
            updated_at: at.parse().unwrap(),
        }
    }

    const NOW: &str = "2026-11-24T20:00:00Z";

    fn everywhere() -> Scope {
        Scope {
            platforms: Platform::ALL.into(),
            left_out: BTreeSet::new(),
        }
    }

    #[test]
    fn auto_apply_keeps_to_the_chosen_platforms_and_companies() {
        let at = |link: &str, company: &str, address: &str| Posting {
            company: company.into(),
            address: Some(address.into()),
            ..posting(link, 90, Stage::Review)
        };
        let postings = [
            at(
                "ashby",
                "Example Co",
                "https://jobs.ashbyhq.com/example/0f3b8231-3a32-478a-b994-3441f4bb9401",
            ),
            at("amazon", "Amazon", "https://www.amazon.jobs/en/jobs/123/engineer"),
            at("lever", "Lever Co", "https://jobs.lever.co/example/1"),
            at(
                "greenhouse",
                "Big Corp",
                "https://job-boards.greenhouse.io/example/jobs/123",
            ),
        ];
        let none = HashMap::new();
        let covered = |scope: &Scope| -> Vec<&str> {
            postings
                .iter()
                .filter(|posting| scope.covers(posting))
                .map(|posting| posting.link.as_str())
                .collect()
        };
        let mut scope = Scope::default();
        assert_eq!(covered(&scope), ["ashby", "greenhouse"]);

        scope.toggle_platform(Platform::Lever);
        scope.toggle_company("big corp ");
        assert_eq!(covered(&scope), ["ashby", "lever"]);
        // Big Corp is left out but still listed once, under the posting's own name; a company
        // left out with nothing waiting is listed with none.
        scope.toggle_company("Gone Inc");
        let listed: Vec<(String, usize)> = companies(&postings, &none, &scope);
        let names: Vec<(&str, usize)> = listed.iter().map(|(name, count)| (name.as_str(), *count)).collect();
        assert_eq!(
            names,
            [
                ("Amazon", 1),
                ("Big Corp", 1),
                ("Example Co", 1),
                ("Lever Co", 1),
                ("Gone Inc", 0)
            ]
        );
        scope.toggle_company("gone inc");

        scope.toggle_company("Big Corp");
        assert!(scope.left_out.is_empty());

        let scratch = tempfile::tempdir().unwrap();
        let database = scratch.path().join("maverick.db");
        assert_eq!(Scope::load(&database), Scope::default());
        scope.save(&database).unwrap();
        assert_eq!(Scope::load(&database), scope);
    }

    #[test]
    fn one_company_hears_from_maverick_at_most_twice_a_day() {
        let at = |link: &str, company: &str| Posting {
            company: company.into(),
            ..posting(link, 90, Stage::Applied)
        };
        let postings = [at("a1", "Big Corp"), at("a2", "big corp "), at("o", "Other Co")];
        let checks = [
            check("a1", &["ada", "bo"]),
            check("a2", &["cy", "di"]),
            check("o", &["eve"]),
        ];
        let handles = |picks: Vec<Pick>| -> Vec<String> {
            picks
                .iter()
                .map(|pick| {
                    pick.profile_url
                        .trim_end_matches('/')
                        .rsplit('/')
                        .next()
                        .unwrap()
                        .to_owned()
                })
                .collect()
        };
        let none = HashMap::new();
        let now: Timestamp = NOW.parse().unwrap();
        let picks = to_reach(&postings, &none, &checks, &[], &everywhere(), now);
        assert_eq!(handles(picks), ["ada", "bo", "eve"]);

        // A send to Big Corp earlier today leaves room for one more there.
        let earlier = [draft(1, "a2", "zed", Outreach::Sent, NOW)];
        let picks = to_reach(&postings, &none, &checks, &earlier, &everywhere(), now);
        assert_eq!(handles(picks), ["ada", "eve"]);
    }

    #[test]
    fn the_best_two_people_at_each_approved_posting_are_reached_once() {
        let postings = [
            posting("a", 80, Stage::Review),
            posting("b", 90, Stage::Review),
            posting("skipped", 99, Stage::Review),
            posting("low", 60, Stage::Review),
        ];
        let decisions = HashMap::from([
            ("a".to_owned(), Decision::Approved),
            ("b".to_owned(), Decision::Approved),
            ("skipped".to_owned(), Decision::Skipped),
            ("low".to_owned(), Decision::Approved),
        ]);
        let checks = [
            check("a", &["ada", "bo", "cy"]),
            check("b", &["di", "ada"]),
            check("skipped", &["eve"]),
            check("low", &["fay"]),
        ];
        // Di's draft waits; Ada already got one about another posting, so she is not reached again.
        let drafts = [
            draft(7, "b", "di", Outreach::Drafted, NOW),
            draft(8, "x", "ada", Outreach::Sent, "2026-11-20T18:00:00Z"),
        ];
        let picks = to_reach(
            &postings,
            &decisions,
            &checks,
            &drafts,
            &everywhere(),
            NOW.parse().unwrap(),
        );
        let said: Vec<(&str, &str, Option<i64>)> = picks
            .iter()
            .map(|pick| {
                let handle = pick.profile_url.trim_end_matches('/').rsplit('/').next().unwrap();
                let About::Posting(link) = &pick.about else {
                    panic!("only postings here")
                };
                (link.as_str(), handle, pick.draft)
            })
            .collect();
        assert_eq!(said, [("b", "di", Some(7)), ("a", "bo", None)]);
    }

    fn post_draft(id: i64, handle: &str, status: Outreach, at: &str) -> Draft {
        Draft {
            about: About::Post(format!("https://www.linkedin.com/feed/update/urn:li:activity:{id}/")),
            ..draft(id, "", handle, status, at)
        }
    }

    #[test]
    fn waiting_hiring_post_drafts_are_sent_newest_first_taking_turns_with_postings() {
        let postings: Vec<Posting> = (0..3)
            .map(|n| posting(&format!("p{n}"), 90 - n, Stage::Applied))
            .collect();
        let checks: Vec<Check> = (0..3).map(|n| check(&format!("p{n}"), &[&format!("a{n}")])).collect();
        let drafts = [
            post_draft(1, "old", Outreach::Drafted, "2026-11-23T18:00:00Z"),
            post_draft(2, "new", Outreach::Drafted, "2026-11-24T18:00:00Z"),
            post_draft(3, "gone", Outreach::Skipped, "2026-11-24T19:00:00Z"),
            post_draft(4, "a1", Outreach::Drafted, "2026-11-24T19:00:00Z"),
            post_draft(5, "twice", Outreach::Drafted, "2026-11-24T17:00:00Z"),
            draft(6, "x", "twice", Outreach::Sent, "2026-11-20T18:00:00Z"),
        ];
        let picks = to_reach(
            &postings,
            &HashMap::new(),
            &checks,
            &drafts,
            &everywhere(),
            NOW.parse().unwrap(),
        );
        let said: Vec<(&str, Option<i64>)> = picks
            .iter()
            .map(|pick| {
                let kind = match pick.about {
                    About::Posting(_) => "posting",
                    About::Post(_) => "post",
                };
                (kind, pick.draft)
            })
            .collect();
        // A1 is reached about the posting, so the post draft to them waits; Twice was already sent to.
        assert_eq!(
            said,
            [
                ("posting", None),
                ("post", Some(2)),
                ("posting", None),
                ("post", Some(1))
            ]
        );
    }

    #[test]
    fn sends_keep_to_the_daily_and_the_run_caps() {
        let postings: Vec<Posting> = (0..20)
            .map(|n| Posting {
                company: format!("Company {n}"),
                ..posting(&format!("p{n}"), 80, Stage::Applied)
            })
            .collect();
        let checks: Vec<Check> = (0..20)
            .map(|n| check(&format!("p{n}"), &[&format!("a{n}"), &format!("b{n}")]))
            .collect();
        let none = HashMap::new();
        let now: Timestamp = NOW.parse().unwrap();
        assert_eq!(
            to_reach(&postings, &none, &checks, &[], &everywhere(), now).len(),
            SENDS_PER_RUN
        );

        let sent_today: Vec<Draft> = (0..SENDS_PER_DAY as i64 - 1)
            .map(|n| draft(n, "old", &format!("s{n}"), Outreach::Sent, NOW))
            .collect();
        assert_eq!(
            to_reach(&postings, &none, &checks, &sent_today, &everywhere(), now).len(),
            1
        );
    }

    #[test]
    fn the_switch_is_off_until_turned_on() {
        let scratch = tempfile::tempdir().unwrap();
        let database = scratch.path().join("maverick.db");
        assert!(!is_on(&database));
        set_on(&database, true).unwrap();
        assert!(is_on(&database));
        set_on(&database, false).unwrap();
        assert!(!is_on(&database));
    }
}
