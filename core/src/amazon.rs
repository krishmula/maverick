//! Fills an application on amazon.jobs step by step, and stops at its review. It never submits.
//!
//! Amazon keeps one profile per account: contact details, education, the resume, the
//! acknowledgements and the voluntary forms carry over from one application to the next. What
//! changes is each job's own questions. The application lists its steps down the side and marks
//! each one complete or not, so Maverick goes to the first incomplete step, answers it, presses
//! its button, and stops once every step is complete.
//!
//! Signing in takes the email, the password from the Keychain and a code Amazon emails each
//! time. Maverick reads the code from the user's Gmail when that account is connected.
//!
//! Amazon takes at most ten active applications per account. A fill stops on Amazon's notice of
//! that limit.
//!
//! Verified on one live application that was already started: the sign-in with the emailed code,
//! every step up to the review, and the stop at the limit. Starting a new application is not
//! verified, because the account was at its limit.

use std::time::{Duration, Instant};

use jiff::Timestamp;
use serde::Deserialize;
use serde_json::Value;

use crate::answers::{Answer, Answers, Kind, Question, Role, Unanswered};
use crate::apply::{
    self, End, Enter, Field, FieldResult, FillError, Form, Outcome, Problem, Report, during, quoted, settles,
};
use crate::credentials::{Keychain, SignIn, SignIns};
use crate::mail;
use crate::page::{Locator, Page};

/// The Keychain item of the user's amazon.jobs sign-in.
pub const SIGN_IN_SERVICE: &str = "dev.maverick.amazon";

/// What Amazon shows once an application went through. Not seen on a live application yet.
pub const SUBMITTED: &str = ".application-submitted, [data-test-id='application-submitted']";

/// The Gmail search for Amazon's sign-in code.
const CODE_MAIL: &str = "amazon (code OR verification OR verify)";

const SCREEN_WAIT: Duration = Duration::from_secs(30);
const LOOK_GAP: Duration = Duration::from_millis(400);
const STEADY_LOOKS: u32 = 3;
/// The most screens one fill goes through: the posting, three of sign-in and about twelve steps.
const MOST_SCREENS: usize = 24;
const LIST_WAIT: Duration = Duration::from_secs(6);

const UNVERIFIED: &str = "Check every answer: Maverick's Amazon filling is verified on one live application only.";

/// True for an amazon.jobs address that names a posting or its application.
pub fn is_posting(address: &str) -> bool {
    job_id(address).is_some()
}

/// The posting's page, `https://www.amazon.jobs/en/jobs/<id>`.
pub fn posting_address(address: &str) -> Option<String> {
    job_id(address).map(|id| format!("https://www.amazon.jobs/en/jobs/{id}"))
}

/// The form opens from the posting's page, which gives Maverick the job's description.
pub fn form_address(address: &str) -> Option<String> {
    posting_address(address)
}

/// The job's number in `www.amazon.jobs/<language>/jobs/<id>` or `…/applicant/jobs/<id>/apply`.
fn job_id(address: &str) -> Option<&str> {
    let rest = address.strip_prefix("https://")?;
    let (host, path) = rest.split_once('/')?;
    if !matches!(host, "www.amazon.jobs" | "amazon.jobs" | "account.amazon.jobs") {
        return None;
    }
    let parts: Vec<&str> = path.split(['?', '#']).next()?.split('/').collect();
    let at = parts.iter().position(|part| *part == "jobs")?;
    let id = *parts.get(at + 1)?;
    (!id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit())).then_some(id)
}

