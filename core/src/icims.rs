//! Fills an application on an iCIMS career site step by step, and stops at the last step.
//! It never submits.
//!
//! An iCIMS site draws its pages in a frame of the same site, so every script here looks inside
//! that frame. The application lists its steps as "Step 1 of 3", and the last step only confirms
//! the application. Maverick fills each step before it, presses that step's own button, and
//! stops on the last one.
//!
//! Every application is behind an account at the employer, kept by iCIMS at `login.icims.com`.
//! The user keeps one password for Workday and iCIMS, so Maverick signs in with the Workday
//! sign-in in the Keychain.
//!
//! Verified on one live application: the email step, the sign-in, the passkey offer, the
//! profile step and the drop-down lists that load their rows from the server. Creating an
//! account, uploading a resume and the last step are not verified.

use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::Value;

use crate::answers::{Answer, Answers, Kind, Question, Role, Unanswered};
use crate::apply::{
    self, End, Enter, Field, FieldResult, FillError, Form, Outcome, Problem, Report, during, quoted, settles,
};
use crate::credentials::{Keychain, SignIn, SignIns};
use crate::page::{Locator, Page};
use crate::workday;

/// The Keychain item of the user's one password for Workday and iCIMS.
pub const SIGN_IN_SERVICE: &str = workday::SIGN_IN_SERVICE;

/// What iCIMS shows once an application went through. Not seen on a live site yet.
pub const SUBMITTED: &str = ".iCIMS_ConfirmationMessage, .iCIMS_Steps_Last.iCIMS_Steps_Completed";

/// How long a screen gets to change after Maverick acted on it.
const SCREEN_WAIT: Duration = Duration::from_secs(30);
const LOOK_GAP: Duration = Duration::from_millis(400);
/// How often in a row a screen must look the same before it counts.
const STEADY_LOOKS: u32 = 3;
/// The most screens one fill goes through: the posting, the email, the sign-in and the steps.
const MOST_SCREENS: usize = 14;
const LIST_WAIT: Duration = Duration::from_secs(8);
const UPLOAD_WAIT: Duration = Duration::from_secs(30);

const UNVERIFIED: &str = "Check every answer: Maverick's iCIMS filling is verified on one live application only.";

/// The document that holds the application: the frame's, or the tab's own when there is none.
const DOC: &str = r#"(document.getElementById("icims_content_iframe")?.contentDocument ?? document)"#;

/// True for an address on an iCIMS career site that names a posting.
pub fn is_posting(address: &str) -> bool {
    posting(address).is_some()
}

/// The posting's page, `https://<site>.icims.com/jobs/<id>/job`.
pub fn posting_address(address: &str) -> Option<String> {
    posting(address).map(|(host, id)| format!("https://{host}/jobs/{id}/job"))
}

/// The form opens from the posting's page: iCIMS accepts the apply link only with a code that
/// the posting's page makes for it.
pub fn form_address(address: &str) -> Option<String> {
    posting_address(address)
}

/// The site and the posting's number.
fn posting(address: &str) -> Option<(&str, &str)> {
    let rest = address.strip_prefix("https://")?;
    let (host, path) = rest.split_once('/')?;
    if !host.ends_with(".icims.com") || host == "login.icims.com" {
        return None;
    }
    let id = path.strip_prefix("jobs/")?.split(['/', '?', '#']).next()?;
    (!id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit())).then_some((host, id))
}

/// Fills the application in a tab on an iCIMS posting, or anywhere inside its application.
pub async fn fill(page: &Page, answers: &Answers) -> Result<Report, FillError> {
    fill_with(page, answers, &Keychain).await
}

