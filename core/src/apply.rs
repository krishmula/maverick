//! What every application system shares: the form as questions, the plan for answering them,
//! and the report. Each system, such as Ashby, adds how it reads its form and how an answer
//! goes into its page.

use std::fmt;
use std::time::{Duration, Instant};

use crate::answers::{Answer, Answers, Kind, Question, Role, Unanswered};
use crate::ashby;
use crate::greenhouse;
use crate::model;
use crate::page::{Locator, Page, PageError};
use crate::workday;

/// How long a page gets to show an answer after it was entered.
const SETTLE_WAIT: Duration = Duration::from_secs(3);
const SETTLE_GAP: Duration = Duration::from_millis(50);

/// An application system Maverick can fill.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum System {
    Ashby,
    Greenhouse,
    Workday,
}

impl System {
    /// The system that hosts the posting at `address`, if Maverick can fill it.
    pub fn of(address: &str) -> Option<Self> {
        ashby::is_posting(address)
            .then_some(Self::Ashby)
            .or_else(|| greenhouse::is_posting(address).then_some(Self::Greenhouse))
            .or_else(|| workday::is_posting(address).then_some(Self::Workday))
    }

    /// The posting's address without anything a tracker or a campaign added to it.
    pub fn posting_address(self, address: &str) -> Option<String> {
        match self {
            Self::Ashby => ashby::posting_address(address),
            Self::Greenhouse => greenhouse::posting_address(address),
            Self::Workday => workday::posting_address(address),
        }
    }

    /// The address of the posting's application form.
    pub fn form_address(self, address: &str) -> Option<String> {
        match self {
            Self::Ashby => ashby::form_address(address),
            Self::Greenhouse => greenhouse::form_address(address),
            Self::Workday => workday::form_address(address),
        }
    }

