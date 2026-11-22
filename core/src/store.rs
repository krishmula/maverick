//! The local database of the job funnel. One `Store` holds the only connection that writes.

use std::path::Path;

use rusqlite::types::Type;
use rusqlite::{Connection, Row, params};

use crate::jobs::{Application, ApplicationStatus, Outreach, Posting, Stage};
use crate::tracker::Tracker;

pub use rusqlite::Error as StoreError;

/// One entry per schema version, applied in order. Add a new entry to change the schema. Never edit an old one.
const MIGRATIONS: &[&str] = &["
    CREATE TABLE postings (
        id INTEGER PRIMARY KEY,
        link TEXT NOT NULL UNIQUE,
        found_at TEXT NOT NULL,
        source TEXT NOT NULL,
        company TEXT NOT NULL,
        title TEXT NOT NULL,
        location TEXT,
        email_subject TEXT,
        stage TEXT NOT NULL CHECK (stage IN ('found', 'duplicate', 'skipped', 'review', 'applied')),
        score INTEGER CHECK (score BETWEEN 0 AND 100),
        variant TEXT,
        flags TEXT,
        notes TEXT
    ) STRICT;

    CREATE TABLE applications (
        id INTEGER PRIMARY KEY,
        sheet_row INTEGER UNIQUE,
        role TEXT,
        link TEXT,
        applied_on TEXT NOT NULL,
        status TEXT NOT NULL CHECK (status IN ('pending', 'rejected')),
        notes TEXT
    ) STRICT;

    CREATE TABLE hiring_posts (
        id INTEGER PRIMARY KEY,
        post_link TEXT NOT NULL,
        role TEXT NOT NULL,
        found_at TEXT NOT NULL,
        poster TEXT NOT NULL,
        company TEXT NOT NULL,
        score INTEGER NOT NULL CHECK (score BETWEEN 0 AND 100),
        variant TEXT NOT NULL,
        outreach TEXT NOT NULL CHECK (outreach IN ('not_started', 'skipped', 'drafted', 'sent', 'replied')),
        notes TEXT,
        UNIQUE (post_link, role)
    ) STRICT;
"];

pub struct Store {
    db: Connection,
}

/// How many records the store holds, by state.
#[derive(Debug, PartialEq)]
pub struct Summary {
    pub postings: Vec<(Stage, u32)>,
    pub applications: Vec<(ApplicationStatus, u32)>,
    pub applications_without_role: u32,
    pub hiring_posts: Vec<(Outreach, u32)>,
}

impl Store {
    /// Opens the database at `path`, creating it or bringing its schema up to date first.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        let mut db = Connection::open(path)?;
        db.pragma_update(None, "journal_mode", "WAL")?;
        db.pragma_update(None, "foreign_keys", true)?;
        let applied: u32 = db.pragma_query_value(None, "user_version", |row| row.get(0))?;
        for (version, migration) in (1u32..).zip(MIGRATIONS).skip(applied as usize) {
            let step = db.transaction()?;
            step.execute_batch(migration)?;
            step.pragma_update(None, "user_version", version)?;
            step.commit()?;
        }
        Ok(Self { db })
    }

    /// Writes every record of the tracker, or none of them.
    /// A record the store already holds is updated in place, so a repeated import changes nothing.
    pub fn save_tracker(&mut self, tracker: &Tracker) -> Result<(), StoreError> {
        let write = self.db.transaction()?;
        {
            let mut posting = write.prepare(
                "INSERT INTO postings
                     (link, found_at, source, company, title, location, email_subject, stage, score, variant, flags, notes)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
                 ON CONFLICT (link) DO UPDATE SET
                     found_at = excluded.found_at, source = excluded.source, company = excluded.company,
                     title = excluded.title, location = excluded.location, email_subject = excluded.email_subject,
                     stage = excluded.stage, score = excluded.score, variant = excluded.variant,
                     flags = excluded.flags, notes = excluded.notes",
            )?;
            for found in &tracker.postings {
                posting.execute(params![
                    found.link,
                    found.found_at.to_string(),
                    found.source,
                    found.company,
                    found.title,
                    found.location,
                    found.email_subject,
                    found.stage.as_str(),
                    found.score,
                    found.variant,
                    found.flags,
                    found.notes,
                ])?;
            }

            let mut application = write.prepare(
                "INSERT INTO applications (sheet_row, role, link, applied_on, status, notes)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT (sheet_row) DO UPDATE SET
                     role = excluded.role, link = excluded.link, applied_on = excluded.applied_on,
                     status = excluded.status, notes = excluded.notes",
            )?;
            for sent in &tracker.applications {
                application.execute(params![
                    sent.sheet_row,
                    sent.role,
                    sent.link,
                    sent.applied_on.to_string(),
                    sent.status.as_str(),
                    sent.notes,
                ])?;
            }

            let mut hiring_post = write.prepare(
                "INSERT INTO hiring_posts (post_link, role, found_at, poster, company, score, variant, outreach, notes)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT (post_link, role) DO UPDATE SET
                     found_at = excluded.found_at, poster = excluded.poster, company = excluded.company,
                     score = excluded.score, variant = excluded.variant, outreach = excluded.outreach,
                     notes = excluded.notes",
            )?;
            for post in &tracker.hiring_posts {
                hiring_post.execute(params![
                    post.post_link,
                    post.role,
                    post.found_at.to_string(),
                    post.poster,
                    post.company,
                    post.score,
                    post.variant,
                    post.outreach.as_str(),
                    post.notes,
                ])?;
            }
        }
        write.commit()
    }

    /// Every posting, oldest first.
    /// Records an application the user submitted: the postings at `posting_links` become applied,
    /// and the application is added unless one with the same link is already recorded.
    pub fn record_application(&mut self, posting_links: &[String], sent: &Application) -> Result<(), StoreError> {
        let write = self.db.transaction()?;
        for link in posting_links {
            write.execute(
                "UPDATE postings SET stage = ?1 WHERE link = ?2",
                params![Stage::Applied.as_str(), link],
            )?;
        }
        write.execute(
            "INSERT INTO applications (sheet_row, role, link, applied_on, status, notes)
             SELECT ?1, ?2, ?3, ?4, ?5, ?6
             WHERE NOT EXISTS (SELECT 1 FROM applications WHERE link = ?3)",
            params![
                sent.sheet_row,
                sent.role,
                sent.link,
                sent.applied_on.to_string(),
                sent.status.as_str(),
                sent.notes,
            ],
        )?;
        write.commit()
    }

    pub fn postings(&self) -> Result<Vec<Posting>, StoreError> {
        let mut select = self.db.prepare(
            "SELECT link, found_at, source, company, title, location, email_subject, stage, score, variant, flags, notes
             FROM postings ORDER BY found_at, id",
        )?;
        select
            .query_map([], |row| {
                Ok(Posting {
                    link: row.get(0)?,
                    found_at: stored(row, 1, |text| text.parse().ok())?,
                    source: row.get(2)?,
                    company: row.get(3)?,
                    title: row.get(4)?,
                    location: row.get(5)?,
                    email_subject: row.get(6)?,
                    stage: stored(row, 7, Stage::parse)?,
                    score: row.get(8)?,
                    variant: row.get(9)?,
                    flags: row.get(10)?,
                    notes: row.get(11)?,
                })
            })?
            .collect()
    }

    /// Every application, in sheet order.
    pub fn applications(&self) -> Result<Vec<Application>, StoreError> {
        let mut select = self.db.prepare(
            "SELECT sheet_row, role, link, applied_on, status, notes FROM applications ORDER BY sheet_row, id",
        )?;
        select
            .query_map([], |row| {
                Ok(Application {
                    sheet_row: row.get(0)?,
                    role: row.get(1)?,
                    link: row.get(2)?,
                    applied_on: stored(row, 3, |text| text.parse().ok())?,
                    status: stored(row, 4, ApplicationStatus::parse)?,
                    notes: row.get(5)?,
                })
            })?
            .collect()
    }

    pub fn summary(&self) -> Result<Summary, StoreError> {
        Ok(Summary {
            postings: self.counts("postings", "stage", Stage::ALL, Stage::as_str)?,
            applications: self.counts(
                "applications",
                "status",
                ApplicationStatus::ALL,
                ApplicationStatus::as_str,
            )?,
            applications_without_role: self.db.query_row(
                "SELECT count(*) FROM applications WHERE role IS NULL",
                [],
                |row| row.get(0),
            )?,
            hiring_posts: self.counts("hiring_posts", "outreach", Outreach::ALL, Outreach::as_str)?,
        })
    }

    /// One count per state, in the order the states are declared. `table` and `column` are names from this file.
    fn counts<T: Copy>(
        &self,
        table: &str,
        column: &str,
        states: &[T],
        name: fn(T) -> &'static str,
    ) -> Result<Vec<(T, u32)>, StoreError> {
        let mut count = self
            .db
            .prepare(&format!("SELECT count(*) FROM {table} WHERE {column} = ?1"))?;
        states
            .iter()
            .map(|&state| Ok((state, count.query_row([name(state)], |row| row.get(0))?)))
            .collect()
    }
}

/// Reads a text column that this file wrote, such as a state name or a time.
fn stored<T>(row: &Row<'_>, column: usize, parse: impl Fn(&str) -> Option<T>) -> Result<T, StoreError> {
    let text: String = row.get(column)?;
    parse(&text).ok_or_else(|| {
        StoreError::FromSqlConversionFailure(
            column,
            Type::Text,
            format!("{text:?} is not a value this version of Maverick wrote").into(),
        )
    })
}
