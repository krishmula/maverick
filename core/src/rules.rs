//! Rules that judge a posting from its row alone, with no network call and no model.
//!
//! These rules skip only when the title leaves no doubt. A title they cannot place
//! is kept, and the scoring step judges it with the full posting text.

use std::collections::HashMap;
use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Intern,
    Staff,
    Principal,
    Director,
    Manager,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkipReason {
    Level(Level),
    JavaCentric,
    /// The title names a job that is not software engineering, such as "data scientist".
    OtherRole(&'static str),
    OutsideUs,
}

impl fmt::Display for SkipReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Level(Level::Intern) => f.write_str("internship"),
            Self::Level(Level::Staff) => f.write_str("staff-level role"),
            Self::Level(Level::Principal) => f.write_str("principal-level role"),
            Self::Level(Level::Director) => f.write_str("director-level role"),
            Self::Level(Level::Manager) => f.write_str("manager-level role"),
            Self::JavaCentric => f.write_str("Java-centric role"),
            Self::OtherRole(role) => write!(f, "not software engineering ({role})"),
            Self::OutsideUs => f.write_str("no location in the United States"),
        }
    }
}

const LEVEL_WORDS: &[(&str, Level)] = &[
    ("intern", Level::Intern),
    ("interns", Level::Intern),
    ("internship", Level::Intern),
    ("co-op", Level::Intern),
    ("coop", Level::Intern),
    ("staff", Level::Staff),
    ("stf", Level::Staff),
    ("principal", Level::Principal),
    ("director", Level::Director),
    ("manager", Level::Manager),
    ("mgr", Level::Manager),
];

/// A title with one of these is a software engineering job, whatever else it says.
const SOFTWARE_PHRASES: &[&str] = &[
    "software engineer",
    "software developer",
    "software development engineer",
    "software dev engineer",
    "programmer",
    "sde",
    "swe",
];

/// Jobs that are not software engineering. A phrase is here only when it names the job without doubt.
/// "Solutions engineer", "security engineer" and "architect" are left out on purpose:
/// some of those postings are software jobs, so the scoring step decides them.
const OTHER_ROLES: &[&str] = &[
    "scientist",
    "analyst",
    "administrator",
    "technician",
    "technical writer",
    "postdoctoral",
    "postdoc",
    "consultant",
    "recruiter",
    "sales",
    "tester",
    "operator",
    "assistant",
    "accountant",
    "attorney",
    "support engineer",
    "technical support",
    "application support",
    "test engineer",
    "design engineer",
    "controls engineer",
    "validation engineer",
    "field engineer",
    "hardware engineer",
    "electrical engineer",
    "mechanical engineer",
    "civil engineer",
    "manufacturing engineer",
    "network engineer",
    "talent pool",
];

/// Decides from the title and location whether a posting can be skipped without reading it.
pub fn prefilter(title: &str, location: Option<&str>) -> Option<SkipReason> {
    let words = words(title);
    let software = SOFTWARE_PHRASES.iter().any(|phrase| has_phrase(&words, phrase));

    if words.iter().any(|word| word == "java" || word == "j2ee") {
        return Some(SkipReason::JavaCentric);
    }
    if let Some(level) = level(&words, software) {
        return Some(SkipReason::Level(level));
    }
    if !software && let Some(role) = OTHER_ROLES.iter().find(|role| has_phrase(&words, role)) {
        return Some(SkipReason::OtherRole(role));
    }
    if location.is_some_and(outside_us) {
        return Some(SkipReason::OutsideUs);
    }
    None
}

fn level(words: &[String], software: bool) -> Option<Level> {
    // "Member of Technical Staff" is an engineer title, not the staff level.
    let technical_staff = has_phrase(words, "technical staff");
    words.iter().find_map(|word| {
        let level = LEVEL_WORDS
            .iter()
            .find(|(name, _)| word.trim_end_matches('+') == *name)?
            .1;
        match level {
            Level::Staff if technical_staff => None,
            // In a software title these words usually name a team or a product,
            // as in "Software Engineer II, Device Manager".
            Level::Manager | Level::Director if software => None,
            level => Some(level),
        }
    })
}

