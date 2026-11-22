//! What the window knows about the browser. Views observe this and call its methods.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::{Context, Task};
use maverick_core::answers::Answers;
use maverick_core::apply::{self, Report, System};
use maverick_core::bridge::{Bridge, BridgeError, BrowserState, BrowserTab, Command, ConnectionId, Tab};
use maverick_core::jobs::{Application, posting_address};
use maverick_core::page::Page;
use maverick_core::store::Store;
use tokio::runtime::Handle;

#[derive(Clone, Debug, PartialEq)]
pub enum Latency {
    Measuring,
    Measured(Duration),
    Failed(String),
}

/// The latest run of the form filler.
#[derive(Clone, Debug, PartialEq)]
pub enum Fill {
    Idle,
    Running { title: String },
    Done { title: String, report: Report },
    Failed { title: String, reason: String },
}

/// What a fill run is about, so a view can tell whether the run belongs to the row it shows.
#[derive(Clone, Debug, PartialEq)]
pub enum Subject {
    /// A posting from the jobs list, by its address.
    Posting(String),
    Tab(ConnectionId, i32),
}

/// An application the user submitted in this run.
#[derive(Clone, Debug, PartialEq)]
pub struct Applied {
    pub title: String,
    /// The posting's address.
    pub address: String,
    /// Why the local database could not record it, when it could not.
    pub not_saved: Option<String>,
}

/// How often a filled form is checked for a submission, and for how long.
const SUBMIT_CHECK_GAP: Duration = Duration::from_secs(2);
const SUBMIT_WATCH: Duration = Duration::from_secs(30 * 60);

pub struct Browser {
    bridge: Arc<Bridge>,
    background: Handle,
    extension_dir: Result<PathBuf, String>,
    answers_file: Option<PathBuf>,
    database: Option<PathBuf>,
    applied: Vec<Applied>,
    /// The connections the last latency measurement covered.
    measured: Vec<ConnectionId>,
    measures: u64,
    extension_is_outdated: bool,
    fill: Fill,
    subject: Option<Subject>,
    /// The tab the last finished fill left its form in.
    filled_tab: Option<Tab>,
    state: BrowserState,
    latency: Latency,
    failure: Option<String>,
    _watch: Task<()>,
}

impl Browser {
    pub fn new(
        bridge: Arc<Bridge>,
        background: Handle,
        extension_dir: Result<PathBuf, String>,
        answers_file: Option<PathBuf>,
        database: Option<PathBuf>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut states = bridge.subscribe();
        let _watch = cx.spawn(async move |this, cx| {
            while states.changed().await.is_ok() {
                let state = states.borrow_and_update().clone();
                if this.update(cx, |this, cx| this.set_state(state, cx)).is_err() {
                    break;
                }
            }
        });
        let mut browser = Self {
            state: bridge.state(),
            bridge,
            background,
            extension_dir,
            answers_file,
            database,
            applied: Vec::new(),
            measured: Vec::new(),
            measures: 0,
            extension_is_outdated: false,
            fill: Fill::Idle,
            subject: None,
            filled_tab: None,
            latency: Latency::Measuring,
            failure: None,
            _watch,
        };
        if browser.is_connected() {
            browser.extension_is_outdated = browser.bridge.extension_is_outdated();
            browser.measured = browser.bridge.connections();
            browser.measure(cx);
        }
        browser
    }

    pub fn state(&self) -> &BrowserState {
        &self.state
    }

    /// The folder to load into Chrome, or why the extension could not be set up.
    pub fn extension_dir(&self) -> Result<&Path, &str> {
        self.extension_dir.as_deref().map_err(String::as_str)
    }

    pub fn latency(&self) -> &Latency {
        &self.latency
    }

    /// Why the last tab switch failed, until one succeeds.
    pub fn failure(&self) -> Option<&str> {
        self.failure.as_deref()
    }

