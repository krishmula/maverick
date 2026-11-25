//! How a fill report is shown: one square per form field, the counts, and the fields that need a look.

use gpui_kit::component::{h_flex, v_flex};
use gpui_kit::{Div, ParentElement as _, SharedString, Styled as _, div, px};
use maverick_core::apply::{FieldResult, Outcome, Report, Source};

use crate::ui::{self, Palette, Signal};

/// The signal a field's outcome carries. A field filled from a standard answer carries none.
pub fn signal_of(result: &FieldResult) -> Option<Signal> {
    match result {
        FieldResult::Filled {
            by: Source::StandardAnswer,
            ..
        } => None,
        FieldResult::Filled { .. } => Some(Signal::Model),
        FieldResult::Left(_) => Some(Signal::Yours),
        FieldResult::Failed(_) => Some(Signal::Failed),
    }
}

/// How many fields ended each way.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Tally {
    /// Filled from the user's standard answers.
    pub filled: usize,
    pub model: usize,
    pub yours: usize,
    pub failed: usize,
}

impl Tally {
    pub fn of(report: &Report) -> Self {
        let mut tally = Self::default();
        for outcome in &report.outcomes {
            match signal_of(&outcome.result) {
                None => tally.filled += 1,
                Some(Signal::Model) => tally.model += 1,
                Some(Signal::Yours) => tally.yours += 1,
                Some(Signal::Failed) => tally.failed += 1,
                Some(Signal::Done) => {}
            }
        }
        tally
    }
}

/// One square per field, in the form's order, with the counts under them.
pub fn form_map(report: &Report, palette: &Palette) -> Div {
    let tally = Tally::of(report);
    let count = |number: usize, name: &str, signal: Option<Signal>| {
        let figure = match signal {
            Some(signal) if number > 0 => palette.signal(signal),
            Some(_) => palette.faint,
            None => palette.ink,
        };
        h_flex()
            .gap(px(5.))
            .child(ui::caps(number.to_string()).text_color(figure))
            .child(ui::caps(name).text_color(palette.faint))
    };
    v_flex()
        .gap_2()
        .child(
            h_flex()
                .flex_wrap()
                .gap(px(3.))
                .children(report.outcomes.iter().map(|outcome| {
                    let color = match signal_of(&outcome.result) {
                        Some(signal) => palette.signal(signal),
                        None => palette.ink.opacity(0.22),
                    };
                    div().flex_none().size(px(10.)).bg(color)
                })),
        )
        .child(
            h_flex()
                .gap_4()
                .child(count(tally.filled, "filled", None))
                .child(count(tally.model, "model", Some(Signal::Model)))
                .child(count(tally.yours, "yours", Some(Signal::Yours)))
                .child(count(tally.failed, "failed", Some(Signal::Failed))),
        )
}

/// The fields the user should look at, most urgent first, then one line for the rest.
pub fn sections(report: &Report, palette: &Palette) -> Div {
    let of = |signal: Signal| {
        report
            .outcomes
            .iter()
            .filter(move |outcome| signal_of(&outcome.result) == Some(signal))
    };
    let section = |signal: Signal, name: &str| {
        let fields: Vec<Div> = of(signal).map(|outcome| field(outcome, signal, palette)).collect();
        (!fields.is_empty()).then(|| {
            v_flex()
                .gap(px(7.))
                .child(h_flex().child(ui::chip(signal, format!("{name} {}", fields.len()), palette)))
                .children(fields)
        })
    };
    let plain: Vec<&str> = report
        .outcomes
        .iter()
        .filter(|outcome| signal_of(&outcome.result).is_none())
        .map(|outcome| outcome.title.as_str())
        .collect();
    v_flex()
        .gap_4()
        .children(
            report
                .model_problem
                .as_ref()
                .map(|problem| line(Signal::Failed, "The model was not asked", problem.clone(), palette)),
        )
        .children(
            report
                .note
                .as_ref()
                .map(|note| ui::serif(note.clone()).text_size(px(13.)).text_color(palette.muted)),
        )
        .children(section(Signal::Failed, "failed"))
        .children(section(Signal::Yours, "yours"))
        .children(section(Signal::Model, "model"))
        .children((!plain.is_empty()).then(|| {
            v_flex()
                .gap(px(7.))
                .child(h_flex().child(ui::boxed(format!("filled {}", plain.len()), palette.muted)))
                .child(
                    div()
                        .font_family(ui::MONO)
                        .text_size(px(11.))
                        .line_height(px(15.))
                        .text_color(palette.muted)
                        .child(SharedString::from(listed(&plain))),
                )
        }))
}

