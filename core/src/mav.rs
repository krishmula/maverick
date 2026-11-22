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
use crate::google;
use crate::providers::{self, Job};
use crate::ticktick::{self, Agenda, Due, Entry, NewTask, Priority, Project};

const INSTRUCTIONS: &str = "You are Mav, the user's planning assistant. The user tells you what to remember, \
what to do, or what time to set aside. You see their TickTick lists and tasks, their calendar, and their free time. \
Propose a TickTick task (kind task) for anything to remember or do, and a calendar block (kind block) for time set aside to work on something. \
Propose only what the user asked for. Never say anything is done: the user confirms each proposal first. \
Dates and times are in the user's time zone; resolve words like tomorrow or Tuesday from the current time. \
A task with no time the user named gets a date only, or no date if none was named. \
remind_minutes_before is 0 when the user asks to be reminded at the time, null when they did not ask for a reminder. \
A block needs a date, a time and an end_time. When the user asks you to find time, choose from the free time listed, \
the earliest stretch that fits, and never one that overlaps an event. \
list is the exact name of one of the user's lists, or null for their default list. \
calendar is the exact name of one of the user's Google profiles when they say which calendar a block goes on, such as work or school, otherwise null. \
To finish, change or delete one of the user's existing tasks, use kind complete, change or delete, set task to its ref such as t3, and title to its current title. \
For change, set only what changes: date and time for a new due date, priority, rename for a new title, and list to move it to another list; leave the rest null. \
priority is high, medium, low or none. For a new task, set it only when the user names one. \
To move or delete one of the user's calendar events, use kind move_event or delete_event, set event to its ref such as e2, and title to its title. \
For move_event, set date and time for the new start, and end_time only when the user gives one; otherwise it keeps its length. \
For a question about their schedule or tasks, answer from what you see, with no proposals. \
In reply, say in one or two short plain sentences what you propose or what you found. No em dashes. \
Never invent tasks or events.";

/// What Mav sees when it answers: the time, the lists, the tasks and the calendar.
#[derive(Clone, Debug)]
pub struct Context {
    pub now: ZonedDateTime,
    pub lists: Vec<Project>,
    pub agenda: Agenda,
    /// Every undone task in the lists Mav shows, dated ones first. Mav refers to them as t1, t2, …
    pub tasks: Vec<Entry>,
    /// The calendar from the start of today through the coming week.
    pub events: Vec<Event>,
    /// The names of the user's Google profiles.
    pub profiles: Vec<String>,
    pub ticktick: ticktick::Settings,
}