/// `fill`, with the source of the sign-in chosen by the caller.
pub async fn fill_with(page: &Page, answers: &Answers, sign_ins: &impl SignIns) -> Result<Report, FillError> {
    let mut whole = Report {
        outcomes: Vec::new(),
        model_problem: None,
        note: None,
        end: End::Review,
    };
    let mut job = Job::default();
    let mut notes: Vec<String> = Vec::new();
    let mut filled: Vec<String> = Vec::new();
    let mut tried = Tried::default();
    let mut last: Option<Screen> = None;
    whole.end = 'screens: {
        for _ in 0..MOST_SCREENS {
            let screen = settled(page, last.as_ref()).await?;
            last = Some(screen.clone());
            match screen {
                Screen::Posting {
                    apply,
                    title,
                    description,
                } => {
                    job = Job { title, description };
                    if std::mem::replace(&mut tried.apply, true) {
                        break 'screens stopped(
                            "the posting's page showed again after Maverick opened the application",
                        );
                    }
                    page.navigate(&apply).await.map_err(during("opening the application"))?;
                }
                Screen::Stale => {
                    break 'screens stopped(
                        "iCIMS refused the link to the application. Open the posting again and click Fill form.",
                    );
                }
                Screen::Email { consent } => {
                    if std::mem::replace(&mut tried.email, true) {
                        break 'screens stopped("iCIMS asked for the email again");
                    }
                    let sign_in = sign_in(sign_ins).await?;
                    if consent > 0 && !answers.standing.agrees_to_terms {
                        break 'screens stopped("iCIMS asks you to accept its privacy notice before you sign in");
                    }
                    enter_email(page, &sign_in.account).await?;
                    if consent > 0 {
                        notes.push(
                            "Maverick accepted the site's privacy notice, as your standing answers allow.".into(),
                        );
                    }
                }
                Screen::Password => {
                    if std::mem::replace(&mut tried.password, true) {
                        return Err(FillError::SignIn(
                            "iCIMS refused your Workday password at sign-in. Sign in in that tab yourself, \
                             then click Fill form again."
                                .into(),
                        ));
                    }
                    let sign_in = sign_in(sign_ins).await?;
                    type_password(page, &sign_in).await?;
                }
                Screen::NewAccount => {
                    if std::mem::replace(&mut tried.account, true) {
                        return Err(FillError::SignIn(
                            "iCIMS did not take the new account. Finish it in that tab, then click Fill form again."
                                .into(),
                        ));
                    }
                    let sign_in = sign_in(sign_ins).await?;
                    type_password(page, &sign_in).await?;
                    notes.push(format!(
                        "Maverick created an iCIMS account for {} with your Workday password.",
                        sign_in.account
                    ));
                }
                Screen::Passkey => {
                    page.click(&Locator::css(
                        r#"button[value="snooze-enrollment"], button[value="refuse-add-device"]"#,
                    ))
                    .await
                    .map_err(during("declining iCIMS's offer of a passkey"))?;
                }
                Screen::Auth { says } => {
                    return Err(FillError::SignIn(format!(
                        "Maverick stopped at iCIMS's sign-in, which says {says:?}. Finish it in that tab, \
                         then click Fill form again."
                    )));
                }
                Screen::Step { at, of, name } => {
                    if job.title.is_empty() {
                        job.title = heading(page).await?;
                    }
                    let report = fill_step(page, answers, &job).await?;
                    whole.model_problem = whole.model_problem.or(report.model_problem);
                    let count =
                        |holds: fn(&Outcome) -> bool| report.outcomes.iter().filter(|outcome| holds(outcome)).count();
                    let failed = count(|outcome| matches!(outcome.result, FieldResult::Failed(_)));
                    let open = count(Outcome::is_open);
                    whole.outcomes.extend(report.outcomes);
                    filled.push(name.clone());
                    if at >= of && failed == 0 && open == 0 {
                        break 'screens Ok(End::Review);
                    }
                    if failed > 0 {
                        break 'screens stopped(&format!(
                            "Maverick could not enter {failed} of its answers on \"{name}\". Fix them in the tab, \
                             then click Fill form again"
                        ));
                    }
                    if open > 0 {
                        break 'screens stopped(&format!(
                            "\"{name}\" has {open} required question{} Maverick has no answer for",
                            if open == 1 { "" } else { "s" }
                        ));
                    }
                    page.click(&next_button())
                        .await
                        .map_err(during(format!("pressing the button at the foot of \"{name}\"")))?;
                    let errors = refused(page, at).await?;
                    if !errors.is_empty() {
                        break 'screens stopped(&format!("iCIMS did not take \"{name}\": {}", errors.join(" ")));
                    }
                }
                Screen::Loading => {}
                Screen::Other { says } => {
                    return Err(FillError::UnexpectedForm(format!(
                        "the iCIMS page shows something Maverick does not know: {says:?}"
                    )));
                }
            }
        }
        stopped(&format!(
            "Maverick went through {MOST_SCREENS} screens without reaching the last step"
        ))
    }?;
    let pages = match filled.len() {
        0 => "It filled no step.".to_owned(),
        _ => format!("It filled: {}.", filled.join(", ")),
    };
    let review = match whole.end {
        End::Review => " It stopped on the last step. Read it, then submit the application yourself.",
        _ => "",
    };
    notes.insert(0, format!("{UNVERIFIED} {pages}{review}"));
    whole.note = Some(notes.join(" "));
    Ok(whole)
}

fn stopped(why: &str) -> Result<End, FillError> {
    Ok(End::Stopped(format!("{why}.")))
}

/// What a fill already did once, so that it never does it twice.
#[derive(Default)]
struct Tried {
    apply: bool,
    email: bool,
    password: bool,
    account: bool,
}

#[derive(Default)]
struct Job {
    title: String,
    description: String,
}

async fn sign_in(sign_ins: &impl SignIns) -> Result<SignIn, FillError> {
    sign_ins
        .sign_in(SIGN_IN_SERVICE)
        .await
        .map_err(|problem| {
            FillError::SignIn(format!(
                "Maverick could not read your Workday sign-in from the Keychain, which iCIMS uses too: \
                 {problem}. Sign in to iCIMS in that tab yourself, then click Fill form again."
            ))
        })?
        .ok_or(FillError::SignInNeeded {
            system: "iCIMS",
            service: SIGN_IN_SERVICE,
        })
}

/// What the tab shows.
#[derive(Clone, Debug, PartialEq)]
enum Screen {
    /// The posting's page, with the link to its application.
    Posting {
        apply: String,
        title: String,
        description: String,
    },
    /// iCIMS refused a link to the application.
    Stale,
    /// The site asks for an email to sign in. `consent` counts the privacy boxes beside it.
    Email {
        consent: usize,
    },
    Password,
    NewAccount,
    /// iCIMS offers to keep a passkey.
    Passkey,
    /// Another screen of iCIMS's sign-in, such as a code sent by email.
    Auth {
        says: String,
    },
    /// A step of the application.
    Step {
        at: u32,
        of: u32,
        name: String,
    },
    Loading,
    Other {
        says: String,
    },
}