/// Fills the application in a tab on an Amazon posting, or anywhere inside its application.
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
    let mut asked_since = Timestamp::now();
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
                Screen::Email => {
                    if std::mem::replace(&mut tried.email, true) {
                        break 'screens stopped("Amazon asked for the email again");
                    }
                    let sign_in = sign_in(sign_ins).await?;
                    page.fill(&Locator::css("#preLoginEmailField"), &sign_in.account)
                        .await
                        .map_err(during("typing the email"))?;
                    page.click(&Locator::css("#preLoginContinueButton"))
                        .await
                        .map_err(during("sending the email"))?;
                }
                Screen::Password => {
                    if std::mem::replace(&mut tried.password, true) {
                        return Err(FillError::SignIn(
                            "Amazon refused the password at sign-in. Sign in in that tab yourself, then click \
                             Fill form again."
                                .into(),
                        ));
                    }
                    let sign_in = sign_in(sign_ins).await?;
                    // An error from here never holds the password: only the field's name goes into it.
                    page.fill(&Locator::css(r#"input[name="password"]"#), sign_in.password.reveal())
                        .await
                        .map_err(during("typing the password"))?;
                    asked_since = Timestamp::now() - jiff::SignedDuration::from_secs(5);
                    page.click(&button("Continue"))
                        .await
                        .map_err(during("pressing Amazon's sign-in button"))?;
                }
                Screen::Code => {
                    if std::mem::replace(&mut tried.code, true) {
                        return Err(FillError::SignIn(
                            "Amazon did not take the code it emailed. Sign in in that tab yourself, then click \
                             Fill form again."
                                .into(),
                        ));
                    }
                    let sign_in = sign_in(sign_ins).await?;
                    let code = mail::sign_in_code(&sign_in.account, CODE_MAIL, asked_since)
                        .await
                        .map_err(|problem| {
                            FillError::SignIn(format!(
                                "Amazon emailed a sign-in code to {}, and Maverick could not read it: {problem}. \
                                 Enter it in that tab, then click Fill form again.",
                                sign_in.account
                            ))
                        })?
                        .ok_or_else(|| {
                            FillError::SignIn(format!(
                                "Amazon emailed a sign-in code to {}. Maverick reads such codes only from a \
                                 connected Gmail account, and none came there in time. Enter it in that tab, then \
                                 click Fill form again.",
                                sign_in.account
                            ))
                        })?;
                    for (at, digit) in code.chars().enumerate() {
                        let box_ = Locator::css(&format!(r#"input[aria-label="Enter verification code {}"]"#, at + 1));
                        page.fill(&box_, &digit.to_string())
                            .await
                            .map_err(during("typing the emailed code"))?;
                    }
                    page.click(&button("Verify")).await.map_err(during("pressing Verify"))?;
                }
                Screen::Auth { says } => {
                    return Err(FillError::SignIn(format!(
                        "Maverick stopped at Amazon's sign-in, which says {says:?}. Finish it in that tab, then \
                         click Fill form again."
                    )));
                }
                Screen::Limit => {
                    break 'screens stopped(
                        "Amazon takes at most 10 active applications per account, and yours is at that limit. Withdraw one \
                         on your Amazon application dashboard, then click Fill form again",
                    );
                }
                Screen::Consent => {
                    if std::mem::replace(&mut tried.consent, true) {
                        break 'screens stopped("Amazon asked again about using your information");
                    }
                    page.click(&Locator::css(r#"label[for="consent-no"]"#))
                        .await
                        .map_err(during("declining Amazon's personalized recommendations"))?;
                    page.click(&button("Continue"))
                        .await
                        .map_err(during("closing Amazon's question about your information"))?;
                    notes.push(
                        "Amazon asked to use your information for job recommendations, and Maverick answered No."
                            .into(),
                    );
                }
                Screen::Leaving => {
                    page.click(&button("Continue applying"))
                        .await
                        .map_err(during("staying in the application"))?;
                }
                Screen::Step {
                    key,
                    name,
                    complete,
                    next,
                    review,
                } => {
                    if job.title.is_empty() {
                        job.title = heading(page).await?;
                    }
                    if complete {
                        if let Some(next) = next {
                            page.click(&step_link(&next))
                                .await
                                .map_err(during("opening the next incomplete step"))?;
                            continue;
                        }
                        if review {
                            page.click(&Locator::css("#my-progress-menu li:not([data-action]) > a"))
                                .await
                                .map_err(during("opening the review"))?;
                            continue;
                        }
                    }
                    let presses = tried.steps.entry(key.clone()).or_default();
                    *presses += 1;
                    if *presses > 2 {
                        break 'screens stopped(&format!("Maverick came back to \"{name}\" without getting further"));
                    }
                    let report = fill_step(page, answers, &job, &key).await?;
                    whole.model_problem = whole.model_problem.or(report.model_problem);
                    let count =
                        |holds: fn(&Outcome) -> bool| report.outcomes.iter().filter(|outcome| holds(outcome)).count();
                    let failed = count(|outcome| matches!(outcome.result, FieldResult::Failed(_)));
                    let open = count(Outcome::is_open);
                    whole.outcomes.extend(report.outcomes);
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
                    if !complete {
                        filled.push(name.clone());
                    }
                    page.click(&next_button(&key))
                        .await
                        .map_err(during(format!("pressing the button at the foot of \"{name}\"")))?;
                    let errors = refused(page, &key).await?;
                    if !errors.is_empty() {
                        break 'screens stopped(&format!("Amazon did not take \"{name}\": {}", errors.join(" ")));
                    }
                }
                Screen::Review => break 'screens Ok(End::Review),
                Screen::Loading => {}
                Screen::Other { says } => {
                    return Err(FillError::UnexpectedForm(format!(
                        "the Amazon page shows something Maverick does not know: {says:?}"
                    )));
                }
            }
        }
        stopped(&format!(
            "Maverick went through {MOST_SCREENS} screens without reaching the review"
        ))
    }?;
    let steps = match filled.len() {
        0 => "It answered no step: every step was already complete.".to_owned(),
        _ => format!("It answered: {}.", filled.join(", ")),
    };
    let review = match whole.end {
        End::Review => " It stopped on the review. Read it, then submit the application yourself.",
        _ => "",
    };
    notes.insert(0, format!("{UNVERIFIED} {steps}{review}"));
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
    code: bool,
    consent: bool,
    /// How often Maverick pressed each step's button, by the step's key.
    steps: std::collections::HashMap<String, u32>,
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
                "Maverick could not read your Amazon sign-in from the Keychain: {problem}. Sign in to \
                 amazon.jobs in that tab yourself, then click Fill form again."
            ))
        })?
        .ok_or(FillError::SignInNeeded {
            system: "Amazon",
            service: SIGN_IN_SERVICE,
        })
}

