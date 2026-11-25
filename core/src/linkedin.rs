//! LinkedIn: the user's connections export, the pages Maverick reads, the typing that stops before
//! Send, and the one flow that presses Send, for postings Maverick reaches by itself.
//!
//! LinkedIn restricts accounts that load many pages, so every page goes through `Visits`:
//! a cap per run and a random wait between loads. Maverick reads a company's People page
//! and the hiring-post search. It runs a people search only for one person the user asked to
//! reach whose profile link is unknown. It opens a profile only to type a draft the user approved,
//! or to read what an InMail costs when the user asks for a draft.

use std::path::Path;
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

/// The company's People page. LinkedIn ignores a `keywords` parameter on it, so Maverick loads the
/// plain list and picks people from it. `slug` is the company's part of its page address.
pub fn people_address(slug: &str) -> String {
    format!("{SITE}/company/{slug}/people/")
}

/// LinkedIn's people search for `keywords`, such as a person's name and company.
pub fn people_search_address(keywords: &str) -> String {
    format!("{SITE}/search/results/people/?keywords={}", encoded(keywords))
}

/// The first person in `people` whose name is `wanted`. Extra words such as a middle name or a
/// credential are allowed, a nickname in brackets is left out, and a word the wanted name shortens
/// to an initial, as in "Ashish S.", fits any word with that initial.
pub fn named_in(people: &[Person], wanted: &str) -> Option<Person> {
    let words = |name: &str| -> Vec<String> {
        name.split(|c: char| !c.is_alphanumeric())
            .filter(|word| !word.is_empty())
            .map(str::to_lowercase)
            .collect()
    };
    // A nickname in brackets, as in "Yao (Jason) Lu", is often not on the card.
    let mut plain = String::new();
    let mut depth = 0;
    for ch in wanted.chars() {
        match ch {
            '(' => depth += 1,
            ')' => depth -= 1,
            _ if depth <= 0 => plain.push(ch),
            _ => {}
        }
    }
    let wanted = words(&plain);
    let fits = |person: &&Person| {
        let found = words(&person.name);
        let mut rest = found.iter();
        found.first() == wanted.first()
            && wanted.iter().all(|word| {
                rest.any(|have| have == word || (word.chars().count() == 1 && have.starts_with(word.as_str())))
            })
    };
    (!wanted.is_empty())
        .then(|| people.iter().find(fits).cloned())
        .flatten()
}

/// The LinkedIn company pages a web page links to, as slugs, the most linked first. A posting page
/// or a company's own website often links the company's page, whose slug need not match its name.
pub fn company_pages_in(html: &str) -> Vec<String> {
    let mut counts: Vec<(String, usize)> = Vec::new();
    for (at, _) in html.match_indices("linkedin.com/company/") {
        let rest = &html[at + "linkedin.com/company/".len()..];
        let slug: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '%'))
            .collect::<String>()
            .to_lowercase();
        if slug.is_empty() || slug == "linkedin" {
            continue;
        }
        match counts.iter_mut().find(|(seen, _)| *seen == slug) {
            Some((_, count)) => *count += 1,
            None => counts.push((slug, 1)),
        }
    }
    // Stable, so a tie keeps the slug the page links first.
    counts.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
    counts.into_iter().map(|(slug, _)| slug).collect()
}

/// The company's own website as an Ashby posting names it.
pub fn company_website_in(html: &str) -> Option<String> {
    let (_, rest) = html.split_once("\"publicWebsite\":\"")?;
    let (website, _) = rest.split_once('"')?;
    website.starts_with("http").then(|| website.to_owned())
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

/// The separate pieces of text in an element, in page order.
fn strings_of(element: ElementRef<'_>) -> Vec<String> {
    element.text().map(squeezed).filter(|text| !text.is_empty()).collect()
}

/// The page title less LinkedIn's suffix and the unread count it puts in front, such as `(3) `.
fn page_title(page: &Html) -> Option<String> {
    let title = page.select(&selector("title")).next().map(text_of)?;
    let title = title.strip_suffix(" | LinkedIn").unwrap_or(&title);
    let title = match title.strip_prefix('(').and_then(|rest| rest.split_once(") ")) {
        Some((count, rest)) if count.chars().all(|c| c.is_ascii_digit()) => rest,
        _ => title,
    };
    Some(title.to_owned())
}

/// Reads the cards on a company's People page. The page's classes are hashed, so cards are found
/// by role: each list item with a profile link is a person, whose texts are the name, the degree
/// (`· 2nd`), the headline and then lines such as a mutual connection, and the name is the text of
/// the profile link. The "Alumni who work here" list after the main one is left out, and so are
/// members shown as "LinkedIn Member" and company cards that link a person in another line.
///
/// `company` is the name in the title, `Example Co: People | LinkedIn`. Any other title, such as a
/// missing company's page, gives `None`.
pub fn people_page(html: &str) -> PeoplePage {
    let page = Html::parse_document(html);
    let company = page_title(&page)
        .and_then(|title| title.strip_suffix(": People").map(str::to_owned))
        .filter(|name| !name.is_empty());
    let mut people: Vec<Person> = Vec::new();
    let mut heading = String::new();
    // Headings and list items come in page order, so each card knows the list it sits under.
    for element in page.select(&selector("h2, [role=\"listitem\"]")) {
        if element.value().name() == "h2" {
            heading = text_of(element);
            continue;
        }
        if heading.contains("Alumni") {
            continue;
        }
        let strings = strings_of(element);
        let Some(name) = strings.first().cloned() else { continue };
        // The name is the text of the person's link. A company card in "People also viewed" links
        // a person only from a line such as "1 person in your network works here".
        let Some(link) = element
            .select(&selector("a[href*=\"/in/\"]"))
            .find(|link| strings_of(*link).first() == Some(&name))
        else {
            continue;
        };
        let profile_url = link.attr("href").map(profile_address);
        if name == "LinkedIn Member" || people.iter().any(|seen| seen.profile_url == profile_url) {
            continue;
        }
        let degree = strings
            .iter()
            .find_map(|text| text.strip_prefix('·'))
            .map(|degree| degree.trim().to_owned());
        let title = strings
            .iter()
            .skip(1)
            .find(|text| !text.starts_with('·'))
            .cloned()
            .unwrap_or_default();
        people.push(Person {
            name,
            title,
            profile_url,
            degree,
        });
    }
    PeoplePage { company, people }
}

/// A post from LinkedIn's content search.
#[derive(Clone, Debug, PartialEq)]
pub struct Post {
    pub poster: String,
    /// `None` when a company page, not a person, posted it.
    pub poster_url: Option<String>,
    pub headline: String,
    /// As the post shows it, such as `1st` or `3rd+`.
    pub degree: Option<String>,
    pub text: String,
    /// False when LinkedIn cut the text short, ending it with `…`.
    pub complete: bool,
}

impl Post {
    /// The search page carries no id for its posts, so the link is the poster's activity page. A
    /// fingerprint of the text follows it, so the same post gives the same link on every sweep.
    pub fn link(&self) -> String {
        let page = match &self.poster_url {
            Some(profile) => format!("{profile}recent-activity/all/"),
            None => format!("{SITE}/feed/"),
        };
        // FNV-1a, which unlike the standard hasher stays the same across Rust releases.
        let fingerprint = self.text.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
        });
        format!("{page}#post-{fingerprint:016x}")
    }

    pub fn is_connection(&self) -> bool {
        self.degree.as_deref() == Some("1st")
    }
}

