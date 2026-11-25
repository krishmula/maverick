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
    /// The name the user goes by, such as `Ada Example`.
    pub name: String,
    /// The name on the user's papers, when it differs from `name`. A question that asks for the
    /// legal name takes it from here.
    #[serde(default)]
    pub legal_name: Option<LegalName>,
    pub email: String,
    pub phone: String,
    pub resume: PathBuf,
    /// The user's school transcript. A form that asks for a transcript file takes this one.
    #[serde(default)]
    pub transcript: Option<PathBuf>,
    /// As the user would type it into a location search, such as `Springfield, Illinois`.
    #[serde(default)]
    pub location: Option<String>,
    /// Where the user gets mail. A form that asks for the address in parts takes them from here.
    #[serde(default)]
    pub address: Option<Address>,
    /// The jobs the user has held, the newest first. A form that asks for each job on its own
    /// takes them from here, in this order.
    #[serde(default)]
    pub work: Vec<Work>,
    /// The schools the user went to, the newest first.
    #[serde(default)]
    pub education: Vec<School>,
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
    /// What the user chooses to tell an employer about gender, ethnicity, veteran status and
    /// disability, one statement per entry. A form's voluntary questions are answered from these
    /// and from the facts. With no entry, those questions are left for the user.
    #[serde(default)]
    pub disclosures: Vec<String>,
    /// The jobs the user is looking for, one statement per entry, such as levels and kinds of work.
    /// Scoring judges each posting against these and the background.
    #[serde(default)]
    pub target: Vec<String>,
    /// The answers the user gave once for every form.
    #[serde(default)]
    pub standing: Standing,
}

/// The name on the user's papers.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LegalName {
    pub first: String,
    pub last: String,
}

/// Answers the user gave once for every form, to questions whose wording differs on each form.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Standing {
    /// True when the user agrees to every form's terms, privacy notice and statement that the
    /// application is true. False leaves those questions for the user.
    #[serde(default)]
    pub agrees_to_terms: bool,
    /// How a question about expected pay is answered. With none, it is left for the user.
    #[serde(default)]
    pub pay: Option<Pay>,
    /// The yearly pay a required question gets when the posting states no range. An optional
    /// question stays empty. With none, the question is left for the user.
    #[serde(default)]
    pub pay_without_a_range: Option<u32>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum Pay {
    /// The middle of the pay range the posting states. A posting without a range gets no answer.
    PostedMidpoint,
}

/// A postal address in the parts forms ask for.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Address {
    pub street: String,
    pub city: String,
    /// The state or province, written out, such as `Illinois`.
    pub region: String,
    pub postal_code: String,
    /// The country as forms list it, such as `United States of America`.
    pub country: String,
}

/// One job the user has held.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Work {
    pub title: String,
    pub company: String,
    pub location: String,
    /// The first month, as `YYYY-MM`.
    pub from: String,
    /// The last month, as `YYYY-MM`. Left out for the job the user holds now.
    #[serde(default)]
    pub to: Option<String>,
    #[serde(default)]
    pub description: String,
}

/// One school the user went to.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct School {
    pub school: String,
    /// The degree as the user writes it, such as `Master of Science`.
    pub degree: String,
    pub field: String,
    /// The first and the last year, as `YYYY`.
    #[serde(default)]
    pub from: Option<String>,
    #[serde(default)]
    pub to: Option<String>,
    #[serde(default)]
    pub grade: Option<String>,
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
    /// Pick one option from a list that shows its options only after a search for some text.
    Search,
    /// A kind of field this code does not fill.
    Unsupported(String),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Role {
    Name,
    /// A form that asks for the name in two fields marks each of them.
    FirstName,
    /// Only a rule answers this. Most users have no middle name to give.
    MiddleName,
    LastName,
    /// The first and the last name on the user's papers, where the form says so without words.
    LegalFirstName,
    LegalLastName,
    /// Whether the user goes by another name than the one on their papers.
    HasPreferredName,
    Resume,
    Street,
    City,
    Region,
    PostalCode,
    Country,
    /// Which kind of phone the number belongs to, such as mobile.
    PhoneKind,
    /// The country code of the phone number, in a field of its own.
    PhoneCode,
    /// The day the form is filled, where the user signs a statement.
    Today,
    /// A part of the job at this place in the user's list of jobs.
    Work(usize, WorkPart),
    /// A part of the school at this place in the user's list of schools.
    School(usize, SchoolPart),
}

