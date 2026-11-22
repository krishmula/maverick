//! What the window knows about the browser. Views observe this and call its methods.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::{Context, Task};
use maverick_core::answers::Answers;
use maverick_core::ashby::{self, Report};
use maverick_core::bridge::{Bridge, BridgeError, BrowserState, BrowserTab, Command, Tab};
use maverick_core::page::Page;
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

pub struct Browser {
    bridge: Arc<Bridge>,
    background: Handle,
    extension_dir: Result<PathBuf, String>,
    answers_file: Option<PathBuf>,
    extension_is_outdated: bool,
    fill: Fill,
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
            extension_is_outdated: false,
            fill: Fill::Idle,
            latency: Latency::Measuring,
            failure: None,
            _watch,
        };
        if browser.is_connected() {
            browser.extension_is_outdated = browser.bridge.extension_is_outdated();
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
                this.latency = latency;
                cx.notify();
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

    /// True when Chrome runs an older extension than this app ships and has not reloaded it.
    pub fn extension_is_outdated(&self) -> bool {
        self.extension_is_outdated
    }

    pub fn fill(&self) -> &Fill {
        &self.fill
    }

    /// Fills the application form in a tab with the standard answers. It never submits the form.
    pub fn fill_form(&mut self, tab: &Tab, cx: &mut Context<Self>) {
        self.start_fill(tab.tab.title.clone(), Target::Tab(tab.clone()), cx);
    }

    /// Opens a posting's application form in a new tab and fills it. It never submits the form.
    pub fn apply(&mut self, title: String, address: String, cx: &mut Context<Self>) {
        self.start_fill(title, Target::Posting(address), cx);
    }

    fn start_fill(&mut self, title: String, target: Target, cx: &mut Context<Self>) {
        if matches!(self.fill, Fill::Running { .. }) {
            return;
        }
        self.fill = Fill::Running { title: title.clone() };
        cx.notify();
        let filled = self
            .background
            .spawn(fill_form(self.bridge.clone(), target, self.answers_file.clone()));
        cx.spawn(async move |this, cx| {
            let fill = match filled.await {
                Ok(Ok(report)) => Fill::Done { title, report },
                Ok(Err(reason)) => Fill::Failed { title, reason },
                Err(error) => Fill::Failed {
                    title,
                    reason: error.to_string(),
                },
            };
            this.update(cx, |this, cx| {
                this.fill = fill;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub fn dismiss_fill(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.fill, Fill::Running { .. }) {
            self.fill = Fill::Idle;
            cx.notify();
        }
    }

    fn is_connected(&self) -> bool {
        matches!(self.state, BrowserState::Connected { .. })
    }

    fn set_state(&mut self, state: BrowserState, cx: &mut Context<Self>) {
        let was_connected = self.is_connected();
        self.state = state;
        self.extension_is_outdated = self.is_connected() && self.bridge.extension_is_outdated();
        if self.is_connected() && !was_connected {
            self.measure(cx);
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

async fn fill_form(bridge: Arc<Bridge>, target: Target, answers_file: Option<PathBuf>) -> Result<Report, String> {
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
            let form =
                ashby::form_address(&address).ok_or_else(|| format!("{address} is not a posting Maverick can fill"))?;
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
    let page = Page::attach(bridge, &tab).await.map_err(|error| error.to_string())?;
    let report = match form {
        Some(form) => match page.navigate(&form).await {
            Ok(()) => ashby::fill(&page, &answers).await,
            Err(error) => Err(error.into()),
        },
        None => ashby::fill(&page, &answers).await,
    };
    // Chrome's debugging bar goes away with the detach, whether or not the fill worked.
    let detached = page.detach().await;
    let report = report.map_err(|error| error.to_string())?;
    detached.map_err(|error| error.to_string())?;
    Ok(report)
}
