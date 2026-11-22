//! Fills an application form on `jobs.ashbyhq.com` and stops. It never submits.
//!
//! Ashby's own API lists a posting's fields, so the plan comes from that list and
//! not from guesses about the page.

use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::answers::{Answer, Answers, Kind, Question, Role};
use crate::apply::{
    self, Enter, Field, FillError, Form, Problem, Report, during, expect_value, plain_text, quoted, settles, text_of,
};
use crate::page::{Key, Locator, Page};

const HOST: &str = "https://jobs.ashbyhq.com/";
const FORM_WAIT: Duration = Duration::from_secs(20);
const OPTIONS_WAIT: Duration = Duration::from_secs(8);
const UPLOAD_WAIT: Duration = Duration::from_secs(30);

const FORM_QUERY: &str = "query ApiJobPosting($organizationHostedJobsPageName: String!, $jobPostingId: String!) { \
    jobPosting(organizationHostedJobsPageName: $organizationHostedJobsPageName, jobPostingId: $jobPostingId) { \
    title locationName descriptionHtml applicationForm { sections { fieldEntries { ... on FormFieldEntry { field isRequired } } } } } }";

/// The element Ashby shows in place of the form once an application went through.
pub const SUBMITTED: &str = ".ashby-application-form-success-container";

/// The posting's address without anything a tracker or a campaign added to it.
pub fn posting_address(address: &str) -> Option<String> {
    posting_path(address).map(|(posting, _)| format!("{HOST}{posting}"))
}

/// True for an address this module can fill.
pub fn is_posting(address: &str) -> bool {
    posting_path(address).is_some()
}

/// The address of a posting's application form.
pub fn form_address(address: &str) -> Option<String> {
    posting_path(address).map(|(posting, _)| format!("{HOST}{posting}/application"))
}

/// The `organization/posting` part of a posting's address, and whether the address is the form itself.
fn posting_path(address: &str) -> Option<(&str, bool)> {
    let path = address.strip_prefix(HOST)?;
    let path = path.split(['?', '#']).next()?;
    let mut parts = path.split('/').filter(|part| !part.is_empty());
    let (organization, posting) = (parts.next()?, parts.next()?);
    let is_id = posting.len() == 36 && posting.bytes().all(|byte| byte.is_ascii_hexdigit() || byte == b'-');
    is_id.then(|| {
        let end = organization.len() + 1 + posting.len();
        (&path[..end], parts.next() == Some("application"))
    })
}