fn field(outcome: &Outcome, signal: Signal, palette: &Palette) -> Div {
    let required = if outcome.required { "Required. " } else { "" };
    let (title, why) = match &outcome.result {
        FieldResult::Left(why) => (
            outcome.title.clone(),
            format!("{required}{}", sentence(&why.to_string())),
        ),
        FieldResult::Failed(problem) => (outcome.title.clone(), format!("{required}{}", sentence(problem))),
        FieldResult::Filled {
            by: Source::Draft { basis },
            ..
        } => (
            outcome.title.clone(),
            format!("Draft. Read it before you submit. {basis}"),
        ),
        FieldResult::Filled {
            shown,
            by: Source::Model { basis },
        } => (format!("{} / {shown}", outcome.title), basis.clone()),
        FieldResult::Filled { .. } => (outcome.title.clone(), String::new()),
    };
    line(signal, &title, why, palette)
}

/// A square in the signal's color, a serif title, and the reason under it in mono.
fn line(signal: Signal, title: &str, why: String, palette: &Palette) -> Div {
    h_flex()
        .items_start()
        .gap(px(9.))
        .child(ui::square(palette.signal(signal)).mt(px(7.)))
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .child(ui::serif(title.to_owned()).text_color(palette.ink))
                .children((!why.is_empty()).then(|| {
                    div()
                        .font_family(ui::MONO)
                        .text_size(px(11.))
                        .line_height(px(15.))
                        .text_color(palette.muted)
                        .child(SharedString::from(why))
                })),
        )
}

/// A reason written as a sentence: a capital first, a full stop last.
fn sentence(reason: &str) -> String {
    let mut letters = reason.trim().chars();
    let Some(first) = letters.next() else {
        return String::new();
    };
    let mut sentence: String = first.to_uppercase().chain(letters).collect();
    if !sentence.ends_with(['.', '!', '?']) {
        sentence.push('.');
    }
    sentence
}

/// The first few names, then how many more there are.
fn listed(names: &[&str]) -> String {
    const NAMED: usize = 4;
    let shown = names[..names.len().min(NAMED)].join(", ");
    match names.len().saturating_sub(NAMED) {
        0 => shown,
        more => format!("{shown} and {more} more"),
    }
}

#[cfg(test)]
mod tests {
    use maverick_core::answers::Unanswered;

    use super::*;

    fn outcome(title: &str, result: FieldResult) -> Outcome {
        Outcome {
            title: title.into(),
            required: false,
            result,
        }
    }

    #[test]
    fn a_report_is_tallied_by_who_answered_each_field() {
        let filled = |by: Source| FieldResult::Filled { shown: "x".into(), by };
        let report = Report {
            outcomes: vec![
                outcome("Name", filled(Source::StandardAnswer)),
                outcome("Email", filled(Source::StandardAnswer)),
                outcome("Years of Rust", filled(Source::Model { basis: "b".into() })),
                outcome("Why us?", filled(Source::Draft { basis: "b".into() })),
                outcome("Expected pay", FieldResult::Left(Unanswered::NoRule)),
                outcome("Resume", FieldResult::Failed("the upload did not finish".into())),
            ],
            model_problem: None,
            note: None,
            end: maverick_core::apply::End::Page,
        };
        assert_eq!(
            Tally::of(&report),
            Tally {
                filled: 2,
                model: 2,
                yours: 1,
                failed: 1
            }
        );
    }

    #[test]
    fn reasons_read_as_sentences_and_long_lists_are_cut_short() {
        assert_eq!(
            sentence("your standard answers do not cover this"),
            "Your standard answers do not cover this."
        );
        assert_eq!(sentence("Chrome said no."), "Chrome said no.");
        assert_eq!(sentence(" "), "");
        assert_eq!(listed(&["Name", "Email"]), "Name, Email");
        assert_eq!(
            listed(&["Name", "Email", "Phone", "Resume", "City", "Website"]),
            "Name, Email, Phone, Resume and 2 more"
        );
    }
}
