//! Asks a Claude model to answer form questions from the user's facts.
//!
//! It runs the `claude` command the user is already signed in to, so there is no API key.

use std::fmt::Write as _;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::process::Command;

use crate::answers::{Kind, Question};

const MODEL: &str = "sonnet";
const TIME_LIMIT: Duration = Duration::from_secs(90);

const INSTRUCTIONS: &str = "You fill in a job application for the applicant described in the facts. \
Answer each question only from the facts. If the facts do not settle a question, the answer is null. \
Never guess, and never invent experience, skills, dates or numbers. \
Always answer null for: pay or salary expectations, open-ended essay questions, demographic questions, \
and agreements, acknowledgments, certifications or signatures. \
A yes or no question takes exactly yes or no. A date takes YYYY-MM-DD. \
A question with options takes the exact text of one option. \
A location takes a city and state as a person would type them into a search box. \
Any other question takes the shortest direct answer, with no explanation. \
In basis, name the fact you used in one short sentence.";

/// What the model said about one question.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelAnswer {
    /// `None` when the facts do not settle the question.
    pub answer: Option<String>,
    /// The fact the model says it used.
    pub basis: String,
}

const ESSAY_INSTRUCTIONS: &str = "You draft answers to the written questions on a job application, \
for the applicant described by the resume and the facts. Write in the first person, as the applicant. \
Use only what the resume and the facts say. Never invent experience, employers, projects, tools or numbers. \
Tie the answer to the job where the job description gives a reason to. \
Write 60 to 130 words unless the question asks for another length. \
Use plain, specific sentences. No em dashes, no bullet lists, no greeting and no sign-off. \
Answer null when the resume does not cover what the question asks, \
or when the field asks no specific question, such as 'Additional information' or 'Cover letter'. \
In basis, name the part of the resume you used in one short sentence.";

/// Answers factual `questions` in order, from the user's facts. `job` says which posting the form belongs to.
pub async fn answer(facts: &[String], job: &str, questions: &[&Question]) -> Result<Vec<ModelAnswer>, String> {
    ask(INSTRUCTIONS, request(facts, job, questions), questions.len()).await
}

/// Drafts answers to essay `questions` in order, from the user's resume.
pub async fn draft(
    background: &[String],
    facts: &[String],
    job: &str,
    description: &str,
    questions: &[&Question],
) -> Result<Vec<ModelAnswer>, String> {
    let mut asked = String::from("The applicant's resume:\n");
    for line in background {
        let _ = writeln!(asked, "{line}");
    }
    let _ = write!(
        asked,
        "\n{}\nThe job description:\n{description}\n",
        request(facts, job, questions)
    );
    ask(ESSAY_INSTRUCTIONS, asked, questions.len()).await
}

async fn ask(instructions: &str, request: String, questions: usize) -> Result<Vec<ModelAnswer>, String> {
    let reply = run(instructions, request, &reply_schema()).await?;
    answers_in(&reply, questions)
}

/// Runs one model call whose reply must match `schema`, and returns the command's JSON output.
/// The model's reply is under `structured_output`.
pub(crate) async fn run(instructions: &str, request: String, schema: &Value) -> Result<Value, String> {
    let run = Command::new(claude_command())
        .arg("--print")
        .arg(request)
        .args(["--model", MODEL, "--output-format", "json"])
        // No tools, no project or user settings, no saved session: one question in, one answer out.
        .args(["--tools", "", "--strict-mcp-config", "--setting-sources", ""])
        .args(["--no-session-persistence", "--disable-slash-commands"])
        .args(["--system-prompt", instructions])
        .args(["--json-schema", &schema.to_string()])
        .current_dir(std::env::temp_dir())
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(TIME_LIMIT, run)
        .await
        .map_err(|_| "the model did not answer in time".to_owned())?
        .map_err(|error| format!("could not run the claude command: {error}"))?;
    let reply: Value = serde_json::from_slice(&output.stdout).map_err(|_| {
        let said = String::from_utf8_lossy(&output.stderr);
        format!("the claude command failed: {}", said.trim())
    })?;
    if reply["is_error"] == true {
        return Err(format!(
            "the model failed: {}",
            reply["result"].as_str().unwrap_or("no reason given")
        ));
    }
    Ok(reply)
}

/// True when the `claude` command can be found.
pub fn claude_found() -> bool {
    let command = claude_command();
    command.is_absolute()
        || std::env::var_os("PATH")
            .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(&command).is_file()))
}

