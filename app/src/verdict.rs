//! Reads the verdict that scoring wrote for a posting, so the Jobs pane can show it in parts.
//!
//! Maverick's scorer and Muse wrote verdicts as one line in several shapes, such as
//! "Review - 72, Backend/Infra: fits. Outreach: ... Team: ...". The status and the score
//! show elsewhere on the pane, so they are taken off, and the labelled parts are split out.

/// A verdict in parts. Each list keeps the order the verdict gave.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Verdict {
    /// Why the posting fits or does not.
    pub reason: String,
    /// What stops an application until the user acts.
    pub blocking: Vec<String>,
    /// What is worth knowing but does not stop an application.
    pub advisory: Vec<String>,
    /// The team the posting is for.
    pub team: Option<String>,
    /// Whom to reach, as the scorer found it.
    pub outreach: Vec<String>,
}

impl Verdict {
    /// Reads a posting's verdict (`notes`) and its flags.
    pub fn read(notes: Option<&str>, flags: Option<&str>) -> Self {
        let mut verdict = Verdict::default();
        let notes = notes.map(str::trim).unwrap_or_default();
        let mut parts = labelled(without_score(without_status(notes)));
        if let Some((None, reason)) = parts.first() {
            verdict.reason = reason.clone();
            parts.remove(0);
        }
        for (label, text) in parts.into_iter().chain(labelled(flags.unwrap_or_default())) {
            verdict.add(label, &text);
        }
        let lists = [&mut verdict.blocking, &mut verdict.advisory, &mut verdict.outreach];
        for text in lists
            .into_iter()
            .flatten()
            .chain([&mut verdict.reason])
            .chain(&mut verdict.team)
        {
            *text = sentence_case(text);
        }
        verdict
    }

    fn add(&mut self, label: Option<Label>, text: &str) {
        let items = || clauses(text).into_iter();
        match label {
            Some(Label::Blocking) => self.blocking.extend(items()),
            Some(Label::Advisory) => self.advisory.extend(items()),
            Some(Label::Outreach) => self.outreach.extend(items()),
            Some(Label::Team) => self.team = Some(text.trim().trim_end_matches('.').to_owned()),
            None => self.reason.push_str(text),
        }
    }
}

/// `text` with its first letter in capitals.
fn sentence_case(text: &str) -> String {
    let mut chars = text.chars();
    chars
        .next()
        .map_or_else(String::new, |first| first.to_uppercase().chain(chars).collect())
}

/// Splits `text` at each "; " outside parentheses, without the closing full stop.
fn clauses(text: &str) -> Vec<String> {
    let mut clauses = Vec::new();
    let (mut depth, mut start) = (0i32, 0);
    for (at, ch) in text.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => depth -= 1,
            ';' if depth <= 0 => {
                clauses.push(&text[start..at]);
                start = at + 1;
            }
            _ => {}
        }
    }
    clauses.push(&text[start..]);
    clauses
        .into_iter()
        .map(|clause| clause.trim().trim_end_matches('.').to_owned())
        .filter(|clause| !clause.is_empty())
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Label {
    Blocking,
    Advisory,
    Team,
    Outreach,
}

const LABELS: [(&str, Label); 5] = [
    ("Outreach verdict:", Label::Outreach),
    ("Outreach:", Label::Outreach),
    ("Blocking:", Label::Blocking),
    ("Advisory:", Label::Advisory),
    ("Team:", Label::Team),
];

/// Splits `text` at each label that starts it or follows the end of a sentence or clause.
fn labelled(text: &str) -> Vec<(Option<Label>, String)> {
    let mut starts: Vec<(usize, usize, Label)> = Vec::new();
    for (word, label) in LABELS {
        for (at, _) in text.match_indices(word) {
            let before = text[..at].trim_end();
            let opens = before.is_empty() || before.ends_with(['.', ';']);
            let taken = starts.iter().any(|(start, end, _)| (*start..*end).contains(&at));
            if opens && !taken {
                starts.push((at, at + word.len(), label));
            }
        }
    }
    starts.sort_by_key(|(start, ..)| *start);
    let mut parts = Vec::new();
    let first = starts.first().map_or(text.len(), |(start, ..)| *start);
    let lead = text[..first].trim().trim_end_matches(';').trim();
    if !lead.is_empty() {
        parts.push((None, lead.to_owned()));
    }
    for (ix, (_, end, label)) in starts.iter().enumerate() {
        let until = starts.get(ix + 1).map_or(text.len(), |(start, ..)| *start);
        let body = text[*end..until].trim().trim_end_matches(';').trim();
        if !body.is_empty() {
            parts.push((Some(*label), body.to_owned()));
        }
    }
    parts
}