const SCREEN: &str = r##"(() => {
    const text = (element) => (element?.innerText ?? element?.textContent ?? "").replace(/\s+/g, " ").trim();
    if (location.host === "login.icims.com") {
        const path = location.pathname;
        if (path.includes("enrollment")) return { screen: "passkey" };
        if (path.includes("/signup")) return { screen: "account" };
        if (document.querySelector('input[type="password"]')) return { screen: "password" };
        return { screen: "auth", says: text(document.querySelector("h1, h2, main") ?? document.body).slice(0, 200) };
    }
    const frame = document.getElementById("icims_content_iframe");
    const doc = frame?.contentDocument ?? document;
    if (!doc.body || doc.readyState === "loading") return { screen: "loading" };
    const body = text(doc.body);
    if (/unable to authenticate request/i.test(body)) return { screen: "stale" };
    const current = text(doc.querySelector(".iCIMS_Steps_Current .iCIMS_Steps_Content, .iCIMS_Steps_Current"));
    const step = current.match(/Step\s+(\d+)\s+of\s+(\d+)\.?\s*(.*?)\s*(\(Current Step\))?$/i);
    if (step) return { screen: "step", at: Number(step[1]), of: Number(step[2]), name: step[3] };
    if (doc.querySelector("#email") && doc.querySelector("#enterEmailSubmitButton")) {
        const consent = [...doc.querySelectorAll('input[type="checkbox"]')].filter((box) => box.getClientRects().length > 0).length;
        return { screen: "email", consent };
    }
    const apply = doc.querySelector("a.iCIMS_ApplyOnlineButton");
    if (apply) {
        const content = doc.querySelector(".iCIMS_JobContent, .iCIMS_InfoMsg_Job") ?? doc.body;
        return {
            screen: "posting",
            apply: apply.href,
            title: text(doc.querySelector("h1")),
            description: text(content).slice(0, 20000),
        };
    }
    if (!frame && document.readyState !== "complete") return { screen: "loading" };
    if (frame && frame.contentDocument?.readyState !== "complete") return { screen: "loading" };
    return { screen: "other", says: body.slice(0, 200) };
})()"##;

fn screen_in(seen: &Value) -> Screen {
    let text = |key: &str| seen[key].as_str().unwrap_or_default().to_owned();
    let number = |key: &str| {
        seen[key]
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or_default()
    };
    match seen["screen"].as_str() {
        Some("posting") => Screen::Posting {
            apply: text("apply"),
            title: text("title"),
            description: text("description"),
        },
        Some("stale") => Screen::Stale,
        Some("email") => Screen::Email {
            consent: seen["consent"].as_u64().unwrap_or_default() as usize,
        },
        Some("password") => Screen::Password,
        Some("account") => Screen::NewAccount,
        Some("passkey") => Screen::Passkey,
        Some("auth") => Screen::Auth { says: text("says") },
        Some("step") => Screen::Step {
            at: number("at"),
            of: number("of"),
            name: text("name"),
        },
        Some("other") => Screen::Other { says: text("says") },
        _ => Screen::Loading,
    }
}

/// Waits until the tab shows something other than a loading page and `passing`, the same
/// `STEADY_LOOKS` times in a row. A screen that stays as `passing` comes back after the wait,
/// so the caller decides what a screen that did not change means.
async fn settled(page: &Page, passing: Option<&Screen>) -> Result<Screen, FillError> {
    let deadline = Instant::now() + SCREEN_WAIT;
    let (mut last, mut steady) = (None::<Screen>, 0);
    loop {
        let now = match page.evaluate(SCREEN).await {
            Ok(seen) => screen_in(&seen),
            Err(error) if error.ends_the_work() => return Err(during("waiting for the iCIMS page")(error)),
            // The tab is between two documents.
            Err(_) => Screen::Loading,
        };
        steady = if last.as_ref() == Some(&now) { steady + 1 } else { 1 };
        last = Some(now.clone());
        let new = Some(&now) != passing;
        if now != Screen::Loading && steady >= STEADY_LOOKS && (new || Instant::now() >= deadline) {
            return Ok(now);
        }
        if Instant::now() >= deadline + SCREEN_WAIT {
            return Ok(now);
        }
        tokio::time::sleep(LOOK_GAP).await;
    }
}

/// Types the email on the site's own sign-in step, ticks its privacy boxes and goes on.
async fn enter_email(page: &Page, email: &str) -> Result<(), FillError> {
    let field = in_doc("the email field", "doc.querySelector('#email')");
    page.fill(&field, email).await.map_err(during("typing the email"))?;
    let ticked = format!(
        "(() => {{ const doc = {DOC}; [...doc.querySelectorAll('input[type=\"checkbox\"]')]
            .filter((box) => box.getClientRects().length > 0 && !box.checked).forEach((box) => box.click()); return true; }})()"
    );
    page.evaluate(&ticked)
        .await
        .map_err(during("accepting the privacy notice"))?;
    page.click(&in_doc(
        "the email step's button",
        "doc.querySelector('#enterEmailSubmitButton')",
    ))
    .await
    .map_err(during("sending the email"))
}