impl Role {
    /// False for a field whose answer is the user's own record and nothing else, such as the
    /// title of the second job. A model would make one up from the user's facts.
    pub fn takes_a_model_answer(self) -> bool {
        !matches!(
            self,
            Self::MiddleName
                | Self::HasPreferredName
                | Self::PhoneKind
                | Self::PhoneCode
                | Self::Today
                | Self::Work(..)
                | Self::School(..)
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum WorkPart {
    Title,
    Company,
    Location,
    /// Whether the user holds this job now.
    Current,
    From,
    To,
    Description,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SchoolPart {
    Name,
    Degree,
    Field,
    Grade,
    From,
    To,
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
    /// The form held an answer before the fill, and the standard answers have none to replace it.
    Held,
    /// Another answer makes this field go away, as the last month of a job the user holds now.
    NotNeeded,
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
            Self::Held => f.write_str("the form already held an answer, and Maverick kept it"),
            Self::NotNeeded => f.write_str("another answer on this page makes this one unnecessary"),
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
            Self::Search => Ok(Answer::Text(stated.to_owned())),
            Self::Location => Ok(Answer::Text(as_searched(stated))),
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
            (Kind::File, _) => {
                let asks_for_transcript = question.title.to_lowercase().contains("transcript");
                let transcript = self.transcript.clone().filter(|_| asks_for_transcript);
                return transcript.map(Answer::File).ok_or(Unanswered::NoRule);
            }
            (Kind::Email, _) => return Ok(Answer::Text(self.email.clone())),
            (Kind::Phone, _) => return Ok(Answer::Text(self.phone.clone())),
            (Kind::Unsupported(kind), _) => return Err(Unanswered::Unsupported(kind.clone())),
            // A rule about the phone number must not answer these two.
            (kind, Some(Role::PhoneKind)) => return kind.answer_from("Mobile"),
            (_, Some(Role::PhoneCode)) => return Err(Unanswered::NoRule),
            (kind, Some(Role::Today)) => return kind.answer_from(&today()),
            (kind, Some(Role::HasPreferredName)) => {
                return kind.answer_from(if self.goes_by_another_name() { "yes" } else { "no" });
            }
            (kind, Some(Role::Work(place, part))) => {
                let job = self.work.get(place).ok_or(Unanswered::NoRule)?;
                return job.answer(part, kind);
            }
            (kind, Some(Role::School(place, part))) => {
                let school = self.education.get(place).ok_or(Unanswered::NoRule)?;
                return school.answer(part, kind);
            }
            (kind, Some(role)) => {
                if let Some(part) = self.address.as_ref().and_then(|address| address.part(role)) {
                    return kind.answer_from(part);
                }
            }
            _ => {}
        }
        let (now, named);
        let stated = match self.rule_answer(&question.title)? {
            Some(stated) => stated,
            None => match (&question.kind, question.role) {
                // A form asks for today's date where the applicant signs a statement.
                (Kind::Date, _)
                    if normalized(&question.title)
                        .replace('\u{2019}', "'")
                        .contains("today's date") =>
                {
                    now = today();
                    &now
                }
                (Kind::Text, role) => match self.name_asked(&question.title, role) {
                    Some(name) => {
                        named = name;
                        &named
                    }
                    None => return Err(Unanswered::NoRule),
                },
                (Kind::Location, _) => self.location.as_deref().ok_or(Unanswered::NoRule)?,
                _ => return Err(Unanswered::NoRule),
            },
        };
        question.kind.answer_from(stated)
    }

    /// The name, or the part of it, that a question asks for. `None` for a question about
    /// anything else, and for one about another person's name.
    ///
    /// A question that says "legal", and a field the form marks as legal, takes the name on the
    /// user's papers. Every other question takes the name the user goes by.
    fn name_asked(&self, title: &str, role: Option<Role>) -> Option<String> {
        let title = normalized(title);
        let words: Vec<&str> = title
            .split(|c: char| !c.is_alphanumeric())
            .filter(|word| !word.is_empty())
            .collect();
        let has = |any: &[&str]| words.iter().any(|word| any.contains(word));
        // Without a role, only a question made of these words is about the user's own name.
        // "Reference's first name" is not.
        if role.is_none() && !(has(&["name"]) && words.iter().all(|word| NAME_WORDS.contains(word))) {
            return None;
        }
        if has(&["middle"]) {
            return None;
        }
        let legal_role = matches!(role, Some(Role::LegalFirstName | Role::LegalLastName));
        let legal = (legal_role || has(&["legal"])) && !has(&["preferred"]);
        let (first, last) = match (&self.legal_name, legal) {
            (Some(papers), true) => (papers.first.trim(), Some(papers.last.trim())),
            _ => self.name_parts(),
        };
        match role {
            Some(Role::FirstName | Role::LegalFirstName) => Some(first.to_owned()),
            Some(Role::LastName | Role::LegalLastName) => last.map(str::to_owned),
            Some(Role::Name) | None => {
                if has(&["first", "given"]) {
                    Some(first.to_owned())
                } else if has(&["last", "family", "surname"]) {
                    last.map(str::to_owned)
                } else {
                    Some(last.map_or_else(|| first.to_owned(), |last| format!("{first} {last}")))
                }
            }
            Some(_) => None,
        }
    }

    /// True when the name on the user's papers is not the name the user goes by.
    fn goes_by_another_name(&self) -> bool {
        self.legal_name.as_ref().is_some_and(|papers| {
            let (first, last) = self.name_parts();
            papers.first.trim() != first || Some(papers.last.trim()) != last
        })
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

/// Today's date where the user is, as `YYYY-MM-DD`.
fn today() -> String {
    jiff::Timestamp::now()
        .to_zoned_datetime(jiff::tz::TimeZone::system())
        .date()
        .to_string()
}

/// A month written `YYYY-MM`, or a year written `YYYY`, as an answer to a date field.
fn month(kind: &Kind, stated: &str) -> Result<Answer, Unanswered> {
    let day = match stated.trim().len() {
        4 => format!("{}-01-01", stated.trim()),
        7 => format!("{}-01", stated.trim()),
        _ => stated.trim().to_owned(),
    };
    match kind {
        Kind::Date => kind.answer_from(&day),
        // A form that takes the month as text gets it as the user wrote it.
        _ => kind.answer_from(stated),
    }
}

impl Work {
    fn answer(&self, part: WorkPart, kind: &Kind) -> Result<Answer, Unanswered> {
        match part {
            WorkPart::Title => kind.answer_from(&self.title),
            WorkPart::Company => kind.answer_from(&self.company),
            WorkPart::Location => kind.answer_from(&self.location),
            WorkPart::Current => kind.answer_from(if self.to.is_none() { "yes" } else { "no" }),
            WorkPart::From => month(kind, &self.from),
            WorkPart::To => month(kind, self.to.as_deref().ok_or(Unanswered::NotNeeded)?),
            WorkPart::Description if self.description.is_empty() => Err(Unanswered::NoRule),
            WorkPart::Description => kind.answer_from(&self.description),
        }
    }
}

impl School {
    fn answer(&self, part: SchoolPart, kind: &Kind) -> Result<Answer, Unanswered> {
        fn stated(text: &Option<String>) -> Result<&str, Unanswered> {
            text.as_deref().ok_or(Unanswered::NoRule)
        }
        match (part, kind) {
            (SchoolPart::Name, _) => kind.answer_from(&self.school),
            (SchoolPart::Degree, Kind::Choice { options }) => degree(options, &self.degree).map(Answer::Choice),
            (SchoolPart::Degree, _) => kind.answer_from(&self.degree),
            (SchoolPart::Field, _) => kind.answer_from(&self.field),
            (SchoolPart::Grade, _) => kind.answer_from(stated(&self.grade)?),
            (SchoolPart::From, _) => month(kind, stated(&self.from)?),
            (SchoolPart::To, _) => month(kind, stated(&self.to)?),
        }
    }
}

/// How high a degree is, by the words every spelling of it shares. Forms list degrees in their
/// own words, such as `MS`, `Masters` or `Master's Degree` for a `Master of Science`.
const DEGREE_LEVELS: [&[&str]; 5] = [
    &["phd", "ph.d", "ph.d.", "doctor", "doctorate", "doctoral"],
    &[
        "master", "masters", "master's", "ms", "m.s", "m.s.", "msc", "meng", "m.eng", "mba", "ma", "m.a.",
    ],
    &[
        "bachelor",
        "bachelors",
        "bachelor's",
        "bs",
        "b.s",
        "b.s.",
        "bsc",
        "ba",
        "b.a.",
        "btech",
        "b.tech",
        "beng",
        "b.e.",
    ],
    &["associate", "associates", "associate's"],
    &["high", "ged", "secondary"],
];

/// Words that say nothing about which degree an option names.
const DEGREE_FILLER: [&str; 10] = [
    "of",
    "or",
    "in",
    "and",
    "the",
    "a",
    "s",
    "degree",
    "equivalent",
    "level",
];

/// Words that name a level of degree and no one degree of that level.
const DEGREE_GENERAL: [&str; 12] = [
    "doctor",
    "doctorate",
    "doctoral",
    "master",
    "masters",
    "bachelor",
    "bachelors",
    "associate",
    "associates",
    "high",
    "secondary",
    "school",
];

/// The option that names the user's degree: the one spelled like it, or else the one option of
/// the same level. With several options of that level, the one that names most of the degree,
/// by its words or by its short form such as "MSc". When none does, the one that names no
/// other degree, such as "Master's Degree" next to "MBA".
fn degree(options: &[String], stated: &str) -> Result<String, Unanswered> {
    if let Ok(option) = option(options, stated) {
        return Ok(option);
    }
    let words = |text: &str| -> Vec<String> {
        normalized(text)
            .split(|c: char| !(c.is_alphanumeric() || c == '.'))
            .map(|word| word.replace('.', ""))
            .filter(|word| !word.is_empty() && !DEGREE_FILLER.contains(&word.as_str()))
            .collect()
    };
    let is_level = |level: &[&str], word: &str| level.iter().any(|name| name.replace(['.', '\''], "") == word);
    let level = |text: &str| {
        let words = words(text);
        DEGREE_LEVELS
            .iter()
            .position(|level| words.iter().any(|word| is_level(level, word)))
    };
    let none = || Unanswered::NoSuchOption(stated.to_owned());
    let level_wanted = level(stated).ok_or_else(none)?;
    let general = |word: &String| DEGREE_GENERAL.contains(&word.as_str());
    let stated_words = words(stated);
    let short: String = stated_words.iter().filter_map(|word| word.chars().next()).collect();
    let mut names: Vec<String> = stated_words.iter().filter(|word| !general(word)).cloned().collect();
    if stated_words.len() > 1 {
        if stated_words.last().is_some_and(|word| word.starts_with("science")) {
            names.push(format!("{short}c"));
        }
        names.push(short);
    }
    // More shared names first, then fewer names of another degree.
    let fit = |option: &&String| {
        let words = words(option);
        let shared = words.iter().filter(|word| names.contains(word)).count();
        let other = words
            .iter()
            .filter(|word| !names.contains(word) && !general(word))
            .count();
        (std::cmp::Reverse(shared), other)
    };
    let same_level: Vec<&String> = options
        .iter()
        .filter(|option| level(option) == Some(level_wanted))
        .collect();
    let best = same_level.iter().map(fit).min().ok_or_else(none)?;
    match same_level
        .iter()
        .filter(|option| fit(option) == best)
        .collect::<Vec<_>>()
        .as_slice()
    {
        [only] => Ok((**only).clone()),
        _ => Err(none()),
    }
}

impl Address {
    /// The part of the address a field with this role asks for.
    fn part(&self, role: Role) -> Option<&str> {
        match role {
            Role::Street => Some(&self.street),
            Role::City => Some(&self.city),
            Role::Region => Some(&self.region),
            Role::PostalCode => Some(&self.postal_code),
            Role::Country => Some(&self.country),
            _ => None,
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

/// The words of a question that asks for the user's own name and nothing else.
const NAME_WORDS: [&str; 27] = [
    "name",
    "names",
    "legal",
    "preferred",
    "first",
    "last",
    "given",
    "family",
    "surname",
    "full",
    "your",
    "my",
    "what",
    "is",
    "s",
    "please",
    "enter",
    "provide",
    "the",
    "a",
    "of",
    "if",
    "different",
    "from",
    "above",
    "applicable",
    "optional",
];

/// Lower case, with each run of white space as one space.
fn normalized(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

/// US states and their postal codes. Location searches list places by the state's name.
const STATES: [(&str, &str); 51] = [
    ("AL", "Alabama"),
    ("AK", "Alaska"),
    ("AZ", "Arizona"),
    ("AR", "Arkansas"),
    ("CA", "California"),
    ("CO", "Colorado"),
    ("CT", "Connecticut"),
    ("DE", "Delaware"),
    ("DC", "District of Columbia"),
    ("FL", "Florida"),
    ("GA", "Georgia"),
    ("HI", "Hawaii"),
    ("ID", "Idaho"),
    ("IL", "Illinois"),
    ("IN", "Indiana"),
    ("IA", "Iowa"),
    ("KS", "Kansas"),
    ("KY", "Kentucky"),
    ("LA", "Louisiana"),
    ("ME", "Maine"),
    ("MD", "Maryland"),
    ("MA", "Massachusetts"),
    ("MI", "Michigan"),
    ("MN", "Minnesota"),
    ("MS", "Mississippi"),
    ("MO", "Missouri"),
    ("MT", "Montana"),
    ("NE", "Nebraska"),
    ("NV", "Nevada"),
    ("NH", "New Hampshire"),
    ("NJ", "New Jersey"),
    ("NM", "New Mexico"),
    ("NY", "New York"),
    ("NC", "North Carolina"),
    ("ND", "North Dakota"),
    ("OH", "Ohio"),
    ("OK", "Oklahoma"),
    ("OR", "Oregon"),
    ("PA", "Pennsylvania"),
    ("RI", "Rhode Island"),
    ("SC", "South Carolina"),
    ("SD", "South Dakota"),
    ("TN", "Tennessee"),
    ("TX", "Texas"),
    ("UT", "Utah"),
    ("VT", "Vermont"),
    ("VA", "Virginia"),
    ("WA", "Washington"),
    ("WV", "West Virginia"),
    ("WI", "Wisconsin"),
    ("WY", "Wyoming"),
];

/// A place as location searches spell it. Ashby and Greenhouse list "Austin, Texas, United States",
/// so a state's postal code and a short name of the country are written out.
/// The city, the first part, stays as it is.
fn as_searched(place: &str) -> String {
    let mut parts = place.split(',').map(str::trim);
    let city = parts.next().unwrap_or_default().to_owned();
    let rest = parts.map(|part| {
        let upper = part.to_uppercase();
        match upper.as_str() {
            "US" | "USA" | "U.S." | "U.S.A." => "United States".to_owned(),
            _ => STATES
                .iter()
                .find(|(code, _)| *code == upper)
                .map_or_else(|| part.to_owned(), |(_, name)| (*name).to_owned()),
        }
    });
    std::iter::once(city).chain(rest).collect::<Vec<_>>().join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_form_that_asks_for_each_job_and_school_gets_them_in_order() {
        let answers: Answers = serde_json::from_str(
            r#"{
                "name": "Ada Example", "email": "ada@example.com", "phone": "+1 555 0100", "resume": "/tmp/ada.pdf",
                "rules": [{ "any": ["phone"], "answer": "+1 555 0100" }],
                "address": { "street": "1 Main Street", "city": "Springfield", "region": "Illinois",
                             "postal_code": "62701", "country": "United States of America" },
                "work": [
                    { "title": "Engineer", "company": "Example Robotics", "location": "Springfield", "from": "2023-06" },
                    { "title": "Intern", "company": "Example Labs", "location": "Chicago", "from": "2022-01", "to": "2022-08" }
                ],
                "education": [{ "school": "Example University", "degree": "Master of Science", "field": "Computer Science", "to": "2023" }]
            }"#,
        )
        .unwrap();
        let ask = |kind: Kind, role: Role| {
            answers.answer(&Question {
                title: "Phone or anything".into(),
                kind,
                role: Some(role),
            })
        };
        let text = |text: &str| Ok(Answer::Text(text.into()));
        let day = |year, month| Ok(Answer::Date(Date::new(year, month, 1).unwrap()));
        assert_eq!(ask(Kind::Text, Role::Work(0, WorkPart::Title)), text("Engineer"));
        assert_eq!(ask(Kind::Text, Role::Work(1, WorkPart::Company)), text("Example Labs"));
        assert_eq!(
            ask(Kind::YesNo, Role::Work(0, WorkPart::Current)),
            Ok(Answer::YesNo(true))
        );
        assert_eq!(
            ask(Kind::YesNo, Role::Work(1, WorkPart::Current)),
            Ok(Answer::YesNo(false))
        );
        assert_eq!(ask(Kind::Date, Role::Work(0, WorkPart::From)), day(2023, 6));
        assert_eq!(ask(Kind::Date, Role::Work(1, WorkPart::To)), day(2022, 8));
        // The job the user holds now has no last month, and the form drops that field.
        assert_eq!(ask(Kind::Date, Role::Work(0, WorkPart::To)), Err(Unanswered::NotNeeded));
        assert_eq!(ask(Kind::Text, Role::Work(2, WorkPart::Title)), Err(Unanswered::NoRule));
        assert_eq!(
            ask(Kind::Search, Role::School(0, SchoolPart::Field)),
            text("Computer Science")
        );
        assert_eq!(ask(Kind::Date, Role::School(0, SchoolPart::To)), day(2023, 1));
        assert_eq!(
            ask(Kind::Date, Role::School(0, SchoolPart::From)),
            Err(Unanswered::NoRule)
        );

        assert_eq!(ask(Kind::Text, Role::Street), text("1 Main Street"));
        assert_eq!(ask(Kind::Text, Role::PostalCode), text("62701"));
        // The rule about the phone number answers neither the kind of phone nor its country code.
        let kinds = Kind::Choice {
            options: vec!["Landline".into(), "Mobile".into()],
        };
        assert_eq!(ask(kinds, Role::PhoneKind), Ok(Answer::Choice("Mobile".into())));
        assert_eq!(ask(Kind::Search, Role::PhoneCode), Err(Unanswered::NoRule));
    }

    #[test]
    fn a_degree_is_found_under_the_forms_own_spelling() {
        let options = |list: &[&str]| list.iter().map(|option| (*option).to_owned()).collect::<Vec<_>>();
        let levels = options(&[
            "High School Diploma",
            "Bachelor's Degree",
            "Master's Degree",
            "Doctorate",
        ]);
        assert_eq!(degree(&levels, "Master of Science"), Ok("Master's Degree".into()));
        assert_eq!(degree(&levels, "PhD"), Ok("Doctorate".into()));
        let short = options(&["BA", "BS", "MA", "MS", "MBA"]);
        assert_eq!(degree(&short, "MS"), Ok("MS".into()));
        assert_eq!(degree(&short, "Master of Science"), Ok("MS".into()));
        // Two options are a bachelor's degree, and neither names this one.
        assert_eq!(
            degree(&short, "Bachelor of Technology"),
            Err(Unanswered::NoSuchOption("Bachelor of Technology".into()))
        );
        let grouped = options(&[
            "Bachelor’s or Equivalent First-Degree (ISCED 6: BA, BSc, BEng, LLB, etc.)",
            "Master of Business Administration - MBA ( (ISCED 7)",
            "Master’s or Equivalent Second-Degree (ISCED 7: MA, MSc, MEng, MPhil, professional licentiate)",
        ]);
        assert_eq!(degree(&grouped, "Master of Science"), Ok(grouped[2].clone()));
        assert_eq!(degree(&grouped, "Bachelor of Science"), Ok(grouped[0].clone()));
        let general = options(&["Bachelor's Degree", "Master's Degree", "MBA"]);
        assert_eq!(degree(&general, "Master of Science"), Ok("Master's Degree".into()));
        let named = options(&["Master of Arts", "Master of Science", "Bachelor of Science"]);
        assert_eq!(degree(&named, "master of science"), Ok("Master of Science".into()));
        assert_eq!(
            degree(&named, "Certificate"),
            Err(Unanswered::NoSuchOption("Certificate".into()))
        );
    }

    #[test]
    fn a_request_for_todays_date_gets_it() {
        let asked = Question {
            title: "Please acknowledge this by entering today\u{2019}s date.".into(),
            kind: Kind::Date,
            role: None,
        };
        let today = jiff::Timestamp::now()
            .to_zoned_datetime(jiff::tz::TimeZone::system())
            .date();
        assert_eq!(answers().answer(&asked), Ok(Answer::Date(today)));
        let other = Question {
            title: "Date of birth".into(),
            ..asked
        };
        assert_eq!(answers().answer(&other), Err(Unanswered::NoRule));
    }

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
    fn a_file_question_about_a_transcript_takes_the_saved_transcript() {
        assert_eq!(ask("Unofficial transcript", Kind::File), Err(Unanswered::NoRule));
        let answers = Answers {
            transcript: Some("/tmp/ada-transcript.pdf".into()),
            ..answers()
        };
        let ask = |title: &str| {
            answers.answer(&Question {
                title: title.into(),
                kind: Kind::File,
                role: None,
            })
        };
        assert_eq!(
            ask("Unofficial Transcript"),
            Ok(Answer::File("/tmp/ada-transcript.pdf".into()))
        );
        assert_eq!(ask("Cover letter"), Err(Unanswered::NoRule));
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
        assert_eq!(name("Legal First Name"), Ok(Answer::Text("Ada".into())));
    }

    #[test]
    fn a_question_about_the_legal_name_takes_the_name_on_the_users_papers() {
        let mut answers = answers();
        answers.rules.clear();
        let ask = |answers: &Answers, title: &str, kind: Kind, role: Option<Role>| {
            answers.answer(&Question {
                title: title.into(),
                kind,
                role,
            })
        };
        let text = |answers: &Answers, title: &str, role| ask(answers, title, Kind::Text, role);
        let said = |name: &str| Ok(Answer::Text(name.into()));
        // Without a legal name on file, the user has one name.
        assert_eq!(text(&answers, "Legal first name", None), said("Ada"));
        assert_eq!(
            ask(
                &answers,
                "I have a preferred name",
                Kind::YesNo,
                Some(Role::HasPreferredName)
            ),
            Ok(Answer::YesNo(false))
        );

        answers.legal_name = Some(LegalName {
            first: "Augusta Ada".into(),
            last: "Example".into(),
        });
        assert_eq!(
            text(&answers, "First Name", Some(Role::LegalFirstName)),
            said("Augusta Ada")
        );
        assert_eq!(text(&answers, "Last Name", Some(Role::LegalLastName)), said("Example"));
        assert_eq!(
            text(&answers, "Legal First Name", Some(Role::FirstName)),
            said("Augusta Ada")
        );
        assert_eq!(text(&answers, "Full legal name", None), said("Augusta Ada Example"));
        assert_eq!(
            text(&answers, "Legal Name", Some(Role::Name)),
            said("Augusta Ada Example")
        );
        // Everything else takes the name the user goes by.
        assert_eq!(text(&answers, "First Name", Some(Role::FirstName)), said("Ada"));
        assert_eq!(text(&answers, "Name", Some(Role::Name)), said("Ada Example"));
        assert_eq!(text(&answers, "Preferred First Name", None), said("Ada"));
        assert_eq!(text(&answers, "Preferred last name", None), said("Example"));
        assert_eq!(
            text(&answers, "Preferred name (if different from legal name)", None),
            said("Ada Example")
        );
        assert_eq!(
            ask(
                &answers,
                "I have a preferred name",
                Kind::YesNo,
                Some(Role::HasPreferredName)
            ),
            Ok(Answer::YesNo(true))
        );
        // Another person's name, and a middle name, are not the user's name.
        for title in [
            "Reference's first name",
            "Name of your manager",
            "Legal middle name",
            "School name",
        ] {
            assert_eq!(text(&answers, title, None), Err(Unanswered::NoRule), "{title}");
        }
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
    fn a_location_is_spelled_the_way_location_searches_list_places() {
        let location = |stated: &str| Kind::Location.answer_from(stated);
        assert_eq!(location("Austin, TX"), Ok(Answer::Text("Austin, Texas".into())));
        assert_eq!(
            location(" springfield , il, USA "),
            Ok(Answer::Text("springfield, Illinois, United States".into()))
        );
        assert_eq!(
            location("Springfield, Illinois"),
            Ok(Answer::Text("Springfield, Illinois".into()))
        );
        // A city that looks like a code, and a part that is not a state, stay as written.
        assert_eq!(location("Ca, Spain"), Ok(Answer::Text("Ca, Spain".into())));
        assert_eq!(location("Toronto, ON"), Ok(Answer::Text("Toronto, ON".into())));
        assert_eq!(location("Remote"), Ok(Answer::Text("Remote".into())));
    }

    #[test]
    fn a_misspelled_key_in_the_file_is_an_error() {
        let file = r#"{ "name": "A", "email": "a@example.com", "phone": "1", "resume": "/r.pdf", "rule": [] }"#;
        assert!(serde_json::from_str::<Answers>(file).is_err());
    }
}