    /// A CSS selector for the element the system shows once an application went through.
    pub fn submitted(self) -> &'static str {
        match self {
            Self::Ashby => ashby::SUBMITTED,
            Self::Greenhouse => greenhouse::SUBMITTED,
            Self::Workday => workday::SUBMITTED,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Ashby => "Ashby",
            Self::Greenhouse => "Greenhouse",
            Self::Workday => "Workday",
        }
    }

    /// A script that gives the button that submits the form on screen, or null when the page
    /// does not show exactly one.
    fn submit_button(self) -> Locator {
        let script = match self {
            Self::Workday => {
                r#"(() => {
                    const button = document.querySelector('[data-automation-id="pageFooterNextButton"]');
                    return button?.textContent.trim().toLowerCase() === "submit" && !button.disabled ? button : null;
                })()"#
            }
            Self::Ashby | Self::Greenhouse => {
                r#"(() => {
                    const found = [...document.querySelectorAll("button")].filter((button) =>
                        /^submit( application)?$/i.test(button.textContent.trim())
                        && button.getClientRects().length > 0 && !button.disabled);
                    return found.length === 1 ? found[0] : null;
                })()"#
            }
        };
        Locator::script("the Submit button", script)
    }

    /// True when the page shows exactly one button that submits the form.
    pub async fn shows_submit(self, page: &Page) -> Result<bool, PageError> {
        let shown = page
            .evaluate(&format!("!!({})", self.submit_button().as_script()))
            .await?;
        Ok(shown == true)
    }

    /// Presses the form's submit button once and waits for the system to confirm.
    ///
    /// Call this only for a report that `Report::held_for_the_user` lets through. It never
    /// presses twice: a press that is not confirmed is left for the user to look at.
    pub async fn submit(self, page: &Page) -> Result<Sent, FillError> {
        let button = self.submit_button();
        if !self
            .shows_submit(page)
            .await
            .map_err(during("looking for the Submit button"))?
        {
            let missing = "The form is complete, and Maverick did not find one Submit button on the page. \
                           Submit it in the tab yourself.";
            return Ok(Sent::Unconfirmed(missing.to_owned()));
        }
        page.click(&button).await.map_err(during("pressing Submit"))?;
        let look = format!(
            r#"(() => {{
                if (document.querySelector({confirmation}) || location.pathname.includes("/jobTasks/completed/")) return "confirmed";
                const text = document.body?.innerText ?? "";
                if (/(security|verification) code/i.test(text)) return "code";
                const challenges = 'iframe[src*="/recaptcha/"][src*="bframe"], iframe[src*="hcaptcha.com"], iframe[src*="challenges.cloudflare.com"]';
                const asked = [...document.querySelectorAll(challenges)].some((frame) => {{
                    const box = frame.getBoundingClientRect();
                    return box.width > 100 && box.height > 100 && getComputedStyle(frame).visibility !== "hidden";
                }});
                return asked ? "challenge" : "waiting";
            }})()"#,
            confirmation = quoted(self.submitted()),
        );
        let name = self.name();
        let deadline = Instant::now() + CONFIRM_WAIT;
        let mut last = String::from("waiting");
        while Instant::now() < deadline {
            tokio::time::sleep(CONFIRM_GAP).await;
            match page.evaluate(&look).await {
                Ok(state) => last = state.as_str().unwrap_or("waiting").to_owned(),
                Err(error) if error.ends_the_work() => return Err(during("waiting for the confirmation")(error)),
                // The page is between two documents right after a submission.
                Err(_) => continue,
            }
            match last.as_str() {
                "confirmed" => return Ok(Sent::Confirmed),
                "code" => {
                    return Ok(Sent::Unconfirmed(format!(
                        "Maverick pressed Submit, and {name} asks for a code it sent to your email. Enter it in the tab."
                    )));
                }
                _ => {}
            }
        }
        let headline = page
            .evaluate(r#"(document.querySelector("h1, h2")?.textContent ?? document.title).replace(/\s+/g, " ").trim().slice(0, 120)"#)
            .await
            .ok()
            .and_then(|text| text.as_str().map(str::to_owned))
            .unwrap_or_default();
        Ok(Sent::Unconfirmed(match last.as_str() {
            "challenge" => {
                format!("Maverick pressed Submit, and {name} shows a check that you are human. Solve it in the tab.")
            }
            _ => format!(
                "Maverick pressed Submit, and {name} did not confirm within {} seconds. The page says {headline:?}. \
                 Look at the tab before you do anything else: the application may have gone through.",
                CONFIRM_WAIT.as_secs()
            ),
        }))
    }

    /// Fills the form in a tab that is on a posting of this system or on its form.
    /// It never submits.
    pub async fn fill(self, page: &Page, answers: &Answers) -> Result<Report, FillError> {
        match self {
            Self::Ashby => ashby::fill(page, answers).await,
            Self::Greenhouse => greenhouse::fill(page, answers).await,
            Self::Workday => workday::fill(page, answers).await,
        }
    }
}

/// The posting's own address, for any system Maverick can fill.
pub fn posting_address(address: &str) -> Option<String> {
    System::of(address)?.posting_address(address)
}

/// How one application system puts an answer into its page.
pub trait Enter {
    /// Enters one answer and returns what the field shows afterwards.
    fn enter(&self, page: &Page, field: &Field, answer: &Answer) -> impl Future<Output = Result<String, Problem>>;
}

#[derive(Debug, PartialEq)]
pub enum FillError {
    /// The tab is not on a posting this code can fill.
    NotAPosting(String),
    /// The posting is no longer listed.
    Closed,
    UnexpectedForm(String),
    /// The application is behind an account, and Maverick has no sign-in stored for the system.
    /// `service` is the name of the Keychain item it looks for.
    SignInNeeded {
        system: &'static str,
        service: &'static str,
    },
    /// Maverick tried to get past a system's sign-in and stopped. The text says what happened
    /// and what the user does next.
    SignIn(String),
    Page(PageError),
    /// The link to the tab broke. `during` says what the filler was doing.
    Interrupted {
        during: String,
        error: PageError,
    },
}

