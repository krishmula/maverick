//! Auto-apply: Maverick approves each posting scored 75 or more, and sends a draft to the best two
//! people its outreach check found there, without the user's review. The user turns it on in
//! Connections; while it is off, the user approves each posting. Whether an approved form is also
//! submitted is the auto-submit switch's call.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use jiff::Timestamp;

use crate::bridge::Bridge;
use crate::jobs::{Decision, Outreach, Posting, Stage};
use crate::linkedin::{self, Delivery, Pacing};
use crate::outreach::{self, About, Check, Draft};
use crate::score::{APPLY_SCORE, Profile};
use crate::store::{Store, StoreError};

/// How many people at one posting Maverick sends to, best first.
pub const PEOPLE_PER_POSTING: usize = 2;
/// The most postings one funnel pass approves, so the apply queue fills a few at a time.
pub const APPROVALS_PER_PASS: usize = 10;
/// The most drafts sent in one Pacific day, the user's own sends included, to keep LinkedIn calm.
pub const SENDS_PER_DAY: usize = 20;
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

fn scored_enough(posting: &Posting) -> bool {
    posting.score.is_some_and(|score| score >= APPLY_SCORE)
}

/// Best score first, then the newest.
fn best_first(postings: &mut [&Posting]) {
    postings.sort_by(|a, b| b.score.cmp(&a.score).then(b.found_at.cmp(&a.found_at)));
}

/// The postings to approve now: scored 75 or more, waiting for review, and not decided by anyone.
pub fn to_approve(postings: &[Posting], decisions: &HashMap<String, Decision>) -> Vec<String> {
    let mut waiting: Vec<&Posting> = postings
        .iter()
        .filter(|posting| {
            scored_enough(posting) && posting.stage == Stage::Review && !decisions.contains_key(&posting.link)
        })
        .collect();
    best_first(&mut waiting);
    waiting
        .into_iter()
        .take(APPROVALS_PER_PASS)
        .map(|posting| posting.link.clone())
        .collect()
}

/// Approves the postings `to_approve` gives, and says how many.
pub fn approve(database: &Path) -> Result<usize, StoreError> {
    let mut store = Store::open(database)?;
    let links = to_approve(&store.postings()?, &store.decisions()?);
    for link in &links {
        store.decide(link, Some(Decision::Approved))?;
    }
    Ok(links.len())
}

/// One person to send to now.
#[derive(Clone, Debug, PartialEq)]
pub struct Pick {
    pub posting_link: String,
    pub profile_url: String,
    /// The waiting draft to them, or `None` when one is still to be written.
    pub draft: Option<i64>,
}

/// The people to send to now. A posting counts once it is scored 75 or more and approved or
/// applied to; its check's best two people with a profile get a draft. Nobody gets a second
/// draft once one to them was typed, sent or skipped, about this posting or any other.
pub fn to_reach(
    postings: &[Posting],
    decisions: &HashMap<String, Decision>,
    checks: &[Check],
    drafts: &[Draft],
    now: Timestamp,
) -> Vec<Pick> {
    let sent_today = drafts
        .iter()
        .filter(|draft| {
            draft.status == Outreach::Sent && outreach::pacific_day(draft.updated_at) == outreach::pacific_day(now)
        })
        .count();
    let room = SENDS_PER_DAY.saturating_sub(sent_today).min(SENDS_PER_RUN);
    let mut chosen: Vec<&Posting> = postings
        .iter()
        .filter(|posting| {
            let decision = decisions.get(&posting.link);
            scored_enough(posting)
                && (decision == Some(&Decision::Approved) || posting.stage == Stage::Applied && decision.is_none())
        })
        .collect();
    best_first(&mut chosen);
    let reached = |profile: &str| {
        drafts.iter().any(|draft| {
            draft.profile_url == profile && !matches!(draft.status, Outreach::Drafted | Outreach::NotStarted)
        })
    };
    let mut picks = Vec::new();
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
        for profile in people {
            if reached(profile) || picks.iter().any(|pick: &Pick| pick.profile_url == profile) {
                continue;
            }
            let waiting = drafts.iter().find(|draft| {
                draft.profile_url == profile
                    && draft.status == Outreach::Drafted
                    && draft.about == About::Posting(posting.link.clone())
            });
            picks.push(Pick {
                posting_link: posting.link.clone(),
                profile_url: profile.to_owned(),
                draft: waiting.map(|draft| draft.id),
            });
        }
    }
    picks.truncate(room);
    picks
}

/// What one auto-apply outreach run did.
#[derive(Debug, Default)]
pub struct ReachRun {
    /// The people a draft went to.
    pub sent: Vec<String>,
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
            Timestamp::now(),
        )
    };
    let mut run = ReachRun::default();
    let mut failures = 0;
    for (index, pick) in picks.iter().enumerate() {
        if index > 0 {
            tokio::time::sleep(BETWEEN_SENDS.wait(fastrand::f64())).await;
        }
        let draft = match pick.draft {
            Some(id) => Store::open(database)
                .and_then(|store| store.draft(id))
                .map_err(said)?
                .ok_or("a draft left the database during the run")?,
            None => match outreach::draft_for_check(database, profile, &pick.posting_link, &pick.profile_url).await {
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
            },
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

    #[test]
    fn postings_scored_75_or_more_and_undecided_are_approved_best_first() {
        let postings = [
            posting("a", 74, Stage::Review),
            posting("b", 75, Stage::Review),
            posting("c", 90, Stage::Review),
            posting("d", 95, Stage::Skipped),
            posting("e", 88, Stage::Review),
            posting("f", 99, Stage::Applied),
        ];
        let decisions = HashMap::from([("e".to_owned(), Decision::Skipped)]);
        assert_eq!(to_approve(&postings, &decisions), ["c", "b"]);

        let many: Vec<Posting> = (0..15).map(|n| posting(&n.to_string(), 80, Stage::Review)).collect();
        assert_eq!(to_approve(&many, &HashMap::new()).len(), APPROVALS_PER_PASS);
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
        let picks = to_reach(&postings, &decisions, &checks, &drafts, NOW.parse().unwrap());
        let said: Vec<(&str, &str, Option<i64>)> = picks
            .iter()
            .map(|pick| {
                let handle = pick.profile_url.trim_end_matches('/').rsplit('/').next().unwrap();
                (pick.posting_link.as_str(), handle, pick.draft)
            })
            .collect();
        assert_eq!(said, [("b", "di", Some(7)), ("a", "bo", None)]);
    }

    #[test]
    fn sends_keep_to_the_daily_and_the_run_caps() {
        let postings: Vec<Posting> = (0..20).map(|n| posting(&format!("p{n}"), 80, Stage::Applied)).collect();
        let checks: Vec<Check> = (0..20)
            .map(|n| check(&format!("p{n}"), &[&format!("a{n}"), &format!("b{n}")]))
            .collect();
        let none = HashMap::new();
        let now: Timestamp = NOW.parse().unwrap();
        assert_eq!(to_reach(&postings, &none, &checks, &[], now).len(), SENDS_PER_RUN);

        let sent_today: Vec<Draft> = (0..SENDS_PER_DAY as i64 - 1)
            .map(|n| draft(n, "old", &format!("s{n}"), Outreach::Sent, NOW))
            .collect();
        assert_eq!(to_reach(&postings, &none, &checks, &sent_today, now).len(), 1);
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