/// Types the password into every password field on iCIMS's sign-in screen and presses its button.
async fn type_password(page: &Page, sign_in: &SignIn) -> Result<(), FillError> {
    let password = sign_in.password.reveal();
    let fields = page
        .evaluate(r#"[...document.querySelectorAll('input[type="password"]')].length"#)
        .await
        .map_err(during("reading iCIMS's sign-in"))?
        .as_u64()
        .unwrap_or_default();
    for index in 0..fields {
        let field = Locator::script(
            "a password field",
            format!("document.querySelectorAll('input[type=\"password\"]')[{index}]"),
        );
        // An error from here never holds the password: only the field's name goes into it.
        page.fill(&field, password)
            .await
            .map_err(during("typing the password"))?;
    }
    let username = Locator::css(r#"input[name="username"], input[name="email"]"#);
    if page
        .evaluate(&format!("({})?.value === ''", username.as_script()))
        .await
        == Ok(Value::Bool(true))
    {
        page.fill(&username, &sign_in.account)
            .await
            .map_err(during("typing the email"))?;
    }
    page.click(&Locator::css(
        r#"button[type="submit"][name="action"][value="default"], button[type="submit"][name="action"]"#,
    ))
    .await
    .map_err(during("pressing iCIMS's sign-in button"))
}

/// The job's title from the page's heading.
async fn heading(page: &Page) -> Result<String, FillError> {
    let title = page
        .evaluate(&format!("{DOC}.querySelector('h1')?.innerText.trim() ?? ''"))
        .await
        .map_err(during("reading the job's title"))?;
    Ok(title.as_str().unwrap_or_default().to_owned())
}

/// The button that saves a step and goes on to the next one.
fn next_button() -> Locator {
    in_doc(
        "the step's button",
        r#"(() => {
            const shown = [...doc.querySelectorAll('input[type="submit"], button[type="submit"]')]
                .filter((button) => button.getClientRects().length > 0 && button.name !== "save"
                    && !/finish later/i.test(button.value || button.textContent));
            return shown.length === 1 ? shown[0] : null;
        })()"#,
    )
}

/// What iCIMS says is wrong with a step after its button was pressed. Empty once the
/// application went on to a later step.
async fn refused(page: &Page, at: u32) -> Result<Vec<String>, FillError> {
    let deadline = Instant::now() + SCREEN_WAIT;
    let look = format!(
        r#"(() => {{
            const doc = {DOC};
            const text = (element) => (element?.innerText ?? "").replace(/\s+/g, " ").trim();
            const current = text(doc.querySelector(".iCIMS_Steps_Current"));
            const step = Number(current.match(/Step\s+(\d+)\s+of/i)?.[1] ?? 0);
            const errors = [...doc.querySelectorAll(".iCIMS_ErrorMessage")]
                .filter((error) => !error.classList.contains("iCIMS_NoCookies") && error.getClientRects().length > 0)
                .map(text).filter((said) => said.length > 0);
            return {{ step, errors: [...new Set(errors)].slice(0, 8), ready: doc.readyState === "complete" }};
        }})()"#
    );
    let (mut last, mut steady) = (None::<Value>, 0);
    while Instant::now() < deadline {
        tokio::time::sleep(LOOK_GAP).await;
        let seen = match page.evaluate(&look).await {
            Ok(seen) => seen,
            Err(error) if error.ends_the_work() => return Err(during("waiting for the next step")(error)),
            Err(_) => continue,
        };
        if seen["step"].as_u64().is_some_and(|step| step > u64::from(at)) && seen["ready"] == true {
            return Ok(Vec::new());
        }
        steady = if last.as_ref() == Some(&seen) { steady + 1 } else { 1 };
        last = Some(seen.clone());
        let errors: Vec<String> = serde_json::from_value(seen["errors"].clone()).unwrap_or_default();
        if steady >= STEADY_LOOKS && seen["ready"] == true && !errors.is_empty() {
            return Ok(errors);
        }
    }
    Ok(vec![format!(
        "the page did not move on within {} seconds. Look at the tab before you click Fill form again.",
        SCREEN_WAIT.as_secs()
    )])
}

/// One control as the page script reads it.
#[derive(Debug, Deserialize, PartialEq)]
struct Seen {
    id: String,
    /// `text`, `email`, `tel`, `textarea`, `select`, `list`, `search`, `check`, `checks`, `radios`, `file`.
    control: String,
    title: String,
    required: bool,
    held: bool,
    #[serde(default)]
    options: Vec<String>,
}

