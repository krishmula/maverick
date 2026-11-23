//! LinkedIn: the user's connections export, the pages Maverick reads, and the typing that stops before Send.
//!
//! LinkedIn restricts accounts that load many pages, so every page goes through `Visits`:
//! a cap per run and a random wait between loads. Maverick reads a company's People page
//! and the hiring-post search, never general people search, and never opens a profile
//! except to type a draft the user approved.

use std::sync::Arc;
use std::time::Duration;

use jiff::civil::Date;
use scraper::{ElementRef, Html, Node, Selector};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::bridge::{Bridge, BridgeError, BrowserTab, Command, Tab};
use crate::page::{Locator, Page, PageError};

const SITE: &str = "https://www.linkedin.com";

/// One row of the Connections.csv file in LinkedIn's data export.
#[derive(Clone, Debug, PartialEq)]
pub struct Connection {
    pub first_name: String,
    pub last_name: String,
    /// Some rows in the export have no profile address.
    pub profile_url: Option<String>,
    pub email: Option<String>,
    pub company: String,
    pub position: String,
    pub connected_on: Option<Date>,
}

impl Connection {
    pub fn name(&self) -> String {
        format!("{} {}", self.first_name, self.last_name).trim().to_owned()
    }
}

const CONNECTION_COLUMNS: [&str; 7] = [
    "First Name",
    "Last Name",
    "URL",
    "Email Address",
    "Company",
    "Position",
    "Connected On",
];

/// Reads LinkedIn's Connections.csv. The export starts with a few lines of notes before the header.
pub fn read_connections(text: &str) -> Result<Vec<Connection>, String> {
    let rows = csv_rows(text.trim_start_matches('\u{feff}'));
    let header = rows
        .iter()
        .position(|row| row.first().map(String::as_str) == Some("First Name"))
        .ok_or("this file has no \"First Name\" header, so it is not LinkedIn's Connections.csv")?;
    let columns: Vec<usize> = CONNECTION_COLUMNS
        .iter()
        .map(|name| {
            rows[header]
                .iter()
                .position(|cell| cell.trim() == *name)
                .ok_or_else(|| format!("Connections.csv has no {name:?} column"))
        })
        .collect::<Result<_, _>>()?;
    let mut connections = Vec::new();
    for (number, row) in rows.iter().enumerate().skip(header + 1) {
        let cell = |at: usize| row.get(columns[at]).map(|text| text.trim()).unwrap_or("");
        let given = |at: usize| Some(cell(at)).filter(|text| !text.is_empty()).map(str::to_owned);
        if row.iter().all(|cell| cell.trim().is_empty()) {
            continue;
        }
        let connected_on = match given(6) {
            Some(day) => Some(
                Date::strptime("%d %b %Y", &day)
                    .map_err(|_| format!("line {}: {day:?} is not a date like 15 Nov 2026", number + 1))?,
            ),
            None => None,
        };
        connections.push(Connection {
            first_name: cell(0).to_owned(),
            last_name: cell(1).to_owned(),
            profile_url: given(2).map(|url| profile_address(&url)),
            email: given(3),
            company: cell(4).to_owned(),
            position: cell(5).to_owned(),
            connected_on,
        });
    }
    Ok(connections)
}

/// Splits CSV text into rows of cells. Quoted cells may hold commas, quotes and line breaks.
fn csv_rows(text: &str) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let (mut row, mut cell) = (Vec::new(), String::new());
    let mut quoted = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match (quoted, c) {
            (true, '"') if chars.peek() == Some(&'"') => {
                cell.push('"');
                chars.next();
            }
            (true, '"') => quoted = false,
            (true, c) => cell.push(c),
            (false, '"') => quoted = true,
            (false, ',') => row.push(std::mem::take(&mut cell)),
            (false, '\r') => {}
            (false, '\n') => {
                row.push(std::mem::take(&mut cell));
                rows.push(std::mem::take(&mut row));
            }
            (false, c) => cell.push(c),
        }
    }
    if !cell.is_empty() || !row.is_empty() {
        row.push(cell);
        rows.push(row);
    }
    rows
}

/// A profile address in one form, `https://www.linkedin.com/in/<id>/`, so the same person always compares equal.
pub fn profile_address(link: &str) -> String {
    let path = link.split(['?', '#']).next().unwrap_or(link).trim_end_matches('/');
    match path.split_once("/in/") {
        Some((_, id)) => format!("{SITE}/in/{}/", id.split('/').next().unwrap_or(id)),
        None => link.to_owned(),
    }
}

