//! The live tracker: the user's Google Sheet, read and written through the Sheets API.
//!
//! The sheet is the source of truth. A pass reads it into the store, scores what is still found,
//! and writes the decisions back. A decision goes only into a row whose status still says found
//! when Maverick writes, so another scorer working on the same sheet is never overwritten.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::credentials::Password;
use crate::google::{self, Client};
use crate::jobs::{Posting, Stage};
use crate::tracker::{self, APPLICATIONS, HIRING_POSTS, POSTINGS, Tracker};
use crate::xlsx::{Cell, Row, Sheet};

const SHEETS_API: &str = "https://sheets.googleapis.com/v4/spreadsheets";

/// Which sheet is the tracker, and which connected account opens it.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct TrackerSheet {
    pub account: String,
    pub spreadsheet_id: String,
    pub name: String,
}

pub fn config_path() -> Option<PathBuf> {
    crate::data_dir().map(|dir| dir.join("tracker-sheet.json"))
}

impl TrackerSheet {
    /// The linked sheet, or `None` before the user links one.
    pub fn linked(file: &Path) -> Result<Option<Self>, String> {
        match std::fs::read_to_string(file) {
            Ok(text) => serde_json::from_str(&text)
                .map(Some)
                .map_err(|error| format!("{} did not parse: {error}", file.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(format!("could not read {}: {error}", file.display())),
        }
    }

    pub fn save(&self, file: &Path) -> Result<(), String> {
        let text = serde_json::to_string_pretty(self).expect("a tracker sheet always serializes");
        std::fs::write(file, text + "\n").map_err(|error| format!("could not write {}: {error}", file.display()))
    }
}

/// A session on the linked tracker, or `None` when no tracker is linked.
pub async fn linked_session() -> Result<Option<Session>, String> {
    let file = config_path().ok_or("this system has no application data directory")?;
    let Some(sheet) = TrackerSheet::linked(&file)? else {
        return Ok(None);
    };
    let client = Client::stored()
        .await?
        .ok_or("a tracker sheet is linked but no Google OAuth client is stored")?;
    Session::open(&client, sheet).await.map(Some)
}

/// Spreadsheets whose name holds `name`, in every connected account, as (account, id, name).
pub async fn find(client: &Client, accounts: &[google::Account], name: &str) -> Result<Vec<TrackerSheet>, String> {
    let mut found = Vec::new();
    for account in accounts {
        let token = google::access_token(client, &account.email).await?;
        let query = format!(
            "name contains '{}' and mimeType = 'application/vnd.google-apps.spreadsheet' and trashed = false",
            name.replace('\\', "\\\\").replace('\'', "\\'")
        );
        let url = format!(
            "https://www.googleapis.com/drive/v3/files?q={}&fields=files(id,name)&orderBy=modifiedTime%20desc",
            encoded(&query)
        );
        let reply = google::call(&token, "GET", &url, None).await?;
        for file in reply["files"].as_array().into_iter().flatten() {
            if let (Some(id), Some(name)) = (file["id"].as_str(), file["name"].as_str()) {
                found.push(TrackerSheet {
                    account: account.email.clone(),
                    spreadsheet_id: id.to_owned(),
                    name: name.to_owned(),
                });
            }
        }
    }
    Ok(found)
}

/// The tracker as the sheet holds it now, with where each posting and each column is.
pub struct Snapshot {
    pub tracker: Tracker,
    /// The sheet row of each posting, by link.
    pub rows: HashMap<String, u32>,
    /// The `Found` tab's columns by header, counting from 0.
    pub columns: HashMap<String, u32>,
}

/// One open session on the sheet: the sheet and a fresh access token for its account.
pub struct Session {
    pub sheet: TrackerSheet,
    token: Password,
}

impl Session {
    pub async fn open(client: &Client, sheet: TrackerSheet) -> Result<Self, String> {
        let token = google::access_token(client, &sheet.account).await?;
        Ok(Self { sheet, token })
    }

    fn url(&self, rest: &str) -> String {
        format!("{SHEETS_API}/{}{rest}", self.sheet.spreadsheet_id)
    }

    /// Reads the three tabs.
    pub async fn read(&self) -> Result<Snapshot, String> {
        let ranges: Vec<String> = [APPLICATIONS, POSTINGS, HIRING_POSTS]
            .iter()
            .map(|tab| format!("ranges={}", encoded(tab)))
            .collect();
        let url = self.url(&format!(
            "?{}&fields=sheets(properties.title,data(startRow,rowData.values(effectiveValue,hyperlink)))",
            ranges.join("&")
        ));
        let reply = google::call(&self.token, "GET", &url, None).await?;
        let sheets = grid_sheets(&reply);
        let tracker = tracker::read_sheets(&sheets).map_err(|error| error.to_string())?;
        let found = sheets
            .iter()
            .find(|sheet| sheet.name == POSTINGS)
            .ok_or("the sheet has no Found tab")?;
        let columns = header(found);
        let link_column = *columns.get("Link").ok_or("the Found tab has no Link column")?;
        let rows = found
            .rows
            .iter()
            .filter(|row| row.number > 1)
            .filter_map(|row| {
                let cell = row.cell(link_column)?;
                let link = cell.link.clone().unwrap_or_else(|| cell.text.trim().to_owned());
                (!link.is_empty()).then_some((link, row.number))
            })
            .collect();
        Ok(Snapshot { tracker, rows, columns })
    }

    /// Writes each decision into its row, if the row still says found. Returns how many it wrote.
    pub async fn write_decisions(&self, snapshot: &Snapshot, decided: &[Posting]) -> Result<usize, String> {
        let column = |name: &str| {
            snapshot
                .columns
                .get(name)
                .copied()
                .ok_or_else(|| format!("the Found tab has no {name} column"))
        };
        let status = column("Status")?;
        let targets: Vec<(&Posting, u32)> = decided
            .iter()
            .filter(|posting| posting.stage != Stage::Found)
            .filter_map(|posting| Some((posting, *snapshot.rows.get(&posting.link)?)))
            .collect();
        if targets.is_empty() {
            return Ok(0);
        }

        // Read the status of each row again now: another scorer may have decided it since the pass began.
        let ranges: Vec<String> = targets
            .iter()
            .map(|(_, row)| format!("ranges={}", encoded(&cell_address(status, *row))))
            .collect();
        let current = google::call(
            &self.token,
            "GET",
            &self.url(&format!("/values:batchGet?{}", ranges.join("&"))),
            None,
        )
        .await?;
        let still_found: Vec<bool> = current["valueRanges"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|range| {
                range["values"][0][0]
                    .as_str()
                    .is_some_and(|text| text.trim().eq_ignore_ascii_case("found"))
            })
            .collect();

        let fields = [
            (status, None),
            (column("Score")?, Some("Score")),
            (column("Variant")?, Some("Variant")),
            (column("Flags")?, Some("Flags")),
            (column("Notes")?, Some("Notes")),
        ];
        let mut data = Vec::new();
        let mut written = 0;
        for ((posting, row), still) in targets.iter().zip(still_found.iter().chain(std::iter::repeat(&false))) {
            if !still {
                continue;
            }
            written += 1;
            for (at, field) in fields {
                data.push(json!({
                    "range": cell_address(at, *row),
                    "values": [[field_value(posting, field)]],
                }));
            }
        }
        if data.is_empty() {
            return Ok(0);
        }
        let body = json!({ "valueInputOption": "RAW", "data": data });
        google::call(&self.token, "POST", &self.url("/values:batchUpdate"), Some(&body)).await?;
        Ok(written)
    }

    /// Adds postings intake found as new rows at the end of the Found tab.
    pub async fn append_found(&self, snapshot: &Snapshot, postings: &[Posting]) -> Result<usize, String> {
        if postings.is_empty() {
            return Ok(0);
        }
        let width = snapshot.columns.values().max().map_or(0, |last| last + 1) as usize;
        let rows: Vec<Vec<Value>> = postings
            .iter()
            .map(|posting| {
                let mut row = vec![Value::String(String::new()); width];
                for (name, &at) in &snapshot.columns {
                    row[at as usize] = new_row_value(posting, name);
                }
                row
            })
            .collect();
        let range = encoded(&format!("{POSTINGS}!A1"));
        let url = self.url(&format!(
            "/values/{range}:append?valueInputOption=USER_ENTERED&insertDataOption=INSERT_ROWS"
        ));
        google::call(&self.token, "POST", &url, Some(&json!({ "values": rows }))).await?;
        Ok(postings.len())
    }
}

/// The tracker's word for a stage, such as `Review`.
fn stage_label(stage: Stage) -> String {
    let name = stage.as_str();
    let mut label = name[..1].to_uppercase();
    label.push_str(&name[1..]);
    label
}

/// What a decision writes into one column. `None` is the Status column.
fn field_value(posting: &Posting, field: Option<&str>) -> Value {
    match field {
        None => Value::String(stage_label(posting.stage)),
        Some("Score") => posting.score.map_or(Value::String(String::new()), |score| json!(score)),
        Some("Variant") => Value::String(posting.variant.clone().unwrap_or_default()),
        Some("Flags") => Value::String(posting.flags.clone().unwrap_or_default()),
        Some(_) => Value::String(posting.notes.clone().unwrap_or_default()),
    }
}

/// What a new row holds in the column named `name`, as a person would type it.
fn new_row_value(posting: &Posting, name: &str) -> Value {
    let text = |value: &Option<String>| Value::String(value.clone().unwrap_or_default());
    match name {
        "Found at" => Value::String(sheet_time(posting.found_at)),
        "Source" => Value::String(posting.source.clone()),
        "Company" => Value::String(posting.company.clone()),
        "Title" => Value::String(posting.title.clone()),
        "Location" => text(&posting.location),
        "Link" => Value::String(posting.link.clone()),
        "Email subject" => text(&posting.email_subject),
        "Status" => Value::String(stage_label(posting.stage)),
        "Score" | "Variant" | "Flags" | "Notes" => field_value(posting, Some(name)),
        _ => Value::String(String::new()),
    }
}

/// A time as the sheet's own date cells take it, in Pacific time.
fn sheet_time(at: Timestamp) -> String {
    at.in_tz("America/Los_Angeles")
        .expect("the time zone database has Pacific time")
        .strftime("%-m/%-d/%Y %H:%M:%S")
        .to_string()
}

/// The header row of a tab: each column's name and where it is.
fn header(sheet: &Sheet) -> HashMap<String, u32> {
    sheet
        .rows
        .iter()
        .find(|row| row.number == 1)
        .map(|row| {
            row.cells()
                .map(|(at, cell)| (cell.text.trim().to_owned(), at))
                .filter(|(name, _)| !name.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// The Sheets API's grid reply as tabs of rows, like an `.xlsx` file reads.
/// A number cell holds the number as text, such as `46267.5`, as the `.xlsx` reader gives it.
fn grid_sheets(reply: &Value) -> Vec<Sheet> {
    reply["sheets"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|sheet| {
            let mut rows = Vec::new();
            for block in sheet["data"].as_array().into_iter().flatten() {
                let start = block["startRow"].as_u64().unwrap_or(0) as u32;
                for (offset, row) in block["rowData"].as_array().into_iter().flatten().enumerate() {
                    let cells = row["values"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .enumerate()
                        .filter_map(|(at, cell)| {
                            let value = &cell["effectiveValue"];
                            let text = match (&value["stringValue"], &value["numberValue"], &value["boolValue"]) {
                                (Value::String(text), _, _) => text.clone(),
                                (_, Value::Number(number), _) => number.to_string(),
                                (_, _, Value::Bool(flag)) => flag.to_string().to_uppercase(),
                                _ => String::new(),
                            };
                            let link = cell["hyperlink"].as_str().map(str::to_owned);
                            (!text.is_empty() || link.is_some()).then_some((at as u32, Cell { text, link }))
                        });
                    rows.push(Row::new(start + offset as u32 + 1, cells));
                }
            }
            Sheet {
                name: sheet["properties"]["title"].as_str().unwrap_or_default().to_owned(),
                rows,
            }
        })
        .collect()
}

/// `Found!H12` for column 7 of row 12.
fn cell_address(column: u32, row: u32) -> String {
    format!("{POSTINGS}!{}{row}", column_letters(column))
}

fn column_letters(mut column: u32) -> String {
    let mut letters = Vec::new();
    loop {
        letters.push(char::from(b'A' + (column % 26) as u8));
        if column < 26 {
            break;
        }
        column = column / 26 - 1;
    }
    letters.iter().rev().collect()
}

fn encoded(text: &str) -> String {
    let mut out = String::new();
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found_tab_reply() -> Value {
        let header: Vec<Value> = [
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
        ]
        .iter()
        .map(|name| json!({ "effectiveValue": { "stringValue": name } }))
        .collect();
        let text = |value: &str| json!({ "effectiveValue": { "stringValue": value } });
        let row = vec![
            json!({ "effectiveValue": { "numberValue": 46347.5 } }),
            text("Notify.careers"),
            text("Example Co"),
            text("Backend Engineer"),
            text("Austin, Texas, United States"),
            json!({ "effectiveValue": { "stringValue": "https://jobs.example.com/1" }, "hyperlink": "https://jobs.example.com/1" }),
            text("Found 1 new job posting!"),
            text("Found"),
        ];
        let tab = |name: &str, rows: Value| json!({ "properties": { "title": name }, "data": [{ "rowData": rows }] });
        json!({ "sheets": [
            tab("Sheet1", json!([{ "values": [text("Company"), text("Applied On"), text("Status"), text("Notes")] }])),
            tab("Found", json!([{ "values": header }, { "values": row }])),
            tab("Hiring Posts", json!([{ "values": [
                text("Found at"), text("Post link"), text("Poster"), text("Company"), text("Role"),
                text("Score"), text("Variant"), text("Outreach Status"), text("Notes"),
            ] }])),
        ] })
    }

    #[test]
    fn the_grid_reply_reads_like_the_exported_file() {
        let sheets = grid_sheets(&found_tab_reply());
        let tracker = tracker::read_sheets(&sheets).unwrap();
        assert!(tracker.unreadable.is_empty(), "{:?}", tracker.unreadable);
        let posting = &tracker.postings[0];
        assert_eq!(posting.link, "https://jobs.example.com/1");
        assert_eq!(posting.stage, Stage::Found);
        assert_eq!(posting.found_at.to_string(), "2026-11-21T20:00:00Z");

        let found = sheets.iter().find(|sheet| sheet.name == "Found").unwrap();
        let columns = header(found);
        assert_eq!(columns["Link"], 5);
        assert_eq!(columns["Notes"], 11);
    }

    #[test]
    fn column_letters_run_past_z() {
        let letters: Vec<String> = [0, 7, 25, 26, 27, 51, 52, 701, 702].map(column_letters).to_vec();
        assert_eq!(letters, ["A", "H", "Z", "AA", "AB", "AZ", "BA", "ZZ", "AAA"]);
        assert_eq!(cell_address(7, 12), "Found!H12");
    }

    #[test]
    fn a_decision_and_a_new_row_are_written_in_the_tracker_words() {
        let posting = Posting {
            link: "https://simplify.jobs/p/1".into(),
            found_at: "2026-11-22T17:05:09Z".parse().unwrap(),
            source: "Simplify".into(),
            company: "Example Co".into(),
            title: "Backend Engineer".into(),
            location: Some("Austin, TX".into()),
            email_subject: None,
            stage: Stage::Review,
            score: Some(72),
            variant: Some("Backend/Infra".into()),
            flags: None,
            notes: Some("Review - 72".into()),
            address: None,
            sponsorship: None,
        };
        assert_eq!(field_value(&posting, None), json!("Review"));
        assert_eq!(field_value(&posting, Some("Score")), json!(72));
        assert_eq!(field_value(&posting, Some("Flags")), json!(""));
        assert_eq!(new_row_value(&posting, "Found at"), json!("11/22/2026 09:05:09"));
        assert_eq!(new_row_value(&posting, "Link"), json!("https://simplify.jobs/p/1"));
        assert_eq!(new_row_value(&posting, "Something else"), json!(""));
        assert_eq!(stage_label(Stage::Found), "Found");
    }
}
