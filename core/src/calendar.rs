//! Google Calendar: the user's events, the free time between them, and new events.
//!
//! It reads the calendars the user chose in each Google profile (the primary one unless they
//! chose others), and writes to the profile the user names, or to the one marked for new blocks.

use jiff::civil::{Date, Time};
use jiff::tz::TimeZone;
use jiff::{SignedDuration, Timestamp, ZonedDateTime};
use serde_json::{Value, json};

use crate::google::{self, Account, CALENDAR_EVENTS, Client};
use crate::http::form_encoded;

const CALENDARS: &str = "https://www.googleapis.com/calendar/v3/calendars";
const CALENDAR_LIST: &str = "https://www.googleapis.com/calendar/v3/users/me/calendarList";
/// The hours the user plans in, when looking for free time.
pub const DAY_STARTS: i8 = 8;
pub const DAY_ENDS: i8 = 22;

/// When an event happens.
#[derive(Clone, Debug, PartialEq)]
pub enum Span {
    /// All day, from the first date up to but not including the last, as Google stores it.
    Days(Date, Date),
    At(ZonedDateTime, ZonedDateTime),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Event {
    pub title: String,
    /// The name of the Google profile it came from.
    pub profile: String,
    pub span: Span,
    /// False for an event marked free, which does not block time.
    pub busy: bool,
    /// Where it lives in Google, so it can be moved or deleted.
    pub source: Source,
}

/// Where an event lives: the account, the calendar in it, and the event's id.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Source {
    pub account: String,
    pub calendar: String,
    pub id: String,
}

impl Event {
    /// The timed part of the event that falls on `day`, if it is a timed event that does.
    fn on(&self, day: Date) -> Option<(ZonedDateTime, ZonedDateTime)> {
        match &self.span {
            Span::At(start, end) if start.date() <= day && end.date() >= day => Some((start.clone(), end.clone())),
            _ => None,
        }
    }

    pub fn touches(&self, day: Date) -> bool {
        match &self.span {
            Span::Days(first, after) => *first <= day && day < *after,
            Span::At(start, end) => start.date() <= day && (end.date() > day || (end.date() == day && start != end)),
        }
    }

    /// `10:00 to 11:30`, or `all day`.
    pub fn hours(&self) -> String {
        match &self.span {
            Span::Days(..) => "all day".to_owned(),
            Span::At(start, end) => format!("{} to {}", start.strftime("%H:%M"), end.strftime("%H:%M")),
        }
    }
}

/// A stretch of free time.
#[derive(Clone, Debug, PartialEq)]
pub struct Slot {
    pub start: ZonedDateTime,
    pub end: ZonedDateTime,
}

impl Slot {
    pub fn minutes(&self) -> i64 {
        self.end.duration_since(&self.start).as_mins()
    }
}

/// The free time on `day` between the planning hours, after `now`, around the busy events.
pub fn free_slots(events: &[Event], day: Date, now: &ZonedDateTime) -> Vec<Slot> {
    let zone = now.time_zone().clone();
    let at = |hour: i8| {
        day.to_datetime(Time::constant(hour, 0, 0, 0))
            .to_zoned_datetime(zone.clone())
            .ok()
    };
    let (Some(opens), Some(closes)) = (at(DAY_STARTS), at(DAY_ENDS)) else {
        return Vec::new();
    };
    let mut busy: Vec<(ZonedDateTime, ZonedDateTime)> = events
        .iter()
        .filter(|event| event.busy)
        .filter_map(|event| event.on(day))
        .collect();
    busy.sort_by(|a, b| a.0.cmp(&b.0));
    let mut cursor = if *now > opens { now.clone() } else { opens };
    let mut slots = Vec::new();
    for (start, end) in busy {
        if start > cursor {
            slots.push(Slot {
                start: cursor.clone(),
                end: start.clone().min(closes.clone()),
            });
        }
        if end > cursor {
            cursor = end;
        }
    }
    if closes > cursor {
        slots.push(Slot {
            start: cursor,
            end: closes,
        });
    }
    slots.retain(|slot| slot.minutes() >= 15);
    slots
}

