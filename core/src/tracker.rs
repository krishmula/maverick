//! Reads the job tracker spreadsheet into funnel records.
//!
//! The tracker is a Google Sheet exported as `.xlsx`. It has three tabs:
//! `Sheet1` (applications), `Found` (postings), and `Hiring Posts`.

use std::collections::HashMap;
use std::fmt;
use std::io::{Read, Seek};

use jiff::civil::{Date, DateTime, date};
use jiff::{Timestamp, ToSpan as _};

use crate::jobs::{Application, ApplicationStatus, HiringPost, Outreach, Posting, Stage};
use crate::xlsx::{self, Row, Sheet, XlsxError};

/// The sheet writes times with no zone. They are Pacific time.
const SHEET_ZONE: &str = "America/Los_Angeles";

pub const APPLICATIONS: &str = "Sheet1";
pub const POSTINGS: &str = "Found";
pub const HIRING_POSTS: &str = "Hiring Posts";

#[derive(Debug, Default, PartialEq)]
pub struct Tracker {
    pub postings: Vec<Posting>,
    pub applications: Vec<Application>,
    pub hiring_posts: Vec<HiringPost>,
    /// Rows the import could not read. They are left out of the records above.
    pub unreadable: Vec<RowProblem>,
}

#[derive(Debug, PartialEq)]
pub struct RowProblem {
    pub sheet: &'static str,
    pub row: u32,
    pub problem: String,
}

#[derive(Debug)]
pub enum TrackerError {
    Unreadable(XlsxError),
    MissingSheet(&'static str),
    MissingColumn { sheet: &'static str, column: &'static str },
}

impl fmt::Display for TrackerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unreadable(error) => write!(f, "{error}"),
            Self::MissingSheet(sheet) => write!(f, "the file has no tab named {sheet:?}"),
            Self::MissingColumn { sheet, column } => write!(f, "the {sheet:?} tab has no {column:?} column"),
        }
    }
}

impl std::error::Error for TrackerError {}

pub fn read(file: impl Read + Seek) -> Result<Tracker, TrackerError> {
    read_sheets(&xlsx::read(file).map_err(TrackerError::Unreadable)?)
}

/// Reads the tracker from its tabs, wherever they came from: an `.xlsx` export or the live sheet.
pub fn read_sheets(sheets: &[Sheet]) -> Result<Tracker, TrackerError> {
    let mut unreadable = Vec::new();
    let applications = Table::open(sheets, APPLICATIONS, &["Company", "Applied On", "Status", "Notes"])?
        .records(&mut unreadable, application);
    let postings = Table::open(
        sheets,
        POSTINGS,
        &[
            "Found at",
            "Source",
            "Company",
            "Title",
            "Location",
            "Link",
            "Email subject",
            "Status",
            "Score",
            "Variant",
            "Flags",
            "Notes",
        ],
    )?
    .records(&mut unreadable, posting);
    let hiring_posts = Table::open(
        sheets,
        HIRING_POSTS,
        &[
            "Found at",
            "Post link",
            "Poster",
            "Company",
            "Role",
            "Score",
            "Variant",
            "Outreach Status",
            "Notes",
        ],
    )?
    .records(&mut unreadable, hiring_post);
    Ok(Tracker {
        postings,
        applications,
        hiring_posts,
        unreadable,
    })
}

/// One tab, read by column name so that a moved or added column does not break the import.
struct Table<'a> {
    sheet: &'static str,
    columns: HashMap<&'static str, u32>,
    rows: &'a [Row],
}

struct Record<'a> {
    table: &'a Table<'a>,
    row: &'a Row,
}

impl<'a> Table<'a> {
    fn open(sheets: &'a [Sheet], sheet: &'static str, columns: &[&'static str]) -> Result<Self, TrackerError> {
        let found = sheets
            .iter()
            .find(|candidate| candidate.name == sheet)
            .ok_or(TrackerError::MissingSheet(sheet))?;
        let (header, rows) = found.rows.split_first().ok_or(TrackerError::MissingSheet(sheet))?;
        let columns = columns
            .iter()
            .map(|&column| {
                header
                    .cells()
                    .find(|(_, cell)| cell.text.trim() == column)
                    .map(|(index, _)| (column, index))
                    .ok_or(TrackerError::MissingColumn { sheet, column })
            })
            .collect::<Result<_, _>>()?;
        Ok(Self { sheet, columns, rows })
    }

    fn records<T>(&'a self, problems: &mut Vec<RowProblem>, read: fn(&Record<'a>) -> Result<T, String>) -> Vec<T> {
        let mut records = Vec::new();
        for row in self.rows {
            match read(&Record { table: self, row }) {
                Ok(record) => records.push(record),
                Err(problem) => problems.push(RowProblem {
                    sheet: self.sheet,
                    row: row.number,
                    problem,
                }),
            }
        }
        records
    }
}

impl<'a> Record<'a> {
    fn cell(&self, column: &str) -> Option<&'a xlsx::Cell> {
        self.row.cell(
            *self
                .table
                .columns
                .get(column)
                .expect("the reader asks only for columns it opened"),
        )
    }

    fn text(&self, column: &str) -> Option<&'a str> {
        self.cell(column)
            .map(|cell| cell.text.trim())
            .filter(|text| !text.is_empty())
    }

    fn owned(&self, column: &str) -> Option<String> {
        self.text(column).map(str::to_owned)
    }

    fn required(&self, column: &str) -> Result<&'a str, String> {
        self.text(column).ok_or_else(|| format!("{column} is empty"))
    }
}

