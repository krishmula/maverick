//! The list of postings Maverick can apply to.

use gpui_kit::component::list::{ListDelegate, ListItem, ListState};
use gpui_kit::component::{IndexPath, h_flex, v_flex};
use gpui_kit::{App, Context, Div, ElementId, Entity, ParentElement as _, Styled as _, Task, Window, px};
use maverick_core::apply::{self, System};
use maverick_core::jobs::{Application, Posting, Stage, posting_address};

use crate::browser::{Browser, Fill, Subject};
use crate::report::Tally;
use crate::ui::{self, Palette, Signal};

/// A posting whose form Maverick knows how to fill.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Job {
    pub company: String,
    pub title: String,
    pub score: Option<u8>,
    /// The posting's own address, not a tracking link to it.
    pub address: String,
    pub location: Option<String>,
    /// Where intake found the posting, such as a job alert.
    pub source: String,
    /// The day intake found it, as the user reads a date.
    pub found_on: String,
}

/// The postings still waiting for an application, best score first.
/// A posting with a recorded application is left out, whatever its stage says.
pub fn open_jobs(postings: &[Posting], applications: &[Application]) -> Vec<Job> {
    let applied: Vec<String> = applications
        .iter()
        .filter_map(|sent| apply::posting_address(sent.link.as_deref()?))
        .collect();
    let mut jobs: Vec<Job> = postings
        .iter()
        .filter(|posting| matches!(posting.stage, Stage::Found | Stage::Review))
        .filter_map(|posting| {
            Some(Job {
                company: posting.company.clone(),
                title: posting.title.clone(),
                score: posting.score,
                address: posting
                    .address
                    .as_deref()
                    .and_then(apply::posting_address)
                    .or_else(|| apply::posting_address(&posting_address(&posting.link)))?,
                location: posting.location.clone(),
                source: posting.source.clone(),
                found_on: posting.found_at.strftime("%b %-d, %Y").to_string(),
            })
        })
        .filter(|job| !applied.contains(&job.address))
        .collect();
    jobs.sort_by_key(|job| std::cmp::Reverse(job.score));
    jobs.dedup_by(|later, earlier| later.address == earlier.address);
    jobs
}

pub struct JobList {
    browser: Entity<Browser>,
    jobs: Vec<Job>,
    query: String,
    matches: Vec<Job>,
    selected: Option<IndexPath>,
    /// Addresses Maverick has opened in this run.
    opened: Vec<String>,
}

impl JobList {
    pub fn new(browser: Entity<Browser>, jobs: Vec<Job>) -> Self {
        Self {
            browser,
            matches: jobs.clone(),
            jobs,
            query: String::new(),
            selected: None,
            opened: Vec::new(),
        }
    }

    pub fn job_at(&self, ix: IndexPath) -> Option<&Job> {
        self.matches.get(ix.row)
    }

    pub fn selected_job(&self) -> Option<&Job> {
        self.job_at(self.selected?)
    }

    /// The best jobs the search still shows, without the one at `except`.
    pub fn best(&self, count: usize, except: Option<&str>) -> impl Iterator<Item = &Job> {
        self.matches
            .iter()
            .filter(move |job| Some(job.address.as_str()) != except)
            .take(count)
    }

    pub fn len(&self) -> usize {
        self.jobs.len()
    }

    /// How many jobs the search still shows.
    pub fn shown(&self) -> usize {
        self.matches.len()
    }

    pub fn mark_opened(&mut self, address: &str) {
        self.opened.push(address.to_owned());
    }

    /// Takes a job off the list once its application went through.
    pub fn remove(&mut self, address: &str) {
        self.jobs.retain(|job| job.address != address);
        self.matches = matching(&self.jobs, &self.query);
    }

    /// What the row says about the job beyond its name: how its fill went, or that it was opened.
    fn mark(&self, job: &Job, palette: &Palette, cx: &App) -> Option<Div> {
        let fill = self.browser.read(cx).fill_of(&Subject::Posting(job.address.clone()));
        match fill {
            Some(Fill::Running { .. }) => Some(ui::boxed("filling", palette.muted)),
            Some(Fill::Failed { .. }) => Some(ui::chip(Signal::Failed, "failed", palette)),
            Some(Fill::Done { report, .. }) => Some(match Tally::of(report) {
                Tally { failed: 1.., .. } => ui::chip(Signal::Failed, "failed", palette),
                Tally { yours: 0, .. } => ui::chip(Signal::Done, "filled", palette),
                Tally { yours, .. } => ui::chip(Signal::Yours, format!("{yours} yours"), palette),
            }),
            Some(Fill::Idle) | None => self
                .opened
                .contains(&job.address)
                .then(|| ui::chip(Signal::Done, "opened", palette)),
        }
    }
}

/// The application system a posting is on, as the user knows it.
pub fn system_name(address: &str) -> Option<&'static str> {
    Some(match System::of(address)? {
        System::Ashby => "Ashby",
        System::Greenhouse => "Greenhouse",
        System::Workday => "Workday",
    })
}