/// The busy events that overlap `start` to `end`.
pub fn clashes<'a>(events: &'a [Event], start: &ZonedDateTime, end: &ZonedDateTime) -> Vec<&'a Event> {
    events
        .iter()
        .filter(|event| event.busy)
        .filter(|event| match &event.span {
            Span::At(from, to) => from < end && to > start,
            Span::Days(..) => false,
        })
        .collect()
}

/// The events between `from` and `to` on the calendars each profile reads, by start.
pub async fn events(from: Timestamp, to: Timestamp, zone: &TimeZone) -> Result<Vec<Event>, String> {
    let Some(client) = Client::stored().await? else {
        return Ok(Vec::new());
    };
    let file = google::accounts_path().ok_or("this system has no application data directory")?;
    let mut all = Vec::new();
    for account in google::accounts(&file)?.iter().filter(|account| can_read(account)) {
        let token = google::access_token(&client, &account.email).await?;
        let primary = ["primary".to_owned()];
        let calendars = if account.calendars.is_empty() {
            &primary[..]
        } else {
            &account.calendars[..]
        };
        for calendar in calendars {
            let url = format!(
                "{CALENDARS}/{}/events?singleEvents=true&orderBy=startTime&maxResults=250&timeMin={}&timeMax={}",
                form_encoded(calendar),
                from.strftime("%Y-%m-%dT%H:%M:%SZ"),
                to.strftime("%Y-%m-%dT%H:%M:%SZ")
            );
            let reply = google::call(&token, "GET", &url, None).await?;
            let source = Source {
                account: account.email.clone(),
                calendar: calendar.clone(),
                id: String::new(),
            };
            all.extend(parse_events(&reply, zone, account.name(), &source));
        }
    }
    all.sort_by_key(|event| match &event.span {
        Span::Days(first, _) => (*first, None),
        Span::At(start, _) => (start.date(), Some(start.timestamp())),
    });
    Ok(all)
}

/// One calendar in a Google account.
#[derive(Clone, Debug, PartialEq)]
pub struct Calendar {
    pub id: String,
    pub name: String,
    pub primary: bool,
}

/// The calendars the account with `email` can see.
pub async fn calendars(email: &str) -> Result<Vec<Calendar>, String> {
    let client = Client::stored().await?.ok_or("no Google OAuth client is stored")?;
    let token = google::access_token(&client, email).await?;
    let reply = google::call(&token, "GET", CALENDAR_LIST, None).await?;
    Ok(parse_calendars(&reply))
}

pub fn parse_calendars(reply: &Value) -> Vec<Calendar> {
    let mut calendars: Vec<Calendar> = reply["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| {
            Some(Calendar {
                id: item["id"].as_str()?.to_owned(),
                name: item["summaryOverride"]
                    .as_str()
                    .or(item["summary"].as_str())
                    .unwrap_or("(no name)")
                    .to_owned(),
                primary: item["primary"] == true,
            })
        })
        .collect();
    calendars.sort_by_key(|calendar| !calendar.primary);
    calendars
}

/// The events in one reply from Google. A cancelled event or one Maverick cannot read is left out.
pub fn parse_events(reply: &Value, zone: &TimeZone, profile: &str, from: &Source) -> Vec<Event> {
    let Some(items) = reply["items"].as_array() else {
        return Vec::new();
    };
    items
        .iter()
        .filter(|item| item["status"] != "cancelled")
        .filter_map(|item| {
            let span = match (item["start"]["dateTime"].as_str(), item["start"]["date"].as_str()) {
                (Some(start), _) => {
                    let end = item["end"]["dateTime"].as_str()?;
                    let moment = |text: &str| {
                        text.parse::<Timestamp>()
                            .ok()
                            .map(|at| at.to_zoned_datetime(zone.clone()))
                    };
                    Span::At(moment(start)?, moment(end)?)
                }
                (None, Some(first)) => Span::Days(first.parse().ok()?, item["end"]["date"].as_str()?.parse().ok()?),
                (None, None) => return None,
            };
            Some(Event {
                title: item["summary"].as_str().unwrap_or("(no title)").to_owned(),
                profile: profile.to_owned(),
                span,
                busy: item["transparency"] != "transparent",
                source: Source {
                    id: item["id"].as_str().unwrap_or_default().to_owned(),
                    ..from.clone()
                },
            })
        })
        .collect()
}