/// What the tab shows.
#[derive(Clone, Debug, PartialEq)]
enum Screen {
    Posting {
        apply: String,
        title: String,
        description: String,
    },
    Email,
    Password,
    /// Amazon emailed a code to type in.
    Code,
    /// Another screen of Amazon's sign-in.
    Auth {
        says: String,
    },
    /// Amazon's notice that the account has its most active applications.
    Limit,
    /// Amazon asks whether it may use the user's information for recommendations.
    Consent,
    /// Amazon asks whether to leave the application.
    Leaving,
    /// A step of the application. `next` is the first incomplete step that can be opened, other
    /// than this one. `review` is true once the review can be opened.
    Step {
        key: String,
        name: String,
        complete: bool,
        next: Option<String>,
        review: bool,
    },
    Review,
    Loading,
    Other {
        says: String,
    },
}

const SCREEN: &str = r##"(() => {
    const text = (element) => (element?.innerText ?? element?.textContent ?? "").replace(/\s+/g, " ").trim();
    const shown = (element) => !!element && element.getClientRects().length > 0 && getComputedStyle(element).visibility !== "hidden";
    if (!document.body || document.readyState === "loading") return { screen: "loading" };
    if (location.search.includes("application_limit_reach")) return { screen: "limit" };
    if (shown(document.querySelector("#preLoginEmailField"))) return { screen: "email" };
    if (shown(document.querySelector('input[aria-label="Enter verification code 1"]'))) return { screen: "code" };
    if (shown(document.querySelector('input[name="password"]'))) return { screen: "password" };
    if (location.host.includes("idprism") || location.host === "passport.amazon.jobs") {
        return { screen: "auth", says: text(document.querySelector("h1, h2") ?? document.body).slice(0, 200) };
    }
    const apply = [...document.querySelectorAll("a")].find((link) => /\/applicant\/jobs\/\d+\/apply/.test(link.href) && /apply/i.test(text(link)));
    if (location.host === "www.amazon.jobs" && apply) {
        return {
            screen: "posting",
            apply: apply.href,
            title: text(document.querySelector("h1")),
            description: text(document.querySelector("#job-detail-body, .job-detail, main") ?? document.body).slice(0, 20000),
        };
    }
    if (shown(document.querySelector("#consent-no"))) return { screen: "consent" };
    if ([...document.querySelectorAll("button")].some((button) => shown(button) && text(button) === "Continue applying")) return { screen: "leaving" };
    const menu = document.querySelector("#my-progress-menu");
    // The application draws its review first, then its steps and their menu.
    if (!menu && /\/applicant\/jobs\/\d+\/apply/.test(location.pathname)) return { screen: "loading" };
    if (menu) {
        const steps = [...menu.querySelectorAll("li")].map((item) => {
            const link = item.firstElementChild;
            const said = link?.getAttribute("aria-label") ?? "";
            return {
                key: item.dataset.action ?? null,
                name: text(link).replace(/\s*Optional$/, ""),
                open: link?.tagName === "A",
                complete: /review completed/.test(said),
            };
        });
        const panel = [...document.querySelectorAll("main [role=tabpanel]")].find(shown);
        if (panel?.id === "REVIEW_AND_SUBMIT") return { screen: "review" };
        const step = steps.find((step) => step.key && step.key === panel?.id);
        if (step) {
            const next = steps.find((other) => other.key && other.key !== step.key && other.open && !other.complete);
            const review = steps.some((other) => !other.key && other.open);
            return { screen: "step", key: step.key, name: step.name, complete: step.complete, next: next?.key ?? null, review };
        }
        if (panel) return { screen: "review" };
        if (document.readyState !== "complete") return { screen: "loading" };
    }
    if (document.readyState !== "complete") return { screen: "loading" };
    return { screen: "other", says: text(document.querySelector("main") ?? document.body).slice(0, 200) };
})()"##;