impl Context {
    /// Reads TickTick and the calendar. A service that is not connected contributes nothing.
    pub async fn read(now: ZonedDateTime) -> Result<Context, String> {
        let start = now.start_of_day().map_err(|error| error.to_string())?;
        let end = start.checked_add(8.days()).map_err(|error| error.to_string())?;
        let ticktick = async {
            match ticktick::stored_token().await? {
                Some(token) => {
                    let (lists, tasks) = ticktick::open_tasks(&token).await?;
                    let tasks = ticktick::entries(&tasks, &lists, now.time_zone());
                    Ok::<_, String>((lists, tasks))
                }
                None => Ok((Vec::new(), Vec::new())),
            }
        };
        let events = calendar::events(start.timestamp(), end.timestamp(), now.time_zone());
        let ((lists, tasks), events) = tokio::try_join!(ticktick, events)?;
        let agenda = Agenda::from_entries(&tasks, &now);
        let profiles = match google::accounts_path() {
            Some(file) => google::accounts(&file)?
                .iter()
                .map(|account| account.name().to_owned())
                .collect(),
            None => Vec::new(),
        };
        Ok(Context {
            now,
            lists,
            agenda,
            tasks,
            events,
            profiles,
            ticktick: ticktick::Settings::current(),
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
        if !self.profiles.is_empty() {
            let _ = write!(text, "\nGoogle profiles: {}", self.profiles.join(", "));
        }
        text.push_str("\n\nUndone tasks (ref: title, due, list, priority):\n");
        for (index, entry) in self.tasks.iter().enumerate() {
            let overdue = self.agenda.overdue.iter().any(|late| late.id == entry.id);
            let _ = writeln!(
                text,
                "- t{}: {} ({}, {}, priority {}{})",
                index + 1,
                entry.title,
                entry.when(),
                entry.list,
                entry.priority.name(),
                if overdue { ", overdue" } else { "" }
            );
        }
        text.push_str("\nCalendar (ref: when, title, profile):\n");
        for (index, event) in self.events.iter().enumerate() {
            let day = match &event.span {
                calendar::Span::Days(first, _) => first.strftime("%a %Y-%m-%d").to_string(),
                calendar::Span::At(start, _) => start.strftime("%a %Y-%m-%d").to_string(),
            };
            let free = if event.busy { "" } else { ", marked free" };
            let _ = writeln!(
                text,
                "- e{}: {day} {}: {} [{}]{free}",
                index + 1,
                event.hours(),
                event.title,
                event.profile
            );
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
    /// The Google profile a block goes on.
    pub calendar: Option<String>,
    /// The ref of an existing task, such as `t3`, for complete, change and delete.
    pub task: Option<String>,
    pub priority: Option<String>,
    /// A new title for a task being changed.
    pub rename: Option<String>,
    /// The ref of a calendar event, such as `e2`, for move_event and delete_event.
    pub event: Option<String>,
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
        /// The Google profile it goes to. `None` means the one marked for new blocks.
        profile: Option<String>,
    },
    Complete {
        entry: Entry,
    },
    Change {
        entry: Entry,
        change: ticktick::Change,
        /// The id and name of the list it moves to.
        to: Option<(String, String)>,
    },
    Delete {
        entry: Entry,
    },
    MoveEvent {
        event: Event,
        start: ZonedDateTime,
        end: ZonedDateTime,
        clashes: Vec<String>,
    },
    DeleteEvent {
        event: Event,
    },
}

impl Action {
    pub fn title(&self) -> &str {
        match self {
            Action::Task { task, .. } => &task.title,
            Action::Block { title, .. } => title,
            Action::Complete { entry } | Action::Change { entry, .. } | Action::Delete { entry } => &entry.title,
            Action::MoveEvent { event, .. } | Action::DeleteEvent { event } => &event.title,
        }
    }

    /// The label on the proposal card.
    pub fn kind(&self) -> &'static str {
        match self {
            Action::Task { .. } => "TickTick · New task",
            Action::Block { .. } => "Calendar · New block",
            Action::Complete { .. } => "TickTick · Finish",
            Action::Change { .. } => "TickTick · Change",
            Action::Delete { .. } => "TickTick · Delete",
            Action::MoveEvent { .. } => "Calendar · Move",
            Action::DeleteEvent { .. } => "Calendar · Delete",
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
                        Some(due) => due_label(due),
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
                if task.priority != Priority::None {
                    facts.push(("Priority", task.priority.name().to_owned()));
                }
                facts
            }
            Action::MoveEvent {
                event,
                start,
                end,
                clashes,
            } => {
                let mut facts = vec![
                    (
                        "When",
                        format!(
                            "{} → {} to {}",
                            event_when(event),
                            start.strftime("%a %-d %b · %H:%M"),
                            end.strftime("%H:%M")
                        ),
                    ),
                    ("Calendar", event.profile.clone()),
                ];
                if !clashes.is_empty() {
                    facts.push(("Clashes", clashes.join(", ")));
                }
                facts
            }
            Action::DeleteEvent { event } => vec![("When", event_when(event)), ("Calendar", event.profile.clone())],
            Action::Complete { entry } | Action::Delete { entry } => {
                vec![("List", entry.list.clone()), ("Due", entry.when())]
            }
            Action::Change { entry, change, to } => {
                let mut facts = Vec::new();
                if let Some(title) = &change.title {
                    facts.push(("Title", format!("{} → {title}", entry.title)));
                }
                if let Some(priority) = change.priority {
                    facts.push(("Priority", format!("{} → {}", entry.priority.name(), priority.name())));
                }
                if let Some(due) = &change.due {
                    facts.push(("Due", format!("{} → {}", entry.when(), due_label(due))));
                }
                if let Some((_, name)) = to {
                    facts.push(("List", format!("{} → {name}", entry.list)));
                }
                facts
            }
            Action::Block {
                start,
                end,
                clashes,
                profile,
                ..
            } => {
                let mut facts = vec![(
                    "When",
                    format!("{} to {}", start.strftime("%a %-d %b · %H:%M"), end.strftime("%H:%M")),
                )];
                facts.push(("Calendar", profile.clone().unwrap_or_else(|| "default".to_owned())));
                if !clashes.is_empty() {
                    facts.push(("Clashes", clashes.join(", ")));
                }
                facts
            }
        }
    }

    /// Writes the action to TickTick or the calendar. Returns what to tell the user.
    pub async fn carry_out(&self) -> Result<String, String> {
        let token = || async {
            ticktick::stored_token()
                .await?
                .ok_or_else(|| "TickTick is not connected; connect it under Connections".to_owned())
        };
        match self {
            Action::MoveEvent { event, start, end, .. } => {
                calendar::reschedule(event, start, end).await?;
                Ok(format!("Moved in the {} calendar", event.profile))
            }
            Action::DeleteEvent { event } => {
                calendar::delete(event).await?;
                Ok(format!("Deleted from the {} calendar", event.profile))
            }
            Action::Complete { entry } => {
                ticktick::complete(&token().await?, &entry.list_id, &entry.id).await?;
                Ok("Finished in TickTick".to_owned())
            }
            Action::Delete { entry } => {
                ticktick::delete(&token().await?, &entry.list_id, &entry.id).await?;
                Ok("Deleted from TickTick".to_owned())
            }
            Action::Change { entry, change, to } => {
                let token = token().await?;
                if !change.is_empty() {
                    ticktick::change(&token, &entry.id, &entry.list_id, change, &TimeZone::system()).await?;
                }
                if let Some((list_id, _)) = to {
                    ticktick::move_to(&token, &entry.id, &entry.list_id, list_id).await?;
                }
                Ok("Changed in TickTick".to_owned())
            }
            Action::Task { task, list } => {
                let token = token().await?;
                let zone = match &task.due {
                    Some(Due::At(at)) => at.time_zone().clone(),
                    _ => TimeZone::system(),
                };
                ticktick::create(&token, task, &zone).await?;
                Ok(format!("Added to {list} in TickTick"))
            }
            Action::Block {
                title,
                start,
                end,
                profile,
                ..
            } => {
                let account = calendar::add(title, start, end, profile.as_deref()).await?;
                Ok(format!("Added to the {account} calendar"))
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
    let answer = &providers::run(Job::Mav, INSTRUCTIONS, request, &schema()).await?;
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
                        "kind": { "type": "string", "enum": ["task", "block", "complete", "change", "delete", "move_event", "delete_event"] },
                        "title": { "type": "string" },
                        "list": maybe("string"),
                        "date": { "type": ["string", "null"], "description": "YYYY-MM-DD" },
                        "time": { "type": ["string", "null"], "description": "HH:MM, 24-hour" },
                        "end_time": { "type": ["string", "null"], "description": "HH:MM, 24-hour, blocks only" },
                        "remind_minutes_before": maybe("integer"),
                        "calendar": maybe("string"),
                        "task": { "type": ["string", "null"], "description": "an existing task's ref, such as t3" },
                        "priority": { "type": ["string", "null"], "enum": ["high", "medium", "low", "none", null] },
                        "rename": maybe("string"),
                        "event": { "type": ["string", "null"], "description": "a calendar event's ref, such as e2" },
                    },
                    "required": ["kind", "title", "list", "date", "time", "end_time", "remind_minutes_before", "calendar", "task", "priority", "rename", "event"],
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
        "complete" | "change" | "delete" => existing(draft.kind.as_str(), draft, context, date, start, &at),
        "move_event" | "delete_event" => {
            let named = draft.event.as_deref().unwrap_or("none");
            let event = named
                .trim()
                .trim_start_matches('e')
                .parse::<usize>()
                .ok()
                .and_then(|number| context.events.get(number.checked_sub(1)?))
                .cloned()
                .ok_or_else(|| format!("Mav pointed at an event Maverick cannot find: {named}"))?;
            if draft.kind == "delete_event" {
                return Ok(Action::DeleteEvent { event });
            }
            let calendar::Span::At(was_start, was_end) = &event.span else {
                return Err(format!(
                    "{} is an all-day event, which Maverick does not move",
                    event.title
                ));
            };
            let (Some(date), Some(time)) = (date, start) else {
                return Err(format!(
                    "Mav proposed moving {} without a new date and time",
                    event.title
                ));
            };
            let start = at(date, time)?;
            let end = match end {
                Some(end) => at(date, end)?,
                None => start
                    .checked_add(was_end.duration_since(was_start))
                    .map_err(|error| error.to_string())?,
            };
            if end <= start {
                return Err(format!("Mav proposed moving {} to end before it starts", event.title));
            }
            let clashes = calendar::clashes(&context.events, &start, &end)
                .into_iter()
                .filter(|other| other.source != event.source)
                .map(|other| other.title.clone())
                .collect();
            Ok(Action::MoveEvent {
                event,
                start,
                end,
                clashes,
            })
        }
        "task" if title.is_empty() => Err("Mav proposed a task with no title".to_owned()),
        "block" if title.is_empty() => Err("Mav proposed a block with no title".to_owned()),
        "task" => {
            let default = context
                .ticktick
                .default_list
                .as_deref()
                .and_then(|id| context.lists.iter().find(|list| list.id == id))
                .map(|list| list.name.as_str());
            let (list_id, list) = list_named(draft.list.as_deref().or(default), &context.lists);
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
                priority: priority_of(draft)?.unwrap_or_default(),
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
            let profile = match draft.calendar.as_deref() {
                None => None,
                Some(name) => Some(
                    context
                        .profiles
                        .iter()
                        .find(|profile| profile.eq_ignore_ascii_case(name.trim()))
                        .cloned()
                        .ok_or_else(|| format!("Mav named a calendar Maverick does not have: {name}"))?,
                ),
            };
            Ok(Action::Block {
                title: title.to_owned(),
                start,
                end,
                clashes,
                profile,
            })
        }
        other => Err(format!("Mav proposed a kind of thing Maverick cannot do: {other}")),
    }
}

/// When an event is, as `Mon 23 Nov · 10:00`, or the first day of an all-day one.
fn event_when(event: &Event) -> String {
    match &event.span {
        calendar::Span::Days(first, _) => first.strftime("%a %-d %b").to_string(),
        calendar::Span::At(start, _) => start.strftime("%a %-d %b · %H:%M").to_string(),
    }
}

/// `Thu 26 Nov · 18:00`, or `Thu 26 Nov` for a whole day.
fn due_label(due: &Due) -> String {
    match due {
        Due::Day(date) => date.strftime("%a %-d %b").to_string(),
        Due::At(at) => at.strftime("%a %-d %b · %H:%M").to_string(),
    }
}

fn priority_of(draft: &Draft) -> Result<Option<Priority>, String> {
    draft
        .priority
        .as_deref()
        .map(|name| Priority::named(name).ok_or_else(|| format!("Mav gave a priority Maverick does not know: {name}")))
        .transpose()
}

/// Checks a draft that finishes, changes or deletes one of the user's tasks.
fn existing(
    kind: &str,
    draft: &Draft,
    context: &Context,
    date: Option<Date>,
    time: Option<Time>,
    at: &dyn Fn(Date, Time) -> Result<ZonedDateTime, String>,
) -> Result<Action, String> {
    let named = draft.task.as_deref().unwrap_or("none");
    let entry = named
        .trim()
        .trim_start_matches('t')
        .parse::<usize>()
        .ok()
        .and_then(|number| context.tasks.get(number.checked_sub(1)?))
        .cloned()
        .ok_or_else(|| format!("Mav pointed at a task Maverick cannot find: {named}"))?;
    match kind {
        "complete" => Ok(Action::Complete { entry }),
        "delete" => Ok(Action::Delete { entry }),
        _ => {
            let due = match (date, time) {
                (Some(date), Some(time)) => Some(Due::At(at(date, time)?)),
                (Some(date), None) => Some(Due::Day(date)),
                (None, Some(time)) => {
                    let day = entry.due.as_ref().map_or(context.now.date(), Due::date);
                    Some(Due::At(at(day, time)?))
                }
                (None, None) => None,
            };
            let priority = priority_of(draft)?.filter(|priority| *priority != entry.priority);
            let title = draft
                .rename
                .as_deref()
                .map(str::trim)
                .filter(|title| !title.is_empty() && *title != entry.title)
                .map(str::to_owned);
            let to = match draft.list.as_deref() {
                None => None,
                Some(name) => {
                    let (id, list) = list_named(Some(name), &context.lists);
                    if list == "Inbox" && !name.trim().eq_ignore_ascii_case("inbox") {
                        return Err(format!("Mav named a list Maverick does not have: {name}"));
                    }
                    let id = if id == "inbox" { inbox_id(context) } else { id };
                    (id != entry.list_id).then_some((id, list))
                }
            };
            let change = ticktick::Change { title, priority, due };
            if change.is_empty() && to.is_none() {
                return Err(format!("Mav proposed a change to {} that changes nothing", entry.title));
            }
            Ok(Action::Change { entry, change, to })
        }
    }
}

/// The Inbox's real id, which carries a number after `inbox`, taken from a task in it.
fn inbox_id(context: &Context) -> String {
    context
        .tasks
        .iter()
        .map(|task| &task.list_id)
        .find(|id| id.starts_with("inbox"))
        .cloned()
        .unwrap_or_else(|| "inbox".to_owned())
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
            priority: Priority::Medium,
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
            tasks: vec![
                Entry {
                    id: "a1".into(),
                    list_id: "inbox42".into(),
                    title: "Study for the quiz".into(),
                    list: "Inbox".into(),
                    due: Some(Due::Day(date(2026, 11, 23))),
                    priority: Priority::Low,
                },
                Entry {
                    id: "a2".into(),
                    list_id: "p1".into(),
                    title: "Renew lease".into(),
                    list: "Job search".into(),
                    due: None,
                    priority: Priority::None,
                },
            ],
            events: vec![Event {
                title: "Screen".into(),
                profile: "Work".into(),
                source: calendar::Source {
                    account: "w@example.com".into(),
                    calendar: "primary".into(),
                    id: "s1".into(),
                },
                span: Span::At(at(10), at(11)),
                busy: true,
            }],
            profiles: vec!["Work".into(), "School".into()],
            ticktick: ticktick::Settings {
                default_list: Some("p1".into()),
                hidden_lists: Vec::new(),
            },
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
                ("Calendar", "default".to_owned()),
                ("Clashes", "Screen".to_owned())
            ]
        );
        let school = Draft {
            calendar: Some("school".into()),
            ..over.clone()
        };
        let Ok(Action::Block { profile, .. }) = check(&school, &context) else {
            panic!("not a block")
        };
        assert_eq!(profile.as_deref(), Some("School"));
        let gym = Draft {
            calendar: Some("Gym".into()),
            ..over
        };
        assert!(check(&gym, &context).unwrap_err().contains("Gym"));
    }

    #[test]
    fn existing_tasks_are_finished_changed_and_moved_by_ref() {
        let context = context();
        let Ok(Action::Complete { entry }) = check(
            &Draft {
                task: Some("t2".into()),
                ..draft("complete")
            },
            &context,
        ) else {
            panic!("not a finish")
        };
        assert_eq!(entry.title, "Renew lease");

        let raise = Draft {
            task: Some("t1".into()),
            priority: Some("high".into()),
            date: Some("2026-11-26".into()),
            time: Some("18:00".into()),
            ..draft("change")
        };
        let Ok(action) = check(&raise, &context) else {
            panic!("not a change")
        };
        assert_eq!(
            action.facts(),
            [
                ("Priority", "low → high".to_owned()),
                ("Due", "Mon 23 → Thu 26 Nov · 18:00".to_owned())
            ]
        );

        let move_it = Draft {
            task: Some("t2".into()),
            list: Some("inbox".into()),
            ..draft("change")
        };
        let Ok(Action::Change { to, .. }) = check(&move_it, &context) else {
            panic!("not a move")
        };
        assert_eq!(to, Some(("inbox42".to_owned(), "Inbox".to_owned())));

        let same = Draft {
            task: Some("t1".into()),
            priority: Some("low".into()),
            ..draft("change")
        };
        assert!(check(&same, &context).unwrap_err().contains("changes nothing"));
        assert!(
            check(
                &Draft {
                    task: Some("t9".into()),
                    ..draft("delete")
                },
                &context
            )
            .unwrap_err()
            .contains("t9")
        );
        let nowhere = Draft {
            task: Some("t1".into()),
            list: Some("Gym".into()),
            ..draft("change")
        };
        assert!(check(&nowhere, &context).unwrap_err().contains("Gym"));
    }

    #[test]
    fn events_are_moved_keeping_their_length_or_deleted_by_ref() {
        let context = context();
        let later = Draft {
            event: Some("e1".into()),
            date: Some("2026-11-23".into()),
            time: Some("15:00".into()),
            ..draft("move_event")
        };
        let Ok(action) = check(&later, &context) else {
            panic!("not a move")
        };
        assert_eq!(
            action.facts(),
            [
                ("When", "Mon 23 Nov · 10:00 → Mon 23 Nov · 15:00 to 16:00".to_owned()),
                ("Calendar", "Work".to_owned())
            ]
        );
        assert!(matches!(
            check(
                &Draft {
                    event: Some("e1".into()),
                    ..draft("delete_event")
                },
                &context
            ),
            Ok(Action::DeleteEvent { .. })
        ));
        assert!(
            check(
                &Draft {
                    event: Some("e7".into()),
                    ..draft("delete_event")
                },
                &context
            )
            .unwrap_err()
            .contains("e7")
        );
        assert!(
            check(
                &Draft {
                    event: Some("e1".into()),
                    ..draft("move_event")
                },
                &context
            )
            .is_err()
        );
    }

    #[test]
    fn a_task_with_no_list_goes_to_the_default_list() {
        let Ok(Action::Task { task, list }) = check(&draft("task"), &context()) else {
            panic!("not a task")
        };
        assert_eq!((task.list_id.as_str(), list.as_str()), ("p1", "Job search"));
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
    fn the_answer_format_allows_every_kind_check_understands() {
        let kinds = schema()["properties"]["proposals"]["items"]["properties"]["kind"]["enum"].clone();
        for kind in [
            "task",
            "block",
            "complete",
            "change",
            "delete",
            "move_event",
            "delete_event",
        ] {
            assert!(
                kinds.as_array().unwrap().iter().any(|allowed| allowed == kind),
                "{kind} is not allowed"
            );
        }
    }

    #[test]
    fn the_model_sees_lists_tasks_events_and_free_time() {
        let described = context().describe();
        assert!(described.contains("Now: Sunday 2026-11-22 16:00 (America/Los_Angeles)"));
        assert!(described.contains("TickTick lists: Inbox, Job search\n"));
        assert!(described.contains("- e1: Mon 2026-11-23 10:00 to 11:00: Screen [Work]"));
        assert!(described.contains("Google profiles: Work, School"));
        assert!(described.contains("- t1: Study for the quiz (Mon 23, Inbox, priority low)"));
        assert!(described.contains("- t2: Renew lease (no date, Job search, priority none)"));
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
