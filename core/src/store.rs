//! The local database of the job funnel. One `Store` holds the only connection that writes.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use jiff::Timestamp;

use rusqlite::types::Type;
use rusqlite::{Connection, Row, params};

use crate::jobs::{Application, ApplicationStatus, Decision, Outreach, Posting, Sponsorship, Stage};
use crate::tracker::Tracker;

pub use rusqlite::Error as StoreError;

/// One entry per schema version, applied in order. Add a new entry to change the schema. Never edit an old one.
const MIGRATIONS: &[&str] = &[
    "
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
",
    "
    ALTER TABLE postings ADD COLUMN address TEXT;
    ALTER TABLE postings ADD COLUMN sponsorship TEXT
        CHECK (sponsorship IN ('sponsors', 'no_sponsorship', 'unknown'));
",
    "
    CREATE TABLE chats (
        id INTEGER PRIMARY KEY,
        title TEXT NOT NULL,
        started_at TEXT NOT NULL,
        updated_at TEXT NOT NULL
    ) STRICT;

    CREATE TABLE chat_lines (
        id INTEGER PRIMARY KEY,
        chat_id INTEGER NOT NULL REFERENCES chats (id) ON DELETE CASCADE,
        said_at TEXT NOT NULL,
        line TEXT NOT NULL
    ) STRICT;
    CREATE INDEX chat_lines_by_chat ON chat_lines (chat_id, id);
",
    // Decisions live apart from the postings, so a sheet sync that rewrites a posting keeps them.
    "
    CREATE TABLE decisions (
        link TEXT PRIMARY KEY,
        decision TEXT NOT NULL CHECK (decision IN ('approved', 'skipped')),
        decided_at TEXT NOT NULL
    ) STRICT;
",
    "
    CREATE TABLE hiring_posts_typed (
        id INTEGER PRIMARY KEY,
        post_link TEXT NOT NULL,
        role TEXT NOT NULL,
        found_at TEXT NOT NULL,
        poster TEXT NOT NULL,
        company TEXT NOT NULL,
        score INTEGER NOT NULL CHECK (score BETWEEN 0 AND 100),
        variant TEXT NOT NULL,
        outreach TEXT NOT NULL
            CHECK (outreach IN ('not_started', 'skipped', 'drafted', 'typed', 'sent', 'replied')),
        notes TEXT,
        UNIQUE (post_link, role)
    ) STRICT;
    INSERT INTO hiring_posts_typed SELECT * FROM hiring_posts;
    DROP TABLE hiring_posts;
    ALTER TABLE hiring_posts_typed RENAME TO hiring_posts;

    CREATE TABLE linkedin_connections (
        id INTEGER PRIMARY KEY,
        first_name TEXT NOT NULL,
        last_name TEXT NOT NULL,
        profile_url TEXT,
        email TEXT,
        company TEXT NOT NULL,
        position TEXT NOT NULL,
        connected_on TEXT
    ) STRICT;

    CREATE TABLE outreach_checks (
        posting_link TEXT PRIMARY KEY,
        verdict TEXT NOT NULL
            CHECK (verdict IN ('connection', 'team_member', 'hiring_manager', 'deferred', 'cold')),
        team TEXT,
        people TEXT NOT NULL,
        note TEXT,
        checked_at TEXT NOT NULL
    ) STRICT;

    CREATE TABLE outreach_drafts (
        id INTEGER PRIMARY KEY,
        about_kind TEXT NOT NULL CHECK (about_kind IN ('posting', 'post')),
        about TEXT NOT NULL,
        person TEXT NOT NULL,
        headline TEXT NOT NULL,
        profile_url TEXT NOT NULL,
        slot TEXT NOT NULL CHECK (slot IN ('note', 'message')),
        text TEXT NOT NULL,
        status TEXT NOT NULL
            CHECK (status IN ('not_started', 'skipped', 'drafted', 'typed', 'sent', 'replied')),
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL,
        UNIQUE (about, profile_url)
    ) STRICT;
",
    "
    CREATE TABLE linkedin_company_pages (
        company TEXT PRIMARY KEY,
        slug TEXT NOT NULL,
        found_at TEXT NOT NULL
    ) STRICT;
",
    "
    CREATE TABLE outreach_drafts_inmail (
        id INTEGER PRIMARY KEY,
        about_kind TEXT NOT NULL CHECK (about_kind IN ('posting', 'post')),
        about TEXT NOT NULL,
        person TEXT NOT NULL,
        headline TEXT NOT NULL,
        profile_url TEXT NOT NULL,
        slot TEXT NOT NULL CHECK (slot IN ('note', 'message', 'inmail')),
        text TEXT NOT NULL,
        status TEXT NOT NULL
            CHECK (status IN ('not_started', 'skipped', 'drafted', 'typed', 'sent', 'replied')),
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL,
        subject TEXT,
        UNIQUE (about, profile_url)
    ) STRICT;
    INSERT INTO outreach_drafts_inmail
        (id, about_kind, about, person, headline, profile_url, slot, text, status, created_at, updated_at)
        SELECT id, about_kind, about, person, headline, profile_url, slot, text, status, created_at, updated_at
        FROM outreach_drafts;
    DROP TABLE outreach_drafts;
    ALTER TABLE outreach_drafts_inmail RENAME TO outreach_drafts;
",
    "
    CREATE TABLE post_readings (
        post_link TEXT PRIMARY KEY,
        poster TEXT NOT NULL,
        poster_url TEXT,
        headline TEXT NOT NULL,
        degree TEXT,
        text TEXT NOT NULL,
        read_at TEXT NOT NULL
    ) STRICT;

    CREATE TABLE inmail_credits (
        id INTEGER PRIMARY KEY CHECK (id = 1),
        credits INTEGER NOT NULL CHECK (credits >= 0),
        read_at TEXT NOT NULL
    ) STRICT;
",
    "
    CREATE TABLE scorer_people (
        posting_link TEXT PRIMARY KEY,
        note TEXT NOT NULL,
        people TEXT NOT NULL,
        read_at TEXT NOT NULL
    ) STRICT;
",
];

