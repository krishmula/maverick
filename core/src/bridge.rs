//! The app's end of the link to the browser extension.
//!
//! Each browser profile that runs the extension holds one connection. Messages
//! are JSON in Chrome's native messaging framing: a 4-byte length in native
//! byte order, then the body.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::fs::{self, File, TryLockError};
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener as StdUnixListener;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, mpsc, oneshot, watch};

const MAX_FRAME_BYTES: u32 = 64 * 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// A page command can wait on a web server, as `Page.navigate` does.
const PAGE_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const HELD_EVENTS: usize = 256;

pub type ConnectionId = u32;

/// A tab as the extension reports it.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserTab {
    pub id: i32,
    pub window_id: i32,
    pub title: String,
    pub url: String,
    pub active: bool,
}

/// A tab plus the connection that owns it. Tab ids repeat across profiles.
#[derive(Clone, Debug, PartialEq)]
pub struct Tab {
    pub connection: ConnectionId,
    pub tab: BrowserTab,
}

#[derive(Clone, Debug, PartialEq)]
pub enum BrowserState {
    Disconnected,
    Connected { tabs: Vec<Tab> },
}

#[derive(Debug, Serialize)]
#[serde(tag = "method")]
pub enum Command {
    #[serde(rename = "ping")]
    Ping,
    #[serde(rename = "tabs.activate", rename_all = "camelCase")]
    ActivateTab { tab_id: i32, window_id: i32 },
    /// Opens a tab. The result holds the new tab's `id` and `windowId`.
    #[serde(rename = "tabs.create")]
    OpenTab { url: String, active: bool },
    /// Says whether a tab's page has an element. It works only on sites the extension may read.
    #[serde(rename = "tabs.has", rename_all = "camelCase")]
    HasElement { tab_id: i32, selector: String },
    /// Loads the extension again from disk.
    #[serde(rename = "reload")]
    ReloadExtension,
    #[serde(rename = "debugger.attach", rename_all = "camelCase")]
    AttachDebugger { tab_id: i32 },
    #[serde(rename = "debugger.detach", rename_all = "camelCase")]
    DetachDebugger { tab_id: i32 },
    /// One Chrome DevTools Protocol command, such as `Runtime.evaluate`.
    #[serde(rename = "debugger.send", rename_all = "camelCase")]
    Debug {
        tab_id: i32,
        command: String,
        params: Value,
    },
}

impl Command {
    fn timeout(&self) -> Duration {
        match self {
            Self::Debug { .. } => PAGE_COMMAND_TIMEOUT,
            _ => REQUEST_TIMEOUT,
        }
    }
}

/// A Chrome DevTools Protocol event from a tab the extension holds a debugger on.
#[derive(Clone, Debug, PartialEq)]
pub struct DebuggerEvent {
    pub connection: ConnectionId,
    pub tab_id: i32,
    pub method: String,
    pub params: Value,
}

#[derive(Serialize)]
struct Request<'a> {
    id: u64,
    #[serde(flatten)]
    command: &'a Command,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum FromBrowser {
    Hello {
        version: String,
    },
    Tabs {
        tabs: Vec<BrowserTab>,
    },
    Result {
        id: u64,
        value: Value,
    },
    Error {
        id: u64,
        message: String,
    },
    #[serde(rename_all = "camelCase")]
    Event {
        tab_id: i32,
        method: String,
        params: Value,
    },
}

#[derive(Debug, PartialEq)]
pub enum BridgeError {
    NotConnected,
    Disconnected,
    Timeout,
    Browser(String),
}