/// Reads the step's fields. A list that loads its rows from the server is `list`, or `search`
/// when it is searched by typing. Check boxes and radio buttons that share a name are one field.
const READ: &str = r#"(() => {
    const doc = (document.getElementById("icims_content_iframe")?.contentDocument ?? document);
    const text = (element) => (element?.innerText ?? element?.textContent ?? "").replace(/\s+/g, " ").trim();
    const label = (element) => {
        const found = element.id && doc.querySelector(`label[for="${CSS.escape(element.id)}"]`);
        if (found) {
            const copy = found.cloneNode(true);
            copy.querySelectorAll(".Field_Required, .sr-only").forEach((part) => part.remove());
            const said = text(copy);
            if (said) return said;
        }
        const data = element.getAttribute("data-label");
        if (data && data !== element.id) return data;
        return element.getAttribute("aria-label") ?? "";
    };
    const shown = (element) => element.getClientRects().length > 0 && getComputedStyle(element).visibility !== "hidden";
    const placeholder = (said) => !said || /^(—\s*make a selection\s*—|please select.*|select one|no results)$/i.test(said);
    const headings = [...doc.querySelectorAll("h1, h2, h3, h4, legend, .iCIMS_SubHeader")];
    const section = (element) => {
        const before = headings.filter((heading) => heading.compareDocumentPosition(element) & Node.DOCUMENT_POSITION_FOLLOWING);
        return text(before.at(-1)).replace(/\*?\s*required\.?$/i, "").trim();
    };
    const shownValue = (element) => element.tagName === "SELECT" ? text(element.selectedOptions[0]) : (element.value ?? "").trim();
    const out = [];
    const grouped = new Set();
    for (const element of doc.querySelectorAll("input, select, textarea")) {
        const type = (element.type || "").toLowerCase();
        if (["hidden", "submit", "button", "image", "reset", "password"].includes(type)) continue;
        if (element.closest(".dropdown-container")) continue;
        if (/(^|\.)Login$/.test(element.name) || element.id === "keepMeAliveButton") continue;
        const required = element.getAttribute("i_required") === "true" || element.required
            || element.getAttribute("aria-required") === "true";
        if (type === "checkbox" || type === "radio") {
            const group = [...doc.querySelectorAll(`input[name="${CSS.escape(element.name)}"]`)];
            if (group.length > 1) {
                if (grouped.has(element.name)) continue;
                grouped.add(element.name);
                if (!group.some(shown)) continue;
                const row = element.closest(".iCIMS_TableRow");
                const question = text(doc.getElementById(`${element.name}_questionText`)) || text(row?.querySelector(".iCIMS_InfoField")) || element.name;
                out.push({
                    id: element.name,
                    control: type === "radio" ? "radios" : "checks",
                    title: question.replace(/\*$/, "").trim(),
                    required: group.some((one) => one.getAttribute("i_required") === "true" || one.required),
                    held: group.some((one) => one.checked),
                    options: group.map(label),
                });
                continue;
            }
            if (!shown(element)) continue;
            out.push({ id: element.id, control: "check", title: label(element), required, held: element.checked });
            continue;
        }
        if (type === "file") {
            const row = element.closest(".iCIMS_TableRow") ?? element.parentElement;
            if (!row || !shown(row)) continue;
            const kept = /replace/i.test(text(row));
            out.push({ id: element.id, control: "file", title: label(element) || text(row).slice(0, 120), required, held: kept });
            continue;
        }
        const widget = element.tagName === "SELECT" && element.getAttribute("icimsdropdown-enabled") === "1";
        const visible = widget ? shown(doc.getElementById(`${element.id}_icimsDropdown`) ?? element) : shown(element);
        if (!visible || !element.id) continue;
        let control = element.tagName === "TEXTAREA" ? "textarea" : element.tagName === "SELECT" ? "select" : type || "text";
        if (widget) control = element.getAttribute("icimsdropdown-search") === "1" ? "search" : "list";
        const value = shownValue(element);
        let title = label(element);
        const entry = element.id.match(/^(\d+)_/);
        const place = section(element);
        if (entry && /\(\d+\)/.test(place)) {
            const others = [...doc.querySelectorAll(`[id^="${entry[1]}_"]`)]
                .filter((other) => other !== element && ["INPUT", "SELECT"].includes(other.tagName) && other.type !== "hidden")
                .map(shownValue).filter((said) => !placeholder(said)).slice(0, 2);
            title = `${title} (${place}${others.length ? ": " + others.join(", ") : ""})`;
        }
        out.push({
            id: element.id,
            control,
            title,
            required: required || element.classList.contains("iCIMS_Forms_RequiredField"),
            held: !placeholder(value),
            options: control === "select" ? [...element.options].map(text).filter((said) => !placeholder(said)) : [],
        });
    }
    return out;
})()"#;

/// The fields of the step on screen, as questions.
fn fields(seen: &[Seen]) -> Vec<Field> {
    seen.iter()
        .map(|seen| {
            let kind = match seen.control.as_str() {
                "textarea" => Kind::LongText,
                "email" => Kind::Email,
                "tel" => Kind::Phone,
                _ if seen.id.ends_with("PhoneNumber") => Kind::Phone,
                "text" | "number" | "url" if is_date_part(&seen.id) => {
                    Kind::Unsupported("a date in three parts".into())
                }
                "text" | "number" | "url" => Kind::Text,
                "date" => Kind::Date,
                "select" if is_date_part(&seen.id) => Kind::Unsupported("a date in three parts".into()),
                "select" | "radios" => Kind::Choice {
                    options: seen.options.clone(),
                },
                // The rows load from the server when the list opens. `with_options` reads them.
                "list" => Kind::Choice { options: Vec::new() },
                "search" => Kind::Search,
                "check" => Kind::YesNo,
                "checks" => Kind::Choices {
                    options: seen.options.clone(),
                },
                "file" => Kind::File,
                other => Kind::Unsupported(other.to_owned()),
            };
            Field {
                path: seen.id.clone(),
                required: seen.required,
                held: seen.held,
                question: Question {
                    title: seen.title.clone(),
                    kind,
                    role: role_of(&seen.id),
                },
            }
        })
        .collect()
}

/// The month, the day and the year of one date, each a field of its own.
fn is_date_part(id: &str) -> bool {
    ["_Month", "_Date", "_Year"].iter().any(|part| id.ends_with(part))
}

