//! The list of postings Maverick can apply to.

use gpui_kit::component::list::{ListDelegate, ListItem, ListState};
use gpui_kit::component::{ActiveTheme as _, IndexPath, h_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{App, Context, ElementId, ParentElement as _, SharedString, Styled as _, Task, Window, div};
use maverick_core::apply;
use maverick_core::jobs::{Application, Posting, Stage, posting_address};

/// A posting whose form Maverick knows how to fill.
#[derive(Clone, Debug, PartialEq)]
pub struct Job {
    pub company: String,
    pub title: String,
    pub score: Option<u8>,
    /// The posting's own address, not a tracking link to it.
    pub address: String,
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
                address: apply::posting_address(&posting_address(&posting.link))?,
            })
        })
        .filter(|job| !applied.contains(&job.address))
        .collect();
    jobs.sort_by_key(|job| std::cmp::Reverse(job.score));
    jobs.dedup_by(|later, earlier| later.address == earlier.address);
    jobs
}

#[derive(Default)]
pub struct JobList {
    jobs: Vec<Job>,
    query: String,
    matches: Vec<Job>,
    selected: Option<IndexPath>,
    /// Addresses Maverick has opened in this run.
    opened: Vec<String>,
}

impl JobList {
    pub fn new(jobs: Vec<Job>) -> Self {
        Self {
            matches: jobs.clone(),
            jobs,
            ..Self::default()
        }
    }

    pub fn job_at(&self, ix: IndexPath) -> Option<&Job> {
        self.matches.get(ix.row)
    }

    pub fn len(&self) -> usize {
        self.jobs.len()
    }

    pub fn mark_opened(&mut self, address: &str) {
        self.opened.push(address.to_owned());
    }

    /// Takes a job off the list once its application went through.
    pub fn remove(&mut self, address: &str) {
        self.jobs.retain(|job| job.address != address);
        self.matches = matching(&self.jobs, &self.query);
    }
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
        let muted = cx.theme().muted_foreground;
        let row = h_flex()
            .gap_3()
            .child(
                div()
                    .flex_none()
                    .w_8()
                    .text_xs()
                    .text_color(muted)
                    .child(SharedString::from(
                        job.score.map(|score| score.to_string()).unwrap_or_default(),
                    )),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .child(SharedString::from(job.title.clone())),
            )
            .child(
                div()
                    .flex_none()
                    .max_w_2_5()
                    .truncate()
                    .text_xs()
                    .text_color(muted)
                    .child(SharedString::from(job.company.clone())),
            )
            .when(self.opened.contains(&job.address), |row| {
                row.child(
                    div()
                        .flex_none()
                        .text_xs()
                        .text_color(cx.theme().success)
                        .child("Opened"),
                )
            });
        Some(
            ListItem::new(job_row_id(ix.row))
                .selected(self.selected == Some(ix))
                .child(row),
        )
    }

    fn render_empty(&mut self, _: &mut Window, cx: &mut Context<ListState<Self>>) -> impl gpui_kit::IntoElement {
        let message = if self.jobs.is_empty() {
            "No open postings that Maverick can fill. Import the tracker to add some.".to_owned()
        } else {
            format!("No job matches \"{}\".", self.query)
        };
        div()
            .p_3()
            .text_color(cx.theme().muted_foreground)
            .child(SharedString::from(message))
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
                ("Reviewed", ashby(1).as_str()),
                ("Not scored", ashby(6).as_str()),
            ]
        );
        assert_eq!(matching(&jobs, "example WRAPPED").len(), 1);
    }
}
