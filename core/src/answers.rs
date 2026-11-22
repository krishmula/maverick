//! The user's standard answers, and how a question on an application form gets one.
//!
//! A question the answers do not cover gets no answer. Nothing here guesses.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use jiff::civil::Date;
use serde::Deserialize;

/// The standard answers file. It holds personal details, so it lives outside the repository.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Answers {
    pub name: String,
    pub email: String,
    pub phone: String,
    pub resume: PathBuf,
    /// As the user would type it into a location search, such as `Springfield, Illinois`.
    #[serde(default)]
    pub location: Option<String>,
    #[serde(default)]
    pub rules: Vec<Rule>,
    /// Facts about the user, one per entry. A model answers from these what the rules do not cover.
    /// With no facts, no model is asked.
    #[serde(default)]
    pub facts: Vec<String>,
    /// The user's resume as text, one line per entry. A model drafts answers to essay questions
    /// from it. With no background, essay questions are left for the user.
    #[serde(default)]
    pub background: Vec<String>,
}

/// Answers every question whose text holds one of `any` and none of `not`.
/// Phrases are matched without regard to case.
///
/// A yes or no question takes `yes` or `no`. A date takes `YYYY-MM-DD`.
/// A question with options takes the text of one option.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    pub any: Vec<String>,
    #[serde(default)]
    pub not: Vec<String>,
    pub answer: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Question {
    pub title: String,
    pub kind: Kind,
    /// What the application system says the field is for, when it says.
    pub role: Option<Role>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    Text,
    LongText,
    Email,
    Phone,
    YesNo,
    Date,
    Location,
    File,
    /// Pick one option.
    Choice {
        options: Vec<String>,
    },
    /// Pick any number of options.
    Choices {
        options: Vec<String>,
    },
    /// A kind of field this code does not fill.
    Unsupported(String),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Role {
    Name,
    /// A form that asks for the name in two fields marks each of them.
    FirstName,
    LastName,
    Resume,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Answer {
    Text(String),
    YesNo(bool),
    Date(Date),
    File(PathBuf),
    /// The option to pick, spelled as the form spells it.
    Choice(String),
    Choices(Vec<String>),
}

/// Why a question got no answer.
#[derive(Clone, Debug, PartialEq)]
pub enum Unanswered {
    NoRule,
    /// More than one rule covers the question, and they give different answers.
    RulesDisagree,
    NotYesOrNo(String),
    NotADate(String),
    NoSuchOption(String),
    Unsupported(String),
}

impl fmt::Display for Unanswered {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoRule => f.write_str("your standard answers do not cover this"),
            Self::RulesDisagree => f.write_str("two of your standard answers cover this and they differ"),
            Self::NotYesOrNo(answer) => write!(f, "this needs yes or no, and your standard answer is {answer:?}"),
            Self::NotADate(answer) => write!(f, "this needs a date, and your standard answer is {answer:?}"),
            Self::NoSuchOption(answer) => write!(f, "none of the options is your standard answer, {answer:?}"),
            Self::Unsupported(kind) => write!(f, "Maverick cannot fill a {kind} field yet"),
        }
    }
}

impl Kind {
    /// Turns an answer written as text into the form this kind of field takes.
    pub fn answer_from(&self, stated: &str) -> Result<Answer, Unanswered> {
        let stated = stated.trim();
        match self {
            Self::Text | Self::LongText | Self::Email | Self::Phone => Ok(Answer::Text(match stated.parse::<Date>() {
                Ok(date) => date.strftime("%B %-d, %Y").to_string(),
                Err(_) => stated.to_owned(),
            })),
            Self::Location => Ok(Answer::Text(stated.to_owned())),
            Self::YesNo => match stated.to_lowercase().as_str() {
                "yes" => Ok(Answer::YesNo(true)),
                "no" => Ok(Answer::YesNo(false)),
                _ => Err(Unanswered::NotYesOrNo(stated.to_owned())),
            },
            Self::Date => stated
                .parse()
                .map(Answer::Date)
                .map_err(|_| Unanswered::NotADate(stated.to_owned())),
            Self::Choice { options } => option(options, stated).map(Answer::Choice),
            Self::Choices { options } => option(options, stated).map(|option| Answer::Choices(vec![option])),
            // A model cannot supply a file, and an unsupported field cannot be filled at all.
            Self::File => Err(Unanswered::NoRule),
            Self::Unsupported(kind) => Err(Unanswered::Unsupported(kind.clone())),
        }
    }
}

impl Answers {
    pub fn load(file: &Path) -> Result<Self, String> {
        let text = fs::read_to_string(file).map_err(|error| format!("could not read {}: {error}", file.display()))?;
        serde_json::from_str(&text).map_err(|error| format!("{} is not a valid answers file: {error}", file.display()))
    }

