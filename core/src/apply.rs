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
    /// The application is behind an account, and only the user signs in to it.
    SignInNeeded {
        system: &'static str,
    },
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
            Self::SignInNeeded { system } => write!(
                f,
                "{system} shows the application only to a signed-in account. \
                 Sign in to {system} in that tab, then click Fill form again."
            ),
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
    // Questions the standard answers left. Essays go to their own model call.
    let open = |essays: bool| -> Vec<usize> {
        (0..fields.len())
            .filter(|&index| {
                let kind = &fields[index].question.kind;
                let supported = !matches!(kind, Kind::File | Kind::Unsupported(_));
                planned[index].is_err() && supported && matches!(kind, Kind::LongText) == essays
            })
            .collect()
    };
    let (factual, essays) = (open(false), open(true));
    let mut results: Vec<Option<FieldResult>> = vec![None; fields.len()];

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
        Some(model::answer(&answers.facts, &job, &questions).await)
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
                (None, Err(why)) => FieldResult::Left(why),
                (None, Ok(_)) => unreachable!("every planned answer was entered above"),
            },
        })
        .collect();
    Ok(Report {
        outcomes,
        model_problem,
        note: None,
    })
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
        Err(Problem::Page(PageError::Bridge(error))) => Err(during(format!("filling {:?}", field.question.title))(
            PageError::Bridge(error),
        )),
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