    pub fn measure(&mut self, cx: &mut Context<Self>) {
        self.latency = Latency::Measuring;
        self.measures += 1;
        let measure = self.measures;
        cx.notify();
        let bridge = self.bridge.clone();
        let pinged = self.background.spawn(async move { bridge.slowest_ping().await });
        cx.spawn(async move |this, cx| {
            let latency = match pinged.await {
                Ok(Ok(took)) => Latency::Measured(took),
                Ok(Err(error)) => Latency::Failed(error.to_string()),
                Err(error) => Latency::Failed(error.to_string()),
            };
            this.update(cx, |this, cx| {
                // An older measurement can finish last, for example one cut off by a reconnect.
                if this.measures == measure {
                    this.latency = latency;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    pub fn activate(&mut self, tab: &Tab, cx: &mut Context<Self>) {
        let bridge = self.bridge.clone();
        let connection = tab.connection;
        let command = Command::ActivateTab {
            tab_id: tab.tab.id,
            window_id: tab.tab.window_id,
        };
        let activated = self
            .background
            .spawn(async move { bridge.request(connection, &command).await });
        cx.spawn(async move |this, cx| {
            let failure = match activated.await {
                Ok(Ok(_)) => None,
                Ok(Err(error)) => Some(error.to_string()),
                Err(error) => Some(error.to_string()),
            };
            this.update(cx, |this, cx| {
                this.failure = failure;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The tokio runtime that runs work off the main thread.
    pub fn background(&self) -> Handle {
        self.background.clone()
    }

    /// True when Chrome runs an older extension than this app ships and has not reloaded it.
    pub fn extension_is_outdated(&self) -> bool {
        self.extension_is_outdated
    }

    /// The applications the user submitted in this run, oldest first.
    pub fn applied(&self) -> &[Applied] {
        &self.applied
    }

    #[cfg(test)]
    pub fn fill(&self) -> &Fill {
        &self.fill
    }

    /// The latest fill run, when it is about `subject`.
    pub fn fill_of(&self, subject: &Subject) -> Option<&Fill> {
        (self.subject.as_ref() == Some(subject) && self.fill != Fill::Idle).then_some(&self.fill)
    }

    /// Brings the tab with the last filled form forward, so the user can review it and submit.
    pub fn review(&mut self, cx: &mut Context<Self>) {
        if let Some(tab) = self.filled_tab.clone() {
            self.activate(&tab, cx);
        }
    }

    /// Fills the application form in a tab with the standard answers. It never submits the form.
    pub fn fill_form(&mut self, tab: &Tab, cx: &mut Context<Self>) {
        let subject = Subject::Tab(tab.connection, tab.tab.id);
        self.start_fill(tab.tab.title.clone(), subject, Target::Tab(tab.clone()), cx);
    }

    /// Opens a posting's application form in a new tab and fills it. It never submits the form.
    pub fn apply(&mut self, title: String, address: String, cx: &mut Context<Self>) {
        self.start_fill(title, Subject::Posting(address.clone()), Target::Posting(address), cx);
    }

    fn start_fill(&mut self, title: String, subject: Subject, target: Target, cx: &mut Context<Self>) {
        if matches!(self.fill, Fill::Running { .. }) {
            return;
        }
        self.fill = Fill::Running { title: title.clone() };
        self.subject = Some(subject);
        self.filled_tab = None;
        cx.notify();
        let filled = self.background.spawn(fill_form(
            self.bridge.clone(),
            title.clone(),
            target,
            self.answers_file.clone(),
        ));
        cx.spawn(async move |this, cx| {
            let (fill, filled) = match filled.await {
                Ok(Ok(filled)) => (
                    Fill::Done {
                        title: title.clone(),
                        report: filled.report.clone(),
                    },
                    Some(filled),
                ),
                Ok(Err(reason)) => (Fill::Failed { title, reason }, None),
                Err(error) => (
                    Fill::Failed {
                        title,
                        reason: error.to_string(),
                    },
                    None,
                ),
            };
            this.update(cx, |this, cx| {
                this.fill = fill;
                if let Some(filled) = filled {
                    this.filled_tab = Some(filled.tab.clone());
                    this.watch_for_submit(filled, cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Waits for the user to submit a filled form, then records the application.
    fn watch_for_submit(&mut self, filled: Filled, cx: &mut Context<Self>) {
        let Filled {
            system,
            tab,
            title,
            address,
            ..
        } = filled;
        let (bridge, database) = (self.bridge.clone(), self.database.clone());
        let recorded = self.background.spawn(async move {
            let confirmation = system.submitted();
            if !submitted(
                &bridge,
                tab.connection,
                tab.tab.id,
                confirmation,
                SUBMIT_CHECK_GAP,
                SUBMIT_WATCH,
            )
            .await
            {
                return None;
            }
            let (role, link) = (title.clone(), address.clone());
            let saved = tokio::task::spawn_blocking(move || record_applied(database.as_deref(), &role, &link)).await;
            let not_saved = match saved {
                Ok(Ok(())) => None,
                Ok(Err(reason)) => Some(reason),
                Err(stopped) => Some(stopped.to_string()),
            };
            Some(Applied {
                title,
                address,
                not_saved,
            })
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Some(applied)) = recorded.await {
                this.update(cx, |this, cx| {
                    this.applied.push(applied);
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    pub fn dismiss_fill(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.fill, Fill::Running { .. }) {
            self.fill = Fill::Idle;
            self.subject = None;
            cx.notify();
        }
    }

    fn is_connected(&self) -> bool {
        matches!(self.state, BrowserState::Connected { .. })
    }

    fn set_state(&mut self, state: BrowserState, cx: &mut Context<Self>) {
        self.state = state;
        self.extension_is_outdated = self.is_connected() && self.bridge.extension_is_outdated();
        // States can arrive merged, so a reconnect may never show as a disconnect.
        // A changed set of connections is the reliable sign that the link is new.
        let connections = self.bridge.connections();
        if connections != self.measured {
            self.measured = connections;
            if self.is_connected() {
                self.measure(cx);
            }
        }
        cx.notify();
    }
}

/// Where the form to fill is.
enum Target {
    /// A tab the user already has open.
    Tab(Tab),
    /// A posting's address, to open in a new tab.
    Posting(String),
}

/// A form that was filled and now waits for the user to submit it.
struct Filled {
    report: Report,
    system: System,
    tab: Tab,
    title: String,
    /// The posting's address.
    address: String,
}

/// True once the tab shows the element at `confirmation`, which an application system shows
/// after a submission. False when the tab closes, Chrome goes away, or `limit` passes first.
async fn submitted(
    bridge: &Bridge,
    connection: ConnectionId,
    tab_id: i32,
    confirmation: &str,
    gap: Duration,
    limit: Duration,
) -> bool {
    let check = Command::HasElement {
        tab_id,
        selector: confirmation.to_owned(),
    };
    let deadline = tokio::time::Instant::now() + limit;
    while tokio::time::Instant::now() < deadline {
        tokio::time::sleep(gap).await;
        match bridge.request(connection, &check).await {
            Ok(shown) if shown == true => return true,
            // A slow answer says nothing about the page.
            Ok(_) | Err(BridgeError::Timeout) => {}
            Err(_) => return false,
        }
    }
    false
}

/// Marks the posting applied in the local database and adds the application.
fn record_applied(database: Option<&Path>, role: &str, address: &str) -> Result<(), String> {
    let database = database.ok_or("This system has no application data directory.")?;
    let said = |error: maverick_core::store::StoreError| error.to_string();
    let mut store = Store::open(database).map_err(said)?;
    let links: Vec<String> = store
        .postings()
        .map_err(said)?
        .into_iter()
        .map(|posting| posting.link)
        .filter(|link| apply::posting_address(&posting_address(link)).as_deref() == Some(address))
        .collect();
    store
        .record_application(&links, &Application::submitted_today(role, address))
        .map_err(said)
}

async fn fill_form(
    bridge: Arc<Bridge>,
    title: String,
    target: Target,
    answers_file: Option<PathBuf>,
) -> Result<Filled, String> {
    let answers_file = answers_file.ok_or("This system has no application data directory.")?;
    // Read on every run, so an edit to the file counts at once.
    let answers = Answers::load(&answers_file)?;
    let said = |error: BridgeError| error.to_string();
    // Either way the tab comes forward, so the user can watch the form fill and then review it.
    let (tab, form) = match target {
        Target::Tab(tab) => {
            let forward = Command::ActivateTab {
                tab_id: tab.tab.id,
                window_id: tab.tab.window_id,
            };
            bridge.request(tab.connection, &forward).await.map_err(said)?;
            (tab, None)
        }
        Target::Posting(address) => {
            let form = System::of(&address)
                .and_then(|system| system.form_address(&address))
                .ok_or_else(|| format!("{address} is not a posting Maverick can fill"))?;
            let connection = *bridge
                .connections()
                .first()
                .ok_or_else(|| said(BridgeError::NotConnected))?;
            // The tab starts empty, so the debugger is attached before the form loads.
            let blank = Command::OpenTab {
                url: "about:blank".into(),
                active: true,
            };
            let opened = bridge.request(connection, &blank).await.map_err(said)?;
            let number = |key: &str| {
                opened[key]
                    .as_i64()
                    .and_then(|number| i32::try_from(number).ok())
                    .ok_or("Chrome did not say which tab it opened.")
            };
            let tab = BrowserTab {
                id: number("id")?,
                window_id: number("windowId")?,
                title: String::new(),
                url: String::new(),
                active: true,
            };
            (Tab { connection, tab }, Some(form))
        }
    };
    let on = form.as_deref().unwrap_or(&tab.tab.url);
    let not_fillable = "This tab is not on a posting Maverick can fill.";
    let system = System::of(on).ok_or(not_fillable)?;
    let address = system.posting_address(on).ok_or(not_fillable)?;
    let page = Page::attach(bridge, &tab).await.map_err(|error| error.to_string())?;
    let report = match form {
        Some(form) => match page.navigate(&form).await {
            Ok(()) => system.fill(&page, &answers).await,
            Err(error) => Err(error.into()),
        },
        None => system.fill(&page, &answers).await,
    };
    // Chrome's debugging bar goes away with the detach, whether or not the fill worked.
    let detached = page.detach().await;
    let report = report.map_err(|error| error.to_string())?;
    detached.map_err(|error| error.to_string())?;
    Ok(Filled {
        report,
        system,
        tab,
        title,
        address,
    })
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;

    use maverick_core::bridge;
    use serde_json::{Value, json};

    use super::*;

    /// Answers each page check from `answers`, in order, as the extension would.
    fn fake_chrome(socket: &Path, answers: Vec<Value>) -> std::thread::JoinHandle<Vec<Value>> {
        let mut chrome = UnixStream::connect(socket).unwrap();
        std::thread::spawn(move || {
            let mut asked = Vec::new();
            for answer in answers {
                let mut length = [0u8; 4];
                chrome.read_exact(&mut length).unwrap();
                let mut body = vec![0u8; u32::from_ne_bytes(length) as usize];
                chrome.read_exact(&mut body).unwrap();
                let request: Value = serde_json::from_slice(&body).unwrap();
                let mut reply = answer;
                reply["id"] = request["id"].clone();
                let reply = serde_json::to_vec(&reply).unwrap();
                chrome
                    .write_all(&u32::try_from(reply.len()).unwrap().to_ne_bytes())
                    .unwrap();
                chrome.write_all(&reply).unwrap();
                asked.push(request);
            }
            asked
        })
    }

    async fn watch(answers: Vec<Value>) -> (bool, Vec<Value>) {
        let scratch = tempfile::tempdir().unwrap();
        let socket = scratch.path().join("bridge.sock");
        let bridge = Bridge::new();
        tokio::spawn(bridge.clone().serve(bridge::bind(&socket).unwrap()));
        let chrome = fake_chrome(&socket, answers);
        while bridge.connections().is_empty() {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        let connection = bridge.connections()[0];
        let gap = Duration::from_millis(5);
        let seen = submitted(&bridge, connection, 31, ".sent", gap, Duration::from_secs(5)).await;
        (seen, chrome.join().unwrap())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_watch_ends_when_the_confirmation_shows() {
        let no = json!({ "kind": "result", "value": false });
        let yes = json!({ "kind": "result", "value": true });
        let (seen, asked) = watch(vec![no.clone(), no, yes]).await;
        assert!(seen);
        assert_eq!(asked.len(), 3);
        assert_eq!(
            asked[0],
            json!({
                "id": asked[0]["id"],
                "method": "tabs.has",
                "tabId": 31,
                "selector": ".sent",
            })
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_watch_gives_up_when_the_tab_is_gone() {
        let no = json!({ "kind": "result", "value": false });
        let gone = json!({ "kind": "error", "message": "No tab with id: 31" });
        assert!(!watch(vec![no, gone]).await.0);
    }

    #[test]
    fn a_submitted_application_is_saved_under_the_posting_address() {
        let scratch = tempfile::tempdir().unwrap();
        let database = scratch.path().join("maverick.db");
        let address = "https://jobs.ashbyhq.com/example-co/0f3b8231-3a32-478a-b994-3441f4bb9455";
        record_applied(Some(&database), "Backend Engineer at Example Co", address).unwrap();
        let saved = Store::open(&database).unwrap().applications().unwrap();
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].link.as_deref(), Some(address));
        assert_eq!(saved[0].role.as_deref(), Some("Backend Engineer at Example Co"));
        assert!(record_applied(None, "x", address).is_err());
    }
}
