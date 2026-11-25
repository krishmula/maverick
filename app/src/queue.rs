//! The apply queue: approved postings, filled several at once, each in its own Chrome tab. It never submits.

use maverick_core::answers::Unanswered;
use maverick_core::apply::{End, FieldResult, Report};
use maverick_core::bridge::Tab;

/// How many forms fill at once unless the user picks another number.
pub const DEFAULT_PARALLEL: usize = 3;
pub const MOST_PARALLEL: usize = 8;

/// Where one queued application is.
#[derive(Clone, Debug, PartialEq)]
pub enum Step {
    /// Waiting for a free slot.
    Queued,
    /// Opening a tab and loading the form.
    Opening,
    /// Answering the form.
    Filling,
    /// The tab waits on the user: required fields left, a sign-in wall, an email to verify,
    /// or a system Maverick cannot fill. The text says which.
    NeedsYou(String),
    /// Every required field is answered. The user reviews and clicks Submit.
    Ready,
    /// The application system confirmed a submission.
    Applied,
    Failed(String),
}

impl Step {
    /// True while the run holds one of the parallel slots.
    pub fn is_running(&self) -> bool {
        matches!(self, Self::Opening | Self::Filling)
    }

    /// The short word a list row shows.
    pub fn word(&self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Opening => "opening",
            Self::Filling => "filling",
            Self::NeedsYou(_) => "needs you",
            Self::Ready => "ready to submit",
            Self::Applied => "applied",
            Self::Failed(_) => "failed",
        }
    }
}

/// What a finished fill leaves for the user: ready to submit, or the required fields still open.
pub fn after_fill(report: &Report) -> Step {
    match &report.end {
        // The form's own checks let every page through.
        End::Review => return Step::Ready,
        End::Stopped(why) => return Step::NeedsYou(why.clone()),
        End::Page => {}
    }
    let answered =
        |result: &FieldResult| matches!(result, FieldResult::Filled { .. } | FieldResult::Left(Unanswered::Held));
    let open = report
        .outcomes
        .iter()
        .filter(|outcome| outcome.required && !answered(&outcome.result))
        .count();
    match open {
        0 => Step::Ready,
        1 => Step::NeedsYou("1 required field needs your answer.".into()),
        open => Step::NeedsYou(format!("{open} required fields need your answer.")),
    }
}

/// One approved posting in the queue.
#[derive(Clone, Debug, PartialEq)]
pub struct Run {
    pub title: String,
    /// The posting's own address, or the link it was found by when Maverick cannot fill it.
    pub address: String,
    pub step: Step,
    /// The tab the run opened, once it has one.
    pub tab: Option<Tab>,
    pub report: Option<Report>,
}

#[derive(Debug)]
pub struct Queue {
    runs: Vec<Run>,
    parallel: usize,
}

impl Queue {
    pub fn new(parallel: usize) -> Self {
        Self {
            runs: Vec::new(),
            parallel: parallel.clamp(1, MOST_PARALLEL),
        }
    }

    pub fn parallel(&self) -> usize {
        self.parallel
    }

    pub fn set_parallel(&mut self, parallel: usize) {
        self.parallel = parallel.clamp(1, MOST_PARALLEL);
    }

    /// Adds a posting at the end. A posting already in the queue stays where it is,
    /// unless its last run failed, in which case it waits for another try.
    pub fn add(&mut self, title: String, address: String) {
        match self.runs.iter_mut().find(|run| run.address == address) {
            Some(run) if matches!(run.step, Step::Failed(_)) => {
                run.step = Step::Queued;
                run.report = None;
            }
            Some(_) => {}
            None => self.runs.push(Run {
                title,
                address,
                step: Step::Queued,
                tab: None,
                report: None,
            }),
        }
    }

    /// Takes a posting out while it still waits. One that started stays, since its tab is open.
    pub fn withdraw(&mut self, address: &str) {
        self.runs
            .retain(|run| !(run.address == address && run.step == Step::Queued));
    }