impl fmt::Display for FillError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAPosting(address) => write!(f, "{address} is not a posting Maverick can fill"),
            Self::Closed => f.write_str("this posting is closed"),
            Self::UnexpectedForm(problem) => {
                write!(f, "the form is described in a way Maverick cannot read: {problem}")
            }
            Self::SignInNeeded { system, service } => write!(
                f,
                "{system} shows the application only to a signed-in account, and Maverick has no {system} \
                 sign-in stored. To store one, run `security add-generic-password -s {service} -a <email> -w` \
                 in Terminal and type the password at the prompt. Then click Fill form again. \
                 Or sign in to {system} in that tab yourself."
            ),
            Self::SignIn(what_happened) => f.write_str(what_happened),
            Self::Page(error) => write!(f, "{error}"),
            Self::Interrupted { during, error } => write!(f, "{error} This happened while {during}."),
        }
    }
}

impl std::error::Error for FillError {}

pub fn during(step: impl Into<String>) -> impl FnOnce(PageError) -> FillError {
    let during = step.into();
    move |error| FillError::Interrupted { during, error }
}

impl From<PageError> for FillError {
    fn from(error: PageError) -> Self {
        Self::Page(error)
    }
}

/// One field of the form.
#[derive(Clone, Debug, PartialEq)]
pub struct Field {
    /// The application system's own name for the field, which its code uses to find the field in the page.
    pub path: String,
    pub required: bool,
    /// True when the form already holds an answer in this field.
    pub held: bool,
    pub question: Question,
}

/// What happened to one field.
#[derive(Clone, Debug, PartialEq)]
pub struct Outcome {
    pub title: String,
    pub required: bool,
    pub result: FieldResult,
}

/// What a fill did, field by field.
#[derive(Clone, Debug, PartialEq)]
pub struct Report {
    pub outcomes: Vec<Outcome>,
    /// Why the model could not be asked, when it could not. The standard answers still went in.
    pub model_problem: Option<String>,
    /// What else the user should know about this fill, when an application system has something to say.
    pub note: Option<String>,
    pub end: End,
}

impl Outcome {
    /// True for a required field that still needs the user's answer.
    pub fn is_open(&self) -> bool {
        let answered = matches!(
            self.result,
            FieldResult::Filled { .. } | FieldResult::Left(Unanswered::Held | Unanswered::NotNeeded)
        );
        self.required && !answered
    }
}

impl Report {
    /// Why the submit is left to the user, or `None` when Maverick may submit the form itself.
    ///
    /// Maverick submits a form only when every answer in it is the user's own or follows from
    /// the user's facts. A draft, an answer that did not go in and an open required field each
    /// leave the form for the user.
    pub fn held_for_the_user(&self) -> Option<String> {
        let count = |holds: fn(&Outcome) -> bool| self.outcomes.iter().filter(|outcome| holds(outcome)).count();
        let fields = |count: usize, one: &str, many: &str| match count {
            1 => format!("1 {one}"),
            count => format!("{count} {many}"),
        };
        if let End::Stopped(why) = &self.end {
            return Some(why.clone());
        }
        if let Some(problem) = &self.model_problem {
            return Some(format!("The model could not be asked: {problem}"));
        }
        let failed = count(|outcome| matches!(outcome.result, FieldResult::Failed(_)));
        if failed > 0 {
            return Some(format!("{} could not be entered.", fields(failed, "answer", "answers")));
        }
        // A form of several pages checked its own required fields on the way to its last page.
        let open = match self.end {
            End::Review => 0,
            _ => count(Outcome::is_open),
        };
        if open > 0 {
            let needs = if open == 1 { "needs" } else { "need" };
            return Some(format!(
                "{} {needs} your answer.",
                fields(open, "required field", "required fields")
            ));
        }
        let drafts = count(|outcome| {
            matches!(
                outcome.result,
                FieldResult::Filled {
                    by: Source::Draft { .. },
                    ..
                }
            )
        });
        if drafts > 0 {
            let is = if drafts == 1 { "is a draft" } else { "are drafts" };
            return Some(format!("{} {is} for you to read.", fields(drafts, "answer", "answers")));
        }
        None
    }
}