fn screen_in(seen: &Value) -> Screen {
    let text = |key: &str| seen[key].as_str().unwrap_or_default().to_owned();
    match seen["screen"].as_str() {
        Some("posting") => Screen::Posting {
            apply: text("apply"),
            title: text("title"),
            description: text("description"),
        },
        Some("email") => Screen::Email,
        Some("password") => Screen::Password,
        Some("code") => Screen::Code,
        Some("auth") => Screen::Auth { says: text("says") },
        Some("limit") => Screen::Limit,
        Some("consent") => Screen::Consent,
        Some("leaving") => Screen::Leaving,
        Some("step") => Screen::Step {
            key: text("key"),
            name: text("name"),
            complete: seen["complete"] == true,
            next: seen["next"].as_str().map(str::to_owned),
            review: seen["review"] == true,
        },
        Some("review") => Screen::Review,
        Some("other") => Screen::Other { says: text("says") },
        _ => Screen::Loading,
    }
}

/// Waits until the tab shows something other than a loading page and `passing`, the same
/// `STEADY_LOOKS` times in a row. A screen that stays as `passing` comes back after the wait.
async fn settled(page: &Page, passing: Option<&Screen>) -> Result<Screen, FillError> {
    let deadline = Instant::now() + SCREEN_WAIT;
    let (mut last, mut steady) = (None::<Screen>, 0);
    loop {
        let now = match page.evaluate(SCREEN).await {
            Ok(seen) => screen_in(&seen),
            Err(error) if error.ends_the_work() => return Err(during("waiting for the Amazon page")(error)),
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

/// The one shown button or link whose text is `label`.
fn button(label: &str) -> Locator {
    Locator::script(
        format!("the {label} button"),
        format!(
            r#"(() => {{
                const found = [...document.querySelectorAll("button, a")].filter((button) =>
                    button.getClientRects().length > 0 && button.innerText.replace(/\s+/g, " ").trim() === {});
                return found.length === 1 ? found[0] : null;
            }})()"#,
            quoted(label)
        ),
    )
}

fn step_link(key: &str) -> Locator {
    Locator::script(
        "the step's link",
        format!(
            "document.querySelector('#my-progress-menu li[data-action=\"' + CSS.escape({}) + '\"] > a')",
            quoted(key)
        ),
    )
}

/// The step's own panel.
fn panel(key: &str) -> String {
    format!("document.getElementById({})", quoted(key))
}

/// The job's title from the application's heading, "Apply | <title> (ID: …)".
async fn heading(page: &Page) -> Result<String, FillError> {
    let title = page
        .evaluate(
            r#"(document.querySelector("main a[href*='/jobs/']")?.innerText ?? "").replace(/\s*\(ID:.*$/, "").trim()"#,
        )
        .await
        .map_err(during("reading the job's title"))?;
    Ok(title.as_str().unwrap_or_default().to_owned())
}

