//! Fills a Workday application page after page, with "Save and Continue" between them, and stops
//! at the page that asks to submit. It never submits.
//!
//! Most employers show a Workday application only to an account on their own Workday site. When
//! the user has stored one Workday sign-in in the Keychain, this code signs in with it, and
//! creates the account at an employer where the sign-in is refused. See `pass_wall`.
//!
//! The parts of this code stand on different ground:
//!
//! - Checked against live applications of two employers, signed in, in November 2026: the sign-in
//!   with a stored sign-in, the page "My Information", the search lists, "Save and Continue", and
//!   the list of errors Workday shows when it does not take a page.
//! - Checked against one of them: the page "My Experience" with its list of jobs, its month and
//!   year boxes, the school, the degree, the field of study and the resume, and the page
//!   "Application Questions" with its drop-down lists, the questions that appear after an answer,
//!   and a date with a day.
//! - Checked against the sign-in walls of 19 employers, with made-up values and WITHOUT pressing
//!   Sign In or Create Account: the markup of the sign-in form, the create-account form and the
//!   chooser, moving between them, typing into them and reading the text back, ticking the
//!   consent box, and which element a click on a submit button lands on.
//! - NOT checked against a live site: creating an account, what the wall shows after a refused
//!   press, the pages "Voluntary Disclosures" and "Self Identify", the last page, a search list
//!   whose options open lists of their own, groups of check boxes, and `SUBMITTED`. These rest
//!   on Workday's own public page scripts (`candidate-experience-jobs/cx-jobs.min.js` and
//!   `candidate-experience-apply-flow/candidate-experience-apply-flow.min.js`, release 2026.47.19)
//!   and the atsfill project's notes (<https://shbernal.github.io/atsfill/adapters/workday/>).
//!   The tests run every part against a reconstruction of the markup, not against Workday.
//!
//! Each fill of an application page saves the page's structure to a file, so the next version of
//! this code can be built on real markup. See `capture`.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use jiff::Timestamp;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::answers::{Answer, Answers, Kind, Question, Role, SchoolPart, WorkPart};
use crate::apply::{
    self, End, Enter, Field, FieldResult, FillError, Form, Outcome, Problem, Report, during, expect_value, plain_text,
    quoted, settles, text_of,
};
use crate::bridge::BridgeError;
use crate::credentials::{Keychain, SignIn, SignIns};
use crate::page::{Key, Locator, Page, PageError};

/// The Keychain item that holds the user's one sign-in for every employer's Workday site.
pub const SIGN_IN_SERVICE: &str = "dev.maverick.workday";

const PAGE_WAIT: Duration = Duration::from_secs(30);
/// How long a press of "Save and Continue" may show nothing before Maverick presses once more.
const PRESS_WAIT: Duration = Duration::from_secs(10);
/// How long Workday gets to answer a press of Sign In or Create Account.
const SIGN_IN_WAIT: Duration = Duration::from_secs(30);
/// How long the wall gets to change from one of its forms to another.
const FORM_WAIT: Duration = Duration::from_secs(10);
/// How many looks in a row a notice must stay before it counts as Workday's answer.
const NOTICE_LOOKS: u32 = 10;
const PAGE_GAP: Duration = Duration::from_millis(300);
const STEADY_LOOKS: u32 = 5;
const UPLOAD_WAIT: Duration = Duration::from_secs(30);

/// Matches the page Workday loads after a signed-in applicant submits. UNVERIFIED: no submission
/// was made to see it.
///
/// Workday's apply script ends a submission with `location.pathname = <site>/jobTasks/completed/application`,
/// and every public page of a Workday site repeats its own address in this `meta` element.
/// That the completed page does so too is an assumption.
pub const SUBMITTED: &str = r#"meta[property="og:url"][content$="/jobTasks/completed/application"]"#;

/// Where in a posting's pages an address points.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Stage {
    Posting,
    /// `/apply`, where Workday offers the ways to apply.
    Choosing,
    /// `/apply/applyManually` and its siblings: the sign-in wall, then the application's pages.
    Applying,
}

/// The parts of a posting's address.
#[derive(Debug, PartialEq)]
struct Address<'a> {
    host: &'a str,
    tenant: &'a str,
    site: &'a str,
    /// True on `myworkdaysite.com`, where one host serves many employers under `/recruiting`.
    shared: bool,
    /// Workday's id for the posting, such as `Backend-Engineer_R-1042`.
    posting: &'a str,
    stage: Stage,
}

impl<'a> Address<'a> {
    fn parse(address: &'a str) -> Option<Self> {
        let (host, path) = address.strip_prefix("https://")?.split_once('/')?;
        let path = path.split(['?', '#']).next()?;
        let mut parts = path.split('/').filter(|part| !part.is_empty()).peekable();
        // Workday puts the language first once the visitor moves around the site.
        parts.next_if(|part| is_language(part));
        let labels: Vec<&str> = host.split('.').collect();
        let (tenant, shared) = match labels.as_slice() {
            [tenant, center, "myworkdayjobs", "com"] if is_data_center(center) => (*tenant, false),
            [center, "myworkdaysite", "com"] if is_data_center(center) => {
                parts.next_if_eq(&"recruiting")?;
                (parts.next()?, true)
            }
            _ => return None,
        };
        let site = parts.next()?;
        parts.next_if_eq(&"job")?;
        let rest: Vec<&str> = parts.collect();
        // The place before the posting's id is optional, so `apply` is looked for after the first part.
        let apply = rest.iter().skip(1).position(|part| *part == "apply").map(|at| at + 1);
        let (job, stage) = match apply {
            None => (&rest[..], Stage::Posting),
            Some(at) => match rest.len() - at {
                1 => (&rest[..at], Stage::Choosing),
                2 => (&rest[..at], Stage::Applying),
                _ => return None,
            },
        };
        let posting = match job {
            [posting] | [_, posting] => *posting,
            _ => return None,
        };
        let plain = |part: &str| {
            part.bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
        };
        (plain(tenant) && plain(site)).then_some(Self {
            host,
            tenant,
            site,
            shared,
            posting,
            stage,
        })
    }

    /// Workday ignores letter case, the language and the place in a posting's path, and its own
    /// canonical link leaves the place out. So one posting gets one address here.
    fn posting_address(&self) -> String {
        let site = match self.shared {
            true => format!("recruiting/{}/{}", self.tenant, self.site),
            false => self.site.to_owned(),
        };
        format!("https://{}/{site}/job/{}", self.host, self.posting).to_lowercase()
    }

    /// The path of the job's public data on the posting's own host.
    fn job_data(&self) -> String {
        format!("/wday/cxs/{}/{}/job/{}", self.tenant, self.site, self.posting)
    }
}

/// True for `en-US` and the like.
fn is_language(part: &str) -> bool {
    let bytes = part.as_bytes();
    bytes.len() == 5
        && bytes[2] == b'-'
        && bytes[..2].iter().all(u8::is_ascii_lowercase)
        && bytes[3..].iter().all(u8::is_ascii_alphabetic)
}

/// True for `wd5` and the like, the name of the Workday data center in a host.
fn is_data_center(label: &str) -> bool {
    label
        .strip_prefix("wd")
        .is_some_and(|number| !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit()))
}

/// True for an address this module can fill.
pub fn is_posting(address: &str) -> bool {
    Address::parse(address).is_some()
}

/// The posting's address without anything a tracker, a campaign or Workday's own pages added to it.
pub fn posting_address(address: &str) -> Option<String> {
    Address::parse(address).map(|at| at.posting_address())
}

/// The full address of the posting's public job data, which any client can read.
pub fn job_data_address(address: &str) -> Option<String> {
    Address::parse(address).map(|at| format!("https://{}{}", at.host, at.job_data()))
}

/// The address where a posting's application starts.
pub fn form_address(address: &str) -> Option<String> {
    posting_address(address).map(|posting| format!("{posting}/apply"))
}

/// What a posting says about its job, for the model's context.
#[derive(Clone, Debug, PartialEq)]
pub struct Job {
    /// The job's title and place.
    pub job: String,
    /// The job description as plain text.
    pub description: String,
}

/// Asks for the job's data from inside the page, where the request is same-origin.
/// Only what `job` reads comes back.
const JOB_REQUEST: &str = r#"(async (path) => {
    const reply = await fetch(path, { headers: { accept: "application/json" } });
    if (!reply.ok) return { status: reply.status };
    const { jobPostingInfo: info } = await reply.json();
    return {
        status: reply.status,
        posted: info?.posted,
        title: info?.title,
        location: info?.location,
        description: info?.jobDescription,
    };
})"#;

/// Reads the job out of what `JOB_REQUEST` returned.
pub fn job(reply: &Value) -> Result<Job, FillError> {
    #[derive(Deserialize)]
    struct Reply {
        status: u16,
        posted: Option<bool>,
        title: Option<String>,
        location: Option<String>,
        description: Option<String>,
    }

    let reply = Reply::deserialize(reply).map_err(|error| FillError::UnexpectedForm(error.to_string()))?;
    // Live sites answered 404 for some postings they took down and 403 for others.
    if matches!(reply.status, 403 | 404) || reply.posted == Some(false) {
        return Err(FillError::Closed);
    }
    let title = reply.title.ok_or_else(|| {
        FillError::UnexpectedForm(format!("Workday answered {} when asked for the job", reply.status))
    })?;
    let job = match reply.location {
        Some(place) => format!("{}, {place}", title.trim()),
        None => title.trim().to_owned(),
    };
    Ok(Job {
        job,
        description: plain_text(reply.description.as_deref().unwrap_or_default()),
    })
}

/// Reads the job of the posting the tab is on. No account is needed for this.
pub async fn read_job(page: &Page) -> Result<Job, FillError> {
    let address = address_of(page).await?;
    let at = Address::parse(&address).ok_or_else(|| FillError::NotAPosting(address.clone()))?;
    job_at(page, &at).await
}

async fn job_at(page: &Page, at: &Address<'_>) -> Result<Job, FillError> {
    let reply = page
        .evaluate(&format!("{JOB_REQUEST}({})", quoted(&at.job_data())))
        .await
        .map_err(during("asking Workday for the job"))?;
    job(&reply)
}

async fn address_of(page: &Page) -> Result<String, FillError> {
    let address = page
        .evaluate("location.href")
        .await
        .map_err(during("reading the tab's address"))?;
    Ok(address.as_str().unwrap_or_default().to_owned())
}

/// Fills the application from the page on screen to its last page, and reports on each field.
/// The tab must be on a Workday posting, on the choice of how to apply, or inside the application.
///
/// From a posting it opens the application and chooses "Apply Manually". When Workday then asks
/// for an account, it signs in with the sign-in in the user's Keychain, or stops with
/// `FillError::SignInNeeded` when there is none. See `pass_wall`.
/// See `fill_pages` for when it stops.
pub async fn fill(page: &Page, answers: &Answers) -> Result<Report, FillError> {
    let captures = crate::data_dir().map(|dir| dir.join("captures"));
    fill_with(page, answers, captures.as_deref(), &Keychain).await
}

/// `fill`, with the folder for captures and the source of the sign-in chosen by the caller.
/// With no folder, no capture is saved. With `&None::<SignIn>`, the Keychain is never read and
/// the fill stops at the sign-in wall.
pub async fn fill_with(
    page: &Page,
    answers: &Answers,
    captures: Option<&Path>,
    sign_ins: &impl SignIns,
) -> Result<Report, FillError> {
    let address = address_of(page).await?;
    let at = Address::parse(&address).ok_or_else(|| FillError::NotAPosting(address.clone()))?;
    let job = job_at(page, &at).await?;
    if at.stage == Stage::Posting {
        page.navigate(&format!("{}/apply", at.posting_address()))
            .await
            .map_err(during("opening the application"))?;
    }
    let mut screen = shown(page, &[]).await?;
    if screen == Screen::Choice {
        page.click(&marked("applyManually"))
            .await
            .map_err(during("choosing Apply Manually"))?;
        screen = shown(page, &[Screen::Choice]).await?;
    }
    fill_from(page, answers, job, captures, sign_ins, screen).await
}

