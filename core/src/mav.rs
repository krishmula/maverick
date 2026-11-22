//! Mav: the user says what to remember or plan, and Mav proposes TickTick tasks and calendar blocks.
//!
//! The model only proposes. Every proposal is checked here against the real lists and calendar,
//! and nothing is written until the user confirms it.

use std::fmt::Write as _;

use jiff::civil::{Date, Time};
use jiff::tz::TimeZone;
use jiff::{ToSpan as _, ZonedDateTime};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::calendar::{self, Event, Slot};
use crate::model;
use crate::ticktick::{self, Agenda, Due, NewTask, Project};

const INSTRUCTIONS: &str = "You are Mav, the user's planning assistant. The user tells you what to remember, \
what to do, or what time to set aside. You see their TickTick lists and tasks, their calendar, and their free time. \
Propose a TickTick task (kind task) for anything to remember or do, and a calendar block (kind block) for time set aside to work on something. \
Propose only what the user asked for. Never say anything is done: the user confirms each proposal first. \
Dates and times are in the user's time zone; resolve words like tomorrow or Tuesday from the current time. \
A task with no time the user named gets a date only, or no date if none was named. \
remind_minutes_before is 0 when the user asks to be reminded at the time, null when they did not ask for a reminder. \
A block needs a date, a time and an end_time. When the user asks you to find time, choose from the free time listed, \
the earliest stretch that fits, and never one that overlaps an event. \
list is the exact name of one of the user's lists, or null for the Inbox. \
For a question about their schedule or tasks, answer from what you see, with no proposals. \
In reply, say in one or two short plain sentences what you propose or what you found. No em dashes. \
Never invent tasks or events.";

/// What Mav sees when it answers: the time, the lists, the tasks and the calendar.
#[derive(Clone, Debug)]
pub struct Context {
    pub now: ZonedDateTime,
    pub lists: Vec<Project>,
    pub agenda: Agenda,
    /// The calendar from the start of today through the coming week.
    pub events: Vec<Event>,
}

impl Context {
    /// Reads TickTick and the calendar. A service that is not connected contributes nothing.
    pub async fn read(now: ZonedDateTime) -> Result<Context, String> {
        let start = now.start_of_day().map_err(|error| error.to_string())?;
        let end = start.checked_add(8.days()).map_err(|error| error.to_string())?;
        let ticktick = async {
            match ticktick::stored_token().await? {
                Some(token) => {
                    let (lists, agenda) = tokio::try_join!(ticktick::projects(&token), ticktick::agenda(&token, &now))?;
                    Ok::<_, String>((lists, agenda))
                }
                None => Ok((Vec::new(), Agenda::default())),
            }
        };
        let events = calendar::events(start.timestamp(), end.timestamp(), now.time_zone());
        let ((lists, agenda), events) = tokio::try_join!(ticktick, events)?;
        Ok(Context {
            now,
            lists,
            agenda,
            events,
        })
    }

    fn zone(&self) -> &TimeZone {
        self.now.time_zone()
    }

    /// The free time from now through the coming week.
    pub fn free(&self) -> Vec<Slot> {
        (0..7)
            .filter_map(|ahead| self.now.date().checked_add(ahead.days()).ok())
            .flat_map(|day| calendar::free_slots(&self.events, day, &self.now))
            .collect()
    }

    /// The context as the model reads it.
    fn describe(&self) -> String {
        let mut text = format!(
            "Now: {} ({}).\n\nTickTick lists: Inbox",
            self.now.strftime("%A %Y-%m-%d %H:%M"),
            self.zone().iana_name().unwrap_or("local time")
        );
        for list in self.lists.iter().filter(|list| !list.closed) {
            let _ = write!(text, ", {}", list.name);
        }
        text.push_str("\n\nUndone tasks:\n");
        let sections = [
            ("overdue", &self.agenda.overdue),
            ("today", &self.agenda.today),
            ("coming", &self.agenda.upcoming),
        ];
        for (name, entries) in sections {
            for entry in entries {
                let _ = writeln!(
                    text,
                    "- [{name}] {} ({}, {})",
                    entry.title,
                    entry.due.label(),
                    entry.list
                );
            }
        }
        text.push_str("\nCalendar:\n");
        for event in &self.events {
            let day = match &event.span {
                calendar::Span::Days(first, _) => first.strftime("%a %Y-%m-%d").to_string(),
                calendar::Span::At(start, _) => start.strftime("%a %Y-%m-%d").to_string(),
            };
            let free = if event.busy { "" } else { ", marked free" };
            let _ = writeln!(text, "- {day} {}: {}{free}", event.hours(), event.title);
        }
        text.push_str("\nFree time:\n");
        for slot in self.free() {
            let _ = writeln!(
                text,
                "- {} {} to {} ({})",
                slot.start.strftime("%a %Y-%m-%d"),
                slot.start.strftime("%H:%M"),
                slot.end.strftime("%H:%M"),
                calendar::length(slot.minutes())
            );
        }
        text
    }
}