/// A company name reduced to what identifies it: lower case, no punctuation, no "Inc." or "LLC".
pub fn company_key(name: &str) -> String {
    const SUFFIXES: &[&str] = &[
        "inc",
        "llc",
        "ltd",
        "corp",
        "corporation",
        "co",
        "company",
        "plc",
        "gmbh",
        "limited",
    ];
    let cleaned: String = name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect();
    let mut words: Vec<&str> = cleaned.split_whitespace().collect();
    while words.len() > 1 && words.last().is_some_and(|word| SUFFIXES.contains(word)) {
        words.pop();
    }
    words.join(" ")
}

/// Companies that count as one employer, a parent with its subsidiaries.
const SAME_EMPLOYER: &[&[&str]] = &[
    &["amazon", "aws", "amazon web services"],
    &["google", "alphabet", "google deepmind", "deepmind", "youtube"],
    &["meta", "facebook", "instagram", "whatsapp"],
    &["microsoft", "github", "linkedin"],
];

/// True when two company names mean the same employer, counting a parent and its subsidiary as one.
pub fn same_company(one: &str, other: &str) -> bool {
    let (one, other) = (company_key(one), company_key(other));
    if one.is_empty() || other.is_empty() {
        return false;
    }
    // "Example" and "Example Robotics" are one employer; "Example" and "Examples" are not.
    let leads = |long: &str, short: &str| long == short || long.starts_with(&format!("{short} "));
    if leads(&one, &other) || leads(&other, &one) {
        return true;
    }
    let group = |key: &str| {
        SAME_EMPLOYER
            .iter()
            .position(|names| names.iter().any(|name| leads(key, name)))
    };
    group(&one).is_some() && group(&one) == group(&other)
}

/// Whether the author of a post works at the company that is hiring, read from their headline.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PosterCheck {
    WorksThere,
    /// The headline names another employer, such as a recruiting agency.
    Elsewhere,
    /// The headline names no employer.
    Unclear,
}

/// Reads the employers a headline names, as in "Engineer at Example" or "Recruiter @ Agency | ex-Other".
pub fn poster_check(headline: &str, company: &str) -> PosterCheck {
    let lowered = headline.to_lowercase();
    let mut employers = Vec::new();
    for marker in [" at ", "@"] {
        let mut rest = lowered.as_str();
        while let Some(at) = rest.find(marker) {
            rest = &rest[at + marker.len()..];
            let employer = rest
                .split(['|', ',', '•', '·', '/', '(', ')', ';'])
                .next()
                .unwrap_or("")
                .split(" - ")
                .next()
                .unwrap_or("")
                .trim();
            if !employer.is_empty() {
                employers.push(employer.to_owned());
            }
        }
    }
    if employers.is_empty() {
        PosterCheck::Unclear
    } else if employers.iter().any(|employer| same_company(employer, company)) {
        PosterCheck::WorksThere
    } else {
        PosterCheck::Elsewhere
    }
}

/// A person on a company's People page. The card carries enough, so the profile is never opened.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Person {
    pub name: String,
    pub title: String,
    pub profile_url: Option<String>,
    /// As the card shows it, such as `2nd`. `None` when it shows none.
    pub degree: Option<String>,
}

/// What a company's People page showed.
#[derive(Clone, Debug, PartialEq)]
pub struct PeoplePage {
    /// The company name at the top of the page, to confirm the page is the right company.
    pub company: Option<String>,
    pub people: Vec<Person>,
}

/// The company's People page, searched for `keywords`. `slug` is the company's part of its page address.
pub fn people_search_address(slug: &str, keywords: &str) -> String {
    format!("{SITE}/company/{slug}/people/?keywords={}", encoded(keywords))
}

/// A guess at a company's page address from its name: lower case, words joined by hyphens.
pub fn company_slug(company: &str) -> String {
    company_key(company).replace(' ', "-")
}

fn selector(css: &str) -> Selector {
    Selector::parse(css).expect("the selectors in this file are valid CSS")
}