/// Fills the application in a tab that is past the choice of how to apply: it gets past the
/// sign-in wall when the wall is up, then fills the page on screen.
pub async fn fill_application(
    page: &Page,
    answers: &Answers,
    job: Job,
    captures: Option<&Path>,
    sign_ins: &impl SignIns,
) -> Result<Report, FillError> {
    let screen = shown(page, &[Screen::Choice]).await?;
    fill_from(page, answers, job, captures, sign_ins, screen).await
}

async fn fill_from(
    page: &Page,
    answers: &Answers,
    job: Job,
    captures: Option<&Path>,
    sign_ins: &impl SignIns,
    screen: Screen,
) -> Result<Report, FillError> {
    if screen != Screen::Wall {
        return fill_pages(page, answers, &job, captures).await;
    }
    let entry = pass_wall(page, sign_ins).await?;
    // The user must learn what was done under their name, also when the fill then fails.
    let after_entry = |error: FillError| FillError::SignIn(format!("{} Then the fill failed: {error}", entry.note()));
    shown(page, &[Screen::Wall, Screen::Choice])
        .await
        .map_err(after_entry)?;
    let mut report = fill_pages(page, answers, &job, captures).await.map_err(after_entry)?;
    report.note = Some(match report.note {
        Some(note) => format!("{} {note}", entry.note()),
        None => entry.note().to_owned(),
    });
    Ok(report)
}

/// What a tab inside Workday's apply flow shows.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Screen {
    Loading,
    /// The ways to apply.
    Choice,
    /// The sign-in or create-account form.
    Wall,
    /// A page of the application.
    Page,
}

/// What the tab shows, and how many fields it holds. The wall comes first, because it sits in the
/// same frame as an application page. A page counts once it has a name other than the loading
/// page's, or a field.
const SCREEN: &str = r#"(() => {
    const marked = (id) => document.querySelector(`[data-automation-id="${id}"]`) !== null;
    const fields = document.querySelectorAll('[data-automation-id^="formField-"]').length;
    if (marked("signInContent") || document.querySelector('input[type="password"]')) return { screen: "wall", fields };
    if (marked("applyManually")) return { screen: "choice", fields };
    const page = [...document.querySelectorAll('[data-automation-id$="Page"]')]
        .map((element) => element.getAttribute("data-automation-id"))
        .some((id) => id !== "applyFlowPage" && /^(applyFlow|takeAssessment)/.test(id));
    const loading = marked("applyFlowLoadingPage") || !(page || fields > 0);
    return { screen: loading ? "loading" : "page", fields };
})()"#;

/// Reads the screen out of what `SCREEN` returned.
fn screen_in(seen: &Value) -> Screen {
    match seen["screen"].as_str() {
        Some("wall") => Screen::Wall,
        Some("choice") => Screen::Choice,
        Some("page") => Screen::Page,
        _ => Screen::Loading,
    }
}

/// Waits until the tab shows something other than a loading page and the screens in `passing`.
/// Workday draws a page piece by piece: a live page showed one field for a moment before the
/// rest. So the same screen must show with the same fields `STEADY_LOOKS` times in a row.
async fn shown(page: &Page, passing: &[Screen]) -> Result<Screen, FillError> {
    let deadline = Instant::now() + PAGE_WAIT;
    let (mut last, mut steady) = (None, 0);
    loop {
        let seen = page
            .evaluate(SCREEN)
            .await
            .map_err(during("waiting for the application to appear"))?;
        let screen = screen_in(&seen);
        let now = Some((screen, seen["fields"].as_u64()));
        steady = if now == last { steady + 1 } else { 1 };
        last = now;
        if screen != Screen::Loading && !passing.contains(&screen) && steady >= STEADY_LOOKS {
            return Ok(screen);
        }
        if Instant::now() >= deadline {
            return Err(FillError::Page(PageError::NoElement {
                element: "a page of the Workday application".into(),
            }));
        }
        tokio::time::sleep(PAGE_GAP).await;
    }
}

/// How a fill got past the sign-in wall.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Entry {
    SignedIn,
    CreatedAccount {
        /// The create-account form had a consent box, and it was ticked.
        consented: bool,
    },
}

impl Entry {
    /// What the report tells the user was done under their name.
    fn note(self) -> &'static str {
        match self {
            Self::SignedIn => "Signed in to Workday with the sign-in in your Keychain.",
            Self::CreatedAccount { consented: false } => {
                "Created your Workday account at this employer with the sign-in in your Keychain."
            }
            Self::CreatedAccount { consented: true } => {
                "Created your Workday account at this employer with the sign-in in your Keychain, \
                 and ticked the consent box on its form."
            }
        }
    }
}

/// The form the sign-in wall shows.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
enum WallForm {
    /// Buttons for Google and the like, and "Sign in with email". 4 of 19 live walls opened so.
    Chooser,
    SignIn,
    /// 15 of 19 live walls opened on this form.
    Create,
    /// Another form, such as "Forgot Password", or no wall at all.
    Other,
}

/// What the sign-in wall shows: its form and what it says. UNVERIFIED on a live site, except the
/// form: no live wall was made to say anything.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
struct Wall {
    form: WallForm,
    /// The ids of the form's fields. Workday gives a field a new id each time it draws the form.
    fields: Vec<String>,
    /// False while the form's button is not ready for a press. See `settled_wall`.
    ready: bool,
    /// Workday's answer to a refused sign-in or a refused new account.
    refusals: Vec<String>,
    /// What the form says about single fields before it sends anything, such as a password that
    /// breaks the employer's rules.
    field_errors: Vec<String>,
    /// A message that is no error, such as the request to verify the email address.
    notices: Vec<String>,
    /// Shown under a refused sign-in when the account exists and its email address is unverified.
    unverified: Vec<String>,
}

impl Wall {
    fn has_errors(&self) -> bool {
        !(self.refusals.is_empty() && self.field_errors.is_empty() && self.unverified.is_empty())
    }

    fn says_something(&self) -> bool {
        self.has_errors() || !self.notices.is_empty()
    }

    /// Everything the wall says, in quotes, for a message to the user. The text stays as Workday
    /// wrote it, so that `pass_wall` can find a password in it.
    fn said(&self) -> String {
        let all = [&self.refusals, &self.field_errors, &self.unverified, &self.notices];
        let said: Vec<&str> = all.into_iter().flatten().map(String::as_str).collect();
        format!("\"{}\"", said.join(" "))
    }
}

/// Reads the wall. The form and its fields are as 19 live walls had them. The four kinds of
/// message are named as Workday's public page script draws them: no live wall showed one.
const WALL: &str = r#"(() => {
    const wall = document.querySelector('[data-automation-id="signInContent"]');
    const has = (id) => wall?.querySelector(`[data-automation-id="${id}"]`) != null;
    const said = (id) => [...(wall?.querySelectorAll(`[data-automation-id="${id}"]`) ?? [])]
        .map((element) => element.textContent.replace(/\s+/g, " ").trim())
        .filter((text) => text !== "");
    const button = wall?.querySelector('button[type="submit"]');
    return {
        form: has("createAccountSubmitButton") ? "create"
            : has("signInSubmitButton") ? "sign-in"
            : has("SignInWithEmailButton") ? "chooser"
            : "other",
        fields: [...(wall?.querySelectorAll("input") ?? [])].map((input) => input.id),
        ready: !button || button.getAttribute("aria-hidden") !== "true" || has("click_filter"),
        refusals: said("errorMessage"),
        fieldErrors: said("inputAlert"),
        notices: said("alertMessage"),
        unverified: said("informationalBlurbText"),
    };
})()"#;

/// Looks at the tab once: which screen it shows, and what the wall says.
async fn look(page: &Page) -> Result<(Screen, Wall), PageError> {
    let seen = page.evaluate(&format!("({{ at: {SCREEN}, wall: {WALL} }})")).await?;
    let wall = Wall::deserialize(&seen["wall"]).map_err(|error| PageError::Script(error.to_string()))?;
    Ok((screen_in(&seen["at"]), wall))
}

/// Gets past the sign-in wall with the user's stored Workday sign-in.
///
/// The user keeps ONE email and password for every employer's Workday site, in the Keychain item
/// `SIGN_IN_SERVICE`. With no such item this stops with `FillError::SignInNeeded` and types nothing.
///
/// With one, it signs in. Workday gives the same answer for a wrong password and for an email
/// address it does not know, so a refused sign-in is followed by ONE try to create the account
/// with the same email and password. It never tries either a second time in one fill. When
/// Workday wants the email address verified, it stops and says so: it never reads email.
///
/// Typing into the wall is verified on live sites. Everything after a press is NOT: see the top
/// of this module.
async fn pass_wall(page: &Page, sign_ins: &impl SignIns) -> Result<Entry, FillError> {
    let sign_in = sign_ins
        .sign_in(SIGN_IN_SERVICE)
        .await
        .map_err(|problem| {
            FillError::SignIn(format!(
                "Maverick could not read your Workday sign-in from the Keychain: {problem}. \
                 Sign in to Workday in that tab yourself, then click Fill form again."
            ))
        })?
        .ok_or(FillError::SignInNeeded {
            system: "Workday",
            service: SIGN_IN_SERVICE,
        })?;
    let password = sign_in.password.reveal();
    // No step below puts the password in an error. Should Workday or Chrome ever repeat it in
    // their own text, that text is dropped here.
    enter_account(page, &sign_in).await.map_err(|error| {
        if !password.is_empty() && error.to_string().contains(password) {
            FillError::SignIn(
                "Maverick stopped at Workday's sign-in. What Workday showed held the password, \
                 so it is left out here. Look at the tab."
                    .into(),
            )
        } else {
            error
        }
    })
}

async fn enter_account(page: &Page, sign_in: &SignIn) -> Result<Entry, FillError> {
    let (email, password) = (sign_in.account.as_str(), sign_in.password.reveal());
    let yourself = "Sign in to Workday in that tab yourself, then click Fill form again.";

    let typed = sign_in_form(page, email, password).await?;
    let refused = match press(page, "signInSubmitButton", &typed, "signing in to Workday").await? {
        Pressed::Application => return Ok(Entry::SignedIn),
        Pressed::Nothing => return Err(no_answer("Sign In")),
        Pressed::Said(wall) if !wall.unverified.is_empty() => {
            return Err(FillError::SignIn(format!(
                "Workday did not sign you in and says the account may need its email address verified: {}. \
                 Open the email from Workday and verify, then click Fill form again. \
                 Maverick tried the sign-in once and created no account.",
                wall.said()
            )));
        }
        // Only a refusal from Workday itself can mean that there is no account yet.
        Pressed::Said(wall) if wall.refusals.is_empty() => {
            return Err(FillError::SignIn(format!(
                "Workday did not sign you in and shows: {}. Maverick tried once and created no account. {yourself}",
                wall.said()
            )));
        }
        Pressed::Said(wall) => wall.said(),
    };

    let (typed, consented) = create_form(page, email, password).await?;
    match press(
        page,
        "createAccountSubmitButton",
        &typed,
        "creating your Workday account",
    )
    .await?
    {
        Pressed::Application => Ok(Entry::CreatedAccount { consented }),
        Pressed::Nothing => Err(no_answer("Create Account")),
        Pressed::Said(wall) if !wall.has_errors() => Err(FillError::SignIn(format!(
            "Workday refused the sign-in, so Maverick asked it to create your account at this employer. \
             Workday now shows: {}. If it asks you to verify your email address, open the email from Workday \
             and verify. Then click Fill form again.",
            wall.said()
        ))),
        Pressed::Said(wall) => Err(FillError::SignIn(format!(
            "Workday refused the sign-in with {refused}, and then refused to create an account with {}. \
             Maverick tried each once and stopped. Check the email and the password in the Keychain item \
             {SIGN_IN_SERVICE}, or sign in to Workday in that tab yourself. Then click Fill form again.",
            wall.said()
        ))),
    }
}