/// Who said a line of the conversation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Speaker {
    User,
    Mav,
}

/// A proposal as the model writes it, before it is checked.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct Draft {
    pub kind: String,
    pub title: String,
    pub list: Option<String>,
    pub date: Option<String>,
    pub time: Option<String>,
    pub end_time: Option<String>,
    pub remind_minutes_before: Option<i64>,
}

/// What Mav says back.
#[derive(Clone, Debug, PartialEq)]
pub struct Reply {
    pub text: String,
    /// Each checked proposal, or why the model's draft could not be used.
    pub proposals: Vec<Result<Action, String>>,
}

/// Something Mav can do once the user confirms it.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    Task {
        task: NewTask,
        /// The name of the list it goes to.
        list: String,
    },
    Block {
        title: String,
        start: ZonedDateTime,
        end: ZonedDateTime,
        /// The busy events it overlaps, which the user should see before confirming.
        clashes: Vec<String>,
    },
}

impl Action {
    pub fn title(&self) -> &str {
        match self {
            Action::Task { task, .. } => &task.title,
            Action::Block { title, .. } => title,
        }
    }

    /// The label on the proposal card.
    pub fn kind(&self) -> &'static str {
        match self {
            Action::Task { .. } => "TickTick · New task",
            Action::Block { .. } => "Calendar · New block",
        }
    }

    /// The facts on the proposal card, as label and value.
    pub fn facts(&self) -> Vec<(&'static str, String)> {
        match self {
            Action::Task { task, list } => {
                let mut facts = vec![("List", list.clone())];
                facts.push((
                    "Due",
                    match &task.due {
                        None => "no date".to_owned(),
                        Some(Due::Day(date)) => date.strftime("%a %-d %b").to_string(),
                        Some(Due::At(at)) => at.strftime("%a %-d %b · %H:%M").to_string(),
                    },
                ));
                if let Some(minutes) = task.remind_before {
                    let when = match (&task.due, minutes) {
                        (Some(Due::Day(_)), _) => "9:00 that day".to_owned(),
                        (_, 0) => "at the due time".to_owned(),
                        (_, minutes) => format!("{} before", calendar::length(minutes)),
                    };
                    facts.push(("Remind", when));
                }
                facts
            }
            Action::Block {
                start, end, clashes, ..
            } => {
                let mut facts = vec![(
                    "When",
                    format!("{} to {}", start.strftime("%a %-d %b · %H:%M"), end.strftime("%H:%M")),
                )];
                if !clashes.is_empty() {
                    facts.push(("Clashes", clashes.join(", ")));
                }
                facts
            }
        }
    }

    /// Writes the action to TickTick or the calendar. Returns what to tell the user.
    pub async fn carry_out(&self) -> Result<String, String> {
        match self {
            Action::Task { task, list } => {
                let token = ticktick::stored_token()
                    .await?
                    .ok_or("TickTick is not connected; run `maverick ticktick token`")?;
                let zone = match &task.due {
                    Some(Due::At(at)) => at.time_zone().clone(),
                    _ => TimeZone::system(),
                };
                ticktick::create(&token, task, &zone).await?;
                Ok(format!("Added to {list} in TickTick"))
            }
            Action::Block { title, start, end, .. } => {
                let account = calendar::add(title, start, end).await?;
                Ok(format!("Added to the calendar of {account}"))
            }
        }
    }
}

/// Asks Mav about `message`, with the conversation so far and what Mav sees.
pub async fn ask(message: &str, said: &[(Speaker, String)], context: &Context) -> Result<Reply, String> {
    let mut request = context.describe();
    if !said.is_empty() {
        request.push_str("\nThe conversation so far:\n");
        for (speaker, line) in said.iter().rev().take(12).rev() {
            let who = if *speaker == Speaker::User { "User" } else { "Mav" };
            let _ = writeln!(request, "{who}: {line}");
        }
    }
    let _ = write!(request, "\nThe user says: {message}\n");
    let reply = model::run(INSTRUCTIONS, request, &schema()).await?;
    let answer = &reply["structured_output"];
    let text = answer["reply"].as_str().unwrap_or_default().to_owned();
    let drafts: Vec<Draft> = serde_json::from_value(answer["proposals"].clone()).unwrap_or_default();
    Ok(Reply {
        text,
        proposals: drafts.iter().map(|draft| check(draft, context)).collect(),
    })
}

