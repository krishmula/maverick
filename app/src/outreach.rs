//! The Outreach view: who to reach at each company, and the LinkedIn drafts waiting for the user.
//!
//! Approving a draft types it into LinkedIn in the user's Chrome and stops. The user clicks Send.
//! While the app is open, checks run every hour and the hiring-post sweep every two hours.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::component::input::{Textarea, TextareaState};
use gpui_kit::component::{h_flex, v_flex};
use gpui_kit::{
    AnyElement, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement, ParentElement as _, Render,
    SharedString, StatefulInteractiveElement as _, Styled as _, Task, TestSupportExt as _, Window, div, px,
};
use jiff::Timestamp;
use maverick_core::answers::Answers;
use maverick_core::bridge::{Bridge, BrowserState};
use maverick_core::jobs::{HiringPost, Outreach, Posting};
use maverick_core::linkedin::Slot;
use maverick_core::outreach::{self, About, Check, Draft, Verdict};
use maverick_core::score::Profile;
use maverick_core::store::Store;
use tokio::runtime::Handle;

use crate::browser::Browser;
use crate::ui::{self, Palette, Signal};

/// The wait after launch before the first outreach run, after the funnel's first pass.
const FIRST_RUN: Duration = Duration::from_secs(5 * 60);
const BETWEEN_RUNS: Duration = Duration::from_secs(60 * 60);
/// The sweep runs on every second run, so every two hours.
const SWEEP_EVERY: u32 = 2;

/// Which checks the list shows.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Filter {
    PeopleFound,
    ApplyCold,
    All,
}

impl Filter {
    const ALL: [Filter; 3] = [Filter::PeopleFound, Filter::ApplyCold, Filter::All];

    fn name(self) -> &'static str {
        match self {
            Filter::PeopleFound => "people found",
            Filter::ApplyCold => "apply cold",
            Filter::All => "all",
        }
    }

    fn keeps(self, verdict: Verdict) -> bool {
        match self {
            Filter::PeopleFound => verdict.found_people(),
            Filter::ApplyCold => !verdict.found_people(),
            Filter::All => true,
        }
    }
}

/// A check with the posting it is about.
#[derive(Clone, Debug)]
pub struct CheckRow {
    pub check: Check,
    pub company: String,
    pub title: String,
    pub score: Option<u8>,
}

/// A draft with what it is about, as one line.
#[derive(Clone, Debug)]
pub struct DraftRow {
    pub draft: Draft,
    pub about: String,
    pub score: Option<u8>,
}

/// Reads the checks and drafts from the store, with the postings and posts they are about.
/// The slot a draft is in, as its card labels it.
/// A slot as a sentence names it.
fn slot_name(slot: Slot) -> &'static str {
    match slot {
        Slot::Note => "connection note",
        Slot::Message => "message",
        Slot::InMail => "InMail",
    }
}

pub fn slot_label(draft: &Draft) -> String {
    match draft.slot {
        Slot::Note => format!("connection note · {} chars", draft.text.chars().count()),
        Slot::Message => "message".to_owned(),
        Slot::InMail => format!("inmail · subject: {}", draft.subject.as_deref().unwrap_or("none")),
    }
}

pub fn rows(store: &Store) -> (Vec<CheckRow>, Vec<DraftRow>) {
    let postings: Vec<Posting> = store.postings().unwrap_or_default();
    let posts: Vec<HiringPost> = store.hiring_posts().unwrap_or_default();
    let posting_of = |link: &str| postings.iter().find(|posting| posting.link == link);
    let checks = store
        .checks()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|check| {
            let posting = posting_of(&check.posting_link)?;
            Some(CheckRow {
                company: posting.company.clone(),
                title: posting.title.clone(),
                score: posting.score,
                check,
            })
        })
        .collect();
    let drafts = store
        .drafts()
        .unwrap_or_default()
        .into_iter()
        .map(|draft| {
            let (about, score) = match &draft.about {
                About::Posting(link) => match posting_of(link) {
                    Some(posting) => (format!("{} at {}", posting.title, posting.company), posting.score),
                    None => (link.clone(), None),
                },
                About::Post(link) => match posts.iter().find(|post| &post.post_link == link) {
                    Some(post) => (format!("{} at {} · post", post.role, post.company), Some(post.score)),
                    None => (link.clone(), None),
                },
            };
            DraftRow { draft, about, score }
        })
        .collect();
    (checks, drafts)
}

/// A row that can have work of its own: a posting's check on the left, or a draft on the right.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum RowKey {
    Posting(String),
    Draft(i64),
}

/// What a row's own work is doing or did, shown on that row.
#[derive(Clone, Debug, PartialEq)]
pub enum RowState {
    /// Running, such as "drafting…". The row's button is disabled meanwhile.
    Working(String),
    Done(String),
    Failed(String),
}

/// What a row's work leaves behind: what to say, and a draft to outline.
#[derive(Debug)]
pub struct Finished {
    pub said: String,
    pub draft: Option<i64>,
}

impl Finished {
    fn said(said: impl Into<String>) -> Self {
        Self {
            said: said.into(),
            draft: None,
        }
    }
}

