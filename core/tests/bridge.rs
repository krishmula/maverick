use std::io::{PipeReader, PipeWriter, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use maverick_core::bridge::{self, Bridge, BridgeError, BrowserState, BrowserTab, Command, Tab};
use maverick_core::relay;
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(5);

fn send(to: &mut impl Write, message: Value) {
    let body = serde_json::to_vec(&message).unwrap();
    to.write_all(&u32::try_from(body.len()).unwrap().to_ne_bytes()).unwrap();
    to.write_all(&body).unwrap();
}

fn receive(from: &mut impl Read) -> Value {
    let mut length = [0u8; 4];
    from.read_exact(&mut length).unwrap();
    let mut body = vec![0u8; u32::from_ne_bytes(length) as usize];
    from.read_exact(&mut body).unwrap();
    serde_json::from_slice(&body).unwrap()
}

fn serve(socket: &Path) -> (Arc<Bridge>, watch::Receiver<BrowserState>) {
    let bridge = Bridge::new();
    let states = bridge.subscribe();
    tokio::spawn(bridge.clone().serve(bridge::bind(socket).unwrap()));
    (bridge, states)
}

async fn next(states: &mut watch::Receiver<BrowserState>, wanted: &BrowserState) {
    timeout(WAIT, states.wait_for(|state| state == wanted))
        .await
        .unwrap_or_else(|_| panic!("the bridge never reached {wanted:?}"))
        .unwrap();
}

/// Stands in for Chrome: holds the two pipes Chrome would give a native messaging host.
fn start_relay(socket: &Path) -> (PipeWriter, PipeReader, thread::JoinHandle<std::io::Result<()>>) {
    let (relay_stdin, browser_writes) = std::io::pipe().unwrap();
    let (browser_reads, relay_stdout) = std::io::pipe().unwrap();
    let app = UnixStream::connect(socket).unwrap();
    let relay = thread::spawn(move || relay::pump(relay_stdin, relay_stdout, app));
    (browser_writes, browser_reads, relay)
}

#[tokio::test(flavor = "multi_thread")]
async fn the_browser_and_the_app_talk_through_the_relay() {
    let scratch = tempfile::tempdir().unwrap();
    let socket = scratch.path().join("bridge.sock");
    let (bridge, mut states) = serve(&socket);
    let (mut to_app, mut from_app, relay) = start_relay(&socket);

    send(
        &mut to_app,
        json!({ "kind": "tabs", "tabs": [
            { "id": 7, "windowId": 3, "title": "Careers", "url": "https://example.com/jobs", "active": true },
        ] }),
    );
    let careers = BrowserTab {
        id: 7,
        window_id: 3,
        title: "Careers".into(),
        url: "https://example.com/jobs".into(),
        active: true,
    };
    next(
        &mut states,
        &BrowserState::Connected {
            tabs: vec![Tab {
                connection: 1,
                tab: careers,
            }],
        },
    )
    .await;

    let browser = thread::spawn(move || {
        let ping = receive(&mut from_app);
        send(
            &mut to_app,
            json!({ "kind": "result", "id": ping["id"], "value": null }),
        );
        let activate = receive(&mut from_app);
        send(
            &mut to_app,
            json!({ "kind": "error", "id": activate["id"], "message": "No tab with id: 9." }),
        );
        (ping, activate, to_app)
    });
    assert_eq!(bridge.request(1, &Command::Ping).await, Ok(Value::Null));
    assert_eq!(
        bridge
            .request(
                1,
                &Command::ActivateTab {
                    tab_id: 9,
                    window_id: 3
                }
            )
            .await,
        Err(BridgeError::Browser("No tab with id: 9.".into()))
    );
    let (ping, activate, to_app) = browser.join().unwrap();
    assert_eq!(ping, json!({ "id": 1, "method": "ping" }));
    assert_eq!(
        activate,
        json!({ "id": 2, "method": "tabs.activate", "tabId": 9, "windowId": 3 })
    );

    drop(to_app);
    next(&mut states, &BrowserState::Disconnected).await;
    relay.join().unwrap().unwrap();
    assert_eq!(bridge.request(1, &Command::Ping).await, Err(BridgeError::NotConnected));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_relay_exits_when_the_app_goes_away() {
    let scratch = tempfile::tempdir().unwrap();
    let socket = scratch.path().join("bridge.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let (_to_app, _from_app, relay) = start_relay(&socket);

    let (app_end, _) = listener.accept().unwrap();
    drop(app_end);

    timeout(WAIT, tokio::task::spawn_blocking(move || relay.join().unwrap()))
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_message_the_bridge_cannot_parse_ends_the_connection() {
    let scratch = tempfile::tempdir().unwrap();
    let socket = scratch.path().join("bridge.sock");
    let (_bridge, mut states) = serve(&socket);
    let mut browser = UnixStream::connect(&socket).unwrap();
    next(&mut states, &BrowserState::Connected { tabs: vec![] }).await;

    send(&mut browser, json!({ "kind": "tabs", "tabs": "none" }));

    next(&mut states, &BrowserState::Disconnected).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn two_profiles_keep_their_own_tabs() {
    let scratch = tempfile::tempdir().unwrap();
    let socket = scratch.path().join("bridge.sock");
    let (_bridge, mut states) = serve(&socket);
    let tab = |title: &str| json!({ "id": 1, "windowId": 1, "title": title, "url": "", "active": true });
    let held = |connection, title: &str| Tab {
        connection,
        tab: BrowserTab {
            id: 1,
            window_id: 1,
            title: title.into(),
            url: String::new(),
            active: true,
        },
    };

    let mut personal = UnixStream::connect(&socket).unwrap();
    send(&mut personal, json!({ "kind": "tabs", "tabs": [tab("Personal")] }));
    next(
        &mut states,
        &BrowserState::Connected {
            tabs: vec![held(1, "Personal")],
        },
    )
    .await;
    let mut school = UnixStream::connect(&socket).unwrap();
    send(&mut school, json!({ "kind": "tabs", "tabs": [tab("School")] }));
    next(
        &mut states,
        &BrowserState::Connected {
            tabs: vec![held(1, "Personal"), held(2, "School")],
        },
    )
    .await;

    drop(personal);
    next(
        &mut states,
        &BrowserState::Connected {
            tabs: vec![held(2, "School")],
        },
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_app_cannot_take_the_socket_from_a_running_one() {
    let scratch = tempfile::tempdir().unwrap();
    let socket = scratch.path().join("bridge.sock");
    let (_bridge, mut states) = serve(&socket);

    let second = bridge::bind(&socket).map(|_| ()).unwrap_err();

    assert_eq!(second.kind(), std::io::ErrorKind::AddrInUse);
    let _browser = UnixStream::connect(&socket).unwrap();
    next(&mut states, &BrowserState::Connected { tabs: vec![] }).await;
}
