//! Fills an application form on a Greenhouse job board and stops. It never submits.
//!
//! The page holds the posting and its questions as data, and draws the form from that data.
//! The plan comes from the same data and not from guesses about the page. Greenhouse's public
//! API lists the questions too, but the page's content security policy blocks requests to it.

use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;

use crate::answers::{Answer, Answers, Kind, Question, Role};
use crate::apply::{
    self, Enter, Field, FillError, Form, Problem, Report, during, expect_value, plain_text, quoted, settles, text_of,
};
use crate::page::{Key, Locator, Page, PageError};

const FORM_WAIT: Duration = Duration::from_secs(20);
const OPTIONS_WAIT: Duration = Duration::from_secs(8);
const UPLOAD_WAIT: Duration = Duration::from_secs(30);

/// The element on the page Greenhouse opens once an application went through.
pub const SUBMITTED: &str = ".confirmation__content";

/// Gives the posting the page drew itself from, or null when the page holds none.
const FORM_DATA: &str = r#"(() => {
    const held = Object.values(window.__remixContext?.state?.loaderData ?? {}).find((data) => data?.jobPost);
    if (!held) return null;
    const { title, job_post_location, content, questions } = held.jobPost;
    return { title, job_post_location, content, questions };
})()"#;

/// The form, once it is ready for input. The server sends the form already drawn, and what is
/// typed before the page's script takes the form over is lost. The script's first act is to
/// ask for the credentials its file uploads need, and an upload before they arrive fails.
/// So the form is ready when that request is done.
const READY_FORM: &str = r#"(() => {
    const asked = performance
        .getEntriesByType("resource")
        .some((request) => request.name.includes("/uncacheable_attributes/presigned_fields"));
    return asked ? document.getElementById("application-form") : null;
})()"#;

/// The posting's address without anything a tracker or a campaign added to it.
/// An address on the older `boards.greenhouse.io` comes back on the host that serves the form.
pub fn posting_address(address: &str) -> Option<String> {
    posting(address).map(|posting| posting.address())
}

/// True for an address this module can fill.
pub fn is_posting(address: &str) -> bool {
    posting(address).is_some()
}

/// The address of a posting's application form. The form is on the posting's own page.
pub fn form_address(address: &str) -> Option<String> {
    posting_address(address)
}

/// Where a posting is on Greenhouse's hosted boards.
#[derive(Debug, PartialEq)]
struct Posting<'a> {
    /// `eu.` for a board in the European region, and empty for the others.
    region: &'a str,
    board: &'a str,
    id: &'a str,
    /// True when the address is the page that holds the form.
    on_form: bool,
}

impl Posting<'_> {
    fn address(&self) -> String {
        format!(
            "https://job-boards.{}greenhouse.io/{}/jobs/{}",
            self.region, self.board, self.id
        )
    }
}

fn posting(address: &str) -> Option<Posting<'_>> {
    let (host, path) = address.strip_prefix("https://")?.split_once('/')?;
    // `boards.` is the older host. It sends the browser on to `job-boards.`.
    let (current, region) = [("job-boards.", true), ("boards.", false)]
        .into_iter()
        .find_map(|(start, current)| Some((current, host.strip_prefix(start)?.strip_suffix("greenhouse.io")?)))?;
    let path = path.split(['?', '#']).next()?;
    let mut parts = path.split('/').filter(|part| !part.is_empty());
    let (board, jobs, id) = (parts.next()?, parts.next()?, parts.next()?);
    let plain = |text: &str| {
        text.bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
    };
    let is_posting =
        matches!(region, "" | "eu.") && plain(board) && jobs == "jobs" && id.bytes().all(|byte| byte.is_ascii_digit());
    is_posting.then(|| Posting {
        region,
        board,
        id,
        on_form: current && parts.next().is_none(),
    })
}

/// True for the address Greenhouse sends the browser to when a posting is no longer listed:
/// the board's front page, marked as an error.
fn says_closed(address: &str) -> bool {
    let on_a_board = address
        .strip_prefix("https://")
        .and_then(|rest| rest.split('/').next())
        .is_some_and(|host| host.ends_with(".greenhouse.io"));
    let marked = address
        .split_once('?')
        .is_some_and(|(_, query)| query.split(['&', '#']).any(|pair| pair == "error=true"));
    on_a_board && marked
}

