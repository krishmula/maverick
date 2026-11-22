//! TickTick, the user's to-do app, through its Open API.
//!
//! Maverick signs in with a personal API token that the user makes in TickTick's settings
//! (Account, API Token). The token lives in the Keychain, never in this repository.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use jiff::civil::Date;
use jiff::tz::TimeZone;
use jiff::{Timestamp, ZonedDateTime};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::credentials::{self, Password};
use crate::http;

/// The Keychain item that holds the API token.
pub const TOKEN_SERVICE: &str = "dev.maverick.ticktick";
const TOKEN_ACCOUNT: &str = "api-token";

const API: &str = "https://api.ticktick.com/open/v1";
/// TickTick's date format, such as `2019-11-13T03:00:00+0000`. Replies add milliseconds.
const DATE_FORMAT: &str = "%Y-%m-%dT%H:%M:%S%z";
const REPLY_DATE_FORMAT: &str = "%Y-%m-%dT%H:%M:%S%.f%z";

pub async fn stored_token() -> Result<Option<Password>, String> {
    credentials::secret(TOKEN_SERVICE, TOKEN_ACCOUNT).await
}

pub async fn save_token(token: &Password) -> Result<(), String> {
    credentials::store(TOKEN_SERVICE, TOKEN_ACCOUNT, token).await
}

pub async fn forget_token() -> Result<(), String> {
    credentials::forget(TOKEN_SERVICE, TOKEN_ACCOUNT).await
}

/// What the user set for TickTick under Connections.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct Settings {
    /// The id of the list new tasks go to when the user names none. `None` means the Inbox.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_list: Option<String>,
    /// The ids of the lists Mav leaves out. The Inbox is `inbox`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hidden_lists: Vec<String>,
}

pub fn settings_path() -> Option<PathBuf> {
    crate::data_dir().map(|dir| dir.join("ticktick.json"))
}

impl Settings {
    /// The saved settings, or the defaults when there are none.
    pub fn load(file: &Path) -> Result<Self, String> {
        match std::fs::read_to_string(file) {
            Ok(text) => {
                serde_json::from_str(&text).map_err(|error| format!("{} did not parse: {error}", file.display()))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(format!("could not read {}: {error}", file.display())),
        }
    }

    pub fn save(&self, file: &Path) -> Result<(), String> {
        if let Some(folder) = file.parent() {
            std::fs::create_dir_all(folder)
                .map_err(|error| format!("could not create {}: {error}", folder.display()))?;
        }
        let text = serde_json::to_string_pretty(self).expect("settings always serialize");
        std::fs::write(file, text + "\n").map_err(|error| format!("could not write {}: {error}", file.display()))
    }

    /// True when Mav shows the list with `id`.
    pub fn shows(&self, id: &str) -> bool {
        let id = if id.starts_with("inbox") { "inbox" } else { id };
        !self.hidden_lists.iter().any(|hidden| hidden == id)
    }

    /// Shows or hides the list with `id`.
    pub fn set_shown(&mut self, id: &str, shown: bool) {
        let id = if id.starts_with("inbox") { "inbox" } else { id };
        self.hidden_lists.retain(|hidden| hidden != id);
        if !shown {
            self.hidden_lists.push(id.to_owned());
        }
    }

    /// Reads the saved settings, falling back to the defaults when the file is missing or broken.
    pub fn current() -> Self {
        settings_path()
            .and_then(|file| Self::load(&file).ok())
            .unwrap_or_default()
    }
}

/// How much a task matters, as TickTick ranks it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Priority {
    #[default]
    None,
    Low,
    Medium,
    High,
}

impl Priority {
    pub const ALL: [Priority; 4] = [Priority::High, Priority::Medium, Priority::Low, Priority::None];

    /// TickTick's number: None 0, Low 1, Medium 3, High 5.
    pub fn value(self) -> u8 {
        match self {
            Priority::None => 0,
            Priority::Low => 1,
            Priority::Medium => 3,
            Priority::High => 5,
        }
    }

    pub fn from_value(value: u8) -> Self {
        match value {
            5.. => Priority::High,
            3 | 4 => Priority::Medium,
            1 | 2 => Priority::Low,
            0 => Priority::None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Priority::None => "none",
            Priority::Low => "low",
            Priority::Medium => "medium",
            Priority::High => "high",
        }
    }