/// Text with runs of white space made single spaces.
fn squeezed(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn text_of(element: ElementRef<'_>) -> String {
    squeezed(&element.text().collect::<String>())
}

/// The text LinkedIn shows sighted users. It repeats names in hidden spans for screen readers.
fn shown_text(element: ElementRef<'_>) -> String {
    match element.select(&selector("[aria-hidden=\"true\"]")).next() {
        Some(shown) => text_of(shown),
        None => text_of(element),
    }
}

/// Reads the cards on a company's People page. Members outside the user's network show as
/// "LinkedIn Member" with no profile; they are left out.
pub fn people_page(html: &str) -> PeoplePage {
    let page = Html::parse_document(html);
    let company = page.select(&selector("h1")).map(text_of).find(|name| !name.is_empty());
    let mut people = Vec::new();
    for card in page.select(&selector(
        ".org-people-profile-card__profile-card-spacing, .org-people-profile-card",
    )) {
        let name = card
            .select(&selector(".artdeco-entity-lockup__title"))
            .next()
            .map(shown_text)
            .unwrap_or_default();
        let profile_url = card
            .select(&selector("a[href*=\"/in/\"]"))
            .filter_map(|link| link.attr("href"))
            .next()
            .map(profile_address);
        if name.is_empty() || name == "LinkedIn Member" || profile_url.is_none() {
            continue;
        }
        let title = card
            .select(&selector(".artdeco-entity-lockup__subtitle"))
            .next()
            .map(text_of)
            .unwrap_or_default();
        let degree = card
            .select(&selector(".artdeco-entity-lockup__degree"))
            .next()
            .map(|degree| text_of(degree).trim_start_matches('·').trim().to_owned())
            .filter(|degree| !degree.is_empty());
        if !people.iter().any(|seen: &Person| seen.profile_url == profile_url) {
            people.push(Person {
                name,
                title,
                profile_url,
                degree,
            });
        }
    }
    PeoplePage { company, people }
}

/// A post from LinkedIn's content search.
#[derive(Clone, Debug, PartialEq)]
pub struct Post {
    /// LinkedIn's id for the post, such as `urn:li:activity:7390000000000000001`.
    pub urn: String,
    pub poster: String,
    /// `None` when a company page, not a person, posted it.
    pub poster_url: Option<String>,
    pub headline: String,
    /// `1st` for a connection, as the post shows it.
    pub degree: Option<String>,
    pub text: String,
    /// False while LinkedIn still cuts the text short behind a "more" button.
    pub complete: bool,
}

impl Post {
    pub fn link(&self) -> String {
        post_address(&self.urn)
    }

    pub fn is_connection(&self) -> bool {
        self.degree.as_deref() == Some("1st")
    }
}

pub fn post_address(urn: &str) -> String {
    format!("{SITE}/feed/update/{urn}/")
}

/// LinkedIn's content search for posts from the past day, newest first.
pub fn post_search_address(keywords: &str) -> String {
    format!(
        "{SITE}/search/results/content/?keywords={}&datePosted=%22past-24h%22&sortBy=%22date_posted%22",
        encoded(keywords)
    )
}

/// Reads the posts on a content search page.
pub fn posts_page(html: &str) -> Vec<Post> {
    let page = Html::parse_document(html);
    let mut posts: Vec<Post> = Vec::new();
    for update in page.select(&selector("[data-urn^=\"urn:li:\"]")) {
        let Some(urn) = update.attr("data-urn") else { continue };
        if posts.iter().any(|post| post.urn == urn) {
            continue;
        }
        let actor = update.select(&selector(".update-components-actor")).next();
        let find = |css: &str| actor.and_then(|actor| actor.select(&selector(css)).next());
        let poster = find(".update-components-actor__title")
            .map(shown_text)
            .unwrap_or_default();
        let poster_url = find("a[href*=\"/in/\"]")
            .and_then(|link| link.attr("href"))
            .map(profile_address);
        let headline = find(".update-components-actor__description")
            .map(shown_text)
            .unwrap_or_default();
        let degree = find(".update-components-actor__supplementary-actor-info")
            .map(text_of)
            .and_then(|info| {
                ["1st", "2nd", "3rd"]
                    .into_iter()
                    .find(|degree| info.contains(degree))
                    .map(str::to_owned)
            });
        let Some(body) = update.select(&selector(".update-components-text")).next() else {
            continue;
        };
        let cut_short = update
            .select(&selector(".feed-shared-inline-show-more-text__see-more-less-toggle"))
            .any(|toggle| {
                let label = text_of(toggle).to_lowercase();
                label.contains("more") && !label.contains("less")
            });
        posts.push(Post {
            urn: urn.to_owned(),
            poster,
            poster_url,
            headline,
            degree,
            text: lines_of(body),
            complete: !cut_short,
        });
    }
    posts
}

/// The text of an element, keeping its line breaks.
fn lines_of(element: ElementRef<'_>) -> String {
    let mut text = String::new();
    for node in element.descendants() {
        match node.value() {
            Node::Text(part) => text.push_str(part),
            Node::Element(tag) if tag.name() == "br" => text.push('\n'),
            _ => {}
        }
    }
    text.lines()
        .map(squeezed)
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_owned()
}

fn encoded(text: &str) -> String {
    let mut out = String::new();
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// The random wait between two LinkedIn page loads.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pacing {
    pub least: Duration,
    pub most: Duration,
}

/// Slow enough to read like a person clicking through.
pub const PACING: Pacing = Pacing {
    least: Duration::from_secs(8),
    most: Duration::from_secs(20),
};

impl Pacing {
    /// The wait for a roll of the dice between 0 and 1.
    pub fn wait(&self, roll: f64) -> Duration {
        self.least + (self.most - self.least).mul_f64(roll.clamp(0.0, 1.0))
    }
}

/// The LinkedIn pages one run may still load, and the wait before the next one.
#[derive(Debug)]
pub struct Visits {
    left: usize,
    pacing: Pacing,
    loaded: usize,
}

impl Visits {
    pub fn new(cap: usize, pacing: Pacing) -> Self {
        Self {
            left: cap,
            pacing,
            loaded: 0,
        }
    }

    /// Takes one page from the cap. Gives the wait before loading it, or `None` when the cap is spent.
    /// The first page of a run loads at once.
    pub fn next(&mut self, roll: f64) -> Option<Duration> {
        self.left = self.left.checked_sub(1)?;
        self.loaded += 1;
        Some(if self.loaded == 1 {
            Duration::ZERO
        } else {
            self.pacing.wait(roll)
        })
    }

    pub fn loaded(&self) -> usize {
        self.loaded
    }
}

/// Why LinkedIn work stopped.
#[derive(Debug, PartialEq)]
pub enum Stop {
    /// The run used every page it may load.
    CapReached,
    /// LinkedIn showed a sign-in page or a security check. Nothing more should load today.
    SignedOut(String),
    Failed(String),
}

impl std::fmt::Display for Stop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CapReached => f.write_str("the run reached its cap of LinkedIn pages"),
            Self::SignedOut(at) => write!(f, "LinkedIn is not signed in, or asks for a check ({at})"),
            Self::Failed(why) => f.write_str(why),
        }
    }
}