/// `claude` is often installed where an app started from the Dock does not look.
fn claude_command() -> PathBuf {
    dirs::home_dir()
        .map(|home| home.join(".local/bin/claude"))
        .filter(|command| command.is_file())
        .unwrap_or_else(|| "claude".into())
}

fn request(facts: &[String], job: &str, questions: &[&Question]) -> String {
    let mut request = String::from("Facts about the applicant:\n");
    for fact in facts {
        let _ = writeln!(request, "- {fact}");
    }
    let _ = write!(request, "\nThe application is for: {job}\n\nQuestions:\n");
    for (index, question) in questions.iter().enumerate() {
        let kind = match &question.kind {
            Kind::YesNo => "yes or no".to_owned(),
            Kind::Date => "date".to_owned(),
            Kind::Location => "location".to_owned(),
            Kind::LongText => "long text".to_owned(),
            Kind::Choice { options } | Kind::Choices { options } => format!("one of {}", json!(options)),
            _ => "text".to_owned(),
        };
        let _ = writeln!(request, "{}. ({kind}) {}", index + 1, question.title);
    }
    request
}

fn reply_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "answers": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "id": { "type": "integer" },
                        "answer": { "type": ["string", "null"] },
                        "basis": { "type": "string" },
                    },
                    "required": ["id", "answer", "basis"],
                    "additionalProperties": false,
                },
            },
        },
        "required": ["answers"],
        "additionalProperties": false,
    })
}

/// Reads the model's answers out of the command's JSON, one per question, in question order.
/// A question the model skipped counts as unanswered.
fn answers_in(reply: &Value, questions: usize) -> Result<Vec<ModelAnswer>, String> {
    #[derive(Deserialize)]
    struct Given {
        id: usize,
        answer: Option<String>,
        basis: String,
    }
    let given: Vec<Given> = serde_json::from_value(reply["structured_output"]["answers"].clone())
        .map_err(|error| format!("the model's reply was not in the agreed form: {error}"))?;
    let mut answers = vec![
        ModelAnswer {
            answer: None,
            basis: String::new(),
        };
        questions
    ];
    for given in given {
        if let Some(slot) = given.id.checked_sub(1).and_then(|index| answers.get_mut(index)) {
            *slot = ModelAnswer {
                answer: given.answer.filter(|answer| !answer.trim().is_empty()),
                basis: given.basis,
            };
        }
    }
    Ok(answers)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn question(title: &str, kind: Kind) -> Question {
        Question {
            title: title.into(),
            kind,
            role: None,
        }
    }

    #[test]
    fn the_request_lists_facts_the_job_and_each_question_with_its_kind() {
        let timezone = question("Are you in a US timezone?", Kind::YesNo);
        let status = question(
            "Work status?",
            Kind::Choice {
                options: vec!["Citizen".into(), "Needs \"sponsorship\"".into()],
            },
        );
        let country = question("What country are you based in?", Kind::Text);
        let request = request(
            &["Lives in Springfield, Illinois.".into(), "Citizen of Canada.".into()],
            "Backend Engineer at Example Co, Remote (US)",
            &[&timezone, &status, &country],
        );
        assert_eq!(
            request,
            "Facts about the applicant:\n\
             - Lives in Springfield, Illinois.\n\
             - Citizen of Canada.\n\
             \n\
             The application is for: Backend Engineer at Example Co, Remote (US)\n\
             \n\
             Questions:\n\
             1. (yes or no) Are you in a US timezone?\n\
             2. (one of [\"Citizen\",\"Needs \\\"sponsorship\\\"\"]) Work status?\n\
             3. (text) What country are you based in?\n"
        );
    }

    #[test]
    fn answers_come_back_in_question_order_and_gaps_stay_unanswered() {
        let reply = json!({
            "is_error": false,
            "structured_output": { "answers": [
                { "id": 3, "answer": "United States", "basis": "Lives in Illinois." },
                { "id": 1, "answer": "yes", "basis": "Central time." },
                { "id": 2, "answer": null, "basis": "No fact covers pay." },
                { "id": 9, "answer": "stray", "basis": "" },
                { "id": 4, "answer": "  ", "basis": "blank" },
            ] },
        });
        let answers = answers_in(&reply, 5).unwrap();
        let said: Vec<Option<&str>> = answers.iter().map(|answer| answer.answer.as_deref()).collect();
        assert_eq!(said, [Some("yes"), None, Some("United States"), None, None]);
        assert_eq!(answers[0].basis, "Central time.");
    }

    #[test]
    fn a_malformed_reply_is_an_error() {
        assert!(answers_in(&json!({ "is_error": false, "result": "plain text" }), 1).is_err());
    }
}