    pub fn named(name: &str) -> Option<Self> {
        Priority::ALL
            .into_iter()
            .find(|priority| priority.name().eq_ignore_ascii_case(name.trim()))
    }

    /// The next priority when the user clicks the tag: high, medium, low, none, then high again.
    pub fn next(self) -> Self {
        match self {
            Priority::High => Priority::Medium,
            Priority::Medium => Priority::Low,
            Priority::Low => Priority::None,
            Priority::None => Priority::High,
        }
    }
}

/// A task as TickTick sends it. Only the fields Maverick reads.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Task {
    pub id: String,
    pub project_id: String,
    pub title: String,
    pub start_date: Option<String>,
    pub due_date: Option<String>,
    #[serde(default)]
    pub is_all_day: bool,
    pub time_zone: Option<String>,
    /// None 0, Low 1, Medium 3, High 5.
    #[serde(default)]
    pub priority: u8,
}

/// A list in TickTick. The API calls it a project.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct Project {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub closed: bool,
}

pub async fn projects(token: &Password) -> Result<Vec<Project>, String> {
    let reply = http::call("TickTick", token, "GET", &format!("{API}/project"), None).await?;
    serde_json::from_value(reply).map_err(|error| format!("TickTick's list of lists did not parse: {error}"))
}

/// Every undone task in the lists Mav shows, the Inbox included, with the lists.
/// Unlike `undone`, this has tasks with no date and tasks of any age.
pub async fn open_tasks(token: &Password) -> Result<(Vec<Project>, Vec<Task>), String> {
    let lists = projects(token).await?;
    let settings = Settings::current();
    let mut ids: Vec<String> = vec!["inbox".to_owned()];
    ids.extend(lists.iter().filter(|list| !list.closed).map(|list| list.id.clone()));
    ids.retain(|id| settings.shows(id));
    let mut reads = tokio::task::JoinSet::new();
    for id in ids {
        let token = token.clone();
        reads.spawn(async move {
            let reply = http::call("TickTick", &token, "GET", &format!("{API}/project/{id}/data"), None).await?;
            serde_json::from_value::<Vec<Task>>(reply["tasks"].clone())
                .map_err(|error| format!("TickTick's tasks did not parse: {error}"))
        });
    }
    let mut tasks = Vec::new();
    while let Some(read) = reads.join_next().await {
        tasks.extend(read.map_err(|error| error.to_string())??);
    }
    Ok((lists, tasks))
}

/// A task Maverick is about to add.
#[derive(Clone, Debug, PartialEq)]
pub struct NewTask {
    pub title: String,
    /// The list's id. The Inbox is `inbox`.
    pub list_id: String,
    pub due: Option<Due>,
    /// Minutes before the due time to remind, or for an all-day task, a reminder at 9:00 that day.
    pub remind_before: Option<i64>,
    pub priority: Priority,
}

/// The body TickTick takes to create `task`.
pub fn new_task_body(task: &NewTask, zone: &TimeZone) -> Value {
    let mut body = json!({ "title": task.title, "projectId": task.list_id, "priority": task.priority.value() });
    let format = |moment: Timestamp| moment.strftime(DATE_FORMAT).to_string();
    let zone_name = zone.iana_name().unwrap_or("UTC");
    match &task.due {
        None => {}
        Some(Due::Day(date)) => {
            let midnight = date
                .to_zoned_datetime(zone.clone())
                .map(|at| at.timestamp())
                .unwrap_or_default();
            body["startDate"] = json!(format(midnight));
            body["dueDate"] = json!(format(midnight));
            body["isAllDay"] = json!(true);
            body["timeZone"] = json!(zone_name);
            if task.remind_before.is_some() {
                body["reminders"] = json!(["TRIGGER:P0DT9H0M0S"]);
            }
        }
        Some(Due::At(moment)) => {
            body["startDate"] = json!(format(moment.timestamp()));
            body["dueDate"] = json!(format(moment.timestamp()));
            body["isAllDay"] = json!(false);
            body["timeZone"] = json!(moment.time_zone().iana_name().unwrap_or(zone_name));
            if let Some(minutes) = task.remind_before {
                let trigger = if minutes <= 0 {
                    "TRIGGER:PT0S".to_owned()
                } else {
                    format!("TRIGGER:-PT{minutes}M")
                };
                body["reminders"] = json!([trigger]);
            }
        }
    }
    body
}