/// Reads the form out of Ashby's reply to `FORM_QUERY`.
pub fn form(reply: &Value) -> Result<Form, FillError> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Reply {
        data: Data,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Data {
        job_posting: Option<Posting>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Posting {
        title: String,
        location_name: Option<String>,
        description_html: Option<String>,
        application_form: Sections,
    }
    #[derive(Deserialize)]
    struct Sections {
        sections: Vec<Section>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Section {
        field_entries: Vec<Entry>,
    }
    /// An entry that is not a field comes back empty.
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Entry {
        field: Option<Described>,
        is_required: Option<bool>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Described {
        path: String,
        title: String,
        #[serde(rename = "type")]
        kind: String,
        #[serde(default)]
        selectable_values: Vec<Selectable>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Selectable {
        label: String,
        #[serde(default)]
        is_archived: bool,
    }

    let reply = Reply::deserialize(reply).map_err(|error| FillError::UnexpectedForm(error.to_string()))?;
    let posting = reply.data.job_posting.ok_or(FillError::Closed)?;
    let mut fields = Vec::new();
    for entry in posting
        .application_form
        .sections
        .into_iter()
        .flat_map(|section| section.field_entries)
    {
        let Some(field) = entry.field else { continue };
        // The path goes into CSS selectors, so it must be plain.
        if field.path.is_empty()
            || !field
                .path
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
        {
            return Err(FillError::UnexpectedForm(format!("the field path {:?}", field.path)));
        }
        let options = || {
            field
                .selectable_values
                .iter()
                .filter(|value| !value.is_archived)
                .map(|value| value.label.clone())
                .collect()
        };
        let kind = match field.kind.as_str() {
            "String" | "Url" => Kind::Text,
            "LongText" => Kind::LongText,
            "Email" => Kind::Email,
            "Phone" => Kind::Phone,
            "Boolean" => Kind::YesNo,
            "Date" => Kind::Date,
            "Location" => Kind::Location,
            "File" => Kind::File,
            "ValueSelect" => Kind::Choice { options: options() },
            "MultiValueSelect" => Kind::Choices { options: options() },
            other => Kind::Unsupported(other.to_owned()),
        };
        let role = match field.path.as_str() {
            "_systemfield_name" => Some(Role::Name),
            "_systemfield_resume" => Some(Role::Resume),
            _ => None,
        };
        fields.push(Field {
            required: entry.is_required.unwrap_or(false),
            question: Question {
                title: field.title.trim().to_owned(),
                kind,
                role,
            },
            path: field.path,
        });
    }
    let job = match posting.location_name {
        Some(place) => format!("{}, {place}", posting.title.trim()),
        None => posting.title.trim().to_owned(),
    };
    let description = plain_text(posting.description_html.as_deref().unwrap_or_default());
    Ok(Form {
        job,
        description,
        fields,
    })
}

/// Fills every field it has an answer for and reports on each field, in the form's order.
/// The tab must be on an Ashby posting or its form.
///
/// The standard answers go in first. A model then answers what they left, from the user's facts.
pub async fn fill(page: &Page, answers: &Answers) -> Result<Report, FillError> {
    let address = page
        .evaluate("location.href")
        .await
        .map_err(during("reading the tab's address"))?;
    let address = address.as_str().unwrap_or_default();
    let (posting, on_form) = posting_path(address).ok_or_else(|| FillError::NotAPosting(address.to_owned()))?;
    if !on_form {
        page.navigate(&format!("{HOST}{posting}/application"))
            .await
            .map_err(during("opening the form"))?;
    }
    let described = page
        .evaluate(&form_request())
        .await
        .map_err(during("asking Ashby for the form's fields"))?;
    let Form {
        job,
        description,
        fields,
    } = form(&described)?;
    if let Some(first) = fields.first() {
        page.wait_for(&heading(&first.path), FORM_WAIT)
            .await
            .map_err(during("waiting for the form to appear"))?;
    }

    apply::fill_form(
        page,
        answers,
        Form {
            job,
            description,
            fields,
        },
        &Ashby,
    )
    .await
}

/// How answers go into an Ashby form.
struct Ashby;

impl Enter for Ashby {
    async fn enter(&self, page: &Page, field: &Field, answer: &Answer) -> Result<String, Problem> {
        enter(page, field, answer).await
    }
}

/// Enters one answer and returns what the field shows afterwards.
async fn enter(page: &Page, field: &Field, answer: &Answer) -> Result<String, Problem> {
    let path = &field.path;
    match (answer, &field.question.kind) {
        (Answer::Text(text), Kind::Location) => {
            let search = combobox(path);
            page.fill(&search, text).await?;
            let first = Locator::css("[role=\"option\"]");
            page.wait_for(&first, OPTIONS_WAIT).await?;
            let offered = text_of(page, &first).await?;
            if !offered.to_lowercase().starts_with(&text.to_lowercase()) {
                return Err(Problem::Shows(format!("{offered} as the closest place")));
            }
            page.click(&first).await?;
            expect_value(page, &search, &offered).await
        }
        (Answer::Text(text), _) => {
            let input = Locator::css(&format!("[id=\"{path}\"]"));
            page.fill(&input, text).await?;
            expect_value(page, &input, text).await
        }
        (Answer::YesNo(yes), _) => {
            let option = if *yes { "yes" } else { "no" };
            let button = Locator::css(&format!(
                "[data-field-path=\"{path}\"] button[data-option=\"{option}\"]"
            ));
            let pressed = format!("({})?.getAttribute('aria-pressed') === 'true'", button.as_script());
            // A second click would take the answer back.
            if page.evaluate(&pressed).await? != true {
                page.click(&button).await?;
            }
            if settles(page, &pressed).await? {
                Ok(if *yes { "Yes" } else { "No" }.to_owned())
            } else {
                Err(Problem::Mismatch("the button did not stay pressed"))
            }
        }
        (Answer::Date(date), _) => {
            let input = Locator::css(&format!("[data-field-path=\"{path}\"] input"));
            let written = date.strftime("%m/%d/%Y").to_string();
            page.fill(&input, &written).await?;
            page.press(Key::Enter).await?;
            expect_value(page, &input, &written).await
        }
        (Answer::File(file), _) => {
            let input = Locator::css(&format!("input[type=\"file\"][id=\"{path}\"]"));
            page.upload(&input, &[file]).await?;
            let name = file
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            // Ashby shows the file's name once it has taken the file.
            let shown = Locator::script(
                format!("the uploaded file {name}"),
                format!(
                    "[...document.querySelectorAll('[data-field-path=\"{path}\"] *')]\
                     .find((element) => element.children.length === 0 && element.textContent.includes({})) ?? null",
                    quoted(&name)
                ),
            );
            page.wait_for(&shown, UPLOAD_WAIT).await?;
            Ok(name)
        }
        (Answer::Choice(option), _) => {
            let search = combobox(path);
            if page.evaluate(&format!("!!({})", search.as_script())).await? == true {
                page.fill(&search, option).await?;
                let offered = Locator::script(
                    format!("the option {option:?}"),
                    format!(
                        "[...document.querySelectorAll('[role=\"option\"]')]\
                         .find((element) => element.textContent.trim() === {}) ?? null",
                        quoted(option)
                    ),
                );
                page.wait_for(&offered, OPTIONS_WAIT).await?;
                page.click(&offered).await?;
                expect_value(page, &search, option).await
            } else {
                check(page, path, option).await?;
                Ok(option.clone())
            }
        }
        (Answer::Choices(options), _) => {
            for option in options {
                check(page, path, option).await?;
            }
            Ok(options.join(", "))
        }
    }
}

/// Ticks the radio button or check box with this label, unless it is ticked already.
async fn check(page: &Page, path: &str, option: &str) -> Result<(), Problem> {
    let label = Locator::script(
        format!("the option {option:?}"),
        format!(
            "[...document.querySelectorAll('label[for=\"{path}\"] ~ div label')]\
             .find((label) => label.textContent.trim() === {}) ?? null",
            quoted(option)
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

/// The question's heading. Every kind of field has one.
fn heading(path: &str) -> Locator {
    Locator::css(&format!("label[for=\"{path}\"]"))
}

/// The search box of a field that offers a list.
fn combobox(path: &str) -> Locator {
    Locator::css(&format!("label[for=\"{path}\"] ~ div input[role=\"combobox\"]"))
}

/// Asks Ashby for the form from inside the page, where the request is same-origin.
fn form_request() -> String {
    let body = json!({
        "operationName": "ApiJobPosting",
        "query": FORM_QUERY,
    });
    format!(
        r#"(async () => {{
            const [, organization, posting] = location.pathname.split("/").map(decodeURIComponent);
            const body = {body};
            body.variables = {{ organizationHostedJobsPageName: organization, jobPostingId: posting }};
            const reply = await fetch("/api/non-user-graphql?op=ApiJobPosting", {{
                method: "POST",
                headers: {{ "content-type": "application/json" }},
                body: JSON.stringify(body),
            }});
            return await reply.json();
        }})()"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_posting_address_is_recognized_with_or_without_the_form() {
        let posting = "example-co/0f3b8231-3a32-478a-b994-3441f4bb9455";
        assert_eq!(
            posting_path(&format!("https://jobs.ashbyhq.com/{posting}")),
            Some((posting, false))
        );
        assert_eq!(
            posting_path(&format!(
                "https://jobs.ashbyhq.com/{posting}/application?utm_source=x#top"
            )),
            Some((posting, true))
        );
        assert_eq!(posting_path("https://jobs.ashbyhq.com/example-co"), None);
        assert_eq!(posting_path("https://jobs.ashbyhq.com/example-co/about-us"), None);
        assert_eq!(posting_path(&format!("https://example.com/{posting}")), None);
        assert!(is_posting(&format!("https://jobs.ashbyhq.com/{posting}")));
        assert_eq!(
            posting_address(&format!("https://jobs.ashbyhq.com/{posting}/application?utm_source=x")),
            Some(format!("https://jobs.ashbyhq.com/{posting}"))
        );
        assert_eq!(
            form_address(&format!("https://jobs.ashbyhq.com/{posting}?utm_source=x")),
            Some(format!("https://jobs.ashbyhq.com/{posting}/application"))
        );
    }

    fn entry(path: &str, title: &str, kind: &str, required: bool) -> Value {
        json!({ "field": { "path": path, "title": title, "type": kind }, "isRequired": required })
    }

    fn reply(entries: Vec<Value>) -> Value {
        json!({ "data": { "jobPosting": {
            "title": " Backend Engineer ",
            "locationName": "Remote (US)",
            "descriptionHtml": "<p>Build <b>fast</b> things &amp; ship.</p><ul><li>Rust</li><li>SQL</li></ul>",
            "applicationForm": { "sections": [{ "fieldEntries": entries }] },
        } } })
    }

    #[test]
    fn the_form_reply_becomes_fields() {
        let mut select = entry("a1b2", "How did you hear about us?", "ValueSelect", false);
        select["field"]["selectableValues"] = json!([
            { "label": "Friend", "value": "Friend" },
            { "label": "Old option", "value": "Old option", "isArchived": true },
            { "label": "Other", "value": "Other", "isArchived": false },
        ]);
        let Form {
            job,
            description,
            fields,
        } = form(&reply(vec![
            entry("_systemfield_name", " Name ", "String", true),
            entry("_systemfield_resume", "Resume", "File", true),
            json!({}),
            select,
            entry("c3d4", "Sign here", "Signature", false),
        ]))
        .unwrap();

        assert_eq!(job, "Backend Engineer, Remote (US)");
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
        assert_eq!(
            fields,
            vec![
                field("_systemfield_name", true, "Name", Kind::Text, Some(Role::Name)),
                field("_systemfield_resume", true, "Resume", Kind::File, Some(Role::Resume)),
                field(
                    "a1b2",
                    false,
                    "How did you hear about us?",
                    Kind::Choice {
                        options: vec!["Friend".into(), "Other".into()]
                    },
                    None
                ),
                field("c3d4", false, "Sign here", Kind::Unsupported("Signature".into()), None),
            ]
        );
    }

    #[test]
    fn a_posting_ashby_no_longer_lists_is_closed() {
        assert_eq!(form(&json!({ "data": { "jobPosting": null } })), Err(FillError::Closed));
    }

    #[test]
    fn a_field_path_that_could_break_a_selector_is_refused() {
        let hostile = reply(vec![entry("a\"] , body [x=\"", "Name", "String", true)]);
        assert!(matches!(form(&hostile), Err(FillError::UnexpectedForm(_))));
        assert!(matches!(
            form(&json!({ "errors": [] })),
            Err(FillError::UnexpectedForm(_))
        ));
    }
}
