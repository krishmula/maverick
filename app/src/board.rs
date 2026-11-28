//! The Jobs tab: the postings that wait on the user's Approve or Skip, the people to reach at each,
//! and the applications already sent.
//!
//! Decisions are kept in the local database, apart from the postings a sheet sync rewrites.
//! They never go to the sheet. Approving a posting puts it in the apply queue, which fills
//! several forms at once.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::PathBuf;

use crate::browser::Browser;
use crate::main_view::header;
use crate::outreach::{OutreachView, draft_state, person_line, verdict_signal};
use crate::queue::{Run, Step};
use crate::report;
use crate::tabs::host_of;
use crate::ui::{self, Palette, Signal};
use crate::verdict::Verdict;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::list::{List, ListDelegate, ListEvent, ListItem, ListState};
use gpui_kit::component::{IndexPath, h_flex, v_flex};
use gpui_kit::{
    AnyElement, App, AppContext as _, ClipboardItem, Context, Div, ElementId, Entity, FocusHandle, Focusable as _,
    InteractiveElement as _, IntoElement, KeyDownEvent, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, TestSupportExt as _, Window, div, px, relative,
};
use jiff::Timestamp;
use maverick_core::apply::{self, System};
use maverick_core::jobs::{Application, ApplicationStatus, Decision, Failure, Outreach, Posting, Sponsorship, Stage};
use maverick_core::outreach::Check;
use maverick_core::platform::{Platform, address_of};
use maverick_core::score::APPLY_READY;
use maverick_core::store::Store;
use serde::{Deserialize, Serialize};

/// A posting on the board.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    /// The posting's link, which its decision is kept under.
    pub link: String,
    pub company: String,
    pub title: String,
    pub score: Option<u8>,
    pub variant: Option<String>,
    /// What scoring flagged, blockers included.
    pub flags: Option<String>,
    /// What scoring said about the fit.
    pub notes: Option<String>,
    pub sponsorship: Sponsorship,
    pub location: Option<String>,
    pub found_at: Timestamp,
    pub source: String,
    /// The posting's own address when Maverick knows it, otherwise the link it was found by.
    pub address: String,
    /// Scoring found nothing that blocks an application.
    pub apply_ready: bool,
    pub decision: Option<Decision>,
}

impl Entry {
    /// The application system the posting is on, as the user knows it.
    pub fn system(&self) -> &'static str {
        Platform::of(&self.address).name()
    }

    /// The name an application is recorded under.
    pub fn role(&self) -> String {
        format!("{} at {}", self.title, self.company)
    }
}

/// Scoring found nothing that blocks an application. Muse worded the rest of that verdict differently.
fn is_apply_ready(notes: &str) -> bool {
    notes.starts_with(APPLY_READY) || notes.starts_with("Apply-ready")
}

/// What the board shows: the postings and the applications.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BoardData {
    pub entries: Vec<Entry>,
    pub applications: Vec<Application>,
    /// Why Maverick could not apply to an approved posting, with what it said, by posting link.
    pub failures: HashMap<String, (Failure, String)>,
}

impl BoardData {
    /// The postings still in play, plus any the user decided, without those already applied to.
    pub fn new(postings: &[Posting], applications: Vec<Application>, decisions: &HashMap<String, Decision>) -> Self {
        let own_address = |address: String| apply::posting_address(&address).unwrap_or(address);
        let applied: Vec<String> = applications
            .iter()
            .filter_map(|sent| Some(own_address(sent.link.clone()?)))
            .collect();
        let mut entries: Vec<Entry> = postings
            .iter()
            .filter(|posting| {
                posting.stage != Stage::Applied
                    && (matches!(posting.stage, Stage::Found | Stage::Review) || decisions.contains_key(&posting.link))
            })
            .map(|posting| Entry {
                link: posting.link.clone(),
                company: posting.company.clone(),
                title: posting.title.clone(),
                score: posting.score,
                variant: posting.variant.clone(),
                flags: posting.flags.clone(),
                notes: posting.notes.clone(),
                sponsorship: posting.sponsorship.unwrap_or(Sponsorship::Unknown),
                location: posting.location.clone(),
                found_at: posting.found_at,
                source: posting.source.clone(),
                address: address_of(posting),
                apply_ready: posting.notes.as_deref().is_some_and(is_apply_ready),
                decision: decisions.get(&posting.link).copied(),
            })
            .filter(|entry| !applied.contains(&entry.address))
            .collect();
        // Two links can lead to one posting. A decided one stands for it, so its decision shows.
        entries.sort_by_key(|entry| entry.decision.is_none());
        let mut seen = std::collections::HashSet::new();
        entries.retain(|entry| seen.insert(entry.address.clone()));
        Self {
            entries,
            applications,
            failures: HashMap::new(),
        }
    }
}

/// How the pending postings are ordered.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Order {
    /// The most recently found first.
    Latest,
    /// Apply-ready first, then the best score.
    TopScore,
    /// Postings with someone to reach first, then as by top score.
    Outreach,
}

impl Order {
    const ALL: [Order; 3] = [Order::Latest, Order::TopScore, Order::Outreach];

    fn id(self) -> &'static str {
        match self {
            Order::Latest => "sort-latest",
            Order::TopScore => "sort-score",
            Order::Outreach => "sort-outreach",
        }
    }

    fn word(self) -> &'static str {
        match self {
            Order::Latest => "latest",
            Order::TopScore => "top score",
            Order::Outreach => "outreach",
        }
    }
}

/// How the applications are ordered.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SubmittedOrder {
    /// The most recently applied first.
    Latest,
    Company,
}

/// Puts the entries in `order`. `reachable` holds the links of postings with someone to reach.
/// Decided entries always come after the pending ones.
pub fn sort(entries: &mut [Entry], order: Order, reachable: &HashSet<String>) {
    let top_score = |a: &Entry, b: &Entry| {
        b.apply_ready
            .cmp(&a.apply_ready)
            .then(b.score.cmp(&a.score))
            .then(b.found_at.cmp(&a.found_at))
    };
    entries.sort_by(|a, b| {
        let pending_first = a.decision.is_some().cmp(&b.decision.is_some());
        pending_first.then_with(|| match order {
            Order::TopScore => top_score(a, b),
            Order::Latest => b.found_at.cmp(&a.found_at).then(b.score.cmp(&a.score)),
            Order::Outreach => reachable
                .contains(&b.link)
                .cmp(&reachable.contains(&a.link))
                .then_with(|| top_score(a, b)),
        })
    });
}

/// The company an application was sent to. Maverick records its own as "title at company".
pub fn company_of(sent: &Application) -> String {
    let role = sent.role.as_deref().unwrap_or_default();
    let ours = sent.sheet_row.is_none();
    match role.rsplit_once(" at ") {
        Some((_, company)) if ours => company.to_owned(),
        _ => role.to_owned(),
    }
}

pub fn sort_submitted(applications: &mut [Application], order: SubmittedOrder) {
    match order {
        SubmittedOrder::Latest => applications.sort_by_key(|sent| std::cmp::Reverse(sent.applied_on)),
        SubmittedOrder::Company => applications
            .sort_by_cached_key(|sent| (company_of(sent).to_lowercase(), std::cmp::Reverse(sent.applied_on))),
    }
}

/// Which companies the pending list shows.
#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
pub struct CompanyFilter {
    /// Companies kept out of the list, as their postings name them. They stay hidden after a restart.
    pub hidden: BTreeSet<String>,
    /// The one company the list shows, until the user shows all again.
    #[serde(skip)]
    pub only: Option<String>,
}

impl CompanyFilter {
    pub fn shows(&self, company: &str) -> bool {
        let same = |name: &String| name.trim().eq_ignore_ascii_case(company.trim());
        match &self.only {
            Some(only) => same(only),
            None => !self.hidden.iter().any(same),
        }
    }

    pub fn is_active(&self) -> bool {
        self.only.is_some() || !self.hidden.is_empty()
    }

    /// The filter saved in `file`, or none when there is no file or it cannot be read.
    pub fn load(file: Option<&std::path::Path>) -> Self {
        file.and_then(|file| std::fs::read(file).ok())
            .and_then(|data| serde_json::from_slice(&data).ok())
            .unwrap_or_default()
    }

    fn save(&self, file: &std::path::Path) -> std::io::Result<()> {
        let data = serde_json::to_vec_pretty(self).expect("the company filter serializes");
        std::fs::write(file, data)
    }
}

/// What the search box asks for. Commas separate alternatives, and every word of one
/// alternative must appear in a posting's company, title, location or application system.
/// "amazon, apple ml" finds Amazon postings and Apple postings that mention ML.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Search(Vec<Vec<String>>);

impl Search {
    pub fn new(text: &str) -> Self {
        Self(
            text.split(',')
                .map(|alternative| {
                    alternative
                        .split_whitespace()
                        .map(str::to_lowercase)
                        .collect::<Vec<_>>()
                })
                .filter(|words| !words.is_empty())
                .collect(),
        )
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn finds(&self, text: &str) -> bool {
        let text = text.to_lowercase();
        self.is_empty() || self.0.iter().any(|words| words.iter().all(|word| text.contains(word)))
    }

    pub fn finds_entry(&self, entry: &Entry) -> bool {
        let location = entry.location.as_deref().unwrap_or_default();
        self.finds(&format!(
            "{} {} {location} {}",
            entry.company,
            entry.title,
            entry.system()
        ))
    }

    pub fn finds_application(&self, sent: &Application) -> bool {
        let role = sent.role.as_deref().unwrap_or_default();
        let host = sent.link.as_deref().map(host_of).unwrap_or_default();
        self.finds(&format!("{role} {host}"))
    }
}

/// Which list the board shows.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Section {
    Pending,
    /// Postings set aside for Run queue, and those Maverick is applying to or waits on the user for.
    Queue,
    /// Approved postings Maverick could not apply to.
    Errors,
    Submitted,
}

pub struct Rows {
    browser: Entity<Browser>,
    outreach: Entity<OutreachView>,
    section: Section,
    /// The pending postings the company filter and the search show.
    entries: Vec<Entry>,
    /// The pending postings the company filter or the search keeps out.
    filtered_out: Vec<Entry>,
    /// The applications the search shows.
    submitted: Vec<Application>,
    /// The applications the search keeps out.
    submitted_out: Vec<Application>,
    search: Search,
    selected: Option<IndexPath>,
    /// The postings marked to decide together, by link.
    marked: BTreeSet<String>,
    /// The addresses of the postings the apply queue is working on or waits on the user for.
    active: HashSet<String>,
    /// Why Maverick could not apply to an approved posting, with what it said, by posting link.
    failures: HashMap<String, (Failure, String)>,
    /// The one kind of failure Errors shows, until the user shows them all again.
    failure_filter: Option<Failure>,
}

impl Rows {
    /// Every posting on the board, shown or filtered out.
    fn all(&self) -> impl Iterator<Item = &Entry> {
        self.entries.iter().chain(&self.filtered_out)
    }

    /// Splits the postings again into those `filter` and the search show and those they keep out,
    /// and the applications into those the search shows and the rest. A search looks past `filter`,
    /// since typing a company's name asks for it even when it is hidden.
    fn refilter(&mut self, filter: &CompanyFilter) {
        let mut all = std::mem::take(&mut self.entries);
        all.append(&mut self.filtered_out);
        let search = &self.search;
        let section = match self.section {
            Section::Submitted => Section::Pending,
            section => section,
        };
        (self.entries, self.filtered_out) = all.into_iter().partition(|entry| {
            (!search.is_empty() || filter.shows(&entry.company))
                && search.finds_entry(entry)
                && self.section_of(entry) == section
                && (section != Section::Errors
                    || self.failure_filter.is_none()
                    || self.failures.get(&entry.link).map(|(failure, _)| *failure) == self.failure_filter)
        });
        let mut sent = std::mem::take(&mut self.submitted);
        sent.append(&mut self.submitted_out);
        (self.submitted, self.submitted_out) = sent.into_iter().partition(|sent| search.finds_application(sent));
    }

    fn shown(&self) -> usize {
        match self.section {
            Section::Submitted => self.submitted.len(),
            _ => self.entries.len(),
        }
    }