/// The button that saves a step and goes on. A step that offers to skip, such as text updates,
/// is skipped.
fn next_button(key: &str) -> Locator {
    Locator::script(
        "the step's button",
        format!(
            r#"((panel) => {{
                const shown = [...(panel?.querySelectorAll("button, a") ?? [])].filter((button) => button.getClientRects().length > 0);
                const said = (button) => button.innerText.replace(/\s+/g, " ").trim();
                for (const label of ["Skip & continue", "Save & continue", "Continue"]) {{
                    const found = shown.filter((button) => said(button) === label);
                    if (found.length === 1) return found[0];
                }}
                return null;
            }})({})"#,
            panel(key)
        ),
    )
}

/// What Amazon says is wrong with a step after its button was pressed. Empty once the
/// application left the step.
async fn refused(page: &Page, key: &str) -> Result<Vec<String>, FillError> {
    let deadline = Instant::now() + SCREEN_WAIT;
    let look = format!(
        r#"((panel) => {{
            const text = (element) => (element?.innerText ?? "").replace(/\s+/g, " ").trim();
            // The review shows every step inside it, so a step counts as left once another panel comes first.
            const first = [...document.querySelectorAll("main [role=tabpanel]")].find((one) => one.getClientRects().length > 0);
            if (!panel || first !== panel) return {{ left: true, errors: [] }};
            const errors = [...panel.querySelectorAll(".invalid-feedback, .error-message, [role=alert], .alert-danger")]
                .filter((error) => error.getClientRects().length > 0).map(text).filter((said) => said.length > 0);
            const invalid = [...panel.querySelectorAll('[aria-invalid="true"]')].length;
            return {{ left: false, errors: [...new Set(errors)].slice(0, 8), invalid }};
        }})({})"#,
        panel(key)
    );
    let (mut last, mut steady) = (None::<Value>, 0);
    while Instant::now() < deadline {
        tokio::time::sleep(LOOK_GAP).await;
        let seen = match page.evaluate(&look).await {
            Ok(seen) => seen,
            Err(error) if error.ends_the_work() => return Err(during("waiting for the next step")(error)),
            Err(_) => continue,
        };
        if seen["left"] == true {
            return Ok(Vec::new());
        }
        steady = if last.as_ref() == Some(&seen) { steady + 1 } else { 1 };
        last = Some(seen.clone());
        let mut errors: Vec<String> = serde_json::from_value(seen["errors"].clone()).unwrap_or_default();
        if errors.is_empty() && seen["invalid"].as_u64().is_some_and(|invalid| invalid > 0) {
            errors.push("it marks some answers as invalid.".into());
        }
        if steady >= STEADY_LOOKS * 2 && !errors.is_empty() {
            return Ok(errors);
        }
    }
    Ok(vec![format!(
        "the page did not move on within {} seconds. Look at the tab before you click Fill form again",
        SCREEN_WAIT.as_secs()
    )])
}

/// One control as the page script reads it.
#[derive(Debug, Deserialize, PartialEq)]
struct Seen {
    /// How `enter` finds the control again: a combobox's label id, a radio group's name, or an id.
    path: String,
    /// `list`, `radios`, `check`, `text`, `email`, `tel`, `textarea`, `date`, `file`.
    control: String,
    title: String,
    required: bool,
    held: bool,
    #[serde(default)]
    options: Vec<String>,
}