fn no_answer(button: &str) -> FillError {
    FillError::SignIn(format!(
        "Maverick pressed {button} on Workday once, and Workday showed neither the application nor a message \
         in {} seconds. Look at the tab before you click Fill form again.",
        SIGN_IN_WAIT.as_secs()
    ))
}

/// An element of the sign-in wall.
fn in_wall(id: &str) -> Locator {
    Locator::css(&format!(
        "[data-automation-id=\"signInContent\"] [data-automation-id=\"{id}\"]"
    ))
}

/// Waits until the wall has finished drawing a form, and returns what it shows.
///
/// A live wall draws a form twice after a switch. About a tenth of a second after the first
/// drawing it puts new fields in place of the first ones, and only then lays the click filter
/// over the button. Text typed into the first drawing is lost, and a click there lands on the
/// bare button. So the same fields must show `STEADY_LOOKS` times in a row, with the button ready.
async fn settled_wall(page: &Page, doing: &'static str) -> Result<Wall, FillError> {
    let deadline = Instant::now() + FORM_WAIT;
    let (mut last, mut steady) = (None, 0);
    loop {
        let (_, wall) = look(page).await.map_err(during(doing))?;
        steady = if last.as_ref() == Some(&wall) { steady + 1 } else { 1 };
        let settled = steady >= STEADY_LOOKS && wall.ready && wall.form != WallForm::Other;
        if settled || Instant::now() >= deadline {
            return Ok(wall);
        }
        last = Some(wall);
        tokio::time::sleep(PAGE_GAP).await;
    }
}

/// Brings up the sign-in form or the create-account form. The wall opens on either one or on the
/// chooser, and has a link from each to the next. Verified on live walls.
async fn bring_up(page: &Page, wanted: WallForm) -> Result<(), FillError> {
    let doing = "looking for Workday's sign-in form";
    // The chooser leads to the sign-in form and that leads to the create-account form.
    for _ in 0..3 {
        let wall = settled_wall(page, doing).await?;
        let link = match wall.form {
            form if form == wanted && wall.ready => return Ok(()),
            form if form == wanted => break,
            WallForm::Chooser => "SignInWithEmailButton",
            WallForm::Create => "signInLink",
            WallForm::SignIn => "createAccountLink",
            WallForm::Other => break,
        };
        page.click(&in_wall(link)).await.map_err(during(doing))?;
        let deadline = Instant::now() + FORM_WAIT;
        while look(page).await.map_err(during(doing))?.1.form == wall.form && Instant::now() < deadline {
            tokio::time::sleep(PAGE_GAP).await;
        }
    }
    Err(FillError::SignIn(
        "Workday shows a sign-in page that Maverick does not know, so it pressed nothing. \
         Sign in to Workday in that tab yourself, then click Fill form again."
            .into(),
    ))
}

/// The fields of a form and the text typed into each, for a last check before a press.
type Typed<'a> = Vec<(&'static str, &'a str)>;

/// Brings up the sign-in form and types the sign-in into it. Verified on live walls.
async fn sign_in_form<'a>(page: &Page, email: &'a str, password: &'a str) -> Result<Typed<'a>, FillError> {
    bring_up(page, WallForm::SignIn).await?;
    type_in(page, "email", "the email address", email).await?;
    type_in(page, "password", "the password", password).await?;
    Ok(vec![("email", email), ("password", password)])
}

/// Brings up the create-account form, types the sign-in into it and ticks its consent box.
/// Returns whether there was a box. Verified on live walls.
async fn create_form<'a>(page: &Page, email: &'a str, password: &'a str) -> Result<(Typed<'a>, bool), FillError> {
    bring_up(page, WallForm::Create).await?;
    type_in(page, "email", "the email address", email).await?;
    type_in(page, "password", "the password", password).await?;
    type_in(page, "verifyPassword", "the password a second time", password).await?;
    let typed = vec![("email", email), ("password", password), ("verifyPassword", password)];
    Ok((typed, consent(page).await?))
}

/// Types into a field of the wall and checks that the field took the text. The check happens in
/// the page and brings back yes or no, so the text never reaches an error. Verified on live walls.
///
/// Fields are found by their own names. The wall also holds a field named `website` that a person
/// cannot see ("beecatcher"): Workday refuses a press when it holds text, and nothing here types
/// into it.
async fn type_in(page: &Page, id: &str, what: &str, text: &str) -> Result<(), FillError> {
    let field = in_wall(id);
    page.fill(&field, text)
        .await
        .map_err(during(format!("typing {what} into Workday's sign-in form")))?;
    if settles(page, &holds(&field, text))
        .await
        .map_err(during(format!("checking {what} in Workday's sign-in form")))?
    {
        return Ok(());
    }
    Err(FillError::SignIn(format!(
        "Workday's sign-in form did not take {what} as typed, so Maverick pressed nothing. \
         Sign in to Workday in that tab yourself, then click Fill form again."
    )))
}

/// A JavaScript condition: the field holds exactly `text`.
fn holds(field: &Locator, text: &str) -> String {
    format!("({})?.value === {}", field.as_script(), quoted(text))
}

/// Ticks the consent box of the create-account form, when the employer put one there, and says
/// whether it did. 10 of 19 live forms had one, and Workday's script creates no account without
/// it. Verified on live walls.
async fn consent(page: &Page) -> Result<bool, FillError> {
    let doing = "ticking the consent box of Workday's create-account form";
    let consent_box = in_wall("createAccountCheckbox");
    let present = format!("!!({})", consent_box.as_script());
    if page.evaluate(&present).await.map_err(during(doing))? != true {
        return Ok(false);
    }
    match tick(page, &consent_box, true).await {
        Ok(()) => Ok(true),
        Err(Problem::Page(error)) => Err(during(doing)(error)),
        Err(problem) => Err(FillError::SignIn(format!(
            "Maverick could not tick the consent box of Workday's create-account form: {problem}. \
             It created no account. Sign in to Workday in that tab yourself, then click Fill form again."
        ))),
    }
}

/// What a rehearsal of the wall found.
#[derive(Debug, PartialEq)]
pub struct Rehearsal {
    /// The create-account form had a consent box, and it was ticked.
    pub consented: bool,
    /// The `data-automation-id` of the element a press of Create Account would land on.
    pub create_press: String,
    /// The same for Sign In.
    pub sign_in_press: String,
}

/// Does on the wall everything `pass_wall` does before a press, and presses NEITHER Sign In nor
/// Create Account: it brings up each form, types `sign_in` into it, ticks the consent box and
/// finds the element a press would land on. A live test runs this with a made-up sign-in, which
/// is how the steps before a press are verified without an account.
pub async fn rehearse_wall(page: &Page, sign_in: &SignIn) -> Result<Rehearsal, FillError> {
    let (email, password) = (sign_in.account.as_str(), sign_in.password.reveal());
    let (typed, consented) = create_form(page, email, password).await?;
    let create_press = press_lands_on(page, "createAccountSubmitButton", &typed).await?;
    let typed = sign_in_form(page, email, password).await?;
    Ok(Rehearsal {
        consented,
        create_press,
        sign_in_press: press_lands_on(page, "signInSubmitButton", &typed).await?,
    })
}

/// The `data-automation-id` of the element a press of `button` would land on.
async fn press_lands_on(page: &Page, button: &str, typed: &Typed<'_>) -> Result<String, FillError> {
    let doing = "rehearsing the press";
    ready_for_press(page, typed, doing).await?;
    let target = format!(
        "({})?.getAttribute('data-automation-id') ?? ''",
        press_target(button).as_script()
    );
    let id = page.evaluate(&target).await.map_err(during(doing))?;
    Ok(id.as_str().unwrap_or_default().to_owned())
}

/// What Workday did with a press of Sign In or Create Account.
enum Pressed {
    /// A page of the application is on screen.
    Application,
    /// The wall stayed and says something.
    Said(Wall),
    /// Neither happened in `SIGN_IN_WAIT`.
    Nothing,
}

/// The element a mouse click on a submit button of the wall lands on.
///
/// On all 19 live walls the real button (`signInSubmitButton` or `createAccountSubmitButton`) is
/// hidden from input, with `aria-hidden` and a `tabindex` of -2, and a `click_filter` element with
/// the role of a button lies over it. A person's click lands on the filter, and Workday's script
/// then clicks the real button itself. `Page::click` refuses the covered button, rightly, so the
/// click goes to the filter. A wall without a filter gets the click on its button.
fn press_target(button: &str) -> Locator {
    Locator::script(
        format!("Workday's {button}"),
        format!(
            "((button) => button?.closest('[data-automation-id=\"noCaptchaWrapper\"]')\
             ?.querySelector('[data-automation-id=\"click_filter\"]') ?? button)({})",
            in_wall(button).as_script()
        ),
    )
}

/// Looks at the wall right before a press and returns what it shows. Should Workday have drawn
/// the form anew since the typing, the fields are empty or the button is bare, and nothing is
/// pressed.
async fn ready_for_press(page: &Page, typed: &Typed<'_>, doing: &'static str) -> Result<Wall, FillError> {
    let (_, wall) = look(page).await.map_err(during(doing))?;
    let mut held = wall.ready;
    for (id, text) in typed {
        held = held && page.evaluate(&holds(&in_wall(id), text)).await.map_err(during(doing))? == true;
    }
    if held {
        return Ok(wall);
    }
    Err(FillError::SignIn(
        "Workday drew its sign-in form anew while Maverick typed into it, so Maverick pressed nothing. \
         Click Fill form again."
            .into(),
    ))
}

/// Presses a submit button of the wall ONCE and waits for Workday's answer.
///
/// UNVERIFIED on a live site: no button was pressed there. Per Workday's script, a good sign-in
/// takes the wall away, a refusal draws a message on the wall, and a new account that must verify
/// its email address loads the site's `/login` page with a notice.
async fn press(page: &Page, button: &str, typed: &Typed<'_>, doing: &'static str) -> Result<Pressed, FillError> {
    let before = ready_for_press(page, typed, doing).await?;
    page.click(&press_target(button)).await.map_err(during(doing))?;
    let deadline = Instant::now() + SIGN_IN_WAIT;
    let (mut last, mut noticed) = (before.clone(), 0);
    loop {
        tokio::time::sleep(PAGE_GAP).await;
        match look(page).await {
            Ok((Screen::Page, _)) => return Ok(Pressed::Application),
            Ok((_, wall)) => {
                // A notice can show for a moment before the application does, so it must stay.
                let news = wall != before && wall.says_something();
                noticed = if news && wall == last { noticed + 1 } else { 0 };
                if news && (wall.has_errors() || noticed >= NOTICE_LOOKS) {
                    return Ok(Pressed::Said(wall));
                }
                last = wall;
            }
            // Workday can load another page after a press, and a look between two pages fails.
            Err(PageError::Script(_) | PageError::Bridge(BridgeError::Browser(_))) => {}
            Err(error) => return Err(during(doing)(error)),
        }
        if Instant::now() >= deadline {
            let news = last != before && last.says_something();
            return Ok(if news { Pressed::Said(last) } else { Pressed::Nothing });
        }
    }
}

/// The most pages one fill goes through. An application has fewer.
const MOST_PAGES: usize = 12;
/// The most looks one fill takes at a page.
const MOST_LOOKS: usize = 4;
/// The only text on the button at the foot of a page that Maverick presses.
const NEXT: &str = "Save and Continue";
/// How often in a row the page must look the same after a press of the button, before what it
/// shows counts as its answer.
const ANSWER_LOOKS: u32 = 4;
const ANSWER_GAP: Duration = Duration::from_millis(500);