fn can_read(account: &Account) -> bool {
    account
        .scopes
        .iter()
        .any(|scope| scope == google::CALENDAR_READ || scope == CALENDAR_EVENTS)
}

/// The account new blocks go to: the profile named `profile`, ignoring case, or else the one
/// marked for new blocks, or else the first that may add events.
pub fn block_account<'a>(accounts: &'a [Account], profile: Option<&str>) -> Result<&'a Account, String> {
    let chosen = match profile {
        Some(name) => accounts
            .iter()
            .find(|account| account.name().eq_ignore_ascii_case(name.trim()))
            .ok_or_else(|| format!("no Google profile is called {name}"))?,
        None => accounts
            .iter()
            .find(|account| account.adds_blocks)
            .or_else(|| accounts.iter().find(|account| account.can(CALENDAR_EVENTS)))
            .ok_or("no Google profile lets Maverick add events yet; sign in to one again under Connections")?,
    };
    if !chosen.can(CALENDAR_EVENTS) {
        return Err(format!(
            "the {} profile does not let Maverick add events yet; sign in to it again under Connections",
            chosen.name()
        ));
    }
    Ok(chosen)
}

/// Adds an event from `start` to `end` to the primary calendar of the profile `block_account` picks.
/// Returns the profile's name.
pub async fn add(
    title: &str,
    start: &ZonedDateTime,
    end: &ZonedDateTime,
    profile: Option<&str>,
) -> Result<String, String> {
    let client = Client::stored()
        .await?
        .ok_or("no Google OAuth client is stored; run `maverick google client` first")?;
    let file = google::accounts_path().ok_or("this system has no application data directory")?;
    let accounts = google::accounts(&file)?;
    let account = block_account(&accounts, profile)?;
    let token = google::access_token(&client, &account.email).await?;
    let moment = |at: &ZonedDateTime| {
        json!({
            "dateTime": at.timestamp().strftime("%Y-%m-%dT%H:%M:%SZ").to_string(),
            "timeZone": at.time_zone().iana_name().unwrap_or("UTC"),
        })
    };
    let body = json!({ "summary": title, "start": moment(start), "end": moment(end) });
    google::call(&token, "POST", &format!("{CALENDARS}/primary/events"), Some(&body)).await?;
    Ok(account.name().to_owned())
}

/// A token for the account `source` belongs to, after checking it may change events.
async fn writer(source: &Source) -> Result<crate::credentials::Password, String> {
    let client = Client::stored().await?.ok_or("no Google OAuth client is stored")?;
    let file = google::accounts_path().ok_or("this system has no application data directory")?;
    let account = google::accounts(&file)?
        .into_iter()
        .find(|account| account.email == source.account)
        .ok_or_else(|| format!("{} is no longer connected", source.account))?;
    if !account.can(CALENDAR_EVENTS) {
        return Err(format!(
            "the {} profile does not let Maverick change events yet; sign in to it again under Connections",
            account.name()
        ));
    }
    google::access_token(&client, &account.email).await
}

fn event_url(source: &Source) -> String {
    format!(
        "{CALENDARS}/{}/events/{}",
        form_encoded(&source.calendar),
        form_encoded(&source.id)
    )
}