/// Reads the form out of the posting that `FORM_DATA` gives.
pub fn form(job_post: &Value) -> Result<Form, FillError> {
    #[derive(Deserialize)]
    struct JobPost {
        title: String,
        job_post_location: Option<String>,
        content: Option<String>,
        questions: Vec<Asked>,
    }
    #[derive(Deserialize)]
    struct Asked {
        label: String,
        #[serde(default)]
        required: bool,
        fields: Vec<Described>,
    }
    #[derive(Deserialize)]
    struct Described {
        name: String,
        #[serde(rename = "type")]
        kind: String,
        #[serde(default)]
        values: Vec<Offered>,
    }
    #[derive(Deserialize)]
    struct Offered {
        label: String,
    }

    if job_post.is_null() {
        return Err(FillError::UnexpectedForm("the page holds no posting".to_owned()));
    }
    let post = JobPost::deserialize(job_post).map_err(|error| FillError::UnexpectedForm(error.to_string()))?;
    let mut fields = Vec::new();
    for asked in post.questions {
        // A question's other fields are other ways to give the same answer, such as pasted
        // text in place of a file. The page draws the first one.
        let Some(field) = asked.fields.into_iter().next() else {
            continue;
        };
        // The name goes into selectors, so it must be plain. A field that takes many options has `[]` at its end.
        if field.name.is_empty()
            || !field
                .name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_-[]".contains(&byte))
        {
            return Err(FillError::UnexpectedForm(format!("the field name {:?}", field.name)));
        }
        let options: Vec<String> = field.values.iter().map(|value| value.label.trim().to_owned()).collect();
        let (kind, role) = match (field.name.as_str(), field.kind.as_str()) {
            ("first_name", "input_text") => (Kind::Text, Some(Role::FirstName)),
            ("last_name", "input_text") => (Kind::Text, Some(Role::LastName)),
            ("email", "input_text") => (Kind::Email, None),
            ("phone", "input_text") => (Kind::Phone, None),
            ("candidate_location", "input_text") => (Kind::Location, None),
            ("resume", "input_file") => (Kind::File, Some(Role::Resume)),
            (_, "input_text") => (Kind::Text, None),
            (_, "textarea") => (Kind::LongText, None),
            (_, "input_file") => (Kind::File, None),
            (_, "multi_value_single_select") if is_yes_or_no(&options) => (Kind::YesNo, None),
            (_, "multi_value_single_select") => (Kind::Choice { options }, None),
            (_, "multi_value_multi_select") => (Kind::Choices { options }, None),
            (_, other) => (Kind::Unsupported(other.to_owned()), None),
        };
        fields.push(Field {
            path: field.name,
            required: asked.required,
            question: Question {
                title: asked.label.trim().to_owned(),
                kind,
                role,
            },
        });
    }
    let job = match post.job_post_location.as_deref().map(str::trim) {
        Some(place) if !place.is_empty() => format!("{}, {place}", post.title.trim()),
        _ => post.title.trim().to_owned(),
    };
    let description = plain_text(post.content.as_deref().unwrap_or_default());
    Ok(Form {
        job,
        description,
        fields,
    })
}

/// Greenhouse has no yes or no field. It offers the two words as a list.
fn is_yes_or_no(options: &[String]) -> bool {
    let mut words: Vec<String> = options.iter().map(|option| option.to_lowercase()).collect();
    words.sort();
    words == ["no", "yes"]
}

/// Fills every field it has an answer for and reports on each field, in the form's order.
/// The tab must be on a Greenhouse posting.
///
/// The standard answers go in first. A model then answers what they left, from the user's facts.
/// The demographic and equal opportunity sections are not among the posting's questions, so they stay as they are.
pub async fn fill(page: &Page, answers: &Answers) -> Result<Report, FillError> {
    let mut address = address_of(page).await?;
    if posting(&address).is_some_and(|posting| !posting.on_form) {
        let form = posting_address(&address).expect("the address was just read as a posting");
        page.navigate(&form).await.map_err(during("opening the form"))?;
        address = address_of(page).await?;
    }
    if says_closed(&address) {
        return Err(FillError::Closed);
    }
    if !posting(&address).is_some_and(|posting| posting.on_form) {
        return Err(FillError::NotAPosting(address));
    }
    let described = page
        .evaluate(FORM_DATA)
        .await
        .map_err(during("reading the form's questions from the page"))?;
    let form = form(&described)?;
    page.wait_for(&Locator::script("the application form", READY_FORM), FORM_WAIT)
        .await
        .map_err(during("waiting for the form to appear"))?;

    apply::fill_form(page, answers, form, &Greenhouse).await
}

