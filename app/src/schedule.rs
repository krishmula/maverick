//! Runs the funnel every hour while the app is open: intake, then scoring. It never submits.

use std::path::{Path, PathBuf};
use std::time::Duration;

use jiff::Timestamp;
use maverick_core::answers::Answers;
use maverick_core::funnel;
use maverick_core::score::Profile;
use maverick_core::store::Store;
use tokio::runtime::Handle;

use crate::band::{self, Cell};
use crate::jobs::{self, Job};

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
    pub lists: Option<(Vec<Job>, Vec<Cell>)>,
}

/// The open postings from the local database, and one band cell for every posting in it.
/// A missing or unreadable database gives none of either.
pub fn stored_jobs(database: Option<&Path>) -> (Vec<Job>, Vec<Cell>) {
    let store = database
        .filter(|path| path.is_file())
        .and_then(|path| Store::open(path).ok());
    let Some(store) = store else {
        return (Vec::new(), Vec::new());
    };
    let postings = store.postings().unwrap_or_default();
    let jobs = jobs::open_jobs(&postings, &store.applications().unwrap_or_default());
    let cells = band::cells(&postings, &jobs);
    (jobs, cells)
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
                let profile = Profile::from(&Answers::load(&answers)?);
                let pass = funnel::run_once(&database, profile).await?;
                let (jobs, cells) = tokio::task::spawn_blocking(move || stored_jobs(Some(&database)))
                    .await
                    .map_err(|stopped| stopped.to_string())?;
                Ok::<_, String>((pass.summary(), jobs, cells))
            })
            .await;
        let at = now
            .in_tz("America/Los_Angeles")
            .expect("the time zone database has Pacific time")
            .strftime("%-I:%M %p");
        Some(match ran {
            Ok(Ok((summary, jobs, cells))) => Refreshed {
                status: format!("Checked at {at}: {summary}"),
                lists: Some((jobs, cells)),
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