fn role_of(id: &str) -> Option<Role> {
    let name = id.rsplit('.').next().unwrap_or(id);
    Some(match name {
        _ if name.contains("Resume") => Role::Resume,
        "AddressStreet1" => Role::Street,
        "AddressCity" => Role::City,
        "AddressZip" => Role::PostalCode,
        "AddressCountry" => Role::Country,
        "AddressState" => Role::Region,
        "PhoneType" => Role::PhoneKind,
        _ => return None,
    })
}

/// Fills the step on screen and stays on it.
///
/// A field that already holds an answer is left as it is: iCIMS keeps the user's profile from
/// one application to the next, and the user may have corrected it there.
async fn fill_step(page: &Page, answers: &Answers, job: &Job) -> Result<Report, FillError> {
    let seen = page.evaluate(READ).await.map_err(during("reading the step's fields"))?;
    let seen: Vec<Seen> = serde_json::from_value(seen)
        .map_err(|error| FillError::UnexpectedForm(format!("the step's fields could not be read: {error}")))?;
    let (held, open): (Vec<Field>, Vec<Field>) = fields(&seen).into_iter().partition(|field| field.held);
    let open = with_options(page, open).await?;
    // Any step can ask about visas, military service or the like, so the model reads the user's
    // disclosures on every step. Its instructions keep them to the questions that ask.
    let disclosing;
    let answers = match answers.disclosures.is_empty() {
        true => answers,
        false => {
            disclosing = Answers {
                facts: [answers.disclosures.as_slice(), answers.facts.as_slice()].concat(),
                ..answers.clone()
            };
            &disclosing
        }
    };
    let form = Form {
        job: job.title.clone(),
        description: job.description.clone(),
        fields: open,
    };
    let mut report = apply::fill_form(page, answers, form, &Icims).await?;
    report.outcomes.extend(held.into_iter().map(|field| Outcome {
        title: field.question.title,
        required: field.required,
        result: FieldResult::Left(Unanswered::Held),
    }));
    Ok(report)
}

/// Reads the rows of each list that loads them from the server.
async fn with_options(page: &Page, mut fields: Vec<Field>) -> Result<Vec<Field>, FillError> {
    for field in &mut fields {
        if let Kind::Choice { options } = &mut field.question.kind
            && options.is_empty()
        {
            *options = match rows(page, &field.path).await {
                Ok(rows) => rows,
                Err(Problem::Page(error)) if error.ends_the_work() => {
                    return Err(during(format!("opening the list {:?}", field.question.title))(error));
                }
                Err(_) => Vec::new(),
            };
            close_list(page, &field.path).await;
        }
    }
    Ok(fields)
}

/// Opens a list that loads its rows from the server and gives the rows' labels.
async fn rows(page: &Page, id: &str) -> Result<Vec<String>, Problem> {
    open_list(page, id).await?;
    let labels = format!(
        "[...({DOC}).getElementById({})?.querySelectorAll('li') ?? []].map((row) => row.title || row.textContent.trim())",
        quoted(&format!("{id}_dropdown-results"))
    );
    settles(
        page,
        &format!("{labels}.filter((label) => !/^(—|No Results|Loading)/i.test(label)).length > 0"),
    )
    .await?;
    let labels: Vec<String> = serde_json::from_value(page.evaluate(&labels).await?).unwrap_or_default();
    Ok(labels
        .into_iter()
        .filter(|label| !label.starts_with('—') && !label.eq_ignore_ascii_case("No Results"))
        .collect())
}

fn list_open(id: &str) -> String {
    format!(
        "!(({DOC}).getElementById({})?.classList.contains('dropdown-invisible') ?? true)",
        quoted(&format!("{id}_icimsDropdown_ctnr"))
    )
}

/// Opens a list with the mouse. The list listens for real clicks only.
async fn open_list(page: &Page, id: &str) -> Result<(), Problem> {
    let open = list_open(id);
    for _ in 0..2 {
        if page.evaluate(&open).await? == true {
            return Ok(());
        }
        page.click(&in_doc(
            "the list",
            &format!("doc.getElementById({})", quoted(&format!("{id}_icimsDropdown"))),
        ))
        .await?;
        if settles(page, &open).await? {
            return Ok(());
        }
    }
    Err(Problem::Mismatch("the list did not open"))
}

/// Closes an open list. A list that stays open covers the field below it.
async fn close_list(page: &Page, id: &str) {
    if page.evaluate(&list_open(id)).await == Ok(Value::Bool(true)) {
        let _ = page
            .click(&in_doc(
                "the list",
                &format!("doc.getElementById({})", quoted(&format!("{id}_icimsDropdown"))),
            ))
            .await;
    }
}

/// An element in the application's document. `script` reads it from `doc`.
fn in_doc(name: &str, script: &str) -> Locator {
    Locator::script(name, format!("((doc) => {script})({DOC})"))
}

fn element(id: &str) -> Locator {
    in_doc(
        &format!("the field {id}"),
        &format!("doc.getElementById({})", quoted(id)),
    )
}

struct Icims;

impl Enter for Icims {
    async fn enter(&self, page: &Page, field: &Field, answer: &Answer) -> Result<String, Problem> {
        enter(page, field, answer).await
    }
}