async fn address_of(page: &Page) -> Result<String, FillError> {
    let address = page
        .evaluate("location.href")
        .await
        .map_err(during("reading the tab's address"))?;
    Ok(address.as_str().unwrap_or_default().to_owned())
}

/// How answers go into a Greenhouse form.
struct Greenhouse;

impl Enter for Greenhouse {
    async fn enter(&self, page: &Page, field: &Field, answer: &Answer) -> Result<String, Problem> {
        enter(page, field, answer).await
    }
}

/// Enters one answer and returns what the field shows afterwards.
///
/// A list or a file field that already shows the answer is left alone, and a text field
/// takes the same text again. So a second fill changes nothing.
async fn enter(page: &Page, field: &Field, answer: &Answer) -> Result<String, Problem> {
    let path = &field.path;
    match (answer, &field.question.kind) {
        (Answer::Text(text), Kind::Location) => {
            let starts_with = format!("text.toLowerCase().startsWith({})", quoted(&text.to_lowercase()));
            match choose(page, path, text, &starts_with).await {
                Err(Problem::Page(error @ PageError::NoElement { .. })) => {
                    let closest = text_of(page, &option("a place", path, "true")).await?;
                    Err(if closest.is_empty() {
                        Problem::Page(error)
                    } else {
                        Problem::Shows(format!("{closest} as the closest place"))
                    })
                }
                entered => entered,
            }
        }
        (Answer::Text(text), Kind::Phone) => {
            let input = input(path);
            page.fill(&input, text).await?;
            // The field may regroup the digits as it formats the number, so only the digits must match.
            let value = format!("({})?.value ?? ''", input.as_script());
            let digits: String = text.chars().filter(char::is_ascii_digit).collect();
            let same_number = format!("({value}).replace(/\\D/g, '') === {}", quoted(&digits));
            let entered = settles(page, &same_number).await?;
            let shown = page.evaluate(&value).await?.as_str().unwrap_or_default().to_owned();
            if entered { Ok(shown) } else { Err(Problem::Shows(shown)) }
        }
        (Answer::Text(text), _) => {
            let input = input(path);
            page.fill(&input, text).await?;
            expect_value(page, &input, text).await
        }
        (Answer::YesNo(yes), _) => {
            let word = if *yes { "yes" } else { "no" };
            choose(page, path, word, &format!("text.toLowerCase() === {}", quoted(word))).await
        }
        (Answer::Choice(wanted), _) => choose(page, path, wanted, &format!("text === {}", quoted(wanted))).await,
        (Answer::Choices(options), _) => {
            // The page draws these as check boxes or as one list, as the company set it.
            let boxes = Locator::css(&format!("fieldset[id=\"{path}\"]"));
            let as_boxes = page.evaluate(&format!("!!({})", boxes.as_script())).await? == true;
            for wanted in options {
                if as_boxes {
                    tick(page, path, wanted).await?;
                } else {
                    choose(page, path, wanted, &format!("text === {}", quoted(wanted))).await?;
                }
            }
            Ok(options.join(", "))
        }
        (Answer::File(file), _) => {
            let name = file
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            // Greenhouse shows the file's name once it has taken the file, and takes the file input away.
            let attached = Locator::css(&format!(
                "[aria-labelledby=\"upload-label-{path}\"] .file-upload__filename p"
            ));
            let before = text_of(page, &attached).await?;
            if before == name {
                return Ok(name);
            }
            if !before.is_empty() {
                return Err(Problem::Shows(before));
            }
            let input = Locator::css(&format!("input[type=\"file\"][id=\"{path}\"]"));
            page.upload(&input, &[file]).await?;
            let shown = Locator::script(
                format!("the uploaded file {name}"),
                format!(
                    "(() => {{ const shown = {}; return shown?.textContent.trim() === {} ? shown : null; }})()",
                    attached.as_script(),
                    quoted(&name)
                ),
            );
            page.wait_for(&shown, UPLOAD_WAIT).await?;
            Ok(name)
        }
        (Answer::Date(_), _) => Err(Problem::Mismatch("Greenhouse forms have no date fields")),
    }
}