pub struct Store {
    pub(crate) db: Connection,
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
        // The app and a funnel pass each open the store. A short wait lets one finish its write.
        db.busy_timeout(Duration::from_secs(5))?;
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

    /// Writes what scoring decided: stage, score, variant, flags, notes, address and sponsorship.
    /// Each posting is found by its link. The fields intake wrote are left as they are.
    pub fn save_decisions(&mut self, postings: &[Posting]) -> Result<(), StoreError> {
        let write = self.db.transaction()?;
        {
            let mut update = write.prepare(
                "UPDATE postings SET stage = ?2, score = ?3, variant = ?4, flags = ?5, notes = ?6,
                     address = ?7, sponsorship = ?8
                 WHERE link = ?1",
            )?;
            for posting in postings {
                update.execute(params![
                    posting.link,
                    posting.stage.as_str(),
                    posting.score,
                    posting.variant,
                    posting.flags,
                    posting.notes,
                    posting.address,
                    posting.sponsorship.map(Sponsorship::as_str),
                ])?;
            }
        }
        write.commit()
    }

    /// Adds postings intake found and returns how many were new.
    /// A posting whose link the store already holds is left as it is.
    pub fn add_found(&mut self, postings: &[Posting]) -> Result<usize, StoreError> {
        let write = self.db.transaction()?;
        let mut added = 0;
        {
            let mut insert = write.prepare(
                "INSERT INTO postings
                     (link, found_at, source, company, title, location, email_subject, stage, score, variant, flags,
                      notes, address, sponsorship)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
                 ON CONFLICT (link) DO NOTHING",
            )?;
            for found in postings {
                added += insert.execute(params![
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
                    found.address,
                    found.sponsorship.map(Sponsorship::as_str),
                ])?;
            }
        }
        write.commit()?;
        Ok(added)
    }

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

    /// Records the user's decision on the posting at `link`. `None` takes the decision back.
    pub fn decide(&mut self, link: &str, decision: Option<Decision>) -> Result<(), StoreError> {
        match decision {
            Some(decision) => self.db.execute(
                "INSERT INTO decisions (link, decision, decided_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT (link) DO UPDATE SET decision = excluded.decision, decided_at = excluded.decided_at",
                params![link, decision.as_str(), Timestamp::now().to_string()],
            )?,
            None => self.db.execute("DELETE FROM decisions WHERE link = ?1", [link])?,
        };
        Ok(())
    }

