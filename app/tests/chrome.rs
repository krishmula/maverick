//! Drives a real Chrome with the real extension and the real app binary as the relay.
//!
//! Run with a Chrome for Testing binary, which still accepts `--load-extension`:
//! `MAVERICK_E2E_CHROME=/path/to/chrome cargo test -p maverick --test chrome -- --ignored --nocapture`

use std::fs;
use std::path::Path;
use std::process::{Child, Command as Process};
use std::time::Duration;

use maverick_core::bridge::{self, Bridge, BrowserState, Command, Tab};
use maverick_core::extension::{NATIVE_HOST, native_host_manifest};
use tokio::sync::watch;
use tokio::time::timeout;

struct Browser(Child);

impl Drop for Browser {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

async fn wait_for(states: &mut watch::Receiver<BrowserState>, what: &str, found: impl Fn(&[Tab]) -> bool) -> Vec<Tab> {
    let reached = states.wait_for(|state| matches!(state, BrowserState::Connected { tabs } if found(tabs)));
    match &*timeout(Duration::from_secs(30), reached)
        .await
        .unwrap_or_else(|_| panic!("timed out: {what}"))
        .unwrap()
    {
        BrowserState::Connected { tabs } => tabs.clone(),
        BrowserState::Disconnected => unreachable!("wait_for only accepts a connected state"),
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs MAVERICK_E2E_CHROME set to a Chrome for Testing binary"]
async fn chrome_lists_tabs_and_switches_them_through_the_app_binary() {
    let chrome = std::env::var("MAVERICK_E2E_CHROME").expect("MAVERICK_E2E_CHROME");
    let scratch = tempfile::tempdir().unwrap();
    let socket = scratch.path().join("bridge.sock");
    let bridge = Bridge::new();
    let mut states = bridge.subscribe();
    tokio::spawn(bridge.clone().serve(bridge::bind(&socket).unwrap()));

    // Chrome looks for native messaging hosts inside a custom profile folder,
    // so the test never touches the real Chrome profile.
    let profile = scratch.path().join("profile");
    let hosts = profile.join("NativeMessagingHosts");
    fs::create_dir_all(&hosts).unwrap();
    let relay = Path::new(env!("CARGO_BIN_EXE_maverick"));
    fs::write(
        hosts.join(format!("{NATIVE_HOST}.json")),
        serde_json::to_vec(&native_host_manifest(relay)).unwrap(),
    )
    .unwrap();

    let extension = Path::new(env!("CARGO_MANIFEST_DIR")).join("../extension");
    let _browser = Browser(
        Process::new(chrome)
            .arg(format!("--user-data-dir={}", profile.display()))
            .arg(format!("--load-extension={}", extension.display()))
            .args([
                "--headless=new",
                "--no-first-run",
                "--no-default-browser-check",
                "--disable-popup-blocking",
            ])
            .arg("--disable-features=DisableLoadExtensionCommandLineSwitch")
            // Headless Chrome takes one start page, so that page opens the second tab.
            .arg(concat!(
                "data:text/html,<title>First</title>",
                "<script>window.open('about:blank').document.title = 'Second'</script>",
            ))
            .env("MAVERICK_BRIDGE_SOCKET", &socket)
            .spawn()
            .unwrap(),
    );

    let titled = |tabs: &[Tab], title: &str| tabs.iter().find(|held| held.tab.title == title).cloned();
    let tabs = wait_for(&mut states, "both tabs listed", |tabs| {
        titled(tabs, "First").is_some() && titled(tabs, "Second").is_some()
    })
    .await;
    let background = tabs
        .iter()
        .find(|held| !held.tab.active)
        .expect("one of two tabs is in the background")
        .clone();

    let activate = Command::ActivateTab {
        tab_id: background.tab.id,
        window_id: background.tab.window_id,
    };
    bridge.request(background.connection, &activate).await.unwrap();
    wait_for(&mut states, "the background tab became active", |tabs| {
        titled(tabs, &background.tab.title).is_some_and(|held| held.tab.active)
    })
    .await;

    let mut samples = Vec::new();
    for _ in 0..200 {
        samples.push(bridge.ping(background.connection).await.unwrap());
    }
    samples.sort();
    println!(
        "ping round trip: median {:?}, p95 {:?}, max {:?}",
        samples[100], samples[190], samples[199]
    );
}