/// LinkedIn's content search for posts from the past day, newest first.
pub fn post_search_address(keywords: &str) -> String {
    format!(
        "{SITE}/search/results/content/?keywords={}&datePosted=%22past-24h%22&sortBy=%22date_posted%22",
        encoded(keywords)
    )
}

/// One post on a content search page.
const POST_CARD: &str = r#"[role="listitem"][componentkey^="update-card-focus"]"#;

/// Reads the posts on a content search page. Each post is a list item whose texts start with the
/// poster's link (name, then degree), then the headline and the age. The post's text sits in its
/// expandable text box.
pub fn posts_page(html: &str) -> Vec<Post> {
    let page = Html::parse_document(html);
    let mut posts: Vec<Post> = Vec::new();
    for card in page.select(&selector(POST_CARD)) {
        let Some(body) = card.select(&selector("[data-testid=\"expandable-text-box\"]")).next() else {
            continue;
        };
        let person = card
            .select(&selector("a[href*=\"/in/\"]"))
            .find(|link| !strings_of(*link).is_empty());
        let poster_url = person.and_then(|link| link.attr("href")).map(profile_address);
        let actor = person.map(strings_of).unwrap_or_default();
        let poster = actor.first().cloned().unwrap_or_else(|| {
            card.select(&selector("a[href*=\"/company/\"]"))
                .map(text_of)
                .find(|name| !name.is_empty())
                .unwrap_or_default()
        });
        let degree = actor
            .iter()
            .find_map(|text| text.strip_prefix('•'))
            .map(|degree| degree.trim().to_owned());
        // The headline is the text right after the poster's link, unless the post's age comes first.
        let strings = strings_of(card);
        let headline = actor
            .last()
            .and_then(|last| strings.iter().position(|text| text == last))
            .and_then(|at| strings.get(at + 1))
            .filter(|text| !text.ends_with('•') && text.as_str() != "--")
            .cloned()
            .unwrap_or_default();
        let text = lines_of(body);
        let post = Post {
            poster,
            poster_url,
            headline,
            degree,
            complete: !text.ends_with('…'),
            text,
        };
        if !posts.iter().any(|seen| seen.link() == post.link()) {
            posts.push(post);
        }
    }
    posts
}

/// The text of an element, keeping its line breaks and leaving out its buttons.
fn lines_of(element: ElementRef<'_>) -> String {
    let mut text = String::new();
    for node in element.descendants() {
        let in_button = node
            .ancestors()
            .take_while(|ancestor| ancestor.id() != element.id())
            .any(|ancestor| matches!(ancestor.value(), Node::Element(tag) if tag.name() == "button"));
        match node.value() {
            _ if in_button => {}
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

/// What a `Reader` does on a page before it reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum First {
    Nothing,
    /// Opens the posts LinkedIn cut short. For a post's own page.
    OpenPosts,
    /// Scrolls a search page for more posts, then opens the cut-short ones.
    ScrollForPosts,
}

/// One background tab in the user's Chrome that loads LinkedIn pages, within a cap and at a person's pace.
pub struct Reader {
    page: Page,
    visits: Visits,
}

/// Presses "more" on each post LinkedIn cut short, so its full text is in the page. Most posts
/// carry their full text already; a cut-short one ends with `…`.
const EXPAND_POSTS: &str = r#"(async () => {
    for (const box of document.querySelectorAll('[data-testid="expandable-text-box"]')) {
        const more = box.querySelector('[data-testid="expandable-text-button"]');
        const text = box.textContent.replace(more?.textContent ?? "", "").trim();
        if (more && text.endsWith("…")) more.click();
    }
    await new Promise((done) => setTimeout(done, 1500));
    return true;
})()"#;

/// The most times one search page is scrolled for more posts.
const SCROLLS: usize = 3;