impl From<PageError> for Stop {
    fn from(error: PageError) -> Self {
        Self::Failed(error.to_string())
    }
}

/// True for an address LinkedIn sends a visitor to when it will not show the page.
pub fn is_wall(address: &str) -> bool {
    ["/login", "/authwall", "/checkpoint", "/uas/", "/signup"]
        .iter()
        .any(|wall| address.contains(wall))
}

/// One background tab in the user's Chrome that loads LinkedIn pages, within a cap and at a person's pace.
pub struct Reader {
    page: Page,
    visits: Visits,
}

/// Opens every "more" button in the posts on the page, so the full text is in the page.
const EXPAND_POSTS: &str = r#"(async () => {
    for (const toggle of document.querySelectorAll(".feed-shared-inline-show-more-text__see-more-less-toggle")) {
        if (/more/i.test(toggle.innerText) && !/less/i.test(toggle.innerText)) toggle.click();
    }
    await new Promise((done) => setTimeout(done, 1500));
    return true;
})()"#;

impl Reader {
    /// Opens a tab behind the user's current one. Chrome shows its debugging bar until `close`.
    pub async fn open(bridge: Arc<Bridge>, cap: usize) -> Result<Self, Stop> {
        let failed = |error: BridgeError| Stop::Failed(error.to_string());
        let connection = *bridge
            .connections()
            .first()
            .ok_or_else(|| failed(BridgeError::NotConnected))?;
        let opened = bridge
            .request(
                connection,
                &Command::OpenTab {
                    url: "about:blank".into(),
                    active: false,
                },
            )
            .await
            .map_err(failed)?;
        let tab = opened_tab(connection, &opened, false)?;
        let page = Page::attach(bridge, &tab).await?;
        Ok(Self {
            page,
            visits: Visits::new(cap, PACING),
        })
    }

    /// Loads a page after the pacing wait and gives its HTML. `expand` opens cut-short posts first.
    pub async fn load(&mut self, address: &str, expand: bool) -> Result<String, Stop> {
        let wait = self.visits.next(fastrand::f64()).ok_or(Stop::CapReached)?;
        tokio::time::sleep(wait).await;
        self.page.navigate(address).await?;
        // LinkedIn draws its results after the load event.
        tokio::time::sleep(Duration::from_secs(3)).await;
        let landed = self.page.evaluate("location.href").await?;
        let landed = landed.as_str().unwrap_or_default();
        if is_wall(landed) {
            return Err(Stop::SignedOut(landed.to_owned()));
        }
        if expand {
            self.page.evaluate(EXPAND_POSTS).await?;
        }
        let html = self.page.evaluate("document.documentElement.outerHTML").await?;
        Ok(html.as_str().unwrap_or_default().to_owned())
    }