/// What happened after Maverick pressed a form's submit button.
#[derive(Clone, Debug, PartialEq)]
pub enum Sent {
    /// The application system showed that the application went through.
    Confirmed,
    /// The button was pressed and the system did not confirm. The text says what the page showed
    /// and what the user does next.
    Unconfirmed(String),
}

/// How long an application system gets to confirm a submission.
const CONFIRM_WAIT: Duration = Duration::from_secs(60);
const CONFIRM_GAP: Duration = Duration::from_millis(500);

/// Where a fill ended.
#[derive(Clone, Debug, PartialEq)]
pub enum End {
    /// The form is one page, and the fill went through it.
    Page,
    /// The form has several pages, and the fill stopped before the last one. The text says why.
    Stopped(String),
    /// The form has several pages, and the fill reached the last one, which only asks to submit.
    Review,
}

/// Where an answer came from.
#[derive(Clone, Debug, PartialEq)]
pub enum Source {
    StandardAnswer,
    /// A model worked it out from the user's facts. `basis` is the fact it says it used.
    Model {
        basis: String,
    },
    /// A model drafted it from the user's resume. The user should read it before submitting.
    Draft {
        basis: String,
    },
}

/// A posting's application form.
#[derive(Clone, Debug, PartialEq)]
pub struct Form {
    /// The job's title and place, for the model's context.
    pub job: String,
    /// The job description as plain text.
    pub description: String,
    pub fields: Vec<Field>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum FieldResult {
    Filled {
        /// What the field shows now.
        shown: String,
        by: Source,
    },
    /// Left as it was, for the user to answer.
    Left(Unanswered),
    /// The answer was known and could not be entered.
    Failed(String),
}

/// Fills every field of a form that is on screen and reports on each field, in the form's order.
///
/// The standard answers go in first. A model then answers what they left, from the user's facts,
/// and drafts the essay questions from the user's resume.
pub async fn fill_form(page: &Page, answers: &Answers, form: Form, system: &impl Enter) -> Result<Report, FillError> {
    let Form {
        job,
        description,
        fields,
    } = form;
    let planned: Vec<Result<Answer, Unanswered>> = fields.iter().map(|field| answers.answer(&field.question)).collect();
    let open = |essays: bool| open_for_a_model(&fields, &planned, essays);
    let (factual, essays) = (open(false), open(true));
    let mut results: Vec<Option<FieldResult>> = vec![None; fields.len()];

    let requirements = description.clone();
    // Drafting takes the model longest, so it starts first and runs beside everything else.
    let drafting = (!answers.background.is_empty() && !essays.is_empty()).then(|| {
        let (background, facts, job) = (answers.background.clone(), answers.facts.clone(), job.clone());
        let questions: Vec<Question> = essays.iter().map(|&index| fields[index].question.clone()).collect();
        tokio::spawn(async move {
            let questions: Vec<&Question> = questions.iter().collect();
            model::draft(&background, &facts, &job, &description, &questions).await
        })
    });
    // The model works out the factual answers while the standard answers go in.
    let asking = async {
        if answers.facts.is_empty() || factual.is_empty() {
            return None;
        }
        let questions: Vec<&Question> = factual.iter().map(|&index| &fields[index].question).collect();
        Some(
            model::answer(
                &facts_for_a_model(answers),
                answers.standing,
                &job,
                &requirements,
                &questions,
            )
            .await,
        )
    };
    let entering = async {
        // The resume goes first, because Ashby can fill other fields from it.
        let resume_first = (0..fields.len()).filter(|&index| fields[index].question.role == Some(Role::Resume));
        let rest = (0..fields.len()).filter(|&index| fields[index].question.role != Some(Role::Resume));
        for index in resume_first.chain(rest) {
            if let Ok(answer) = &planned[index] {
                results[index] = Some(entered(system, page, &fields[index], answer, Source::StandardAnswer).await?);
            }
        }
        Ok::<_, FillError>(())
    };
    let (said, entered_standard) = tokio::join!(asking, entering);
    entered_standard?;

    let mut model_problem = None;
    let drafted = match drafting {
        Some(drafting) => Some(drafting.await.unwrap_or_else(|stopped| Err(stopped.to_string()))),
        None => None,
    };
    let from_model: [FromModel<'_>; 2] = [
        (&factual, said, |basis| Source::Model { basis }),
        (&essays, drafted, |basis| Source::Draft { basis }),
    ];
    for (asked, said, by) in from_model {
        match said {
            None => {}
            Some(Err(problem)) => model_problem = Some(problem),
            Some(Ok(said)) => {
                for (&index, said) in asked.iter().zip(said) {
                    let Some(stated) = said.answer else { continue };
                    results[index] = Some(match fields[index].question.kind.answer_from(&stated) {
                        Ok(answer) => entered(system, page, &fields[index], &answer, by(said.basis)).await?,
                        Err(why) => FieldResult::Left(why),
                    });
                }
            }
        }
    }

    let outcomes = fields
        .into_iter()
        .zip(results)
        .zip(planned)
        .map(|((field, result), planned)| Outcome {
            title: field.question.title,
            required: field.required,
            result: match (result, planned) {
                (Some(result), _) => result,
                (None, Err(_)) if field.held => FieldResult::Left(Unanswered::Held),
                (None, Err(why)) => FieldResult::Left(why),
                (None, Ok(_)) => unreachable!("every planned answer was entered above"),
            },
        })
        .collect();
    Ok(Report {
        outcomes,
        model_problem,
        note: None,
        end: End::Page,
    })
}

/// The user's facts, with the address a form can ask for in words of its own, such as "ZIP code".
fn facts_for_a_model(answers: &Answers) -> Vec<String> {
    let address = answers.address.as_ref().map(|at| {
        format!(
            "Mailing address: {}, {}, {} {}, {}.",
            at.street, at.city, at.region, at.postal_code, at.country
        )
    });
    answers.facts.iter().cloned().chain(address).collect()
}

/// The places of the questions the standard answers left for a model: the essays, or the rest.
/// Essays go to their own model call.
fn open_for_a_model(fields: &[Field], planned: &[Result<Answer, Unanswered>], essays: bool) -> Vec<usize> {
    (0..fields.len())
        .filter(|&index| {
            let kind = &fields[index].question.kind;
            let supported = !matches!(kind, Kind::File | Kind::Unsupported(_));
            // An answer the form holds came from the user, so no model replaces it.
            let for_a_model = fields[index].question.role.is_none_or(Role::takes_a_model_answer);
            let open = planned[index].is_err() && !fields[index].held && for_a_model;
            open && supported && matches!(kind, Kind::LongText) == essays
        })
        .collect()
}

/// What a fill of `form` would do, worked out without a page. Nothing is entered anywhere.
///
/// The model answers the factual questions as it would in a fill. An essay is reported as a
/// draft and is not written.
pub async fn rehearse(answers: &Answers, form: Form) -> Report {
    let Form {
        job,
        description,
        fields,
    } = form;
    let planned: Vec<Result<Answer, Unanswered>> = fields.iter().map(|field| answers.answer(&field.question)).collect();
    let factual = open_for_a_model(&fields, &planned, false);
    let essays = match answers.background.is_empty() {
        true => Vec::new(),
        false => open_for_a_model(&fields, &planned, true),
    };
    let would = |shown: String, by| FieldResult::Filled { shown, by };
    let mut results: Vec<Option<FieldResult>> = vec![None; fields.len()];
    for &index in &essays {
        let basis = "not written in a rehearsal".to_owned();
        results[index] = Some(would(String::new(), Source::Draft { basis }));
    }
    let mut model_problem = None;
    if !answers.facts.is_empty() && !factual.is_empty() {
        let questions: Vec<&Question> = factual.iter().map(|&index| &fields[index].question).collect();
        match model::answer(
            &facts_for_a_model(answers),
            answers.standing,
            &job,
            &description,
            &questions,
        )
        .await
        {
            Err(problem) => model_problem = Some(problem),
            Ok(said) => {
                for (&index, said) in factual.iter().zip(said) {
                    let Some(stated) = said.answer else { continue };
                    results[index] = Some(match fields[index].question.kind.answer_from(&stated) {
                        Ok(_) => would(stated, Source::Model { basis: said.basis }),
                        Err(why) => FieldResult::Left(why),
                    });
                }
            }
        }
    }
    let outcomes = fields
        .into_iter()
        .zip(results)
        .zip(planned)
        .map(|((field, result), planned)| Outcome {
            title: field.question.title,
            required: field.required,
            result: match (result, planned) {
                (Some(result), _) => result,
                (None, Ok(_)) => would(String::new(), Source::StandardAnswer),
                (None, Err(why)) => FieldResult::Left(why),
            },
        })
        .collect();
    Report {
        outcomes,
        model_problem,
        note: None,
        end: End::Page,
    }
}

/// The fields a model call was asked about, what it said, and how to label its answers.
type FromModel<'a> = (
    &'a Vec<usize>,
    Option<Result<Vec<model::ModelAnswer>, String>>,
    fn(String) -> Source,
);