/// Scrolls to the last post until LinkedIn stops adding more, at most `SCROLLS` times. A search
/// page first shows only a screen of posts and loads the next ones when the last comes into view.
/// Gives how many posts the page holds in the end.
fn more_posts() -> String {
    format!(
        r#"(async () => {{
    const posts = () => document.querySelectorAll('{POST_CARD}');
    for (let scroll = 0; scroll < {SCROLLS}; scroll++) {{
        const before = posts().length;
        if (before === 0) break;
        posts()[before - 1].scrollIntoView({{ block: "start" }});
        const until = Date.now() + 5000;
        while (posts().length === before && Date.now() < until) {{
            await new Promise((done) => setTimeout(done, 500));
        }}
        if (posts().length === before) break;
    }}
    return posts().length;
}})()"#
    )
}

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

    /// Loads a page after the pacing wait, does what `first` says, and gives the page's HTML.
    pub async fn load(&mut self, address: &str, first: First) -> Result<String, Stop> {
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
        if first == First::ScrollForPosts {
            self.page.evaluate(&more_posts()).await?;
        }
        if first != First::Nothing {
            self.page.evaluate(EXPAND_POSTS).await?;
        }
        let html = self.page.evaluate("document.documentElement.outerHTML").await?;
        Ok(html.as_str().unwrap_or_default().to_owned())
    }

    /// Opens the profile and its message window, and gives what the window says an InMail costs.
    /// `None` when the window says nothing about it. Nothing is typed there.
    pub async fn inmail_cost(&mut self, profile_url: &str) -> Result<Option<InMailCost>, Stop> {
        Ok(inmail_cost(&self.message_window(profile_url).await?))
    }

    /// Opens the profile and its message window, and gives the window's text. Nothing is typed there.
    pub async fn message_window(&mut self, profile_url: &str) -> Result<String, Stop> {
        self.load(profile_url, First::Nothing).await?;
        self.page.wait_for(&Control::Message.locator(), STEP_WAIT).await?;
        self.page.click(&Control::Message.locator()).await?;
        let window = Locator::script("message window", compose_text());
        self.page.wait_for(&window, STEP_WAIT).await?;
        // The credits line is drawn a few seconds after the window.
        let mut text = String::new();
        for _ in 0..8 {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let shown = self.page.evaluate(&compose_text()).await?;
            text = shown.as_str().unwrap_or_default().to_owned();
            if inmail_cost(&text).is_some() {
                break;
            }
        }
        Ok(text)
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
    /// A Premium InMail to someone outside the network: a subject and a body. Sending one uses a credit.
    #[serde(alias = "inmail")]
    InMail,
}

impl Slot {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Note => "note",
            Self::Message => "message",
            Self::InMail => "inmail",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "note" => Some(Self::Note),
            "message" => Some(Self::Message),
            "inmail" => Some(Self::InMail),
            _ => None,
        }
    }

    /// The other way to reach someone outside the network: a note becomes an InMail and back.
    /// A connection gets a message, which has no other form.
    pub fn switched(self) -> Option<Self> {
        match self {
            Self::Note => Some(Self::InMail),
            Self::InMail => Some(Self::Note),
            Self::Message => None,
        }
    }

    /// A connection gets a message. Anyone else gets an InMail while a credit is free, and a
    /// connection note otherwise.
    pub fn to_reach(degree: Option<&str>, inmail_credit: bool) -> Self {
        match degree {
            Some("1st") => Self::Message,
            _ if inmail_credit => Self::InMail,
            _ => Self::Note,
        }
    }
}

/// The longest connection note LinkedIn takes.
pub const NOTE_LIMIT: usize = 300;
/// The longest InMail subject Maverick writes; LinkedIn takes a little more.
pub const INMAIL_SUBJECT_LIMIT: usize = 200;
/// The longest InMail body LinkedIn takes.
pub const INMAIL_BODY_LIMIT: usize = 1900;

/// The only things the typing flow ever clicks. There is no Send here, on purpose: only
/// `send_draft` presses Send, and the user clicks Send on every draft typed for them.
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
        // The profile's top card is the first profile card; other people's Connect and Message
        // controls fill the side column, outside it. The card draws each control twice, once per
        // layout, so only a control that takes up room on the screen counts.
        let in_card = |test: &str| {
            format!(
                r#"(() => {{
                    const card = document.querySelector('[componentkey^="com.linkedin.sdui.profile.card"]')
                        || document.querySelector("main section");
                    if (!card) return null;
                    return [...card.querySelectorAll("button, a")].find((control) => {{
                        if (!control.getClientRects().length) return false;
                        const label = (control.getAttribute("aria-label") || "").trim();
                        const text = control.innerText.trim();
                        const href = control.getAttribute("href") || "";
                        return {test};
                    }}) || null;
                }})()"#
            )
        };
        let (name, script) = match self {
            Control::Message => (
                "Message button",
                in_card(r#"href.includes("/messaging/compose") || /^Message\b/i.test(label) || text === "Message""#),
            ),
            Control::Connect => (
                "Connect button",
                in_card(r#"/^Invite .* to connect$/i.test(label) || text === "Connect""#),
            ),
            Control::MoreActions => (
                "More button",
                in_card(r#"label === "More" || label === "More actions" || text === "More""#),
            ),
            Control::ConnectInMenu => (
                "Connect in the More menu",
                r#"[...document.querySelectorAll("[role='menu'], .artdeco-dropdown__content")]
                    .flatMap((menu) => [...menu.querySelectorAll("[role='menuitem'], [role='button'], button, a")])
                    .find((item) => item.getClientRects().length
                        && (/^Invite .* to connect$/i.test(item.getAttribute("aria-label") || "")
                            || item.innerText.trim() === "Connect")) || null"#
                    .to_owned(),
            ),
            Control::AddNote => (
                "Add a note button",
                format!(
                    r#"[...document.querySelectorAll("{OPEN_DIALOG}")]
                        .flatMap((dialog) => [...dialog.querySelectorAll("button, a, [role='button']")])
                        .find((control) => control.getClientRects().length
                            && (control.getAttribute("aria-label") || control.innerText).trim() === "Add a note") || null"#
                ),
            ),
        };
        Locator::script(name, script)
    }
}

/// LinkedIn's dialogs are native `<dialog open>` elements now; older pages used `role="dialog"`.
/// Hidden video-player dialogs carry `aria-hidden`, so they never count.
const OPEN_DIALOG: &str = "dialog[open], [role='dialog']:not([aria-hidden='true'])";
const NOTE_FIELD: &str = "document.querySelector(\"dialog[open] textarea, [role='dialog'] textarea[name='message'], \
                          textarea#custom-message, dialog[open] [contenteditable='true']\")";
/// The newest open message form. LinkedIn draws its message windows inside a shadow root, which a
/// query on the document does not reach, so this looks in the document and in that root.
const MESSAGE_FORM: &str = r#"[document, ...[...document.querySelectorAll('[data-testid="interop-shadowdom"]')]
        .map((host) => host.shadowRoot).filter(Boolean)]
    .flatMap((root) => [...root.querySelectorAll(".msg-form")])
    .filter((form) => form.getClientRects().length > 0)
    .pop()"#;

/// A script that gives one part of the newest open message form, or `null`.
fn in_message_form(part: &str) -> String {
    format!("(() => {{ const form = {MESSAGE_FORM}; return form ? ({part}) : null; }})()")
}