/// Adds `task` to TickTick and returns it as TickTick stored it.
pub async fn create(token: &Password, task: &NewTask, zone: &TimeZone) -> Result<Task, String> {
    let reply = http::call(
        "TickTick",
        token,
        "POST",
        &format!("{API}/task"),
        Some(&new_task_body(task, zone)),
    )
    .await?;
    serde_json::from_value(reply).map_err(|error| format!("TickTick's reply to the new task did not parse: {error}"))
}

/// What to change in a task. A field left `None` stays as it is.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Change {
    pub title: Option<String>,
    pub priority: Option<Priority>,
    pub due: Option<Due>,
}

impl Change {
    pub fn is_empty(&self) -> bool {
        self.title.is_none() && self.priority.is_none() && self.due.is_none()
    }
}

/// The body TickTick takes to change task `id` in list `list_id`. TickTick keeps fields it is not sent.
pub fn change_body(id: &str, list_id: &str, change: &Change, zone: &TimeZone) -> Value {
    let mut body = json!({ "id": id, "projectId": list_id });
    if let Some(title) = &change.title {
        body["title"] = json!(title);
    }
    if let Some(priority) = change.priority {
        body["priority"] = json!(priority.value());
    }
    if let Some(due) = &change.due {
        let dated = new_task_body(
            &NewTask {
                title: String::new(),
                list_id: String::new(),
                due: Some(due.clone()),
                remind_before: None,
                priority: Priority::None,
            },
            zone,
        );
        for field in ["startDate", "dueDate", "isAllDay", "timeZone"] {
            body[field] = dated[field].clone();
        }
    }
    body
}

pub async fn change(token: &Password, id: &str, list_id: &str, change: &Change, zone: &TimeZone) -> Result<(), String> {
    let body = change_body(id, list_id, change, zone);
    http::call("TickTick", token, "POST", &format!("{API}/task/{id}"), Some(&body))
        .await
        .map(drop)
}

/// Moves task `id` from one list to another. A plain change cannot move a task.
pub async fn move_to(token: &Password, id: &str, from: &str, to: &str) -> Result<(), String> {
    let body = json!([{ "fromProjectId": from, "toProjectId": to, "taskId": id }]);
    http::call("TickTick", token, "POST", &format!("{API}/task/move"), Some(&body))
        .await
        .map(drop)
}

pub async fn complete(token: &Password, list_id: &str, id: &str) -> Result<(), String> {
    let url = format!("{API}/project/{list_id}/task/{id}/complete");
    http::call("TickTick", token, "POST", &url, None).await.map(drop)
}

pub async fn delete(token: &Password, list_id: &str, task_id: &str) -> Result<(), String> {
    let url = format!("{API}/project/{list_id}/task/{task_id}");
    http::call("TickTick", token, "DELETE", &url, None).await.map(drop)
}

/// When a task is due, as the user reads it.
#[derive(Clone, Debug, PartialEq)]
pub enum Due {
    /// All day on a date.
    Day(Date),
    /// At a moment, in the user's time zone.
    At(ZonedDateTime),
}

impl Due {
    /// Day by day; within a day, all-day tasks first, then by time.
    fn order(&self) -> (Date, Option<Timestamp>) {
        match self {
            Due::Day(date) => (*date, None),
            Due::At(moment) => (moment.date(), Some(moment.timestamp())),
        }
    }

    pub fn date(&self) -> Date {
        match self {
            Due::Day(date) => *date,
            Due::At(moment) => moment.date(),
        }
    }

    /// `Mon 23`, or `Mon 23 · 10:00` for a task with a time.
    pub fn label(&self) -> String {
        match self {
            Due::Day(date) => date.strftime("%a %-d").to_string(),
            Due::At(moment) => moment.strftime("%a %-d · %H:%M").to_string(),
        }
    }

    /// `10:00` for a task with a time, nothing for a task that is due all day.
    pub fn time(&self) -> Option<String> {
        match self {
            Due::Day(_) => None,
            Due::At(moment) => Some(moment.strftime("%H:%M").to_string()),
        }
    }
}

/// One undone task, as the user reads it.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub id: String,
    pub list_id: String,
    pub title: String,
    /// The name of the task's list.
    pub list: String,
    pub due: Option<Due>,
    pub priority: Priority,
}