/// Chooses the option of a list that passes `wanted`, unless the list shows it already.
/// Returns what the list shows for it. `wanted` is a JavaScript test on `text`, an option's text.
///
/// A list that asks a service for its options, as the location does, shows the choice a moment after the click.
async fn choose(page: &Page, path: &str, typed: &str, wanted: &str) -> Result<String, Problem> {
    let chosen = chosen(path, wanted);
    let shown = format!("({chosen})?.textContent.trim() ?? ''");
    if page.evaluate(&format!("!!({chosen})")).await? != true {
        pick(page, path, typed, wanted).await?;
        if !settles(page, &format!("!!({chosen})")).await? {
            return Err(Problem::Shows(shown_choices(page, path).await?));
        }
    }
    Ok(page.evaluate(&shown).await?.as_str().unwrap_or_default().to_owned())
}

/// Searches a list for `typed` and clicks the option that passes `wanted`.
async fn pick(page: &Page, path: &str, typed: &str, wanted: &str) -> Result<(), Problem> {
    page.fill(&input(path), typed).await?;
    // Typed text does not open the list. A key does.
    page.press(Key::ArrowDown).await?;
    let offered = option(format!("the option {typed:?}"), path, wanted);
    page.wait_for(&offered, OPTIONS_WAIT).await?;
    page.click(&offered).await?;
    Ok(())
}

/// Ticks the check box with this label, unless it is ticked already.
async fn tick(page: &Page, path: &str, wanted: &str) -> Result<(), Problem> {
    let label = Locator::script(
        format!("the option {wanted:?}"),
        format!(
            "[...document.querySelectorAll('fieldset[id=\"{path}\"] label')]\
             .find((label) => label.textContent.trim() === {}) ?? null",
            quoted(wanted)
        ),
    );
    let ticked = format!("(({})?.control?.checked) === true", label.as_script());
    if page.evaluate(&ticked).await? != true {
        page.click(&label).await?;
    }
    if settles(page, &ticked).await? {
        Ok(())
    } else {
        Err(Problem::Mismatch("the option did not stay ticked"))
    }
}

/// The input a field's answer is typed into. Its id is the field's name, except for the location.
fn input(path: &str) -> Locator {
    let id = if path == "candidate_location" {
        "candidate-location"
    } else {
        path
    };
    Locator::css(&format!("[id=\"{id}\"]"))
}

/// The box around a list field: its input, the options it has chosen, and its open list.
fn list(path: &str) -> String {
    format!("({})?.closest('.select__container')", input(path).as_script())
}

/// The first option in a field's open list that passes `wanted`. `name` is what errors call it.
fn option(name: impl Into<String>, path: &str, wanted: &str) -> Locator {
    Locator::script(name, among(&list(path), ".select__option", wanted))
}

/// The option a list field shows as chosen, if it passes `wanted`. A list that takes many shows each one.
fn chosen(path: &str, wanted: &str) -> String {
    among(
        &list(path),
        ".select__single-value, .select__multi-value__label",
        wanted,
    )
}

/// The first element under `root` that matches `selector` and whose text passes `wanted`.
fn among(root: &str, selector: &str, wanted: &str) -> String {
    format!(
        "[...({root})?.querySelectorAll({}) ?? []]\
         .find((element) => {{ const text = element.textContent.trim(); return {wanted}; }}) ?? null",
        quoted(selector)
    )
}