    pub fn answer(&self, question: &Question) -> Result<Answer, Unanswered> {
        match (&question.kind, question.role) {
            (Kind::File, Some(Role::Resume)) => return Ok(Answer::File(self.resume.clone())),
            (Kind::File, _) => return Err(Unanswered::NoRule),
            (Kind::Email, _) => return Ok(Answer::Text(self.email.clone())),
            (Kind::Phone, _) => return Ok(Answer::Text(self.phone.clone())),
            (Kind::Unsupported(kind), _) => return Err(Unanswered::Unsupported(kind.clone())),
            _ => {}
        }
        let stated = match self.rule_answer(&question.title)? {
            Some(stated) => stated,
            None => match (&question.kind, question.role) {
                (Kind::Text, Some(Role::Name)) if !asks_for_part_of_a_name(&question.title) => &self.name,
                (Kind::Text, Some(Role::FirstName)) => self.name_parts().0,
                (Kind::Text, Some(Role::LastName)) => self.name_parts().1.ok_or(Unanswered::NoRule)?,
                (Kind::Location, _) => self.location.as_deref().ok_or(Unanswered::NoRule)?,
                _ => return Err(Unanswered::NoRule),
            },
        };
        question.kind.answer_from(stated)
    }

    /// The first word of the name, and the rest of it as the last name. A name of one word has no last name.
    fn name_parts(&self) -> (&str, Option<&str>) {
        match self.name.trim().split_once(char::is_whitespace) {
            Some((first, last)) => (first, Some(last.trim_start())),
            None => (self.name.trim(), None),
        }
    }

    /// The answer every matching rule agrees on, if any rule matches.
    fn rule_answer(&self, title: &str) -> Result<Option<&str>, Unanswered> {
        let title = normalized(title);
        let holds = |phrases: &[String]| phrases.iter().any(|phrase| title.contains(&normalized(phrase)));
        let mut matching = self
            .rules
            .iter()
            .filter(|rule| holds(&rule.any) && !holds(&rule.not))
            .map(|rule| rule.answer.as_str());
        let Some(first) = matching.next() else {
            return Ok(None);
        };
        if matching.all(|answer| answer == first) {
            Ok(Some(first))
        } else {
            Err(Unanswered::RulesDisagree)
        }
    }
}

fn option(options: &[String], stated: &str) -> Result<String, Unanswered> {
    options
        .iter()
        .find(|option| normalized(option) == normalized(stated))
        .cloned()
        .ok_or_else(|| Unanswered::NoSuchOption(stated.to_owned()))
}

/// True for a title such as "Legal first name", where the full name would be a wrong answer.
fn asks_for_part_of_a_name(title: &str) -> bool {
    normalized(title)
        .split(|c: char| !c.is_alphanumeric())
        .any(|word| matches!(word, "first" | "last" | "middle" | "given" | "family"))
}