/// Moves `event` to start at `start` and end at `end`.
pub async fn reschedule(event: &Event, start: &ZonedDateTime, end: &ZonedDateTime) -> Result<(), String> {
    let token = writer(&event.source).await?;
    let moment = |at: &ZonedDateTime| {
        json!({
            "dateTime": at.timestamp().strftime("%Y-%m-%dT%H:%M:%SZ").to_string(),
            "timeZone": at.time_zone().iana_name().unwrap_or("UTC"),
        })
    };
    let body = json!({ "start": moment(start), "end": moment(end) });
    google::call(&token, "PATCH", &event_url(&event.source), Some(&body))
        .await
        .map(drop)
}

pub async fn delete(event: &Event) -> Result<(), String> {
    let token = writer(&event.source).await?;
    google::call(&token, "DELETE", &event_url(&event.source), None)
        .await
        .map(drop)
}

/// The free stretches and events of `day` in time order, for showing them together.
pub fn day_plan<'a>(events: &'a [Event], day: Date, now: &ZonedDateTime) -> Vec<Stretch<'a>> {
    let mut plan: Vec<Stretch<'a>> = events
        .iter()
        .filter(|event| event.touches(day))
        .map(Stretch::Event)
        .collect();
    plan.extend(free_slots(events, day, now).into_iter().map(Stretch::Free));
    let key = |stretch: &Stretch| match stretch {
        Stretch::Event(event) => match &event.span {
            Span::Days(..) => (0, None),
            Span::At(start, _) => (1, Some(start.timestamp())),
        },
        Stretch::Free(slot) => (1, Some(slot.start.timestamp())),
    };
    plan.sort_by_key(key);
    plan
}

/// One line of a day: an event, or free time between events.
#[derive(Clone, Debug, PartialEq)]
pub enum Stretch<'a> {
    Event(&'a Event),
    Free(Slot),
}

