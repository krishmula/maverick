# Maverick

Maverick is a personal desktop app for macOS, written in Rust with [GPUI](https://www.gpui.rs). It works with the Chrome you already use, through a small extension.

Today it lists every open Chrome tab, updates the list as tabs change, and switches Chrome to the tab you pick.

## Requirements

- macOS with Xcode installed.
- Rust 1.89 or later. The code is tested on Rust 1.100.
- Google Chrome 116 or later.

## Run the app

```sh
cargo run --release -p maverick
```

Only one copy runs at a time. A second launch exits without opening a window.

## Connect Chrome

On each start, the app writes the extension to `~/Library/Application Support/dev.maverick.app/extension` and registers itself with Chrome. Load the extension once:

1. Open `chrome://extensions` in Chrome and turn on **Developer mode**.
2. In the app, click **Show folder**.
3. Drag the `extension` folder onto the extensions page.

The app shows your tabs when Chrome connects. If the app starts after Chrome, Chrome reconnects within 30 seconds.

## Use the tab list

- Type to filter tabs by title or address. Every word you type must match.
- Press the Up and Down arrow keys to move the selection.
- Press Enter, or click a row, to switch Chrome to that tab.

## Run the tests

```sh
cargo test --workspace
```

One test drives a real browser and does not run by default. It needs a [Chrome for Testing](https://googlechromelabs.github.io/chrome-for-testing/) binary, because Google Chrome no longer accepts `--load-extension`:

```sh
MAVERICK_E2E_CHROME="/path/to/Google Chrome for Testing" \
	cargo test -p maverick --test chrome -- --ignored --nocapture
```

## Layout

| Path | Contents |
|---|---|
| `core/` | The `maverick-core` crate. It has no UI code. It holds the bridge to Chrome, the relay, and the extension installer. |
| `app/` | The `maverick` crate. It draws the window with GPUI and [GPUI Kit](https://gpui-kit.com). |
| `extension/` | The Chrome extension. It reports tabs and runs the commands the app sends. |

## How the app talks to Chrome

Chrome starts the `maverick` binary as a native messaging host and passes the extension's origin as the first argument. In that mode the binary is a relay. The relay copies bytes between Chrome's pipes and a Unix socket that the running app listens on.

The app opens no network port. Chrome lets only this extension start the relay, and only the current user can open the socket.
