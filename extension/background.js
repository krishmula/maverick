// Chrome's end of the link to the Maverick app. The app's end is core/src/bridge.rs.

const HOST = "dev.maverick.bridge";
const RETRY_GAP_MS = 3000;
const TABS_DEBOUNCE_MS = 50;

let port = null;
let lastAttempt = 0;
let tabsTimer = null;

function connect() {
  if (port || Date.now() - lastAttempt < RETRY_GAP_MS) return;
  lastAttempt = Date.now();
  const opened = chrome.runtime.connectNative(HOST);
  port = opened;
  opened.onMessage.addListener((request) => void answer(opened, request));
  opened.onDisconnect.addListener(() => {
    // Reading lastError stops Chrome from logging an error each time the app is closed.
    void chrome.runtime.lastError;
    if (port === opened) port = null;
  });
  void sendTabs();
}

function post(to, message) {
  try {
    to.postMessage(message);
  } catch {
    // The port closed after the caller picked it. The next connect sends fresh tabs.
  }
}

async function sendTabs() {
  const tabs = await chrome.tabs.query({});
  if (!port) return;
  post(port, {
    kind: "tabs",
    tabs: tabs
      .filter((tab) => tab.id !== undefined && tab.id !== chrome.tabs.TAB_ID_NONE)
      .map((tab) => ({
        id: tab.id,
        windowId: tab.windowId,
        title: tab.title ?? "",
        url: tab.url ?? "",
        active: tab.active,
      })),
  });
}

function tabsChanged() {
  connect();
  if (!port || tabsTimer !== null) return;
  tabsTimer = setTimeout(() => {
    tabsTimer = null;
    void sendTabs();
  }, TABS_DEBOUNCE_MS);
}

async function run(request) {
  switch (request.method) {
    case "ping":
      return null;
    case "tabs.activate":
      await chrome.tabs.update(request.tabId, { active: true });
      await chrome.windows.update(request.windowId, { focused: true });
      return null;
    default:
      throw new Error(`Unknown method: ${request.method}`);
  }
}

async function answer(to, request) {
  try {
    post(to, { kind: "result", id: request.id, value: await run(request) });
  } catch (error) {
    post(to, { kind: "error", id: request.id, message: error instanceof Error ? error.message : String(error) });
  }
}

for (const event of [
  chrome.tabs.onCreated,
  chrome.tabs.onRemoved,
  chrome.tabs.onUpdated,
  chrome.tabs.onActivated,
  chrome.tabs.onMoved,
  chrome.tabs.onAttached,
  chrome.tabs.onDetached,
  chrome.tabs.onReplaced,
]) {
  event.addListener(tabsChanged);
}

// An open port keeps this service worker alive. While the app is closed the
// worker sleeps, and this alarm wakes it to try again.
chrome.alarms.create("connect", { periodInMinutes: 0.5 });
chrome.alarms.onAlarm.addListener(connect);
chrome.runtime.onStartup.addListener(connect);
chrome.runtime.onInstalled.addListener(connect);
connect();