fn matching(jobs: &[Job], query: &str) -> Vec<Job> {
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    jobs.iter()
        .filter(|job| {
            let text = format!("{} {}", job.company, job.title).to_lowercase();
            words.iter().all(|word| text.contains(word))
        })
        .cloned()
        .collect()
}

pub(crate) fn job_row_id(row: usize) -> ElementId {
    ElementId::NamedInteger("job".into(), row as u64)
}

impl ListDelegate for JobList {
    type Item = ListItem;

    fn items_count(&self, _section: usize, _: &App) -> usize {
        self.matches.len()
    }

    fn perform_search(&mut self, query: &str, _: &mut Window, _: &mut Context<ListState<Self>>) -> Task<()> {
        self.query = query.to_owned();
        self.matches = matching(&self.jobs, &self.query);
        Task::ready(())
    }

    fn render_item(&mut self, ix: IndexPath, _: &mut Window, cx: &mut Context<ListState<Self>>) -> Option<ListItem> {
        let job = self.matches.get(ix.row)?;
        let palette = Palette::of(cx);
        let selected = self.selected == Some(ix);
        let tone = match job.score {
            _ if selected => palette.ink,
            Some(score) if score < 60 => palette.faint,
            _ => palette.muted,
        };
        let from = match system_name(&job.address) {
            Some(system) => format!("{} / {system}", job.company),
            None => job.company.clone(),
        };
        let row = h_flex()
            .gap_3()
            .child(ui::score(job.score, 19.).w(px(30.)).text_color(tone))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(ui::serif(job.title.clone()).truncate().text_color(tone))
                    .child(ui::caps(from).truncate().text_color(palette.faint)),
            )
            .children(self.mark(job, &palette, cx));
        Some(ui::row(job_row_id(ix.row), selected, row, &palette))
    }

    fn render_empty(&mut self, _: &mut Window, cx: &mut Context<ListState<Self>>) -> impl gpui_kit::IntoElement {
        let message = if self.jobs.is_empty() {
            "No open postings that Maverick can fill. Import the tracker to add some.".to_owned()
        } else {
            format!("No job matches \"{}\".", self.query)
        };
        ui::serif(message).p_4().text_color(Palette::of(cx).muted)
    }

    fn set_selected_index(&mut self, ix: Option<IndexPath>, _: &mut Window, cx: &mut Context<ListState<Self>>) {
        self.selected = ix;
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn posting(link: &str, title: &str, stage: Stage, score: Option<u8>) -> Posting {
        Posting {
            link: link.into(),
            found_at: "2026-11-01T00:00:00Z".parse().unwrap(),
            source: "Notify".into(),
            company: "Example Co".into(),
            title: title.into(),
            location: None,
            email_subject: None,
            stage,
            score,
            variant: None,
            flags: None,
            notes: None,
            address: None,
            sponsorship: None,
        }
    }

    #[test]
    fn open_jobs_are_fillable_postings_not_yet_applied_to_with_the_best_score_first() {
        let ashby = |id: u8| format!("https://jobs.ashbyhq.com/example-co/0f3b8231-3a32-478a-b994-3441f4bb94{id:02}");
        let wrapped = format!(
            "https://links.notify.careers/CL0/{}/1/0111-000/x=258",
            ashby(3).replace('/', "%2F")
        );
        let postings = [
            posting(&ashby(1), "Reviewed", Stage::Review, Some(66)),
            posting(&ashby(2), "Applied already", Stage::Applied, Some(90)),
            posting(&wrapped, "Wrapped link", Stage::Found, Some(82)),
            posting(&ashby(4), "Skipped", Stage::Skipped, Some(40)),
            posting(
                "https://boards.example.com/jobs/5",
                "Other system",
                Stage::Review,
                Some(95),
            ),
            posting(&ashby(6), "Not scored", Stage::Found, None),
            Posting {
                address: Some(format!("{}?utm_source=board", ashby(8))),
                ..posting("https://simplify.jobs/p/8", "Board copy", Stage::Review, Some(70))
            },
            posting(&ashby(1), "Reviewed again", Stage::Review, Some(66)),
            posting(
                &format!("{}?utm_source=x", ashby(7)),
                "Sent since the import",
                Stage::Review,
                Some(99),
            ),
        ];
        let sent = Application::submitted_today("Sent since the import", &format!("{}/application", ashby(7)));

        let jobs = open_jobs(&postings, &[sent]);
        let listed: Vec<(&str, &str)> = jobs
            .iter()
            .map(|job| (job.title.as_str(), job.address.as_str()))
            .collect();
        assert_eq!(
            listed,
            [
                ("Wrapped link", ashby(3).as_str()),
                ("Board copy", ashby(8).as_str()),
                ("Reviewed", ashby(1).as_str()),
                ("Not scored", ashby(6).as_str()),
            ]
        );
        assert_eq!(matching(&jobs, "example WRAPPED").len(), 1);
    }
}