    /// The list a posting belongs in: the queue while it is set aside or being applied to,
    /// Errors when Maverick could not apply to it, and Pending otherwise.
    fn section_of(&self, entry: &Entry) -> Section {
        match entry.decision {
            Some(Decision::Queued) => Section::Queue,
            Some(Decision::Approved) if self.active.contains(&entry.address) => Section::Queue,
            Some(Decision::Approved) if self.failures.contains_key(&entry.link) => Section::Errors,
            _ => Section::Pending,
        }
    }

    /// How many postings each list holds, those the search and the company filter keep out included.
    fn count(&self, section: Section) -> usize {
        self.all()
            .filter(|entry| self.section_of(entry) == section)
            .filter(|entry| section != Section::Pending || entry.decision.is_none())
            .count()
    }

    /// Marks the selected posting, or unmarks it when it was marked.
    fn toggle_mark(&mut self) {
        let Some(link) = self.selected_entry().map(|entry| entry.link.clone()) else {
            return;
        };
        if !self.marked.remove(&link) {
            self.marked.insert(link);
        }
    }

    fn selected_entry(&self) -> Option<&Entry> {
        (self.section != Section::Submitted)
            .then(|| self.entries.get(self.selected?.row))
            .flatten()
    }

    fn selected_application(&self) -> Option<&Application> {
        (self.section == Section::Submitted)
            .then(|| self.submitted.get(self.selected?.row))
            .flatten()
    }
}

pub(crate) fn board_row_id(row: usize) -> ElementId {
    ElementId::NamedInteger("board-row".into(), row as u64)
}

/// The badge for what a posting says about visa sponsorship.
fn sponsorship_chip(sponsorship: Sponsorship, palette: &Palette) -> Div {
    match sponsorship {
        Sponsorship::Sponsors => ui::chip(Signal::Done, "sponsors", palette),
        Sponsorship::NoSponsorship => ui::chip(Signal::Failed, "no sponsorship", palette),
        Sponsorship::Unknown => ui::boxed("sponsorship unknown", palette.faint),
    }
}

/// Where a queued posting is, in the signal that says whose move it is.
fn step_chip(step: &Step, palette: &Palette) -> Div {
    match step {
        Step::Queued | Step::Opening | Step::Filling | Step::Submitting => ui::boxed(step.word(), palette.muted),
        Step::NeedsYou(_) | Step::Ready => ui::chip(Signal::Yours, step.word(), palette),
        Step::Applied => ui::chip(Signal::Done, step.word(), palette),
        Step::Failed(..) => ui::chip(Signal::Failed, step.word(), palette),
    }
}

/// The card's line on whom to reach: the people found, or why there is no one.
fn reach_line(check: Option<&Check>, palette: &Palette) -> Div {
    let Some(check) = check else {
        return ui::caps("outreach not checked yet")
            .truncate()
            .text_color(palette.faint);
    };
    let Some(first) = check.people.first().filter(|_| check.verdict.found_people()) else {
        let said = match check.verdict {
            maverick_core::outreach::Verdict::Deferred => "outreach: lookup deferred",
            _ => "outreach: no one, apply cold",
        };
        return ui::caps(said).truncate().text_color(palette.faint);
    };
    let more = match check.people.len() {
        1 => String::new(),
        count => format!("  +{}", count - 1),
    };
    h_flex()
        .min_w_0()
        .gap(px(7.))
        .child(ui::square(palette.signal(verdict_signal(check.verdict))))
        .child(
            ui::caps(format!("reach {}{more}", person_line(first)))
                .truncate()
                .text_color(palette.muted),
        )
}

fn decision_chip(decision: Decision, palette: &Palette) -> Div {
    match decision {
        Decision::Approved => ui::chip(Signal::Done, "approved", palette),
        Decision::Skipped => ui::boxed("skipped", palette.faint),
        Decision::Queued => ui::boxed("queued", palette.muted),
    }
}

/// Whether the user can mark a posting submitted: nothing is filling it and no submit is recorded.
fn can_mark_submitted(run: Option<&Step>) -> bool {
    !matches!(
        run,
        Some(Step::Opening | Step::Filling | Step::Submitting | Step::Applied)
    )
}

impl ListDelegate for Rows {
    type Item = ListItem;

    fn items_count(&self, _section: usize, _: &App) -> usize {
        self.shown()
    }

    fn render_item(&mut self, ix: IndexPath, _: &mut Window, cx: &mut Context<ListState<Self>>) -> Option<ListItem> {
        let palette = Palette::of(cx);
        let selected = self.selected == Some(ix);
        let row = match self.section {
            Section::Pending | Section::Queue | Section::Errors => {
                let entry = self.entries.get(ix.row)?;
                let run = self.browser.read(cx).queue().get(&entry.address).cloned();
                let tone = match (entry.decision, entry.score) {
                    _ if selected => palette.ink,
                    (Some(_), _) => palette.faint,
                    (None, Some(score)) if score < 60 => palette.faint,
                    _ => palette.muted,
                };
                let company_tone = if selected { palette.ink } else { palette.muted };
                let mut more = vec![entry.found_at.strftime("%b %-d").to_string()];
                more.extend(entry.location.clone());
                let outreach = self.outreach.read(cx);
                let reach = reach_line(outreach.check(&entry.link), &palette);
                let draft = outreach.draft_status(&entry.link).map(|status| {
                    let (signal, word) = draft_state(status);
                    let word = if status == Outreach::Typed { "typed" } else { word };
                    ui::chip(signal, format!("note {word}"), &palette)
                });
                let marked = self.marked.contains(&entry.link);
                h_flex()
                    .gap_3()
                    .child(
                        div()
                            .flex_none()
                            .w(px(6.))
                            .children(marked.then(|| ui::square(palette.signal(Signal::Yours)))),
                    )
                    .child(ui::score(entry.score, 19.).w(px(30.)).text_color(tone))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(ui::serif(entry.title.clone()).truncate().text_color(tone))
                            .child(
                                h_flex()
                                    .min_w_0()
                                    .gap(px(7.))
                                    .py(px(2.))
                                    .child(
                                        ui::caps(&entry.company)
                                            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                            .truncate()
                                            .text_color(company_tone),
                                    )
                                    .child(ui::caps("on").flex_none().text_color(palette.faint))
                                    .child(ui::caps(entry.system()).flex_none().text_color(palette.muted)),
                            )
                            .child(ui::caps(more.join(" · ")).truncate().text_color(palette.faint))
                            .child(reach),
                    )
                    .children(draft)
                    .children(
                        entry
                            .apply_ready
                            .then(|| ui::chip(Signal::Yours, "apply-ready", &palette)),
                    )
                    .child(sponsorship_chip(entry.sponsorship, &palette))
                    .children(match (&run, entry.decision) {
                        (_, Some(Decision::Approved)) if self.section == Section::Errors => self
                            .failures
                            .get(&entry.link)
                            .map(|(failure, _)| ui::chip(Signal::Failed, failure.label(), &palette)),
                        (Some(run), Some(Decision::Approved)) => Some(step_chip(&run.step, &palette)),
                        (_, decision) => decision.map(|decision| decision_chip(decision, &palette)),
                    })
            }
            Section::Submitted => {
                let sent = self.submitted.get(ix.row)?;
                let tone = if selected { palette.ink } else { palette.muted };
                h_flex()
                    .gap_3()
                    .child(
                        ui::caps(sent.applied_on.strftime("%b %-d").to_string())
                            .w(px(52.))
                            .text_color(palette.faint),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                ui::serif(sent.role.clone().unwrap_or_else(|| "(no company in the sheet)".into()))
                                    .truncate()
                                    .text_color(tone),
                            )
                            .child(
                                ui::caps(sent.link.as_deref().map(host_of).unwrap_or_default())
                                    .truncate()
                                    .text_color(palette.faint),
                            ),
                    )
                    .children(
                        (sent.status == ApplicationStatus::Rejected)
                            .then(|| ui::chip(Signal::Failed, "rejected", &palette)),
                    )
            }
        };
        Some(ui::row(board_row_id(ix.row), selected, row, &palette))
    }

    fn render_empty(&mut self, _: &mut Window, cx: &mut Context<ListState<Self>>) -> impl IntoElement {
        let searching = !self.search.is_empty();
        let message = match self.section {
            Section::Pending if searching && !self.filtered_out.is_empty() => "No posting matches the search.",
            Section::Pending if !self.filtered_out.is_empty() => "The company filter hides every posting.",
            Section::Pending => "Nothing waits on a decision. New postings arrive with the hourly check.",
            Section::Queue if searching && !self.filtered_out.is_empty() => {
                "No posting in the queue matches the search."
            }
            Section::Queue => "The queue is empty. Add to queue sets a posting aside for Run queue.",
            Section::Errors if self.failure_filter.is_some() => "No posting failed this way.",
            Section::Errors => "Maverick applied to every posting you approved, or is still at it.",
            Section::Submitted if searching && !self.submitted_out.is_empty() => "No application matches the search.",
            Section::Submitted => "No applications are recorded yet.",
        };
        ui::serif(message).p_4().text_color(Palette::of(cx).muted)
    }

    fn set_selected_index(&mut self, ix: Option<IndexPath>, _: &mut Window, cx: &mut Context<ListState<Self>>) {
        self.selected = ix;
        cx.notify();
    }

    /// Command-click marks or unmarks a posting.
    fn confirm(&mut self, secondary: bool, _: &mut Window, cx: &mut Context<ListState<Self>>) {
        if secondary {
            self.toggle_mark();
            cx.notify();
        }
    }
}

