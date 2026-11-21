//! What the window knows about the browser. Views observe this and call its methods.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::{Context, Task};
use maverick_core::bridge::{Bridge, BrowserState, Command, Tab};
use tokio::runtime::Handle;

#[derive(Clone, Debug, PartialEq)]
pub enum Latency {
    Measuring,
    Measured(Duration),
    Failed(String),
}

pub struct Browser {
    bridge: Arc<Bridge>,
    background: Handle,
    extension_dir: Result<PathBuf, String>,
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
            latency: Latency::Measuring,
            failure: None,
            _watch,
        };
        if browser.is_connected() {
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

    fn is_connected(&self) -> bool {
        matches!(self.state, BrowserState::Connected { .. })
    }

    fn set_state(&mut self, state: BrowserState, cx: &mut Context<Self>) {
        let was_connected = self.is_connected();
        self.state = state;
        if self.is_connected() && !was_connected {
            self.measure(cx);
        }
        cx.notify();
    }
}