/// True only when the location names places and none of them is in the United States.
fn outside_us(location: &str) -> bool {
    // A long list ends in "...", so the places that were cut off are unknown.
    if location.contains("...") || location.contains('…') {
        return false;
    }
    let lower = location.to_lowercase();
    let in_us = ["united states", "usa", "u.s."].iter().any(|name| lower.contains(name))
        || lower.split(|c: char| !c.is_ascii_alphabetic()).any(|word| word == "us");
    !in_us && !lower.contains("remote") && lower.chars().any(char::is_alphabetic)
}

/// Lowercase words. Letters, digits, `+` and `#` stay in a word, and so does a hyphen inside one.
fn words(text: &str) -> Vec<String> {
    let lower = text.to_lowercase();
    let mut words = Vec::new();
    let mut word = String::new();
    let mut chars = lower.chars().peekable();
    while let Some(c) = chars.next() {
        let inner_hyphen = c == '-' && !word.is_empty() && chars.peek().is_some_and(|next| next.is_alphanumeric());
        if c.is_alphanumeric() || c == '+' || c == '#' || inner_hyphen {
            word.push(c);
        } else if !word.is_empty() {
            words.push(std::mem::take(&mut word));
        }
    }
    if !word.is_empty() {
        words.push(word);
    }
    words
}

fn has_phrase(words: &[String], phrase: &str) -> bool {
    let wanted: Vec<&str> = phrase.split(' ').collect();
    words
        .windows(wanted.len())
        .any(|window| window.iter().zip(&wanted).all(|(word, wanted)| word == wanted))
}

/// The jobs seen so far, for spotting a posting that repeats one of them.
///
/// Two postings are the same job when the company and title match, whatever the case
/// and punctuation, and with "Sr" equal to "Senior". A requisition code such as
/// `R123456` is set apart from the title: two different codes mean two different jobs,
/// and a posting with no code repeats any job with the same title.
#[derive(Default)]
pub struct SeenJobs {
    codes_by_title: HashMap<String, Vec<Option<String>>>,
}

impl SeenJobs {
    /// Records the job and says whether it repeats one recorded earlier.
    pub fn insert(&mut self, company: &str, title: &str) -> bool {
        let (key, code) = job_key(company, title);
        let codes = self.codes_by_title.entry(key).or_default();
        let repeat = !codes.is_empty() && (code.is_none() || codes.iter().any(|seen| seen.is_none() || *seen == code));
        codes.push(code);
        repeat
    }
}