impl Entry {
    /// When it is due, or `no date`.
    pub fn when(&self) -> String {
        self.due.as_ref().map_or_else(|| "no date".to_owned(), Due::label)
    }
}

/// The tasks as the user reads them: dated ones by date, then undated ones, higher priority first.
pub fn entries(tasks: &[Task], projects: &[Project], zone: &TimeZone) -> Vec<Entry> {
    let names: HashMap<&str, &str> = projects
        .iter()
        .map(|project| (project.id.as_str(), project.name.as_str()))
        .collect();
    let mut entries: Vec<Entry> = tasks
        .iter()
        .map(|task| Entry {
            id: task.id.clone(),
            list_id: task.project_id.clone(),
            title: task.title.clone(),
            list: list_name(&task.project_id, &names),
            due: due(task, zone),
            priority: Priority::from_value(task.priority),
        })
        .collect();
    entries.sort_by(|a, b| {
        let order = |entry: &Entry| (entry.due.is_none(), entry.due.as_ref().map(Due::order));
        order(a).cmp(&order(b)).then(b.priority.cmp(&a.priority))
    });
    entries
}

/// The tasks grouped by priority, each group by date.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Board {
    pub high: Vec<Entry>,
    pub medium: Vec<Entry>,
    pub low: Vec<Entry>,
    pub none: Vec<Entry>,
}

impl Board {
    pub fn of(entries: &[Entry]) -> Self {
        let mut board = Board::default();
        for entry in entries {
            match entry.priority {
                Priority::High => board.high.push(entry.clone()),
                Priority::Medium => board.medium.push(entry.clone()),
                Priority::Low => board.low.push(entry.clone()),
                Priority::None => board.none.push(entry.clone()),
            }
        }
        board
    }

    pub fn groups(&self) -> [(Priority, &[Entry]); 4] {
        [
            (Priority::High, &self.high),
            (Priority::Medium, &self.medium),
            (Priority::Low, &self.low),
            (Priority::None, &self.none),
        ]
    }
}

/// The undone tasks the user should see, sorted by when they are due.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Agenda {
    /// Due before now: an earlier day, or a time today that has passed.
    pub overdue: Vec<Entry>,
    pub today: Vec<Entry>,
    /// Due in the coming days.
    pub upcoming: Vec<Entry>,
}

impl Agenda {
    /// Sorts `tasks` into the agenda as seen at `now`. A task with no date is left out.
    pub fn of(tasks: &[Task], projects: &[Project], now: &ZonedDateTime) -> Self {
        Self::from_entries(&entries(tasks, projects, now.time_zone()), now)
    }

    /// The dated ones of `entries`, already in order, sorted into the agenda.
    pub fn from_entries(entries: &[Entry], now: &ZonedDateTime) -> Self {
        let today = now.date();
        let mut agenda = Agenda::default();
        for entry in entries.iter().filter(|entry| entry.due.is_some()).cloned() {
            let Some(due) = entry.due.clone() else { continue };
            let passed = match &due {
                Due::Day(date) => *date < today,
                Due::At(moment) => moment < now,
            };
            if passed {
                agenda.overdue.push(entry);
            } else if due.date() == today {
                agenda.today.push(entry);
            } else {
                agenda.upcoming.push(entry);
            }
        }
        agenda
    }
}

fn list_name(project_id: &str, names: &HashMap<&str, &str>) -> String {
    match names.get(project_id) {
        Some(name) => (*name).to_owned(),
        // The Inbox is not in the list of lists. Its id is `inbox` and a number.
        None if project_id.starts_with("inbox") => "Inbox".to_owned(),
        None => "Unknown list".to_owned(),
    }
}