/// Takes off "Review -", "Apply-ready (...) - needs your approval to submit." and the like.
fn without_status(notes: &str) -> &str {
    let mut rest = notes;
    if rest.starts_with("Apply-ready") {
        rest = match rest.find("to submit.") {
            Some(at) => &rest[at + "to submit.".len()..],
            None => rest.trim_start_matches("Apply-ready"),
        };
    } else {
        for word in ["Review", "Skipped", "Not scored yet", "Found"] {
            if let Some(after) = rest.strip_prefix(word) {
                rest = after;
                break;
            }
        }
    }
    rest.trim_start().trim_start_matches(['—', '-', ':']).trim_start()
}

/// Takes off a leading score and variant, such as "72, Backend/Infra:", "fit 72." or "score 82 (...):".
fn without_score(text: &str) -> &str {
    let mut rest = text;
    for word in ["fit ", "score "] {
        if let Some(after) = rest.strip_prefix(word) {
            rest = after;
        }
    }
    let digits = rest.len() - rest.trim_start_matches(|ch: char| ch.is_ascii_digit()).len();
    if digits == 0 || digits > 3 {
        return text;
    }
    let after = &rest[digits..];
    let variant_head = |text: &str| {
        text.find(':')
            .is_some_and(|colon| colon <= 40 && !text[..colon].contains(". "))
    };
    let ends_score = after.is_empty()
        || after.starts_with([',', '.', ';', '('])
        || after.starts_with(" (")
        || after.starts_with(" —")
        || (after.starts_with(' ') && variant_head(after));
    if !ends_score {
        return text;
    }
    rest = after.trim_start_matches([',', ' ']);
    if rest.starts_with('(')
        && let Some(close) = rest.find(')')
    {
        rest = &rest[close + 1..];
    }
    // A short head before a colon names the variant, as in "Backend/Infra:".
    if let Some(colon) = rest.find(':') {
        let head = &rest[..colon];
        if colon <= 40 && !head.contains(". ") {
            rest = &rest[colon + 1..];
        }
    }
    rest.trim_start()
        .trim_start_matches(['.', ';', ',', '—', '-'])
        .trim_start()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maverick_verdicts_lose_the_status_and_score_and_keep_the_reason() {
        let verdict = Verdict::read(
            Some(
                "Apply-ready (>=75, no blocking flags) - needs your approval to submit. 80, Backend/Infra: SDE I backend Python services match the resume.",
            ),
            Some("Blocking: mandatory Workday account creation. Advisory: SRE III seniority; Ansible gaps"),
        );
        assert_eq!(
            verdict,
            Verdict {
                reason: "SDE I backend Python services match the resume.".into(),
                blocking: vec!["Mandatory Workday account creation".into()],
                advisory: vec!["SRE III seniority".into(), "Ansible gaps".into()],
                team: None,
                outreach: vec![],
            }
        );
    }

    #[test]
    fn muse_verdicts_split_into_reason_team_and_outreach() {
        let verdict = Verdict::read(
            Some(
                "Review — 0-3 yrs platform services fits; Workday is blocking. Team: Platform Services. Outreach: None — apply cold (no HM named; connections CSV not on file).",
            ),
            None,
        );
        assert_eq!(verdict.reason, "0-3 yrs platform services fits; Workday is blocking.");
        assert_eq!(verdict.team.as_deref(), Some("Platform Services"));
        assert_eq!(
            verdict.outreach,
            ["None — apply cold (no HM named; connections CSV not on file)"]
        );

        for (notes, reason) in [
            (
                "Review — fit 72. Detection team (Security Response).",
                "Detection team (Security Response).",
            ),
            (
                "Review — score 82 (Agentic AI; Backend/Infra 78): backend services match. Blocking: Workday account creation.",
                "Backend services match.",
            ),
            (
                "Review — 68 Backend/Infra: AWS/React/Mongo fit.",
                "AWS/React/Mongo fit.",
            ),
            (
                "Review — strong skills match (AWS/Terraform) but 5–7 yrs required.",
                "Strong skills match (AWS/Terraform) but 5–7 yrs required.",
            ),
            (
                "Posting closed — the company page says this job is no longer available.",
                "Posting closed — the company page says this job is no longer available.",
            ),
        ] {
            assert_eq!(Verdict::read(Some(notes), None).reason, reason, "{notes}");
        }
        let blocked = Verdict::read(
            Some(
                "Review — score 82 (Agentic AI): match. Blocking: Workday account creation. Outreach verdict: None — apply cold.",
            ),
            None,
        );
        assert_eq!(blocked.blocking, ["Workday account creation"]);
        assert_eq!(blocked.outreach, ["None — apply cold"]);
    }
}