pub struct Board {
    browser: Entity<Browser>,
    outreach: Entity<OutreachView>,
    database: Option<PathBuf>,
    list: Entity<ListState<Rows>>,
    order: Order,
    submitted_order: SubmittedOrder,
    filter: CompanyFilter,
    /// The search box over both lists.
    search: Entity<InputState>,
    /// Where the hidden companies are saved.
    filter_file: Option<PathBuf>,
    /// Each batch of decisions made in this run, newest last, with the decision each replaced.
    made: Vec<Vec<(String, Option<Decision>)>>,
    /// Why the last decision could not be saved.
    problem: Option<String>,
    focus: FocusHandle,
    /// How many of the applications sent in this run the board has moved to Submitted.
    moved: usize,
    /// The address last copied, so its button can say so.
    copied: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl Board {
    pub fn new(
        browser: Entity<Browser>,
        outreach: Entity<OutreachView>,
        data: BoardData,
        database: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let rows = Rows {
            browser: browser.clone(),
            outreach: outreach.clone(),
            section: Section::Pending,
            entries: Vec::new(),
            filtered_out: Vec::new(),
            submitted: Vec::new(),
            submitted_out: Vec::new(),
            search: Search::default(),
            selected: None,
            marked: BTreeSet::new(),
            active: HashSet::new(),
            failures: HashMap::new(),
            failure_filter: None,
        };
        let list = cx.new(|cx| ListState::new(rows, window, cx));
        let search = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Search: amazon, apple")
                .clean_on_escape()
        });
        let _subscriptions = vec![
            cx.subscribe_in(
                &search,
                window,
                |this, input, event: &InputEvent, window, cx| match event {
                    InputEvent::Change => {
                        let text = input.read(cx).value();
                        this.set_search(Search::new(&text), window, cx);
                    }
                    // Enter hands the keys back to the list, so a and s decide the postings found.
                    InputEvent::PressEnter { .. } => this.focus(window, cx),
                    _ => {}
                },
            ),
            cx.subscribe_in(&list, window, |this, _, event: &ListEvent, _, cx| {
                // Enter on a posting with an open tab brings the tab forward.
                if let ListEvent::Confirm(_) = event {
                    this.show_tab(cx);
                }
                cx.notify();
            }),
            cx.observe_in(&browser, window, |this, _, window, cx| {
                this.move_applied(window, cx);
                this.follow_runs(window, cx);
            }),
            // A check or a draft changes what the cards say about outreach.
            cx.observe(&outreach, |this, _, cx| {
                this.list.update(cx, |_, cx| cx.notify());
                cx.notify();
            }),
        ];
        let filter_file = database
            .as_ref()
            .and_then(|database| Some(database.parent()?.join("company-filter.json")));
        let mut board = Self {
            filter: CompanyFilter::load(filter_file.as_deref()),
            search,
            filter_file,
            browser,
            outreach,
            database,
            list,
            order: Order::TopScore,
            submitted_order: SubmittedOrder::Latest,
            made: Vec::new(),
            problem: None,
            focus: cx.focus_handle(),
            moved: 0,
            copied: None,
            _subscriptions,
        };
        board.replace(data, window, cx);
        board
    }

    /// Shows what the database holds now, keeping the section and the orders.
    pub fn replace(&mut self, data: BoardData, window: &mut Window, cx: &mut Context<Self>) {
        let (order, submitted_order) = (self.order, self.submitted_order);
        let filter = self.filter.clone();
        let reachable = self.outreach.read(cx).reachable();
        self.list.update(cx, |list, cx| {
            let rows = list.delegate_mut();
            rows.entries = data.entries;
            rows.filtered_out.clear();
            rows.failures = data.failures;
            rows.submitted = data.applications;
            rows.submitted_out.clear();
            rows.refilter(&filter);
            sort(&mut rows.entries, order, &reachable);
            sort_submitted(&mut rows.submitted, submitted_order);
            select_row(list, 0, window, cx);
        });
        cx.notify();
    }

    /// How many postings wait on a decision, those the company filter keeps out included.
    pub fn pending(&self, cx: &App) -> usize {
        let rows = self.list.read(cx).delegate();
        rows.all().filter(|entry| entry.decision.is_none()).count()
    }

    /// The selected posting, when the pending list shows.
    pub fn selected_entry<'a>(&self, cx: &'a App) -> Option<&'a Entry> {
        self.list.read(cx).delegate().selected_entry()
    }

    /// The best pending postings, without the one at `except`.
    pub fn best<'a>(&self, count: usize, except: Option<&str>, cx: &'a App) -> Vec<&'a Entry> {
        let mut pending: Vec<&Entry> = self
            .list
            .read(cx)
            .delegate()
            .all()
            .filter(|entry| entry.decision.is_none() && Some(entry.address.as_str()) != except)
            .collect();
        pending.sort_by_key(|entry| std::cmp::Reverse(entry.score));
        pending.truncate(count);
        pending
    }

    pub fn focus(&self, window: &mut Window, cx: &mut App) {
        self.list.update(cx, |list, cx| list.focus(window, cx));
    }

    fn show(&mut self, section: Section, window: &mut Window, cx: &mut Context<Self>) {
        let (order, filter) = (self.order, self.filter.clone());
        let reachable = self.outreach.read(cx).reachable();
        self.list.update(cx, |list, cx| {
            let rows = list.delegate_mut();
            rows.section = section;
            rows.refilter(&filter);
            sort(&mut rows.entries, order, &reachable);
            select_row(list, 0, window, cx);
        });
        self.focus(window, cx);
        cx.notify();
    }

    /// Shows the errors of one kind, or all of them with `None`.
    fn filter_failures(&mut self, failure: Option<Failure>, window: &mut Window, cx: &mut Context<Self>) {
        self.list
            .update(cx, |list, _| list.delegate_mut().failure_filter = failure);
        self.regroup(window, cx);
    }

    fn set_order(&mut self, order: Order, window: &mut Window, cx: &mut Context<Self>) {
        self.order = order;
        let reachable = self.outreach.read(cx).reachable();
        self.list.update(cx, |list, cx| {
            sort(&mut list.delegate_mut().entries, order, &reachable);
            select_row(list, 0, window, cx);
        });
        cx.notify();
    }

    fn set_submitted_order(&mut self, order: SubmittedOrder, window: &mut Window, cx: &mut Context<Self>) {
        self.submitted_order = order;
        self.list.update(cx, |list, cx| {
            sort_submitted(&mut list.delegate_mut().submitted, order);
            select_row(list, 0, window, cx);
        });
        cx.notify();
    }

    /// Shows the postings `filter` lets through, keeping the selection at `row`, and saves the hidden companies.
    fn set_filter(&mut self, filter: CompanyFilter, row: usize, window: &mut Window, cx: &mut Context<Self>) {
        if filter.hidden != self.filter.hidden
            && let Some(file) = &self.filter_file
        {
            self.problem = filter
                .save(file)
                .err()
                .map(|error| format!("The hidden companies were not saved: {error}"));
        }
        self.filter = filter;
        let (order, filter) = (self.order, self.filter.clone());
        let reachable = self.outreach.read(cx).reachable();
        self.list.update(cx, |list, cx| {
            let rows = list.delegate_mut();
            rows.refilter(&filter);
            sort(&mut rows.entries, order, &reachable);
            select_row(list, row, window, cx);
        });
        cx.notify();
    }

    /// Shows the postings and applications `search` finds, in the section on screen.
    fn set_search(&mut self, search: Search, window: &mut Window, cx: &mut Context<Self>) {
        let (order, submitted_order, filter) = (self.order, self.submitted_order, self.filter.clone());
        let reachable = self.outreach.read(cx).reachable();
        self.list.update(cx, |list, cx| {
            let rows = list.delegate_mut();
            rows.search = search;
            rows.refilter(&filter);
            sort(&mut rows.entries, order, &reachable);
            sort_submitted(&mut rows.submitted, submitted_order);
            select_row(list, 0, window, cx);
        });
        cx.notify();
    }

    fn focus_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.search.update(cx, |search, cx| search.focus(window, cx));
    }

    /// Keeps the selected posting's company out of the list. The selection lands on the next posting.
    fn hide_company(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let list = self.list.read(cx);
        let row = list.selected_index().map_or(0, |ix| ix.row);
        let Some(company) = list.delegate().selected_entry().map(|entry| entry.company.clone()) else {
            return;
        };
        let mut filter = self.filter.clone();
        filter.only = None;
        filter.hidden.insert(company);
        self.set_filter(filter, row, window, cx);
    }

    /// Shows only the selected posting's company.
    fn only_company(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(company) = self.selected_entry(cx).map(|entry| entry.company.clone()) else {
            return;
        };
        let filter = CompanyFilter {
            only: Some(company),
            ..self.filter.clone()
        };
        self.set_filter(filter, 0, window, cx);
    }

    /// Decides the marked postings, or the selected one when none is marked. The selection stays
    /// in place, so it lands on the next pending one.
    pub fn decide(&mut self, decision: Decision, window: &mut Window, cx: &mut Context<Self>) {
        let links = self.list.update(cx, |list, _| {
            let rows = list.delegate_mut();
            let mut links: Vec<String> = std::mem::take(&mut rows.marked).into_iter().collect();
            if links.is_empty() {
                links.extend(rows.selected_entry().map(|entry| entry.link.clone()));
            }
            links
        });
        self.decide_links(&links, decision, window, cx);
    }

    /// Approves every posting in the queue, as one batch that undo takes back together.
    fn run_queue(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let links: Vec<String> = self
            .list
            .read(cx)
            .delegate()
            .all()
            .filter(|entry| entry.decision == Some(Decision::Queued))
            .map(|entry| entry.link.clone())
            .collect();
        self.decide_links(&links, Decision::Approved, window, cx);
    }

    /// Records that the user submitted the selected posting's application themselves.
    fn mark_submitted(&mut self, cx: &mut Context<Self>) {
        let Some(entry) = self.selected_entry(cx).cloned() else {
            return;
        };
        let run = self
            .browser
            .read(cx)
            .queue()
            .get(&entry.address)
            .map(|run| run.step.clone());
        if !can_mark_submitted(run.as_ref()) {
            return;
        }
        self.browser.update(cx, |browser, cx| {
            browser.record_submitted(entry.role(), entry.address, cx)
        });
    }

    fn decide_links(&mut self, links: &[String], decision: Decision, window: &mut Window, cx: &mut Context<Self>) {
        let order = self.order;
        let reachable = self.outreach.read(cx).reachable();
        let decided = self.list.update(cx, |list, cx| {
            let row = list.selected_index().map_or(0, |ix| ix.row);
            let rows = list.delegate_mut();
            let mut made = Vec::new();
            for entry in rows.entries.iter_mut().chain(&mut rows.filtered_out) {
                if links.contains(&entry.link) {
                    made.push((entry.link.clone(), entry.decision.replace(decision)));
                }
            }
            sort(&mut rows.entries, order, &reachable);
            select_row(list, row, window, cx);
            made
        });
        if !decided.is_empty() {
            let saves = decided.iter().map(|(link, _)| (link.clone(), Some(decision))).collect();
            self.save(saves, cx);
            for (link, _) in &decided {
                // A new decision, another try included, sets the last failure aside.
                self.set_failure(link, None, cx);
                self.sync_queue(link, Some(decision), cx);
            }
            self.made.push(decided);
        }
        self.follow_runs(window, cx);
        cx.notify();
    }

    /// Keeps the lists in step with the apply queue: a posting moves to the queue while Maverick
    /// works on it, and to Errors when Maverick could not apply to it.
    fn follow_runs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (active, failed): (HashSet<String>, Vec<(String, Failure, String)>) = {
            let runs = self.browser.read(cx).queue().runs();
            let active = runs
                .iter()
                .filter(|run| !matches!(run.step, Step::Failed(..) | Step::Applied))
                .map(|run| run.address.clone())
                .collect();
            let failed = runs
                .iter()
                .filter_map(|run| match &run.step {
                    Step::Failed(failure, why) => Some((run.address.clone(), *failure, why.clone())),
                    _ => None,
                })
                .collect();
            (active, failed)
        };
        let rows = self.list.read(cx).delegate();
        let news: Vec<(String, (Failure, String))> = failed
            .into_iter()
            .filter_map(|(address, failure, why)| {
                let entry = rows
                    .all()
                    .find(|entry| entry.address == address && entry.decision == Some(Decision::Approved))?;
                let said = (failure, why);
                (rows.failures.get(&entry.link) != Some(&said)).then(|| (entry.link.clone(), said))
            })
            .collect();
        let moved = rows.active != active || !news.is_empty();
        self.list.update(cx, |list, _| list.delegate_mut().active = active);
        for (link, said) in news {
            self.set_failure(&link, Some(said), cx);
        }
        if moved {
            self.regroup(window, cx);
        }
    }

    /// Sorts the postings into the lists again, keeping the selection's place.
    fn regroup(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (order, filter) = (self.order, self.filter.clone());
        let reachable = self.outreach.read(cx).reachable();
        self.list.update(cx, |list, cx| {
            let row = list.selected_index().map_or(0, |ix| ix.row);
            let rows = list.delegate_mut();
            rows.refilter(&filter);
            sort(&mut rows.entries, order, &reachable);
            select_row(list, row, window, cx);
        });
        cx.notify();
    }

    /// Records why Maverick could not apply to the posting at `link`, or clears it with `None`.
    fn set_failure(&mut self, link: &str, failure: Option<(Failure, String)>, cx: &mut Context<Self>) {
        let changed = self.list.update(cx, |list, _| {
            let failures = &mut list.delegate_mut().failures;
            match &failure {
                Some(said) => failures.insert(link.to_owned(), said.clone()).as_ref() != Some(said),
                None => failures.remove(link).is_some(),
            }
        });
        if !changed {
            return;
        }
        let (database, link) = (self.database.clone(), link.to_owned());
        let saved = self.browser.read(cx).background().spawn_blocking(move || {
            let database = database.ok_or("this system has no application data directory")?;
            let mut store = Store::open(&database).map_err(|error| error.to_string())?;
            match failure {
                Some((failure, detail)) => store.record_failure(&link, failure, &detail),
                None => store.clear_failure(&link),
            }
            .map_err(|error| error.to_string())
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Err(reason)) = saved.await {
                this.update(cx, |this, cx| {
                    this.problem = Some(format!("The error was not saved: {reason}"));
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    /// Marks the selected posting, or unmarks it, and moves to the next one.
    fn toggle_mark(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.list.update(cx, |list, cx| {
            let row = list.selected_index().map_or(0, |ix| ix.row);
            list.delegate_mut().toggle_mark();
            select_row(list, row + 1, window, cx);
        });
        cx.notify();
    }

    /// Marks every posting the list shows that waits on a decision.
    fn mark_all(&mut self, cx: &mut Context<Self>) {
        self.list.update(cx, |list, cx| {
            let rows = list.delegate_mut();
            let pending: Vec<String> = rows
                .entries
                .iter()
                .filter(|entry| entry.decision.is_none())
                .map(|entry| entry.link.clone())
                .collect();
            rows.marked.extend(pending);
            cx.notify();
        });
        cx.notify();
    }

    fn clear_marks(&mut self, cx: &mut Context<Self>) {
        self.list.update(cx, |list, cx| {
            list.delegate_mut().marked.clear();
            cx.notify();
        });
        cx.notify();
    }

    fn marked(&self, cx: &App) -> usize {
        self.list.read(cx).delegate().marked.len()
    }

    /// Starts filling an approved posting Maverick can fill, and takes any other out while it waits.
    /// A posting on a site Maverick cannot fill opens nothing: the user applies there themselves.
    fn sync_queue(&mut self, link: &str, decision: Option<Decision>, cx: &mut Context<Self>) {
        let rows = self.list.read(cx).delegate();
        let Some(entry) = rows.all().find(|entry| entry.link == link) else {
            return;
        };
        let (role, address) = (entry.role(), entry.address.clone());
        let fillable = System::of(&address).is_some();
        if decision == Some(Decision::Approved) && !fillable {
            let platform = Platform::of(&address).name();
            let said = format!("Maverick cannot fill {platform} forms yet. Apply on the site, then mark it submitted.");
            self.set_failure(link, Some((Failure::Unfillable, said)), cx);
        }
        self.browser.update(cx, |browser, cx| match decision {
            Some(Decision::Approved) if fillable => browser.enqueue(role, address, cx),
            _ => browser.withdraw(&address, cx),
        });
    }

    /// Brings the selected posting's tab forward, when the queue opened one.
    fn show_tab(&mut self, cx: &mut Context<Self>) {
        if let Some(entry) = self.list.read(cx).delegate().selected_entry().cloned() {
            self.browser
                .update(cx, |browser, cx| browser.show_run(&entry.address, cx));
        }
    }

    /// Moves each posting whose application went through to the Submitted list.
    fn move_applied(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let applied = self.browser.read(cx).applied();
        let new: Vec<_> = applied[self.moved.min(applied.len())..].to_vec();
        self.moved = applied.len();
        if new.is_empty() {
            cx.notify();
            return;
        }
        let (submitted_order, filter) = (self.submitted_order, self.filter.clone());
        self.list.update(cx, |list, cx| {
            let row = list.selected_index().map_or(0, |ix| ix.row);
            let rows = list.delegate_mut();
            for sent in new {
                rows.entries.retain(|entry| entry.address != sent.address);
                rows.filtered_out.retain(|entry| entry.address != sent.address);
                rows.submitted
                    .push(Application::submitted_today(&sent.title, &sent.address));
            }
            rows.refilter(&filter);
            sort_submitted(&mut rows.submitted, submitted_order);
            select_row(list, row, window, cx);
        });
        cx.notify();
    }

    /// Takes back the last batch of decisions and selects its first posting again.
    pub fn undo(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(batch) = self.made.pop() else {
            return;
        };
        self.list.update(cx, |list, _| {
            let rows = list.delegate_mut();
            for entry in rows.entries.iter_mut().chain(&mut rows.filtered_out) {
                if let Some((_, before)) = batch.iter().find(|(link, _)| *link == entry.link) {
                    entry.decision = *before;
                }
            }
        });
        self.save(batch.clone(), cx);
        for (link, before) in &batch {
            self.set_failure(link, None, cx);
            self.sync_queue(link, *before, cx);
        }
        self.follow_runs(window, cx);
        // The list that holds the first posting taken back shows, with that posting selected.
        let (order, filter) = (self.order, self.filter.clone());
        let reachable = self.outreach.read(cx).reachable();
        self.list.update(cx, |list, cx| {
            let rows = list.delegate_mut();
            let first = batch.first().map(|(link, _)| link);
            let restored = rows
                .all()
                .find(|entry| Some(&entry.link) == first)
                .map(|entry| rows.section_of(entry));
            if let Some(section) = restored {
                rows.section = section;
            }
            rows.refilter(&filter);
            sort(&mut rows.entries, order, &reachable);
            let row = rows
                .entries
                .iter()
                .position(|entry| Some(&entry.link) == first)
                .unwrap_or(0);
            select_row(list, row, window, cx);
        });
        cx.notify();
    }

    /// Writes decisions to the database off the main thread. A failure shows on the board.
    fn save(&mut self, decisions: Vec<(String, Option<Decision>)>, cx: &mut Context<Self>) {
        let database = self.database.clone();
        let saved = self.browser.read(cx).background().spawn_blocking(move || {
            let database = database.ok_or("this system has no application data directory")?;
            let mut store = Store::open(&database).map_err(|error| error.to_string())?;
            for (link, decision) in decisions {
                store.decide(&link, decision).map_err(|error| error.to_string())?;
            }
            Ok::<_, String>(())
        });
        cx.spawn(async move |this, cx| {
            let problem = match saved.await {
                Ok(Ok(())) => None,
                Ok(Err(reason)) => Some(format!("The decision was not saved: {reason}")),
                Err(stopped) => Some(format!("The decision was not saved: {stopped}")),
            };
            this.update(cx, |this, cx| {
                this.problem = problem;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The address of the selected posting or application.
    fn selected_address(&self, cx: &App) -> Option<String> {
        let rows = self.list.read(cx).delegate();
        match rows.selected_entry() {
            Some(entry) => Some(entry.address.clone()),
            None => rows.selected_application()?.link.clone(),
        }
    }

    /// Opens the selected posting in the default browser.
    fn open_selected(&mut self, cx: &mut Context<Self>) {
        if let Some(address) = self.selected_address(cx) {
            cx.open_url(&address);
        }
    }

    fn copy_selected(&mut self, cx: &mut Context<Self>) {
        if let Some(address) = self.selected_address(cx) {
            cx.write_to_clipboard(ClipboardItem::new_string(address.clone()));
            self.copied = Some(address);
            cx.notify();
        }
    }

    /// The buttons that open and copy the posting's address, and the address itself, which opens it too.
    fn link_bar(&self, address: &str, palette: &Palette, cx: &mut Context<Self>) -> Div {
        let copied = self.copied.as_deref() == Some(address);
        let ink = palette.ink;
        let shown = address
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .trim_start_matches("www.")
            .to_owned();
        h_flex()
            .flex_none()
            .gap_3()
            .px_4()
            .pb(px(12.))
            .border_b_1()
            .border_color(palette.line)
            .child(
                ui::button(
                    "open-posting",
                    "Open posting",
                    ui::quiet_face("open posting ↗  o", palette),
                )
                .tip("Open the posting in your browser.")
                .on_click(cx.listener(|this, _, _, cx| this.open_selected(cx))),
            )
            .child(
                ui::button(
                    "copy-link",
                    "Copy link",
                    ui::quiet_face(if copied { "copied" } else { "copy link" }, palette),
                )
                .on_click(cx.listener(|this, _, _, cx| this.copy_selected(cx))),
            )
            .child(
                ui::button(
                    "posting-address",
                    "Posting address",
                    div()
                        .flex_1()
                        .min_w_0()
                        .font_family(ui::MONO)
                        .text_size(px(11.))
                        .truncate()
                        .text_color(palette.faint)
                        .hover(move |face| face.text_color(ink))
                        .child(SharedString::from(shown)),
                )
                .tip(SharedString::from(address.to_owned()))
                .on_click(cx.listener(|this, _, _, cx| this.open_selected(cx))),
            )
    }

    fn on_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        // Letters typed into a draft or the search are text, not decisions.
        let searching = self.search.read(cx).focus_handle(cx).is_focused(window);
        if self.outreach.read(cx).is_editing() || searching {
            return;
        }
        let modifiers = event.keystroke.modifiers;
        match event.keystroke.key.as_str() {
            "z" if modifiers.platform => self.undo(window, cx),
            "f" if modifiers.platform => self.focus_search(window, cx),
            "a" if modifiers.platform => self.mark_all(cx),
            _ if modifiers.modified() => return,
            "escape" if self.marked(cx) > 0 => self.clear_marks(cx),
            "x" => self.toggle_mark(window, cx),
            "/" => self.focus_search(window, cx),
            "a" => self.decide(Decision::Approved, window, cx),
            "q" => self.decide(Decision::Queued, window, cx),
            "s" => self.decide(Decision::Skipped, window, cx),
            "m" => self.mark_submitted(cx),
            "o" => self.open_selected(cx),
            "h" => self.hide_company(window, cx),
            _ => return,
        }
        cx.stop_propagation();
    }

    /// The board's controls: which list, its order, and undo.
    fn toolbar(&self, palette: &Palette, cx: &mut Context<Self>) -> Div {
        let rows = self.list.read(cx).delegate();
        let section = rows.section;
        let (pending, queued, errors) = (
            rows.count(Section::Pending),
            rows.count(Section::Queue),
            rows.count(Section::Errors),
        );
        let sent = rows.submitted.len();
        let failure_filter = rows.failure_filter;
        let failures_of = |failure: Failure| {
            rows.all()
                .filter(|entry| rows.section_of(entry) == Section::Errors)
                .filter(|entry| rows.failures.get(&entry.link).map(|(kind, _)| *kind) == Some(failure))
                .count()
        };
        let failure_counts: Vec<(Failure, usize)> = Failure::ALL
            .iter()
            .map(|&failure| (failure, failures_of(failure)))
            .filter(|(_, count)| *count > 0)
            .collect();
        let choice = |id: &'static str, name: String, word: String, chosen: bool| {
            let ink = palette.ink;
            let face = ui::caps(word)
                .text_color(if chosen { palette.ink } else { palette.faint })
                .hover(move |face| face.text_color(ink));
            ui::button(id, name, face)
        };
        let orders: Vec<AnyElement> = match section {
            // Errors are filtered by what went wrong in place of a sort.
            Section::Errors => std::iter::once((None, "all".to_owned(), "error-all".to_owned()))
                .chain(failure_counts.into_iter().map(|(failure, count)| {
                    (
                        Some(failure),
                        format!("{} {count}", failure.label()),
                        format!("error-{}", failure.as_str()),
                    )
                }))
                .map(|(failure, word, id)| {
                    choice_owned(id, format!("Show {word}"), word, failure_filter == failure, palette)
                        .on_click(cx.listener(move |this, _, window, cx| this.filter_failures(failure, window, cx)))
                        .into_any_element()
                })
                .collect(),
            Section::Pending | Section::Queue => Order::ALL
                .into_iter()
                .map(|order| {
                    let word = order.word();
                    choice(
                        order.id(),
                        format!("Sort by {word}"),
                        word.to_owned(),
                        self.order == order,
                    )
                    .on_click(cx.listener(move |this, _, window, cx| this.set_order(order, window, cx)))
                    .into_any_element()
                })
                .collect(),
            Section::Submitted => [
                (SubmittedOrder::Latest, "sort-applied", "latest"),
                (SubmittedOrder::Company, "sort-company", "company a-z"),
            ]
            .into_iter()
            .map(|(order, id, word)| {
                choice(
                    id,
                    format!("Sort by {word}"),
                    word.to_owned(),
                    self.submitted_order == order,
                )
                .on_click(cx.listener(move |this, _, window, cx| this.set_submitted_order(order, window, cx)))
                .into_any_element()
            })
            .collect(),
        };
        h_flex()
            .flex_none()
            .gap_4()
            .px(px(14.))
            .py(px(8.))
            .border_b_1()
            .border_color(palette.line)
            .child(
                choice(
                    "board-pending",
                    format!("Pending · {pending}"),
                    format!("pending {pending}"),
                    section == Section::Pending,
                )
                .on_click(cx.listener(|this, _, window, cx| this.show(Section::Pending, window, cx))),
            )
            .child(
                choice(
                    "board-queue",
                    format!("Queue · {queued}"),
                    format!("queue {queued}"),
                    section == Section::Queue,
                )
                .on_click(cx.listener(|this, _, window, cx| this.show(Section::Queue, window, cx))),
            )
            .child(
                choice(
                    "board-errors",
                    format!("Errors · {errors}"),
                    format!("errors {errors}"),
                    section == Section::Errors,
                )
                .on_click(cx.listener(|this, _, window, cx| this.show(Section::Errors, window, cx))),
            )
            .child(
                choice(
                    "board-submitted",
                    format!("Submitted · {sent}"),
                    format!("submitted {sent}"),
                    section == Section::Submitted,
                )
                .on_click(cx.listener(|this, _, window, cx| this.show(Section::Submitted, window, cx))),
            )
            .child(div().flex_1())
            .children(self.queue_controls(palette, cx))
            .child(ui::caps("sort").text_color(palette.faint))
            .children(orders)
            .children((!self.made.is_empty()).then(|| {
                ui::button("undo-decision", "Undo", ui::quiet_face("undo ⌘z", palette))
                    .on_click(cx.listener(|this, _, window, cx| this.undo(window, cx)))
            }))
    }

    /// How many postings are marked, with ways to decide them together.
    fn marked_strip(&self, palette: &Palette, cx: &mut Context<Self>) -> Option<Div> {
        let marked = self.marked(cx);
        if marked == 0 {
            return None;
        }
        let said = format!("{marked} marked");
        Some(
            h_flex()
                .flex_none()
                .gap_3()
                .px(px(14.))
                .py(px(6.))
                .border_b_1()
                .border_color(palette.line)
                .child(ui::square(palette.signal(Signal::Yours)))
                .child(
                    ui::caps(said.clone())
                        .id("marked-count")
                        .test_support()
                        .role(gpui_kit::Role::Label)
                        .aria_label(said)
                        .text_color(palette.ink),
                )
                .child(
                    ui::button(
                        "approve-marked",
                        format!("Approve {marked} marked"),
                        ui::primary_face(format!("approve {marked}"), Some("a"), palette),
                    )
                    .on_click(cx.listener(|this, _, window, cx| this.decide(Decision::Approved, window, cx))),
                )
                .child(
                    ui::button(
                        "skip-marked",
                        format!("Skip {marked} marked"),
                        ui::quiet_face("skip  s", palette),
                    )
                    .on_click(cx.listener(|this, _, window, cx| this.decide(Decision::Skipped, window, cx))),
                )
                .child(div().flex_1())
                .child(ui::caps("x marks · ⌘a marks all").text_color(palette.faint))
                .child(
                    ui::button("clear-marks", "Clear marks", ui::quiet_face("clear  esc", palette))
                        .on_click(cx.listener(|this, _, _, cx| this.clear_marks(cx))),
                ),
        )
    }

    /// The company filter in force, with a way to take back each part of it.
    fn filter_strip(&self, palette: &Palette, cx: &mut Context<Self>) -> Option<Div> {
        if !self.filter.is_active() {
            return None;
        }
        let rows = self.list.read(cx).delegate();
        let searching = !rows.search.is_empty();
        let out = rows
            .filtered_out
            .iter()
            .filter(|entry| entry.decision.is_none() && !self.filter.shows(&entry.company))
            .count();
        let unset = |id: ElementId, name: String, word: String, filter: CompanyFilter| {
            ui::button(id, name, ui::quiet_face(format!("{word} ×"), palette))
                .on_click(cx.listener(move |this, _, window, cx| this.set_filter(filter.clone(), 0, window, cx)))
                .into_any_element()
        };
        let parts: Vec<AnyElement> = match &self.filter.only {
            Some(only) => vec![unset(
                "company-only".into(),
                format!("Show all companies, not only {only}"),
                format!("only {only}"),
                CompanyFilter {
                    only: None,
                    ..self.filter.clone()
                },
            )],
            None => self
                .filter
                .hidden
                .iter()
                .enumerate()
                .map(|(ix, company)| {
                    let mut filter = self.filter.clone();
                    filter.hidden.remove(company);
                    unset(
                        ElementId::NamedInteger("unhide-company".into(), ix as u64),
                        format!("Show {company} again"),
                        company.clone(),
                        filter,
                    )
                })
                .collect(),
        };
        let label = if self.filter.only.is_some() {
            "showing"
        } else {
            "hiding"
        };
        Some(
            h_flex()
                .flex_none()
                .flex_wrap()
                .gap_2()
                .px(px(14.))
                .py(px(6.))
                .border_b_1()
                .border_color(palette.line)
                .child(ui::caps(label).text_color(palette.faint))
                .children(parts)
                .child(div().flex_1())
                .child({
                    let said = if searching {
                        "the search looks past this".to_owned()
                    } else {
                        format!("{out} pending hidden")
                    };
                    ui::caps(said.clone())
                        .id("filtered-out")
                        .test_support()
                        .role(gpui_kit::Role::Label)
                        .aria_label(said)
                        .text_color(palette.faint)
                })
                .children((self.filter.hidden.len() > 1 || self.filter.only.is_some()).then(|| {
                    ui::button(
                        "show-all-companies",
                        "Show all companies",
                        ui::quiet_face("show all", palette),
                    )
                    .on_click(
                        cx.listener(|this, _, window, cx| this.set_filter(CompanyFilter::default(), 0, window, cx)),
                    )
                })),
        )
    }

    /// The queue's counts, how many forms fill at once, and a way to start approvals from before.
    fn queue_controls(&self, palette: &Palette, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let browser = self.browser.read(cx);
        let queue = browser.queue();
        let parallel = queue.parallel();
        let count = |pick: fn(&Step) -> bool| queue.runs().iter().filter(|run| pick(&run.step)).count();
        let (running, waiting, yours) = (
            count(Step::is_running),
            count(|step| *step == Step::Queued),
            count(|step| matches!(step, Step::NeedsYou(_) | Step::Ready)),
        );
        let mut controls = Vec::new();
        if !queue.runs().is_empty() {
            let status = format!("{running} running · {waiting} queued · {yours} yours");
            controls.push(
                ui::caps(status.clone())
                    .id("queue-status")
                    .test_support()
                    .role(gpui_kit::Role::Label)
                    .aria_label(status)
                    .text_color(palette.muted)
                    .into_any_element(),
            );
        }
        let queued = self
            .list
            .read(cx)
            .delegate()
            .all()
            .filter(|entry| entry.decision == Some(Decision::Queued))
            .count();
        if queued > 0 {
            controls.push(
                ui::button(
                    "run-queue",
                    format!("Run queue · {queued}"),
                    ui::quiet_face(format!("run queue {queued}"), palette),
                )
                .tip("Apply to every posting in the queue. Maverick submits each form it can complete.")
                .on_click(cx.listener(|this, _, window, cx| this.run_queue(window, cx)))
                .into_any_element(),
            );
        }
        let step = |id: &'static str, name: &'static str, word: &'static str, to: usize| {
            ui::button(id, name, ui::quiet_face(word, palette))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.browser.update(cx, |browser, cx| browser.set_parallel(to, cx));
                }))
                .into_any_element()
        };
        controls.push(step(
            "fewer-parallel",
            "Fill fewer at once",
            "-",
            parallel.saturating_sub(1),
        ));
        controls.push(
            ui::caps(format!("{parallel} at once"))
                .id("parallel")
                .test_support()
                .role(gpui_kit::Role::Label)
                .aria_label(format!("{parallel} at once"))
                .text_color(palette.muted)
                .into_any_element(),
        );
        controls.push(step("more-parallel", "Fill more at once", "+", parallel + 1));
        controls
    }

    /// The selected row in full, with the decision buttons for a posting.
    fn pane(&self, palette: &Palette, cx: &mut Context<Self>) -> Div {
        let rows = self.list.read(cx).delegate();
        if let Some(sent) = rows.selected_application() {
            let facts = [
                ("applied", sent.applied_on.strftime("%b %-d, %Y").to_string()),
                ("status", sent.status.as_str().to_owned()),
                ("address", sent.link.clone().unwrap_or_default()),
                ("notes", sent.notes.clone().unwrap_or_default()),
            ];
            let role = sent.role.clone().unwrap_or_else(|| "(no company in the sheet)".into());
            let link = sent.link.clone();
            return v_flex()
                .size_full()
                .child(header("submitted", &role, None, palette))
                .children(link.map(|link| self.link_bar(&link, palette, cx)))
                .child(div().px_4().py_3().child(ui::facts(&facts, palette)));
        }
        let Some(entry) = rows.selected_entry().cloned() else {
            return v_flex();
        };
        let sponsorship = match entry.sponsorship {
            Sponsorship::Sponsors => "sponsors",
            Sponsorship::NoSponsorship => "no sponsorship",
            Sponsorship::Unknown => "unknown",
        };
        let facts = [
            ("location", entry.location.clone().unwrap_or_default()),
            ("visa", sponsorship.to_owned()),
            ("found", entry.found_at.strftime("%b %-d, %Y").to_string()),
            ("source", entry.source.clone()),
            ("address", entry.address.clone()),
        ];
        let run = match entry.decision {
            Some(Decision::Approved) => self.browser.read(cx).queue().get(&entry.address).cloned(),
            _ => None,
        };
        let fillable = System::of(&entry.address).is_some();
        let failure = rows
            .failures
            .get(&entry.link)
            .filter(|_| rows.section_of(&entry) == Section::Errors)
            .cloned();
        let note = match (&entry.decision, run.as_ref().map(|run| &run.step)) {
            _ if failure.is_some() && fillable => "approve to try again.",
            _ if failure.is_some() => "apply on the site, then mark it submitted.",
            (None | Some(Decision::Approved), None) if !fillable => "maverick can't fill this site.",
            (None, _) => "approve applies now.",
            (Some(Decision::Queued), _) => "waits for run queue.",
            (Some(Decision::Skipped), _) => "skipped.",
            (Some(Decision::Approved), None) => "approve again to apply.",
            (_, Some(Step::Queued)) => "waits for a free slot.",
            (_, Some(Step::Opening | Step::Filling)) => "filling. maverick submits it if it can.",
            (_, Some(Step::Submitting)) => "submitting.",
            (_, Some(Step::NeedsYou(_) | Step::Ready)) => "you click submit.",
            (_, Some(Step::Applied)) => "applied.",
            (_, Some(Step::Failed(..))) => "nothing was submitted.",
        };
        let marked = rows.marked.len();
        let (word, name) = match run.as_ref().map(|run| &run.step) {
            _ if marked > 0 => (format!("approve {marked}"), format!("Approve {marked} marked")),
            _ if failure.is_some() && fillable => ("try again".into(), "Try again".into()),
            _ if failure.is_some() => (String::new(), String::new()),
            None if entry.decision == Some(Decision::Approved) && fillable => ("apply".into(), "Apply".into()),
            None if entry.decision == Some(Decision::Approved) => (String::new(), String::new()),
            None => ("approve".into(), "Approve".into()),
            Some(Step::Failed(..)) => ("try again".into(), "Try again".into()),
            Some(_) => (String::new(), String::new()),
        };
        let queueable = entry.decision.is_none() && run.is_none() && fillable;
        let markable = marked == 0 && can_mark_submitted(run.as_ref().map(|run| &run.step));
        let link = entry.link.clone();
        let reported = run.as_ref().is_some_and(|run| run.report.is_some());
        let scorer_note = Verdict::read(entry.notes.as_deref(), entry.flags.as_deref()).outreach;
        let (scorer, outreach) = self.outreach.update(cx, |outreach, cx| {
            let scorer = (!reported)
                .then(|| outreach.scorer_section(&link, &scorer_note, palette, cx))
                .flatten();
            (scorer, outreach.posting_section(&link, palette, cx))
        });
        let reviewable = marked == 0
            && run
                .as_ref()
                .is_some_and(|run| run.tab.is_some() && matches!(run.step, Step::NeedsYou(_) | Step::Ready));
        let primary = if reviewable {
            Some(
                ui::button(
                    "review-run",
                    "Review in Chrome",
                    ui::primary_face("review in chrome", Some("enter"), palette),
                )
                .on_click(cx.listener(|this, _, _, cx| this.show_tab(cx)))
                .into_any_element(),
            )
        } else {
            (!word.is_empty()).then(|| {
                ui::button("approve", name, ui::primary_face(word, Some("a"), palette))
                    .on_click(cx.listener(|this, _, window, cx| this.decide(Decision::Approved, window, cx)))
                    .into_any_element()
            })
        };
        let link_bar = self.link_bar(&entry.address, palette, cx);
        let company_actions = h_flex()
            .gap_2()
            .child(
                ui::button(
                    "hide-company",
                    format!("Hide {}", entry.company),
                    ui::quiet_face("hide  h", palette),
                )
                .tip("Keep this company's postings out of the list. It stays hidden after a restart.")
                .on_click(cx.listener(|this, _, window, cx| this.hide_company(window, cx))),
            )
            .children(self.filter.only.is_none().then(|| {
                ui::button(
                    "only-company",
                    format!("Only {}", entry.company),
                    ui::quiet_face("only", palette),
                )
                .tip("Show this company's postings alone.")
                .on_click(cx.listener(|this, _, window, cx| this.only_company(window, cx)))
            }));
        v_flex()
            .size_full()
            .child(posting_header(&entry, company_actions, palette))
            .child(link_bar)
            .children(match &failure {
                Some((failure, why)) => Some(failure_status(*failure, why, palette).into_any_element()),
                None => run.as_ref().map(|run| run_status(run, palette).into_any_element()),
            })
            .child(
                div()
                    .id("board-facts")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px_4()
                    .py_3()
                    .child(match run.as_ref().and_then(|run| run.report.as_ref()) {
                        Some(report) => report::sections(report, palette),
                        None => verdict_view(&entry, palette),
                    })
                    .children(scorer)
                    .child(outreach)
                    .child(ui::caps("details").pb(px(8.)).text_color(palette.faint))
                    .child(ui::facts(&facts, palette)),
            )
            .child(
                h_flex()
                    .flex_none()
                    .gap_3()
                    .px_4()
                    .py(px(10.))
                    .min_h(px(46.))
                    .border_t_1()
                    .border_color(palette.line)
                    .children(primary)
                    .children(queueable.then(|| {
                        let (word, name) = match marked {
                            0 => ("add to queue  q".to_owned(), "Add to queue".to_owned()),
                            marked => (format!("queue {marked}  q"), format!("Add {marked} marked to queue")),
                        };
                        ui::button("add-to-queue", name, ui::quiet_face(word, palette))
                            .tip("Save it for later. Run queue applies to every posting in the queue.")
                            .on_click(cx.listener(|this, _, window, cx| this.decide(Decision::Queued, window, cx)))
                    }))
                    .children(markable.then(|| {
                        ui::button(
                            "mark-submitted",
                            "Mark submitted",
                            ui::quiet_face("mark submitted  m", palette),
                        )
                        .tip("You submitted it yourself. Maverick records the application.")
                        .on_click(cx.listener(|this, _, _, cx| this.mark_submitted(cx)))
                    }))
                    .child(
                        ui::button("skip", "Skip", ui::quiet_face("skip  s", palette))
                            .on_click(cx.listener(|this, _, window, cx| this.decide(Decision::Skipped, window, cx))),
                    )
                    .child(
                        ui::serif(note)
                            .ml_auto()
                            .min_w_0()
                            .truncate()
                            .text_size(px(13.5))
                            .text_color(palette.muted),
                    ),
            )
    }
}

/// A word in the toolbar that picks a list, an order or a filter.
fn choice_owned(id: String, name: String, word: String, chosen: bool, palette: &Palette) -> ui::Button {
    let ink = palette.ink;
    let face = ui::caps(word)
        .text_color(if chosen { palette.ink } else { palette.faint })
        .hover(move |face| face.text_color(ink));
    ui::button(SharedString::from(id), name, face)
}

/// The company, its filter buttons and the application system above the posting's title, which can be copied.
fn posting_header(entry: &Entry, company_actions: Div, palette: &Palette) -> Div {
    h_flex()
        .flex_none()
        .items_end()
        .justify_between()
        .gap_4()
        .px_4()
        .pt(px(14.))
        .pb(px(10.))
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap(px(6.))
                .child(
                    h_flex()
                        .gap(px(10.))
                        .child(
                            div()
                                .min_w_0()
                                .font_family(ui::MONO)
                                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                .text_size(px(13.))
                                .line_height(px(17.))
                                .text_color(palette.ink)
                                .child(ui::selectable("posting-company", &entry.company.to_uppercase())),
                        )
                        .child(ui::boxed(entry.system(), palette.muted))
                        .child(company_actions),
                )
                .child(
                    div()
                        .font_family(ui::SERIF)
                        .text_size(px(26.))
                        .line_height(px(30.))
                        .text_color(palette.ink)
                        .child(ui::selectable("posting-title", &entry.title)),
                ),
        )
        .child(ui::score(entry.score, 54.).text_color(palette.ink))
}

/// What scoring said, in parts: the reason, then what blocks, what to watch and the team. Whom the
/// scorer said to reach shows in its own section, where each person can be reached.
fn verdict_view(entry: &Entry, palette: &Palette) -> Div {
    let verdict = Verdict::read(entry.notes.as_deref(), entry.flags.as_deref());
    let status = match (&entry.notes, entry.apply_ready) {
        (_, true) => ui::chip(Signal::Yours, "apply-ready", palette),
        (None, _) => ui::boxed("not scored", palette.faint),
        (Some(_), false) => ui::boxed("review", palette.muted),
    };
    let reading = |size: f32, line: f32| {
        div()
            .flex_1()
            .min_w_0()
            .font_family(ui::SERIF)
            .text_size(px(size))
            .line_height(px(line))
    };
    let points = |id: &'static str, label: &str, items: &[String], mark: gpui_kit::Hsla| {
        (!items.is_empty()).then(|| {
            v_flex()
                .gap(px(5.))
                .child(ui::caps(label).text_color(palette.faint))
                .children(items.iter().enumerate().map(|(ix, item)| {
                    h_flex()
                        .items_start()
                        .gap(px(9.))
                        .child(ui::square(mark).mt(px(7.)))
                        .child(
                            reading(13.5, 19.)
                                .text_color(palette.muted)
                                .child(ui::selectable(format!("{id}-{ix}"), item)),
                        )
                }))
        })
    };
    let team: Vec<String> = verdict.team.iter().cloned().collect();
    v_flex()
        .gap(px(12.))
        .pb(px(14.))
        .mb(px(14.))
        .border_b_1()
        .border_color(palette.line)
        .child(
            h_flex()
                .gap(px(8.))
                .child(ui::caps("verdict").text_color(palette.faint))
                .child(status)
                .children(entry.variant.clone().map(|variant| ui::boxed(variant, palette.faint))),
        )
        .children((!verdict.reason.is_empty()).then(|| {
            reading(15., 22.)
                .text_color(palette.ink)
                .child(ui::selectable("verdict-reason", &verdict.reason))
        }))
        .children(points(
            "blocking",
            "blocking",
            &verdict.blocking,
            palette.signal(Signal::Yours),
        ))
        .children(points("advisory", "watch for", &verdict.advisory, palette.muted))
        .children(points("team", "team", &team, palette.faint))
}

/// Where a queued posting is, and the fill report's map once there is one.
/// Why Maverick could not apply to a posting in Errors.
fn failure_status(failure: Failure, why: &str, palette: &Palette) -> impl IntoElement + use<> {
    h_flex()
        .id("failure-status")
        .test_support()
        .role(gpui_kit::Role::Label)
        .aria_label(format!("{}: {why}", failure.label()))
        .flex_none()
        .items_start()
        .gap(px(9.))
        .px_4()
        .pb_3()
        .border_b_1()
        .border_color(palette.line)
        .child(ui::square(palette.signal(Signal::Failed)).mt(px(6.)))
        .child(ui::serif(capitalized(why)).flex_1().min_w_0().text_size(px(13.5)))
}

fn capitalized(text: &str) -> String {
    let mut letters = text.chars();
    letters
        .next()
        .map(|first| first.to_uppercase().chain(letters).collect())
        .unwrap_or_default()
}

fn run_status(run: &Run, palette: &Palette) -> impl IntoElement + use<> {
    let (signal, text) = match &run.step {
        Step::Queued => (None, "Queued. It starts when a slot is free.".to_owned()),
        Step::Opening => (None, "Opening the form in a background tab.".to_owned()),
        Step::Filling => (Some(Signal::Model), "Filling the form in a background tab.".to_owned()),
        Step::Submitting => (
            Some(Signal::Model),
            "Nothing was left for you, so Maverick pressed Submit.".to_owned(),
        ),
        Step::NeedsYou(why) => (Some(Signal::Yours), why.clone()),
        // The report says why Maverick would not submit this form itself, such as a draft to read.
        Step::Ready => (
            Some(Signal::Yours),
            match run.report.as_ref().and_then(|report| report.held_for_the_user()) {
                Some(why) => format!("Every required field is answered. {why} Review it and click Submit."),
                None => "Every required field is answered. Review it and click Submit.".to_owned(),
            },
        ),
        Step::Applied => (Some(Signal::Done), "Submitted and recorded.".to_owned()),
        Step::Failed(_, why) => (Some(Signal::Failed), why.clone()),
    };
    v_flex()
        .id("run-status")
        .test_support()
        .role(gpui_kit::Role::Label)
        .aria_label(format!("{}: {text}", run.step.word()))
        .flex_none()
        .gap_2()
        .px_4()
        .pb_3()
        .border_b_1()
        .border_color(palette.line)
        .child(
            h_flex()
                .items_start()
                .gap(px(9.))
                .child(ui::square(signal.map_or(palette.faint, |signal| palette.signal(signal))).mt(px(6.)))
                .child(ui::serif(text).flex_1().min_w_0().text_size(px(13.5))),
        )
        .children(run.report.as_ref().map(|report| report::form_map(report, palette)))
}

/// Selects `row`, or the last row when the list is shorter, and brings it into view.
fn select_row(list: &mut ListState<Rows>, row: usize, window: &mut Window, cx: &mut Context<ListState<Rows>>) {
    let shown = list.delegate().shown();
    let selected = (shown > 0).then(|| IndexPath::new(row.min(shown - 1)));
    list.set_selected_index(selected, window, cx);
    list.scroll_to_selected_item(window, cx);
    cx.notify();
}

impl Render for Board {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::of(cx);
        v_flex()
            .id("board")
            .size_full()
            .track_focus(&self.focus)
            .test_support()
            .on_key_down(cx.listener(Self::on_key))
            .child(self.toolbar(&palette, cx))
            .children(self.filter_strip(&palette, cx))
            .children(self.marked_strip(&palette, cx))
            .children(self.problem.clone().map(|problem| {
                h_flex()
                    .gap(px(9.))
                    .px(px(14.))
                    .py(px(7.))
                    .border_b_1()
                    .border_color(palette.line)
                    .child(ui::square(palette.signal(Signal::Failed)))
                    .child(ui::serif(SharedString::from(problem)).text_size(px(13.5)))
            }))
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .items_start()
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .border_r_1()
                            .border_color(palette.line)
                            .child(
                                div()
                                    .flex_none()
                                    .px(px(10.))
                                    .py(px(8.))
                                    .border_b_1()
                                    .border_color(palette.line)
                                    .child(
                                        Input::new(&self.search)
                                            .id("job-search")
                                            .aria_label("Search postings and applications")
                                            .cleanable(true),
                                    ),
                            )
                            .child(div().flex_1().min_h_0().child(List::new(&self.list))),
                    )
                    .child(
                        div()
                            .flex_none()
                            .w(relative(0.52))
                            .max_w(px(620.))
                            .h_full()
                            .child(self.pane(&palette, cx)),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{Bounds, TestAppContext, WindowBounds, WindowOptions, size};
    use jiff::civil::date;

    use super::*;

    const ASHBY: &str = "https://jobs.ashbyhq.com/example-co/0f3b8231-3a32-478a-b994-3441f4bb9401";
    const READY_ASHBY: &str = "https://jobs.ashbyhq.com/example-co/1a2b3c4d-1111-4222-8333-444455556666";
    const HIGH_ASHBY: &str = "https://jobs.ashbyhq.com/example-co/9f8e7d6c-1111-4222-8333-444455556666";

    fn posting(link: &str, title: &str, stage: Stage, score: Option<u8>, found: &str) -> Posting {
        Posting {
            link: link.into(),
            found_at: format!("2026-11-{found}T12:00:00Z").parse().unwrap(),
            source: "Simplify".into(),
            company: "Example Co".into(),
            title: title.into(),
            location: Some("Austin, TX".into()),
            email_subject: None,
            stage,
            score,
            variant: Some("Backend/Infra".into()),
            flags: None,
            notes: None,
            address: None,
            sponsorship: None,
        }
    }

    fn titles(entries: &[Entry]) -> Vec<&str> {
        entries.iter().map(|entry| entry.title.as_str()).collect()
    }

    fn sample() -> (Vec<Posting>, Vec<Application>) {
        let postings = vec![
            posting("https://simplify.jobs/p/1", "Middling", Stage::Review, Some(68), "10"),
            Posting {
                notes: Some(format!("{APPLY_READY} 77, Backend/Infra: fits")),
                sponsorship: Some(Sponsorship::Sponsors),
                address: Some(READY_ASHBY.into()),
                ..posting("https://simplify.jobs/p/2", "Ready", Stage::Review, Some(77), "08")
            },
            Posting {
                address: Some(HIGH_ASHBY.into()),
                ..posting(
                    "https://simplify.jobs/p/3",
                    "High but not ready",
                    Stage::Review,
                    Some(90),
                    "09",
                )
            },
            posting("https://simplify.jobs/p/4", "Unscored", Stage::Found, None, "12"),
            posting(
                "https://simplify.jobs/p/5",
                "Skipped by rules",
                Stage::Skipped,
                Some(30),
                "12",
            ),
            posting(
                "https://simplify.jobs/p/6",
                "Applied in the sheet",
                Stage::Applied,
                Some(85),
                "11",
            ),
            Posting {
                address: Some(format!("{ASHBY}?utm_source=x")),
                ..posting(
                    "https://simplify.jobs/p/7",
                    "Sent through Maverick",
                    Stage::Review,
                    Some(88),
                    "11",
                )
            },
            posting(
                "https://simplify.jobs/p/8",
                "Skipped but decided",
                Stage::Skipped,
                Some(55),
                "07",
            ),
        ];
        let sent = Application::submitted_today("Sent through Maverick at Example Co", &format!("{ASHBY}/application"));
        (postings, vec![sent])
    }

    #[test]
    fn the_board_lists_postings_in_play_apply_ready_first_and_decided_ones_last() {
        let (postings, applications) = sample();
        let decisions = HashMap::from([
            ("https://simplify.jobs/p/3".to_owned(), Decision::Skipped),
            ("https://simplify.jobs/p/8".to_owned(), Decision::Approved),
        ]);
        let mut entries = BoardData::new(&postings, applications, &decisions).entries;

        let none = HashSet::new();
        sort(&mut entries, Order::TopScore, &none);
        assert_eq!(
            titles(&entries),
            [
                "Ready",
                "Middling",
                "Unscored",
                "High but not ready",
                "Skipped but decided"
            ]
        );
        assert_eq!(entries[0].sponsorship, Sponsorship::Sponsors);
        assert_eq!(entries[1].sponsorship, Sponsorship::Unknown);
        assert_eq!(entries[0].system(), "Ashby");
        assert_eq!(entries[1].system(), "Simplify");

        sort(&mut entries, Order::Latest, &none);
        assert_eq!(
            titles(&entries),
            [
                "Unscored",
                "Middling",
                "Ready",
                "High but not ready",
                "Skipped but decided"
            ]
        );

        // Postings with someone to reach come first, each group by top score.
        let reachable = HashSet::from([
            "https://simplify.jobs/p/1".to_owned(),
            "https://simplify.jobs/p/4".to_owned(),
        ]);
        sort(&mut entries, Order::Outreach, &reachable);
        assert_eq!(
            titles(&entries),
            [
                "Middling",
                "Unscored",
                "Ready",
                "High but not ready",
                "Skipped but decided"
            ]
        );
    }

    #[test]
    fn the_company_filter_hides_companies_or_shows_one_alone() {
        let mut filter = CompanyFilter::default();
        assert!(filter.shows("Amazon"));
        filter.hidden.insert("Amazon".into());
        assert!(!filter.shows("amazon "));
        assert!(filter.shows("Apple"));
        filter.only = Some("Apple".into());
        assert!(filter.shows("Apple"));
        assert!(!filter.shows("Google"));

        // Only the hidden companies outlast a restart.
        let scratch = tempfile::tempdir().unwrap();
        let file = scratch.path().join("company-filter.json");
        filter.save(&file).unwrap();
        let loaded = CompanyFilter::load(Some(&file));
        assert_eq!(loaded.hidden, filter.hidden);
        assert_eq!(loaded.only, None);
        assert_eq!(
            CompanyFilter::load(Some(&scratch.path().join("none.json"))),
            CompanyFilter::default()
        );
    }

    #[test]
    fn a_search_finds_any_alternative_and_every_word_of_one() {
        let (postings, applications) = sample();
        let entries = BoardData::new(&postings, applications.clone(), &HashMap::new()).entries;
        let found = |text: &str| {
            let search = Search::new(text);
            entries
                .iter()
                .filter(|entry| search.finds_entry(entry))
                .map(|entry| entry.title.as_str())
                .collect::<Vec<_>>()
        };
        assert_eq!(found("").len(), entries.len());
        assert_eq!(found(" , ").len(), entries.len());
        assert_eq!(found("MIDDLING"), ["Middling"]);
        assert_eq!(found("ready, middling").len(), 3);
        assert_eq!(found("example ready not"), ["High but not ready"]);
        assert_eq!(found("austin simplify unscored"), ["Unscored"]);
        assert!(found("nowhere inc").is_empty());

        let sent = Application::submitted_today("Engineer at Example Co", "https://jobs.lever.co/example/1");
        assert!(Search::new("example co, apple").finds_application(&sent));
        assert!(Search::new("lever.co").finds_application(&sent));
        assert!(!Search::new("apple").finds_application(&sent));
    }

    #[test]
    fn applications_sort_by_date_or_by_company() {
        let sent = |row: Option<u32>, role: &str, day: i8| Application {
            sheet_row: row,
            role: Some(role.into()),
            link: None,
            applied_on: date(2026, 11, day),
            status: ApplicationStatus::Pending,
            notes: None,
        };
        let mut applications = vec![
            sent(Some(2), "Beta Labs", 3),
            sent(None, "Engineer at Zeta Inc", 9),
            sent(Some(3), "alpha corp", 5),
        ];
        sort_submitted(&mut applications, SubmittedOrder::Latest);
        let order = |applications: &[Application]| -> Vec<String> { applications.iter().map(company_of).collect() };
        assert_eq!(order(&applications), ["Zeta Inc", "alpha corp", "Beta Labs"]);
        sort_submitted(&mut applications, SubmittedOrder::Company);
        assert_eq!(order(&applications), ["alpha corp", "Beta Labs", "Zeta Inc"]);
    }

    fn wait_until(what: &str, reached: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !reached() {
            assert!(Instant::now() < deadline, "timed out waiting until {what}");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[gpui_kit::test]
    fn keys_and_buttons_decide_postings_and_undo_takes_the_last_one_back(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let scratch = tempfile::tempdir().unwrap();
        let database = scratch.path().join("maverick.db");
        let (postings, applications) = sample();
        let mut store = Store::open(&database).unwrap();
        store.add_found(&postings).unwrap();
        for sent in &applications {
            store.record_application(&[], sent).unwrap();
        }
        drop(store);
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::ui::install(cx);
        });
        let data = BoardData::new(&postings, applications, &HashMap::new());
        let background = runtime.handle().clone();
        // Chrome is not connected, so approved postings wait in the queue.
        let browser = cx.update(|cx| {
            cx.new(|cx| {
                let bridge = maverick_core::bridge::Bridge::new();
                Browser::new(bridge, background, Err("none".into()), None, None, cx)
            })
        });
        let (window, board) = cx.update(|cx| {
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                    Default::default(),
                    size(px(1000.), px(640.)),
                ))),
                ..Default::default()
            };
            gpui_kit::open_window(options, cx, |window, cx| {
                let outreach = cx.new(|cx| OutreachView::new(browser.clone(), None, window, cx));
                cx.new(|cx| Board::new(browser.clone(), outreach, data, Some(database.clone()), window, cx))
            })
            .expect("the test window opens")
        });
        let decisions = || Store::open(&database).unwrap().decisions().unwrap();
        let shown = |cx: &mut TestAppContext| {
            board.read_with(cx, |board, cx| {
                let rows = board.list.read(cx).delegate();
                let selected = rows.selected_entry().map(|entry| entry.title.clone());
                (titles(&rows.entries).join(", "), selected, board.pending(cx))
            })
        };

        assert_eq!(
            shown(cx),
            (
                "Ready, High but not ready, Middling, Unscored".into(),
                Some("Ready".into()),
                4
            )
        );
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(window.find("board-pending").label(), Some("Pending · 4"));
            board.update(cx, |board, cx| board.focus(window, cx));
            window.press("a", cx);
            window.press("s", cx);
        })
        .unwrap();
        wait_until("both decisions are saved", || decisions().len() == 2);
        assert_eq!(decisions()["https://simplify.jobs/p/2"], Decision::Approved);
        assert_eq!(decisions()["https://simplify.jobs/p/3"], Decision::Skipped);
        // The approved posting moves to the queue and the skipped one sinks below the pending
        // ones, so the selection lands on the next pending posting.
        assert_eq!(
            shown(cx),
            (
                "Middling, Unscored, High but not ready".into(),
                Some("Middling".into()),
                2
            )
        );
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(window.find("board-queue").label(), Some("Queue · 1"));
        })
        .unwrap();

        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.click("undo-decision", cx);
            window.render_frame(cx);
            window.click("approve", cx);
            window.render_frame(cx);
            window.click("sort-latest", cx);
        })
        .unwrap();
        wait_until("the undo and the click are saved", || {
            decisions().get("https://simplify.jobs/p/3") == Some(&Decision::Approved)
        });
        assert_eq!(decisions().len(), 2);
        let queued: Vec<(String, Step)> = browser.read_with(cx, |browser, _| {
            let runs = browser.queue().runs();
            runs.iter().map(|run| (run.title.clone(), run.step.clone())).collect()
        });
        assert_eq!(
            queued,
            [
                ("Ready at Example Co".to_owned(), Step::Queued),
                ("High but not ready at Example Co".to_owned(), Step::Queued),
            ]
        );
        assert_eq!(shown(cx), ("Unscored, Middling".into(), Some("Unscored".into()), 2));
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.click("board-queue", cx);
            window.render_frame(cx);
        })
        .unwrap();
        assert_eq!(
            shown(cx),
            ("High but not ready, Ready".into(), Some("High but not ready".into()), 2)
        );
        cx.update_window(window, |_, window, cx| {
            window.click("board-pending", cx);
            window.render_frame(cx);
        })
        .unwrap();

        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.click("copy-link", cx);
            window.render_frame(cx);
            assert_eq!(window.find("copy-link").label(), Some("Copy link"));
        })
        .unwrap();
        let copied = cx.read_from_clipboard().and_then(|item| item.text());
        assert_eq!(copied.as_deref(), Some("https://simplify.jobs/p/4"));

        cx.update_window(window, |_, window, cx| {
            window.click("board-submitted", cx);
            window.render_frame(cx);
            assert_eq!(window.find("board-submitted").label(), Some("Submitted · 1"));
            assert!(window.try_find("approve").is_none());
        })
        .unwrap();
    }

    #[test]
    fn a_submit_marks_the_posting_applied_when_it_was_found_by_another_link() {
        let scratch = tempfile::tempdir().unwrap();
        let database = scratch.path().join("maverick.db");
        let (postings, _) = sample();
        Store::open(&database).unwrap().add_found(&postings).unwrap();
        let address = format!("{READY_ASHBY}?utm_source=mail");
        crate::browser::record_applied(Some(&database), "Ready at Example Co", &address).unwrap();
        let store = Store::open(&database).unwrap();
        let ready = store
            .postings()
            .unwrap()
            .into_iter()
            .find(|posting| posting.link == "https://simplify.jobs/p/2")
            .unwrap();
        assert_eq!(ready.stage, Stage::Applied);
    }

    #[gpui_kit::test]
    fn the_queue_waits_for_run_queue_and_a_site_maverick_cannot_fill_is_marked_submitted(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let scratch = tempfile::tempdir().unwrap();
        let database = scratch.path().join("maverick.db");
        let (postings, applications) = sample();
        Store::open(&database).unwrap().add_found(&postings).unwrap();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::ui::install(cx);
        });
        let data = BoardData::new(&postings, applications, &HashMap::new());
        let background = runtime.handle().clone();
        let browser = cx.update(|cx| {
            cx.new(|cx| {
                let bridge = maverick_core::bridge::Bridge::new();
                Browser::new(bridge, background, Err("none".into()), None, Some(database.clone()), cx)
            })
        });
        let (window, board) = cx.update(|cx| {
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                    Default::default(),
                    size(px(1000.), px(640.)),
                ))),
                ..Default::default()
            };
            gpui_kit::open_window(options, cx, |window, cx| {
                let outreach = cx.new(|cx| OutreachView::new(browser.clone(), None, window, cx));
                cx.new(|cx| Board::new(browser.clone(), outreach, data, Some(database.clone()), window, cx))
            })
            .expect("the test window opens")
        });
        let decisions = || Store::open(&database).unwrap().decisions().unwrap();
        let runs = |cx: &mut TestAppContext| -> Vec<String> {
            browser.read_with(cx, |browser, _| {
                browser.queue().runs().iter().map(|run| run.address.clone()).collect()
            })
        };

        // q adds Ready and then High but not ready to the queue. Nothing opens yet.
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            board.update(cx, |board, cx| board.focus(window, cx));
            window.press("q", cx);
            window.press("q", cx);
            window.render_frame(cx);
        })
        .unwrap();
        wait_until("both are queued", || decisions().len() == 2);
        assert!(decisions().values().all(|decision| *decision == Decision::Queued));
        assert!(runs(cx).is_empty());

        // Run queue starts both, as one batch.
        cx.update_window(window, |_, window, cx| {
            assert_eq!(window.find("run-queue").label(), Some("Run queue · 2"));
            window.click("run-queue", cx);
            window.render_frame(cx);
            assert!(window.try_find("run-queue").is_none());
        })
        .unwrap();
        wait_until("both are approved", || {
            decisions().values().all(|decision| *decision == Decision::Approved)
        });
        assert_eq!(runs(cx), [READY_ASHBY, HIGH_ASHBY]);

        // Middling is on a site Maverick cannot fill. Approving it opens nothing and moves it to
        // Errors; the user applies on the site and marks it submitted.
        let selected = board.read_with(cx, |board, cx| {
            board.selected_entry(cx).map(|entry| entry.title.clone())
        });
        assert_eq!(selected.as_deref(), Some("Middling"));
        cx.update_window(window, |_, window, cx| {
            window.press("a", cx);
            window.render_frame(cx);
            assert_eq!(window.find("board-queue").label(), Some("Queue · 2"));
            assert_eq!(window.find("board-errors").label(), Some("Errors · 1"));
            window.click("board-errors", cx);
            window.render_frame(cx);
            assert_eq!(window.find("error-unfillable").label(), Some("Show can't fill site 1"));
            assert!(
                window
                    .find("failure-status")
                    .label()
                    .unwrap()
                    .starts_with("can't fill site: ")
            );
        })
        .unwrap();
        wait_until("the error is saved", || {
            Store::open(&database).unwrap().failures().unwrap()["https://simplify.jobs/p/1"].0 == Failure::Unfillable
        });
        cx.update_window(window, |_, window, cx| {
            window.click("mark-submitted", cx);
        })
        .unwrap();
        assert_eq!(runs(cx).len(), 2);
        wait_until("the application is recorded", || {
            let store = Store::open(&database).unwrap();
            let applied = store.applications().unwrap();
            applied
                .iter()
                .any(|sent| sent.link.as_deref() == Some("https://simplify.jobs/p/1"))
                && store
                    .postings()
                    .unwrap()
                    .iter()
                    .any(|posting| posting.link == "https://simplify.jobs/p/1" && posting.stage == Stage::Applied)
        });
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(window.find("board-submitted").label(), Some("Submitted · 1"));
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn marked_postings_are_approved_together_and_undone_together(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let scratch = tempfile::tempdir().unwrap();
        let database = scratch.path().join("maverick.db");
        let (postings, applications) = sample();
        Store::open(&database).unwrap().add_found(&postings).unwrap();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::ui::install(cx);
        });
        let data = BoardData::new(&postings, applications, &HashMap::new());
        let background = runtime.handle().clone();
        let browser = cx.update(|cx| {
            cx.new(|cx| {
                let bridge = maverick_core::bridge::Bridge::new();
                Browser::new(bridge, background, Err("none".into()), None, None, cx)
            })
        });
        let (window, board) = cx.update(|cx| {
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                    Default::default(),
                    size(px(1000.), px(640.)),
                ))),
                ..Default::default()
            };
            gpui_kit::open_window(options, cx, |window, cx| {
                let outreach = cx.new(|cx| OutreachView::new(browser.clone(), None, window, cx));
                cx.new(|cx| Board::new(browser.clone(), outreach, data, Some(database.clone()), window, cx))
            })
            .expect("the test window opens")
        });
        let decisions = || Store::open(&database).unwrap().decisions().unwrap();
        let queued =
            |cx: &mut TestAppContext| -> usize { browser.read_with(cx, |browser, _| browser.queue().runs().len()) };

        // x marks the selected posting and moves on, so two presses mark Ready and the one after it.
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            board.update(cx, |board, cx| board.focus(window, cx));
            window.press("x", cx);
            window.press("x", cx);
            window.render_frame(cx);
            assert_eq!(window.find("marked-count").label(), Some("2 marked"));
            window.click("approve-marked", cx);
            window.render_frame(cx);
            assert!(window.try_find("marked-count").is_none());
        })
        .unwrap();
        wait_until("both approvals are saved", || decisions().len() == 2);
        assert!(decisions().values().all(|decision| *decision == Decision::Approved));
        assert_eq!(queued(cx), 2);

        // Undo takes back the whole batch.
        cx.update_window(window, |_, window, cx| {
            window.press("cmd-z", cx);
        })
        .unwrap();
        wait_until("the undo is saved", || decisions().is_empty());

        // Command-A marks every pending posting shown; escape clears the marks.
        cx.update_window(window, |_, window, cx| {
            window.press("cmd-a", cx);
            window.render_frame(cx);
            assert_eq!(window.find("marked-count").label(), Some("4 marked"));
            window.press("escape", cx);
            window.render_frame(cx);
            assert!(window.try_find("marked-count").is_none());
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn hiding_a_company_takes_its_postings_out_until_it_is_shown_again(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let scratch = tempfile::tempdir().unwrap();
        let database = scratch.path().join("maverick.db");
        let at = |company: &str, posting: Posting| Posting {
            company: company.into(),
            ..posting
        };
        let postings = vec![
            at(
                "Big Corp",
                posting("https://simplify.jobs/p/1", "Big one", Stage::Review, Some(90), "10"),
            ),
            at(
                "Big Corp",
                posting("https://simplify.jobs/p/2", "Big two", Stage::Review, Some(80), "10"),
            ),
            at(
                "Small Co",
                posting("https://simplify.jobs/p/3", "Small one", Stage::Review, Some(70), "10"),
            ),
            at(
                "Third Inc",
                posting("https://simplify.jobs/p/4", "Third one", Stage::Review, Some(60), "10"),
            ),
        ];
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::ui::install(cx);
        });
        let data = BoardData::new(&postings, Vec::new(), &HashMap::new());
        let background = runtime.handle().clone();
        let browser = cx.update(|cx| {
            cx.new(|cx| {
                let bridge = maverick_core::bridge::Bridge::new();
                Browser::new(bridge, background, Err("none".into()), None, None, cx)
            })
        });
        let open = |cx: &mut TestAppContext, data: BoardData| {
            cx.update(|cx| {
                let options = WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                        Default::default(),
                        size(px(1000.), px(640.)),
                    ))),
                    ..Default::default()
                };
                gpui_kit::open_window(options, cx, |window, cx| {
                    let outreach = cx.new(|cx| OutreachView::new(browser.clone(), None, window, cx));
                    cx.new(|cx| Board::new(browser.clone(), outreach, data, Some(database.clone()), window, cx))
                })
                .expect("the test window opens")
            })
        };
        let (window, board) = open(cx, data.clone());
        let shown = |cx: &mut TestAppContext, board: &Entity<Board>| {
            board.read_with(cx, |board, cx| {
                let rows = board.list.read(cx).delegate();
                let selected = rows.selected_entry().map(|entry| entry.title.clone());
                (titles(&rows.entries).join(", "), selected, board.pending(cx))
            })
        };

        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            board.update(cx, |board, cx| board.focus(window, cx));
            window.press("h", cx);
            window.render_frame(cx);
            assert_eq!(window.find("filtered-out").label(), Some("2 pending hidden"));
        })
        .unwrap();
        assert_eq!(
            shown(cx, &board),
            ("Small one, Third one".into(), Some("Small one".into()), 4)
        );

        // The hidden company stays hidden in a new window, as after a restart.
        let (window, board) = open(cx, data);
        assert_eq!(
            shown(cx, &board),
            ("Small one, Third one".into(), Some("Small one".into()), 4)
        );
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.click("only-company", cx);
            window.render_frame(cx);
            assert_eq!(
                window.find("company-only").label(),
                Some("Show all companies, not only Small Co")
            );
        })
        .unwrap();
        assert_eq!(shown(cx, &board).0, "Small one");

        cx.update_window(window, |_, window, cx| {
            window.click("company-only", cx);
            window.render_frame(cx);
            window.click(ElementId::NamedInteger("unhide-company".into(), 0), cx);
            window.render_frame(cx);
            assert!(window.try_find("filtered-out").is_none());
        })
        .unwrap();
        assert_eq!(shown(cx, &board).0, "Big one, Big two, Small one, Third one");

        // The search narrows the list as the user types, and letters typed there decide nothing.
        cx.update_window(window, |_, window, cx| {
            board.update(cx, |board, cx| board.focus(window, cx));
            window.press("/", cx);
            window.input("small, third", cx);
            window.render_frame(cx);
        })
        .unwrap();
        assert_eq!(
            shown(cx, &board),
            ("Small one, Third one".into(), Some("Small one".into()), 4)
        );
        cx.update_window(window, |_, window, cx| {
            window.press("escape", cx);
            window.input("third inc a", cx);
            window.render_frame(cx);
        })
        .unwrap();
        assert_eq!(shown(cx, &board), ("Third one".into(), Some("Third one".into()), 4));
        cx.update_window(window, |_, window, cx| {
            window.press("enter", cx);
        })
        .unwrap();
        cx.update_window(window, |_, window, cx| {
            window.press("s", cx);
            window.render_frame(cx);
        })
        .unwrap();
        assert_eq!(shown(cx, &board), ("Third one".into(), Some("Third one".into()), 3));
        cx.update_window(window, |_, window, cx| {
            window.press("cmd-f", cx);
            window.press("escape", cx);
            window.render_frame(cx);
        })
        .unwrap();
        assert_eq!(shown(cx, &board).0, "Big one, Big two, Small one, Third one");
        let saved = std::fs::read_to_string(scratch.path().join("company-filter.json")).unwrap();
        assert!(!saved.contains("Big Corp"), "{saved}");

        // A search looks past hidden companies, since typing a name asks for that company.
        cx.update_window(window, |_, window, cx| {
            board.update(cx, |board, cx| board.focus(window, cx));
            window.press("h", cx);
        })
        .unwrap();
        assert_eq!(shown(cx, &board).0, "Small one, Third one");
        cx.update_window(window, |_, window, cx| {
            window.press("/", cx);
            window.input("big two", cx);
        })
        .unwrap();
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(window.find("filtered-out").label(), Some("the search looks past this"));
        })
        .unwrap();
        assert_eq!(shown(cx, &board).0, "Big two");
    }
}