/// The box a message or an InMail body is typed in.
fn message_field() -> String {
    in_message_form(r#"form.querySelector("[contenteditable='true']")"#)
}

/// The subject field, which only an InMail has.
fn inmail_subject_field() -> String {
    in_message_form(r#"form.querySelector("input[name='subject']")"#)
}

/// The file input that takes a document. The one before it takes images only.
fn attachment_field() -> String {
    in_message_form(
        r#"[...form.querySelectorAll("input[type='file']")].find((input) => input.accept.includes(".pdf")) || null"#,
    )
}

/// The text of the window the newest message form is in, or `null` while none is open.
fn compose_text() -> String {
    in_message_form(r#"(form.closest(".msg-overlay-conversation-bubble") || form).innerText"#)
}

/// The InMail credits a message window says are left, as in "Use 1 of 3 InMail credits".
pub fn credits_in(compose: &str) -> Option<u32> {
    let words: Vec<&str> = compose.split_whitespace().collect();
    words
        .windows(3)
        .find(|part| part[1] == "InMail" && part[2].starts_with("credit"))
        .and_then(|part| part[0].parse().ok())
}

/// What a message window says an InMail to its person costs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InMailCost {
    /// The person takes messages from anyone, so no credit is used.
    Free,
    /// One credit, of this many left.
    Credit { left: u32 },
}

pub fn inmail_cost(compose: &str) -> Option<InMailCost> {
    if compose.contains("Free message") {
        return Some(InMailCost::Free);
    }
    credits_in(compose).map(|left| InMailCost::Credit { left })
}

/// Whether the user's resume went into a typed message.
#[derive(Clone, Debug, PartialEq)]
pub enum Resume {
    /// A connection note takes no file, or no resume was given.
    NotAsked,
    Attached,
    /// The message is typed, but the file did not go in.
    NotAttached(String),
}

/// What typing a draft left in LinkedIn.
#[derive(Clone, Debug, PartialEq)]
pub struct Typed {
    pub resume: Resume,
    /// The InMail credits the message window said are left.
    pub credits: Option<u32>,
}

impl Typed {
    /// What to tell the user to do next.
    pub fn said(&self) -> String {
        match &self.resume {
            Resume::NotAsked => "Typed in Chrome. Read it there and click Send yourself, then mark it sent.".into(),
            Resume::Attached => "Typed in Chrome with your resume attached. Read it there and click Send yourself, \
                                 then mark it sent."
                .into(),
            Resume::NotAttached(why) => format!(
                "Typed in Chrome, but your resume was not attached: {why}. Attach it there if you want it, \
                 click Send yourself, then mark it sent."
            ),
        }
    }
}

/// LinkedIn asks for the person's email before some connection requests. Maverick never invents one.
const EMAIL_WALL: &str = "document.querySelector(\"dialog[open] input[type='email'], dialog[open] input[name='email'], \
     [role='dialog'] input[name='email'], [role='dialog'] input[type='email']\")";

const STEP_WAIT: Duration = Duration::from_secs(10);
/// How long LinkedIn takes, after the load event, to make its buttons work.
const SETTLE: Duration = Duration::from_secs(3);

/// Opens the person's profile in the user's Chrome, opens the message or connection dialog,
/// and types the approved draft. A message or an InMail also gets `resume` attached.
/// It stops there: the user reviews it and clicks Send.
pub async fn type_draft(
    bridge: Arc<Bridge>,
    profile_url: &str,
    slot: Slot,
    subject: Option<&str>,
    text: &str,
    resume: Option<&Path>,
) -> Result<Typed, String> {
    let page = open_profile_tab(bridge, profile_url).await?;
    let typed = match land(&page, profile_url).await {
        Ok(()) => type_in(&page, slot, subject, text, resume).await,
        Err(why) => Err(why),
    };
    // The debugging bar goes away; the dialog and the typed draft stay for the user.
    let _ = page.detach().await;
    typed
}

/// A new, active tab in the user's Chrome with the debugger attached, for typing to `profile_url`.
async fn open_profile_tab(bridge: Arc<Bridge>, profile_url: &str) -> Result<Page, String> {
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
    Page::attach(bridge, &tab).await.map_err(|error| error.to_string())
}

/// What pressing Send led to.
#[derive(Clone, Debug, PartialEq)]
pub enum Delivery {
    /// The page showed the draft as sent.
    Sent,
    /// Send was pressed, but the page did not show the draft as sent. The tab stays open.
    Unconfirmed(String),
    /// Nothing was typed or sent: the profile's location, given here, is outside the US.
    OutsideUs(String),
}

/// How long LinkedIn may take to show a sent draft, with an attached resume still uploading.
const SEND_WAIT: Duration = Duration::from_secs(30);

/// Types a draft like `type_draft`, presses Send, and checks the page shows it sent. A person whose
/// profile is outside the US gets nothing. A sent draft's
/// tab closes; any other stays open for the user. An error means Send was never pressed.
pub async fn send_draft(
    bridge: Arc<Bridge>,
    profile_url: &str,
    slot: Slot,
    subject: Option<&str>,
    text: &str,
    resume: Option<&Path>,
) -> Result<(Typed, Delivery), String> {
    let page = open_profile_tab(bridge, profile_url).await?;
    let sent = async {
        land(&page, profile_url).await?;
        let location = page
            .evaluate(PROFILE_LOCATION)
            .await
            .map_err(|error| error.to_string())?;
        let location = location
            .as_str()
            .ok_or("the profile shows no location, so Maverick could not tell whether this person is in the US")?;
        if !is_in_us(location) {
            let typed = Typed {
                resume: Resume::NotAsked,
                credits: None,
            };
            return Ok((typed, Delivery::OutsideUs(location.to_owned())));
        }
        let typed = type_in(&page, slot, subject, text, resume).await?;
        // The Send button stays disabled while the resume uploads.
        let send = send_button(slot);
        page.wait_for(&send, SEND_WAIT)
            .await
            .map_err(|error| format!("{error}. The draft is typed in Chrome; nothing was sent"))?;
        page.click(&send)
            .await
            .map_err(|error| format!("{error}. The draft is typed in Chrome; nothing was sent"))?;
        Ok::<_, String>((typed, confirm_sent(&page, slot, text).await))
    }
    .await;
    if let Ok((_, Delivery::Sent | Delivery::OutsideUs(_))) = &sent {
        let _ = page.send("Page.close", json!({})).await;
    }
    let _ = page.detach().await;
    sent
}

/// The Send button that goes with a typed draft, once it can be pressed.
fn send_button(slot: Slot) -> Locator {
    const ENABLED: &str = r#"(button) => button.getClientRects().length && !button.disabled
        && button.getAttribute("aria-disabled") !== "true""#;
    match slot {
        Slot::Note => Locator::script(
            "the invitation's Send button",
            format!(
                r#"[...document.querySelectorAll("{OPEN_DIALOG}")]
                    .flatMap((dialog) => [...dialog.querySelectorAll("button")])
                    .filter({ENABLED})
                    .find((button) => /^send( invitation| now)?$/i
                        .test((button.getAttribute("aria-label") || button.innerText).trim())) || null"#
            ),
        ),
        Slot::Message | Slot::InMail => Locator::script(
            "the message window's Send button",
            in_message_form(&format!(
                r#"[...form.querySelectorAll("button")]
                    .filter({ENABLED})
                    .find((button) => button.classList.contains("msg-form__send-button")
                        || /^send$/i.test((button.getAttribute("aria-label") || button.innerText).trim())) || null"#
            )),
        ),
    }
}

/// The start of a draft, with its spaces collapsed, which the page shows once the draft is sent.
fn opening(text: &str) -> String {
    let words = text.split_whitespace().collect::<Vec<_>>().join(" ");
    words.chars().take(60).collect()
}

/// A script that says `"sent"` once the page shows the draft as sent, `"limit"` when LinkedIn
/// refuses more invitations, and `"waiting"` before either.
fn sent_check(slot: Slot, text: &str) -> String {
    const LIMIT: &str =
        r#"/weekly invitation limit|reached the weekly limit|out of invitations/i.test(document.body.innerText)"#;
    match slot {
        Slot::Note => format!(
            r#"(() => {{
                if ({LIMIT}) return "limit";
                return {NOTE_FIELD} ? "waiting" : "sent";
            }})()"#
        ),
        Slot::Message | Slot::InMail => {
            let opening = serde_json::to_string(&opening(text)).expect("a string is plain JSON");
            format!(
                r#"(() => {{
                    const form = {MESSAGE_FORM};
                    if (!form) return "waiting";
                    const field = form.querySelector("[contenteditable='true']");
                    if (field && field.innerText.trim() !== "") return "waiting";
                    const root = form.getRootNode();
                    const shown = (root === document ? document.body.innerText
                        : [...root.children].map((part) => part.innerText || "").join(" ")).replace(/\s+/g, " ");
                    return shown.includes({opening}) ? "sent" : "waiting";
                }})()"#
            )
        }
    }
}

