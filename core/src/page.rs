//! Acts on one browser tab through the Chrome DevTools Protocol.
//!
//! The extension reaches the protocol with `chrome.debugger`, so this works in the
//! browser the user already has open. Text and clicks sent this way are real input:
//! the page sees trusted events, the same as from a keyboard and a mouse.

use std::fmt;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::sync::broadcast::error::RecvError;

use crate::bridge::{Bridge, BridgeError, Command, ConnectionId, Tab};

const LOAD_TIMEOUT: Duration = Duration::from_secs(30);
const POLL_GAP: Duration = Duration::from_millis(100);

/// Finds one element in a page.
#[derive(Clone, Debug, PartialEq)]
pub struct Locator {
    name: String,
    script: String,
}

impl Locator {
    /// The first element that matches a CSS selector.
    pub fn css(selector: &str) -> Self {
        Self {
            name: selector.to_owned(),
            script: format!("document.querySelector({})", quoted(selector)),
        }
    }

    /// `script` is a JavaScript expression that gives the element, or null when there is none.
    /// `name` is what errors call the element.
    pub fn script(name: impl Into<String>, script: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            script: script.into(),
        }
    }

    /// The JavaScript expression that gives the element.
    pub fn as_script(&self) -> &str {
        &self.script
    }
}

/// A key that has no text of its own.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Key {
    Enter,
    Tab,
    Escape,
    ArrowDown,
    ArrowUp,
}

impl Key {
    fn name(self) -> &'static str {
        match self {
            Self::Enter => "Enter",
            Self::Tab => "Tab",
            Self::Escape => "Escape",
            Self::ArrowDown => "ArrowDown",
            Self::ArrowUp => "ArrowUp",
        }
    }

    fn code(self) -> u32 {
        match self {
            Self::Enter => 13,
            Self::Tab => 9,
            Self::Escape => 27,
            Self::ArrowDown => 40,
            Self::ArrowUp => 38,
        }
    }
}

pub struct Page {
    bridge: Arc<Bridge>,
    connection: ConnectionId,
    tab_id: i32,
}

#[derive(Debug, PartialEq)]
pub enum PageError {
    Bridge(BridgeError),
    /// A script this module ran in the page threw.
    Script(String),
    NoElement {
        element: String,
    },
    /// Another element lies over the middle of this one, so a click would not reach it.
    Covered {
        element: String,
    },
    CannotFocus {
        element: String,
    },
    NotLoaded(String),
}

impl fmt::Display for PageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bridge(error) => write!(f, "{error}"),
            Self::Script(problem) => write!(f, "a script failed in the page: {problem}"),
            Self::NoElement { element } => write!(f, "the page has no element {element:?}"),
            Self::Covered { element } => write!(f, "another element covers {element:?}"),
            Self::CannotFocus { element } => write!(f, "{element:?} cannot take the keyboard focus"),
            Self::NotLoaded(problem) => write!(f, "the page did not load: {problem}"),
        }
    }
}

impl std::error::Error for PageError {}

impl From<BridgeError> for PageError {
    fn from(error: BridgeError) -> Self {
        Self::Bridge(error)
    }
}

impl Page {
    /// Chrome shows its debugging bar from here until `detach`.
    pub async fn attach(bridge: Arc<Bridge>, tab: &Tab) -> Result<Self, PageError> {
        let page = Self {
            bridge,
            connection: tab.connection,
            tab_id: tab.tab.id,
        };
        page.attach_debugger().await?;
        Ok(page)
    }

    async fn attach_debugger(&self) -> Result<(), PageError> {
        self.request(&Command::AttachDebugger { tab_id: self.tab_id }).await?;
        self.request(&self.debug("Page.enable", json!({}))).await?;
        // Lets the page take input while its tab is in the background.
        let focused = self.debug("Emulation.setFocusEmulationEnabled", json!({ "enabled": true }));
        self.request(&focused).await?;
        Ok(())
    }

    fn debug(&self, command: &str, params: Value) -> Command {
        Command::Debug {
            tab_id: self.tab_id,
            command: command.to_owned(),
            params,
        }
    }

    pub async fn detach(self) -> Result<(), PageError> {
        self.request(&Command::DetachDebugger { tab_id: self.tab_id }).await?;
        Ok(())
    }

    /// Sends one protocol command, such as `Runtime.evaluate`, and returns its result.
    ///
    /// Chrome can drop the debugger in the middle of work, for example when the user closes
    /// the debugging bar. In that case this attaches again and sends the command once more.
    pub async fn send(&self, command: &str, params: Value) -> Result<Value, PageError> {
        let debug = self.debug(command, params);
        match self.request(&debug).await {
            Err(PageError::Bridge(BridgeError::Browser(message))) if lost_debugger(&message) => {
                // The detach clears the extension's record of the tab. It fails when Chrome
                // already detached, which is the usual case here.
                let _ = self.request(&Command::DetachDebugger { tab_id: self.tab_id }).await;
                self.attach_debugger().await?;
                self.request(&debug).await
            }
            reply => reply,
        }
    }