fn schema() -> Value {
    let maybe = |kind: &str| json!({ "type": [kind, "null"] });
    json!({
        "type": "object",
        "properties": {
            "reply": { "type": "string" },
            "proposals": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "kind": { "type": "string", "enum": ["task", "block"] },
                        "title": { "type": "string" },
                        "list": maybe("string"),
                        "date": { "type": ["string", "null"], "description": "YYYY-MM-DD" },
                        "time": { "type": ["string", "null"], "description": "HH:MM, 24-hour" },
                        "end_time": { "type": ["string", "null"], "description": "HH:MM, 24-hour, blocks only" },
                        "remind_minutes_before": maybe("integer"),
                    },
                    "required": ["kind", "title", "list", "date", "time", "end_time", "remind_minutes_before"],
                    "additionalProperties": false,
                },
            },
        },
        "required": ["reply", "proposals"],
        "additionalProperties": false,
    })
}

/// Turns the model's draft into an action, or says why it cannot be one.
pub fn check(draft: &Draft, context: &Context) -> Result<Action, String> {
    let title = draft.title.trim();
    if title.is_empty() {
        return Err("Mav proposed something with no title".to_owned());
    }
    let date = draft
        .date
        .as_deref()
        .map(|text| {
            text.parse::<Date>()
                .map_err(|_| format!("Mav gave a date Maverick cannot read: {text}"))
        })
        .transpose()?;
    let time = |text: Option<&str>| {
        text.map(|text| {
            text.parse::<Time>()
                .map_err(|_| format!("Mav gave a time Maverick cannot read: {text}"))
        })
        .transpose()
    };
    let (start, end) = (time(draft.time.as_deref())?, time(draft.end_time.as_deref())?);
    let at = |date: Date, time: Time| {
        date.to_datetime(time)
            .to_zoned_datetime(context.zone().clone())
            .map_err(|error| error.to_string())
    };
    match draft.kind.as_str() {
        "task" => {
            let (list_id, list) = list_named(draft.list.as_deref(), &context.lists);
            let due = match (date, start) {
                (Some(date), Some(time)) => Some(Due::At(at(date, time)?)),
                (Some(date), None) => Some(Due::Day(date)),
                (None, Some(time)) => Some(Due::At(at(context.now.date(), time)?)),
                (None, None) => None,
            };
            let task = NewTask {
                title: title.to_owned(),
                list_id,
                due,
                remind_before: draft.remind_minutes_before,
            };
            Ok(Action::Task { task, list })
        }
        "block" => {
            let (Some(date), Some(start), Some(end)) = (date, start, end) else {
                return Err(format!(
                    "Mav proposed a block for {title} without a date, start and end"
                ));
            };
            let (start, end) = (at(date, start)?, at(date, end)?);
            if end <= start {
                return Err(format!("Mav proposed a block for {title} that ends before it starts"));
            }
            let clashes = calendar::clashes(&context.events, &start, &end)
                .into_iter()
                .map(|event| event.title.clone())
                .collect();
            Ok(Action::Block {
                title: title.to_owned(),
                start,
                end,
                clashes,
            })
        }
        other => Err(format!("Mav proposed a kind of thing Maverick cannot do: {other}")),
    }
}

/// The id and name of the list called `name`, ignoring case. An unknown name means the Inbox.
fn list_named(name: Option<&str>, lists: &[Project]) -> (String, String) {
    name.and_then(|name| {
        lists
            .iter()
            .find(|list| !list.closed && list.name.trim().eq_ignore_ascii_case(name.trim()))
    })
    .map_or(("inbox".to_owned(), "Inbox".to_owned()), |list| {
        (list.id.clone(), list.name.clone())
    })
}

