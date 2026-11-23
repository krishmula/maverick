//! The Outreach view: who to reach at each company, and the LinkedIn drafts waiting for the user.
//!
//! Approving a draft types it into LinkedIn in the user's Chrome and stops. The user clicks Send.
//! While the app is open, checks run every hour and the hiring-post sweep every two hours.

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
    /// What runs now, such as "typing the draft in Chrome".
    working: Option<String>,
    /// What the last action or run left to say, and whether it worked.
    said: Option<(Signal, String)>,
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

    /// Runs `work` on the background runtime, then reloads and shows what it said.
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

    fn database_or_fail(&mut self, cx: &mut Context<Self>) -> Option<PathBuf> {
        if self.database.is_none() {
            self.said = Some((Signal::Failed, "This system has no application data directory.".into()));
            cx.notify();
        }
        self.database.clone()
    }

    /// Types an approved draft in the user's Chrome. It stops before Send.
    fn approve(&mut self, id: i64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(database) = self.database_or_fail(cx) else {
            return;
        };
        let Some(bridge) = self.bridge(cx) else {
            self.said = Some((Signal::Failed, "Chrome is not connected, so nothing was typed.".into()));
            cx.notify();
            return;
        };
        self.run(
            "typing the draft in chrome",
            async move {
                outreach::type_approved(&database, bridge, id).await?;
                Ok("Typed in Chrome. Read it there and click Send yourself, then mark it sent.".to_owned())
            },
            window,
            cx,
        );
    }

    fn set_status(&mut self, id: i64, status: Outreach, cx: &mut Context<Self>) {
        let Some(database) = self.database_or_fail(cx) else {
            return;
        };
        let saved = Store::open(&database).and_then(|mut store| store.set_draft_status(id, status, Timestamp::now()));
        if let Err(error) = saved {
            self.said = Some((Signal::Failed, error.to_string()));
        }
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
        let (Some(id), Some(database)) = (self.editing, self.database.clone()) else {
            return;
        };
        let Some(row) = self.drafts.iter().find(|row| row.draft.id == id) else {
            return;
        };
        let text = self.editor.read(cx).value().to_string();
        match outreach::check_draft(&text, row.draft.slot) {
            Ok(text) => {
                let saved = Store::open(&database).and_then(|store| store.set_draft_text(id, &text, Timestamp::now()));
                self.said = Some(match saved {
                    Ok(()) => (Signal::Done, "Draft saved.".into()),
                    Err(error) => (Signal::Failed, error.to_string()),
                });
                self.editing = None;
            }
            Err(why) => self.said = Some((Signal::Failed, format!("Not saved: {why}."))),
        }
        self.reload();
        cx.notify();
    }

    fn draft_for(&mut self, posting_link: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(database) = self.database_or_fail(cx) else {
            return;
        };
        let profile = match self.profile() {
            Ok(profile) => profile,
            Err(error) => {
                self.said = Some((Signal::Failed, error));
                cx.notify();
                return;
            }
        };
        self.run(
            "drafting a note",
            async move {
                let draft = outreach::draft_for_check(&database, &profile, &posting_link).await?;
                Ok(format!(
                    "Drafted a {} to {}. Read it before you approve.",
                    draft.slot.as_str(),
                    draft.person
                ))
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
                let mut lines = vec![outreach::check_postings(&database, bridge.clone()).await?.summary()];
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
        let draft_button = can_draft.then(|| {
            ui::button(
                SharedString::from(format!("draft-{link}")),
                format!("Draft a note for {}", row.company),
                ui::quiet_face("draft note", palette),
            )
            .on_click(cx.listener(move |this, _, window, cx| this.draft_for(link.clone(), window, cx)))
        });
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
        let slot = match draft.slot {
            Slot::Note => format!("connection note · {} chars", draft.text.chars().count()),
            Slot::Message => "message".to_owned(),
        };
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
        let mut actions = h_flex().gap(px(8.)).flex_wrap();
        let quiet = |key: &str, name: String, text: &str| {
            ui::button(
                SharedString::from(format!("{key}-{id}")),
                name,
                ui::quiet_face(text, palette),
            )
        };
        let busy = self.working.is_some();
        match draft.status {
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
                    );
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
                    }));
            }
            Outreach::Sent => {
                actions = actions.child(
                    quiet("replied", format!("Mark that {} replied", draft.person), "mark replied")
                        .on_click(cx.listener(move |this, _, _, cx| this.set_status(id, Outreach::Replied, cx))),
                );
            }
            _ => {}
        }
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
            .border_color(palette.line)
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
            .child(actions)
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
                text: "Hi Bo, I applied to the backend role on Storage.".into(),
                status: Outreach::Drafted,
                created_at: at,
                updated_at: at,
            })
            .unwrap()
            .unwrap()
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