const UNVERIFIED: &str = "Check every answer: most of Maverick's Workday filling is not verified on live pages yet.";

/// Fills the page on screen, presses "Save and Continue", and does the same on each next page.
///
/// It stops, and says why, when an answer could not be entered, when Workday refuses the page,
/// and when the button at the foot of the page says anything other than "Save and Continue".
/// The last page's button says "Submit", so the fill ends there with `End::Review`.
async fn fill_pages(page: &Page, answers: &Answers, job: &Job, captures: Option<&Path>) -> Result<Report, FillError> {
    let mut whole = Report {
        outcomes: Vec::new(),
        model_problem: None,
        note: None,
        end: End::Review,
    };
    let mut steps: Vec<String> = Vec::new();
    whole.end = loop {
        let at = place(page).await?;
        if !at.next.eq_ignore_ascii_case(NEXT) {
            break match at.next.eq_ignore_ascii_case("Submit") {
                true => End::Review,
                false => End::Stopped(format!(
                    "Maverick stopped on \"{}\": the button at the foot of the page says {:?}, and Maverick only \
                     presses \"{NEXT}\".",
                    at.step, at.next
                )),
            };
        }
        if steps.len() == MOST_PAGES {
            break End::Stopped(format!("Maverick stopped on \"{}\" after {MOST_PAGES} pages.", at.step));
        }
        steps.push(at.step.clone());
        // An answer can bring up questions the page did not show before. Each look at the page
        // fills the fields that no earlier look saw, and the fields that lost their answer.
        let mut done = Vec::new();
        let mut entered: Vec<(String, Outcome)> = Vec::new();
        for look in 1..=MOST_LOOKS {
            let before = done.len();
            let report = fill_one(page, answers, job, captures, &mut done).await?;
            whole.model_problem = whole.model_problem.or(report.model_problem);
            if report.outcomes.is_empty() {
                break;
            }
            entered.extend(done[before..].iter().cloned().zip(report.outcomes));
            tokio::time::sleep(ANSWER_GAP).await;
            // Workday can draw the page anew a moment after an answer, and an answer entered in
            // that moment is gone. On a live page a ticked box was lost right after a date.
            let empty = empty_fields(page).await?;
            let lost = |name: &String, outcome: &Outcome| {
                matches!(outcome.result, FieldResult::Filled { .. }) && empty.contains(name)
            };
            if look < MOST_LOOKS {
                done.retain(|name| {
                    !entered
                        .iter()
                        .any(|(entered, outcome)| entered == name && lost(name, outcome))
                });
                entered.retain(|(name, outcome)| !lost(name, outcome));
            } else {
                for (name, outcome) in &mut entered {
                    if lost(name, outcome) {
                        outcome.result = FieldResult::Failed("the page did not keep the answer".into());
                    }
                }
            }
        }
        let failed = entered
            .iter()
            .filter(|(_, outcome)| matches!(outcome.result, FieldResult::Failed(_)))
            .count();
        whole.outcomes.extend(entered.into_iter().map(|(_, outcome)| outcome));
        if failed > 0 {
            break End::Stopped(format!(
                "Maverick stopped on \"{}\": it could not enter {failed} of its answers there. \
                 Fix them in the tab, then click Fill form again.",
                at.step
            ));
        }
        page.click(&marked("pageFooterNextButton"))
            .await
            .map_err(during(format!("pressing \"{NEXT}\" on \"{}\"", at.step)))?;
        let mut moved = after_next(page, &at, PRESS_WAIT).await?;
        if matches!(moved, Moved::Nowhere) && place(page).await? == at {
            // Workday can draw the page anew while the button is pressed, and that press does
            // nothing. On a live page a second press moved on. The page is the same one, and
            // its button still says "Save and Continue".
            page.click(&marked("pageFooterNextButton"))
                .await
                .map_err(during(format!("pressing \"{NEXT}\" on \"{}\" again", at.step)))?;
            moved = after_next(page, &at, PAGE_WAIT).await?;
        }
        match moved {
            Moved::On => {}
            Moved::Refused(errors) => {
                break End::Stopped(format!(
                    "Workday did not take \"{}\": {} Fix this in the tab, then click Fill form again.",
                    at.step,
                    errors.join(" ")
                ));
            }
            Moved::Nowhere => {
                break End::Stopped(format!(
                    "Maverick pressed \"{NEXT}\" on \"{}\", and the page did not change in {} seconds. \
                     Look at the tab before you click Fill form again.",
                    at.step,
                    PAGE_WAIT.as_secs()
                ));
            }
        }
    };
    let pages = match steps.len() {
        0 => "It filled no page.".to_owned(),
        _ => format!("It filled: {}.", steps.join(", ")),
    };
    let saved = captures.map_or_else(String::new, |folder| {
        format!(
            " The structure of each page, without your answers, is saved in {}.",
            folder.display()
        )
    });
    let review = match whole.end {
        End::Review => " It stopped at the last page. Read it, then click Submit yourself.",
        _ => "",
    };
    whole.note = Some(format!("{UNVERIFIED} {pages}{review}{saved}"));
    Ok(whole)
}

/// Where the application is: the step the progress bar marks, and the text of the button at the
/// foot of the page.
#[derive(Debug, PartialEq)]
struct Place {
    step: String,
    next: String,
    errors: Vec<String>,
}

/// Reads `Place`. Workday lists what it refuses about a page in `errorHeading`, one entry per
/// field, and the entry's button names its message in `aria-describedby`.
const PLACE: &str = r#"(() => {
    const text = (element) => (element?.textContent ?? "").replace(/\s+/g, " ").trim();
    const marked = (id) => document.querySelector(`[data-automation-id="${id}"]`);
    const listed = [...document.querySelectorAll('[data-automation-id="errorHeading"] li')].map((entry) => {
        const button = entry.querySelector("button");
        const message = text(document.getElementById(button?.getAttribute("aria-describedby") ?? ""));
        return message || text(entry);
    });
    const beside = [...document.querySelectorAll('[data-automation-id="inputAlert"]')].map(text);
    return {
        step: text(marked("progressBarActiveStep")).replace(/^.*\bstep \d+ of \d+\s*/i, ""),
        next: text(marked("pageFooterNextButton")),
        errors: listed.length > 0 ? listed : beside,
    };
})()"#;

async fn place(page: &Page) -> Result<Place, FillError> {
    let seen = page
        .evaluate(PLACE)
        .await
        .map_err(during("reading where the application is"))?;
    let text = |key: &str| seen[key].as_str().unwrap_or_default().to_owned();
    Ok(Place {
        step: text("step"),
        next: text("next"),
        errors: serde_json::from_value(seen["errors"].clone()).unwrap_or_default(),
    })
}

/// What a press of "Save and Continue" led to.
enum Moved {
    /// The application shows another page.
    On,
    /// Workday kept the page and says what is wrong with it.
    Refused(Vec<String>),
    Nowhere,
}

/// Waits for Workday's answer to a press of "Save and Continue" on the page at `before`.
///
/// A page that was refused before can still list its old errors for a moment, so errors count
/// only once the page has looked the same `ANSWER_LOOKS` times in a row.
async fn after_next(page: &Page, before: &Place, limit: Duration) -> Result<Moved, FillError> {
    let deadline = Instant::now() + limit;
    let (mut last, mut steady) = (None, 0);
    loop {
        tokio::time::sleep(ANSWER_GAP).await;
        let now = place(page).await?;
        if !now.step.is_empty() && now.step != before.step {
            // The next page draws piece by piece.
            shown(page, &[]).await?;
            return Ok(Moved::On);
        }
        steady = if last.as_ref() == Some(&now) { steady + 1 } else { 1 };
        if steady >= ANSWER_LOOKS && !now.errors.is_empty() {
            return Ok(Moved::Refused(now.errors));
        }
        last = Some(now);
        if Instant::now() >= deadline {
            return Ok(Moved::Nowhere);
        }
    }
}

/// Fills the application page on screen and stays on it. See `fill_one`.
///
/// Only partly verified against live Workday pages. See the top of this module.
pub async fn fill_page(page: &Page, answers: &Answers, job: Job, captures: Option<&Path>) -> Result<Report, FillError> {
    fill_one(page, answers, &job, captures, &mut Vec::new()).await
}

/// Fills the application page on screen: reads its fields from the page itself, saves a capture
/// of its structure under `captures`, then enters the answers. It never leaves the page.
///
/// `done` names the fields an earlier look at this page filled. This look leaves them alone and
/// adds its own. With no field left, it saves no capture.
async fn fill_one(
    page: &Page,
    answers: &Answers,
    job: &Job,
    captures: Option<&Path>,
    done: &mut Vec<String>,
) -> Result<Report, FillError> {
    add_entries(page, "Work-Experience-section", answers.work.len()).await?;
    add_entries(page, "Education-section", answers.education.len()).await?;
    let seen = page.evaluate(READ).await.map_err(during("reading the page's fields"))?;
    // Any page can ask about military service or the like, so the model reads the user's
    // disclosures on every page. Its instructions keep them to the questions that ask.
    let disclose = !answers.disclosures.is_empty();
    let disclosing;
    let answers = match disclose {
        true => {
            disclosing = Answers {
                facts: [answers.disclosures.as_slice(), answers.facts.as_slice()].concat(),
                ..answers.clone()
            };
            &disclosing
        }
        false => answers,
    };
    let read = match fields(&seen, disclose) {
        Ok(mut fields) => {
            fields.retain(|field| !done.contains(&name_of(field)));
            done.extend(fields.iter().map(name_of));
            with_options(page, fields).await
        }
        Err(error) => Err(error),
    };
    let nothing_new = matches!(&read, Ok(fields) if fields.is_empty());
    let note = match captures
        .filter(|_| !nothing_new)
        .map(|folder| capture(folder, &seen, &read))
    {
        Some(Ok(file)) => format!(
            "{UNVERIFIED} The structure of this page, without your answers, is saved in {}.",
            file.display()
        ),
        Some(Err(problem)) => format!("{UNVERIFIED} The structure of this page could not be saved: {problem}."),
        None => UNVERIFIED.to_owned(),
    };
    let fields = read.map_err(|error| match error {
        FillError::UnexpectedForm(problem) => FillError::UnexpectedForm(format!("{problem}. {note}")),
        other => other,
    })?;
    let form = Form {
        job: job.job.clone(),
        description: job.description.clone(),
        fields,
    };
    let mut report = apply::fill_form(page, answers, form, &Workday).await?;
    report.note = Some(note);
    Ok(report)
}

/// The name a fill knows a field by. A field without an id of its own is known by its question.
fn name_of(field: &Field) -> String {
    match field.path.is_empty() {
        true => field.question.title.clone(),
        false => field.path.clone(),
    }
}

/// The names of the fields on the page that hold no answer, among the kinds whose answer shows.
/// A box that is not ticked and a date say nothing here: the page reads the same with and
/// without an answer.
async fn empty_fields(page: &Page) -> Result<Vec<String>, FillError> {
    let seen = page.evaluate(READ).await.map_err(during("reading the page's fields"))?;
    let shows = |kind: &Kind| !matches!(kind, Kind::YesNo | Kind::Date | Kind::Unsupported(_));
    Ok(fields(&seen, true)?
        .iter()
        .filter(|field| !field.held && shows(&field.question.kind))
        .map(name_of)
        .collect())
}

/// How many entries a list on the page has, such as the jobs. Null when the page has no such
/// list. A list is a group named by its heading, and each entry is a group named by a `-panel`.
const ENTRIES: &str = r#"((heading) => {
    const list = document.querySelector(`[role="group"][aria-labelledby="${heading}"]`);
    return list ? list.querySelectorAll('[role="group"][aria-labelledby$="-panel"]').length : null;
})"#;