fn application(record: &Record<'_>) -> Result<Application, String> {
    Ok(Application {
        sheet_row: Some(record.row.number),
        role: record.owned("Company"),
        link: record
            .cell("Company")
            .and_then(|cell| cell.link.as_deref())
            .map(web_link)
            .transpose()?,
        applied_on: applied_on(record.required("Applied On")?)?,
        status: labeled(record.required("Status")?, ApplicationStatus::parse)?,
        notes: record.owned("Notes"),
    })
}

fn posting(record: &Record<'_>) -> Result<Posting, String> {
    Ok(Posting {
        link: web_link(record.required("Link")?)?,
        found_at: serial_time(record.required("Found at")?)?,
        source: record.required("Source")?.to_owned(),
        company: record.required("Company")?.to_owned(),
        title: record.required("Title")?.to_owned(),
        location: record.owned("Location"),
        email_subject: record.owned("Email subject"),
        stage: labeled(record.required("Status")?, Stage::parse)?,
        score: record.text("Score").map(score).transpose()?,
        variant: record.owned("Variant"),
        flags: record.owned("Flags"),
        notes: record.owned("Notes"),
        address: None,
        sponsorship: None,
    })
}

fn hiring_post(record: &Record<'_>) -> Result<HiringPost, String> {
    // The status can carry a detail after a dash, as in "Replied — referral in progress".
    let (status, detail) = match record.required("Outreach Status")?.split_once(" — ") {
        Some((status, detail)) => (status, Some(detail)),
        None => (record.required("Outreach Status")?, None),
    };
    let notes = match (detail, record.text("Notes")) {
        (Some(detail), Some(notes)) => Some(format!("[{detail}] {notes}")),
        (Some(detail), None) => Some(format!("[{detail}]")),
        (None, notes) => notes.map(str::to_owned),
    };
    Ok(HiringPost {
        post_link: web_link(record.required("Post link")?)?,
        role: record.required("Role")?.to_owned(),
        found_at: pacific_time(record.required("Found at")?)?,
        poster: record.required("Poster")?.to_owned(),
        company: record.required("Company")?.to_owned(),
        score: score(record.required("Score")?)?,
        variant: record.required("Variant")?.to_owned(),
        outreach: labeled(status, Outreach::parse)?,
        notes,
    })
}

/// Matches a label such as "Not started" against stored names such as `not_started`.
fn labeled<T>(label: &str, parse: fn(&str) -> Option<T>) -> Result<T, String> {
    parse(&label.trim().to_lowercase().replace(' ', "_"))
        .ok_or_else(|| format!("{label:?} is not a status this import knows"))
}

fn web_link(text: &str) -> Result<String, String> {
    if text.starts_with("https://") || text.starts_with("http://") {
        Ok(text.to_owned())
    } else {
        Err(format!("{text:?} is not a web address"))
    }
}

fn score(text: &str) -> Result<u8, String> {
    let number: f64 = text
        .parse()
        .map_err(|_| format!("the score {text:?} is not a number"))?;
    if number.fract() != 0.0 || !(0.0..=100.0).contains(&number) {
        return Err(format!("the score {text:?} is not a whole number from 0 to 100"));
    }
    Ok(number as u8)
}

/// A spreadsheet stores a date as days since 1899-12-30, with the time of day as the fraction.
fn serial(text: &str) -> Result<DateTime, String> {
    let days: f64 = text
        .parse()
        .map_err(|_| format!("{text:?} is not a spreadsheet date"))?;
    if !(1.0..200_000.0).contains(&days) {
        return Err(format!("{text:?} is outside the range of spreadsheet dates"));
    }
    let seconds = (days.fract() * 86_400.0).round() as i64;
    date(1899, 12, 30)
        .at(0, 0, 0, 0)
        .checked_add((days.trunc() as i64).days().seconds(seconds))
        .map_err(|error| format!("{text:?} is not a spreadsheet date: {error}"))
}

fn in_sheet_zone(time: DateTime) -> Result<Timestamp, String> {
    time.in_tz(SHEET_ZONE)
        .map(|zoned| zoned.timestamp())
        .map_err(|error| format!("{time} has no Pacific time: {error}"))
}

fn serial_time(text: &str) -> Result<Timestamp, String> {
    in_sheet_zone(serial(text)?)
}

/// A time written as `2026-11-17 00:52 PT`.
fn pacific_time(text: &str) -> Result<Timestamp, String> {
    let written = text
        .strip_suffix(" PT")
        .ok_or_else(|| format!("{text:?} does not end in PT"))?;
    let time =
        DateTime::strptime("%Y-%m-%d %H:%M", written).map_err(|error| format!("{text:?} is not a time: {error}"))?;
    in_sheet_zone(time)
}

/// The sheet holds most dates as numbers and a few as `MM/DD/YYYY` text.
fn applied_on(text: &str) -> Result<Date, String> {
    if text.contains('/') {
        Date::strptime("%m/%d/%Y", text).map_err(|error| format!("{text:?} is not a date: {error}"))
    } else {
        serial(text).map(|time| time.date())
    }
}