pub struct OutreachView {
    browser: Entity<Browser>,
    background: Handle,
    database: Option<PathBuf>,
    answers: Option<PathBuf>,
    filter: Filter,
    checks: Vec<CheckRow>,
    drafts: Vec<DraftRow>,
    /// The draft open in the editor.
    editing: Option<i64>,
    editor: Entity<TextareaState>,
    /// The check-and-sweep run going now, if any.
    working: Option<String>,
    /// What the last check-and-sweep run left to say, and whether it worked.
    said: Option<(Signal, String)>,
    /// Each row's own work. Rows work independently, so a click on one never waits on another.
    row_states: HashMap<RowKey, RowState>,
    /// The draft a row's work just made or changed, outlined so the user sees where it went.
    outlined: Option<i64>,
    /// Chrome has one active tab, so drafts are typed one at a time; the rest wait their turn.
    typing: Arc<tokio::sync::Mutex<()>>,
    _schedule: Option<Task<()>>,
}

impl OutreachView {
    pub fn new(
        browser: Entity<Browser>,
        answers: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let background = browser.read(cx).background();
        let database = browser.read(cx).database();
        let editor = cx.new(|cx| TextareaState::new(window, cx).auto_grow(3, 10));
        let mut view = Self {
            browser,
            background,
            database,
            answers,
            filter: Filter::PeopleFound,
            checks: Vec::new(),
            drafts: Vec::new(),
            editing: None,
            editor,
            working: None,
            said: None,
            row_states: HashMap::new(),
            outlined: None,
            typing: Arc::new(tokio::sync::Mutex::new(())),
            _schedule: None,
        };
        view.reload();
        view
    }

    /// Drafts waiting for the user's decision, for the bar.
    pub fn waiting(&self) -> usize {
        self.drafts
            .iter()
            .filter(|row| row.draft.status == Outreach::Drafted)
            .count()
    }

    pub fn reload(&mut self) {
        let store = self
            .database
            .as_deref()
            .filter(|path| path.is_file())
            .and_then(|path| Store::open(path).ok());
        if let Some(store) = store {
            (self.checks, self.drafts) = rows(&store);
        }
    }

    fn bridge(&self, cx: &Context<Self>) -> Option<Arc<Bridge>> {
        let browser = self.browser.read(cx);
        matches!(browser.state(), BrowserState::Connected { .. }).then(|| browser.bridge())
    }

    fn profile(&self) -> Result<Profile, String> {
        let file = self
            .answers
            .as_deref()
            .ok_or("This system has no application data directory.")?;
        Ok(Profile::from(&Answers::load(file)?))
    }