impl fmt::Display for BridgeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotConnected => f.write_str("Chrome is not connected"),
            Self::Disconnected => f.write_str("Chrome disconnected before it answered"),
            Self::Timeout => f.write_str("Chrome did not answer in time"),
            Self::Browser(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for BridgeError {}

type Reply = Result<Value, BridgeError>;

struct Connection {
    outbox: mpsc::UnboundedSender<Vec<u8>>,
    pending: HashMap<u64, oneshot::Sender<Reply>>,
    tabs: Vec<BrowserTab>,
    /// The extension's version, once it has said it. Versions before 0.3.0 never say.
    extension: Option<String>,
}

#[derive(Default)]
struct Inner {
    last_request: u64,
    last_connection: ConnectionId,
    connections: BTreeMap<ConnectionId, Connection>,
}

pub struct Bridge {
    inner: Mutex<Inner>,
    state: watch::Sender<BrowserState>,
    events: broadcast::Sender<DebuggerEvent>,
    /// Set once this app has asked an out-of-date extension to reload, so a reload that
    /// brings back the same old version cannot loop.
    asked_reload: AtomicBool,
}

/// The socket the relay connects to, plus the lock that keeps a second app from taking it.
pub struct Listener {
    socket: StdUnixListener,
    _lock: File,
}

/// Creates the socket, replacing one left by an earlier run.
/// Fails with `AddrInUse` when another running app already holds it.
pub fn bind(path: &Path) -> io::Result<Listener> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let lock = File::create(path.with_extension("lock"))?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => {
            return Err(io::Error::new(io::ErrorKind::AddrInUse, "Maverick is already running"));
        }
        Err(TryLockError::Error(error)) => return Err(error),
    }
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let socket = StdUnixListener::bind(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    socket.set_nonblocking(true)?;
    Ok(Listener { socket, _lock: lock })
}