/// Everything a list field shows as chosen.
async fn shown_choices(page: &Page, path: &str) -> Result<String, Problem> {
    let shown = page
        .evaluate(&format!(
            "[...({})?.querySelectorAll('.select__single-value, .select__multi-value__label') ?? []]\
             .map((element) => element.textContent.trim()).join(', ')",
            list(path)
        ))
        .await?;
    Ok(shown.as_str().unwrap_or_default().to_owned())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_posting_address_is_recognized_on_every_hosted_board() {
        let posting = "https://job-boards.greenhouse.io/example-co/jobs/4012345006";
        assert_eq!(
            super::posting(posting),
            Some(Posting {
                region: "",
                board: "example-co",
                id: "4012345006",
                on_form: true
            })
        );
        assert_eq!(posting_address(posting).as_deref(), Some(posting));
        assert_eq!(form_address(posting).as_deref(), Some(posting));
        assert_eq!(
            posting_address(&format!("{posting}?gh_jid=4012345006&utm_source=x#app")).as_deref(),
            Some(posting)
        );
        // The older host sends the browser on, so the form is one step away.
        let older = "https://boards.greenhouse.io/example-co/jobs/4012345006?gh_jid=4012345006";
        assert!(!super::posting(older).unwrap().on_form);
        assert_eq!(form_address(older).as_deref(), Some(posting));
        assert_eq!(
            posting_address("https://boards.eu.greenhouse.io/example_co/jobs/4012345101").as_deref(),
            Some("https://job-boards.eu.greenhouse.io/example_co/jobs/4012345101")
        );
        // The page after a submission belongs to the posting, and it is not the form.
        let submitted = format!("{posting}/confirmation");
        assert!(!super::posting(&submitted).unwrap().on_form);
        assert_eq!(posting_address(&submitted).as_deref(), Some(posting));
    }

    #[test]
    fn an_address_that_is_no_posting_is_not_recognized() {
        for address in [
            "https://job-boards.greenhouse.io/example-co",
            "https://job-boards.greenhouse.io/example-co?error=true",
            "https://job-boards.greenhouse.io/example-co/jobs",
            "https://job-boards.greenhouse.io/example-co/jobs/latest",
            "https://job-boards.greenhouse.io/embed/job_app?for=example-co&token=4012345006",
            "https://job-boards.xx.greenhouse.io/example-co/jobs/4012345006",
            "https://app.greenhouse.io/example-co/jobs/4012345006",
            "https://job-boards.greenhouse.io.example.com/example-co/jobs/4012345006",
            "https://example.com/careers?gh_jid=4012345006",
            "http://job-boards.greenhouse.io/example-co/jobs/4012345006",
        ] {
            assert!(!is_posting(address), "{address}");
        }
    }

    #[test]
    fn a_posting_greenhouse_no_longer_lists_is_told_by_where_the_browser_lands() {
        assert!(says_closed("https://job-boards.greenhouse.io/example-co?error=true"));
        assert!(says_closed(
            "https://job-boards.eu.greenhouse.io/example-co?gh_jid=1&error=true"
        ));
        assert!(!says_closed("https://job-boards.greenhouse.io/example-co"));
        assert!(!says_closed("https://example.com/careers?error=true"));
    }

    fn asked(name: &str, label: &str, kind: &str, required: bool) -> Value {
        json!({ "label": label, "required": required, "description": null, "fields": [{ "name": name, "type": kind }] })
    }

    fn offering(mut question: Value, labels: &[&str]) -> Value {
        let values: Vec<Value> = labels
            .iter()
            .enumerate()
            .map(|(value, label)| json!({ "value": value, "label": label }))
            .collect();
        question["fields"][0]["values"] = json!(values);
        question
    }

    fn job_post(questions: Vec<Value>) -> Value {
        json!({
            "title": " Backend Engineer ",
            "job_post_location": "Springfield, IL",
            "content": "<p>Build <b>fast</b> things &amp; ship.</p><ul><li>Rust</li><li>SQL</li></ul>",
            "questions": questions,
        })
    }

    #[test]
    fn the_posting_in_the_page_becomes_fields() {
        let mut resume = asked("resume", "Resume/CV", "input_file", true);
        resume["fields"]
            .as_array_mut()
            .unwrap()
            .push(json!({ "name": "resume_text", "type": "textarea" }));
        let Form {
            job,
            description,
            fields,
        } = form(&job_post(vec![
            asked("first_name", "First Name", "input_text", true),
            asked("last_name", "Last Name", "input_text", true),
            asked("preferred_name", "Preferred First Name", "input_text", false),
            asked("email", "Email", "input_text", true),
            asked("phone", "Phone", "input_text", false),
            asked("candidate_location", "Candidate Location", "input_text", true),
            resume,
            asked("cover_letter", "Cover Letter", "input_file", false),
            asked("question_101", "LinkedIn Profile", "input_text", false),
            asked("question_102", " Why us?\n", "textarea", true),
            offering(
                asked(
                    "question_103",
                    "Will you need sponsorship?",
                    "multi_value_single_select",
                    true,
                ),
                &["Yes", "No"],
            ),
            offering(
                asked(
                    "question_104",
                    "How did you hear about us?",
                    "multi_value_single_select",
                    false,
                ),
                &["Friend ", "Other"],
            ),
            offering(
                asked(
                    "question_105[]",
                    "Which offices suit you?",
                    "multi_value_multi_select",
                    false,
                ),
                &["Springfield", "Remote"],
            ),
            asked("question_106", "Sign here", "signature", false),
            json!({ "label": "Nothing to draw", "required": false, "fields": [] }),
        ]))
        .unwrap();

        assert_eq!(job, "Backend Engineer, Springfield, IL");
        assert_eq!(description, "Build fast things & ship.\nRust\nSQL");
        let field = |path: &str, required, title: &str, kind, role| Field {
            path: path.into(),
            required,
            question: Question {
                title: title.into(),
                kind,
                role,
            },
        };
        let options = |labels: &[&str]| labels.iter().map(|&label| label.to_owned()).collect();
        assert_eq!(
            fields,
            vec![
                field("first_name", true, "First Name", Kind::Text, Some(Role::FirstName)),
                field("last_name", true, "Last Name", Kind::Text, Some(Role::LastName)),
                field("preferred_name", false, "Preferred First Name", Kind::Text, None),
                field("email", true, "Email", Kind::Email, None),
                field("phone", false, "Phone", Kind::Phone, None),
                field("candidate_location", true, "Candidate Location", Kind::Location, None),
                field("resume", true, "Resume/CV", Kind::File, Some(Role::Resume)),
                field("cover_letter", false, "Cover Letter", Kind::File, None),
                field("question_101", false, "LinkedIn Profile", Kind::Text, None),
                field("question_102", true, "Why us?", Kind::LongText, None),
                field("question_103", true, "Will you need sponsorship?", Kind::YesNo, None),
                field(
                    "question_104",
                    false,
                    "How did you hear about us?",
                    Kind::Choice {
                        options: options(&["Friend", "Other"])
                    },
                    None
                ),
                field(
                    "question_105[]",
                    false,
                    "Which offices suit you?",
                    Kind::Choices {
                        options: options(&["Springfield", "Remote"])
                    },
                    None
                ),
                field(
                    "question_106",
                    false,
                    "Sign here",
                    Kind::Unsupported("signature".into()),
                    None
                ),
            ]
        );
    }

    #[test]
    fn a_list_of_yes_and_no_is_a_yes_or_no_question_however_it_is_spelled() {
        let kind_of = |labels: &[&str]| {
            let question = offering(asked("question_1", "Agree?", "multi_value_single_select", true), labels);
            form(&job_post(vec![question])).unwrap().fields.remove(0).question.kind
        };
        assert_eq!(kind_of(&["No", "Yes"]), Kind::YesNo);
        assert_eq!(kind_of(&["YES", "no "]), Kind::YesNo);
        assert!(matches!(kind_of(&["Yes", "No", "N/A"]), Kind::Choice { .. }));
        assert!(matches!(kind_of(&["Yes"]), Kind::Choice { .. }));
    }

    #[test]
    fn a_posting_without_a_place_is_named_by_its_title() {
        let mut post = job_post(vec![]);
        post["job_post_location"] = json!(null);
        post["content"] = json!(null);
        let form = form(&post).unwrap();
        assert_eq!((form.job.as_str(), form.description.as_str()), ("Backend Engineer", ""));
    }

    #[test]
    fn a_page_without_a_readable_posting_is_refused() {
        assert!(matches!(form(&Value::Null), Err(FillError::UnexpectedForm(_))));
        assert!(matches!(
            form(&json!({ "title": "Backend Engineer" })),
            Err(FillError::UnexpectedForm(_))
        ));
        let hostile = job_post(vec![asked("a\"] , body [x=\"", "Name", "input_text", true)]);
        assert!(matches!(form(&hostile), Err(FillError::UnexpectedForm(_))));
    }
}
