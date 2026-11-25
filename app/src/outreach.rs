//! LinkedIn outreach: who to reach at each posting's company, the hiring posts worth a note, and the
//! drafts for both. The Jobs tab shows a posting's people and drafts on its card; the Posts tab
//! shows the hiring posts in four sections: found, drafted, sent and rejected.
//!
//! Approving a draft types it into LinkedIn in the user's Chrome and stops. The user clicks Send.
//! While the app is open, checks run every hour and the hiring-post sweep every two hours.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::{h_flex, v_flex};
use gpui_kit::{
    AnyElement, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement, ParentElement as _, Render,
    SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, Task, TestSupportExt as _, Window, div,
    px,
};
use jiff::Timestamp;
use maverick_core::answers::Answers;
use maverick_core::bridge::{Bridge, BrowserState};
use maverick_core::jobs::{HiringPost, Outreach, Posting};
use maverick_core::linkedin::{Person, Slot};
use maverick_core::outreach::{self, About, Added, Check, Draft, ScorerPeople, Verdict};
use maverick_core::score::Profile;
use maverick_core::store::Store;
use tokio::runtime::Handle;

use crate::board::Search;
use crate::browser::Browser;
use crate::ui::{self, Palette, Signal};

/// The wait after launch before the first outreach run, after the funnel's first pass.
const FIRST_RUN: Duration = Duration::from_secs(5 * 60);
const BETWEEN_RUNS: Duration = Duration::from_secs(60 * 60);
/// The sweep runs on every second run, so every two hours.
const SWEEP_EVERY: u32 = 2;

/// A draft with what it is about, as one line.
#[derive(Clone, Debug)]
pub struct DraftRow {
    pub draft: Draft,
    pub about: String,
    pub score: Option<u8>,
}

/// A slot as a sentence names it.
fn slot_name(slot: Slot) -> &'static str {
    match slot {
        Slot::Note => "connection note",
        Slot::Message => "message",
        Slot::InMail => "InMail",
    }
}

/// The slot a draft is in, as its card labels it.
pub fn slot_label(draft: &Draft) -> String {
    match draft.slot {
        Slot::Note => format!("connection note · {} chars", draft.text.chars().count()),
        Slot::Message => "message".to_owned(),
        Slot::InMail => format!("inmail · subject: {}", draft.subject.as_deref().unwrap_or("none")),
    }
}

/// The color a check's verdict shows in: people found, a lookup still to do, or no one.
pub fn verdict_signal(verdict: Verdict) -> Signal {
    match verdict {
        Verdict::Connection | Verdict::TeamMember | Verdict::HiringManager => Signal::Done,
        Verdict::Deferred => Signal::Yours,
        Verdict::Cold => Signal::Failed,
    }
}

/// One person as a line: name, title and connection degree.
pub fn person_line(person: &Person) -> String {
    let mut line = person.name.clone();
    if !person.title.is_empty() {
        line.push_str(", ");
        line.push_str(&person.title);
    }
    if let Some(degree) = &person.degree {
        line.push_str(" · ");
        line.push_str(degree);
    }
    line
}

/// Where a draft stands, as a card or a job's row says it.
pub fn draft_state(status: Outreach) -> (Signal, &'static str) {
    match status {
        Outreach::Drafted => (Signal::Model, "drafted"),
        Outreach::Typed => (Signal::Yours, "typed, click send in chrome"),
        Outreach::Sent => (Signal::Done, "sent"),
        Outreach::Replied => (Signal::Done, "replied"),
        Outreach::Skipped | Outreach::NotStarted => (Signal::Failed, "skipped"),
    }
}

/// Where a hiring post without a draft in Maverick stands.
pub fn post_state(status: Outreach) -> (Signal, &'static str) {
    match status {
        Outreach::NotStarted => (Signal::Yours, "no draft"),
        Outreach::Drafted | Outreach::Typed => (Signal::Model, "drafted elsewhere"),
        Outreach::Sent => (Signal::Done, "sent"),
        Outreach::Replied => (Signal::Done, "replied"),
        Outreach::Skipped => (Signal::Failed, "rejected"),
    }
}

/// Which hiring posts the Posts tab lists. A post with a draft in Maverick is where its draft is.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PostsSection {
    /// Posts with no draft yet: the queue the user drafts from or rejects.
    Found,
    /// Posts with a note drafted or typed, waiting on the user's approval or Send.
    Drafted,
    /// Posts whose note went out, and those that got a reply.
    Sent,
    /// Posts the user or the sweep rejected. They can be restored.
    Rejected,
}

impl PostsSection {
    const ALL: [Self; 4] = [Self::Found, Self::Drafted, Self::Sent, Self::Rejected];

    pub fn of(status: Outreach) -> Self {
        match status {
            Outreach::NotStarted => Self::Found,
            Outreach::Drafted | Outreach::Typed => Self::Drafted,
            Outreach::Sent | Outreach::Replied => Self::Sent,
            Outreach::Skipped => Self::Rejected,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Found => "found",
            Self::Drafted => "drafted",
            Self::Sent => "sent",
            Self::Rejected => "rejected",
        }
    }

    /// The heading over the posts in this section that have no draft in Maverick, when drafts come first.
    fn without_a_draft(self) -> &'static str {
        match self {
            Self::Found => "found, no draft",
            Self::Drafted => "drafted outside maverick",
            Self::Sent => "sent outside maverick",
            Self::Rejected => "rejected before a draft",
        }
    }

    fn empty(self) -> &'static str {
        match self {
            Self::Found => {
                "Nothing new. Every two hours the sweep reads LinkedIn for hiring posts and drafts a note for \
                 the ones that fit. Paste a post's link above to add one yourself."
            }
            Self::Drafted => "No drafts waiting. Draft a note from a found post.",
            Self::Sent => "No notes sent yet. A note lands here once you mark it sent.",
            Self::Rejected => "Nothing rejected. A post you reject lands here, out of the queue.",
        }
    }
}

/// The most hiring posts without a draft that one section lists.
const FOUND_SHOWN: usize = 100;

/// Reads the checks, the drafts with the postings and posts they are about, and the hiring posts
/// with no draft, newest first.
pub fn rows(store: &Store) -> (Vec<Check>, Vec<DraftRow>, Vec<HiringPost>) {
    let postings: Vec<Posting> = store.postings().unwrap_or_default();
    let posts: Vec<HiringPost> = store.hiring_posts().unwrap_or_default();
    let posting_of = |link: &str| postings.iter().find(|posting| posting.link == link);
    let checks = store.checks().unwrap_or_default();
    let drafts: Vec<DraftRow> = store
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
    let drafted: HashSet<&str> = drafts
        .iter()
        .filter_map(|row| match &row.draft.about {
            About::Post(link) => Some(link.as_str()),
            About::Posting(_) => None,
        })
        .collect();
    let found = posts
        .iter()
        .filter(|post| !drafted.contains(post.post_link.as_str()))
        .cloned()
        .collect();
    (checks, drafts, found)
}

/// A row that can have work of its own: a posting's check on the left, or a draft on the right.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum RowKey {
    /// One person a posting's check found: the posting's link and the person's profile address.
    Person(String, String),
    /// One person a posting's scorer note names: the posting's link and the person's name.
    Named(String, String),
    Draft(i64),
    /// A hiring post with no draft, by its link.
    Post(String),
    /// The box that adds a post by its link.
    Add,
}

