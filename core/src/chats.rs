//! Mav's chats, kept in the local database so the user can come back to them.
//!
//! A saved proposal keeps what its card showed and how it ended. It cannot be confirmed again
//! later, because the tasks and calendar it was checked against will have moved on.

use jiff::Timestamp;
use rusqlite::{OptionalExtension as _, params};
use serde::{Deserialize, Serialize};

use crate::store::{Store, StoreError};

/// One saved line of a chat.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Line {
    User {
        text: String,
    },
    Mav {
        text: String,
    },
    /// A proposal Maverick could not use, and why.
    Refused {
        text: String,
    },
    Proposal {
        label: String,
        title: String,
        facts: Vec<(String, String)>,
        /// How it ended: proposed, skipped, done or failed, with the detail.
        outcome: String,
    },
}

/// A chat in the list, newest first.
#[derive(Clone, Debug, PartialEq)]
pub struct Chat {
    pub id: i64,
    pub title: String,
    pub updated_at: Timestamp,
}

/// The longest title a chat takes from its first message.
const TITLE_LENGTH: usize = 60;

/// A chat's title: its first message, cut at a word near 60 characters.
pub fn title_of(message: &str) -> String {
    let message = message.trim();
    if message.chars().count() <= TITLE_LENGTH {
        return message.to_owned();
    }
    let cut: String = message.chars().take(TITLE_LENGTH).collect();
    let cut = cut.rsplit_once(' ').map_or(cut.as_str(), |(head, _)| head);
    format!("{}…", cut.trim_end())
}

fn parse_time(text: String) -> Timestamp {
    text.parse().unwrap_or(Timestamp::UNIX_EPOCH)
}

impl Store {
    /// Starts a chat titled after its first message, and returns its id.
    pub fn new_chat(&self, first_message: &str, at: Timestamp) -> Result<i64, StoreError> {
        self.db.execute(
            "INSERT INTO chats (title, started_at, updated_at) VALUES (?1, ?2, ?2)",
            params![title_of(first_message), at.to_string()],
        )?;
        Ok(self.db.last_insert_rowid())
    }

    /// The most recent chats, newest first.
    pub fn chats(&self, limit: usize) -> Result<Vec<Chat>, StoreError> {
        let mut query = self
            .db
            .prepare("SELECT id, title, updated_at FROM chats ORDER BY updated_at DESC, id DESC LIMIT ?1")?;
        query
            .query_map([limit as i64], |row| {
                Ok(Chat {
                    id: row.get(0)?,
                    title: row.get(1)?,
                    updated_at: parse_time(row.get(2)?),
                })
            })?
            .collect()
    }

    /// Adds a line to `chat` and returns the line's id.
    pub fn add_line(&self, chat: i64, line: &Line, at: Timestamp) -> Result<i64, StoreError> {
        let text = serde_json::to_string(line).expect("a line always serializes");
        self.db.execute(
            "INSERT INTO chat_lines (chat_id, said_at, line) VALUES (?1, ?2, ?3)",
            params![chat, at.to_string(), text],
        )?;
        let id = self.db.last_insert_rowid();
        self.db.execute(
            "UPDATE chats SET updated_at = ?2 WHERE id = ?1",
            params![chat, at.to_string()],
        )?;
        Ok(id)
    }

    /// Records how a saved proposal ended.
    pub fn set_outcome(&self, line: i64, outcome: &str) -> Result<(), StoreError> {
        let saved: Option<String> = self
            .db
            .query_row("SELECT line FROM chat_lines WHERE id = ?1", [line], |row| row.get(0))
            .optional()?;
        let Some(Ok(Line::Proposal {
            label, title, facts, ..
        })) = saved.map(|text| serde_json::from_str(&text))
        else {
            return Ok(());
        };
        let updated = Line::Proposal {
            label,
            title,
            facts,
            outcome: outcome.to_owned(),
        };
        self.db.execute(
            "UPDATE chat_lines SET line = ?2 WHERE id = ?1",
            params![line, serde_json::to_string(&updated).expect("a line always serializes")],
        )?;
        Ok(())
    }

    /// Every line of `chat`, oldest first. A line that no longer parses is left out.
    pub fn lines(&self, chat: i64) -> Result<Vec<(i64, Line)>, StoreError> {
        let mut query = self
            .db
            .prepare("SELECT id, line FROM chat_lines WHERE chat_id = ?1 ORDER BY id")?;
        let rows: Vec<(i64, String)> = query
            .query_map([chat], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<_, _>>()?;
        Ok(rows
            .into_iter()
            .filter_map(|(id, text)| serde_json::from_str(&text).ok().map(|line| (id, line)))
            .collect())
    }

    pub fn delete_chat(&self, chat: i64) -> Result<(), StoreError> {
        self.db.execute("DELETE FROM chats WHERE id = ?1", [chat]).map(drop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_chat_keeps_its_lines_and_how_each_proposal_ended() {
        let scratch = tempfile::tempdir().unwrap();
        let store = Store::open(&scratch.path().join("maverick.db")).unwrap();
        let at = |seconds: i64| Timestamp::from_second(1_795_000_000 + seconds).unwrap();
        let first = store.new_chat("remind me to call the bank", at(0)).unwrap();
        let second = store.new_chat("plan the week", at(10)).unwrap();
        store
            .add_line(
                first,
                &Line::User {
                    text: "remind me to call the bank".into(),
                },
                at(20),
            )
            .unwrap();
        let card = store
            .add_line(
                first,
                &Line::Proposal {
                    label: "TickTick · New task".into(),
                    title: "Call the bank".into(),
                    facts: vec![("List".into(), "Inbox".into())],
                    outcome: "proposed".into(),
                },
                at(21),
            )
            .unwrap();
        store.set_outcome(card, "added to Inbox in TickTick").unwrap();

        let chats = store.chats(10).unwrap();
        assert_eq!(chats.iter().map(|chat| chat.id).collect::<Vec<_>>(), [first, second]);
        let lines = store.lines(first).unwrap();
        assert_eq!(lines.len(), 2);
        assert!(matches!(&lines[1].1, Line::Proposal { outcome, .. } if outcome == "added to Inbox in TickTick"));

        store.delete_chat(first).unwrap();
        assert!(store.lines(first).unwrap().is_empty());
        assert_eq!(store.chats(10).unwrap().len(), 1);
    }

    #[test]
    fn a_long_first_message_is_cut_at_a_word() {
        assert_eq!(title_of("  plan the week  "), "plan the week");
        let long = "mark the lease done, make the quiz prep high priority and move it to thursday at six";
        let title = title_of(long);
        assert!(
            title.ends_with('…') && title.chars().count() <= TITLE_LENGTH + 1,
            "{title}"
        );
        assert!(long.starts_with(title.trim_end_matches('…')));
    }
}