/// Presses a list's Add button until the list has `wanted` entries. A list that will not grow is
/// left as it is: the fill then reports the fields it has.
async fn add_entries(page: &Page, heading: &str, wanted: usize) -> Result<(), FillError> {
    let adding = || during(format!("adding entries to {heading}"));
    let count = format!("{ENTRIES}({})", quoted(heading));
    let add = Locator::script(
        format!("the Add button of {heading}"),
        format!(
            "document.querySelector('[role=\"group\"][aria-labelledby=\"{heading}\"] [data-automation-id=\"add-button\"]')"
        ),
    );
    loop {
        let shown = page.evaluate(&count).await.map_err(adding())?.as_u64();
        let Some(shown) = shown.filter(|&shown| (shown as usize) < wanted) else {
            return Ok(());
        };
        match page.click(&add).await {
            Ok(()) => {}
            Err(error) if error.ends_the_work() => return Err(adding()(error)),
            Err(_) => return Ok(()),
        }
        if !settles(page, &format!("{count} > {shown}")).await.map_err(adding())? {
            return Ok(());
        }
    }
}

/// Reads the page in one pass: the fields as this module understands them, and the page's
/// structure for a capture. It reads whether a field holds an answer, and never the answer itself.
///
/// A field is a `formField-` container. Workday's script gives it the field's id as `data-fkit-id`
/// and puts the question in a `label`, or in a `legend` when the field is a group.
const READ: &str = r#"(() => {
    const text = (element) => (element?.textContent ?? "").replace(/\s+/g, " ").trim();
    const attribute = (element, name) => element.getAttribute(name);
    const typed = 'textarea, input:not([type]), input[type="text"], input[type="email"], input[type="tel"], input[type="number"], input[type="url"]';
    const prompt = '[data-automation-id="multiSelectContainer"], [data-automation-id="multiselectInputContainer"], [data-uxi-widget-type]';
    const ids = [...document.querySelectorAll("[data-fkit-id]")].map((element) => attribute(element, "data-fkit-id"));
    const containers = [...document.querySelectorAll('[data-automation-id^="formField-"]')].filter(
        (field) =>
            !field.closest('[data-automation-id="signInContent"]') &&
            !field.querySelector('input[type="password"]') &&
            !field.parentElement?.closest('[data-automation-id^="formField-"]'),
    );
    // A list of entries, such as the jobs, numbers its entries as it likes: `workExperience-19`.
    // The place of an entry is its place on the page.
    const lists = {};
    const place = (id) => {
        const [, list, number] = (id ?? "").split("--")[0].match(/^(.*)-(\d+)$/) ?? [];
        if (!list) return null;
        const entries = (lists[list] ??= []);
        if (!entries.includes(number)) entries.push(number);
        return entries.indexOf(number);
    };
    const fields = containers.map((field) => {
        const heading = field.querySelector("legend, label")?.cloneNode(true);
        const starred = heading?.querySelector("abbr") != null;
        heading?.querySelectorAll("abbr").forEach((star) => star.remove());
        const id = attribute(field, "data-fkit-id") ?? field.querySelector("[data-fkit-id]")?.getAttribute("data-fkit-id") ?? null;
        const inputs = (type) => [...field.querySelectorAll(`input[type="${type}"]`)];
        const labels = (list) => list.map((input) => text(input.labels?.[0]));
        const [radios, boxes, texts] = [inputs("radio"), inputs("checkbox"), [...field.querySelectorAll(typed)]];
        let [control, options] = ["unknown", []];
        if (inputs("file").length === 1) control = "file";
        else if (field.querySelector(prompt)) control = "prompt";
        else if (field.querySelector('[data-automation-id^="dateSection"]')) control = "date";
        else if (field.querySelectorAll('button[aria-haspopup="listbox"]').length === 1) control = "dropdown";
        else if (radios.length > 0) [control, options] = ["radio", labels(radios)];
        else if (boxes.length > 1) [control, options] = ["checkboxes", labels(boxes)];
        else if (boxes.length === 1) control = "checkbox";
        else if (texts.length === 1) control = texts[0].tagName === "TEXTAREA" ? "textarea" : "text";
        const held =
            control === "file" ? field.querySelector('[data-automation-id="file-upload-item"]') !== null
            : control === "prompt" ? field.querySelector('[data-automation-id="selectedItem"]') !== null
            : control === "dropdown" ? field.querySelector('button[aria-haspopup="listbox"]').value !== ""
            : control === "radio" ? radios.some((radio) => radio.checked)
            : control === "checkboxes" ? boxes.some((box) => box.checked)
            : control === "text" || control === "textarea" ? texts[0].value !== ""
            : false;
        return {
            id,
            entry: place(id),
            held,
            unique: ids.filter((other) => other === id).length === 1,
            title: text(heading).replace(/\*$/, "").trim() || attribute(field, "data-automation-id"),
            required: starred || field.querySelector('[aria-required="true"]') !== null,
            control,
            input: texts[0]?.getAttribute("type") ?? null,
            options,
        };
    });

    // The values of these say how a control is built. Any other `aria-` attribute can repeat what
    // the user chose or typed, so only its name is kept.
    const structural = ["aria-haspopup", "aria-required", "aria-controls", "aria-expanded", "aria-invalid",
        "aria-multiselectable", "aria-labelledby", "aria-describedby", "aria-autocomplete", "aria-hidden", "aria-disabled"];
    const described = (element) => {
        const aria = {};
        for (const { name, value } of element.attributes) {
            if (name.startsWith("aria-")) aria[name] = structural.includes(name) ? value : null;
        }
        return {
            tag: element.tagName.toLowerCase(),
            type: attribute(element, "type"),
            role: attribute(element, "role"),
            automationId: attribute(element, "data-automation-id"),
            fkitId: attribute(element, "data-fkit-id"),
            id: element.id || null,
            aria,
            label: element.labels?.length ? text(element.labels[0]) : null,
            options: element.tagName === "SELECT" ? [...element.options].map((option) => option.label) : null,
        };
    };
    const controls = "input, textarea, select, button, [role], [data-automation-id]";
    const frame = document.querySelector('[data-automation-id="applyFlowPage"]') ?? document.body;
    const structure = {
        fields: containers.map((field) => ({
            ...described(field),
            label: text(field.querySelector("legend, label")),
            controls: [...field.querySelectorAll(controls)].map(described),
        })),
        other: [...frame.querySelectorAll(controls)]
            .filter((element) => !element.closest('[data-automation-id^="formField-"]'))
            .map(described),
    };

    const page = [...document.querySelectorAll('[data-automation-id$="Page"]')]
        .map((element) => attribute(element, "data-automation-id"))
        .find((id) => !["applyFlowPage", "applyFlowLoadingPage"].includes(id) && /^(applyFlow|takeAssessment)/.test(id));
    const step = text(document.querySelector('[data-automation-id="progressBarActiveStep"]'));
    return {
        address: location.origin + location.pathname,
        page: page ?? "",
        step: step.replace(/^.*\bstep \d+ of \d+\s*/i, ""),
        fields,
        structure,
    };
})()"#;

/// Turns what `READ` saw into fields. A control this code does not know, or cannot find again
/// by a plain id, becomes a field that is left for the user.
///
/// A drop-down list comes back as a choice with no options. `with_options` reads them.
///
/// On a page of voluntary disclosures, every field is left for the user unless `disclose` is set.
pub fn fields(seen: &Value, disclose: bool) -> Result<Vec<Field>, FillError> {
    #[derive(Deserialize)]
    struct Seen {
        page: String,
        step: String,
        fields: Vec<SeenField>,
    }
    #[derive(Deserialize)]
    struct SeenField {
        id: Option<String>,
        #[serde(default)]
        entry: Option<usize>,
        #[serde(default)]
        held: bool,
        unique: bool,
        title: String,
        required: bool,
        control: String,
        input: Option<String>,
        options: Vec<String>,
    }

    let seen = Seen::deserialize(seen).map_err(|error| FillError::UnexpectedForm(error.to_string()))?;
    let voluntary = is_voluntary(&seen.page, &seen.step);
    // The user has a phone number without an extension, and a rule about the phone would fill it.
    let asked = seen
        .fields
        .into_iter()
        .filter(|field| field.id.as_deref() != Some("phoneNumber--extension"));
    let fields = asked.map(|field| {
        // The id goes into CSS selectors, so it must be plain, and it must find one field.
        let plain = |id: &String| {
            !id.is_empty()
                && id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
        };
        let id = field.id.filter(|id| field.unique && plain(id)).unwrap_or_default();
        let unsupported = |kind: &str| Kind::Unsupported(kind.to_owned());
        let named = !field.options.is_empty() && field.options.iter().all(|option| !option.is_empty());
        let kind = match field.control.as_str() {
            _ if voluntary && !disclose => unsupported("voluntary disclosure"),
            _ if id.is_empty() => unsupported("unidentified"),
            // The same question repeats for each job and each school, and each needs its own answer.
            _ if in_a_list(&id) && entry_role(&id, field.entry).is_none() => unsupported("repeated entry"),
            "text" => text_kind(&id, &field.title, field.input.as_deref()),
            "textarea" => Kind::LongText,
            "dropdown" => Kind::Choice { options: Vec::new() },
            "radio" if named => Kind::Choice { options: field.options },
            "checkboxes" if named => Kind::Choices { options: field.options },
            "checkbox" => Kind::YesNo,
            "file" => Kind::File,
            "date" => Kind::Date,
            "prompt" => Kind::Search,
            _ => unsupported("unrecognized"),
        };
        let about_resume = |text: &str| {
            let text = text.to_lowercase();
            text.contains("resume") || text.contains("cv")
        };
        let resume = (kind == Kind::File && (about_resume(&id) || about_resume(&field.title))).then_some(Role::Resume);
        let role = resume.or_else(|| role_of(&id)).or_else(|| entry_role(&id, field.entry));
        Field {
            path: id,
            required: field.required,
            held: field.held,
            question: Question {
                title: field.title,
                kind,
                role,
            },
        }
    });
    Ok(fields.collect())
}

/// What a field is for, where Workday's own id for the field says it.
fn role_of(id: &str) -> Option<Role> {
    Some(match id {
        "name--legalName--firstName" => Role::LegalFirstName,
        "name--legalName--middleName" => Role::MiddleName,
        "name--legalName--lastName" => Role::LegalLastName,
        "name--preferredCheck" => Role::HasPreferredName,
        "name--preferredName--firstName" => Role::FirstName,
        "name--preferredName--lastName" => Role::LastName,
        "address--addressLine1" => Role::Street,
        "address--city" => Role::City,
        "address--countryRegion" => Role::Region,
        "address--postalCode" => Role::PostalCode,
        "country--country" => Role::Country,
        "phoneNumber--phoneType" => Role::PhoneKind,
        "phoneNumber--countryPhoneCode" => Role::PhoneCode,
        "selfIdentifiedDisabilityData--name" => Role::Name,
        "selfIdentifiedDisabilityData--dateSignedOn" => Role::Today,
        _ => return None,
    })
}

/// What a field of a job or of a school is for. `place` is the entry's place on the page, which
/// is also its place in the user's list.
fn entry_role(id: &str, place: Option<usize>) -> Option<Role> {
    let (entry, part) = id.split_once("--")?;
    let (list, _) = entry.rsplit_once('-')?;
    let place = place?;
    Some(match (list, part) {
        ("workExperience", "jobTitle") => Role::Work(place, WorkPart::Title),
        ("workExperience", "companyName") => Role::Work(place, WorkPart::Company),
        ("workExperience", "location") => Role::Work(place, WorkPart::Location),
        ("workExperience", "currentlyWorkHere") => Role::Work(place, WorkPart::Current),
        ("workExperience", "startDate") => Role::Work(place, WorkPart::From),
        ("workExperience", "endDate") => Role::Work(place, WorkPart::To),
        ("workExperience", "roleDescription") => Role::Work(place, WorkPart::Description),
        ("education", "schoolName" | "school") => Role::School(place, SchoolPart::Name),
        ("education", "degree") => Role::School(place, SchoolPart::Degree),
        ("education", "fieldOfStudy") => Role::School(place, SchoolPart::Field),
        ("education", "gradeAverage") => Role::School(place, SchoolPart::Grade),
        ("education", "firstYearAttended") => Role::School(place, SchoolPart::From),
        ("education", "lastYearAttended") => Role::School(place, SchoolPart::To),
        _ => return None,
    })
}