/// The task Mav suggests after an application goes through: follow up in a week, at 10:00.
pub fn follow_up(posting: &str, applied: &ZonedDateTime, lists: &[Project]) -> Option<Action> {
    let due = applied
        .date()
        .checked_add(7.days())
        .ok()?
        .to_datetime(Time::constant(10, 0, 0, 0));
    let due = due.to_zoned_datetime(applied.time_zone().clone()).ok()?;
    let job_list = lists
        .iter()
        .find(|list| !list.closed && list.name.to_lowercase().contains("job"))
        .map(|list| list.name.as_str());
    let (list_id, list) = list_named(job_list, lists);
    Some(Action::Task {
        task: NewTask {
            title: format!("Follow up on {posting}"),
            list_id,
            due: Some(Due::At(due)),
            remind_before: Some(0),
        },
        list,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calendar::Span;
    use jiff::civil::date;

    fn context() -> Context {
        let now = date(2026, 11, 22).at(16, 0, 0, 0).in_tz("America/Los_Angeles").unwrap();
        let at = |hour: i8| {
            date(2026, 11, 23)
                .at(hour, 0, 0, 0)
                .in_tz("America/Los_Angeles")
                .unwrap()
        };
        Context {
            now,
            lists: vec![
                Project {
                    id: "p1".into(),
                    name: "Job search".into(),
                    closed: false,
                },
                Project {
                    id: "p2".into(),
                    name: "Old".into(),
                    closed: true,
                },
            ],
            agenda: Agenda::default(),
            events: vec![Event {
                title: "Screen".into(),
                span: Span::At(at(10), at(11)),
                busy: true,
            }],
        }
    }

    fn draft(kind: &str) -> Draft {
        Draft {
            kind: kind.into(),
            title: "Prep".into(),
            ..Draft::default()
        }
    }

    #[test]
    fn a_task_goes_to_the_named_list_or_the_inbox() {
        let context = context();
        let named = Draft {
            list: Some("job SEARCH".into()),
            date: Some("2026-11-24".into()),
            time: Some("10:00".into()),
            remind_minutes_before: Some(0),
            ..draft("task")
        };
        let Ok(Action::Task { task, list }) = check(&named, &context) else {
            panic!("not a task")
        };
        assert_eq!((task.list_id.as_str(), list.as_str()), ("p1", "Job search"));
        assert_eq!(task.due.unwrap().label(), "Tue 24 · 10:00");

        let closed = Draft {
            list: Some("Old".into()),
            ..draft("task")
        };
        let Ok(Action::Task { task, list }) = check(&closed, &context) else {
            panic!("not a task")
        };
        assert_eq!(
            (task.list_id.as_str(), list.as_str(), task.due),
            ("inbox", "Inbox", None)
        );
    }

    #[test]
    fn a_block_needs_a_date_and_both_times_and_shows_its_clashes() {
        let context = context();
        assert!(
            check(
                &Draft {
                    date: Some("2026-11-23".into()),
                    ..draft("block")
                },
                &context
            )
            .is_err()
        );
        let backwards = Draft {
            date: Some("2026-11-23".into()),
            time: Some("12:00".into()),
            end_time: Some("11:00".into()),
            ..draft("block")
        };
        assert!(check(&backwards, &context).is_err());
        let over = Draft {
            date: Some("2026-11-23".into()),
            time: Some("10:30".into()),
            end_time: Some("12:00".into()),
            ..draft("block")
        };
        let Ok(action) = check(&over, &context) else {
            panic!("not a block")
        };
        assert_eq!(
            action.facts(),
            [
                ("When", "Mon 23 Nov · 10:30 to 12:00".to_owned()),
                ("Clashes", "Screen".to_owned())
            ]
        );
    }

    #[test]
    fn unreadable_drafts_are_refused_with_a_reason() {
        let context = context();
        assert!(
            check(
                &Draft {
                    date: Some("next tuesday".into()),
                    ..draft("task")
                },
                &context
            )
            .unwrap_err()
            .contains("next tuesday")
        );
        assert!(check(&draft("email"), &context).is_err());
        assert!(
            check(
                &Draft {
                    title: " ".into(),
                    ..draft("task")
                },
                &context
            )
            .is_err()
        );
    }

    #[test]
    fn the_model_sees_lists_tasks_events_and_free_time() {
        let described = context().describe();
        assert!(described.contains("Now: Sunday 2026-11-22 16:00 (America/Los_Angeles)"));
        assert!(described.contains("TickTick lists: Inbox, Job search\n"));
        assert!(described.contains("- Mon 2026-11-23 10:00 to 11:00: Screen"));
        assert!(described.contains("- Mon 2026-11-23 08:00 to 10:00 (2 h)"));
    }

    #[test]
    fn a_follow_up_is_due_a_week_later_in_the_job_list() {
        let context = context();
        let Some(action) = follow_up("Engineer at Northwind", &context.now, &context.lists) else {
            panic!()
        };
        assert_eq!(action.title(), "Follow up on Engineer at Northwind");
        assert_eq!(action.facts()[0], ("List", "Job search".to_owned()));
        assert_eq!(action.facts()[1], ("Due", "Sun 29 Nov · 10:00".to_owned()));
        assert_eq!(action.facts()[2], ("Remind", "at the due time".to_owned()));
    }
}
