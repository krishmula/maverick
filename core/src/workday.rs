//! Fills the page of a Workday application that is on screen and stops. It never signs in,
//! never moves to the next page and never submits.
//!
//! Most employers show a Workday application only to an account on their own Workday site, and
//! this code was written without one. So its parts stand on different ground:
//!
//! - Checked against live sites: the address shapes, the job's public data, the choice between
//!   "Autofill with Resume", "Apply Manually" and "Use My Last Application", and the sign-in wall.
//! - Checked against the two live sites found that ask for no account, on their first page only
//!   ("My Information"), with made-up answers and without saving the page: reading the fields,
//!   typing text, an email and a phone number, a radio group, a single check box, and reading and
//!   picking from a drop-down list.
//! - NOT checked against a live page: every later page of an application, a signed-in session,
//!   file uploads, groups of check boxes, long text, and `SUBMITTED`. These rest on two sources:
//!   Workday's own public page scripts (`candidate-experience-jobs/cx-jobs.min.js` and
//!   `candidate-experience-apply-flow/candidate-experience-apply-flow.min.js`, release 2026.47.19),
//!   which name the `data-automation-id` values and draw the controls, and the atsfill project's
//!   notes on the wizard as it saw it signed in (<https://shbernal.github.io/atsfill/adapters/workday/>).
//!   The tests run these parts against a reconstruction of that markup, not against Workday.
//!
//! Each fill of an application page saves the page's structure to a file, so the next version of
//! this code can be built on real markup. See `capture`.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use jiff::Timestamp;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::answers::{Answer, Answers, Kind, Question, Role};
use crate::apply::{
    self, Enter, Field, FillError, Form, Problem, Report, during, expect_value, plain_text, quoted, settles, text_of,
};
use crate::page::{Key, Locator, Page, PageError};

const PAGE_WAIT: Duration = Duration::from_secs(30);
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

/// Fills the page of the application that is on screen and reports on each of its fields.
/// The tab must be on a Workday posting, on the choice of how to apply, or inside the application.
///
/// From a posting it opens the application and chooses "Apply Manually". When Workday then asks
/// for an account, it stops with `FillError::SignInNeeded`: the user signs in and fills again.
/// It never clicks "Save and Continue".
pub async fn fill(page: &Page, answers: &Answers) -> Result<Report, FillError> {
    let captures = crate::data_dir().map(|dir| dir.join("captures"));
    fill_saving_captures(page, answers, captures.as_deref()).await
}