/// Reads the fields of one step. A list is a hidden `select` that a search list stands in for;
/// radio buttons in one group are one field.
fn read(key: &str) -> String {
    format!(
        r#"((panel) => {{
    const text = (element) => (element?.innerText ?? element?.textContent ?? "").replace(/\s+/g, " ").trim();
    const shown = (element) => !!element && element.getClientRects().length > 0 && getComputedStyle(element).visibility !== "hidden";
    const named = (element) => text(document.getElementById(element.getAttribute("aria-labelledby") ?? "")) ;
    const label = (element) => text(element.labels?.[0]) || element.getAttribute("aria-label") || "";
    const out = [];
    if (!panel) return out;
    for (const select of panel.querySelectorAll("select")) {{
        const combo = select.nextElementSibling?.querySelector('[role="combobox"]');
        if (!shown(combo)) continue;
        out.push({{
            path: combo.getAttribute("aria-labelledby") ?? "",
            control: "list",
            title: named(combo),
            required: combo.getAttribute("aria-required") === "true",
            held: select.value !== "",
            options: [...select.options].map(text).filter((said) => said.length > 0),
        }});
    }}
    for (const group of panel.querySelectorAll('[role="radiogroup"]')) {{
        const radios = [...group.querySelectorAll('input[type="radio"]')];
        if (!radios.length || !radios.some((radio) => shown(radio.closest(".custom-control") ?? radio))) continue;
        out.push({{
            path: radios[0].name,
            control: "radios",
            title: named(group),
            required: radios.some((radio) => radio.getAttribute("aria-required") === "true" || radio.required),
            held: radios.some((radio) => radio.checked),
            options: radios.map(label),
        }});
    }}
    for (const input of panel.querySelectorAll("input, textarea")) {{
        const type = (input.type || "").toLowerCase();
        if (["hidden", "radio", "submit", "button", "password"].includes(type)) continue;
        if (input.closest(".select2-container")) continue;
        const required = input.required || input.getAttribute("aria-required") === "true";
        if (type === "file") {{
            out.push({{ path: input.id, control: "file", title: "Resume", required: true, held: /download /i.test(text(panel)) }});
            continue;
        }}
        if (!shown(input) && !(type === "checkbox" && shown(input.closest(".custom-control")))) continue;
        if (type === "checkbox") {{
            out.push({{ path: input.id, control: "check", title: label(input), required, held: input.checked }});
            continue;
        }}
        const control = input.tagName === "TEXTAREA" ? "textarea" : input.getAttribute("role") === "combobox" ? "date" : type || "text";
        out.push({{ path: input.id, control, title: label(input), required, held: (input.value ?? "").trim() !== "" }});
    }}
    return out;
}})({})"#,
        panel(key)
    )
}

/// The fields of the step on screen, as questions.
fn fields(seen: &[Seen]) -> Vec<Field> {
    seen.iter()
        .map(|seen| {
            let kind = match seen.control.as_str() {
                _ if seen.path.is_empty() => Kind::Unsupported("a field Maverick cannot find again".into()),
                "textarea" => Kind::LongText,
                "email" => Kind::Email,
                "tel" => Kind::Phone,
                "text" | "number" | "url" => Kind::Text,
                "list" | "radios" => Kind::Choice {
                    options: seen.options.clone(),
                },
                "check" => Kind::YesNo,
                "file" => Kind::File,
                other => Kind::Unsupported(other.to_owned()),
            };
            Field {
                path: seen.path.clone(),
                required: seen.required,
                held: seen.held,
                question: Question {
                    title: seen.title.clone(),
                    kind,
                    role: role_of(&seen.path, &seen.control),
                },
            }
        })
        .collect()
}

fn role_of(path: &str, control: &str) -> Option<Role> {
    Some(match path {
        _ if control == "file" => Role::Resume,
        "applicant_address_street_line1" => Role::Street,
        "applicant_address_city" => Role::City,
        "applicant_address_zip" => Role::PostalCode,
        _ => return None,
    })
}

/// Fills the step on screen and stays on it.
///
/// A field that already holds an answer is left as it is: Amazon keeps the user's profile from
/// one application to the next, and the user may have corrected it there.
async fn fill_step(page: &Page, answers: &Answers, job: &Job, key: &str) -> Result<Report, FillError> {
    let seen = page
        .evaluate(&read(key))
        .await
        .map_err(during("reading the step's fields"))?;
    let seen: Vec<Seen> = serde_json::from_value(seen)
        .map_err(|error| FillError::UnexpectedForm(format!("the step's fields could not be read: {error}")))?;
    let (held, open): (Vec<Field>, Vec<Field>) = fields(&seen).into_iter().partition(|field| field.held);
    // Any step can ask about visas or the like, so the model reads the user's disclosures on
    // every step. Its instructions keep them to the questions that ask.
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
    let mut report = apply::fill_form(page, answers, form, &Amazon).await?;
    report.outcomes.extend(held.into_iter().map(|field| Outcome {
        title: field.question.title,
        required: field.required,
        result: FieldResult::Left(Unanswered::Held),
    }));
    Ok(report)
}

