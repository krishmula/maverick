//! How the user writes: rules they set for a message and messages they sent themselves.
//! Both are personal, so the file lives outside the repository, next to the answers file.

use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use serde::Deserialize;

use crate::linkedin::Slot;

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Voice {
    /// What a message holds and how it reads, one rule per entry.
    #[serde(default)]
    pub rules: Vec<String>,
    /// Messages the user sent. A draft copies their greeting, length, wording and sign-off.
    #[serde(default)]
    pub examples: Vec<Example>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Example {
    pub slot: Slot,
    /// Who it went to and how it went, such as `a recruiter; got a reply`.
    #[serde(default)]
    pub about: Option<String>,
    #[serde(default)]
    pub subject: Option<String>,
    pub text: String,
}

impl Voice {
    /// The voice in `file`, or an empty one when there is no file.
    pub fn load(file: &Path) -> Result<Self, String> {
        if !file.exists() {
            return Ok(Self::default());
        }
        let text = fs::read_to_string(file).map_err(|error| format!("could not read {}: {error}", file.display()))?;
        serde_json::from_str(&text).map_err(|error| format!("{} is not a valid voice file: {error}", file.display()))
    }

    /// What the model is told about the user's voice for a draft in `slot`. Empty with no voice.
    /// A note is shown only notes. A message and an InMail read alike, so each is shown both.
    pub fn told(&self, slot: Slot) -> String {
        let mut told = String::new();
        if !self.rules.is_empty() {
            told.push_str("\nThe applicant's rules for a message:\n");
            for rule in &self.rules {
                let _ = writeln!(told, "- {rule}");
            }
        }
        let alike = |example: &&Example| (example.slot == Slot::Note) == (slot == Slot::Note);
        let examples: Vec<_> = self.examples.iter().filter(alike).collect();
        if !examples.is_empty() {
            told.push_str("\nMessages the applicant wrote and sent:\n");
        }
        for example in examples {
            told.push_str("---\n");
            if let Some(about) = &example.about {
                let _ = writeln!(told, "(To {about})");
            }
            if let Some(subject) = &example.subject {
                let _ = writeln!(told, "Subject: {subject}");
            }
            let _ = writeln!(told, "{}", example.text);
        }
        told
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn voice() -> Voice {
        serde_json::from_str(
            r#"{
                "rules": ["End on one ask."],
                "examples": [
                    {"slot": "note", "text": "Hi Bo, saw your post. Would love to connect."},
                    {"slot": "in_mail", "about": "a recruiter", "subject": "Applied: Backend",
                     "text": "Hi Ada,\n\nI applied.\n\nThanks,\nSam"}
                ]
            }"#,
        )
        .unwrap()
    }

    #[test]
    fn a_note_is_shown_the_rules_and_only_the_notes() {
        let told = voice().told(Slot::Note);
        assert!(told.contains("- End on one ask."));
        assert!(told.contains("Hi Bo, saw your post."));
        assert!(!told.contains("Hi Ada"));
    }

    #[test]
    fn a_message_is_shown_the_inmails_with_their_subjects() {
        let told = voice().told(Slot::Message);
        assert!(told.contains("(To a recruiter)\nSubject: Applied: Backend\nHi Ada,"));
        assert!(!told.contains("Hi Bo"));
    }

    #[test]
    fn no_file_is_no_voice_and_says_nothing() {
        let voice = Voice::load(Path::new("/nonexistent/voice.json")).unwrap();
        assert_eq!(voice, Voice::default());
        assert_eq!(voice.told(Slot::InMail), "");
    }
}
