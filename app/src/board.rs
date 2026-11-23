//! The decision board: the postings that wait on the user's Approve or Skip, and the applications already sent.
//!
//! Decisions are kept in the local database, apart from the postings a sheet sync rewrites.
//! They never go to the sheet. Approving a posting puts it in the apply queue.

use std::collections::HashMap;
use std::path::PathBuf;

use crate::browser::Browser;
use crate::jobs::system_name;
use crate::main_view::header;
use crate::queue::{Run, Step};
use crate::report;
use crate::tabs::host_of;
use crate::ui::{self, Palette, Signal};
use gpui_kit::component::list::{List, ListDelegate, ListEvent, ListItem, ListState};
use gpui_kit::component::{IndexPath, h_flex, v_flex};
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, Div, ElementId, Entity, FocusHandle, InteractiveElement as _,
    IntoElement, KeyDownEvent, ParentElement as _, Render, SharedString, StatefulInteractiveElement as _, Styled as _,
    Subscription, TestSupportExt as _, Window, div, px, relative,
};
use jiff::Timestamp;
use maverick_core::apply;
use maverick_core::jobs::{Application, ApplicationStatus, Decision, Posting, Sponsorship, Stage, posting_address};
use maverick_core::score::APPLY_READY;
use maverick_core::store::Store;

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
        system_label(&self.address)
    }

    /// The name an application is recorded under.
    pub fn role(&self) -> String {
        format!("{} at {}", self.title, self.company)
    }
}

/// What the board shows: the postings and the applications.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BoardData {
    pub entries: Vec<Entry>,
    pub applications: Vec<Application>,
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
                address: own_address(
                    posting
                        .address
                        .clone()
                        .unwrap_or_else(|| posting_address(&posting.link)),
                ),
                apply_ready: posting
                    .notes
                    .as_deref()
                    .is_some_and(|notes| notes.starts_with(APPLY_READY)),
                decision: decisions.get(&posting.link).copied(),
            })
            .filter(|entry| !applied.contains(&entry.address))
            .collect();
        // Two links can lead to one posting. A decided one stands for it, so its decision shows.
        entries.sort_by_key(|entry| entry.decision.is_none());
        let mut seen = std::collections::HashSet::new();
        entries.retain(|entry| seen.insert(entry.address.clone()));
        Self { entries, applications }
    }
}

/// The application system at `address`. Maverick fills the first three. The rest open for the user.
pub fn system_label(address: &str) -> &'static str {
    if let Some(name) = system_name(address) {
        return name;
    }
    let host = host_of(address);
    let on = |domain: &str| host == domain || host.ends_with(&format!(".{domain}"));
    if on("simplify.jobs") {
        "Simplify"
    } else if on("amazon.jobs") {
        "Amazon"
    } else if on("oraclecloud.com") {
        "Oracle"
    } else if on("lever.co") {
        "Lever"
    } else if on("linkedin.com") {
        "LinkedIn"
    } else {
        "Company site"
    }
}

/// How the pending postings are ordered.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Order {
    /// Apply-ready first, then the best score.
    TopScore,
    /// The most recently found first.
    Latest,
}

/// How the applications are ordered.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SubmittedOrder {
    /// The most recently applied first.
    Latest,
    Company,
}