async fn enter(page: &Page, field: &Field, answer: &Answer) -> Result<String, Problem> {
    let id = &field.path;
    match (answer, &field.question.kind) {
        (Answer::Text(text), Kind::Search) => pick(page, id, text, true).await,
        (Answer::Text(text), Kind::Phone) => {
            let input = element(id);
            page.fill(&input, text).await?;
            let value = format!("({})?.value ?? ''", input.as_script());
            let digits: String = text.chars().filter(char::is_ascii_digit).collect();
            let same = format!(
                "({value}).replace(/\\D/g, '').endsWith({})",
                quoted(&digits[digits.len().saturating_sub(10)..])
            );
            let entered = settles(page, &same).await?;
            let shown = page.evaluate(&value).await?.as_str().unwrap_or_default().to_owned();
            if entered { Ok(shown) } else { Err(Problem::Shows(shown)) }
        }
        (Answer::Text(text), _) => {
            let input = element(id);
            page.fill(&input, text).await?;
            apply::expect_value(page, &input, text).await
        }
        (Answer::YesNo(yes), Kind::YesNo) => {
            let input = element(id);
            let set = format!("({})?.checked === {yes}", input.as_script());
            if page.evaluate(&set).await? != true {
                page.click(&input).await?;
            }
            match settles(page, &set).await? {
                true => Ok(if *yes { "ticked" } else { "not ticked" }.into()),
                false => Err(Problem::Mismatch("the box did not stay as set")),
            }
        }
        (Answer::YesNo(yes), _) => choose(page, field, if *yes { "Yes" } else { "No" }).await,
        (Answer::Choice(wanted), _) => choose(page, field, wanted).await,
        (Answer::Choices(wanted), _) => {
            for option in wanted {
                tick_option(page, id, option).await?;
            }
            Ok(wanted.join(", "))
        }
        (Answer::File(file), _) => {
            let name = file
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            page.upload(&element(id), &[file]).await?;
            let row = format!(
                "(({}))?.closest('.iCIMS_TableRow')?.innerText.includes({})",
                element(id).as_script(),
                quoted(&name)
            );
            let deadline = Instant::now() + UPLOAD_WAIT;
            while Instant::now() < deadline {
                if page.evaluate(&row).await? == true {
                    return Ok(name);
                }
                tokio::time::sleep(LOOK_GAP).await;
            }
            Err(Problem::Mismatch("iCIMS did not show the uploaded file"))
        }
        (Answer::Date(_), _) => Err(Problem::Mismatch("iCIMS asks for a date in three parts")),
    }
}

/// Picks an option in a list of any kind: a plain list, one that loads its rows, or radio buttons.
async fn choose(page: &Page, field: &Field, wanted: &str) -> Result<String, Problem> {
    let id = &field.path;
    let control = page
        .evaluate(&format!("(({}) ?? null)?.tagName ?? 'radios'", element(id).as_script()))
        .await?;
    if control == "SELECT" {
        let widget = page
            .evaluate(&format!(
                "({})?.getAttribute('icimsdropdown-enabled') === '1'",
                element(id).as_script()
            ))
            .await?;
        if widget == true {
            return pick(page, id, wanted, false).await;
        }
        let set = format!(
            r#"((select, wanted) => {{
                const option = [...select.options].find((option) => option.text.trim().toLowerCase() === wanted.toLowerCase());
                if (!option) return null;
                if (select.value !== option.value) {{
                    select.value = option.value;
                    select.dispatchEvent(new Event("change", {{ bubbles: true }}));
                }}
                return select.selectedOptions[0]?.text.trim() ?? null;
            }})({}, {})"#,
            element(id).as_script(),
            quoted(wanted)
        );
        return match page.evaluate(&set).await? {
            Value::String(shown) => Ok(shown),
            _ => Err(Problem::Mismatch("the list has no such option")),
        };
    }
    tick_option(page, id, wanted).await?;
    Ok(wanted.to_owned())
}

/// Ticks the radio button or check box named `name` whose label is `wanted`.
async fn tick_option(page: &Page, name: &str, wanted: &str) -> Result<(), Problem> {
    let input = in_doc(
        &format!("the option {wanted:?}"),
        &format!(
            r#"[...doc.querySelectorAll('input[name="' + CSS.escape({}) + '"]')].find((input) => {{
                const label = doc.querySelector('label[for="' + CSS.escape(input.id) + '"]');
                return (label?.innerText ?? "").replace(/\s+/g, " ").replace(/\*$/, "").trim().toLowerCase() === {};
            }}) ?? null"#,
            quoted(name),
            quoted(&wanted.to_lowercase())
        ),
    );
    let set = format!("({})?.checked === true", input.as_script());
    if page.evaluate(&set).await? != true {
        page.click(&input).await?;
    }
    match settles(page, &set).await? {
        true => Ok(()),
        false => Err(Problem::Mismatch("the option did not stay ticked")),
    }
}

