//! The apply queue: approved postings, filled several at once, each in its own Chrome tab.

use maverick_core::apply::{End, Report, Sent};
use maverick_core::bridge::Tab;
use maverick_core::jobs::Failure;

/// How many forms fill at once unless the user picks another number.
pub const DEFAULT_PARALLEL: usize = 3;
pub const MOST_PARALLEL: usize = 8;
/// The most tabs the queue keeps open at once, filling or waiting on the user. Past it, no
/// new form opens until the user submits or closes one, so a long queue cannot swamp Chrome.
pub const MOST_OPEN_TABS: usize = 8;

/// Where one queued application is.
#[derive(Clone, Debug, PartialEq)]
pub enum Step {
    /// Waiting for a free slot.
    Queued,
    /// Opening a tab and loading the form.
    Opening,
    /// Answering the form.
    Filling,
    /// Nothing in the form was left for the user, and Maverick pressed Submit.
    Submitting,
    /// The tab waits on the user: required fields left, a sign-in wall, an email to verify,
    /// or a system Maverick cannot fill. The text says which.
    NeedsYou(String),
    /// Every required field is answered. The user reviews and clicks Submit.
    Ready,
    /// The application system confirmed a submission.
    Applied,
    /// Maverick could not apply. The text says why.
    Failed(Failure, String),
}

impl Step {
    /// True while the run holds one of the parallel slots.
    pub fn is_running(&self) -> bool {
        matches!(self, Self::Opening | Self::Filling | Self::Submitting)
    }

    /// The short word a list row shows.
    pub fn word(&self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Opening => "opening",
            Self::Filling => "filling",
            Self::Submitting => "submitting",
            Self::NeedsYou(_) => "needs you",
            Self::Ready => "ready to submit",
            Self::Applied => "applied",
            Self::Failed(..) => "failed",
        }
    }
}

/// Where a finished fill leaves a run. `sent` says what a press of Submit led to, when Maverick
/// pressed it.
pub fn after_fill(report: &Report, sent: Option<&Sent>) -> Step {
    match sent {
        // The step turns to `Applied` once the application is recorded.
        Some(Sent::Confirmed) => return Step::Submitting,
        Some(Sent::Unconfirmed(why)) => return Step::NeedsYou(why.clone()),
        None => {}
    }
    match &report.end {
        // The form's own checks let every page through.
        End::Review => return Step::Ready,
        End::Stopped(why) => return Step::NeedsYou(why.clone()),
        End::Page => {}
    }
    match report.outcomes.iter().filter(|outcome| outcome.is_open()).count() {
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
            Some(run) if matches!(run.step, Step::Failed(..)) => {
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

    /// Moves waiting runs to `Opening` while slots are free and fewer than `MOST_OPEN_TABS`
    /// tabs are open, and returns their addresses.
    pub fn start_next(&mut self) -> Vec<String> {
        let open = self.running() + self.waiting_on_you();
        let mut free = self
            .parallel
            .saturating_sub(self.running())
            .min(MOST_OPEN_TABS.saturating_sub(open));
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

    /// Runs whose tab waits on the user.
    pub fn waiting_on_you(&self) -> usize {
        self.runs
            .iter()
            .filter(|run| matches!(run.step, Step::Ready | Step::NeedsYou(_)))
            .count()
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
    use maverick_core::apply::{FieldResult, Outcome, Source};

    use super::*;

    fn after_fill_alone(report: &Report) -> Step {
        after_fill(report, None)
    }

    #[test]
    fn a_press_of_submit_decides_the_step_whatever_the_report_says() {
        let report = Report {
            outcomes: Vec::new(),
            model_problem: None,
            note: None,
            end: End::Page,
        };
        assert_eq!(after_fill(&report, Some(&Sent::Confirmed)), Step::Submitting);
        assert_eq!(
            after_fill(&report, Some(&Sent::Unconfirmed("Greenhouse asks for a code.".into()))),
            Step::NeedsYou("Greenhouse asks for a code.".into())
        );
    }

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

        queue.get_mut("b").unwrap().step = Step::Failed(Failure::Failed, "the page did not load".into());
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
    fn no_new_form_opens_while_too_many_tabs_wait_on_the_user() {
        let mut queue = Queue::new(3);
        for id in 0..MOST_OPEN_TABS + 2 {
            queue.add(format!("Job {id}"), id.to_string());
        }
        for _ in 0..MOST_OPEN_TABS {
            for address in queue.start_next() {
                queue.get_mut(&address).unwrap().step = Step::Ready;
            }
        }
        assert_eq!(queue.waiting_on_you(), MOST_OPEN_TABS);
        assert!(queue.start_next().is_empty());

        queue.get_mut("0").unwrap().step = Step::Applied;
        assert_eq!(queue.start_next(), [MOST_OPEN_TABS.to_string()]);
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
        assert_eq!(after_fill_alone(&report(End::Page)), Step::Ready);
        assert_eq!(after_fill_alone(&report(End::Review)), Step::Ready);
        assert_eq!(
            after_fill_alone(&report(End::Stopped("Workday did not take My Experience.".into()))),
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
            after_fill_alone(&report(vec![outcome(true, filled()), outcome(false, left())])),
            Step::Ready
        );
        assert_eq!(
            after_fill_alone(&report(vec![
                outcome(true, left()),
                outcome(true, FieldResult::Failed("the field stayed empty".into())),
            ])),
            Step::NeedsYou("2 required fields need your answer.".into())
        );
    }
}