async fn confirm_sent(page: &Page, slot: Slot, text: &str) -> Delivery {
    let check = sent_check(slot, text);
    let deadline = tokio::time::Instant::now() + SEND_WAIT;
    loop {
        match page.evaluate(&check).await.as_ref().map(|said| said.as_str()) {
            Ok(Some("sent")) => return Delivery::Sent,
            Ok(Some("limit")) => {
                return Delivery::Unconfirmed("LinkedIn says the weekly invitation limit is reached".into());
            }
            Err(error) => return Delivery::Unconfirmed(format!("the page could not be read after Send: {error}")),
            _ => {}
        }
        if tokio::time::Instant::now() >= deadline {
            return Delivery::Unconfirmed(
                "Send was pressed, but the page never showed the draft as sent; check the tab in Chrome".into(),
            );
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Puts the resume into the open message form. The message stays typed when this fails.
async fn attach(page: &Page, resume: Option<&Path>) -> Resume {
    let Some(resume) = resume else {
        return Resume::NotAsked;
    };
    if !resume.is_file() {
        return Resume::NotAttached(format!("{} is not a file", resume.display()));
    }
    let field = Locator::script("attach a file", attachment_field());
    match page.upload(&field, &[resume]).await {
        Ok(()) => Resume::Attached,
        Err(error) => Resume::NotAttached(error.to_string()),
    }
}

/// Loads the profile and waits for LinkedIn to make its buttons work.
async fn land(page: &Page, profile_url: &str) -> Result<(), String> {
    let said = |error: PageError| error.to_string();
    page.navigate(profile_url).await.map_err(said)?;
    let landed = page.evaluate("location.href").await.map_err(said)?;
    if is_wall(landed.as_str().unwrap_or_default()) {
        return Err("LinkedIn is not signed in in this Chrome".into());
    }
    // LinkedIn wires its buttons up after the load event. Clicked before that, Connect is a plain
    // link back to the profile, so the click reloads the page and no dialog opens.
    tokio::time::sleep(SETTLE).await;
    Ok(())
}

/// The location on the profile's top card, the text next to "Contact info", such as
/// `Austin, Texas, United States`.
const PROFILE_LOCATION: &str = r#"(() => {
    const contact = document.querySelector("a[href*='/overlay/contact-info']");
    for (let box = contact && contact.parentElement; box && box !== document.body; box = box.parentElement) {
        const text = box.innerText.replace(/Contact info/g, "").replace(/[·•]/g, " ").replace(/\s+/g, " ").trim();
        if (text) return text;
    }
    return null;
})()"#;

/// US states and the District of Columbia, in lower case.
const US_STATES: &[&str] = &[
    "alabama",
    "alaska",
    "arizona",
    "arkansas",
    "california",
    "colorado",
    "connecticut",
    "delaware",
    "district of columbia",
    "florida",
    "georgia",
    "hawaii",
    "idaho",
    "illinois",
    "indiana",
    "iowa",
    "kansas",
    "kentucky",
    "louisiana",
    "maine",
    "maryland",
    "massachusetts",
    "michigan",
    "minnesota",
    "mississippi",
    "missouri",
    "montana",
    "nebraska",
    "nevada",
    "new hampshire",
    "new jersey",
    "new mexico",
    "new york",
    "north carolina",
    "north dakota",
    "ohio",
    "oklahoma",
    "oregon",
    "pennsylvania",
    "rhode island",
    "south carolina",
    "south dakota",
    "tennessee",
    "texas",
    "utah",
    "vermont",
    "virginia",
    "washington",
    "west virginia",
    "wisconsin",
    "wyoming",
];

/// Cities in the names of LinkedIn's US metro areas, such as `San Francisco Bay Area` or
/// `Greater Seattle Area`, which name no state.
const US_METROS: &[&str] = &[
    "san francisco",
    "bay area",
    "silicon valley",
    "san jose",
    "seattle",
    "new york",
    "boston",
    "los angeles",
    "chicago",
    "dallas",
    "fort worth",
    "washington dc",
    "baltimore",
    "atlanta",
    "houston",
    "denver",
    "austin",
    "philadelphia",
    "miami",
    "minneapolis",
    "phoenix",
    "salt lake city",
    "raleigh",
    "durham",
    "san diego",
    "sacramento",
    "detroit",
    "pittsburgh",
    "st. louis",
    "nashville",
    "orlando",
    "charlotte",
    "kansas city",
    "cleveland",
    "tampa",
    "las vegas",
    "indianapolis",
    "cincinnati",
    "milwaukee",
    "new orleans",
    "boise",
    "honolulu",
    "omaha",
    "albuquerque",
    "louisville",
    "memphis",
    "jacksonville",
    "provo",
    "san antonio",
];

/// True when a profile's location is in the United States. A location that names another country
/// last, such as `Toronto, Ontario, Canada`, is not.
pub fn is_in_us(location: &str) -> bool {
    let lower = location.to_lowercase();
    if lower.contains("united states") || lower.split(',').any(|part| part.trim() == "us" || part.trim() == "usa") {
        return true;
    }
    let words: Vec<&str> = lower
        .split(|c: char| !c.is_alphanumeric() && c != '.')
        .filter(|word| !word.is_empty())
        .collect();
    let joined = format!(" {} ", words.join(" "));
    let names = |list: &[&str]| list.iter().any(|name| joined.contains(&format!(" {name} ")));
    // `Tbilisi, Georgia` names the country; LinkedIn adds `United States` after a US state.
    let last = lower.rsplit(',').next().unwrap_or_default().trim().to_owned();
    if lower.contains(',') && last == "georgia" {
        return false;
    }
    names(US_STATES) || names(US_METROS)
}

async fn type_in(
    page: &Page,
    slot: Slot,
    subject: Option<&str>,
    text: &str,
    resume: Option<&Path>,
) -> Result<Typed, String> {
    let said = |error: PageError| error.to_string();
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
            let field = Locator::script("message box", message_field());
            page.wait_for(&field, STEP_WAIT).await.map_err(said)?;
            page.fill(&field, text).await.map_err(said)?;
            return Ok(Typed {
                resume: attach(page, resume).await,
                credits: None,
            });
        }
        Slot::Note => {
            if let Ok(connect) = found(Control::Connect).await {
                page.click(&connect.locator()).await.map_err(said)?;
                let opened = Locator::script("an open dialog", format!("document.querySelector(\"{OPEN_DIALOG}\")"));
                if page.wait_for(&opened, SETTLE).await.is_err() {
                    // The click went to the link, not the dialog. Connect never sends by itself:
                    // the dialog's own Send buttons do, and Maverick never presses those.
                    tokio::time::sleep(SETTLE).await;
                    let again = found(Control::Connect).await.map_err(|_| {
                        "Connect opened no dialog and is gone now; check the profile in Chrome".to_owned()
                    })?;
                    page.click(&again.locator()).await.map_err(said)?;
                }
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
        Slot::InMail => {
            let subject = subject.ok_or("the InMail has no subject")?;
            // Each step names itself, so a failure says which part of the page to look at with `dump-tab`.
            let step = |number: u8, part: &'static str| {
                move |error: PageError| {
                    format!(
                        "InMail step {number} of 4, {part}: {error}. Nothing was sent. \
                         Save the tab with `maverick linkedin dump-tab <out.html>` so the step can be fixed"
                    )
                }
            };
            page.wait_for(&Control::Message.locator(), STEP_WAIT)
                .await
                .map_err(step(1, "the top card's Message button"))?;
            page.click(&Control::Message.locator())
                .await
                .map_err(step(1, "the top card's Message button"))?;
            let subject_field = Locator::script("InMail subject field", inmail_subject_field());
            page.wait_for(&subject_field, STEP_WAIT)
                .await
                .map_err(step(2, "the InMail window with a subject field"))?;
            page.fill(&subject_field, subject)
                .await
                .map_err(step(3, "the InMail subject field"))?;
            let body = Locator::script("InMail body field", message_field());
            page.wait_for(&body, STEP_WAIT)
                .await
                .map_err(step(4, "the InMail body field"))?;
            page.fill(&body, text).await.map_err(step(4, "the InMail body field"))?;
            let window = page.evaluate(&compose_text()).await.map_err(said)?;
            return Ok(Typed {
                resume: attach(page, resume).await,
                credits: credits_in(window.as_str().unwrap_or_default()),
            });
        }
    };
    page.wait_for(&field, STEP_WAIT).await.map_err(said)?;
    page.fill(&field, text).await.map_err(said)?;
    Ok(Typed {
        resume: Resume::NotAsked,
        credits: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_profiles_located_in_the_us_count_as_in_the_us() {
        for here in [
            "Austin, Texas, United States",
            "San Francisco Bay Area",
            "Greater Seattle Area",
            "New York City Metropolitan Area",
            "Washington DC-Baltimore Area",
            "Atlanta, Georgia, United States",
            "Portland, Oregon Metropolitan Area",
            "United States",
        ] {
            assert!(is_in_us(here), "{here}");
        }
        for abroad in [
            "Bengaluru, Karnataka, India",
            "Toronto, Ontario, Canada",
            "London Area, United Kingdom",
            "Greater Sydney Area",
            "Tbilisi, Georgia",
            "Mexico City, Mexico",
            "Hyderabad, Telangana, India",
        ] {
            assert!(!is_in_us(abroad), "{abroad}");
        }
    }

    const CONNECTIONS: &str = "Notes:\n\
\"When exporting your connection data, you may notice that some of the email addresses are missing.\"\n\
\n\
First Name,Last Name,URL,Email Address,Company,Position,Connected On\n\
Ada,Example,https://www.linkedin.com/in/ada-example-1a2b3c,,Example Co,\"Software Engineer, Storage\",15 Nov 2026\n\
Bo,Sample,https://www.linkedin.com/in/bo-sample?trk=x,bo@sample.test,Sample Inc,\"Recruiter, \"\"Tech\"\"\",02 Jan 2025\n\
Cy,Nolink,,,Other LLC,Engineer,\n";

    #[test]
    fn a_name_fits_with_extra_words_and_initials_but_not_another_first_name() {
        let person = |name: &str| Person {
            name: name.into(),
            title: String::new(),
            profile_url: Some(format!("https://www.linkedin.com/in/{}/", name.replace(' ', "-"))),
            degree: None,
        };
        let people = [
            person("Ashley Sharma"),
            person("Ashish Kumar Sharma, PhD"),
            person("Felix Schäfer"),
            person("Yao Lu"),
        ];
        let found = |wanted: &str| named_in(&people, wanted).map(|person| person.name);
        assert_eq!(found("Ashish S.").as_deref(), Some("Ashish Kumar Sharma, PhD"));
        assert_eq!(found("Ashish Sharma").as_deref(), Some("Ashish Kumar Sharma, PhD"));
        assert_eq!(found("felix schäfer").as_deref(), Some("Felix Schäfer"));
        assert_eq!(found("Yao (Jason) Lu").as_deref(), Some("Yao Lu"));
        assert_eq!(found("Sharma"), None);
        assert_eq!(found("Ashish Patel"), None);
        assert_eq!(
            people_search_address("Felix Schäfer Clera"),
            "https://www.linkedin.com/search/results/people/?keywords=Felix%20Sch%C3%A4fer%20Clera"
        );
    }

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

    /// Made up, shaped like the real page on 2026-11-23: hashed classes, cards as list items,
    /// the company only in the title and an h2, and an alumni list after the main one.
    const PEOPLE: &str = r#"<html><head><title>Example Co: People | LinkedIn</title></head><body><main>
<h2 class="cfsq1">Example Co</h2>
<h2 class="cfsq1">1,204 associated members</h2>
<h3 class="cfsz9">How you are connected</h3>
<div class="cfsa1 cfsb2">
 <div role="listitem" class="cfsk3" componentkey="auto-component-1">
  <a href="https://www.linkedin.com/in/ada-example-1a2b3c/?miniProfileUrn=x" class="cfsp0"><img alt="Ada Example’s profile picture" class="cfsi1"></a>
  <a href="https://www.linkedin.com/in/ada-example-1a2b3c/" class="cfsn2"><span class="cfsn3">Ada Example</span></a>
  <span aria-hidden="true" class="cfsd4">· 2nd</span>
  <div class="cfsh5"><a href="https://www.linkedin.com/in/ada-example-1a2b3c/" class="cfsh6">Software Engineer, Storage Infrastructure</a></div>
  <p class="cfsm7">Bo Sample is a mutual connection</p>
  <button class="cfsb8" aria-label="Invite Ada Example to connect"><span>Connect</span></button>
 </div>
 <div role="listitem" class="cfsk3" componentkey="auto-component-2">
  <a href="https://www.linkedin.com/in/cy-person/" class="cfsn2"><span>Cy Person</span></a>
  <span aria-hidden="true" class="cfsd4">· 3rd</span>
  <div class="cfsh5">Head of Marketing</div>
  <p class="cfsm7">Based on your profile</p>
  <button class="cfsb8"><span>Follow</span></button>
 </div>
 <div role="listitem" class="cfsk3" componentkey="auto-component-3">
  <a href="https://www.linkedin.com/in/ada-example-1a2b3c/" class="cfsn2"><span>Ada Example</span></a>
 </div>
 <div role="listitem" class="cfsk3" componentkey="auto-component-4"><span>People highlights</span></div>
</div>
<h2 class="cfsq1">People also viewed</h2>
<div class="cfsa1">
 <div role="listitem" class="cfsk3"><a href="https://www.linkedin.com/company/other-co/"><span>Other Co</span></a><p>Software Development</p><p>297 followers</p>
  <p><span><a href="https://www.linkedin.com/in/fay-friend/">1 person in your network works here</a></span></p><button>Follow</button></div>
</div>
<h2 class="cfsq1">Alumni who work here</h2>
<div class="cfsa1">
 <div role="listitem" class="cfsk3"><a href="https://www.linkedin.com/in/di-alum/"><span>Di Alum</span></a><span>· 2nd</span><div>Engineer</div><button>Connect</button></div>
</div>
</main></body></html>"#;

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
                    name: "Cy Person".into(),
                    title: "Head of Marketing".into(),
                    profile_url: Some("https://www.linkedin.com/in/cy-person/".into()),
                    degree: Some("3rd".into()),
                },
            ]
        );
        assert_eq!(
            people_address("example-co"),
            "https://www.linkedin.com/company/example-co/people/"
        );
        // A wrong guess at the address lands on a page that is not a company's People page.
        let missing = "<html><head><title>(2) Page not found | LinkedIn</title></head><body></body></html>";
        assert_eq!(people_page(missing).company, None);
    }

    #[test]
    fn a_page_names_the_company_page_it_links_most() {
        let footer = r#"<a href="https://www.linkedin.com/company/linkedin/">us</a>
            <a href="https://twitter.com/x">x</a>
            <a href="https://www.linkedin.com/company/example-co-ai/">LinkedIn</a>
            <a href="https://linkedin.com/company/Example-Co-AI?trk=footer">LinkedIn</a>
            <a href="https://www.linkedin.com/company/partner-co/">partner</a>"#;
        assert_eq!(company_pages_in(footer), ["example-co-ai", "partner-co"]);
        assert!(company_pages_in("<p>no links</p>").is_empty());
        let ashby = r#"{"organization":{"name":"Example Co","publicWebsite":"https://example.com/","x":1}}"#;
        assert_eq!(company_website_in(ashby).as_deref(), Some("https://example.com/"));
        assert_eq!(company_website_in(r#"{"publicWebsite":""}"#), None);
    }

    /// Made up, shaped like the real search page on 2026-11-23: each post a list item keyed
    /// `update-card-focus…`, no post id in the card, the text in an expandable box.
    const POSTS: &str = r#"<html><head><title>Search | LinkedIn</title></head><body><main>
<div role="listitem" class="cfsx1" componentkey="update-card-focusAbCdEf0123FeedType_FLAGSHIP_SEARCH">
  <h2 class="cfsv0">Feed post</h2>
  <a href="https://www.linkedin.com/in/sam-poster/?miniProfileUrn=x" aria-label="Sam Poster Verified Premium 2nd"><img alt=""></a>
  <a href="https://www.linkedin.com/in/sam-poster/"><span class="cfsa2">Sam Poster</span><span class="cfsa3">• 2nd</span></a>
  <p class="cfsa4">Engineering Manager at Example Co</p>
  <p class="cfsa5">2h •</p>
  <button class="cfsf6">Follow</button>
  <div class="cfsb7"><span data-testid="expandable-text-box" class="cfsb8">We're hiring new grad backend engineers!<br>Team: Storage.   Location: Austin, TX<br><br>Apply below.<button data-testid="expandable-text-button" aria-hidden="true">… more</button></span></div>
</div>
<div role="listitem" class="cfsx1" componentkey="update-card-focusZz99FeedType_FLAGSHIP_SEARCH">
  <h2 class="cfsv0">Feed post</h2>
  <a href="https://www.linkedin.com/company/jobs-digest/"><span>Jobs Digest</span></a>
  <p class="cfsa5">21h •</p>
  <span data-testid="expandable-text-box">Top 50 companies hiring this week, and the first ten are…<button data-testid="expandable-text-button">… more</button></span>
</div>
<div role="listitem" class="cfsx1" componentkey="update-card-focusQq11FeedType_FLAGSHIP_SEARCH">
  <h2 class="cfsv0">Feed post</h2>
  <a href="https://www.linkedin.com/in/sam-poster/"><span>Sam Poster</span><span>• 2nd</span></a>
  <p>Engineering Manager at Example Co</p>
  <p>2h •</p>
  <span data-testid="expandable-text-box">We're hiring new grad backend engineers!<br>Team: Storage.   Location: Austin, TX<br><br>Apply below.<button data-testid="expandable-text-button">… more</button></span>
</div>
<div role="listitem" class="cfsx1" componentkey="update-card-focusRr22FeedType_FLAGSHIP_SEARCH">
  <h2 class="cfsv0">Feed post</h2>
  <a href="https://www.linkedin.com/in/eve-new/"><span>Eve New</span><span>• 3rd+</span></a>
  <p>--</p>
  <p>7h •</p>
  <span data-testid="expandable-text-box">Hiring a platform engineer.<button data-testid="expandable-text-button">… more</button></span>
</div>
</main></body></html>"#;

    #[test]
    fn a_search_page_gives_each_post_once_with_its_author_and_full_text() {
        let posts = posts_page(POSTS);
        assert_eq!(posts.len(), 3);
        assert_eq!(
            posts[0],
            Post {
                poster: "Sam Poster".into(),
                poster_url: Some("https://www.linkedin.com/in/sam-poster/".into()),
                headline: "Engineering Manager at Example Co".into(),
                degree: Some("2nd".into()),
                text: "We're hiring new grad backend engineers!\nTeam: Storage. Location: Austin, TX\n\nApply below."
                    .into(),
                complete: true,
            }
        );
        assert!(!posts[0].is_connection());
        assert_eq!(
            (
                posts[1].poster.as_str(),
                posts[1].poster_url.as_deref(),
                posts[1].complete
            ),
            ("Jobs Digest", None, false)
        );
        assert_eq!(
            (posts[2].headline.as_str(), posts[2].degree.as_deref()),
            ("", Some("3rd+"))
        );
        // The same post gives the same link on every page; another post by the same poster does not.
        let link = posts[0].link();
        assert!(link.starts_with("https://www.linkedin.com/in/sam-poster/recent-activity/all/#post-"));
        assert_eq!(link, posts_page(POSTS)[0].link());
        let other = Post {
            text: "We're hiring interns too.".into(),
            ..posts[0].clone()
        };
        assert_ne!(other.link(), link);
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
    fn a_message_window_says_how_many_inmail_credits_are_left() {
        let window = "Sam Poster\nEngineer at Example Co\nPremium\nUse 1 of 3 InMail credits\nSubject (optional)";
        assert_eq!(credits_in(window), Some(3));
        assert_eq!(credits_in("You have 0 InMail credits"), Some(0));
        assert_eq!(credits_in("Use 1 of 1 InMail credit"), Some(1));
        assert_eq!(credits_in("Sam Poster\nWrite a message…"), None);
        assert_eq!(inmail_cost(window), Some(InMailCost::Credit { left: 3 }));
        assert_eq!(
            inmail_cost("Sam Poster\nPremium InMail composer\nFree message | Why?"),
            Some(InMailCost::Free)
        );
        assert_eq!(inmail_cost("Sam Poster\nWrite a message…"), None);
    }

    #[test]
    fn nothing_the_typing_flow_clicks_is_a_send_button() {
        for control in Control::ALL {
            let locator = control.locator();
            let script = locator.as_script().to_lowercase();
            assert!(!script.contains("send"), "{control:?} could reach a Send button");
            assert!(!script.contains("submit"), "{control:?} could reach a submit button");
        }
        for script in [
            message_field(),
            inmail_subject_field(),
            attachment_field(),
            compose_text(),
        ] {
            let script = script.to_lowercase();
            assert!(!script.contains("send") && !script.contains("submit") && !script.contains("click"));
        }
        assert_eq!(Slot::to_reach(Some("1st"), true), Slot::Message);
        assert_eq!(Slot::to_reach(Some("2nd"), true), Slot::InMail);
        assert_eq!(Slot::to_reach(Some("3rd+"), false), Slot::Note);
        assert_eq!(Slot::to_reach(None, false), Slot::Note);
        for slot in [Slot::Note, Slot::Message, Slot::InMail] {
            assert_eq!(Slot::parse(slot.as_str()), Some(slot));
        }
        assert_eq!(Slot::parse("InMail"), None);
        assert_eq!(Slot::Note.switched(), Some(Slot::InMail));
        assert_eq!(Slot::InMail.switched(), Some(Slot::Note));
        assert_eq!(Slot::Message.switched(), None);
    }
}