/// Puts the entries in `order`. Decided entries always come after the pending ones.
pub fn sort(entries: &mut [Entry], order: Order) {
    entries.sort_by(|a, b| {
        let pending_first = a.decision.is_some().cmp(&b.decision.is_some());
        pending_first.then_with(|| match order {
            Order::TopScore => b
                .apply_ready
                .cmp(&a.apply_ready)
                .then(b.score.cmp(&a.score))
                .then(b.found_at.cmp(&a.found_at)),
            Order::Latest => b.found_at.cmp(&a.found_at).then(b.score.cmp(&a.score)),
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

/// Which list the board shows.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Section {
    Pending,
    Submitted,
}

pub struct Rows {
    browser: Entity<Browser>,
    section: Section,
    entries: Vec<Entry>,
    submitted: Vec<Application>,
    selected: Option<IndexPath>,
}

impl Rows {
    fn shown(&self) -> usize {
        match self.section {
            Section::Pending => self.entries.len(),
            Section::Submitted => self.submitted.len(),
        }
    }

    fn selected_entry(&self) -> Option<&Entry> {
        (self.section == Section::Pending)
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
        Step::Queued | Step::Opening | Step::Filling => ui::boxed(step.word(), palette.muted),
        Step::NeedsYou(_) | Step::Ready => ui::chip(Signal::Yours, step.word(), palette),
        Step::Applied => ui::chip(Signal::Done, step.word(), palette),
        Step::Failed(_) => ui::chip(Signal::Failed, step.word(), palette),
    }
}

fn decision_chip(decision: Decision, palette: &Palette) -> Div {
    match decision {
        Decision::Approved => ui::chip(Signal::Done, "approved", palette),
        Decision::Skipped => ui::boxed("skipped", palette.faint),
    }
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
            Section::Pending => {
                let entry = self.entries.get(ix.row)?;
                let run = self.browser.read(cx).queue().get(&entry.address).cloned();
                let tone = match (entry.decision, entry.score) {
                    _ if selected => palette.ink,
                    (Some(_), _) => palette.faint,
                    (None, Some(score)) if score < 60 => palette.faint,
                    _ => palette.muted,
                };
                let mut from = vec![entry.company.clone(), entry.system().to_owned()];
                from.extend(entry.variant.clone());
                let mut more = vec![entry.found_at.strftime("%b %-d").to_string()];
                more.extend(entry.location.clone());
                more.extend(entry.flags.clone());
                h_flex()
                    .gap_3()
                    .child(ui::score(entry.score, 19.).w(px(30.)).text_color(tone))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(ui::serif(entry.title.clone()).truncate().text_color(tone))
                            .child(ui::caps(from.join(" / ")).truncate().text_color(palette.faint))
                            .child(ui::caps(more.join(" · ")).truncate().text_color(palette.faint)),
                    )
                    .children(
                        entry
                            .apply_ready
                            .then(|| ui::chip(Signal::Yours, "apply-ready", &palette)),
                    )
                    .child(sponsorship_chip(entry.sponsorship, &palette))
                    .children(match (&run, entry.decision) {
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
        let message = match self.section {
            Section::Pending => "Nothing waits on a decision. New postings arrive with the hourly check.",
            Section::Submitted => "No applications are recorded yet.",
        };
        ui::serif(message).p_4().text_color(Palette::of(cx).muted)
    }

    fn set_selected_index(&mut self, ix: Option<IndexPath>, _: &mut Window, cx: &mut Context<ListState<Self>>) {
        self.selected = ix;
        cx.notify();
    }
}

pub struct Board {
    browser: Entity<Browser>,
    database: Option<PathBuf>,
    list: Entity<ListState<Rows>>,
    order: Order,
    submitted_order: SubmittedOrder,
    /// Each decision made in this run, newest last, with the decision it replaced.
    made: Vec<(String, Option<Decision>)>,
    /// Why the last decision could not be saved.
    problem: Option<String>,
    focus: FocusHandle,
    /// How many of the applications sent in this run the board has moved to Submitted.
    moved: usize,
    _subscriptions: Vec<Subscription>,
}

impl Board {
    pub fn new(
        browser: Entity<Browser>,
        data: BoardData,
        database: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let rows = Rows {
            browser: browser.clone(),
            section: Section::Pending,
            entries: Vec::new(),
            submitted: Vec::new(),
            selected: None,
        };
        let list = cx.new(|cx| ListState::new(rows, window, cx));
        let _subscriptions = vec![
            cx.subscribe_in(&list, window, |this, _, event: &ListEvent, _, cx| {
                // Enter on a posting with an open tab brings the tab forward.
                if let ListEvent::Confirm(_) = event {
                    this.show_tab(cx);
                }
                cx.notify();
            }),
            cx.observe_in(&browser, window, |this, _, window, cx| this.move_applied(window, cx)),
        ];
        let mut board = Self {
            browser,
            database,
            list,
            order: Order::TopScore,
            submitted_order: SubmittedOrder::Latest,
            made: Vec::new(),
            problem: None,
            focus: cx.focus_handle(),
            moved: 0,
            _subscriptions,
        };
        board.replace(data, window, cx);
        board
    }

    /// Shows what the database holds now, keeping the section and the orders.
    pub fn replace(&mut self, data: BoardData, window: &mut Window, cx: &mut Context<Self>) {
        let (order, submitted_order) = (self.order, self.submitted_order);
        self.list.update(cx, |list, cx| {
            let rows = list.delegate_mut();
            rows.entries = data.entries;
            rows.submitted = data.applications;
            sort(&mut rows.entries, order);
            sort_submitted(&mut rows.submitted, submitted_order);
            select_row(list, 0, window, cx);
        });
        cx.notify();
    }

    /// How many postings wait on a decision.
    pub fn pending(&self, cx: &App) -> usize {
        let rows = self.list.read(cx).delegate();
        rows.entries.iter().filter(|entry| entry.decision.is_none()).count()
    }

    pub fn focus(&self, window: &mut Window, cx: &mut App) {
        self.list.update(cx, |list, cx| list.focus(window, cx));
    }

    fn show(&mut self, section: Section, window: &mut Window, cx: &mut Context<Self>) {
        self.list.update(cx, |list, cx| {
            list.delegate_mut().section = section;
            select_row(list, 0, window, cx);
        });
        self.focus(window, cx);
        cx.notify();
    }

    fn set_order(&mut self, order: Order, window: &mut Window, cx: &mut Context<Self>) {
        self.order = order;
        self.list.update(cx, |list, cx| {
            sort(&mut list.delegate_mut().entries, order);
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

    /// Decides the selected posting. The selection stays in place, so it lands on the next pending one.
    pub fn decide(&mut self, decision: Decision, window: &mut Window, cx: &mut Context<Self>) {
        let order = self.order;
        let decided = self.list.update(cx, |list, cx| {
            let row = list.selected_index()?.row;
            let rows = list.delegate_mut();
            if rows.section != Section::Pending {
                return None;
            }
            let entry = rows.entries.get_mut(row)?;
            let before = entry.decision.replace(decision);
            let link = entry.link.clone();
            sort(&mut rows.entries, order);
            select_row(list, row, window, cx);
            Some((link, before))
        });
        if let Some((link, before)) = decided {
            self.save(link.clone(), Some(decision), cx);
            self.sync_queue(&link, Some(decision), cx);
            self.made.push((link, before));
        }
        cx.notify();
    }

    /// Puts an approved posting in the apply queue, and takes any other out while it waits.
    fn sync_queue(&mut self, link: &str, decision: Option<Decision>, cx: &mut Context<Self>) {
        let rows = self.list.read(cx).delegate();
        let Some(entry) = rows.entries.iter().find(|entry| entry.link == link) else {
            return;
        };
        let (role, address) = (entry.role(), entry.address.clone());
        self.browser.update(cx, |browser, cx| match decision {
            Some(Decision::Approved) => browser.enqueue(role, address, cx),
            _ => browser.withdraw(&address, cx),
        });
    }

    /// Queues every approved posting that is not in the queue yet, such as those approved before a restart.
    fn apply_approved(&mut self, cx: &mut Context<Self>) {
        let waiting: Vec<String> = self.waiting_approved(cx).into_iter().map(|entry| entry.link).collect();
        for link in waiting {
            self.sync_queue(&link, Some(Decision::Approved), cx);
        }
    }

    /// Approved postings that have not been queued in this run.
    fn waiting_approved(&self, cx: &App) -> Vec<Entry> {
        let queue = self.browser.read(cx).queue();
        self.list
            .read(cx)
            .delegate()
            .entries
            .iter()
            .filter(|entry| entry.decision == Some(Decision::Approved) && queue.get(&entry.address).is_none())
            .cloned()
            .collect()
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
        let submitted_order = self.submitted_order;
        self.list.update(cx, |list, cx| {
            let row = list.selected_index().map_or(0, |ix| ix.row);
            let rows = list.delegate_mut();
            for sent in new {
                rows.entries.retain(|entry| entry.address != sent.address);
                rows.submitted
                    .push(Application::submitted_today(&sent.title, &sent.address));
            }
            sort_submitted(&mut rows.submitted, submitted_order);
            select_row(list, row, window, cx);
        });
        cx.notify();
    }

    /// Takes back the last decision and selects its posting again.
    pub fn undo(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((link, before)) = self.made.pop() else {
            return;
        };
        let order = self.order;
        self.list.update(cx, |list, cx| {
            let rows = list.delegate_mut();
            rows.section = Section::Pending;
            if let Some(entry) = rows.entries.iter_mut().find(|entry| entry.link == link) {
                entry.decision = before;
            }
            sort(&mut rows.entries, order);
            let row = rows.entries.iter().position(|entry| entry.link == link).unwrap_or(0);
            select_row(list, row, window, cx);
        });
        self.save(link.clone(), before, cx);
        self.sync_queue(&link, before, cx);
        cx.notify();
    }

    /// Writes a decision to the database off the main thread. A failure shows on the board.
    fn save(&mut self, link: String, decision: Option<Decision>, cx: &mut Context<Self>) {
        let database = self.database.clone();
        let saved = self.browser.read(cx).background().spawn_blocking(move || {
            let database = database.ok_or("this system has no application data directory")?;
            Store::open(&database)
                .and_then(|mut store| store.decide(&link, decision))
                .map_err(|error| error.to_string())
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

    fn on_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let modifiers = event.keystroke.modifiers;
        match event.keystroke.key.as_str() {
            "z" if modifiers.platform => self.undo(window, cx),
            _ if modifiers.modified() => return,
            "a" => self.decide(Decision::Approved, window, cx),
            "s" => self.decide(Decision::Skipped, window, cx),
            _ => return,
        }
        cx.stop_propagation();
    }

    /// The board's controls: which list, its order, and undo.
    fn toolbar(&self, palette: &Palette, cx: &mut Context<Self>) -> Div {
        let rows = self.list.read(cx).delegate();
        let section = rows.section;
        let pending = rows.entries.iter().filter(|entry| entry.decision.is_none()).count();
        let sent = rows.submitted.len();
        let choice = |id: &'static str, name: String, word: String, chosen: bool| {
            let ink = palette.ink;
            let face = ui::caps(word)
                .text_color(if chosen { palette.ink } else { palette.faint })
                .hover(move |face| face.text_color(ink));
            ui::button(id, name, face)
        };
        let orders: Vec<AnyElement> = match section {
            Section::Pending => [
                (Order::TopScore, "sort-score", "top score"),
                (Order::Latest, "sort-latest", "latest"),
            ]
            .into_iter()
            .map(|(order, id, word)| {
                choice(id, format!("Sort by {word}"), word.to_owned(), self.order == order)
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
        let waiting_approved = self.waiting_approved(cx).len();
        if waiting_approved > 0 {
            controls.push(
                ui::button(
                    "apply-approved",
                    format!("Apply approved · {waiting_approved}"),
                    ui::quiet_face(format!("apply approved {waiting_approved}"), palette),
                )
                .tip("Queue the postings approved before Maverick last started.")
                .on_click(cx.listener(|this, _, _, cx| this.apply_approved(cx)))
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
            return v_flex()
                .size_full()
                .child(header("submitted", &role, None, palette))
                .child(div().px_4().py_3().child(ui::facts(&facts, palette)));
        }
        let Some(entry) = rows.selected_entry() else {
            return v_flex();
        };
        let sponsorship = match entry.sponsorship {
            Sponsorship::Sponsors => "sponsors",
            Sponsorship::NoSponsorship => "no sponsorship",
            Sponsorship::Unknown => "unknown",
        };
        let facts = [
            ("variant", entry.variant.clone().unwrap_or_default()),
            ("flags", entry.flags.clone().unwrap_or_default()),
            ("visa", sponsorship.to_owned()),
            ("location", entry.location.clone().unwrap_or_default()),
            ("found", entry.found_at.strftime("%b %-d, %Y").to_string()),
            ("source", entry.source.clone()),
            ("system", entry.system().to_owned()),
            ("address", entry.address.clone()),
            ("verdict", entry.notes.clone().unwrap_or_default()),
        ];
        let tag = format!("{} / {}", entry.company, entry.system());
        let run = match entry.decision {
            Some(Decision::Approved) => self.browser.read(cx).queue().get(&entry.address).cloned(),
            _ => None,
        };
        let note = match (&entry.decision, run.as_ref().map(|run| &run.step)) {
            (None, _) => "approve queues it. nothing gets submitted.",
            (Some(Decision::Skipped), _) => "skipped.",
            (Some(Decision::Approved), None) => "approve again to queue it.",
            (_, Some(Step::Queued | Step::Opening | Step::Filling)) => "filling. nothing gets submitted.",
            (_, Some(Step::NeedsYou(_) | Step::Ready)) => "you click submit.",
            (_, Some(Step::Applied)) => "applied.",
            (_, Some(Step::Failed(_))) => "nothing was submitted.",
        };
        let (word, name) = match run.as_ref().map(|run| &run.step) {
            None if entry.decision == Some(Decision::Approved) => ("apply", "Apply"),
            None => ("approve", "Approve"),
            Some(Step::Failed(_)) => ("try again", "Try again"),
            Some(_) => ("", ""),
        };
        let reviewable = run
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
        v_flex()
            .size_full()
            .child(header(&tag, &entry.title, Some(entry.score), palette))
            .children(run.as_ref().map(|run| run_status(run, palette)))
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
                        None => ui::facts(&facts, palette),
                    }),
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
                    .child(
                        ui::button("skip", "Skip", ui::quiet_face("skip  s", palette))
                            .on_click(cx.listener(|this, _, window, cx| this.decide(Decision::Skipped, window, cx))),
                    )
                    .child(ui::serif(note).ml_auto().text_size(px(13.5)).text_color(palette.muted)),
            )
    }
}

/// Where a queued posting is, and the fill report's map once there is one.
fn run_status(run: &Run, palette: &Palette) -> impl IntoElement + use<> {
    let (signal, text) = match &run.step {
        Step::Queued => (None, "Queued. It starts when a slot is free.".to_owned()),
        Step::Opening => (None, "Opening the form in a background tab.".to_owned()),
        Step::Filling => (Some(Signal::Model), "Filling the form in a background tab.".to_owned()),
        Step::NeedsYou(why) => (Some(Signal::Yours), why.clone()),
        Step::Ready => (
            Some(Signal::Yours),
            "Every required field is answered. Review it and click Submit.".to_owned(),
        ),
        Step::Applied => (Some(Signal::Done), "Submitted and recorded.".to_owned()),
        Step::Failed(why) => (Some(Signal::Failed), why.clone()),
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
                        div()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .border_r_1()
                            .border_color(palette.line)
                            .child(List::new(&self.list)),
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
                ..posting("https://simplify.jobs/p/2", "Ready", Stage::Review, Some(77), "08")
            },
            posting(
                "https://simplify.jobs/p/3",
                "High but not ready",
                Stage::Review,
                Some(90),
                "09",
            ),
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

        sort(&mut entries, Order::TopScore);
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
        assert_eq!(entries[0].system(), "Simplify");

        sort(&mut entries, Order::Latest);
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
    }

    #[test]
    fn each_posting_names_its_application_system() {
        for (address, system) in [
            (ASHBY, "Ashby"),
            ("https://job-boards.greenhouse.io/example/jobs/123", "Greenhouse"),
            ("https://www.amazon.jobs/en/jobs/123/engineer", "Amazon"),
            ("https://example.fa.us2.oraclecloud.com/hcmUI/job/1", "Oracle"),
            ("https://jobs.lever.co/example/1", "Lever"),
            ("https://simplify.jobs/p/9", "Simplify"),
            ("https://careers.example.com/jobs/1", "Company site"),
        ] {
            assert_eq!(system_label(address), system, "{address}");
        }
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
                cx.new(|cx| Board::new(browser.clone(), data, Some(database.clone()), window, cx))
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
        // Decided rows sink below the pending ones, so the selection lands on the next pending posting.
        assert_eq!(
            shown(cx),
            (
                "Middling, Unscored, Ready, High but not ready".into(),
                Some("Middling".into()),
                2
            )
        );

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
        assert_eq!(
            shown(cx),
            (
                "Unscored, Middling, High but not ready, Ready".into(),
                Some("Unscored".into()),
                2
            )
        );

        cx.update_window(window, |_, window, cx| {
            window.click("board-submitted", cx);
            window.render_frame(cx);
            assert_eq!(window.find("board-submitted").label(), Some("Submitted · 1"));
            assert!(window.try_find("approve").is_none());
        })
        .unwrap();
    }
}
