//! One pass of the job funnel: find new postings, then score what is found. It never submits.

use std::path::Path;

use jiff::Timestamp;

use crate::intake;
use crate::jobs::{Posting, Stage};
use crate::score::{self, APPLY_READY, Profile, RUN_CAP};
use crate::sheet::Session;
use crate::store::Store;

/// The run happens from 8 AM until 11 PM Pacific time, when the user can act on what it finds.
pub fn in_hours(now: Timestamp) -> bool {
    let hour = now
        .in_tz("America/Los_Angeles")
        .expect("the time zone database has Pacific time")
        .hour();
    (8..23).contains(&hour)
}

/// What one pass did.
#[derive(Debug)]
pub struct Pass {
    /// New postings intake added. `Err` when the pull failed; scoring still ran.
    pub added: Result<usize, String>,
    pub duplicates: usize,
    pub skipped_by_rules: usize,
    pub scored: Vec<Posting>,
    pub waiting: usize,
    pub unreadable: usize,
    /// Decisions written to the live tracker. `None` when no tracker is linked.
    pub written: Option<Result<usize, String>>,
}

impl Pass {
    pub fn apply_ready(&self) -> usize {
        self.scored
            .iter()
            .filter(|posting| {
                posting
                    .notes
                    .as_deref()
                    .is_some_and(|notes| notes.starts_with(APPLY_READY))
            })
            .count()
    }

    pub fn to_review(&self) -> usize {
        self.scored
            .iter()
            .filter(|posting| posting.stage == Stage::Review)
            .count()
            - self.apply_ready()
    }

    /// One line for a log or a status bar.
    pub fn summary(&self) -> String {
        let added = match &self.added {
            Ok(count) => format!("{count} new postings"),
            Err(error) => format!("no new postings ({error})"),
        };
        let failed = self
            .scored
            .iter()
            .filter(|posting| posting.stage == Stage::Found)
            .count();
        let mut line = format!(
            "{added}; {} scored ({} apply-ready, {} to review)",
            self.scored.len() - failed,
            self.apply_ready(),
            self.to_review()
        );
        if failed > 0 {
            line.push_str(&format!("; {failed} could not be scored"));
        }
        if self.waiting > 0 {
            line.push_str(&format!("; {} wait for the next run", self.waiting));
        }
        match &self.written {
            Some(Ok(count)) => line.push_str(&format!("; {count} written to the sheet")),
            Some(Err(error)) => line.push_str(&format!("; the sheet was not updated ({error})")),
            None => {}
        }
        line
    }
}

/// Reads the live tracker into the database when one is linked, pulls new postings, scores up
/// to the cap, and writes every decision back to the sheet.
/// The store is opened only between network calls, so the app's own connection is never held up.
pub async fn run_once(database: &Path, profile: Profile, sheet: Option<Session>) -> Result<Pass, String> {
    let said = |error: rusqlite::Error| format!("the database failed: {error}");
    let mut snapshot = match &sheet {
        Some(session) => {
            let snapshot = session.read().await?;
            Store::open(database)
                .and_then(|mut store| store.save_tracker(&snapshot.tracker))
                .map_err(said)?;
            Some(snapshot)
        }
        None => None,
    };

    let known = Store::open(database).and_then(|store| store.postings()).map_err(said)?;
    let pulled = intake::pull(&known, Timestamp::now()).await;
    let added = match pulled {
        Ok(found) => Ok(Store::open(database)
            .and_then(|mut store| store.add_found(&found))
            .map_err(said)?),
        Err(error) => Err(error),
    };
    if let (Some(session), Some(read)) = (&sheet, &snapshot) {
        // Every posting intake found goes into the sheet, including any an earlier pass could not add.
        let missing: Vec<Posting> = Store::open(database)
            .and_then(|store| store.postings())
            .map_err(said)?
            .into_iter()
            .filter(|posting| posting.source == intake::SOURCE && !read.rows.contains_key(&posting.link))
            .collect();
        if !missing.is_empty() {
            session.append_found(read, &missing).await?;
            // Read again, so the new rows have row numbers for their decisions.
            snapshot = Some(session.read().await?);
        }
    }

    let mut store = Store::open(database).map_err(said)?;
    let plan = score::plan(
        &store.postings().map_err(said)?,
        &store.applications().map_err(said)?,
        RUN_CAP,
    );
    store.save_decisions(&plan.duplicates).map_err(said)?;
    store.save_decisions(&plan.skipped).map_err(said)?;
    drop(store);

    let scored = score::score_all(plan.to_score, profile, |_| {}).await;
    Store::open(database)
        .and_then(|mut store| store.save_decisions(&scored))
        .map_err(said)?;

    let written = match (&sheet, &snapshot) {
        (Some(session), Some(snapshot)) => {
            let decided: Vec<Posting> = plan
                .duplicates
                .iter()
                .chain(&plan.skipped)
                .chain(&scored)
                .cloned()
                .collect();
            Some(session.write_decisions(snapshot, &decided).await)
        }
        _ => None,
    };
    Ok(Pass {
        added,
        duplicates: plan.duplicates.len(),
        skipped_by_rules: plan.skipped.len(),
        scored,
        waiting: plan.waiting,
        unreadable: plan.unreadable,
        written,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_summary_counts_only_postings_that_were_scored() {
        let posting = |stage: Stage, notes: &str| Posting {
            link: "https://simplify.jobs/p/1".into(),
            found_at: "2026-11-22T17:00:00Z".parse().unwrap(),
            source: "Simplify".into(),
            company: "Example Co".into(),
            title: "Engineer".into(),
            location: None,
            email_subject: None,
            stage,
            score: None,
            variant: None,
            flags: None,
            notes: Some(notes.into()),
            address: None,
            sponsorship: None,
        };
        let pass = Pass {
            added: Ok(3),
            duplicates: 0,
            skipped_by_rules: 0,
            scored: vec![
                posting(Stage::Review, &format!("{APPLY_READY} 80, Backend/Infra: fits")),
                posting(Stage::Review, "Review - 65, Backend/Infra: partial"),
                posting(Stage::Skipped, "Skipped - 40"),
                posting(Stage::Found, "Not scored yet: offline"),
            ],
            waiting: 2,
            unreadable: 0,
            written: None,
        };
        assert_eq!(
            pass.summary(),
            "3 new postings; 3 scored (1 apply-ready, 1 to review); 1 could not be scored; 2 wait for the next run"
        );
        let failed_pull = Pass {
            added: Err("GitHub answered the Simplify list with status 503".into()),
            scored: Vec::new(),
            waiting: 0,
            written: Some(Ok(4)),
            ..pass
        };
        assert_eq!(
            failed_pull.summary(),
            "no new postings (GitHub answered the Simplify list with status 503); 0 scored (0 apply-ready, 0 to review); \
             4 written to the sheet"
        );
    }

    #[test]
    fn the_run_keeps_to_daytime_pacific_hours() {
        // 2026-11-22 is in standard time, eight hours behind UTC.
        let at = |utc: &str| in_hours(utc.parse().unwrap());
        assert!(!at("2026-11-22T15:59:00Z"));
        assert!(at("2026-11-22T16:00:00Z"));
        assert!(at("2026-11-23T06:59:00Z"));
        assert!(!at("2026-11-23T07:00:00Z"));
    }
}