/// `fill`, with the folder for captures chosen by the caller. With none, no capture is saved.
pub async fn fill_saving_captures(
    page: &Page,
    answers: &Answers,
    captures: Option<&Path>,
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
    if screen == Screen::Wall {
        return Err(FillError::SignInNeeded { system: "Workday" });
    }
    fill_page(page, answers, job, captures).await
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
        let screen = match seen["screen"].as_str() {
            Some("wall") => Screen::Wall,
            Some("choice") => Screen::Choice,
            Some("page") => Screen::Page,
            _ => Screen::Loading,
        };
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

/// Fills the application page on screen: reads its fields from the page itself, saves a capture
/// of its structure under `captures`, then enters the answers. It never leaves the page.
///
/// Only partly verified against live Workday pages. See the top of this module.
pub async fn fill_page(page: &Page, answers: &Answers, job: Job, captures: Option<&Path>) -> Result<Report, FillError> {
    let seen = page.evaluate(READ).await.map_err(during("reading the page's fields"))?;
    let read = match fields(&seen) {
        Ok(fields) => with_options(page, fields).await,
        Err(error) => Err(error),
    };
    let unverified = "Check every answer: most of Maverick's Workday filling is not verified on live pages yet.";
    let note = match captures.map(|folder| capture(folder, &seen, &read)) {
        Some(Ok(file)) => format!(
            "{unverified} The structure of this page, without your answers, is saved in {}.",
            file.display()
        ),
        Some(Err(problem)) => format!("{unverified} The structure of this page could not be saved: {problem}."),
        None => unverified.to_owned(),
    };
    let fields = read.map_err(|error| match error {
        FillError::UnexpectedForm(problem) => FillError::UnexpectedForm(format!("{problem}. {note}")),
        other => other,
    })?;
    let form = Form {
        job: job.job,
        description: job.description,
        fields,
    };
    let mut report = apply::fill_form(page, answers, form, &Workday).await?;
    report.note = Some(note);
    Ok(report)
}

/// Reads the page in one pass: the fields as this module understands them, and the page's
/// structure for a capture. It reads no field's value and no text the user typed.
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
        return {
            id,
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
pub fn fields(seen: &Value) -> Result<Vec<Field>, FillError> {
    #[derive(Deserialize)]
    struct Seen {
        page: String,
        step: String,
        fields: Vec<SeenField>,
    }
    #[derive(Deserialize)]
    struct SeenField {
        id: Option<String>,
        unique: bool,
        title: String,
        required: bool,
        control: String,
        input: Option<String>,
        options: Vec<String>,
    }

    let seen = Seen::deserialize(seen).map_err(|error| FillError::UnexpectedForm(error.to_string()))?;
    let voluntary = is_voluntary(&seen.page, &seen.step);
    let fields = seen.fields.into_iter().map(|field| {
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
            _ if voluntary => unsupported("voluntary disclosure"),
            _ if id.is_empty() => unsupported("unidentified"),
            // The same question repeats for each job and each school, and each needs its own answer.
            _ if in_a_list(&id) => unsupported("repeated entry"),
            "text" => text_kind(&id, &field.title, field.input.as_deref()),
            "textarea" => Kind::LongText,
            "dropdown" => Kind::Choice { options: Vec::new() },
            "radio" if named => Kind::Choice { options: field.options },
            "checkboxes" if named => Kind::Choices { options: field.options },
            "checkbox" => Kind::YesNo,
            "file" => Kind::File,
            "date" => unsupported("date"),
            "prompt" => unsupported("search list"),
            _ => unsupported("unrecognized"),
        };
        let about_resume = |text: &str| {
            let text = text.to_lowercase();
            text.contains("resume") || text.contains("cv")
        };
        let role = (kind == Kind::File && (about_resume(&id) || about_resume(&field.title))).then_some(Role::Resume);
        Field {
            path: id,
            required: field.required,
            question: Question {
                title: field.title,
                kind,
                role,
            },
        }
    });
    Ok(fields.collect())
}

/// True for the pages that ask about gender, ethnicity, veteran status or disability.
/// Maverick leaves every field on them.
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
            Err(Problem::Page(PageError::Bridge(error))) => {
                return Err(during(format!("reading the options of {:?}", field.question.title))(
                    PageError::Bridge(error),
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
            let shows = format!("({})?.textContent.trim() === {}", button.as_script(), quoted(option));
            if page.evaluate(&shows).await? == true {
                return Ok(option.clone());
            }
            open(page, &button).await?;
            choose(page, &button, option).await?;
            // A list still counts as open for a moment after a pick. On a live page, lists stayed
            // open when the fill moved on before that moment had passed.
            if settles(page, &format!("{shows} && !({})", expanded(&button))).await? {
                Ok(option.clone())
            } else {
                Err(Problem::Shows(text_of(page, &button).await?))
            }
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
        // No date field is offered for an answer yet. See `fields`.
        (Answer::Date(_), _) => Err(Problem::Mismatch("Maverick cannot enter a date on Workday yet")),
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
        fields(&seen)
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
        let fields = fields(&seen).unwrap();
        let kinds: Vec<&Kind> = fields.iter().map(|field| &field.question.kind).collect();
        assert_eq!(
            kinds,
            [
                &Kind::Text,
                &Kind::Phone,
                &Kind::Text,
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
        assert!(fields[6].required && !fields[0].required);
        assert_eq!(fields[8].question.role, Some(Role::Resume));
        assert_eq!(fields[9].question.role, None);
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
                unsupported("date"),
                unsupported("search list"),
                unsupported("unrecognized"),
                unsupported("repeated entry"),
                unsupported("unidentified"),
                unsupported("unidentified"),
                unsupported("unidentified"),
                unsupported("unrecognized"),
            ]
        );
        assert!(matches!(
            fields(&json!({ "fields": [] })),
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