    /// Runs a check-and-sweep on the background runtime, then reloads and shows what it said at the
    /// top of the drafts. Only one runs at a time; its button is hidden meanwhile, and the schedule
    /// skips a turn while one runs.
    fn run(
        &mut self,
        doing: impl Into<String>,
        work: impl Future<Output = Result<String, String>> + Send + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.working.is_some() {
            return;
        }
        self.working = Some(doing.into());
        self.said = None;
        cx.notify();
        let done = self.background.spawn(work);
        cx.spawn_in(window, async move |this, cx| {
            let said = match done.await {
                Ok(Ok(line)) => (Signal::Done, line),
                Ok(Err(error)) => (Signal::Failed, error),
                Err(stopped) => (Signal::Failed, stopped.to_string()),
            };
            this.update(cx, |this, cx| {
                this.working = None;
                this.said = Some(said);
                this.reload();
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// What `key`'s row is doing or did.
    #[cfg(test)]
    pub fn row_state(&self, key: &RowKey) -> Option<&RowState> {
        self.row_states.get(key)
    }

    fn row_failed(&mut self, key: RowKey, why: impl Into<String>, cx: &mut Context<Self>) {
        self.row_states.insert(key, RowState::Failed(why.into()));
        cx.notify();
    }

    /// Runs one row's work on the background runtime. The row shows `doing` at once and then what
    /// came of it. Rows run side by side; only the same row cannot start twice.
    pub fn run_row(
        &mut self,
        key: RowKey,
        doing: impl Into<String>,
        work: impl Future<Output = Result<Finished, String>> + Send + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if matches!(self.row_states.get(&key), Some(RowState::Working(_))) {
            return;
        }
        self.row_states.insert(key.clone(), RowState::Working(doing.into()));
        cx.notify();
        let done = self.background.spawn(work);
        cx.spawn_in(window, async move |this, cx| {
            let outcome = done.await.unwrap_or_else(|stopped| Err(stopped.to_string()));
            this.update(cx, |this, cx| {
                let state = match outcome {
                    Ok(finished) => {
                        if finished.draft.is_some() {
                            this.outlined = finished.draft;
                        }
                        RowState::Done(finished.said)
                    }
                    Err(why) => RowState::Failed(why),
                };
                this.row_states.insert(key, state);
                this.reload();
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn database_or_fail(&mut self, cx: &mut Context<Self>) -> Option<PathBuf> {
        if self.database.is_none() {
            self.said = Some((Signal::Failed, "This system has no application data directory.".into()));
            cx.notify();
        }
        self.database.clone()
    }

    /// A disabled button face for a row whose work is running, in the model's color.
    fn pending(id: String, doing: &str, palette: &Palette) -> impl IntoElement + use<> {
        div()
            .id(SharedString::from(id))
            .test_support()
            .role(gpui_kit::Role::Button)
            .aria_label(doing.to_owned())
            .child(ui::boxed(doing, palette.signal(Signal::Model)))
    }

    /// The line under a row that says what its work came to.
    fn row_said(&self, key: &RowKey, id: String, palette: &Palette) -> Option<impl IntoElement + use<>> {
        let (signal, text) = match self.row_states.get(key)? {
            RowState::Working(_) => return None,
            RowState::Done(text) => (Signal::Done, text.clone()),
            RowState::Failed(text) => (Signal::Failed, text.clone()),
        };
        Some(
            h_flex()
                .id(SharedString::from(id))
                .test_support()
                .role(gpui_kit::Role::Label)
                .aria_label(text.clone())
                .w_full()
                .min_w_0()
                .items_start()
                .gap(px(8.))
                .child(ui::square(palette.signal(signal)).mt(px(5.)))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_size(px(12.))
                        .text_color(palette.signal(signal))
                        .child(SharedString::from(text)),
                ),
        )
    }

    fn database_for(&mut self, key: &RowKey, cx: &mut Context<Self>) -> Option<PathBuf> {
        if self.database.is_none() {
            self.row_failed(key.clone(), "This system has no application data directory.", cx);
        }
        self.database.clone()
    }

    /// The user's profile from answers.json, or `None` after saying on the row why it could not be read.
    fn profile_for(&mut self, key: &RowKey, cx: &mut Context<Self>) -> Option<Profile> {
        match self.profile() {
            Ok(profile) => Some(profile),
            Err(error) => {
                self.row_failed(key.clone(), format!("Could not read your answers: {error}"), cx);
                None
            }
        }
    }

    /// Types an approved draft in the user's Chrome. It stops before Send.
    fn approve(&mut self, id: i64, window: &mut Window, cx: &mut Context<Self>) {
        let key = RowKey::Draft(id);
        let Some(database) = self.database_for(&key, cx) else {
            return;
        };
        let Some(bridge) = self.bridge(cx) else {
            self.row_failed(key, "Chrome is not connected, so nothing was typed.", cx);
            return;
        };
        let typing = self.typing.clone();
        self.run_row(
            key,
            "typing in chrome…",
            async move {
                // Another draft may be typing; this one waits for the tab to be free.
                let _turn = typing.lock().await;
                outreach::type_approved(&database, bridge, id).await?;
                Ok(Finished::said(
                    "Typed in Chrome. Read it there and click Send yourself, then mark it sent.",
                ))
            },
            window,
            cx,
        );
    }

    fn set_status(&mut self, id: i64, status: Outreach, cx: &mut Context<Self>) {
        let key = RowKey::Draft(id);
        let Some(database) = self.database_for(&key, cx) else {
            return;
        };
        let saved = Store::open(&database).and_then(|mut store| store.set_draft_status(id, status, Timestamp::now()));
        match saved {
            Ok(()) => {
                self.row_states.remove(&key);
            }
            Err(error) => self.row_failed(key, error.to_string(), cx),
        }
        self.reload();
        cx.notify();
    }

    /// Puts a skipped or typed draft back to waiting for approval. Nothing else changes.
    fn reset(&mut self, id: i64, cx: &mut Context<Self>) {
        let key = RowKey::Draft(id);
        let Some(database) = self.database_for(&key, cx) else {
            return;
        };
        let reset = Store::open(&database)
            .map_err(|error| error.to_string())
            .and_then(|store| store.reset_draft(id, Timestamp::now()));
        let state = match reset {
            Ok(()) => RowState::Done("Back to waiting for your approval.".into()),
            Err(why) => RowState::Failed(why),
        };
        self.row_states.insert(key, state);
        self.reload();
        cx.notify();
    }

    fn edit(&mut self, id: i64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self.drafts.iter().find(|row| row.draft.id == id) else {
            return;
        };
        let text = row.draft.text.clone();
        self.editor.update(cx, |editor, cx| editor.set_value(text, window, cx));
        self.editing = Some(id);
        cx.notify();
    }

    fn save_edit(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.editing else {
            return;
        };
        let key = RowKey::Draft(id);
        let Some(database) = self.database_for(&key, cx) else {
            return;
        };
        let Some(row) = self.drafts.iter().find(|row| row.draft.id == id) else {
            return;
        };
        let text = self.editor.read(cx).value().to_string();
        let state = match outreach::check_draft(&text, row.draft.slot) {
            Ok(text) => {
                self.editing = None;
                match Store::open(&database).and_then(|store| store.set_draft_text(id, &text, Timestamp::now())) {
                    Ok(()) => RowState::Done("Draft saved.".into()),
                    Err(error) => RowState::Failed(error.to_string()),
                }
            }
            Err(why) => RowState::Failed(format!("Not saved: {why}.")),
        };
        self.row_states.insert(key, state);
        self.reload();
        cx.notify();
    }

    /// Drafts a note for a posting's check. The row shows "drafting…", then where the draft went.
    fn draft_for(&mut self, posting_link: String, window: &mut Window, cx: &mut Context<Self>) {
        let key = RowKey::Posting(posting_link.clone());
        let Some(database) = self.database_for(&key, cx) else {
            return;
        };
        let Some(profile) = self.profile_for(&key, cx) else {
            return;
        };
        self.run_row(
            key,
            "drafting…",
            async move {
                let draft = outreach::draft_for_check(&database, &profile, &posting_link).await?;
                Ok(Finished {
                    said: format!(
                        "Drafted a {} to {}. It is first under Drafts, outlined.",
                        slot_name(draft.slot),
                        draft.person
                    ),
                    draft: Some(draft.id),
                })
            },
            window,
            cx,
        );
    }

    /// Rewrites a waiting draft as an InMail, or an InMail back as a connection note.
    fn switch_form(&mut self, id: i64, window: &mut Window, cx: &mut Context<Self>) {
        let key = RowKey::Draft(id);
        let Some(database) = self.database_for(&key, cx) else {
            return;
        };
        let Some(profile) = self.profile_for(&key, cx) else {
            return;
        };
        self.run_row(
            key,
            "rewriting…",
            async move {
                let draft = outreach::redraft(&database, &profile, id).await?;
                Ok(Finished {
                    said: match draft.slot {
                        Slot::InMail => {
                            "Rewritten as an InMail. Read it before you approve; sending it uses a credit.".into()
                        }
                        _ => "Rewritten as a connection note.".into(),
                    },
                    draft: Some(draft.id),
                })
            },
            window,
            cx,
        );
    }

    /// One outreach run: checks every posting that needs one, then, when `sweep`, the hiring posts.
    fn run_now(&mut self, sweep: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(database) = self.database_or_fail(cx) else {
            return;
        };
        let bridge = self.bridge(cx);
        let profile = self.profile();
        let doing = if sweep {
            "checking postings and sweeping hiring posts"
        } else {
            "checking postings"
        };
        self.run(
            doing,
            async move {
                let mut lines = vec![
                    outreach::check_postings(&database, bridge.clone(), &[])
                        .await?
                        .summary(),
                ];
                if sweep {
                    let bridge = bridge.ok_or("Chrome is not connected, so the hiring-post sweep did not run.")?;
                    lines.push(outreach::sweep(&database, &profile?, bridge).await?.summary());
                }
                Ok(lines.join("; "))
            },
            window,
            cx,
        );
    }

    /// Runs outreach a few minutes after launch and then hourly in daytime hours, sweeping every second time.
    pub fn start_schedule(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self._schedule = Some(cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(FIRST_RUN).await;
            let mut run = 0u32;
            loop {
                if maverick_core::funnel::in_hours(Timestamp::now()) {
                    let sweep = run.is_multiple_of(SWEEP_EVERY);
                    run += 1;
                    let started = this.update_in(cx, |this, window, cx| this.run_now(sweep, window, cx));
                    if started.is_err() {
                        break;
                    }
                }
                cx.background_executor().timer(BETWEEN_RUNS).await;
            }
        }));
    }

    fn checks_column(&self, palette: &Palette, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let filters = h_flex().gap(px(6.)).children(Filter::ALL.map(|filter| {
            let count = self.checks.iter().filter(|row| filter.keeps(row.check.verdict)).count();
            let text = format!("{} · {count}", filter.name());
            let face = if filter == self.filter {
                ui::boxed(&text, palette.ink).py(px(1.))
            } else {
                ui::quiet_face(&text, palette)
            };
            ui::button(
                SharedString::from(format!("filter-{}", filter.name())),
                text.clone(),
                face,
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                this.filter = filter;
                cx.notify();
            }))
        }));
        let rows: Vec<AnyElement> = self
            .checks
            .iter()
            .filter(|row| self.filter.keeps(row.check.verdict))
            .map(|row| self.check_row(row, palette, cx).into_any_element())
            .collect();
        let empty = rows.is_empty().then(|| {
            ui::serif("Nothing here yet. Checks run for postings to review, apply-ready first.")
                .text_color(palette.muted)
        });
        v_flex()
            .id("outreach-checks")
            .flex_1()
            .min_w_0()
            .h_full()
            .overflow_y_scroll()
            .px(px(20.))
            .py(px(16.))
            .gap(px(12.))
            .border_r_1()
            .border_color(palette.line)
            .child(
                h_flex()
                    .justify_between()
                    .child(ui::caps("per-job verdicts").text_color(palette.muted))
                    .child(filters),
            )
            .children(empty)
            .children(rows)
    }

    fn check_row(&self, row: &CheckRow, palette: &Palette, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let verdict = row.check.verdict;
        let signal = match verdict {
            Verdict::Connection | Verdict::TeamMember | Verdict::HiringManager => Signal::Done,
            Verdict::Deferred => Signal::Yours,
            Verdict::Cold => Signal::Failed,
        };
        let label = match (verdict, &row.check.team) {
            (Verdict::Deferred, Some(_)) => "team named, lookup deferred".to_owned(),
            (verdict, _) => verdict.label().to_lowercase(),
        };
        let has_draft = self
            .drafts
            .iter()
            .any(|draft| draft.draft.about == About::Posting(row.check.posting_link.clone()));
        let can_draft = !has_draft && row.check.people.iter().any(|person| person.profile_url.is_some());
        let link = row.check.posting_link.clone();
        let key = RowKey::Posting(link.clone());
        let draft_button: Option<AnyElement> = match self.row_states.get(&key) {
            Some(RowState::Working(doing)) => {
                Some(Self::pending(format!("draft-{link}"), doing, palette).into_any_element())
            }
            _ => can_draft.then(|| {
                ui::button(
                    SharedString::from(format!("draft-{link}")),
                    format!("Draft a note for {}", row.company),
                    ui::quiet_face("draft note", palette),
                )
                .on_click(cx.listener(move |this, _, window, cx| this.draft_for(link.clone(), window, cx)))
                .into_any_element()
            }),
        };
        let said = self.row_said(&key, format!("said-{}", row.check.posting_link), palette);
        v_flex()
            .id(SharedString::from(format!("check-{}", row.check.posting_link)))
            .test_support()
            .role(gpui_kit::Role::Group)
            .aria_label(format!("{} · {} · {}", row.company, row.title, row.check.summary()))
            .gap(px(5.))
            .pb(px(10.))
            .border_b_1()
            .border_color(palette.line)
            .child(
                h_flex()
                    .gap(px(10.))
                    .items_start()
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap(px(3.))
                            .child(
                                h_flex()
                                    .gap(px(6.))
                                    .child(ui::chip(signal, label, palette))
                                    .children(row.check.team.as_ref().map(|team| ui::boxed(team, palette.faint))),
                            )
                            .child(ui::serif(format!("{} · {}", row.company, row.title)).text_color(palette.ink)),
                    )
                    .child(ui::score(row.score, 22.).text_color(palette.muted)),
            )
            .children(row.check.people.iter().map(|person| {
                let degree = person
                    .degree
                    .as_deref()
                    .map(|degree| format!(" · {degree}"))
                    .unwrap_or_default();
                h_flex().gap(px(8.)).child(ui::square(palette.signal(signal))).child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_color(palette.muted)
                        .child(SharedString::from(format!("{}, {}{degree}", person.name, person.title))),
                )
            }))
            .children(row.check.note.as_ref().map(|note| {
                div()
                    .text_size(px(11.))
                    .text_color(palette.faint)
                    .child(SharedString::from(note.clone()))
            }))
            .children(draft_button.map(|button| h_flex().child(button)))
            .children(said)
    }

    fn drafts_column(&self, palette: &Palette, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let mut rows: Vec<&DraftRow> = self.drafts.iter().collect();
        // Drafts waiting for a decision come first; decided ones keep their order below.
        rows.sort_by_key(|row| row.draft.status != Outreach::Drafted);
        let cards: Vec<AnyElement> = rows
            .into_iter()
            .map(|row| self.draft_card(row, palette, cx).into_any_element())
            .collect();
        let empty = cards.is_empty().then(|| {
            ui::serif("No drafts yet. The sweep drafts notes for hiring posts, up to 10 a day.")
                .text_color(palette.muted)
        });
        let run_now = ui::button(
            "run-outreach",
            "Check and sweep now",
            ui::quiet_face("check and sweep now", palette),
        )
        .on_click(cx.listener(|this, _, window, cx| this.run_now(true, window, cx)));
        v_flex()
            .id("outreach-drafts")
            .flex_1()
            .min_w_0()
            .h_full()
            .overflow_y_scroll()
            .px(px(20.))
            .py(px(16.))
            .gap(px(12.))
            .child(
                h_flex()
                    .justify_between()
                    .child(ui::caps(format!("drafts · {} waiting", self.waiting())).text_color(palette.muted))
                    .children(self.working.is_none().then_some(run_now)),
            )
            .children(
                self.working
                    .clone()
                    .map(|working| ui::caps(working).text_color(palette.signal(Signal::Model))),
            )
            .children(self.said.clone().map(|(signal, text)| {
                h_flex()
                    .id("outreach-said")
                    .test_support()
                    .role(gpui_kit::Role::Label)
                    .aria_label(text.clone())
                    .items_start()
                    .gap(px(9.))
                    .child(ui::square(palette.signal(signal)).mt(px(6.)))
                    .child(ui::serif(text).flex_1().text_size(px(13.5)).text_color(palette.ink))
            }))
            .children(empty)
            .children(cards)
    }

    fn draft_card(&self, row: &DraftRow, palette: &Palette, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let draft = &row.draft;
        let id = draft.id;
        let (signal, state) = match draft.status {
            Outreach::Drafted => (Signal::Model, "drafted"),
            Outreach::Typed => (Signal::Yours, "typed, click send in chrome"),
            Outreach::Sent => (Signal::Done, "sent"),
            Outreach::Replied => (Signal::Done, "replied"),
            Outreach::Skipped | Outreach::NotStarted => (Signal::Failed, "skipped"),
        };
        let slot = slot_label(draft);
        let editing = self.editing == Some(id);
        let body: AnyElement = if editing {
            v_flex()
                .gap(px(6.))
                .child(Textarea::new(&self.editor))
                .child(
                    h_flex()
                        .gap(px(8.))
                        .child(
                            ui::button("save-draft", "Save the draft", ui::primary_face("save", None, palette))
                                .on_click(cx.listener(|this, _, _, cx| this.save_edit(cx))),
                        )
                        .child(
                            ui::button("cancel-edit", "Cancel the edit", ui::quiet_face("cancel", palette)).on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.editing = None;
                                    cx.notify();
                                }),
                            ),
                        ),
                )
                .into_any_element()
        } else {
            ui::serif(draft.text.clone())
                .text_size(px(13.5))
                .text_color(palette.ink)
                .into_any_element()
        };
        let credit = (draft.slot == Slot::InMail && draft.status == Outreach::Drafted).then(|| {
            div().text_size(px(11.)).text_color(palette.muted).child(
                "Sending an InMail uses one of your InMail credits. Maverick only fills the box; you click Send.",
            )
        });
        let mut actions = h_flex().gap(px(8.)).flex_wrap();
        let quiet = |key: &str, name: String, text: &str| {
            ui::button(
                SharedString::from(format!("{key}-{id}")),
                name,
                ui::quiet_face(text, palette),
            )
        };
        let key = RowKey::Draft(id);
        let running = match self.row_states.get(&key) {
            Some(RowState::Working(doing)) => Some(doing.clone()),
            _ => None,
        };
        let busy = running.is_some();
        let reset = || {
            quiet(
                "reset",
                format!("Put the draft to {} back to waiting for approval", draft.person),
                "reset",
            )
            .on_click(cx.listener(move |this, _, _, cx| this.reset(id, cx)))
        };
        match draft.status {
            _ if busy => {}
            Outreach::Drafted if !editing => {
                actions = actions
                    .children((!busy).then(|| {
                        ui::button(
                            SharedString::from(format!("approve-{id}")),
                            format!("Approve and type the draft to {}", draft.person),
                            ui::primary_face("approve, type it", None, palette),
                        )
                        .on_click(cx.listener(move |this, _, window, cx| this.approve(id, window, cx)))
                    }))
                    .child(
                        quiet("edit", format!("Edit the draft to {}", draft.person), "edit")
                            .on_click(cx.listener(move |this, _, window, cx| this.edit(id, window, cx))),
                    )
                    .child(
                        quiet("skip", format!("Skip the draft to {}", draft.person), "skip")
                            .on_click(cx.listener(move |this, _, _, cx| this.set_status(id, Outreach::Skipped, cx))),
                    )
                    .children(draft.slot.switched().filter(|_| !busy).map(|other| {
                        let (name, text) = match other {
                            Slot::InMail => (
                                format!("Rewrite the draft to {} as an InMail", draft.person),
                                "use InMail",
                            ),
                            _ => (
                                format!("Rewrite the draft to {} as a connection note", draft.person),
                                "use note",
                            ),
                        };
                        quiet("switch", name, text)
                            .on_click(cx.listener(move |this, _, window, cx| this.switch_form(id, window, cx)))
                    }));
            }
            Outreach::Typed => {
                actions = actions
                    .child(
                        quiet("sent", format!("Mark the note to {} sent", draft.person), "mark sent")
                            .on_click(cx.listener(move |this, _, _, cx| this.set_status(id, Outreach::Sent, cx))),
                    )
                    .children((!busy).then(|| {
                        quiet(
                            "retype",
                            format!("Type the draft to {} again", draft.person),
                            "type again",
                        )
                        .on_click(cx.listener(move |this, _, window, cx| this.approve(id, window, cx)))
                    }))
                    .child(reset());
            }
            Outreach::Skipped => actions = actions.child(reset()),
            Outreach::Sent => {
                actions = actions.child(
                    quiet("replied", format!("Mark that {} replied", draft.person), "mark replied")
                        .on_click(cx.listener(move |this, _, _, cx| this.set_status(id, Outreach::Replied, cx))),
                );
            }
            _ => {}
        }
        if let Some(doing) = &running {
            actions = actions.child(Self::pending(format!("pending-{id}"), doing, palette));
        }
        let said = self.row_said(&key, format!("said-draft-{id}"), palette);
        let outlined = self.outlined == Some(id);
        if let About::Post(link) = &draft.about {
            let link = link.clone();
            actions = actions.child(
                quiet("open-post", format!("Open the post by {}", draft.person), "open post")
                    .on_click(move |_, _, cx| cx.open_url(&link)),
            );
        }
        v_flex()
            .id(SharedString::from(format!("draft-{id}")))
            .test_support()
            .role(gpui_kit::Role::Group)
            .aria_label(format!("{} · {} · {}", draft.person, row.about, state))
            .relative()
            .gap(px(7.))
            .p(px(12.))
            .border_1()
            .border_color(if outlined {
                palette.signal(Signal::Model)
            } else {
                palette.line
            })
            .bg(palette.background)
            .children(
                (draft.status == Outreach::Drafted)
                    .then(|| ui::brackets(palette.signal(Signal::Model)))
                    .into_iter()
                    .flatten(),
            )
            .child(
                h_flex()
                    .gap(px(10.))
                    .items_start()
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap(px(3.))
                            .child(
                                h_flex()
                                    .gap(px(6.))
                                    .child(ui::chip(signal, state, palette))
                                    .child(ui::boxed(slot, palette.faint)),
                            )
                            .child(ui::serif(draft.person.clone()).text_color(palette.ink))
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(palette.muted)
                                    .truncate()
                                    .child(SharedString::from(row.about.clone())),
                            ),
                    )
                    .child(ui::score(row.score, 22.).text_color(palette.muted)),
            )
            .child(body)
            .children(credit)
            .child(actions)
            .children(said)
    }
}