    /// Moves waiting runs to `Opening` while slots are free, and returns their addresses.
    pub fn start_next(&mut self) -> Vec<String> {
        let mut free = self.parallel.saturating_sub(self.running());
        let mut started = Vec::new();
        for run in &mut self.runs {
            if free == 0 {
                break;
            }
            if run.step == Step::Queued {
                run.step = Step::Opening;
                started.push(run.address.clone());
                free -= 1;
            }
        }
        started
    }

    pub fn running(&self) -> usize {
        self.runs.iter().filter(|run| run.step.is_running()).count()
    }

    pub fn get(&self, address: &str) -> Option<&Run> {
        self.runs.iter().find(|run| run.address == address)
    }

    pub fn get_mut(&mut self, address: &str) -> Option<&mut Run> {
        self.runs.iter_mut().find(|run| run.address == address)
    }

    pub fn runs(&self) -> &[Run] {
        &self.runs
    }
}

#[cfg(test)]
mod tests {
    use maverick_core::answers::Unanswered;
    use maverick_core::apply::{Outcome, Source};

    use super::*;

    #[test]
    fn the_queue_starts_no_more_runs_than_the_parallel_limit() {
        let mut queue = Queue::new(2);
        for id in ["a", "b", "c", "d"] {
            queue.add(format!("Job {id}"), id.into());
        }
        queue.add("Job a again".into(), "a".into());
        assert_eq!(queue.runs().len(), 4);

        assert_eq!(queue.start_next(), ["a", "b"]);
        assert!(queue.start_next().is_empty());
        queue.get_mut("a").unwrap().step = Step::Filling;
        assert!(queue.start_next().is_empty());

        queue.get_mut("b").unwrap().step = Step::Failed("the page did not load".into());
        assert_eq!(queue.start_next(), ["c"]);

        queue.withdraw("d");
        queue.withdraw("a");
        let left: Vec<&str> = queue.runs().iter().map(|run| run.address.as_str()).collect();
        assert_eq!(left, ["a", "b", "c"]);

        // A failed run goes back in line when the posting is approved again.
        queue.set_parallel(3);
        queue.add("Job b".into(), "b".into());
        assert_eq!(queue.start_next(), ["b"]);
        queue.set_parallel(0);
        assert_eq!(queue.parallel(), 1);
    }

    #[test]
    fn a_form_of_several_pages_is_ready_only_at_its_last_page() {
        let left = Outcome {
            title: "Country".into(),
            required: true,
            result: FieldResult::Left(Unanswered::Held),
        };
        let report = |end| Report {
            outcomes: vec![left.clone()],
            model_problem: None,
            note: None,
            end,
        };
        // An answer the form held already leaves nothing for the user to do.
        assert_eq!(after_fill(&report(End::Page)), Step::Ready);
        assert_eq!(after_fill(&report(End::Review)), Step::Ready);
        assert_eq!(
            after_fill(&report(End::Stopped("Workday did not take My Experience.".into()))),
            Step::NeedsYou("Workday did not take My Experience.".into())
        );
    }

    #[test]
    fn a_fill_with_required_fields_left_needs_the_user() {
        let outcome = |required: bool, result: FieldResult| Outcome {
            title: "Field".into(),
            required,
            result,
        };
        let filled = || FieldResult::Filled {
            shown: "Ada".into(),
            by: Source::StandardAnswer,
        };
        let left = || FieldResult::Left(Unanswered::NoRule);
        let report = |outcomes| Report {
            outcomes,
            model_problem: None,
            note: None,
            end: maverick_core::apply::End::Page,
        };
        assert_eq!(
            after_fill(&report(vec![outcome(true, filled()), outcome(false, left())])),
            Step::Ready
        );
        assert_eq!(
            after_fill(&report(vec![
                outcome(true, left()),
                outcome(true, FieldResult::Failed("the field stayed empty".into())),
            ])),
            Step::NeedsYou("2 required fields need your answer.".into())
        );
    }
}