    /// Opens `url` in this tab and waits for the page's load event.
    pub async fn navigate(&self, url: &str) -> Result<(), PageError> {
        let mut events = self.bridge.events();
        let started = self.send("Page.navigate", json!({ "url": url })).await?;
        if let Some(problem) = started.get("errorText").and_then(Value::as_str) {
            return Err(PageError::NotLoaded(problem.to_owned()));
        }
        let loaded = async {
            loop {
                match events.recv().await {
                    Ok(event) => {
                        let ours = event.connection == self.connection && event.tab_id == self.tab_id;
                        if ours && event.method == "Page.loadEventFired" {
                            return true;
                        }
                    }
                    Err(RecvError::Lagged(_)) => {}
                    Err(RecvError::Closed) => return false,
                }
            }
        };
        match tokio::time::timeout(LOAD_TIMEOUT, loaded).await {
            Ok(true) => Ok(()),
            Ok(false) => Err(PageError::Bridge(BridgeError::Disconnected)),
            Err(_) => Err(PageError::NotLoaded(format!("{url} sent no load event in time"))),
        }
    }

    /// Runs a JavaScript expression in the page and returns its value. A promise is awaited.
    pub async fn evaluate(&self, expression: &str) -> Result<Value, PageError> {
        let mut reply = self
            .send(
                "Runtime.evaluate",
                json!({ "expression": expression, "returnByValue": true, "awaitPromise": true }),
            )
            .await?;
        if let Some(thrown) = reply.get("exceptionDetails") {
            let problem = thrown["exception"]["description"]
                .as_str()
                .or(thrown["text"].as_str())
                .unwrap_or("no description");
            return Err(PageError::Script(problem.to_owned()));
        }
        Ok(reply["result"]["value"].take())
    }

    /// Replaces the content of a text field or an editable element with `text`.
    pub async fn fill(&self, element: &Locator, text: &str) -> Result<(), PageError> {
        let focused = self
            .evaluate(&format!(
                r#"(() => {{
                    const element = {};
                    if (!element) return "missing";
                    element.scrollIntoView({{ block: "center", behavior: "instant" }});
                    element.focus();
                    if (typeof element.select === "function") element.select();
                    else getSelection().selectAllChildren(element);
                    return document.activeElement === element ? "focused" : "unfocused";
                }})()"#,
                element.script
            ))
            .await?;
        match focused.as_str() {
            Some("focused") => {}
            Some("missing") => return Err(no_element(element)),
            _ => {
                return Err(PageError::CannotFocus {
                    element: element.name.clone(),
                });
            }
        }
        self.send("Input.insertText", json!({ "text": text })).await?;
        Ok(())
    }

    /// Clicks the middle of an element with the mouse.
    pub async fn click(&self, element: &Locator) -> Result<(), PageError> {
        let target = self
            .evaluate(&format!(
                r#"(() => {{
                    const element = {};
                    if (!element) return "missing";
                    element.scrollIntoView({{ block: "center", behavior: "instant" }});
                    const box = element.getBoundingClientRect();
                    const x = box.left + box.width / 2;
                    const y = box.top + box.height / 2;
                    const top = document.elementFromPoint(x, y);
                    return top && element.contains(top) ? {{ x, y }} : "covered";
                }})()"#,
                element.script
            ))
            .await?;
        let (Some(x), Some(y)) = (target["x"].as_f64(), target["y"].as_f64()) else {
            return Err(match target.as_str() {
                Some("missing") => no_element(element),
                _ => PageError::Covered {
                    element: element.name.clone(),
                },
            });
        };
        for (kind, buttons) in [("mouseMoved", 0), ("mousePressed", 1), ("mouseReleased", 0)] {
            let button = if kind == "mouseMoved" { "none" } else { "left" };
            self.send(
                "Input.dispatchMouseEvent",
                json!({ "type": kind, "x": x, "y": y, "button": button, "buttons": buttons, "clickCount": 1 }),
            )
            .await?;
        }
        Ok(())
    }

    /// Puts files from this computer into a file input, as if the user had picked them.
    pub async fn upload(&self, element: &Locator, files: &[&Path]) -> Result<(), PageError> {
        let found = self
            .send("Runtime.evaluate", json!({ "expression": element.script }))
            .await?;
        let object = found["result"]["objectId"]
            .as_str()
            .ok_or_else(|| no_element(element))?;
        self.send("DOM.setFileInputFiles", json!({ "objectId": object, "files": files }))
            .await?;
        Ok(())
    }

    /// Presses a key in whatever has the keyboard focus.
    pub async fn press(&self, key: Key) -> Result<(), PageError> {
        for kind in ["rawKeyDown", "keyUp"] {
            self.send(
                "Input.dispatchKeyEvent",
                json!({
                    "type": kind,
                    "key": key.name(),
                    "code": key.name(),
                    "windowsVirtualKeyCode": key.code(),
                }),
            )
            .await?;
        }
        Ok(())
    }

    /// Waits until the page has the element.
    pub async fn wait_for(&self, element: &Locator, limit: Duration) -> Result<(), PageError> {
        let deadline = Instant::now() + limit;
        loop {
            if self.evaluate(&format!("!!({})", element.script)).await? == true {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(no_element(element));
            }
            tokio::time::sleep(POLL_GAP).await;
        }
    }

    async fn request(&self, command: &Command) -> Result<Value, PageError> {
        Ok(self.bridge.request(self.connection, command).await?)
    }
}

/// True for Chrome's errors that mean the debugger is no longer attached to the tab.
fn lost_debugger(message: &str) -> bool {
    message.contains("Detached while handling command") || message.contains("Debugger is not attached")
}

/// A string as a JavaScript literal.
fn quoted(text: &str) -> String {
    serde_json::to_string(text).expect("a string is plain JSON")
}

fn no_element(element: &Locator) -> PageError {
    PageError::NoElement {
        element: element.name.clone(),
    }
}