/// Picks a row in a list that loads its rows from the server. With `search`, it first types
/// `wanted` into the list's search box. A row that reads `wanted` wins over one that starts with it.
async fn pick(page: &Page, id: &str, wanted: &str, search: bool) -> Result<String, Problem> {
    let shown = format!("({})?.selectedOptions[0]?.text.trim() ?? ''", element(id).as_script());
    let same = |said: &Value| said.as_str().is_some_and(|said| said.eq_ignore_ascii_case(wanted));
    if same(&page.evaluate(&shown).await?) {
        return Ok(wanted.to_owned());
    }
    open_list(page, id).await?;
    let results = format!("({DOC}).getElementById({})", quoted(&format!("{id}_dropdown-results")));
    if search {
        // The list searches when a key comes up in its box.
        let typed = format!(
            r#"((box) => {{
                if (!box) return false;
                box.value = {};
                box.dispatchEvent(new Event("input", {{ bubbles: true }}));
                box.dispatchEvent(new KeyboardEvent("keyup", {{ bubbles: true, key: "a" }}));
                return true;
            }})(({DOC}).getElementById({})?.querySelector("input.dropdown-search"))"#,
            quoted(wanted),
            quoted(&format!("{id}_icimsDropdown_ctnr"))
        );
        if page.evaluate(&typed).await? != true {
            return Err(Problem::Mismatch("the list has no search box"));
        }
    }
    let row = format!(
        r#"((list, wanted) => {{
            const rows = [...(list?.querySelectorAll("li.result-selectable") ?? [])]
                .map((row) => [row, (row.title || row.textContent).trim().toLowerCase()]);
            const found = rows.find(([, label]) => label === wanted) ?? rows.find(([, label]) => label.startsWith(wanted));
            return found?.[0] ?? null;
        }})({results}, {})"#,
        quoted(&wanted.to_lowercase())
    );
    let deadline = Instant::now() + LIST_WAIT;
    while page.evaluate(&format!("!!{row}")).await? != true {
        if Instant::now() >= deadline {
            close_list(page, id).await;
            return Err(Problem::Mismatch("the list has no such row"));
        }
        tokio::time::sleep(LOOK_GAP).await;
    }
    // The list takes a row on the mouse's press and release, which a script can send.
    let chosen = format!(
        r#"((row) => {{ ["mousedown", "mouseup", "click"].forEach((kind) => row.dispatchEvent(new MouseEvent(kind, {{ bubbles: true }}))); return true; }})({row})"#
    );
    page.evaluate(&chosen).await?;
    let picked = format!("({shown}) !== ''");
    settles(page, &picked).await?;
    let now = page.evaluate(&shown).await?.as_str().unwrap_or_default().to_owned();
    close_list(page, id).await;
    if now.is_empty() {
        Err(Problem::Shows(now))
    } else {
        Ok(now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_posting_is_known_by_its_site_and_number() {
        let at = "https://careers-example.icims.com/jobs/21486/software-engineer-1/job?mode=apply&apply=yes";
        assert!(is_posting(at));
        assert_eq!(
            posting_address(at).as_deref(),
            Some("https://careers-example.icims.com/jobs/21486/job")
        );
        assert_eq!(
            form_address("https://careers-example.icims.com/jobs/21486/job?in_iframe=1").as_deref(),
            Some("https://careers-example.icims.com/jobs/21486/job")
        );
        assert!(!is_posting("https://login.icims.com/jobs/1/job"));
        assert!(!is_posting("https://careers-example.icims.com/jobs/search?ss=1"));
        assert!(!is_posting("https://jobs.example.com/jobs/56742?icims=1"));
        assert!(!is_posting("https://example.icims.com.attacker.io/jobs/1/job"));
    }

    fn seen(id: &str, control: &str, title: &str) -> Seen {
        Seen {
            id: id.into(),
            control: control.into(),
            title: title.into(),
            required: true,
            held: false,
            options: vec!["Yes".into(), "No".into()],
        }
    }

    #[test]
    fn each_control_becomes_a_question_of_its_kind() {
        let read = fields(&[
            seen("442593_PersonProfileFields.AddressState", "search", "State/Province"),
            seen(
                "459550_PersonProfileFields.PhoneNumber",
                "text",
                "Number (XXX-XXX-XXXX)",
            ),
            seen("Q162", "select", "Are you over the age of 18?"),
            seen("rcf3553", "list", "Text messages"),
            seen("9502331_PersonProfileFields.rcf3214_Month", "select", "Month"),
            seen("Q261", "check", "I attest that all information is true"),
            seen("PortalProfileFields.Resume_File", "file", "Please upload your resume"),
        ]);
        let kinds: Vec<(Kind, Option<Role>)> = read
            .into_iter()
            .map(|field| (field.question.kind, field.question.role))
            .collect();
        assert_eq!(
            kinds,
            vec![
                (Kind::Search, Some(Role::Region)),
                (Kind::Phone, None),
                (
                    Kind::Choice {
                        options: vec!["Yes".into(), "No".into()]
                    },
                    None
                ),
                (Kind::Choice { options: Vec::new() }, None),
                (Kind::Unsupported("a date in three parts".into()), None),
                (Kind::YesNo, None),
                (Kind::File, Some(Role::Resume)),
            ]
        );
    }

    #[test]
    fn the_screen_is_read_from_what_the_page_script_saw() {
        let step = serde_json::json!({ "screen": "step", "at": 2, "of": 3, "name": "Job Specific Questions" });
        assert_eq!(
            screen_in(&step),
            Screen::Step {
                at: 2,
                of: 3,
                name: "Job Specific Questions".into()
            }
        );
        assert_eq!(screen_in(&serde_json::json!({ "screen": "passkey" })), Screen::Passkey);
        assert_eq!(screen_in(&serde_json::json!({})), Screen::Loading);
    }
}