/// When `task` is due in `zone`: its due date, or its start date when it has no due date.
fn due(task: &Task, zone: &TimeZone) -> Option<Due> {
    let text = task.due_date.as_deref().or(task.start_date.as_deref())?;
    let moment = jiff::fmt::strtime::parse(REPLY_DATE_FORMAT, text)
        .ok()?
        .to_timestamp()
        .ok()?;
    if task.is_all_day {
        // An all-day task is stored as midnight in the zone it was made in.
        let made_in = task
            .time_zone
            .as_deref()
            .and_then(|name| TimeZone::get(name).ok())
            .unwrap_or_else(|| zone.clone());
        Some(Due::Day(moment.to_zoned_datetime(made_in).date()))
    } else {
        Some(Due::At(moment.to_zoned_datetime(zone.clone())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiff::civil::date;

    fn task(title: &str, due: &str, all_day: bool) -> Task {
        Task {
            id: title.to_owned(),
            project_id: "p1".to_owned(),
            title: title.to_owned(),
            due_date: Some(due.to_owned()),
            is_all_day: all_day,
            time_zone: Some("America/Los_Angeles".to_owned()),
            ..Task::default()
        }
    }

    fn sunday_afternoon() -> ZonedDateTime {
        date(2026, 11, 22).at(16, 0, 0, 0).in_tz("America/Los_Angeles").unwrap()
    }

    fn titles(entries: &[Entry]) -> Vec<&str> {
        entries.iter().map(|entry| entry.title.as_str()).collect()
    }

    #[test]
    fn tasks_are_sorted_into_overdue_today_and_upcoming() {
        let projects = [Project {
            id: "p1".into(),
            name: "Job search".into(),
            closed: false,
        }];
        let tasks = [
            // Midnight Pacific on Fri 20 Nov, all day.
            task("Renew lease", "2026-11-20T08:00:00+0000", true),
            // 07:00 Pacific on Thu 19 Nov: earlier than the all-day task on the 20th.
            task("Pay rent", "2026-11-19T15:00:00+0000", false),
            // 10:00 Pacific today, already past at 16:00.
            task("Call bank", "2026-11-22T18:00:00+0000", false),
            // 18:00 Pacific today.
            task("Send references", "2026-11-23T02:00:00+0000", false),
            task("Water plants", "2026-11-22T08:00:00+0000", true),
            // 10:00 Pacific on Tue 24 Nov, with milliseconds as TickTick's replies have.
            task("Follow up with recruiter", "2026-11-24T18:00:00.000+0000", false),
            Task {
                title: "Someday".into(),
                ..Task::default()
            },
        ];
        let agenda = Agenda::of(&tasks, &projects, &sunday_afternoon());
        assert_eq!(titles(&agenda.overdue), ["Pay rent", "Renew lease", "Call bank"]);
        assert_eq!(titles(&agenda.today), ["Water plants", "Send references"]);
        assert_eq!(titles(&agenda.upcoming), ["Follow up with recruiter"]);
        assert_eq!(agenda.upcoming[0].list, "Job search");
        assert_eq!(agenda.upcoming[0].when(), "Tue 24 · 10:00");
        assert_eq!(agenda.today[0].due.as_ref().and_then(Due::time), None);
    }

    #[test]
    fn an_all_day_task_keeps_the_date_it_was_made_for() {
        // Made in Tokyo for Mon 23 Nov: midnight there is 15:00 UTC on the 22nd.
        let mut tokyo = task("Pack", "2026-11-22T15:00:00+0000", true);
        tokyo.time_zone = Some("Asia/Tokyo".to_owned());
        let agenda = Agenda::of(&[tokyo], &[], &sunday_afternoon());
        assert_eq!(agenda.upcoming[0].due, Some(Due::Day(date(2026, 11, 23))));
    }

    #[test]
    fn the_inbox_is_named_although_it_is_not_a_listed_project() {
        let mut inbox = task("Buy milk", "2026-11-23T18:00:00+0000", false);
        inbox.project_id = "inbox118293".to_owned();
        let agenda = Agenda::of(&[inbox], &[], &sunday_afternoon());
        assert_eq!(agenda.upcoming[0].list, "Inbox");
    }

    #[test]
    fn a_new_task_carries_its_time_zone_and_reminder() {
        let at = date(2026, 11, 24).at(10, 0, 0, 0).in_tz("America/Los_Angeles").unwrap();
        let timed = NewTask {
            title: "Follow up".into(),
            list_id: "inbox".into(),
            due: Some(Due::At(at.clone())),
            remind_before: Some(30),
            priority: Priority::High,
        };
        let zone = at.time_zone().clone();
        let body = new_task_body(&timed, &zone);
        assert_eq!(body["dueDate"], "2026-11-24T18:00:00+0000");
        assert_eq!(body["timeZone"], "America/Los_Angeles");
        assert_eq!(body["isAllDay"], false);
        assert_eq!(body["reminders"], json!(["TRIGGER:-PT30M"]));
        assert_eq!(body["priority"], 5);

        let all_day = NewTask {
            due: Some(Due::Day(date(2026, 11, 24))),
            remind_before: None,
            ..timed
        };
        let body = new_task_body(&all_day, &zone);
        assert_eq!(body["dueDate"], "2026-11-24T08:00:00+0000");
        assert_eq!(body["isAllDay"], true);
        assert!(body.get("reminders").is_none());

        let undated = NewTask { due: None, ..all_day };
        assert!(new_task_body(&undated, &zone).get("dueDate").is_none());
    }

    #[test]
    fn hidden_lists_are_kept_by_id_and_the_inbox_by_name() {
        let mut settings = Settings::default();
        settings.set_shown("inbox118293", false);
        settings.set_shown("p2", false);
        settings.set_shown("p2", false);
        assert_eq!(settings.hidden_lists, ["inbox", "p2"]);
        assert!(!settings.shows("inbox118293") && !settings.shows("p2") && settings.shows("p1"));
        settings.set_shown("p2", true);
        assert!(settings.shows("p2"));

        let scratch = tempfile::tempdir().unwrap();
        let file = scratch.path().join("ticktick.json");
        assert_eq!(Settings::load(&file), Ok(Settings::default()));
        settings.save(&file).unwrap();
        assert_eq!(Settings::load(&file), Ok(settings));
    }

    #[test]
    fn undated_tasks_go_last_and_the_board_groups_by_priority() {
        let mut urgent = task("Pay rent", "2026-11-24T18:00:00+0000", false);
        urgent.priority = 5;
        let mut someday = Task {
            id: "s".into(),
            title: "Read a book".into(),
            priority: 1,
            ..Task::default()
        };
        someday.project_id = "inbox1".into();
        let mut medium = task("Email Sam", "2026-11-23T18:00:00+0000", false);
        medium.priority = 3;
        let entries = entries(
            &[someday, urgent, medium],
            &[],
            &TimeZone::get("America/Los_Angeles").unwrap(),
        );
        assert_eq!(titles(&entries), ["Email Sam", "Pay rent", "Read a book"]);
        assert_eq!(entries[2].when(), "no date");
        let board = Board::of(&entries);
        assert_eq!(titles(&board.high), ["Pay rent"]);
        assert_eq!(titles(&board.medium), ["Email Sam"]);
        assert_eq!(titles(&board.low), ["Read a book"]);
        // The agenda leaves an undated task out.
        assert_eq!(Agenda::from_entries(&entries, &sunday_afternoon()).upcoming.len(), 2);
    }

    #[test]
    fn priorities_round_trip_and_cycle() {
        for priority in Priority::ALL {
            assert_eq!(Priority::from_value(priority.value()), priority);
            assert_eq!(Priority::named(priority.name()), Some(priority));
        }
        assert_eq!(Priority::High.next(), Priority::Medium);
        assert_eq!(Priority::None.next(), Priority::High);
    }

    #[test]
    fn a_change_sends_only_what_changes() {
        let zone = TimeZone::get("America/Los_Angeles").unwrap();
        let raise = Change {
            priority: Some(Priority::High),
            ..Change::default()
        };
        assert_eq!(
            change_body("t1", "p1", &raise, &zone),
            json!({ "id": "t1", "projectId": "p1", "priority": 5 })
        );
        let move_day = Change {
            due: Some(Due::Day(date(2026, 11, 26))),
            title: Some("Quiz prep".into()),
            ..Change::default()
        };
        let body = change_body("t1", "p1", &move_day, &zone);
        assert_eq!(body["dueDate"], "2026-11-26T08:00:00+0000");
        assert_eq!(body["isAllDay"], true);
        assert_eq!(body["title"], "Quiz prep");
        assert!(body.get("priority").is_none());
    }

    #[test]
    fn tasks_parse_from_ticktick_json() {
        let reply = json!([{
            "id": "63b7", "projectId": "6226", "title": "Task Title", "isAllDay": true,
            "timeZone": "America/Los_Angeles", "dueDate": "2019-11-14T03:00:00+0000",
            "reminders": ["TRIGGER:PT0S"], "priority": 1, "status": 0, "items": []
        }]);
        let tasks: Vec<Task> = serde_json::from_value(reply).unwrap();
        assert_eq!(tasks[0].title, "Task Title");
        assert!(tasks[0].is_all_day);
    }
}