/// Lower case, with each run of white space as one space.
fn normalized(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answers() -> Answers {
        serde_json::from_str(
            r#"{
                "name": "Ada Example",
                "email": "ada@example.com",
                "phone": "+1 555 0100",
                "resume": "/tmp/ada.pdf",
                "location": "Springfield, Illinois",
                "rules": [
                    { "any": ["linkedin"], "answer": "https://linkedin.example/ada" },
                    { "any": ["sponsor"], "not": ["without"], "answer": "yes" },
                    { "any": ["authorized to work"], "not": ["sponsor"], "answer": "yes" },
                    { "any": ["start date"], "answer": "2027-01-18" },
                    { "any": ["how did you hear"], "answer": "linkedin" },
                    { "any": ["website"], "answer": "https://ada.example" },
                    { "any": ["legal name"], "answer": "Ada Byron Example" }
                ]
            }"#,
        )
        .unwrap()
    }

    fn ask(title: &str, kind: Kind) -> Result<Answer, Unanswered> {
        answers().answer(&Question {
            title: title.into(),
            kind,
            role: None,
        })
    }

    fn options(labels: &[&str]) -> Vec<String> {
        labels.iter().map(|&label| label.to_owned()).collect()
    }

    #[test]
    fn the_kind_of_field_decides_contact_details_and_the_resume() {
        assert_eq!(ask("Email", Kind::Email), Ok(Answer::Text("ada@example.com".into())));
        assert_eq!(ask("Mobile", Kind::Phone), Ok(Answer::Text("+1 555 0100".into())));
        let resume = Question {
            title: "Resume".into(),
            kind: Kind::File,
            role: Some(Role::Resume),
        };
        assert_eq!(answers().answer(&resume), Ok(Answer::File("/tmp/ada.pdf".into())));
        assert_eq!(ask("Cover letter", Kind::File), Err(Unanswered::NoRule));
    }

    #[test]
    fn the_name_field_takes_the_name_unless_a_rule_covers_its_title() {
        let name = |title: &str| {
            answers().answer(&Question {
                title: title.into(),
                kind: Kind::Text,
                role: Some(Role::Name),
            })
        };
        assert_eq!(name("Name"), Ok(Answer::Text("Ada Example".into())));
        assert_eq!(name("Legal Name"), Ok(Answer::Text("Ada Byron Example".into())));
        assert_eq!(name("Legal First Name"), Err(Unanswered::NoRule));
    }

    #[test]
    fn a_form_with_two_name_fields_gets_the_name_in_two_parts() {
        let part = |answers: &Answers, title: &str, role| {
            answers.answer(&Question {
                title: title.into(),
                kind: Kind::Text,
                role: Some(role),
            })
        };
        let mut answers = answers();
        assert_eq!(
            part(&answers, "First Name", Role::FirstName),
            Ok(Answer::Text("Ada".into()))
        );
        assert_eq!(
            part(&answers, "Last Name", Role::LastName),
            Ok(Answer::Text("Example".into()))
        );
        // A rule that covers the title wins, as it does for every other question.
        assert_eq!(
            part(&answers, "Legal Name", Role::FirstName),
            Ok(Answer::Text("Ada Byron Example".into()))
        );

        answers.name = "Ada  Byron Example".into();
        assert_eq!(
            part(&answers, "First Name", Role::FirstName),
            Ok(Answer::Text("Ada".into()))
        );
        assert_eq!(
            part(&answers, "Last Name", Role::LastName),
            Ok(Answer::Text("Byron Example".into()))
        );

        answers.name = "Ada".into();
        assert_eq!(
            part(&answers, "First Name", Role::FirstName),
            Ok(Answer::Text("Ada".into()))
        );
        assert_eq!(part(&answers, "Last Name", Role::LastName), Err(Unanswered::NoRule));
    }

    #[test]
    fn a_rule_answers_in_the_form_the_field_needs() {
        assert_eq!(
            ask("LinkedIn  Profile URL", Kind::Text),
            Ok(Answer::Text("https://linkedin.example/ada".into()))
        );
        assert_eq!(
            ask("Will you now or in the future require sponsorship?", Kind::YesNo),
            Ok(Answer::YesNo(true))
        );
        assert_eq!(
            ask("What is your earliest start date?", Kind::Date),
            Ok(Answer::Date(jiff::civil::date(2027, 1, 18)))
        );
        assert_eq!(
            ask("Earliest start date", Kind::Text),
            Ok(Answer::Text("January 18, 2027".into()))
        );
        let hear = options(&["Company website", "LinkedIn", "Other"]);
        assert_eq!(
            ask("How did you hear about us?", Kind::Choice { options: hear.clone() }),
            Ok(Answer::Choice("LinkedIn".into()))
        );
        assert_eq!(
            ask("How did you hear about us?", Kind::Choices { options: hear }),
            Ok(Answer::Choices(vec!["LinkedIn".into()]))
        );
    }

    #[test]
    fn a_question_no_rule_covers_gets_no_answer() {
        assert_eq!(
            ask("What is your desired compensation?", Kind::Text),
            Err(Unanswered::NoRule)
        );
        assert_eq!(ask("Why us?", Kind::LongText), Err(Unanswered::NoRule));
        // The `not` phrase keeps the sponsorship rule away from this one.
        assert_eq!(
            ask("Can you work in the US without sponsorship?", Kind::YesNo),
            Err(Unanswered::NoRule)
        );
    }

    #[test]
    fn rules_that_disagree_give_no_answer() {
        assert_eq!(ask("Website or LinkedIn", Kind::Text), Err(Unanswered::RulesDisagree));
    }

    #[test]
    fn an_answer_that_does_not_fit_the_field_gives_no_answer() {
        assert_eq!(
            ask("LinkedIn member?", Kind::YesNo),
            Err(Unanswered::NotYesOrNo("https://linkedin.example/ada".into()))
        );
        assert_eq!(
            ask("LinkedIn join date", Kind::Date),
            Err(Unanswered::NotADate("https://linkedin.example/ada".into()))
        );
        let statuses = options(&["I hold a visa and need sponsorship later", "I am a citizen"]);
        assert_eq!(
            ask(
                "Are you authorized to work in the US?",
                Kind::Choice { options: statuses }
            ),
            Err(Unanswered::NoSuchOption("yes".into()))
        );
        assert_eq!(
            ask("Signature", Kind::Unsupported("Signature".into())),
            Err(Unanswered::Unsupported("Signature".into()))
        );
    }

    #[test]
    fn a_location_field_takes_the_location_and_stays_empty_without_one() {
        assert_eq!(
            ask("Where are you located?", Kind::Location),
            Ok(Answer::Text("Springfield, Illinois".into()))
        );
        let mut answers = answers();
        answers.location = None;
        let question = Question {
            title: "Where are you located?".into(),
            kind: Kind::Location,
            role: None,
        };
        assert_eq!(answers.answer(&question), Err(Unanswered::NoRule));
    }

    #[test]
    fn a_misspelled_key_in_the_file_is_an_error() {
        let file = r#"{ "name": "A", "email": "a@example.com", "phone": "1", "resume": "/r.pdf", "rule": [] }"#;
        assert!(serde_json::from_str::<Answers>(file).is_err());
    }
}