/// True for the pages that ask about gender, ethnicity, veteran status or disability.
/// Maverick leaves every field on them, unless the user's answers hold disclosures.
fn is_voluntary(page: &str, step: &str) -> bool {
    let step = step.to_lowercase();
    matches!(page, "applyFlowVoluntaryDisclosuresPage" | "applyFlowSelfIdentifyPage")
        || ["voluntary", "self identify", "self-identify", "disability"]
            .iter()
            .any(|word| step.contains(word))
}

/// True for the id of a field in a list the applicant can add entries to, such as
/// `workExperience-8--jobTitle`.
fn in_a_list(id: &str) -> bool {
    id.split("--")
        .next()
        .and_then(|list| list.rsplit_once('-'))
        .is_some_and(|(_, number)| !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit()))
}

fn text_kind(id: &str, title: &str, input: Option<&str>) -> Kind {
    let name = id.rsplit("--").next().unwrap_or_default().to_lowercase();
    let title = title.to_lowercase();
    if input == Some("email") || name.starts_with("email") || title.contains("email") {
        Kind::Email
    } else if input == Some("tel") || name == "phonenumber" || title == "phone number" {
        Kind::Phone
    } else {
        Kind::Text
    }
}

/// Opens each drop-down list once to read its options, and closes it again.
/// A list that does not open, or shows nothing, is left for the user.
async fn with_options(page: &Page, mut fields: Vec<Field>) -> Result<Vec<Field>, FillError> {
    for field in &mut fields {
        if !matches!(&field.question.kind, Kind::Choice { options } if options.is_empty()) {
            continue;
        }
        field.question.kind = match options(page, &field.path).await {
            Ok(options) if !options.is_empty() => Kind::Choice { options },
            Ok(_) | Err(Problem::Shows(_) | Problem::Mismatch(_)) => Kind::Unsupported("list that did not open".into()),
            Err(Problem::Page(error)) if error.ends_the_work() => {
                return Err(during(format!("reading the options of {:?}", field.question.title))(
                    error,
                ));
            }
            Err(Problem::Page(_)) => Kind::Unsupported("list that did not open".into()),
        };
    }
    Ok(fields)
}

/// The text of a list's options. The list is the element its button names in `aria-controls`,
/// and no other: a search list keeps the user's picked items in a `listbox` of its own.
const OPTIONS: &str = r#"((button) => {
    const list = document.getElementById(button?.getAttribute("aria-controls") ?? "");
    return [...(list?.querySelectorAll('[role="option"]') ?? [])].map((option) => option.textContent.replace(/\s+/g, " ").trim());
})"#;

async fn options(page: &Page, id: &str) -> Result<Vec<String>, Problem> {
    let button = dropdown(id);
    open(page, &button).await?;
    let listed = format!("{OPTIONS}({})", button.as_script());
    settles(page, &format!("{listed}.length > 0")).await?;
    let labels = page.evaluate(&listed).await?;
    page.press(Key::Escape).await?;
    if !settles(page, &format!("!({})", expanded(&button))).await? {
        return Err(Problem::Mismatch("the list did not close"));
    }
    let labels: Vec<String> = serde_json::from_value(labels).unwrap_or_default();
    // An empty list shows Workday's own placeholder as its first row.
    let placeholder = |label: &String| label.is_empty() || label.eq_ignore_ascii_case("select one");
    Ok(labels.into_iter().filter(|label| !placeholder(label)).collect())
}

fn expanded(button: &Locator) -> String {
    format!("({})?.getAttribute('aria-expanded') === 'true'", button.as_script())
}

/// Opens a drop-down list. A click right after other input can go unheard, so it is tried twice.
async fn open(page: &Page, button: &Locator) -> Result<(), Problem> {
    let expanded = expanded(button);
    for _ in 0..2 {
        if page.evaluate(&expanded).await? == true {
            return Ok(());
        }
        page.click(button).await?;
        if settles(page, &expanded).await? {
            return Ok(());
        }
    }
    Err(Problem::Mismatch("the list did not open"))
}

/// Saves what a fill saw of the page to `folder` as `workday-<time>.json` and returns the file.
///
/// The file holds the page's address, its step, each control's tag, type, role, ids and `aria-`
/// attributes, the questions and the option labels. It holds no field value and no typed text.
fn capture(folder: &Path, seen: &Value, read: &Result<Vec<Field>, FillError>) -> Result<PathBuf, String> {
    let read = match read {
        Ok(fields) => fields
            .iter()
            .map(|field| {
                json!({
                    "id": field.path,
                    "title": field.question.title,
                    "required": field.required,
                    "kind": format!("{:?}", field.question.kind),
                })
            })
            .collect(),
        Err(error) => json!({ "problem": error.to_string() }),
    };
    let now = Timestamp::now();
    let capture = json!({
        "system": "workday",
        "captured_at": now.to_string(),
        "address": seen["address"],
        "page": seen["page"],
        "step": seen["step"],
        "structure": seen["structure"],
        "read": read,
    });
    let file = folder.join(format!("workday-{}.json", now.strftime("%Y%m%dT%H%M%SZ")));
    let text = serde_json::to_string_pretty(&capture).map_err(|error| error.to_string())?;
    fs::create_dir_all(folder)
        .and_then(|()| fs::write(&file, text))
        .map_err(|error| format!("{}: {error}", file.display()))?;
    Ok(file)
}

/// How answers go into a Workday page.
struct Workday;

impl Enter for Workday {
    async fn enter(&self, page: &Page, field: &Field, answer: &Answer) -> Result<String, Problem> {
        enter(page, field, answer).await
    }
}

const TYPED: &str = "textarea, input:not([type]), input[type=\"text\"], input[type=\"email\"], \
    input[type=\"tel\"], input[type=\"number\"], input[type=\"url\"]";

/// Enters one answer and returns what the field shows afterwards.
async fn enter(page: &Page, field: &Field, answer: &Answer) -> Result<String, Problem> {
    let id = &field.path;
    match (answer, &field.question.kind) {
        (Answer::Text(text), Kind::Search) => search(page, id, text).await,
        (Answer::Text(text), kind) => {
            let text = match kind {
                Kind::Phone => national_number(text, shown_country_code(page).await?.as_deref())?,
                _ => text.clone(),
            };
            let input = within(id, TYPED);
            page.fill(&input, &text).await?;
            let shown = expect_value(page, &input, &text).await?;
            // Workday takes a typed value when the keyboard focus leaves the field.
            page.press(Key::Tab).await?;
            Ok(shown)
        }
        (Answer::YesNo(yes), _) => {
            tick(page, &within(id, "input[type=\"checkbox\"]"), *yes).await?;
            Ok(if *yes { "Yes" } else { "No" }.to_owned())
        }
        (Answer::Choice(option), _) => {
            let button = dropdown(id);
            if page.evaluate(&format!("!!({})", button.as_script())).await? != true {
                tick(page, &option_input(id, "radio", option), true).await?;
                return Ok(option.clone());
            }
            // The button can hold a no-break space where the list's row holds a plain one.
            let shows = format!(
                "({})?.textContent.replace(/\\s+/g, ' ').trim() === {}",
                button.as_script(),
                quoted(option)
            );
            if page.evaluate(&shows).await? == true {
                return Ok(option.clone());
            }
            // An earlier answer can make the page draw new questions, and a list that is open at
            // that moment loses its pick. On a live page the second try went through.
            for last in [false, true] {
                open(page, &button).await?;
                match choose(page, &button, option).await {
                    Err(Problem::Mismatch(_)) if !last => continue,
                    chosen => chosen?,
                }
                // A list still counts as open for a moment after a pick. On a live page, lists
                // stayed open when the fill moved on before that moment had passed.
                if settles(page, &format!("{shows} && !({})", expanded(&button))).await? {
                    return Ok(option.clone());
                }
            }
            Err(Problem::Shows(text_of(page, &button).await?))
        }
        (Answer::Choices(options), _) => {
            for option in options {
                tick(page, &option_input(id, "checkbox", option), true).await?;
            }
            Ok(options.join(", "))
        }
        (Answer::File(file), _) => {
            let name = file
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            // Workday lists the file's name once it has taken the file.
            let listed = Locator::script(
                format!("the uploaded file {name}"),
                format!(
                    "[...({})?.querySelectorAll('*') ?? []]\
                     .find((element) => element.children.length === 0 && element.textContent.includes({})) ?? null",
                    container(id),
                    quoted(&name)
                ),
            );
            if page.evaluate(&format!("!!({})", listed.as_script())).await? == true {
                return Ok(name);
            }
            // Workday keeps a file from an earlier application. Replacing it means deleting it first.
            let other = within(id, "[data-automation-id=\"file-upload-item\"]");
            if page.evaluate(&format!("!!({})", other.as_script())).await? == true {
                return Err(Problem::Mismatch(
                    "another file is attached already, so this one was not added",
                ));
            }
            page.upload(&within(id, "input[type=\"file\"]"), &[file]).await?;
            page.wait_for(&listed, UPLOAD_WAIT).await?;
            Ok(name)
        }
        (Answer::Date(date), _) => {
            // A date field has a box for each part it asks for. A job's month has no day.
            let parts = [
                ("Month", i32::from(date.month()), 2),
                ("Day", i32::from(date.day()), 2),
                ("Year", i32::from(date.year()), 4),
            ];
            let mut typed = Vec::new();
            for (part, number, width) in parts {
                let input = within(id, &format!("[data-automation-id=\"dateSection{part}-input\"]"));
                if page.evaluate(&format!("!!({})", input.as_script())).await? != true {
                    continue;
                }
                // The box holds the number without its leading zero.
                let wanted = number.to_string();
                let shows = format!("({})?.value === {}", input.as_script(), quoted(&wanted));
                if page.evaluate(&shows).await? != true {
                    // A date box reads key presses. On a live page, text put into a box that
                    // held a date changed the month and left a wrong day.
                    let focus = format!("((box) => {{ box.focus(); box.select?.(); }})({})", input.as_script());
                    page.evaluate(&focus).await?;
                    page.press_digits(&format!("{number:0width$}")).await?;
                    expect_value(page, &input, &wanted).await?;
                }
                typed.push(format!("{number:0width$}"));
            }
            if typed.is_empty() {
                return Err(Problem::Mismatch("the date field has no box to type in"));
            }
            page.press(Key::Tab).await?;
            Ok(typed.join("/"))
        }
    }
}

/// The labels of the items a search list holds as picked.
const PICKED: &str = r#"((field) => [...(field?.querySelectorAll('[data-automation-id="selectedItem"]') ?? [])].map((item) => {
    const option = item.querySelector('[data-automation-id="promptOption"]');
    return (option?.getAttribute("data-automation-label") ?? (option ?? item).textContent).replace(/\s+/g, " ").trim();
}))"#;

/// The rows of the search list that is open. A row that is not a leaf opens a list of its own.
const ROWS: &str = r#"[...document.querySelectorAll('[data-automation-id="activeListContainer"] [data-automation-id="menuItem"]')].map((row) => {
    const option = row.querySelector('[data-automation-id="promptOption"]');
    return {
        label: (option?.getAttribute("data-automation-label") ?? option?.textContent ?? "").replace(/\s+/g, " ").trim(),
        leaf: row.querySelector('[data-automation-id="promptLeafNode"]') !== null,
    };
})"#;