/// Enters one answer. An error means the link to the tab is gone, so no later field can work either.
async fn entered(
    system: &impl Enter,
    page: &Page,
    field: &Field,
    answer: &Answer,
    by: Source,
) -> Result<FieldResult, FillError> {
    match system.enter(page, field, answer).await {
        Ok(shown) => Ok(FieldResult::Filled { shown, by }),
        Err(Problem::Page(error)) if error.ends_the_work() => {
            Err(during(format!("filling {:?}", field.question.title))(error))
        }
        Err(problem) => Ok(FieldResult::Failed(problem.to_string())),
    }
}

/// Why a known answer could not be entered.
pub enum Problem {
    Page(PageError),
    /// The field does not show what was entered.
    Shows(String),
    Mismatch(&'static str),
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Page(error) => write!(f, "{error}"),
            Self::Shows(shown) if shown.is_empty() => f.write_str("the field stayed empty"),
            Self::Shows(shown) => write!(f, "the field shows {shown:?} instead"),
            Self::Mismatch(problem) => f.write_str(problem),
        }
    }
}

impl From<PageError> for Problem {
    fn from(error: PageError) -> Self {
        Self::Page(error)
    }
}

/// Waits for an input to show `expected`, and says what it shows instead when it does not.
pub async fn expect_value(page: &Page, input: &Locator, expected: &str) -> Result<String, Problem> {
    let value = format!("({})?.value ?? ''", input.as_script());
    if settles(page, &format!("({value}) === {}", quoted(expected))).await? {
        return Ok(expected.to_owned());
    }
    let shown = page.evaluate(&value).await?;
    Err(Problem::Shows(shown.as_str().unwrap_or_default().to_owned()))
}