/// The normalized company and title, and the requisition code if the title has one.
fn job_key(company: &str, title: &str) -> (String, Option<String>) {
    let mut key: Vec<&str> = Vec::new();
    let mut code: Vec<&str> = Vec::new();
    let words: Vec<String> = words(company).into_iter().chain(words(title)).collect();
    for word in &words {
        let word = word.trim_matches(|c| c == '+' || c == '#');
        let digits = word.chars().filter(char::is_ascii_digit).count();
        let year = digits == 4 && word.len() == 4 && (word.starts_with("19") || word.starts_with("20"));
        match word {
            "" => {}
            code_word if digits >= 4 && !year => code.push(code_word),
            "sr" => key.push("senior"),
            "jr" => key.push("junior"),
            other => key.push(other),
        }
    }
    (key.join(" "), (!code.is_empty()).then(|| code.join(" ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skip(title: &str) -> Option<SkipReason> {
        prefilter(title, Some("Austin, Texas, United States"))
    }

    #[test]
    fn software_titles_at_the_target_levels_are_kept() {
        for title in [
            "Software Engineer",
            "Software Engineer II, Backend",
            "Senior Software Engineer",
            "Software Development Engineer, New Grad 2027",
            "Member of Technical Staff",
            "Machine Learning Engineer",
            "Site Reliability Engineer",
            "Full Stack Developer (JavaScript, TypeScript)",
            "Software Engineer II, Device Manager",
            "Software Engineer, Director Services Platform",
            "Software Engineer, Internal Tools",
            "Programmer Analyst",
            "Solutions Engineer",
            "Application Security Engineer",
        ] {
            assert_eq!(skip(title), None, "{title}");
        }
    }

    #[test]
    fn a_level_outside_the_target_is_skipped() {
        assert_eq!(skip("Staff Software Engineer"), Some(SkipReason::Level(Level::Staff)));
        assert_eq!(skip("Software Engineer Stf"), Some(SkipReason::Level(Level::Staff)));
        assert_eq!(skip("Staff+ Software Engineer"), Some(SkipReason::Level(Level::Staff)));
        assert_eq!(
            skip("Principal Engineer, Storage"),
            Some(SkipReason::Level(Level::Principal))
        );
        assert_eq!(
            skip("Engineering Manager, Payments"),
            Some(SkipReason::Level(Level::Manager))
        );
        assert_eq!(skip("AI Engineering Mgr"), Some(SkipReason::Level(Level::Manager)));
        assert_eq!(
            skip("Director of Engineering"),
            Some(SkipReason::Level(Level::Director))
        );
        assert_eq!(
            skip("Software Engineer Intern, Summer 2027"),
            Some(SkipReason::Level(Level::Intern))
        );
        assert_eq!(
            skip("Software Engineering Co-op"),
            Some(SkipReason::Level(Level::Intern))
        );
    }

    #[test]
    fn java_in_the_title_is_skipped_and_javascript_is_not() {
        assert_eq!(skip("Java Developer"), Some(SkipReason::JavaCentric));
        assert_eq!(
            skip("Software Engineer (Java/Spring Boot)"),
            Some(SkipReason::JavaCentric)
        );
        assert_eq!(skip("J2EE Engineer"), Some(SkipReason::JavaCentric));
        assert_eq!(skip("JavaScript Engineer"), None);
    }

    #[test]
    fn a_title_that_names_another_job_is_skipped() {
        assert_eq!(skip("Data Scientist"), Some(SkipReason::OtherRole("scientist")));
        assert_eq!(skip("Business Analyst II"), Some(SkipReason::OtherRole("analyst")));
        assert_eq!(
            skip("IT Systems Administrator"),
            Some(SkipReason::OtherRole("administrator"))
        );
        assert_eq!(
            skip("Technical Support Specialist"),
            Some(SkipReason::OtherRole("technical support"))
        );
        assert_eq!(
            skip("Mechanical Engineer"),
            Some(SkipReason::OtherRole("mechanical engineer"))
        );
    }

    #[test]
    fn a_posting_is_skipped_for_location_only_when_every_place_is_abroad() {
        let title = "Software Engineer";
        assert_eq!(
            prefilter(title, Some("Toronto, Ontario, Canada")),
            Some(SkipReason::OutsideUs)
        );
        assert_eq!(
            prefilter(title, Some("Leeds, England, United Kingdom | Dublin, Ireland")),
            Some(SkipReason::OutsideUs)
        );
        assert_eq!(
            prefilter(title, Some("Toronto, Ontario, Canada | Austin, Texas, United States")),
            None
        );
        assert_eq!(
            prefilter(title, Some("Toronto, Ontario, Canada | Lyon, France | ...")),
            None
        );
        assert_eq!(prefilter(title, Some("Remote")), None);
        assert_eq!(prefilter(title, Some("Austin, TX, US")), None);
        assert_eq!(prefilter(title, None), None);
    }

    #[test]
    fn a_job_written_another_way_repeats_the_first() {
        let mut seen = SeenJobs::default();
        assert!(!seen.insert("Example Corp", "Senior Software Engineer, Payments"));
        assert!(seen.insert("EXAMPLE CORP.", "Sr. Software Engineer - Payments"));
        assert!(seen.insert("Example Corp", "Sr Software Engineer, Payments (R123456)"));
        assert!(!seen.insert("Example Corp", "Senior Software Engineer, Search"));
        assert!(!seen.insert("Other Corp", "Senior Software Engineer, Payments"));
        assert!(!seen.insert("Example Corp", "Software Engineer, New Grad 2026"));
        assert!(!seen.insert("Example Corp", "Software Engineer, New Grad 2027"));
    }

    #[test]
    fn two_requisition_codes_are_two_jobs() {
        let mut seen = SeenJobs::default();
        assert!(!seen.insert("Example Corp", "Software Engineer II - REQ10563965"));
        assert!(!seen.insert("Example Corp", "Software Engineer II - REQ10573435"));
        assert!(seen.insert("Example Corp", "Software Engineer II (REQ10563965)"));
        assert!(seen.insert("Example Corp", "Software Engineer II"));
    }
}