    /// The user's decisions, by posting link.
    pub fn decisions(&self) -> Result<HashMap<String, Decision>, StoreError> {
        let mut select = self.db.prepare("SELECT link, decision FROM decisions")?;
        select
            .query_map([], |row| Ok((row.get(0)?, stored(row, 1, Decision::parse)?)))?
            .collect()
    }

    /// Every posting, oldest first.
    pub fn postings(&self) -> Result<Vec<Posting>, StoreError> {
        let mut select = self.db.prepare(
            "SELECT link, found_at, source, company, title, location, email_subject, stage, score, variant, flags, notes,
                 address, sponsorship
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
                    address: row.get(12)?,
                    sponsorship: match row.get_ref(13)?.as_str_or_null()? {
                        Some(_) => Some(stored(row, 13, Sponsorship::parse)?),
                        None => None,
                    },
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
pub(crate) fn stored<T>(row: &Row<'_>, column: usize, parse: impl Fn(&str) -> Option<T>) -> Result<T, StoreError> {
    let text: String = row.get(column)?;
    parse(&text).ok_or_else(|| {
        StoreError::FromSqlConversionFailure(
            column,
            Type::Text,
            format!("{text:?} is not a value this version of Maverick wrote").into(),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fifth_migration_keeps_the_hiring_posts_and_lets_them_be_typed() {
        let scratch = tempfile::tempdir().unwrap();
        let path = scratch.path().join("maverick.db");
        {
            let db = Connection::open(&path).unwrap();
            for migration in &MIGRATIONS[..3] {
                db.execute_batch(migration).unwrap();
            }
            db.pragma_update(None, "user_version", 3).unwrap();
            db.execute(
                "INSERT INTO hiring_posts (post_link, role, found_at, poster, company, score, variant, outreach, notes)
                 VALUES ('https://example.test/post/1', 'Backend Engineer', '2026-11-20T17:00:00Z', 'Sam Poster',
                     'Example Co', 81, 'Backend/Infra', 'replied', 'a note')",
                [],
            )
            .unwrap();
        }
        let store = Store::open(&path).unwrap();
        let version: u32 = store
            .db
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, MIGRATIONS.len() as u32);
        let kept: (String, String, Option<String>) = store
            .db
            .query_row("SELECT post_link, outreach, notes FROM hiring_posts", [], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .unwrap();
        assert_eq!(
            kept,
            (
                "https://example.test/post/1".to_owned(),
                "replied".to_owned(),
                Some("a note".to_owned())
            )
        );
        store
            .db
            .execute("UPDATE hiring_posts SET outreach = 'typed'", [])
            .unwrap();
    }

    #[test]
    fn drafts_made_before_inmail_survive_and_an_inmail_can_be_stored() {
        let scratch = tempfile::tempdir().unwrap();
        let path = scratch.path().join("maverick.db");
        {
            let db = Connection::open(&path).unwrap();
            for migration in &MIGRATIONS[..6] {
                db.execute_batch(migration).unwrap();
            }
            db.pragma_update(None, "user_version", 6).unwrap();
            db.execute(
                "INSERT INTO outreach_drafts (about_kind, about, person, headline, profile_url, slot, text, status,
                     created_at, updated_at)
                 VALUES ('posting', 'https://example.test/p/1', 'Bo Sample', 'Engineer', 'https://www.linkedin.com/in/bo/',
                     'note', 'Hi Bo.', 'typed', '2026-11-20T17:00:00Z', '2026-11-21T17:00:00Z')",
                [],
            )
            .unwrap();
        }
        let store = Store::open(&path).unwrap();
        let kept: (i64, String, String, Option<String>) = store
            .db
            .query_row("SELECT id, slot, status, subject FROM outreach_drafts", [], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })
            .unwrap();
        assert_eq!(kept, (1, "note".to_owned(), "typed".to_owned(), None));
        store
            .db
            .execute(
                "UPDATE outreach_drafts SET slot = 'inmail', subject = 'About the role'",
                [],
            )
            .unwrap();
        assert!(
            store
                .db
                .execute("UPDATE outreach_drafts SET slot = 'letter'", [])
                .is_err()
        );
    }
}
