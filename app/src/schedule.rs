//! Runs the funnel every hour while the app is open: intake, then scoring. It never submits.

use std::path::{Path, PathBuf};
use std::time::Duration;

use jiff::Timestamp;
use maverick_core::funnel;
use maverick_core::jobs::Decision;
use maverick_core::score::Profile;
use maverick_core::store::Store;
use tokio::runtime::Handle;

use crate::band::{self, Cell};
use crate::board::BoardData;

/// The wait after launch before the first pass, so starting the app stays quick.
pub const FIRST_PASS: Duration = Duration::from_secs(60);
pub const BETWEEN_PASSES: Duration = Duration::from_secs(60 * 60);

/// Where the funnel reads and writes.
#[derive(Clone)]
pub struct Funnel {
    pub database: PathBuf,
    pub answers: PathBuf,
    pub background: Handle,
}

/// What the window shows after a pass.
pub struct Refreshed {
    /// One line on what the pass did, or why it did not run.
    pub status: String,
    /// The lists read back after the pass. `None` when the pass failed, so the window keeps what it shows.
    pub lists: Option<Lists>,
}

/// What the window lists, read from the local database.
#[derive(Default)]
pub struct Lists {
    /// One band cell for every posting.
    pub cells: Vec<Cell>,
    pub board: BoardData,
}

/// The lists from the local database. A missing or unreadable database gives empty lists.
pub fn stored_lists(database: Option<&Path>) -> Lists {
    let store = database
        .filter(|path| path.is_file())
        .and_then(|path| Store::open(path).ok());
    let Some(store) = store else {
        return Lists::default();
    };
    let postings = store.postings().unwrap_or_default();
    let applications = store.applications().unwrap_or_default();
    let decisions = store.decisions().unwrap_or_default();
    let board = BoardData::new(&postings, applications, &decisions);
    let open: Vec<&str> = board
        .entries
        .iter()
        .filter(|entry| entry.decision != Some(Decision::Skipped))
        .map(|entry| entry.address.as_str())
        .collect();
    let cells = band::cells(&postings, &open);
    Lists { cells, board }
}

impl Funnel {
    /// One pass, on the tokio runtime, if it is daytime in Pacific time. `None` outside those hours.
    pub async fn pass(&self) -> Option<Refreshed> {
        let now = Timestamp::now();
        if !funnel::in_hours(now) {
            return None;
        }
        let (database, answers) = (self.database.clone(), self.answers.clone());
        let ran = self
            .background
            .spawn(async move {
                let profile = Profile::load(&answers)?;
                let sheet = maverick_core::sheet::linked_session().await?;
                let pass = funnel::run_once(&database, profile, sheet).await?;
                let lists = tokio::task::spawn_blocking(move || stored_lists(Some(&database)))
                    .await
                    .map_err(|stopped| stopped.to_string())?;
                Ok::<_, String>((pass.summary(), lists))
            })
            .await;
        let at = now
            .in_tz("America/Los_Angeles")
            .expect("the time zone database has Pacific time")
            .strftime("%-I:%M %p");
        Some(match ran {
            Ok(Ok((summary, lists))) => Refreshed {
                status: format!("Checked at {at}: {summary}"),
                lists: Some(lists),
            },
            Ok(Err(error)) => failed(format!("The {at} check failed: {error}")),
            Err(stopped) => failed(format!("The {at} check stopped: {stopped}")),
        })
    }
}

fn failed(status: String) -> Refreshed {
    eprintln!("maverick: {status}");
    Refreshed { status, lists: None }
}

#[cfg(test)]
mod tests {
    use maverick_core::jobs::{Posting, Stage};

    use super::*;

    #[test]
    fn a_posting_skipped_on_the_board_stays_on_the_board_and_its_band_cell_is_not_open() {
        let scratch = tempfile::tempdir().unwrap();
        let database = scratch.path().join("maverick.db");
        let posting = |id: u8| Posting {
            link: format!("https://jobs.ashbyhq.com/example-co/0f3b8231-3a32-478a-b994-3441f4bb94{id:02}"),
            found_at: "2026-11-20T12:00:00Z".parse().unwrap(),
            source: "Simplify".into(),
            company: "Example Co".into(),
            title: format!("Engineer {id}"),
            location: None,
            email_subject: None,
            stage: Stage::Review,
            score: Some(70),
            variant: None,
            flags: None,
            notes: None,
            address: None,
            sponsorship: None,
        };
        let mut store = Store::open(&database).unwrap();
        store.add_found(&[posting(1), posting(2)]).unwrap();
        store.decide(&posting(2).link, Some(Decision::Skipped)).unwrap();
        drop(store);

        let lists = stored_lists(Some(&database));
        let kinds: Vec<_> = lists.cells.iter().map(|cell| cell.kind).collect();
        assert_eq!(kinds, [band::Kind::Open, band::Kind::Scored(70)]);
        let board: Vec<_> = lists
            .board
            .entries
            .iter()
            .map(|entry| (entry.title.as_str(), entry.decision))
            .collect();
        assert_eq!(board, [("Engineer 2", Some(Decision::Skipped)), ("Engineer 1", None)]);
    }
}