impl Render for OutreachView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::of(cx);
        h_flex()
            .size_full()
            .items_start()
            .child(self.checks_column(&palette, cx))
            .child(self.drafts_column(&palette, cx))
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{TestAppContext, WindowOptions};
    use maverick_core::jobs::{Posting, Stage};
    use maverick_core::linkedin::Person;

    use super::*;

    fn seeded(database: &std::path::Path) -> i64 {
        let mut store = Store::open(database).unwrap();
        let at: Timestamp = "2026-11-23T17:00:00Z".parse().unwrap();
        let posting = |link: &str, company: &str| Posting {
            link: link.into(),
            found_at: at,
            source: "Simplify".into(),
            company: company.into(),
            title: "Backend Engineer".into(),
            location: None,
            email_subject: None,
            stage: Stage::Review,
            score: Some(81),
            variant: Some("Backend/Infra".into()),
            flags: None,
            notes: None,
            address: None,
            sponsorship: None,
        };
        store
            .add_found(&[
                posting("https://simplify.jobs/p/1", "Example Co"),
                posting("https://simplify.jobs/p/2", "Sample Inc"),
            ])
            .unwrap();
        store
            .save_check(&Check {
                posting_link: "https://simplify.jobs/p/1".into(),
                verdict: Verdict::TeamMember,
                team: Some("Storage".into()),
                people: vec![Person {
                    name: "Bo Sample".into(),
                    title: "Storage Engineer".into(),
                    profile_url: Some("https://www.linkedin.com/in/bo-sample/".into()),
                    degree: Some("2nd".into()),
                }],
                note: None,
                checked_at: at,
            })
            .unwrap();
        store
            .save_check(&Check {
                posting_link: "https://simplify.jobs/p/2".into(),
                verdict: Verdict::Cold,
                team: None,
                people: Vec::new(),
                note: None,
                checked_at: at,
            })
            .unwrap();
        store
            .add_draft(&Draft {
                id: 0,
                about: About::Posting("https://simplify.jobs/p/1".into()),
                person: "Bo Sample".into(),
                headline: "Storage Engineer".into(),
                profile_url: "https://www.linkedin.com/in/bo-sample/".into(),
                slot: Slot::Note,
                subject: None,
                text: "Hi Bo, I applied to the backend role on Storage.".into(),
                status: Outreach::Drafted,
                created_at: at,
                updated_at: at,
            })
            .unwrap()
            .unwrap()
    }

    #[test]
    fn each_draft_card_names_its_slot_and_an_inmail_its_subject() {
        let at: Timestamp = "2026-11-23T17:00:00Z".parse().unwrap();
        let mut draft = Draft {
            id: 1,
            about: About::Posting("https://simplify.jobs/p/1".into()),
            person: "Bo Sample".into(),
            headline: "Storage Engineer".into(),
            profile_url: "https://www.linkedin.com/in/bo-sample/".into(),
            slot: Slot::Note,
            subject: None,
            text: "Hi Bo.".into(),
            status: Outreach::Drafted,
            created_at: at,
            updated_at: at,
        };
        assert_eq!(slot_label(&draft), "connection note · 6 chars");
        draft.slot = Slot::InMail;
        draft.subject = Some("The backend role on Storage".into());
        assert_eq!(slot_label(&draft), "inmail · subject: The backend role on Storage");
    }

    /// A view over the seeded database, with work running on a real runtime so rows can finish.
    fn opened(
        cx: &mut TestAppContext,
        database: &std::path::Path,
        runtime: &tokio::runtime::Runtime,
    ) -> (gpui_kit::AnyWindowHandle, Entity<OutreachView>) {
        cx.executor().allow_parking();
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::ui::install(cx);
        });
        let background = runtime.handle().clone();
        let database = database.to_owned();
        let browser = cx.update(|cx| {
            cx.new(|cx| Browser::new(Bridge::new(), background, Ok("/x".into()), None, Some(database), cx))
        });
        cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| OutreachView::new(browser, None, window, cx))
            })
            .unwrap()
        })
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap()
    }

    /// Waits until `key`'s row has stopped working.
    fn settled(cx: &mut TestAppContext, view: &Entity<OutreachView>, key: &RowKey) -> RowState {
        for _ in 0..1000 {
            cx.run_until_parked();
            let state = view.read_with(cx, |view, _| view.row_state(key).cloned());
            match state {
                Some(RowState::Working(_)) | None => std::thread::sleep(Duration::from_millis(2)),
                Some(state) => return state,
            }
        }
        panic!("the row's work did not finish");
    }

    #[gpui_kit::test]
    fn a_row_click_is_never_dropped_and_each_row_shows_its_own_work(cx: &mut TestAppContext) {
        let scratch = tempfile::tempdir().unwrap();
        let database = scratch.path().join("maverick.db");
        let id = seeded(&database);
        // Sample Inc now has an engineer with a profile and no draft yet, so its row offers one.
        Store::open(&database)
            .unwrap()
            .save_check(&Check {
                posting_link: "https://simplify.jobs/p/2".into(),
                verdict: Verdict::TeamMember,
                team: None,
                people: vec![Person {
                    name: "Cy Person".into(),
                    title: "Software Engineer".into(),
                    profile_url: Some("https://www.linkedin.com/in/cy-person/".into()),
                    degree: Some("2nd".into()),
                }],
                note: None,
                checked_at: "2026-11-23T17:00:00Z".parse().unwrap(),
            })
            .unwrap();
        let runtime = runtime();
        let (window, view) = opened(cx, &database, &runtime);
        let posting = RowKey::Posting("https://simplify.jobs/p/2".into());
        let draft = RowKey::Draft(id);

        // A scheduled check is running. A click on a row still does something, and says so on the row:
        // here the answers file is missing, which used to fail at the top of the other column.
        cx.update_window(window, |_, window, cx| {
            view.update(cx, |view, _| view.working = Some("checking postings".into()));
            window.render_frame(cx);
            window.click("draft-https://simplify.jobs/p/2", cx);
            window.render_frame(cx);
            assert!(
                window
                    .find("said-https://simplify.jobs/p/2")
                    .label()
                    .unwrap()
                    .starts_with("Could not read your answers")
            );
        })
        .unwrap();

        // Two rows work at once. Each shows its own disabled "…ing" button right away.
        let (finish_posting, posting_done) = tokio::sync::oneshot::channel::<Result<Finished, String>>();
        let (finish_draft, draft_done) = tokio::sync::oneshot::channel::<Result<Finished, String>>();
        cx.update_window(window, |_, window, cx| {
            view.update(cx, |view, cx| {
                view.run_row(
                    posting.clone(),
                    "drafting…",
                    async move { posting_done.await.unwrap() },
                    window,
                    cx,
                );
                view.run_row(
                    draft.clone(),
                    "typing in chrome…",
                    async move { draft_done.await.unwrap() },
                    window,
                    cx,
                );
            });
            window.render_frame(cx);
            assert_eq!(
                window.find("draft-https://simplify.jobs/p/2").label(),
                Some("drafting…")
            );
            assert_eq!(
                window.find(SharedString::from(format!("pending-{id}"))).label(),
                Some("typing in chrome…")
            );
            assert!(window.try_find(SharedString::from(format!("approve-{id}"))).is_none());
        })
        .unwrap();
        finish_draft
            .send(Err("the profile has no Message button".into()))
            .unwrap();
        assert_eq!(
            settled(cx, &view, &draft),
            RowState::Failed("the profile has no Message button".into())
        );
        // The other row is still drafting; the failure did not touch it.
        assert!(matches!(
            view.read_with(cx, |view, _| view.row_state(&posting).cloned()),
            Some(RowState::Working(_))
        ));
        finish_posting
            .send(Ok(Finished {
                said: "Drafted a connection note to Cy Person. It is first under Drafts, outlined.".into(),
                draft: Some(id),
            }))
            .unwrap();
        settled(cx, &view, &posting);
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(
                window.find("said-https://simplify.jobs/p/2").label(),
                Some("Drafted a connection note to Cy Person. It is first under Drafts, outlined.")
            );
            assert_eq!(
                window.find(SharedString::from(format!("said-draft-{id}"))).label(),
                Some("the profile has no Message button")
            );
            assert!(window.try_find(SharedString::from(format!("approve-{id}"))).is_some());
            assert_eq!(view.read(cx).outlined, Some(id));
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn a_skipped_or_typed_draft_resets_to_waiting_and_nothing_else_moves(cx: &mut TestAppContext) {
        let scratch = tempfile::tempdir().unwrap();
        let database = scratch.path().join("maverick.db");
        let id = seeded(&database);
        let runtime = runtime();
        let (window, view) = opened(cx, &database, &runtime);
        let checks = || Store::open(&database).unwrap().checks().unwrap();
        let before = checks();
        let at: Timestamp = "2026-11-23T18:00:00Z".parse().unwrap();
        for status in [Outreach::Skipped, Outreach::Typed] {
            Store::open(&database)
                .unwrap()
                .set_draft_status(id, status, at)
                .unwrap();
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, _| view.reload());
                window.render_frame(cx);
                window.click(SharedString::from(format!("reset-{id}")), cx);
                window.render_frame(cx);
                assert_eq!(
                    window.find(SharedString::from(format!("draft-{id}"))).label(),
                    Some("Bo Sample · Backend Engineer at Example Co · drafted")
                );
                assert_eq!(
                    window.find(SharedString::from(format!("said-draft-{id}"))).label(),
                    Some("Back to waiting for your approval.")
                );
                assert!(window.try_find(SharedString::from(format!("approve-{id}"))).is_some());
                assert_eq!(view.read(cx).waiting(), 1);
            })
            .unwrap();
            let draft = Store::open(&database).unwrap().draft(id).unwrap().unwrap();
            assert_eq!((draft.status, draft.slot), (Outreach::Drafted, Slot::Note));
            assert_eq!(draft.text, "Hi Bo, I applied to the backend role on Storage.");
        }
        assert_eq!(checks(), before);
        // A sent draft offers no reset.
        Store::open(&database)
            .unwrap()
            .set_draft_status(id, Outreach::Sent, at)
            .unwrap();
        cx.update_window(window, |_, window, cx| {
            view.update(cx, |view, _| view.reload());
            window.render_frame(cx);
            assert!(window.try_find(SharedString::from(format!("reset-{id}"))).is_none());
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn the_view_lists_verdicts_by_filter_and_a_skip_decides_a_draft(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::ui::install(cx);
        });
        let scratch = tempfile::tempdir().unwrap();
        let database = scratch.path().join("maverick.db");
        let id = seeded(&database);
        let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let bridge = Bridge::new();
        let background = runtime.handle().clone();
        let browser = cx.update(|cx| {
            cx.new(|cx| Browser::new(bridge, background, Ok("/x".into()), None, Some(database.clone()), cx))
        });
        let (window, view) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| OutreachView::new(browser, None, window, cx))
            })
            .unwrap()
        });
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(
                window.find("check-https://simplify.jobs/p/1").label(),
                Some("Example Co · Backend Engineer · Team member found: Bo Sample, Storage Engineer (2nd)")
            );
            assert!(window.try_find("check-https://simplify.jobs/p/2").is_none());
            assert_eq!(
                window.find(SharedString::from(format!("draft-{id}"))).label(),
                Some("Bo Sample · Backend Engineer at Example Co · drafted")
            );
            assert_eq!(view.read(cx).waiting(), 1);
            // The draft exists, so the check offers no second one.
            assert!(window.try_find("draft-https://simplify.jobs/p/1").is_none());

            view.update(cx, |view, cx| {
                view.filter = Filter::ApplyCold;
                view.set_status(id, Outreach::Skipped, cx);
            });
            window.render_frame(cx);
            assert!(window.try_find("check-https://simplify.jobs/p/1").is_none());
            assert!(window.try_find("check-https://simplify.jobs/p/2").is_some());
            assert_eq!(
                window.find(SharedString::from(format!("draft-{id}"))).label(),
                Some("Bo Sample · Backend Engineer at Example Co · skipped")
            );
            assert!(window.try_find(SharedString::from(format!("approve-{id}"))).is_none());
            assert_eq!(view.read(cx).waiting(), 0);
        })
        .unwrap();
    }
}