impl Bridge {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::default(),
            state: watch::Sender::new(BrowserState::Disconnected),
            events: broadcast::Sender::new(HELD_EVENTS),
            asked_reload: AtomicBool::new(false),
        })
    }

    pub fn state(&self) -> BrowserState {
        self.state.borrow().clone()
    }

    pub fn subscribe(&self) -> watch::Receiver<BrowserState> {
        self.state.subscribe()
    }

    /// Debugger events from every tab, starting now.
    pub fn events(&self) -> broadcast::Receiver<DebuggerEvent> {
        self.events.subscribe()
    }

    /// True when a connected browser runs an extension other than the one this app ships.
    /// The extension says its version before its first tab report, so ask after one.
    pub fn extension_is_outdated(&self) -> bool {
        let current = crate::extension::version();
        self.lock()
            .connections
            .values()
            .any(|held| held.extension.as_deref() != Some(current.as_str()))
    }

    pub fn connections(&self) -> Vec<ConnectionId> {
        self.lock().connections.keys().copied().collect()
    }

    pub async fn serve(self: Arc<Self>, listener: Listener) -> io::Result<()> {
        let socket = UnixListener::from_std(listener.socket)?;
        let _lock = listener._lock;
        loop {
            let (stream, _) = socket.accept().await?;
            tokio::spawn(self.clone().run_connection(stream));
        }
    }

    pub async fn request(&self, connection: ConnectionId, command: &Command) -> Reply {
        let (reply, answered) = oneshot::channel();
        let id = {
            let mut inner = self.lock();
            inner.last_request += 1;
            let id = inner.last_request;
            let body = serde_json::to_vec(&Request { id, command }).expect("a request is plain JSON");
            let target = inner
                .connections
                .get_mut(&connection)
                .ok_or(BridgeError::NotConnected)?;
            target.outbox.send(body).map_err(|_| BridgeError::NotConnected)?;
            target.pending.insert(id, reply);
            id
        };
        match tokio::time::timeout(command.timeout(), answered).await {
            Ok(Ok(reply)) => reply,
            Ok(Err(_)) => Err(BridgeError::Disconnected),
            Err(_) => {
                if let Some(target) = self.lock().connections.get_mut(&connection) {
                    target.pending.remove(&id);
                }
                Err(BridgeError::Timeout)
            }
        }
    }

    /// Round trip to the extension and back.
    pub async fn ping(&self, connection: ConnectionId) -> Result<Duration, BridgeError> {
        let started = Instant::now();
        self.request(connection, &Command::Ping).await?;
        Ok(started.elapsed())
    }

    /// Round trip to the slowest connected profile.
    pub async fn slowest_ping(&self) -> Result<Duration, BridgeError> {
        let mut slowest = None;
        for connection in self.connections() {
            slowest = slowest.max(Some(self.ping(connection).await?));
        }
        slowest.ok_or(BridgeError::NotConnected)
    }

    async fn run_connection(self: Arc<Self>, stream: UnixStream) {
        let (mut from_browser, mut to_browser) = stream.into_split();
        let (outbox, mut queued) = mpsc::unbounded_channel::<Vec<u8>>();
        let id = {
            let mut inner = self.lock();
            inner.last_connection += 1;
            let id = inner.last_connection;
            inner.connections.insert(
                id,
                Connection {
                    outbox,
                    pending: HashMap::new(),
                    tabs: Vec::new(),
                    extension: None,
                },
            );
            self.publish(&inner);
            id
        };
        let writer = tokio::spawn(async move {
            while let Some(body) = queued.recv().await {
                if write_frame(&mut to_browser, &body).await.is_err() {
                    break;
                }
            }
        });
        // A message the bridge cannot parse ends the connection: the extension is ours,
        // so a bad message means the two sides disagree on the protocol.
        while let Ok(Some(body)) = read_frame(&mut from_browser).await {
            if !self.receive(id, &body) {
                break;
            }
        }
        writer.abort();
        let mut inner = self.lock();
        inner.connections.remove(&id);
        self.publish(&inner);
    }

    fn receive(&self, connection: ConnectionId, body: &[u8]) -> bool {
        let Ok(message) = serde_json::from_slice::<FromBrowser>(body) else {
            return false;
        };
        let mut inner = self.lock();
        let Some(source) = inner.connections.get_mut(&connection) else {
            return false;
        };
        match message {
            FromBrowser::Hello { version } => {
                source.extension = Some(version.clone());
                if version != crate::extension::version() && !self.asked_reload.swap(true, Ordering::Relaxed) {
                    // Nobody waits for the answer: the extension restarts and connects again.
                    let request = Request {
                        id: 0,
                        command: &Command::ReloadExtension,
                    };
                    let _ = source
                        .outbox
                        .send(serde_json::to_vec(&request).expect("a request is plain JSON"));
                }
            }
            FromBrowser::Tabs { tabs } => {
                source.tabs = tabs;
                self.publish(&inner);
            }
            FromBrowser::Result { id, value } => settle(source, id, Ok(value)),
            FromBrowser::Error { id, message } => settle(source, id, Err(BridgeError::Browser(message))),
            FromBrowser::Event { tab_id, method, params } => {
                // Sending fails only when nobody listens, which is fine.
                let _ = self.events.send(DebuggerEvent {
                    connection,
                    tab_id,
                    method,
                    params,
                });
            }
        }
        true
    }

    /// Runs with the lock held so subscribers see states in the order they happened.
    fn publish(&self, inner: &Inner) {
        let state = if inner.connections.is_empty() {
            BrowserState::Disconnected
        } else {
            let tabs = inner
                .connections
                .iter()
                .flat_map(|(&connection, held)| held.tabs.iter().cloned().map(move |tab| Tab { connection, tab }))
                .collect();
            BrowserState::Connected { tabs }
        };
        self.state.send_replace(state);
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().expect("no bridge code panics while holding the lock")
    }
}

fn settle(connection: &mut Connection, id: u64, reply: Reply) {
    // A reply with no waiter belongs to a request that already timed out, or to a reload.
    if let Some(waiter) = connection.pending.remove(&id) {
        let _ = waiter.send(reply);
    }
}

async fn read_frame(reader: &mut (impl AsyncRead + Unpin)) -> io::Result<Option<Vec<u8>>> {
    let mut length = [0u8; 4];
    match reader.read_exact(&mut length).await {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error),
    }
    let length = u32::from_ne_bytes(length);
    if length > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame is larger than the bridge accepts",
        ));
    }
    let mut body = vec![0u8; length as usize];
    reader.read_exact(&mut body).await?;
    Ok(Some(body))
}

async fn write_frame(writer: &mut (impl AsyncWrite + Unpin), body: &[u8]) -> io::Result<()> {
    let length = u32::try_from(body.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "message is too large to frame"))?;
    writer.write_all(&length.to_ne_bytes()).await?;
    writer.write_all(body).await?;
    writer.flush().await
}
