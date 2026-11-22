// Chrome's end of the link to the Maverick app. The app's end is core/src/bridge.rs.

const HOST = "dev.maverick.bridge";
const RETRY_GAP_MS = 3000;
const TABS_DEBOUNCE_MS = 50;
const DEVTOOLS_PROTOCOL = "1.3";

let port = null;
let lastAttempt = 0;
let tabsTimer = null;
// Tabs this extension holds a debugger on. Chrome shows its debugging bar while any are held.
const debugged = new Set();

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
    // Nothing can drive the tabs now, so the debugging bar should not stay up.
    for (const tabId of debugged) chrome.debugger.detach({ tabId }).catch(() => {});
    debugged.clear();
  });
  // The app compares this with the version it ships and asks for a reload when they differ.
  post(opened, { kind: "hello", version: chrome.runtime.getManifest().version });
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
    case "tabs.create": {
      const tab = await chrome.tabs.create({ url: request.url, active: request.active });
      if (request.active) await chrome.windows.update(tab.windowId, { focused: true });
      return { id: tab.id, windowId: tab.windowId };
    }
    case "tabs.has": {
      // Looks at a page without the debugger, so no debugging bar shows while the app only watches.
      const [frame] = await chrome.scripting.executeScript({
        target: { tabId: request.tabId },
        func: (selector) => document.querySelector(selector) !== null,
        args: [request.selector],
      });
      return frame.result === true;
    }
    case "reload":
      // Loads the extension again from the folder the app wrote. The answer goes out first.
      setTimeout(() => chrome.runtime.reload(), 100);
      return null;
    case "debugger.attach":
      if (!debugged.has(request.tabId)) {
        await chrome.debugger.attach({ tabId: request.tabId }, DEVTOOLS_PROTOCOL);
        debugged.add(request.tabId);
      }
      return null;
    case "debugger.detach":
      // Chrome sends no detach event for a detach the extension asked for.
      if (debugged.delete(request.tabId)) await chrome.debugger.detach({ tabId: request.tabId });
      return null;
    case "debugger.send":
      return (await chrome.debugger.sendCommand({ tabId: request.tabId }, request.command, request.params)) ?? null;
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

chrome.debugger.onEvent.addListener((source, method, params) => {
  if (port && debugged.has(source.tabId)) {
    post(port, { kind: "event", tabId: source.tabId, method, params: params ?? null });
  }
});
// Fires when the tab closes or the user dismisses the debugging bar.
chrome.debugger.onDetach.addListener((source) => debugged.delete(source.tabId));

// An open port keeps this service worker alive. While the app is closed the
// worker sleeps, and this alarm wakes it to try again.
chrome.alarms.create("connect", { periodInMinutes: 0.5 });
chrome.alarms.onAlarm.addListener(connect);
chrome.runtime.onStartup.addListener(connect);
chrome.runtime.onInstalled.addListener(connect);
connect();