struct Amazon;

impl Enter for Amazon {
    async fn enter(&self, page: &Page, field: &Field, answer: &Answer) -> Result<String, Problem> {
        enter(page, field, answer).await
    }
}

fn by_id(id: &str) -> Locator {
    Locator::script(
        format!("the field {id}"),
        format!("document.getElementById({})", quoted(id)),
    )
}

async fn enter(page: &Page, field: &Field, answer: &Answer) -> Result<String, Problem> {
    let path = &field.path;
    match (answer, &field.question.kind) {
        (Answer::Text(text), _) => {
            let input = by_id(path);
            page.fill(&input, text).await?;
            apply::expect_value(page, &input, text).await
        }
        (Answer::YesNo(yes), Kind::YesNo) => {
            let input = by_id(path);
            let set = format!("({})?.checked === {yes}", input.as_script());
            if page.evaluate(&set).await? != true {
                let label = Locator::script(
                    "the box's label",
                    format!(
                        "document.querySelector('label[for=\"' + CSS.escape({}) + '\"]')",
                        quoted(path)
                    ),
                );
                page.click(&label).await?;
            }
            match settles(page, &set).await? {
                true => Ok(if *yes { "ticked" } else { "not ticked" }.into()),
                false => Err(Problem::Mismatch("the box did not stay as set")),
            }
        }
        (Answer::YesNo(yes), _) => choose(page, path, if *yes { "Yes" } else { "No" }).await,
        (Answer::Choice(wanted), _) => choose(page, path, wanted).await,
        (Answer::Choices(_), _) => Err(Problem::Mismatch("Amazon's steps have no lists of several answers")),
        (Answer::File(_), _) => Err(Problem::Mismatch("Maverick keeps the resume on your Amazon profile")),
        (Answer::Date(_), _) => Err(Problem::Mismatch("Maverick does not enter dates on Amazon")),
    }
}

/// Picks `wanted` in a list or a group of radio buttons.
async fn choose(page: &Page, path: &str, wanted: &str) -> Result<String, Problem> {
    let combo = Locator::script(
        "the list",
        format!(
            "document.querySelector('[role=\"combobox\"][aria-labelledby=\"' + CSS.escape({}) + '\"]')",
            quoted(path)
        ),
    );
    if page.evaluate(&format!("!!({})", combo.as_script())).await? == true {
        return pick(page, &combo, wanted).await;
    }
    let label = Locator::script(
        format!("the option {wanted:?}"),
        format!(
            r#"(() => {{
                const radio = [...document.querySelectorAll('input[type="radio"][name="' + CSS.escape({}) + '"]')]
                    .find((radio) => (radio.labels?.[0]?.innerText ?? "").replace(/\s+/g, " ").trim().toLowerCase() === {});
                return radio?.labels?.[0] ?? null;
            }})()"#,
            quoted(path),
            quoted(&wanted.to_lowercase())
        ),
    );
    let set = format!(
        "(document.getElementById(({})?.htmlFor ?? '')?.checked === true)",
        label.as_script()
    );
    if page.evaluate(&set).await? != true {
        page.click(&label).await?;
    }
    match settles(page, &set).await? {
        true => Ok(wanted.to_owned()),
        false => Err(Problem::Mismatch("the option did not stay ticked")),
    }
}