/// Where a person at a posting came from, which says how a draft to them is made.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Source {
    /// The posting's outreach check, which always found a profile to draft to.
    Check,
    /// The scorer's outreach note. A person it gives no profile link for is looked up when drafted to.
    Scorer,
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
    checks: Vec<Check>,
    /// The scorer notes read so far, by posting link.
    scorer: HashMap<String, ScorerPeople>,
    /// Scorer notes being read now, and why a read failed, by posting link.
    scorer_reads: HashMap<String, Option<String>>,
    drafts: Vec<DraftRow>,
    /// The hiring posts that have no draft, newest first.
    found: Vec<HiringPost>,
    section: PostsSection,
    /// The search box over every section, and what it asks for.
    search: Entity<InputState>,
    query: Search,
    /// The box a post's link is pasted into to add it.
    adder: Entity<InputState>,
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
    _subscriptions: Vec<Subscription>,
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
        let search = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Search: stripe, openai")
                .clean_on_escape()
        });
        let adder = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Paste a LinkedIn post link to add it")
                .clean_on_escape()
        });
        let _subscriptions = vec![
            cx.subscribe_in(&search, window, |this, input, event: &InputEvent, _, cx| {
                if let InputEvent::Change = event {
                    this.query = Search::new(&input.read(cx).value());
                    cx.notify();
                }
            }),
            cx.subscribe_in(&adder, window, |this, _, event: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    this.add_post(window, cx);
                }
            }),
        ];
        let mut view = Self {
            browser,
            background,
            database,
            answers,
            checks: Vec::new(),
            scorer: HashMap::new(),
            scorer_reads: HashMap::new(),
            drafts: Vec::new(),
            found: Vec::new(),
            section: PostsSection::Found,
            search,
            query: Search::default(),
            adder,
            editing: None,
            editor,
            working: None,
            said: None,
            row_states: HashMap::new(),
            outlined: None,
            typing: Arc::new(tokio::sync::Mutex::new(())),
            _schedule: None,
            _subscriptions,
        };
        view.reload();
        // Drafts waiting on the user's approval or Send come first; with none, the queue of found posts.
        let drafted = view.drafts.iter().any(|row| {
            matches!(row.draft.about, About::Post(_)) && PostsSection::of(row.draft.status) == PostsSection::Drafted
        });
        if drafted {
            view.section = PostsSection::Drafted;
        }
        view
    }

    /// Hiring-post drafts waiting for the user's decision, for the bar.
    pub fn waiting(&self) -> usize {
        self.drafts
            .iter()
            .filter(|row| matches!(row.draft.about, About::Post(_)) && row.draft.status == Outreach::Drafted)
            .count()
    }

    /// The check for the posting at `link`, if one ran.
    pub fn check(&self, link: &str) -> Option<&Check> {
        self.checks.iter().find(|check| check.posting_link == link)
    }

    /// The links of the postings with someone to reach, found by a check or named in a scorer note.
    pub fn reachable(&self) -> HashSet<String> {
        let checked = self
            .checks
            .iter()
            .filter(|check| check.verdict.found_people())
            .map(|check| &check.posting_link);
        let named = self
            .scorer
            .values()
            .filter(|read| !read.people.is_empty())
            .map(|read| &read.posting_link);
        checked.chain(named).cloned().collect()
    }

    /// Where the newest live draft for the posting at `link` stands. A skipped one counts only when it is the only one.
    pub fn draft_status(&self, link: &str) -> Option<Outreach> {
        let about = About::Posting(link.to_owned());
        let mut drafts: Vec<&Draft> = self
            .drafts
            .iter()
            .map(|row| &row.draft)
            .filter(|draft| draft.about == about)
            .collect();
        drafts.sort_by_key(|draft| (draft.status != Outreach::Skipped, draft.id));
        drafts.last().map(|draft| draft.status)
    }

    /// True while a draft is open in the editor, so the keys typed there are not shortcuts.
    pub fn is_editing(&self) -> bool {
        self.editing.is_some()
    }

    pub fn reload(&mut self) {
        let store = self
            .database
            .as_deref()
            .filter(|path| path.is_file())
            .and_then(|path| Store::open(path).ok());
        if let Some(store) = store {
            (self.checks, self.drafts, self.found) = rows(&store);
            self.scorer = store
                .scorer_people()
                .unwrap_or_default()
                .into_iter()
                .map(|read| (read.posting_link.clone(), read))
                .collect();
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
        Profile::load(file)
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
                let (key, state) = match outcome {
                    Ok(finished) => {
                        if finished.draft.is_some() {
                            this.outlined = finished.draft;
                        }
                        // A post with a draft leaves the found list, so its draft's card says what happened.
                        let key = match (key, finished.draft) {
                            (RowKey::Post(post), Some(draft)) => {
                                this.row_states.remove(&RowKey::Post(post));
                                RowKey::Draft(draft)
                            }
                            (key, _) => key,
                        };
                        (key, RowState::Done(finished.said))
                    }
                    Err(why) => (key, RowState::Failed(why)),
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
        // The resume goes into a message or an InMail. Without it the draft is still typed.
        let resume = self
            .answers
            .as_deref()
            .and_then(|file| Answers::load(file).ok())
            .map(|answers| answers.resume);
        self.run_row(
            key,
            "typing in chrome…",
            async move {
                // Another draft may be typing; this one waits for the tab to be free.
                let _turn = typing.lock().await;
                let typed = outreach::type_approved(&database, bridge, id, resume.as_deref()).await?;
                Ok(Finished::said(typed.said()))
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

    /// Drafts a note to one person a posting's check found. Their row shows "drafting…", then where
    /// the draft went.
    fn draft_for(&mut self, posting_link: String, person: String, window: &mut Window, cx: &mut Context<Self>) {
        let key = RowKey::Person(posting_link.clone(), person.clone());
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
                let draft = outreach::draft_for_check(&database, &profile, &posting_link, &person).await?;
                Ok(Finished {
                    said: format!(
                        "Drafted a {} to {}. It is below, outlined.",
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

    /// Drafts a note to one person a posting's scorer note names. Their profile is looked up first
    /// when the note gives no link for them.
    fn draft_for_named(&mut self, posting_link: String, person: Person, window: &mut Window, cx: &mut Context<Self>) {
        let key = RowKey::Named(posting_link.clone(), person.name.clone());
        let Some(database) = self.database_for(&key, cx) else {
            return;
        };
        let Some(profile) = self.profile_for(&key, cx) else {
            return;
        };
        let bridge = self.bridge(cx);
        let doing = if person.profile_url.is_some() {
            "drafting…"
        } else {
            "finding profile…"
        };
        self.run_row(
            key,
            doing,
            async move {
                let draft =
                    outreach::draft_for_scorer(&database, &profile, bridge, &posting_link, &person.name).await?;
                Ok(Finished {
                    said: format!(
                        "Drafted a {} to {}. It is below, outlined.",
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

    /// Reads the people in a posting's scorer note with the model, once per note, in the background.
    fn read_scorer_note(&mut self, link: &str, note: String, cx: &mut Context<Self>) {
        let Some(database) = self.database.clone() else {
            return;
        };
        self.scorer_reads.insert(link.to_owned(), None);
        let posting_link = link.to_owned();
        let done = self
            .background
            .spawn(async move { outreach::scorer_people(&database, &posting_link, &note).await });
        let link = link.to_owned();
        cx.spawn(async move |this, cx| {
            let outcome = done.await.unwrap_or_else(|stopped| Err(stopped.to_string()));
            this.update(cx, |this, cx| {
                match outcome {
                    Ok(_) => {
                        this.scorer_reads.remove(&link);
                        this.reload();
                    }
                    Err(why) => {
                        this.scorer_reads.insert(link, Some(why));
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// What the scorer wrote about whom to reach, with a row for each person it names, which can be
    /// reached like the people a check finds. `None` when the scorer wrote nothing about it.
    pub fn scorer_section(
        &mut self,
        link: &str,
        note: &[String],
        palette: &Palette,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if note.is_empty() {
            return None;
        }
        let joined = note.join("; ");
        let read = self.scorer.get(link).filter(|read| read.note == joined).cloned();
        let reading = self.scorer_reads.get(link).cloned();
        if read.is_none() && reading.is_none() {
            self.read_scorer_note(link, joined, cx);
        }
        let drafts: Vec<&DraftRow> = self
            .drafts
            .iter()
            .filter(|row| row.draft.about == About::Posting(link.to_owned()))
            .collect();
        let people: Vec<AnyElement> = read
            .iter()
            .flat_map(|read| &read.people)
            .map(|person| self.person_row(link, person, &drafts, Signal::Done, Source::Scorer, palette, cx))
            .collect();
        let state = match (&read, reading) {
            (Some(_), _) => None,
            (None, Some(Some(why))) => Some(
                div()
                    .text_size(px(11.))
                    .text_color(palette.signal(Signal::Failed))
                    .child(SharedString::from(format!(
                        "Could not read the people in this note: {why}"
                    ))),
            ),
            (None, _) => Some(
                div()
                    .text_size(px(11.))
                    .text_color(palette.signal(Signal::Model))
                    .child("reading the people in this note…"),
            ),
        };
        Some(
            v_flex()
                .id(SharedString::from(format!("scorer-{link}")))
                .gap(px(6.))
                .pb(px(12.))
                .mb(px(12.))
                .border_b_1()
                .border_color(palette.line)
                .child(ui::caps("scorer's outreach note").text_color(palette.muted))
                .children(people)
                .children(note.iter().enumerate().map(|(ix, item)| {
                    div()
                        .text_size(px(12.))
                        .line_height(px(17.))
                        .text_color(palette.faint)
                        .child(ui::selectable(format!("scorer-outreach-{ix}"), item))
                }))
                .children(state)
                .into_any_element(),
        )
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

    /// Drafts a message to the author of a found hiring post. Maverick reads the post in Chrome
    /// when the store does not hold it.
    fn draft_post(&mut self, post_link: String, window: &mut Window, cx: &mut Context<Self>) {
        let key = RowKey::Post(post_link.clone());
        let Some(database) = self.database_for(&key, cx) else {
            return;
        };
        let Some(profile) = self.profile_for(&key, cx) else {
            return;
        };
        let bridge = self.bridge(cx);
        self.run_row(
            key,
            "reading and drafting…",
            async move {
                let made = outreach::draft_for_post(&database, &profile, bridge, &post_link).await?;
                let form = match made.draft.slot {
                    Slot::InMail => "an InMail",
                    Slot::Note => "a connection note",
                    Slot::Message => "a message",
                };
                let mut said = format!("Drafted {form} to {}.", made.draft.person);
                if let Some(why) = made.credits_unread {
                    said.push_str(&format!(
                        " Your InMail credits could not be read ({why}), so it is a connection note."
                    ));
                }
                Ok(Finished {
                    said,
                    draft: Some(made.draft.id),
                })
            },
            window,
            cx,
        );
    }

    /// Moves a hiring post with no draft to `outreach`: rejected out of the queue, or back into it.
    fn set_post(&mut self, link: String, outreach: Outreach, said: &str, cx: &mut Context<Self>) {
        let key = RowKey::Post(link.clone());
        let Some(database) = self.database_for(&key, cx) else {
            return;
        };
        let state = match Store::open(&database).and_then(|store| store.set_post_outreach(&link, outreach)) {
            Ok(()) => RowState::Done(said.to_owned()),
            Err(error) => RowState::Failed(error.to_string()),
        };
        self.row_states.insert(key, state);
        self.reload();
        cx.notify();
    }

    /// Adds the post whose link is in the add box: Maverick reads it in Chrome and keeps it as found.
    fn add_post(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let key = RowKey::Add;
        if matches!(self.row_states.get(&key), Some(RowState::Working(_))) {
            return;
        }
        let pasted = self.adder.read(cx).value().to_string();
        if let Err(why) = outreach::pasted_post_link(&pasted) {
            self.row_failed(key, why, cx);
            return;
        }
        let Some(database) = self.database_for(&key, cx) else {
            return;
        };
        let Some(profile) = self.profile_for(&key, cx) else {
            return;
        };
        let bridge = self.bridge(cx);
        self.row_states
            .insert(key.clone(), RowState::Working("reading the post…".into()));
        cx.notify();
        let done = self
            .background
            .spawn(async move { outreach::add_post(&database, &profile, bridge, &pasted).await });
        cx.spawn_in(window, async move |this, cx| {
            let outcome = done.await.unwrap_or_else(|stopped| Err(stopped.to_string()));
            this.update_in(cx, |this, window, cx| {
                let state = match outcome {
                    Ok(Added::New(post)) => {
                        this.adder.update(cx, |input, cx| input.set_value("", window, cx));
                        this.section = PostsSection::Found;
                        RowState::Done(format!(
                            "Added {} at {}, posted by {}. It is at the top of found.",
                            post.role, post.company, post.poster
                        ))
                    }
                    Ok(Added::Known(post)) => {
                        this.adder.update(cx, |input, cx| input.set_value("", window, cx));
                        this.reload();
                        let section = this.section_of(&post);
                        this.section = section;
                        RowState::Done(format!(
                            "Maverick already has this post ({} at {}), under {}.",
                            post.role,
                            post.company,
                            section.name()
                        ))
                    }
                    Err(why) => RowState::Failed(why),
                };
                this.row_states.insert(RowKey::Add, state);
                this.reload();
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The section a hiring post is listed in: where its draft is, or where it stands without one.
    fn section_of(&self, post: &HiringPost) -> PostsSection {
        let about = About::Post(post.post_link.clone());
        let draft = self.drafts.iter().find(|row| row.draft.about == about);
        PostsSection::of(draft.map_or(post.outreach, |row| row.draft.status))
    }

    /// One outreach run: checks every posting that needs one, then, when `sweep`, the hiring posts.
    fn run_now(&mut self, sweep: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(database) = self.database_or_fail(cx) else {
            return;
        };
        let bridge = self.bridge(cx);
        let profile = self.profile();
        let auto_apply = maverick_core::auto::is_on(&database);
        let typing = self.typing.clone();
        let resume = self
            .answers
            .as_deref()
            .and_then(|file| Answers::load(file).ok())
            .map(|answers| answers.resume);
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
                if auto_apply && let (Some(bridge), Ok(profile)) = (&bridge, &profile) {
                    // Chrome has one active tab, so sends wait for any draft being typed.
                    let _turn = typing.lock().await;
                    let reached = maverick_core::auto::reach(&database, profile, bridge.clone(), resume.as_deref());
                    lines.push(reached.await?.summary());
                }
                if sweep {
                    let bridge = bridge.ok_or("Chrome is not connected, so the hiring-post sweep did not run.")?;
                    let swept = outreach::sweep(&database, &profile?, bridge).await?;
                    if let Some(notice) = crate::notify::post_drafts(swept.drafted) {
                        crate::notify::post(notice);
                    }
                    lines.push(swept.summary());
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

    /// A posting's outreach for its card on the Jobs tab: the check, the people, and the drafts to them.
    pub fn posting_section(&self, link: &str, palette: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let section = v_flex()
            .id(SharedString::from(format!("outreach-{link}")))
            .gap(px(6.))
            .pb(px(12.))
            .mb(px(12.))
            .border_b_1()
            .border_color(palette.line)
            .child(ui::caps("outreach").text_color(palette.muted));
        let Some(check) = self.check(link) else {
            return section
                .child(
                    ui::serif("Not checked yet. Checks run every hour, apply-ready postings first.")
                        .text_size(px(13.5))
                        .text_color(palette.muted),
                )
                .into_any_element();
        };
        let verdict = check.verdict;
        let signal = verdict_signal(verdict);
        let label = match (verdict, &check.team) {
            (Verdict::Deferred, Some(_)) => "team named, lookup deferred".to_owned(),
            (verdict, _) => verdict.label().to_lowercase(),
        };
        let drafts: Vec<&DraftRow> = self
            .drafts
            .iter()
            .filter(|row| row.draft.about == About::Posting(link.to_owned()))
            .collect();
        let cards: Vec<AnyElement> = drafts
            .iter()
            .map(|row| self.draft_card(row, palette, cx).into_any_element())
            .collect();
        section
            .child(
                h_flex()
                    .id(SharedString::from(format!("check-{link}")))
                    .test_support()
                    .role(gpui_kit::Role::Label)
                    .aria_label(check.summary())
                    .gap(px(6.))
                    .child(ui::chip(signal, label, palette))
                    .children(check.team.as_ref().map(|team| ui::boxed(team, palette.faint))),
            )
            .children(
                check
                    .people
                    .iter()
                    .map(|person| self.person_row(link, person, &drafts, signal, Source::Check, palette, cx)),
            )
            .children(check.note.as_ref().map(|note| {
                div()
                    .text_size(px(11.))
                    .text_color(palette.faint)
                    .child(SharedString::from(note.clone()))
            }))
            .children(cards)
            .into_any_element()
    }

    /// One person at a posting, with their own ways to reach them: a draft to them, or where their
    /// draft stands, and their LinkedIn profile.
    #[expect(clippy::too_many_arguments, reason = "each is what one row shows")]
    fn person_row(
        &self,
        link: &str,
        person: &Person,
        drafts: &[&DraftRow],
        signal: Signal,
        source: Source,
        palette: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let line = h_flex().gap(px(8.)).child(ui::square(palette.signal(signal))).child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_color(palette.muted)
                .child(SharedString::from(person_line(person))),
        );
        let profile = person.profile_url.clone();
        let key = match (source, &profile) {
            (Source::Check, Some(profile)) => RowKey::Person(link.to_owned(), profile.clone()),
            (Source::Scorer, _) => RowKey::Named(link.to_owned(), person.name.clone()),
            (Source::Check, None) => {
                return line
                    .child(ui::caps("no profile").flex_none().text_color(palette.faint))
                    .into_any_element();
            }
        };
        let id = |what: &str| {
            SharedString::from(match &key {
                RowKey::Named(_, name) => format!("{what}-{link}|named|{name}"),
                _ => format!("{what}-{link}|{}", profile.as_deref().unwrap_or_default()),
            })
        };
        let draft = drafts
            .iter()
            .find(|row| Some(&row.draft.profile_url) == profile.as_ref());
        let reach: AnyElement = match (self.row_states.get(&key), draft) {
            (Some(RowState::Working(doing)), _) => {
                Self::pending(id("draft").to_string(), doing, palette).into_any_element()
            }
            (_, Some(row)) => {
                let (signal, word) = draft_state(row.draft.status);
                let word = if row.draft.status == Outreach::Typed {
                    "typed"
                } else {
                    word
                };
                let form = match row.draft.slot {
                    Slot::InMail => "inmail",
                    Slot::Note => "note",
                    Slot::Message => "message",
                };
                ui::chip(signal, format!("{form} {word}"), palette).into_any_element()
            }
            (_, None) => {
                let first = person.degree.as_deref() == Some("1st");
                let (word, tip) = match (&profile, first) {
                    (None, _) => (
                        "find + draft",
                        "Look up their LinkedIn profile by name and company, one page load, then write a draft.",
                    ),
                    (Some(_), true) => ("draft message", "Write a message to this connection."),
                    (Some(_), false) => (
                        "draft",
                        "Write an InMail while a credit is free, otherwise a connection note.",
                    ),
                };
                let name = format!("Draft to {}", person.name);
                let posting = link.to_owned();
                let target = person.clone();
                ui::button(id("draft"), name, ui::quiet_face(word, palette))
                    .tip(tip)
                    .on_click(cx.listener(move |this, _, window, cx| match source {
                        Source::Check => {
                            let profile = target.profile_url.clone().unwrap_or_default();
                            this.draft_for(posting.clone(), profile, window, cx)
                        }
                        Source::Scorer => this.draft_for_named(posting.clone(), target.clone(), window, cx),
                    }))
                    .into_any_element()
            }
        };
        let said = self.row_said(&key, id("said").to_string(), palette);
        let open = profile.clone().map(|open| {
            ui::button(id("profile"), "Open profile", ui::quiet_face("profile ↗", palette))
                .tip("Open their LinkedIn profile in your browser.")
                .on_click(move |_, _, cx| cx.open_url(&open))
        });
        v_flex()
            .gap(px(4.))
            .child(line.child(reach).children(open))
            .children(said)
            .into_any_element()
    }

    /// The Posts tab: the hiring posts in one section at a time, the search over them, the box that
    /// adds one by its link, and the run that finds them.
    fn posts_column(&self, palette: &Palette, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let section = self.section;
        let query = &self.query;
        let drafts: Vec<&DraftRow> = self
            .drafts
            .iter()
            .filter(|row| matches!(row.draft.about, About::Post(_)))
            .filter(|row| query.finds(&format!("{} {}", row.draft.person, row.about)))
            .collect();
        let found: Vec<&HiringPost> = self
            .found
            .iter()
            .filter(|post| query.finds(&format!("{} {} {}", post.company, post.role, post.poster)))
            .collect();
        let count = |shows: PostsSection| {
            drafts
                .iter()
                .filter(|row| PostsSection::of(row.draft.status) == shows)
                .count()
                + found
                    .iter()
                    .filter(|post| PostsSection::of(post.outreach) == shows)
                    .count()
        };
        let mut rows: Vec<&DraftRow> = drafts
            .iter()
            .copied()
            .filter(|row| PostsSection::of(row.draft.status) == section)
            .collect();
        match section {
            // Drafts waiting for approval come before those typed and waiting on Send.
            PostsSection::Drafted => rows.sort_by_key(|row| row.draft.status != Outreach::Drafted),
            _ => rows.sort_by_key(|row| std::cmp::Reverse(row.draft.updated_at)),
        }
        let counts = PostsSection::ALL.map(count);
        let found: Vec<&HiringPost> = found
            .iter()
            .copied()
            .filter(|post| PostsSection::of(post.outreach) == section)
            .collect();
        let cards: Vec<AnyElement> = rows
            .into_iter()
            .map(|row| self.draft_card(row, palette, cx).into_any_element())
            .collect();
        let empty = (cards.is_empty() && found.is_empty()).then(|| {
            ui::serif(if query.is_empty() {
                section.empty()
            } else {
                "No post here matches the search. The counts above say where the matches are."
            })
            .text_color(palette.muted)
        });
        let run_now = ui::button(
            "run-outreach",
            "Check and sweep now",
            ui::quiet_face("check and sweep now", palette),
        )
        .on_click(cx.listener(|this, _, window, cx| this.run_now(true, window, cx)));
        let switches = PostsSection::ALL.into_iter().zip(counts).map(|(shows, n)| {
            let ink = palette.ink;
            let face = ui::caps(format!("{} {n}", shows.name()))
                .text_color(if section == shows { palette.ink } else { palette.faint })
                .hover(move |face| face.text_color(ink));
            let mut name = shows.name().to_owned();
            name[..1].make_ascii_uppercase();
            ui::button(
                SharedString::from(format!("posts-{}", shows.name())),
                format!("{name} · {n}"),
                face,
            )
            .on_click(cx.listener(move |this, _, _, cx| this.show_section(shows, cx)))
        });
        let adding = match self.row_states.get(&RowKey::Add) {
            Some(RowState::Working(doing)) => Self::pending("adding-post".into(), doing, palette).into_any_element(),
            _ => ui::button("add-post", "Add the post", ui::primary_face("add", None, palette))
                .on_click(cx.listener(|this, _, window, cx| this.add_post(window, cx)))
                .into_any_element(),
        };
        let found_heading = (!found.is_empty() && !cards.is_empty()).then(|| {
            let count = match found.len() {
                all if all > FOUND_SHOWN => format!("newest {FOUND_SHOWN} of {all}"),
                all => all.to_string(),
            };
            ui::caps(format!("{} · {count}", section.without_a_draft()))
                .pt(px(8.))
                .text_color(palette.muted)
        });
        let more = (cards.is_empty() && found.len() > FOUND_SHOWN).then(|| {
            ui::caps(format!(
                "newest {FOUND_SHOWN} of {}; search to find the rest",
                found.len()
            ))
            .text_color(palette.muted)
        });
        v_flex()
            .id("outreach-drafts")
            .size_full()
            .max_w(px(760.))
            .overflow_y_scroll()
            .px(px(20.))
            .py(px(16.))
            .gap(px(12.))
            .child(
                h_flex()
                    .gap_4()
                    .children(switches)
                    .child(div().flex_1())
                    .children(self.working.is_none().then_some(run_now)),
            )
            .child(
                h_flex()
                    .gap(px(8.))
                    .child(
                        div().flex_1().min_w_0().child(
                            Input::new(&self.search)
                                .id("post-search")
                                .aria_label("Search hiring posts")
                                .cleanable(true),
                        ),
                    )
                    .child(
                        div().flex_1().min_w_0().child(
                            Input::new(&self.adder)
                                .id("post-link")
                                .aria_label("A LinkedIn post link to add"),
                        ),
                    )
                    .child(adding),
            )
            .children(self.row_said(&RowKey::Add, "said-add".into(), palette))
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
            .children(found_heading)
            .children(more)
            .children(
                found
                    .into_iter()
                    .take(FOUND_SHOWN)
                    .enumerate()
                    .map(|(at, post)| self.post_card(at, post, palette, cx))
                    .collect::<Vec<_>>(),
            )
    }

    fn show_section(&mut self, section: PostsSection, cx: &mut Context<Self>) {
        self.section = section;
        cx.notify();
    }

    /// A hiring post with no draft: who posted it, the role, why there is no draft, and the post.
    fn post_card(
        &self,
        at: usize,
        post: &HiringPost,
        palette: &Palette,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let (signal, state) = post_state(post.outreach);
        let about = format!("{} at {}", post.role, post.company);
        let link = post.post_link.clone();
        let key = RowKey::Post(link.clone());
        let draft = match self.row_states.get(&key) {
            Some(RowState::Working(doing)) => {
                Some(Self::pending(format!("pending-post-{at}"), doing, palette).into_any_element())
            }
            // A post the tracker links to a search page has no author Maverick can find.
            _ if outreach::post_without_a_page(&link).is_some() => None,
            // Its note already went out from somewhere else, or the user rejected it.
            _ if matches!(
                PostsSection::of(post.outreach),
                PostsSection::Sent | PostsSection::Rejected
            ) =>
            {
                None
            }
            _ => {
                let link = link.clone();
                Some(
                    ui::button(
                        SharedString::from(format!("draft-post-{at}")),
                        format!("Draft a message to {}", post.poster),
                        ui::primary_face("draft", None, palette),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| this.draft_post(link.clone(), window, cx)))
                    .into_any_element(),
                )
            }
        };
        let busy = matches!(self.row_states.get(&key), Some(RowState::Working(_)));
        let decide = match PostsSection::of(post.outreach) {
            _ if busy => None,
            PostsSection::Found | PostsSection::Drafted => {
                let link = link.clone();
                Some(
                    ui::button(
                        SharedString::from(format!("reject-post-{at}")),
                        format!("Reject the post by {}", post.poster),
                        ui::quiet_face("reject", palette),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.set_post(link.clone(), Outreach::Skipped, "Rejected. It is under rejected.", cx)
                    }))
                    .into_any_element(),
                )
            }
            PostsSection::Rejected => {
                let link = link.clone();
                Some(
                    ui::button(
                        SharedString::from(format!("restore-post-{at}")),
                        format!("Restore the post by {}", post.poster),
                        ui::quiet_face("restore", palette),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.set_post(
                            link.clone(),
                            Outreach::NotStarted,
                            "Restored. It is back under found.",
                            cx,
                        )
                    }))
                    .into_any_element(),
                )
            }
            PostsSection::Sent => None,
        };
        let said = self.row_said(&key, format!("said-post-{at}"), palette);
        v_flex()
            .id(SharedString::from(format!("post-{at}")))
            .test_support()
            .role(gpui_kit::Role::Group)
            .aria_label(format!("{} · {about} · {state}", post.poster))
            .gap(px(5.))
            .p(px(12.))
            .border_1()
            .border_color(palette.line)
            .bg(palette.background)
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
                                    .child(ui::boxed(post.found_at.strftime("%b %-d").to_string(), palette.faint)),
                            )
                            .child(ui::serif(post.poster.clone()).text_color(palette.ink))
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(palette.muted)
                                    .truncate()
                                    .child(SharedString::from(about)),
                            ),
                    )
                    .child(ui::score(Some(post.score), 22.).text_color(palette.muted)),
            )
            .children(post.notes.clone().map(|notes| {
                div()
                    .text_size(px(11.))
                    .text_color(palette.faint)
                    .child(SharedString::from(notes))
            }))
            .child(
                h_flex().gap(px(8.)).children(draft).children(decide).child(
                    ui::button(
                        SharedString::from(format!("open-found-post-{at}")),
                        format!("Open the post by {}", post.poster),
                        ui::quiet_face("open post", palette),
                    )
                    .on_click(move |_, _, cx| cx.open_url(&link)),
                ),
            )
            .children(said)
    }

    fn draft_card(&self, row: &DraftRow, palette: &Palette, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let draft = &row.draft;
        let id = draft.id;
        // A hiring post's draft is rejected and restored; a posting's, on a Jobs card, skipped and reset.
        let for_post = matches!(draft.about, About::Post(_));
        let (signal, state) = match draft_state(draft.status) {
            (signal, "skipped") if for_post => (signal, "rejected"),
            state => state,
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
                "Sending an InMail uses one of your InMail credits, unless LinkedIn's window says Free message. \
                 Maverick fills the box and attaches your resume; you click Send.",
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
        let reject = || {
            let (key, verb) = if for_post {
                ("reject", "Reject")
            } else {
                ("skip", "Skip")
            };
            quiet(
                key,
                format!("{verb} the draft to {}", draft.person),
                &verb.to_lowercase(),
            )
            .on_click(cx.listener(move |this, _, _, cx| this.set_status(id, Outreach::Skipped, cx)))
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
                    .child(reject())
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
                    .child(reset())
                    .children(for_post.then(reject));
            }
            Outreach::Skipped if for_post => {
                actions = actions.child(
                    quiet("restore", format!("Restore the draft to {}", draft.person), "restore")
                        .on_click(cx.listener(move |this, _, _, cx| this.reset(id, cx))),
                );
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
            .child(self.posts_column(&palette, cx))
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

    /// Postings' outreach sections, as their cards on the Jobs tab show them.
    struct Cards {
        outreach: Entity<OutreachView>,
        links: Vec<String>,
        /// The scorer's outreach note shown above each posting's section, in parts.
        scorer_note: Vec<String>,
    }

    impl Render for Cards {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let palette = Palette::of(cx);
            let links = self.links.clone();
            let sections: Vec<AnyElement> = links
                .iter()
                .flat_map(|link| {
                    self.outreach.update(cx, |outreach, cx| {
                        let scorer = outreach.scorer_section(link, &self.scorer_note, &palette, cx);
                        scorer
                            .into_iter()
                            .chain([outreach.posting_section(link, &palette, cx)])
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            v_flex().children(sections)
        }
    }

    /// The cards for `links` over the seeded database, with work running on a real runtime so rows can finish.
    fn opened(
        cx: &mut TestAppContext,
        database: &std::path::Path,
        runtime: &tokio::runtime::Runtime,
        links: &[&str],
        scorer_note: &[&str],
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
        let links: Vec<String> = links.iter().map(|link| (*link).to_owned()).collect();
        let scorer_note: Vec<String> = scorer_note.iter().map(|part| (*part).to_owned()).collect();
        let (window, cards) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                let outreach = cx.new(|cx| OutreachView::new(browser, None, window, cx));
                cx.new(|cx| {
                    cx.observe(&outreach, |_, _, cx| cx.notify()).detach();
                    Cards {
                        outreach,
                        links,
                        scorer_note,
                    }
                })
            })
            .unwrap()
        });
        let outreach = cards.read_with(cx, |cards, _| cards.outreach.clone());
        (window, outreach)
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
    fn each_person_in_the_scorer_note_can_be_reached_like_a_checked_one(cx: &mut TestAppContext) {
        let scratch = tempfile::tempdir().unwrap();
        let database = scratch.path().join("maverick.db");
        seeded(&database);
        let note = "Team member found — Ann Named (SWE, linkedin.com/in/ann-named) and Cy Nolink (Recruiter)";
        // Read before, so the section shows its rows without asking the model.
        Store::open(&database)
            .unwrap()
            .save_scorer_people(
                &ScorerPeople {
                    posting_link: "https://simplify.jobs/p/1".into(),
                    note: note.into(),
                    people: vec![
                        Person {
                            name: "Ann Named".into(),
                            title: "SWE".into(),
                            profile_url: Some("https://www.linkedin.com/in/ann-named/".into()),
                            degree: None,
                        },
                        Person {
                            name: "Cy Nolink".into(),
                            title: "Recruiter".into(),
                            profile_url: None,
                            degree: None,
                        },
                    ],
                },
                "2026-11-23T17:00:00Z".parse().unwrap(),
            )
            .unwrap();
        let runtime = runtime();
        let (window, view) = opened(cx, &database, &runtime, &["https://simplify.jobs/p/1"], &[note]);
        let id = |what: &str, name: &str| SharedString::from(format!("{what}-https://simplify.jobs/p/1|named|{name}"));
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(
                window.find(id("draft", "Ann Named")).label(),
                Some("Draft to Ann Named")
            );
            assert_eq!(window.find(id("profile", "Ann Named")).label(), Some("Open profile"));
            // No link in the note: the draft looks the profile up first, and there is none to open yet.
            assert_eq!(
                window.find(id("draft", "Cy Nolink")).label(),
                Some("Draft to Cy Nolink")
            );
            assert!(window.try_find(id("profile", "Cy Nolink")).is_none());
        })
        .unwrap();
        assert!(
            view.read_with(cx, |view, _| view.reachable())
                .contains("https://simplify.jobs/p/1")
        );
    }

    #[gpui_kit::test]
    fn each_person_at_a_posting_can_be_reached_on_their_own(cx: &mut TestAppContext) {
        let scratch = tempfile::tempdir().unwrap();
        let database = scratch.path().join("maverick.db");
        seeded(&database);
        let person = |name: &str, profile: Option<&str>, degree: &str| Person {
            name: name.into(),
            title: "Software Engineer".into(),
            profile_url: profile.map(Into::into),
            degree: Some(degree.into()),
        };
        // Bo Sample already has a draft about this posting. Di Other and Ed Nolink do not.
        Store::open(&database)
            .unwrap()
            .save_check(&Check {
                posting_link: "https://simplify.jobs/p/1".into(),
                verdict: Verdict::Connection,
                team: None,
                people: vec![
                    person("Bo Sample", Some("https://www.linkedin.com/in/bo-sample/"), "2nd"),
                    person("Di Other", Some("https://www.linkedin.com/in/di-other/"), "1st"),
                    person("Ed Nolink", None, "2nd"),
                ],
                note: None,
                checked_at: "2026-11-23T17:00:00Z".parse().unwrap(),
            })
            .unwrap();
        let runtime = runtime();
        let (window, _) = opened(cx, &database, &runtime, &["https://simplify.jobs/p/1"], &[]);
        let id = |what: &str, profile: &str| {
            SharedString::from(format!(
                "{what}-https://simplify.jobs/p/1|https://www.linkedin.com/in/{profile}/"
            ))
        };
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find(id("draft", "bo-sample")).is_none());
            assert_eq!(window.find(id("draft", "di-other")).label(), Some("Draft to Di Other"));
            for profile in ["bo-sample", "di-other"] {
                assert_eq!(window.find(id("profile", profile)).label(), Some("Open profile"));
            }
        })
        .unwrap();
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
        let (window, view) = opened(
            cx,
            &database,
            &runtime,
            &["https://simplify.jobs/p/1", "https://simplify.jobs/p/2"],
            &[],
        );
        let posting = RowKey::Person(
            "https://simplify.jobs/p/2".into(),
            "https://www.linkedin.com/in/cy-person/".into(),
        );
        let draft = RowKey::Draft(id);

        // A scheduled check is running. A click on a row still does something, and says so on the row:
        // here the answers file is missing, which used to fail at the top of the other column.
        cx.update_window(window, |_, window, cx| {
            view.update(cx, |view, _| view.working = Some("checking postings".into()));
            window.render_frame(cx);
            window.click(
                "draft-https://simplify.jobs/p/2|https://www.linkedin.com/in/cy-person/",
                cx,
            );
            window.render_frame(cx);
            assert!(
                window
                    .find("said-https://simplify.jobs/p/2|https://www.linkedin.com/in/cy-person/")
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
                window
                    .find("draft-https://simplify.jobs/p/2|https://www.linkedin.com/in/cy-person/")
                    .label(),
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
                said: "Drafted a connection note to Cy Person. It is below, outlined.".into(),
                draft: Some(id),
            }))
            .unwrap();
        settled(cx, &view, &posting);
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(
                window
                    .find("said-https://simplify.jobs/p/2|https://www.linkedin.com/in/cy-person/")
                    .label(),
                Some("Drafted a connection note to Cy Person. It is below, outlined.")
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
        let (window, view) = opened(cx, &database, &runtime, &["https://simplify.jobs/p/1"], &[]);
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
                assert_eq!(
                    view.read(cx).draft_status("https://simplify.jobs/p/1"),
                    Some(Outreach::Drafted)
                );
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
    fn a_card_shows_its_postings_check_and_draft_and_a_skip_decides_the_draft(cx: &mut TestAppContext) {
        let scratch = tempfile::tempdir().unwrap();
        let database = scratch.path().join("maverick.db");
        let id = seeded(&database);
        let runtime = runtime();
        let (window, view) = opened(
            cx,
            &database,
            &runtime,
            &["https://simplify.jobs/p/1", "https://simplify.jobs/p/2"],
            &[],
        );
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(
                window.find("check-https://simplify.jobs/p/1").label(),
                Some("Team member found: Bo Sample, Storage Engineer (2nd)")
            );
            assert_eq!(
                window.find("check-https://simplify.jobs/p/2").label(),
                Some("None, apply cold")
            );
            assert_eq!(
                window.find(SharedString::from(format!("draft-{id}"))).label(),
                Some("Bo Sample · Backend Engineer at Example Co · drafted")
            );
            // The draft exists, so the check offers no second one.
            assert!(window.try_find("draft-https://simplify.jobs/p/1").is_none());
            let outreach = view.read(cx);
            assert_eq!(
                outreach.reachable(),
                HashSet::from(["https://simplify.jobs/p/1".to_owned()])
            );
            assert_eq!(
                outreach.draft_status("https://simplify.jobs/p/1"),
                Some(Outreach::Drafted)
            );
            assert_eq!(outreach.draft_status("https://simplify.jobs/p/2"), None);
            // A posting's draft does not wait on the Posts tab.
            assert_eq!(outreach.waiting(), 0);

            view.update(cx, |view, cx| view.set_status(id, Outreach::Skipped, cx));
            window.render_frame(cx);
            assert_eq!(
                window.find(SharedString::from(format!("draft-{id}"))).label(),
                Some("Bo Sample · Backend Engineer at Example Co · skipped")
            );
            assert!(window.try_find(SharedString::from(format!("approve-{id}"))).is_none());
            assert_eq!(
                view.read(cx).draft_status("https://simplify.jobs/p/1"),
                Some(Outreach::Skipped)
            );
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn the_posts_tab_sorts_posts_into_found_drafted_sent_and_rejected(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::ui::install(cx);
        });
        let scratch = tempfile::tempdir().unwrap();
        let database = scratch.path().join("maverick.db");
        let posting_draft = seeded(&database);
        let at: Timestamp = "2026-11-23T17:00:00Z".parse().unwrap();
        let post_draft = Store::open(&database)
            .unwrap()
            .add_draft(&Draft {
                id: 0,
                about: About::Post("https://www.linkedin.com/feed/update/urn:li:activity:1/".into()),
                person: "Di Poster".into(),
                headline: "Engineering Manager".into(),
                profile_url: "https://www.linkedin.com/in/di-poster/".into(),
                slot: Slot::Note,
                subject: None,
                text: "Hi Di, I saw your post about the backend team.".into(),
                status: Outreach::Drafted,
                created_at: at,
                updated_at: at,
            })
            .unwrap()
            .unwrap();
        let post = |link: &str, poster: &str, found_at: &str, outreach: Outreach, notes: &str| HiringPost {
            post_link: link.into(),
            role: "Backend Engineer".into(),
            found_at: found_at.parse().unwrap(),
            poster: poster.into(),
            company: "Example Co".into(),
            score: 72,
            variant: "Backend/Infra".into(),
            outreach,
            notes: Some(notes.into()),
        };
        let update = "https://www.linkedin.com/feed/update/urn:li:activity";
        let store = Store::open(&database).unwrap();
        for found in [
            post(
                &format!("{update}:1/"),
                "Di Poster",
                "2026-11-23T17:00:00Z",
                Outreach::Drafted,
                "72, Backend/Infra",
            ),
            post(
                &format!("{update}:2/"),
                "Ed Older",
                "2026-11-22T17:00:00Z",
                Outreach::Drafted,
                "Drafted in the sheet",
            ),
            post(
                &format!("{update}:3/"),
                "Fay Newer",
                "2026-11-23T19:00:00Z",
                Outreach::NotStarted,
                "Poster check unclear",
            ),
            post(
                &format!("{update}:4/"),
                "Gus Skipped",
                "2026-11-23T20:00:00Z",
                Outreach::Skipped,
                "Skipped - senior roles only",
            ),
            HiringPost {
                company: "Zeta Labs".into(),
                ..post(
                    "https://www.linkedin.com/search/results/content/?keywords=hiring",
                    "Hal Searched",
                    "2026-11-21T17:00:00Z",
                    Outreach::NotStarted,
                    "From the tracker",
                )
            },
        ] {
            assert!(store.add_hiring_post(&found).unwrap());
        }
        let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let background = runtime.handle().clone();
        let browser = cx.update(|cx| {
            cx.new(|cx| {
                Browser::new(
                    Bridge::new(),
                    background,
                    Ok("/x".into()),
                    None,
                    Some(database.clone()),
                    cx,
                )
            })
        });
        let (window, view) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| OutreachView::new(browser, None, window, cx))
            })
            .unwrap()
        });
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            // A draft waits on the user, so the tab opens on drafted.
            assert_eq!(view.read(cx).waiting(), 1);
            let switches = ["found", "drafted", "sent", "rejected"].map(|name| {
                window
                    .find(SharedString::from(format!("posts-{name}")))
                    .label()
                    .map(str::to_owned)
            });
            assert_eq!(
                switches,
                ["Found · 2", "Drafted · 2", "Sent · 0", "Rejected · 1"].map(|label| Some(label.to_owned()))
            );
            assert!(
                window
                    .try_find(SharedString::from(format!("draft-{post_draft}")))
                    .is_some()
            );
            // A posting's draft stays on its Jobs card.
            assert!(
                window
                    .try_find(SharedString::from(format!("draft-{posting_draft}")))
                    .is_none()
            );
            assert!(window.try_find("check-https://simplify.jobs/p/1").is_none());
            assert!(
                window
                    .try_find(SharedString::from(format!("reject-{post_draft}")))
                    .is_some()
            );
            assert_eq!(
                window.find("post-0").label(),
                Some("Ed Older · Backend Engineer at Example Co · drafted elsewhere")
            );
            assert!(window.try_find("post-1").is_none());

            // Found lists the posts with no draft, newest first; a post the tracker links to a
            // search page offers no draft.
            window.click("posts-found", cx);
            window.render_frame(cx);
            assert_eq!(
                window.find("post-0").label(),
                Some("Fay Newer · Backend Engineer at Example Co · no draft")
            );
            assert!(window.try_find("draft-post-0").is_some());
            assert_eq!(
                window.find("post-1").label(),
                Some("Hal Searched · Backend Engineer at Zeta Labs · no draft")
            );
            assert!(window.try_find("draft-post-1").is_none());
            assert!(window.try_find("post-2").is_none());

            // A rejected post leaves the queue for rejected, and restore brings it back.
            window.click("reject-post-0", cx);
            window.render_frame(cx);
            assert_eq!(window.find("posts-found").label(), Some("Found · 1"));
            assert_eq!(window.find("posts-rejected").label(), Some("Rejected · 2"));
            assert_eq!(
                window.find("post-0").label(),
                Some("Hal Searched · Backend Engineer at Zeta Labs · no draft")
            );
            window.click("posts-rejected", cx);
            window.render_frame(cx);
            assert_eq!(
                window.find("post-0").label(),
                Some("Gus Skipped · Backend Engineer at Example Co · rejected")
            );
            assert_eq!(
                window.find("post-1").label(),
                Some("Fay Newer · Backend Engineer at Example Co · rejected")
            );
            assert!(window.try_find("draft-post-1").is_none());
            window.click("restore-post-1", cx);
            window.render_frame(cx);
            assert_eq!(window.find("posts-found").label(), Some("Found · 2"));
            assert!(window.try_find("post-1").is_none());

            // A search narrows every section and its count.
            view.update(cx, |view, cx| {
                view.query = Search::new("zeta");
                cx.notify();
            });
            window.click("posts-found", cx);
            window.render_frame(cx);
            assert_eq!(window.find("posts-found").label(), Some("Found · 1"));
            assert_eq!(window.find("posts-drafted").label(), Some("Drafted · 0"));
            assert_eq!(
                window.find("post-0").label(),
                Some("Hal Searched · Backend Engineer at Zeta Labs · no draft")
            );
            assert!(window.try_find("post-1").is_none());
        })
        .unwrap();
        let status = |link: &str| {
            Store::open(&database)
                .unwrap()
                .hiring_posts()
                .unwrap()
                .into_iter()
                .find(|post| post.post_link == link)
                .unwrap()
                .outreach
        };
        assert_eq!(status(&format!("{update}:3/")), Outreach::NotStarted);

        // Rejecting a draft moves it, and its post, to rejected; restore puts the draft back.
        cx.update_window(window, |_, window, cx| {
            view.update(cx, |view, cx| {
                view.query = Search::default();
                view.show_section(PostsSection::Drafted, cx);
            });
            window.render_frame(cx);
            window.click(SharedString::from(format!("reject-{post_draft}")), cx);
            window.render_frame(cx);
            assert!(
                window
                    .try_find(SharedString::from(format!("draft-{post_draft}")))
                    .is_none()
            );
            window.click("posts-rejected", cx);
            window.render_frame(cx);
            assert_eq!(
                window.find(SharedString::from(format!("draft-{post_draft}"))).label(),
                Some("Di Poster · Backend Engineer at Example Co · post · rejected")
            );
            window.click(SharedString::from(format!("restore-{post_draft}")), cx);
            window.render_frame(cx);
            assert!(
                window
                    .try_find(SharedString::from(format!("draft-{post_draft}")))
                    .is_none()
            );
            assert_eq!(window.find("posts-drafted").label(), Some("Drafted · 2"));
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn a_note_marked_sent_leaves_drafted_for_sent(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::ui::install(cx);
        });
        let scratch = tempfile::tempdir().unwrap();
        let database = scratch.path().join("maverick.db");
        let at: Timestamp = "2026-11-23T17:00:00Z".parse().unwrap();
        let update = "https://www.linkedin.com/feed/update/urn:li:activity";
        let store = Store::open(&database).unwrap();
        let post = |n: u8, poster: &str, outreach: Outreach| HiringPost {
            post_link: format!("{update}:{n}/"),
            role: "Backend Engineer".into(),
            found_at: at,
            poster: poster.into(),
            company: "Example Co".into(),
            score: 72,
            variant: "Backend/Infra".into(),
            outreach,
            notes: None,
        };
        // Di Poster has a draft in Maverick. Jo Sheet's note went out before Maverick, so only the sheet knows.
        for found in [
            post(1, "Di Poster", Outreach::Drafted),
            post(2, "Jo Sheet", Outreach::Sent),
        ] {
            assert!(store.add_hiring_post(&found).unwrap());
        }
        let id = store
            .add_draft(&Draft {
                id: 0,
                about: About::Post(format!("{update}:1/")),
                person: "Di Poster".into(),
                headline: "Engineering Manager".into(),
                profile_url: "https://www.linkedin.com/in/di-poster/".into(),
                slot: Slot::Note,
                subject: None,
                text: "Hi Di, I saw your post about the backend team.".into(),
                status: Outreach::Typed,
                created_at: at,
                updated_at: at,
            })
            .unwrap()
            .unwrap();
        drop(store);
        let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let background = runtime.handle().clone();
        let browser = cx.update(|cx| {
            cx.new(|cx| Browser::new(Bridge::new(), background, Ok("/x".into()), None, Some(database), cx))
        });
        let (window, view) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| OutreachView::new(browser, None, window, cx))
            })
            .unwrap()
        });
        let card = SharedString::from(format!("draft-{id}"));
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(window.find("posts-drafted").label(), Some("Drafted · 1"));
            assert_eq!(window.find("posts-sent").label(), Some("Sent · 1"));
            assert!(window.try_find(card.clone()).is_some());
            assert!(window.try_find("post-0").is_none());

            view.update(cx, |view, cx| view.set_status(id, Outreach::Sent, cx));
            window.render_frame(cx);
            assert_eq!(window.find("posts-drafted").label(), Some("Drafted · 0"));
            assert_eq!(window.find("posts-sent").label(), Some("Sent · 2"));
            assert!(window.try_find(card.clone()).is_none());

            view.update(cx, |view, cx| view.show_section(PostsSection::Sent, cx));
            window.render_frame(cx);
            assert_eq!(
                window.find(card.clone()).label(),
                Some("Di Poster · Backend Engineer at Example Co · post · sent")
            );
            assert_eq!(
                window.find("post-0").label(),
                Some("Jo Sheet · Backend Engineer at Example Co · sent")
            );
            // Its note is out, so the post offers no draft.
            assert!(window.try_find("draft-post-0").is_none());
        })
        .unwrap();
    }
}