/// Picks an option of a search list: types the answer into the list's search box and presses
/// Enter. Returns the label of the picked option.
///
/// Workday picks the option itself when the search finds one. When the search finds several, the
/// list opens, and this clicks the row spelled like the answer. When the search finds none, the
/// list opens with every option. With no row spelled like the answer, nothing is picked.
async fn search(page: &Page, id: &str, wanted: &str) -> Result<String, Problem> {
    #[derive(Deserialize, PartialEq)]
    struct Row {
        label: String,
        leaf: bool,
    }
    #[derive(Deserialize, PartialEq)]
    struct Seen {
        picked: Vec<String>,
        rows: Vec<Row>,
        /// What the search box holds. Workday empties it once it has picked an option.
        typed: String,
    }
    let same = |label: &str| label.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
    let picked = format!("{PICKED}({})", container(id));
    let input = within(id, "input");
    let look = format!(
        "({{ picked: {picked}, rows: {ROWS}, typed: ({})?.value ?? '' }})",
        input.as_script()
    );
    let seen = |value: Value| {
        serde_json::from_value::<Seen>(value).unwrap_or(Seen {
            picked: Vec::new(),
            rows: Vec::new(),
            typed: String::new(),
        })
    };
    let before = seen(page.evaluate(&look).await?).picked;
    if let Some(label) = before.iter().find(|label| same(label) == same(wanted)) {
        return Ok(label.clone());
    }
    page.fill(&input, wanted).await?;
    page.press(Key::Enter).await?;

    // The answer comes from the server, and the list draws its rows piece by piece.
    let deadline = Instant::now() + FORM_WAIT;
    let (mut now, mut steady) = (seen(Value::Null), 0);
    while steady < STEADY_LOOKS && Instant::now() < deadline {
        tokio::time::sleep(PAGE_GAP).await;
        let next = seen(page.evaluate(&look).await?);
        if let Some(label) = next.picked.iter().find(|label| !before.contains(label)) {
            // Workday picked the one option the search found.
            return Ok(label.clone());
        }
        // No rows and an empty search box: Workday picked the option that was picked already.
        let answered = !next.rows.is_empty() || next.typed.is_empty();
        steady = if next == now && answered { steady + 1 } else { 1 };
        now = next;
    }
    if let (true, [label]) = (now.rows.is_empty() && now.typed.is_empty(), now.picked.as_slice()) {
        return Ok(label.clone());
    }
    let spelled = (0..now.rows.len()).find(|&row| now.rows[row].leaf && same(&now.rows[row].label) == same(wanted));
    let Some(row) = spelled else {
        // Tab closes the list and leaves the field as it was.
        page.press(Key::Tab).await?;
        return Err(Problem::Mismatch(
            "the search found no option, or several options and none of them spelled like the answer",
        ));
    };
    let option = Locator::script(
        format!("the option {:?}", now.rows[row].label),
        format!(
            "document.querySelectorAll('[data-automation-id=\"activeListContainer\"] [data-automation-id=\"menuItem\"]')[{row}]\
             ?.querySelector('[data-automation-id=\"promptOption\"]') ?? null"
        ),
    );
    page.click(&option).await?;
    let label = &now.rows[row].label;
    let held = settles(page, &format!("{picked}.includes({})", quoted(label))).await?;
    page.press(Key::Tab).await?;
    if held {
        Ok(label.clone())
    } else {
        let shown = seen(page.evaluate(&look).await?).picked;
        Err(Problem::Shows(shown.join(", ")))
    }
}

/// How many rows below the list's active row an option is. Negative when it is above, and null
/// when the list names no active row or has no such option.
const ROWS_AWAY: &str = r#"((button, wanted) => {
    const list = document.getElementById(button?.getAttribute("aria-controls") ?? "");
    const rows = [...(list?.querySelectorAll('[role="option"]') ?? [])];
    const active = rows.findIndex((row) => row.id !== "" && row.id === list.getAttribute("aria-activedescendant"));
    const target = rows.findIndex((row) => row.textContent.replace(/\s+/g, " ").trim() === wanted);
    return active < 0 || target < 0 ? null : target - active;
})"#;

/// Picks an option of an open list with the arrow keys and Enter.
///
/// A click is not used: on a live page a long list moved its rows between the look and the click,
/// and the click picked another row. The keys go by the row the list itself calls active.
async fn choose(page: &Page, button: &Locator, option: &str) -> Result<(), Problem> {
    let away = format!("{ROWS_AWAY}({}, {})", button.as_script(), quoted(option));
    settles(page, &format!("{away} !== null")).await?;
    let rows = page.evaluate(&away).await?.as_i64();
    for _ in 0..rows.unwrap_or_default().unsigned_abs() {
        let key = if rows > Some(0) { Key::ArrowDown } else { Key::ArrowUp };
        page.press(key).await?;
    }
    if rows.is_none() || !settles(page, &format!("{away} === 0")).await? {
        // Escape closes the list and leaves the field as it was.
        page.press(Key::Escape).await?;
        return Err(Problem::Mismatch("the list did not move to the option"));
    }
    page.press(Key::Enter).await?;
    Ok(())
}

/// The phone number as Workday wants it typed: without the country code, which has its own field.
/// A number with another country's code is not typed at all.
fn national_number(phone: &str, shown_code: Option<&str>) -> Result<String, Problem> {
    let phone = phone.trim();
    let Some(international) = phone.strip_prefix('+') else {
        return Ok(phone.to_owned());
    };
    let code = shown_code.ok_or(Problem::Mismatch(
        "the form does not show which country code it expects, so the phone number was not typed",
    ))?;
    international
        .strip_prefix(code)
        .map(|rest| rest.trim_start_matches([' ', '-', '.']))
        .filter(|rest| !rest.is_empty())
        .map(str::to_owned)
        .ok_or(Problem::Mismatch(
            "the phone number's country code is not the one the form shows",
        ))
}

/// The digits of the country code the form shows beside the phone number, as in `(+1)`.
async fn shown_country_code(page: &Page) -> Result<Option<String>, PageError> {
    let code = page
        .evaluate(
            "(document.querySelector('[data-fkit-id$=\"countryPhoneCode\"]')?.textContent ?? '')\
             .match(/\\(\\+(\\d+)\\)/)?.[1] ?? null",
        )
        .await?;
    Ok(code.as_str().map(str::to_owned))
}

/// Sets a check box or a radio button, unless it is set already. A second click would undo it.
async fn tick(page: &Page, input: &Locator, wanted: bool) -> Result<(), Problem> {
    let set = format!("({})?.checked === {wanted}", input.as_script());
    if page.evaluate(&set).await? != true {
        match page.click(input).await {
            // Workday draws its own box over the real one, so the click goes to the label.
            // A label that holds a link is not clicked: the click could leave the page.
            Err(PageError::Covered { .. }) => {
                let label = Locator::script(
                    "the option's label",
                    format!(
                        "((label) => (label && !label.querySelector('a') ? label : null))(({})?.labels?.[0])",
                        input.as_script()
                    ),
                );
                page.click(&label).await?
            }
            clicked => clicked?,
        }
    }
    if settles(page, &set).await? {
        Ok(())
    } else {
        Err(Problem::Mismatch("the option did not stay as set"))
    }
}

/// The element marked with a `data-automation-id`.
fn marked(id: &str) -> Locator {
    Locator::css(&format!("[data-automation-id=\"{id}\"]"))
}

/// A JavaScript expression for the field's container. `fields` lets only plain ids through.
fn container(id: &str) -> String {
    format!("document.querySelector('[data-fkit-id=\"{id}\"]')?.closest('[data-automation-id^=\"formField-\"]')")
}

/// The first element inside a field that matches a CSS selector.
fn within(id: &str, selector: &str) -> Locator {
    Locator::script(
        format!("{selector} in the field {id}"),
        format!("({}?.querySelector({}) ?? null)", container(id), quoted(selector)),
    )
}

/// The button of a field's drop-down list.
fn dropdown(id: &str) -> Locator {
    within(id, "button[aria-haspopup=\"listbox\"]")
}