    /// How many pages this reader loaded.
    pub fn loaded(&self) -> usize {
        self.visits.loaded()
    }

    /// Closes the tab. A failure leaves an idle tab behind, which does no harm.
    pub async fn close(self) {
        let _ = self.page.send("Page.close", json!({})).await;
        let _ = self.page.detach().await;
    }
}

fn opened_tab(connection: crate::bridge::ConnectionId, opened: &Value, active: bool) -> Result<Tab, Stop> {
    let number = |key: &str| {
        opened[key]
            .as_i64()
            .and_then(|number| i32::try_from(number).ok())
            .ok_or_else(|| Stop::Failed("Chrome did not say which tab it opened".into()))
    };
    Ok(Tab {
        connection,
        tab: BrowserTab {
            id: number("id")?,
            window_id: number("windowId")?,
            title: String::new(),
            url: String::new(),
            active,
        },
    })
}

/// Where on LinkedIn a draft goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Slot {
    /// The note on a connection request: about 300 characters, no attachment.
    Note,
    /// A direct message, which only a connection can receive.
    Message,
}

impl Slot {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Note => "note",
            Self::Message => "message",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "note" => Some(Self::Note),
            "message" => Some(Self::Message),
            _ => None,
        }
    }

    /// A connection gets a message; anyone else gets a connection note first.
    pub fn for_degree(degree: Option<&str>) -> Self {
        if degree == Some("1st") {
            Self::Message
        } else {
            Self::Note
        }
    }
}

/// The longest connection note LinkedIn takes.
pub const NOTE_LIMIT: usize = 300;

/// The only things the typing flow ever clicks. There is no Send here, on purpose:
/// the user reads the typed draft and clicks Send in Chrome.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Control {
    Message,
    Connect,
    MoreActions,
    ConnectInMenu,
    AddNote,
}

impl Control {
    #[cfg(test)]
    const ALL: [Control; 5] = [
        Control::Message,
        Control::Connect,
        Control::MoreActions,
        Control::ConnectInMenu,
        Control::AddNote,
    ];