/// Waits a short time for a JavaScript condition to hold. The page redraws after input, not during it.
pub async fn settles(page: &Page, condition: &str) -> Result<bool, PageError> {
    let deadline = Instant::now() + SETTLE_WAIT;
    loop {
        if page.evaluate(condition).await? == true {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        tokio::time::sleep(SETTLE_GAP).await;
    }
}

/// The text of an element, trimmed. Empty when the element is missing.
pub async fn text_of(page: &Page, element: &Locator) -> Result<String, PageError> {
    let text = page
        .evaluate(&format!("({})?.textContent.trim() ?? ''", element.as_script()))
        .await?;
    Ok(text.as_str().unwrap_or_default().to_owned())
}

/// HTML as plain text: tags go, block ends become line breaks, and common entities are read.
pub fn plain_text(html: &str) -> String {
    let mut text = String::new();
    let mut rest = html;
    while let Some(open) = rest.find('<') {
        text.push_str(&rest[..open]);
        let Some(close) = rest[open..].find('>') else {
            rest = "";
            break;
        };
        let tag = rest[open + 1..open + close].to_lowercase();
        if ["/p", "/li", "/h1", "/h2", "/h3", "/h4", "/div", "br", "br/", "br /"].contains(&tag.as_str()) {
            text.push('\n');
        }
        rest = &rest[open + close + 1..];
    }
    text.push_str(rest);
    let text = [
        ("&nbsp;", " "),
        ("&lt;", "<"),
        ("&gt;", ">"),
        ("&quot;", "\""),
        ("&#39;", "'"),
        ("&amp;", "&"),
    ]
    .iter()
    .fold(text, |text, (entity, plain)| text.replace(entity, plain));
    let lines: Vec<&str> = text.lines().map(str::trim).filter(|line| !line.is_empty()).collect();
    lines.join("\n")
}

/// A string as a JavaScript literal.
pub fn quoted(text: &str) -> String {
    serde_json::to_string(text).expect("a string is plain JSON")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(required: bool, result: FieldResult) -> Outcome {
        Outcome {
            title: "Field".into(),
            required,
            result,
        }
    }

    fn filled(by: Source) -> FieldResult {
        FieldResult::Filled {
            shown: "shown".into(),
            by,
        }
    }

    fn report(outcomes: Vec<Outcome>, end: End) -> Report {
        Report {
            outcomes,
            model_problem: None,
            note: None,
            end,
        }
    }

    #[test]
    fn maverick_submits_only_a_form_with_nothing_left_for_the_user() {
        let own = || outcome(true, filled(Source::StandardAnswer));
        let from_facts = || outcome(true, filled(Source::Model { basis: "a fact".into() }));
        let draft = |required| {
            outcome(
                required,
                filled(Source::Draft {
                    basis: "the resume".into(),
                }),
            )
        };
        let left = |required| outcome(required, FieldResult::Left(Unanswered::NoRule));
        let held = |outcomes| report(outcomes, End::Page).held_for_the_user();

        assert_eq!(held(vec![own(), from_facts(), left(false)]), None);
        assert_eq!(
            held(vec![own(), outcome(true, FieldResult::Left(Unanswered::Held))]),
            None
        );
        assert_eq!(
            held(vec![own(), left(true)]).as_deref(),
            Some("1 required field needs your answer.")
        );
        // A draft holds the form back even where the question is optional: the user has not read it.
        assert_eq!(
            held(vec![own(), draft(false)]).as_deref(),
            Some("1 answer is a draft for you to read.")
        );
        assert_eq!(
            held(vec![
                own(),
                outcome(false, FieldResult::Failed("the field stayed empty".into()))
            ])
            .as_deref(),
            Some("1 answer could not be entered.")
        );
        let mut unasked = report(vec![own()], End::Page);
        unasked.model_problem = Some("no model is set up".into());
        assert!(unasked.held_for_the_user().is_some());
    }

    #[test]
    fn a_form_of_several_pages_is_submitted_only_from_its_last_page() {
        let own = || outcome(true, filled(Source::StandardAnswer));
        let left = || outcome(true, FieldResult::Left(Unanswered::NoRule));
        // The system let every page through, so it holds an answer Maverick did not see.
        assert_eq!(report(vec![own(), left()], End::Review).held_for_the_user(), None);
        assert_eq!(
            report(vec![own()], End::Stopped("Workday did not take My Experience.".into()))
                .held_for_the_user()
                .as_deref(),
            Some("Workday did not take My Experience.")
        );
        let draft = outcome(
            false,
            filled(Source::Draft {
                basis: "the resume".into(),
            }),
        );
        assert!(report(vec![own(), draft], End::Review).held_for_the_user().is_some());
    }
}