/// The radio button or check box of a field whose label is `option`.
fn option_input(id: &str, kind: &str, option: &str) -> Locator {
    Locator::script(
        format!("the option {option:?}"),
        format!(
            "([...({}?.querySelectorAll('input[type=\"{kind}\"]') ?? [])]\
             .find((input) => (input.labels?.[0]?.textContent ?? '').replace(/\\s+/g, ' ').trim() === {}) ?? null)",
            container(id),
            quoted(option)
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_posting_address_is_recognized_on_both_hosts_and_at_each_stage() {
        let own = "https://example-co.wd5.myworkdayjobs.com/Careers/job/Springfield-IL/Backend-Engineer_R-1042";
        fn at(address: &str) -> Option<(&str, &str, &str, Stage)> {
            Address::parse(address).map(|at| (at.tenant, at.site, at.posting, at.stage))
        }
        let job = ("example-co", "Careers", "Backend-Engineer_R-1042");
        assert_eq!(at(own), Some((job.0, job.1, job.2, Stage::Posting)));
        assert_eq!(
            at("https://example-co.wd5.myworkdayjobs.com/Careers/job/Backend-Engineer_R-1042?source=x#top"),
            Some((job.0, job.1, job.2, Stage::Posting))
        );
        assert_eq!(
            at(&format!("{own}/apply")),
            Some((job.0, job.1, job.2, Stage::Choosing))
        );
        assert_eq!(
            at(
                "https://example-co.wd5.myworkdayjobs.com/en-US/Careers/job/Springfield-IL/Backend-Engineer_R-1042/apply/applyManually"
            ),
            Some((job.0, job.1, job.2, Stage::Applying))
        );
        assert_eq!(
            at(
                "https://example-co.wd5.myworkdayjobs.com/Careers/job/Backend-Engineer_R-1042/apply/useMyLastApplication"
            ),
            Some((job.0, job.1, job.2, Stage::Applying))
        );
        // A site can be named `apply`.
        assert_eq!(
            at("https://example-co.wd12.myworkdayjobs.com/apply/job/Remote/Backend-Engineer_R-1042"),
            Some((job.0, "apply", job.2, Stage::Posting))
        );
        let shared =
            "https://wd1.myworkdaysite.com/recruiting/example-co/Careers/job/Springfield-IL/Backend-Engineer_R-1042";
        assert_eq!(at(shared), Some((job.0, job.1, job.2, Stage::Posting)));
        assert_eq!(
            at("https://wd1.myworkdaysite.com/fr-CA/recruiting/example-co/Careers/job/Backend-Engineer_R-1042/apply"),
            Some((job.0, job.1, job.2, Stage::Choosing))
        );

        for other in [
            "https://example-co.wd5.myworkdayjobs.com/Careers",
            "https://example-co.wd5.myworkdayjobs.com/Careers/login",
            "https://example-co.wd5.myworkdayjobs.com/Careers/job",
            "https://example-co.wd5.myworkdayjobs.com/Careers/job/a/b/c",
            "https://example-co.wd5.myworkdayjobs.com/Careers/job/a/b/apply/c/d",
            "https://example-co.wd5.myworkdayjobs.com/Careers/jobTasks/completed/application",
            "https://wd1.myworkdaysite.com/example-co/Careers/job/Backend-Engineer_R-1042",
            "https://example-co.myworkdayjobs.com/Careers/job/Backend-Engineer_R-1042",
            "https://example-co.wd5.myworkdayjobs.com.example.com/Careers/job/Backend-Engineer_R-1042",
            "https://example.com/Careers/job/Springfield-IL/Backend-Engineer_R-1042",
            "http://example-co.wd5.myworkdayjobs.com/Careers/job/Backend-Engineer_R-1042",
        ] {
            assert!(!is_posting(other), "{other}");
        }
    }

    #[test]
    fn every_address_of_a_posting_gives_the_same_posting_address() {
        let posting = "https://example-co.wd5.myworkdayjobs.com/careers/job/backend-engineer_r-1042";
        for address in [
            "https://example-co.wd5.myworkdayjobs.com/Careers/job/Springfield-IL/Backend-Engineer_R-1042",
            "https://example-co.wd5.myworkdayjobs.com/careers/job/springfield-il/backend-engineer_r-1042?utm_source=x",
            "https://example-co.wd5.myworkdayjobs.com/en-US/Careers/job/Backend-Engineer_R-1042/apply/applyManually",
        ] {
            assert_eq!(posting_address(address).as_deref(), Some(posting), "{address}");
            assert_eq!(form_address(address), Some(format!("{posting}/apply")), "{address}");
        }
        assert_eq!(
            posting_address("https://wd1.myworkdaysite.com/en-US/recruiting/example-co/Careers/job/Remote/Backend-Engineer_R-1042/apply").as_deref(),
            Some("https://wd1.myworkdaysite.com/recruiting/example-co/careers/job/backend-engineer_r-1042")
        );
        let at = Address::parse(
            "https://wd1.myworkdaysite.com/recruiting/example-co/Careers/job/Remote/Backend-Engineer_R-1042",
        );
        assert_eq!(
            at.unwrap().job_data(),
            "/wday/cxs/example-co/Careers/job/Backend-Engineer_R-1042"
        );
    }

    #[test]
    fn the_job_reply_becomes_the_job() {
        let reply = json!({
            "status": 200,
            "posted": true,
            "title": " Backend Engineer ",
            "location": "Springfield, IL",
            "description": "<p>Build <b>fast</b> things &amp; ship.</p><ul><li>Rust</li></ul>",
        });
        assert_eq!(
            job(&reply),
            Ok(Job {
                job: "Backend Engineer, Springfield, IL".into(),
                description: "Build fast things & ship.\nRust".into(),
            })
        );
        assert_eq!(
            job(&json!({ "status": 200, "title": "Backend Engineer" })).unwrap().job,
            "Backend Engineer"
        );
    }

    #[test]
    fn a_posting_workday_no_longer_lists_is_closed() {
        assert_eq!(job(&json!({ "status": 404 })), Err(FillError::Closed));
        assert_eq!(job(&json!({ "status": 403 })), Err(FillError::Closed));
        assert_eq!(
            job(&json!({ "status": 200, "posted": false, "title": "Backend Engineer" })),
            Err(FillError::Closed)
        );
        assert!(matches!(
            job(&json!({ "status": 500 })),
            Err(FillError::UnexpectedForm(_))
        ));
        assert!(matches!(job(&json!("nonsense")), Err(FillError::UnexpectedForm(_))));
    }

    fn seen_field(id: &str, title: &str, control: &str) -> Value {
        json!({ "id": id, "unique": true, "title": title, "required": false, "control": control, "input": null, "options": [] })
    }

    fn kinds(page: &str, step: &str, seen: Vec<Value>) -> Vec<Kind> {
        let seen = json!({ "page": page, "step": step, "fields": seen });
        fields(&seen, false)
            .unwrap()
            .into_iter()
            .map(|field| field.question.kind)
            .collect()
    }

    fn unsupported(kind: &str) -> Kind {
        Kind::Unsupported(kind.into())
    }

    #[test]
    fn what_the_page_shows_becomes_fields() {
        let mut radio = seen_field(
            "previousWorker--candidateIsPreviousWorker",
            "Worked here before?",
            "radio",
        );
        radio["options"] = json!(["Yes", "No"]);
        radio["required"] = json!(true);
        let mut email = seen_field("contact--other", "Recruiter contact", "text");
        email["input"] = json!("email");
        let seen = json!({ "page": "applyFlowMyInfoPage", "step": "My Information", "fields": [
            seen_field("name--legalName--firstName", "First Name", "text"),
            seen_field("phoneNumber--phoneNumber", "Phone Number", "text"),
            seen_field("phoneNumber--extension", "Phone Extension", "text"),
            email,
            seen_field("primaryQuestionnaire--a1b2", "Why this job?", "textarea"),
            seen_field("address--countryRegion", "State", "dropdown"),
            radio,
            seen_field("name--preferredCheck", "I have a preferred name", "checkbox"),
            seen_field("resumeAttachments--attachments", "Upload a file", "file"),
            seen_field("otherAttachments--attachments", "Cover letter", "file"),
        ] });
        let fields = fields(&seen, false).unwrap();
        let kinds: Vec<&Kind> = fields.iter().map(|field| &field.question.kind).collect();
        assert_eq!(
            kinds,
            [
                &Kind::Text,
                &Kind::Phone,
                &Kind::Email,
                &Kind::LongText,
                &Kind::Choice { options: vec![] },
                &Kind::Choice {
                    options: vec!["Yes".into(), "No".into()]
                },
                &Kind::YesNo,
                &Kind::File,
                &Kind::File,
            ]
        );
        assert_eq!(fields[0].path, "name--legalName--firstName");
        assert_eq!(fields[0].question.title, "First Name");
        assert!(fields[5].required && !fields[0].required);
        assert_eq!(fields[7].question.role, Some(Role::Resume));
        assert_eq!(fields[8].question.role, None);
        // Workday's own ids say what these are for, so no rule about a phone or a name answers them.
        assert_eq!(fields[0].question.role, Some(Role::LegalFirstName));
        assert_eq!(fields[4].question.role, Some(Role::Region));
        assert!(fields.iter().all(|field| field.path != "phoneNumber--extension"));
    }

    #[test]
    fn a_field_of_a_job_or_a_school_knows_its_place_in_the_list() {
        let entry = |id: &str, title: &str, control: &str, place: usize| {
            let mut field = seen_field(id, title, control);
            field["entry"] = json!(place);
            field
        };
        let seen = json!({ "page": "applyFlowMyExpPage", "step": "My Experience", "fields": [
            entry("workExperience-19--jobTitle", "Job Title", "text", 0),
            entry("workExperience-19--startDate", "From", "date", 0),
            entry("workExperience-23--currentlyWorkHere", "I currently work here", "checkbox", 1),
            entry("education-20--degree", "Degree", "dropdown", 0),
            entry("education-20--fieldOfStudy", "Field of Study", "prompt", 0),
            entry("language-4--language", "Language", "dropdown", 0),
        ] });
        let read: Vec<(Kind, Option<Role>)> = fields(&seen, false)
            .unwrap()
            .into_iter()
            .map(|field| (field.question.kind, field.question.role))
            .collect();
        assert_eq!(
            read,
            [
                (Kind::Text, Some(Role::Work(0, WorkPart::Title))),
                (Kind::Date, Some(Role::Work(0, WorkPart::From))),
                (Kind::YesNo, Some(Role::Work(1, WorkPart::Current))),
                (
                    Kind::Choice { options: vec![] },
                    Some(Role::School(0, SchoolPart::Degree))
                ),
                (Kind::Search, Some(Role::School(0, SchoolPart::Field))),
                (unsupported("repeated entry"), None),
            ]
        );
    }

    #[test]
    fn a_control_this_code_does_not_know_is_left_for_the_user() {
        let mut twice = seen_field("name--legalName--lastName", "Last Name", "text");
        twice["unique"] = json!(false);
        let mut absent = seen_field("", "Middle Name", "text");
        absent["id"] = Value::Null;
        let mut unnamed = seen_field("source--pick", "Pick one", "radio");
        unnamed["options"] = json!(["Yes", ""]);
        assert_eq!(
            kinds(
                "applyFlowMyExpPage",
                "My Experience",
                vec![
                    seen_field("startDate--date", "Start date", "date"),
                    seen_field("source--source", "How Did You Hear About Us?", "prompt"),
                    seen_field("signature--draw", "Sign here", "unknown"),
                    seen_field("workExperience-8--jobTitle", "Job Title", "text"),
                    seen_field("a\"] , body [x=\"", "Name", "text"),
                    twice,
                    absent,
                    unnamed,
                ]
            ),
            [
                Kind::Date,
                Kind::Search,
                unsupported("unrecognized"),
                unsupported("repeated entry"),
                unsupported("unidentified"),
                unsupported("unidentified"),
                unsupported("unidentified"),
                unsupported("unrecognized"),
            ]
        );
        assert!(matches!(
            fields(&json!({ "fields": [] }), false),
            Err(FillError::UnexpectedForm(_))
        ));
    }

    #[test]
    fn every_field_of_a_voluntary_disclosure_page_is_left_for_the_user() {
        let page = || {
            vec![
                seen_field("personalInfoUS--gender", "Gender", "dropdown"),
                seen_field("selfIdentifiedDisabilityData--name", "Name", "text"),
            ]
        };
        let left = [unsupported("voluntary disclosure"), unsupported("voluntary disclosure")];
        assert_eq!(kinds("applyFlowVoluntaryDisclosuresPage", "", page()), left);
        assert_eq!(kinds("applyFlowSelfIdentifyPage", "", page()), left);
        assert_eq!(kinds("", "Voluntary Disclosures", page()), left);
        assert_eq!(kinds("", "Self Identify", page()), left);
        assert_ne!(kinds("applyFlowMyInfoPage", "My Information", page()), left);

        // A user who wrote down what to disclose gets these pages filled like any other.
        let seen = json!({ "page": "applyFlowSelfIdentifyPage", "step": "Self Identify", "fields": page() });
        let disclosed: Vec<(Kind, Option<Role>)> = fields(&seen, true)
            .unwrap()
            .into_iter()
            .map(|field| (field.question.kind, field.question.role))
            .collect();
        assert_eq!(
            disclosed,
            [(Kind::Choice { options: vec![] }, None), (Kind::Text, Some(Role::Name))]
        );
    }

    #[test]
    fn a_phone_number_is_typed_without_the_country_code_the_form_shows() {
        let typed = |phone, code| national_number(phone, code).ok();
        assert_eq!(typed("+1 415 555 0100", Some("1")).as_deref(), Some("415 555 0100"));
        assert_eq!(typed("+44-20-5550-0100", Some("44")).as_deref(), Some("20-5550-0100"));
        assert_eq!(typed(" 415 555 0100 ", None).as_deref(), Some("415 555 0100"));
        assert_eq!(typed("+1 415 555 0100", Some("44")), None);
        assert_eq!(typed("+1 415 555 0100", None), None);
        assert_eq!(typed("+1", Some("1")), None);
    }

    #[test]
    fn a_capture_names_the_fields_and_what_was_read() {
        let folder = tempfile::tempdir().unwrap();
        let seen = json!({
            "address": "https://example-co.wd5.myworkdayjobs.com/en-US/Careers/job/Backend-Engineer_R-1042/apply/applyManually",
            "page": "applyFlowMyInfoPage",
            "step": "My Information",
            "structure": { "fields": [{ "automationId": "formField-firstName" }], "other": [] },
        });
        let read = Ok(vec![Field {
            held: false,
            path: "address--countryRegion".into(),
            required: true,
            question: Question {
                title: "State".into(),
                kind: Kind::Choice {
                    options: vec!["Illinois".into()],
                },
                role: None,
            },
        }]);
        let file = capture(&folder.path().join("captures"), &seen, &read).unwrap();
        let name = file.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with("workday-") && name.ends_with("Z.json"), "{name}");
        let saved: Value = serde_json::from_str(&fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(saved["step"], "My Information");
        assert_eq!(saved["structure"], seen["structure"]);
        assert_eq!(saved["read"][0]["title"], "State");
        assert!(saved["read"][0]["kind"].as_str().unwrap().contains("Illinois"));

        let failed = Err(FillError::UnexpectedForm("no fields".into()));
        let file = capture(folder.path(), &seen, &failed).unwrap();
        let saved: Value = serde_json::from_str(&fs::read_to_string(&file).unwrap()).unwrap();
        assert!(saved["read"]["problem"].as_str().unwrap().contains("no fields"));
    }
}