    fn locator(self) -> Locator {
        // The profile's top card is the first section of the page's main part.
        let in_card = |test: &str| {
            format!(
                r#"(() => {{
                    const card = document.querySelector("main section");
                    if (!card) return null;
                    return [...card.querySelectorAll("button, a")].find((control) => {{
                        const label = (control.getAttribute("aria-label") || "").trim();
                        const text = control.innerText.trim();
                        return {test};
                    }}) || null;
                }})()"#
            )
        };
        let (name, script) = match self {
            Control::Message => ("Message button", in_card(r#"/^Message\b/i.test(label) || text === "Message""#)),
            Control::Connect => (
                "Connect button",
                in_card(r#"/^Invite .* to connect$/i.test(label) || text === "Connect""#),
            ),
            Control::MoreActions => ("More button", in_card(r#"label === "More actions" || text === "More""#)),
            Control::ConnectInMenu => (
                "Connect in the More menu",
                r#"[...document.querySelectorAll(".artdeco-dropdown__content [role='button'], .artdeco-dropdown__content button")]
                    .find((item) => /^Invite .* to connect$/i.test(item.getAttribute("aria-label") || "")
                        || item.innerText.trim() === "Connect") || null"#
                    .to_owned(),
            ),
            Control::AddNote => (
                "Add a note button",
                r#"[...document.querySelectorAll("[role='dialog'] button")]
                    .find((button) => (button.getAttribute("aria-label") || button.innerText).trim() === "Add a note") || null"#
                    .to_owned(),
            ),
        };
        Locator::script(name, script)
    }
}

const NOTE_FIELD: &str =
    "document.querySelector(\"[role='dialog'] textarea[name='message'], textarea#custom-message\")";
const MESSAGE_FIELD: &str =
    "[...document.querySelectorAll(\".msg-form__contenteditable[contenteditable='true']\")].pop() || null";
/// LinkedIn asks for the person's email before some connection requests. Maverick never invents one.
const EMAIL_WALL: &str =
    "document.querySelector(\"[role='dialog'] input[name='email'], [role='dialog'] input[type='email']\")";

const STEP_WAIT: Duration = Duration::from_secs(10);

/// Opens the person's profile in the user's Chrome, opens the message or connection dialog,
/// and types the approved draft. It stops there: the user reviews it and clicks Send.
pub async fn type_draft(bridge: Arc<Bridge>, profile_url: &str, slot: Slot, text: &str) -> Result<(), String> {
    if !profile_url.starts_with(&format!("{SITE}/in/")) {
        return Err(format!("{profile_url} is not a LinkedIn profile"));
    }
    let said = |error: BridgeError| error.to_string();
    let connection = *bridge
        .connections()
        .first()
        .ok_or_else(|| said(BridgeError::NotConnected))?;
    let blank = Command::OpenTab {
        url: "about:blank".into(),
        active: true,
    };
    let opened = bridge.request(connection, &blank).await.map_err(said)?;
    let tab = opened_tab(connection, &opened, true).map_err(|stop| stop.to_string())?;
    let page = Page::attach(bridge, &tab).await.map_err(|error| error.to_string())?;
    let typed = type_in(&page, profile_url, slot, text).await;
    // The debugging bar goes away; the dialog and the typed draft stay for the user.
    let _ = page.detach().await;
    typed
}

async fn type_in(page: &Page, profile_url: &str, slot: Slot, text: &str) -> Result<(), String> {
    let said = |error: PageError| error.to_string();
    page.navigate(profile_url).await.map_err(said)?;
    let landed = page.evaluate("location.href").await.map_err(said)?;
    if is_wall(landed.as_str().unwrap_or_default()) {
        return Err("LinkedIn is not signed in in this Chrome".into());
    }
    let found = |control: Control| async move {
        page.wait_for(&control.locator(), STEP_WAIT)
            .await
            .map(|()| control)
            .map_err(said)
    };
    let field = match slot {
        Slot::Message => {
            let message = found(Control::Message)
                .await
                .map_err(|_| "the profile has no Message button, so this person may not be a connection".to_owned())?;
            page.click(&message.locator()).await.map_err(said)?;
            Locator::script("message box", MESSAGE_FIELD)
        }
        Slot::Note => {
            if let Ok(connect) = found(Control::Connect).await {
                page.click(&connect.locator()).await.map_err(said)?;
            } else {
                let more = found(Control::MoreActions).await.map_err(|_| {
                    "the profile shows no Connect button; the request may be pending already".to_owned()
                })?;
                page.click(&more.locator()).await.map_err(said)?;
                let connect = found(Control::ConnectInMenu)
                    .await
                    .map_err(|_| "the profile offers no Connect action".to_owned())?;
                page.click(&connect.locator()).await.map_err(said)?;
            }
            let add_note = found(Control::AddNote).await;
            if page.evaluate(&format!("!!({EMAIL_WALL})")).await.map_err(said)? == true {
                return Err("LinkedIn asks for this person's email address before connecting. \
                            Maverick stopped; enter it yourself if you know it."
                    .into());
            }
            let add_note = add_note.map_err(|_| "the connection dialog has no Add a note button".to_owned())?;
            page.click(&add_note.locator()).await.map_err(said)?;
            Locator::script("note box", NOTE_FIELD)
        }
    };
    page.wait_for(&field, STEP_WAIT).await.map_err(said)?;
    page.fill(&field, text).await.map_err(said)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONNECTIONS: &str = "Notes:\n\
\"When exporting your connection data, you may notice that some of the email addresses are missing.\"\n\
\n\
First Name,Last Name,URL,Email Address,Company,Position,Connected On\n\
Ada,Example,https://www.linkedin.com/in/ada-example-1a2b3c,,Example Co,\"Software Engineer, Storage\",15 Nov 2026\n\
Bo,Sample,https://www.linkedin.com/in/bo-sample?trk=x,bo@sample.test,Sample Inc,\"Recruiter, \"\"Tech\"\"\",02 Jan 2025\n\
Cy,Nolink,,,Other LLC,Engineer,\n";

    #[test]
    fn the_connections_export_is_read_past_its_notes() {
        let read = read_connections(CONNECTIONS).unwrap();
        assert_eq!(read.len(), 3);
        assert_eq!(
            read[0],
            Connection {
                first_name: "Ada".into(),
                last_name: "Example".into(),
                profile_url: Some("https://www.linkedin.com/in/ada-example-1a2b3c/".into()),
                email: None,
                company: "Example Co".into(),
                position: "Software Engineer, Storage".into(),
                connected_on: Some(jiff::civil::date(2026, 11, 15)),
            }
        );
        assert_eq!(read[1].position, "Recruiter, \"Tech\"");
        assert_eq!(read[1].email.as_deref(), Some("bo@sample.test"));
        assert_eq!(
            read[1].profile_url.as_deref(),
            Some("https://www.linkedin.com/in/bo-sample/")
        );
        assert_eq!(read[2].profile_url, None);
        assert_eq!(read[2].connected_on, None);
        assert!(read_connections("Name,Company\nA,B\n").is_err());
    }

    #[test]
    fn companies_match_by_name_and_by_parent() {
        assert!(same_company("Example Co, Inc.", "example co"));
        assert!(same_company("Example", "Example Robotics"));
        assert!(same_company("Amazon", "Amazon Web Services (AWS)"));
        assert!(same_company("AWS", "Amazon"));
        assert!(!same_company("Example", "Examples"));
        assert!(!same_company("Amazon", "Google"));
        assert!(!same_company("", "Example"));
        assert_eq!(company_slug("Example Labs, Inc."), "example-labs");
    }

    #[test]
    fn the_headline_says_whether_the_poster_works_at_the_company() {
        assert_eq!(
            poster_check("Engineering Manager at Example Co | Hiring", "Example"),
            PosterCheck::WorksThere
        );
        assert_eq!(
            poster_check("Senior SWE @AWS · ex-Sample", "Amazon"),
            PosterCheck::WorksThere
        );
        assert_eq!(
            poster_check(
                "Technical Recruiter at Talent Agency - hiring for top startups",
                "Example"
            ),
            PosterCheck::Elsewhere
        );
        assert_eq!(
            poster_check("Helping engineers find jobs", "Example"),
            PosterCheck::Unclear
        );
    }

    const PEOPLE: &str = r#"<html><body><main>
<h1 class="org-top-card-summary__title" title="Example Co">
  Example Co
</h1>
<ul>
 <li class="grid org-people-profile-card__profile-card-spacing">
  <section class="artdeco-card">
   <div class="artdeco-entity-lockup">
    <div class="artdeco-entity-lockup__image"><a href="https://www.linkedin.com/in/ada-example-1a2b3c?miniProfileUrn=urn%3Ali%3Afs"><img alt="Ada Example"></a></div>
    <div class="artdeco-entity-lockup__title"><a href="https://www.linkedin.com/in/ada-example-1a2b3c?miniProfileUrn=urn%3Ali%3Afs"><div class="lt-line-clamp">
      Ada Example
    </div></a></div>
    <div class="artdeco-entity-lockup__badge"><span class="a11y-text">2nd degree connection</span><span class="artdeco-entity-lockup__degree" aria-hidden="true">· 2nd</span></div>
    <div class="artdeco-entity-lockup__subtitle"><div class="lt-line-clamp">Software Engineer, Storage Infrastructure</div></div>
   </div>
  </section>
 </li>
 <li class="grid org-people-profile-card__profile-card-spacing">
  <section class="artdeco-card"><div class="artdeco-entity-lockup">
    <div class="artdeco-entity-lockup__title"><div class="lt-line-clamp">LinkedIn Member</div></div>
    <div class="artdeco-entity-lockup__subtitle"><div>Engineer</div></div>
  </div></section>
 </li>
 <li class="grid org-people-profile-card__profile-card-spacing">
  <section class="artdeco-card"><div class="artdeco-entity-lockup">
    <div class="artdeco-entity-lockup__title"><a href="/in/bo-sample/"><div>Bo Sample</div></a></div>
    <div class="artdeco-entity-lockup__subtitle"><div>Engineering Manager, Storage</div></div>
  </div></section>
 </li>
</ul></main></body></html>"#;

    #[test]
    fn a_people_page_gives_named_cards_with_title_profile_and_degree() {
        let page = people_page(PEOPLE);
        assert_eq!(page.company.as_deref(), Some("Example Co"));
        assert_eq!(
            page.people,
            [
                Person {
                    name: "Ada Example".into(),
                    title: "Software Engineer, Storage Infrastructure".into(),
                    profile_url: Some("https://www.linkedin.com/in/ada-example-1a2b3c/".into()),
                    degree: Some("2nd".into()),
                },
                Person {
                    name: "Bo Sample".into(),
                    title: "Engineering Manager, Storage".into(),
                    profile_url: Some("https://www.linkedin.com/in/bo-sample/".into()),
                    degree: None,
                },
            ]
        );
        assert_eq!(
            people_search_address("example-co", "storage & infra"),
            "https://www.linkedin.com/company/example-co/people/?keywords=storage%20%26%20infra"
        );
    }

    const POSTS: &str = r#"<html><body><div class="search-results-container"><ul role="list">
<li><div data-urn="urn:li:activity:7390000000000000001" class="feed-shared-update-v2">
  <div class="update-components-actor">
    <a class="update-components-actor__meta-link" href="https://www.linkedin.com/in/sam-poster?miniProfileUrn=x">
      <span class="update-components-actor__title"><span dir="ltr"><span aria-hidden="true"><!---->Sam Poster<!----></span><span class="visually-hidden">Sam Poster</span></span>
      <span class="update-components-actor__supplementary-actor-info"> • 2nd</span></span>
      <span class="update-components-actor__description"><span aria-hidden="true">Engineering Manager at Example Co</span><span class="visually-hidden">Engineering Manager at Example Co</span></span>
    </a>
  </div>
  <div class="feed-shared-inline-show-more-text">
    <div class="update-components-text"><span class="break-words"><span dir="ltr">We're hiring new grad backend engineers!<br>Team: Storage.   Location: Austin, TX<br><br>Apply below.</span></span></div>
    <button class="feed-shared-inline-show-more-text__see-more-less-toggle">…less</button>
  </div>
</div></li>
<li><div data-urn="urn:li:activity:7390000000000000002" class="feed-shared-update-v2">
  <div class="update-components-actor">
    <a href="https://www.linkedin.com/company/jobs-digest/"><span class="update-components-actor__title"><span aria-hidden="true">Jobs Digest</span></span>
    <span class="update-components-actor__description"><span aria-hidden="true">12,000 followers</span></span></a>
  </div>
  <div class="update-components-text"><span>Top 50 companies hiring this week</span></div>
  <button class="feed-shared-inline-show-more-text__see-more-less-toggle">…more</button>
</div></li>
<li><div data-urn="urn:li:activity:7390000000000000001" class="feed-shared-update-v2"></div></li>
</ul></div></body></html>"#;

    #[test]
    fn a_search_page_gives_each_post_once_with_its_author_and_full_text() {
        let posts = posts_page(POSTS);
        assert_eq!(posts.len(), 2);
        assert_eq!(
            posts[0],
            Post {
                urn: "urn:li:activity:7390000000000000001".into(),
                poster: "Sam Poster".into(),
                poster_url: Some("https://www.linkedin.com/in/sam-poster/".into()),
                headline: "Engineering Manager at Example Co".into(),
                degree: Some("2nd".into()),
                text: "We're hiring new grad backend engineers!\nTeam: Storage. Location: Austin, TX\n\nApply below."
                    .into(),
                complete: true,
            }
        );
        assert_eq!(
            posts[0].link(),
            "https://www.linkedin.com/feed/update/urn:li:activity:7390000000000000001/"
        );
        assert!(!posts[0].is_connection());
        assert_eq!(posts[1].poster_url, None);
        assert!(!posts[1].complete);
    }

    #[test]
    fn walls_and_profiles_are_recognized() {
        assert!(is_wall("https://www.linkedin.com/checkpoint/challenge/abc"));
        assert!(is_wall("https://www.linkedin.com/authwall?trk=x"));
        assert!(!is_wall("https://www.linkedin.com/company/example/people/"));
        assert_eq!(
            profile_address("https://www.linkedin.com/in/ada/recent-activity/?x=1"),
            "https://www.linkedin.com/in/ada/"
        );
    }

    #[test]
    fn page_loads_keep_to_the_cap_and_wait_a_random_while_between() {
        let pacing = Pacing {
            least: Duration::from_secs(8),
            most: Duration::from_secs(20),
        };
        assert_eq!(pacing.wait(0.0), Duration::from_secs(8));
        assert_eq!(pacing.wait(0.5), Duration::from_secs(14));
        assert_eq!(pacing.wait(7.0), Duration::from_secs(20));
        let mut visits = Visits::new(3, pacing);
        assert_eq!(visits.next(0.9), Some(Duration::ZERO));
        for _ in 0..2 {
            let wait = visits.next(fastrand::f64()).unwrap();
            assert!((pacing.least..=pacing.most).contains(&wait));
        }
        assert_eq!(visits.next(0.5), None);
        assert_eq!(visits.next(0.5), None);
        assert_eq!(visits.loaded(), 3);
        assert!(PACING.least >= Duration::from_secs(5));
    }

    #[test]
    fn nothing_the_typing_flow_clicks_is_a_send_button() {
        for control in Control::ALL {
            let locator = control.locator();
            let script = locator.as_script().to_lowercase();
            assert!(!script.contains("send"), "{control:?} could reach a Send button");
            assert!(!script.contains("submit"), "{control:?} could reach a submit button");
        }
        assert_eq!(Slot::for_degree(Some("1st")), Slot::Message);
        assert_eq!(Slot::for_degree(Some("2nd")), Slot::Note);
        assert_eq!(Slot::for_degree(None), Slot::Note);
    }
}