/// How long a stretch is, as `2 h` or `1 h 30 min`.
pub fn length(minutes: i64) -> String {
    let duration = SignedDuration::from_mins(minutes);
    match (duration.as_hours(), minutes % 60) {
        (0, rest) => format!("{rest} min"),
        (hours, 0) => format!("{hours} h"),
        (hours, rest) => format!("{hours} h {rest} min"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiff::civil::date;

    fn zone() -> TimeZone {
        TimeZone::get("America/Los_Angeles").unwrap()
    }

    fn at(day: u8, hour: i8, minute: i8) -> ZonedDateTime {
        date(2026, 11, day as i8)
            .at(hour, minute, 0, 0)
            .to_zoned_datetime(zone())
            .unwrap()
    }

    fn timed(title: &str, start: ZonedDateTime, end: ZonedDateTime) -> Event {
        Event {
            title: title.into(),
            profile: "Work".into(),
            source: Source::default(),
            span: Span::At(start, end),
            busy: true,
        }
    }

    #[test]
    fn free_time_runs_between_busy_events_inside_the_planning_hours() {
        let events = [
            timed("Standup", at(23, 9, 0), at(23, 9, 30)),
            timed("Screen", at(23, 10, 0), at(23, 11, 30)),
            // Overlaps the screen: the free time starts after the later end.
            timed("Prep", at(23, 11, 0), at(23, 12, 0)),
            Event {
                title: "Lunch (free)".into(),
                profile: "Work".into(),
                source: Source::default(),
                span: Span::At(at(23, 12, 0), at(23, 13, 0)),
                busy: false,
            },
            timed("Gym", at(23, 16, 30), at(23, 17, 30)),
        ];
        let slots = free_slots(&events, date(2026, 11, 23), &at(22, 16, 0));
        let hours: Vec<String> = slots
            .iter()
            .map(|slot| format!("{}-{}", slot.start.strftime("%H:%M"), slot.end.strftime("%H:%M")))
            .collect();
        assert_eq!(hours, ["08:00-09:00", "09:30-10:00", "12:00-16:30", "17:30-22:00"]);
    }

    #[test]
    fn free_time_today_starts_now() {
        let slots = free_slots(&[], date(2026, 11, 22), &at(22, 16, 10));
        assert_eq!(slots[0].start, at(22, 16, 10));
        assert_eq!(slots[0].minutes(), 350);
    }

    #[test]
    fn a_clash_is_a_busy_event_that_overlaps() {
        let events = [timed("Screen", at(23, 10, 0), at(23, 11, 30))];
        assert_eq!(clashes(&events, &at(23, 11, 0), &at(23, 12, 0)).len(), 1);
        assert!(clashes(&events, &at(23, 11, 30), &at(23, 12, 0)).is_empty());
    }

    #[test]
    fn events_parse_from_googles_reply() {
        let reply = json!({ "items": [
            { "id": "e1", "summary": "Screen", "start": { "dateTime": "2026-11-23T10:00:00-08:00" },
              "end": { "dateTime": "2026-11-23T11:30:00-08:00" } },
            { "summary": "Trip", "start": { "date": "2026-11-23" }, "end": { "date": "2026-11-25" } },
            { "summary": "Gone", "status": "cancelled", "start": { "date": "2026-11-23" }, "end": { "date": "2026-11-24" } },
            { "summary": "Hold", "transparency": "transparent",
              "start": { "dateTime": "2026-11-23T18:00:00Z" }, "end": { "dateTime": "2026-11-23T19:00:00Z" } }
        ]});
        let from = Source {
            account: "w@example.com".into(),
            calendar: "primary".into(),
            id: String::new(),
        };
        let events = parse_events(&reply, &zone(), "Work", &from);
        assert_eq!(
            events[0].source,
            Source {
                id: "e1".into(),
                ..from.clone()
            }
        );
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].hours(), "10:00 to 11:30");
        assert!(events[1].touches(date(2026, 11, 24)) && !events[1].touches(date(2026, 11, 25)));
        assert!(!events[2].busy);
    }

    #[test]
    fn calendars_parse_with_the_primary_first() {
        let reply = json!({ "items": [
            { "id": "team@group.calendar.google.com", "summary": "Team", "summaryOverride": "On-call" },
            { "id": "kris@example.com", "summary": "kris@example.com", "primary": true }
        ]});
        let calendars = parse_calendars(&reply);
        assert!(calendars[0].primary);
        assert_eq!(calendars[1].name, "On-call");
    }

    #[test]
    fn blocks_go_to_the_named_profile_or_the_marked_one() {
        let profile = |email: &str, label: &str, write: bool, marked: bool| Account {
            email: email.into(),
            label: Some(label.into()),
            scopes: if write {
                vec![CALENDAR_EVENTS.into()]
            } else {
                vec![google::CALENDAR_READ.into()]
            },
            adds_blocks: marked,
            ..Account::default()
        };
        let accounts = [
            profile("w@example.com", "Work", true, false),
            profile("s@example.com", "School", true, true),
            profile("o@example.com", "Old", false, false),
        ];
        assert_eq!(block_account(&accounts, None).unwrap().email, "s@example.com");
        assert_eq!(block_account(&accounts, Some(" work ")).unwrap().email, "w@example.com");
        assert!(
            block_account(&accounts, Some("Old"))
                .unwrap_err()
                .contains("sign in to it again")
        );
        assert!(block_account(&accounts, Some("Gym")).is_err());
    }

    #[test]
    fn a_day_lists_events_and_free_time_in_order() {
        let events = [
            timed("Standup", at(23, 9, 0), at(23, 9, 30)),
            timed("Gym", at(23, 17, 0), at(23, 18, 0)),
        ];
        let plan = day_plan(&events, date(2026, 11, 23), &at(22, 16, 0));
        let read: Vec<String> = plan
            .iter()
            .map(|stretch| match stretch {
                Stretch::Event(event) => event.title.clone(),
                Stretch::Free(slot) => format!("free {}", slot.start.strftime("%H:%M")),
            })
            .collect();
        assert_eq!(read, ["free 08:00", "Standup", "free 09:30", "Gym", "free 18:00"]);
    }

    #[test]
    fn lengths_read_as_hours_and_minutes() {
        assert_eq!(length(120), "2 h");
        assert_eq!(length(90), "1 h 30 min");
        assert_eq!(length(45), "45 min");
    }
}