/// Picks a row in a search list. The list opens and takes a row on real mouse clicks only.
async fn pick(page: &Page, combo: &Locator, wanted: &str) -> Result<String, Problem> {
    let shown = format!(
        "(({})?.closest('.select2-container')?.previousElementSibling?.selectedOptions?.[0]?.text ?? '').trim()",
        combo.as_script()
    );
    let same = |said: &Value| said.as_str().is_some_and(|said| said.eq_ignore_ascii_case(wanted));
    if same(&page.evaluate(&shown).await?) {
        return Ok(wanted.to_owned());
    }
    // A list that is still closing takes the next click for itself.
    let closed = "!document.querySelector('.select2-container--open')";
    if page.evaluate(closed).await? != true {
        page.press(crate::page::Key::Escape).await?;
        settles(page, closed).await?;
    }
    let open = format!(
        "({})?.getAttribute('aria-expanded') === 'true' && !!document.querySelector('.select2-container--open .select2-results__option')",
        combo.as_script()
    );
    for _ in 0..3 {
        page.click(combo).await?;
        if settles(page, &open).await? {
            break;
        }
    }
    let row = Locator::script(
        format!("the row {wanted:?}"),
        format!(
            r#"((wanted) => {{
                const rows = [...document.querySelectorAll(".select2-container--open .select2-results__option")]
                    .map((row) => [row, row.innerText.replace(/\s+/g, " ").trim().toLowerCase()]);
                const found = rows.find(([, label]) => label === wanted) ?? rows.find(([, label]) => label.startsWith(wanted));
                return found?.[0] ?? null;
            }})({})"#,
            quoted(&wanted.to_lowercase())
        ),
    );
    let deadline = Instant::now() + LIST_WAIT;
    while page.evaluate(&format!("!!({})", row.as_script())).await? != true {
        if Instant::now() >= deadline {
            let _ = page.press(crate::page::Key::Escape).await;
            return Err(Problem::Mismatch("the list has no such row"));
        }
        tokio::time::sleep(LOOK_GAP).await;
    }
    page.click(&row).await?;
    settles(page, &format!("({shown}) !== ''")).await?;
    settles(page, closed).await?;
    let now = page.evaluate(&shown).await?.as_str().unwrap_or_default().to_owned();
    if now.eq_ignore_ascii_case(wanted) || (!now.is_empty() && now.to_lowercase().starts_with(&wanted.to_lowercase())) {
        Ok(now)
    } else {
        Err(Problem::Shows(now))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_posting_is_known_by_its_number() {
        for at in [
            "https://www.amazon.jobs/en/jobs/1234567/software-engineer",
            "https://www.amazon.jobs/jobs/1234567",
            "https://account.amazon.jobs/en-US/applicant/jobs/1234567/apply",
        ] {
            assert_eq!(
                posting_address(at).as_deref(),
                Some("https://www.amazon.jobs/en/jobs/1234567"),
                "{at}"
            );
        }
        assert!(!is_posting("https://www.amazon.jobs/en/search?base_query=sde"));
        assert!(!is_posting("https://www.amazon.jobs.example.io/en/jobs/1"));
        assert!(!is_posting("https://www.amazon.com/jobs/1"));
    }

    fn seen(path: &str, control: &str) -> Seen {
        Seen {
            path: path.into(),
            control: control.into(),
            title: "Question".into(),
            required: true,
            held: false,
            options: vec!["Yes".into(), "No".into()],
        }
    }

    #[test]
    fn each_control_becomes_a_question_of_its_kind() {
        let kinds: Vec<(Kind, Option<Role>)> = fields(&[
            seen("18f4d83a-AQ-label", "list"),
            seen("ARE_YOU_WILLING_TO_RELOCATE", "radios"),
            seen("check-915ffe8a", "check"),
            seen("applicant_address_city", "text"),
            seen("document-replace-input", "file"),
            seen("", "text"),
        ])
        .into_iter()
        .map(|field| (field.question.kind, field.question.role))
        .collect();
        let yes_no = Kind::Choice {
            options: vec!["Yes".into(), "No".into()],
        };
        assert_eq!(
            kinds,
            vec![
                (yes_no.clone(), None),
                (yes_no, None),
                (Kind::YesNo, None),
                (Kind::Text, Some(Role::City)),
                (Kind::File, Some(Role::Resume)),
                (Kind::Unsupported("a field Maverick cannot find again".into()), None),
            ]
        );
    }

    #[test]
    fn the_screen_is_read_from_what_the_page_script_saw() {
        let step = serde_json::json!({
            "screen": "step", "key": "RESUME", "name": "Resume", "complete": true, "next": null, "review": true
        });
        assert_eq!(
            screen_in(&step),
            Screen::Step {
                key: "RESUME".into(),
                name: "Resume".into(),
                complete: true,
                next: None,
                review: true
            }
        );
        assert_eq!(screen_in(&serde_json::json!({ "screen": "limit" })), Screen::Limit);
        assert_eq!(screen_in(&serde_json::json!({})), Screen::Loading);
    }
}
