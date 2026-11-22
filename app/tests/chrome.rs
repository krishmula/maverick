//! Drives a real Chrome with the real extension and the real app binary as the relay.
//!
//! Run with a Chrome for Testing binary, which still accepts `--load-extension`:
//! `MAVERICK_E2E_CHROME=/path/to/chrome cargo test -p maverick --test chrome -- --ignored --nocapture`

use std::fs;
use std::path::Path;
use std::process::{Child, Command as Process};
use std::sync::Arc;
use std::time::Duration;

use maverick_core::answers::Answers;
use maverick_core::ashby::{self, FieldResult, Source};
use maverick_core::bridge::{self, Bridge, BrowserState, Command, Tab};
use maverick_core::extension::{NATIVE_HOST, native_host_manifest};
use maverick_core::page::{Locator, Page, PageError};
use serde_json::json;
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::time::timeout;

/// Opens a second tab, so the tab titled First ends up in the background.
const TWO_TABS: &str = concat!(
    "data:text/html,<title>First</title>",
    "<script>window.open('about:blank').document.title = 'Second'</script>",
);

const FORM: &str = r#"<!doctype html>
<title>Form</title>
<input id="name" value="old name">
<textarea id="why"></textarea>
<div id="message" contenteditable="true">old draft</div>
<input id="resume" type="file">
<button id="send" type="button">Send</button>
<div style="position: relative; width: 200px">
  <button id="covered" type="button">Covered</button>
  <div style="position: absolute; inset: 0"></div>
</div>
<div style="height: 3000px"></div>
<button id="far" type="button">Far down the page</button>
<script>
  window.seen = [];
  for (const id of ["name", "why", "message"]) {
    document.getElementById(id).addEventListener("input", (event) => seen.push(`${id} input ${event.isTrusted}`));
  }
  for (const id of ["send", "covered", "far"]) {
    document.getElementById(id).addEventListener("click", (event) => seen.push(`${id} click ${event.isTrusted}`));
  }
</script>
"#;

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

struct Running {
    bridge: Arc<Bridge>,
    states: watch::Receiver<BrowserState>,
    scratch: TempDir,
    _browser: Browser,
}

/// Starts the bridge and a Chrome that runs the extension, with the app binary as the relay.
fn launch(start_page: &str) -> Running {
    let chrome = std::env::var("MAVERICK_E2E_CHROME").expect("MAVERICK_E2E_CHROME");
    let scratch = tempfile::tempdir().unwrap();
    let socket = scratch.path().join("bridge.sock");
    let bridge = Bridge::new();
    let states = bridge.subscribe();
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
    let browser = Browser(
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
            // Headless Chrome takes one start page.
            .arg(start_page)
            .env("MAVERICK_BRIDGE_SOCKET", &socket)
            .spawn()
            .unwrap(),
    );
    Running {
        bridge,
        states,
        scratch,
        _browser: browser,
    }
}

fn titled(tabs: &[Tab], title: &str) -> Option<Tab> {
    tabs.iter().find(|held| held.tab.title == title).cloned()
}

/// Serves one page at every path and returns its address.
async fn serve(page: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut request = [0u8; 4096];
                let _ = stream.read(&mut request).await;
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    page.len()
                );
                let _ = stream.write_all(head.as_bytes()).await;
                let _ = stream.write_all(page.as_bytes()).await;
            });
        }
    });
    format!("http://{address}/")
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs MAVERICK_E2E_CHROME set to a Chrome for Testing binary"]
async fn chrome_lists_tabs_and_switches_them_through_the_app_binary() {
    let Running {
        bridge,
        mut states,
        scratch: _scratch,
        _browser,
    } = launch(TWO_TABS);

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

    let third = Command::OpenTab {
        url: "data:text/html,<title>Third</title>".into(),
        active: true,
    };
    let opened = bridge.request(background.connection, &third).await.unwrap();
    wait_for(&mut states, "the opened tab listed as active", |tabs| {
        titled(tabs, "Third").is_some_and(|held| held.tab.active && opened["id"] == held.tab.id)
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

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs MAVERICK_E2E_CHROME set to a Chrome for Testing binary"]
async fn a_background_tab_takes_typing_clicks_and_a_file_as_real_input() {
    let Running {
        bridge,
        mut states,
        scratch,
        _browser,
    } = launch(TWO_TABS);
    let form = serve(FORM).await;
    let tabs = wait_for(&mut states, "both tabs listed", |tabs| {
        titled(tabs, "First").is_some() && titled(tabs, "Second").is_some_and(|held| held.tab.active)
    })
    .await;
    let background = titled(&tabs, "First").unwrap();
    assert!(!background.tab.active);

    let page = Page::attach(bridge.clone(), &background).await.unwrap();
    page.navigate(&form).await.unwrap();
    assert_eq!(page.evaluate("document.title").await.unwrap(), "Form");

    page.fill(&Locator::css("#name"), "Ada before the drop").await.unwrap();
    page.fill(&Locator::css("#why"), "Line one\nLine two").await.unwrap();
    page.fill(&Locator::css("#message"), "Hello, \"Grace\"").await.unwrap();
    // Chrome can drop the debugger mid-work. The page attaches again and carries on.
    let dropped = Command::DetachDebugger {
        tab_id: background.tab.id,
    };
    bridge.request(background.connection, &dropped).await.unwrap();
    page.fill(&Locator::css("#name"), "Ada Lovelace").await.unwrap();
    assert!(!bridge.extension_is_outdated(), "the extension said its version");
    page.click(&Locator::css("#send")).await.unwrap();
    page.click(&Locator::css("#far")).await.unwrap();

    let resume = scratch.path().join("resume.txt");
    fs::write(&resume, "made-up resume").unwrap();
    page.upload(&Locator::css("#resume"), &[&resume]).await.unwrap();

    let read = page
        .evaluate(
            r#"(async () => {
                const file = document.getElementById("resume").files[0];
                return {
                    name: document.getElementById("name").value,
                    why: document.getElementById("why").value,
                    message: document.getElementById("message").textContent,
                    file: [file.name, await file.text()],
                    seen: [...new Set(window.seen)],
                };
            })()"#,
        )
        .await
        .unwrap();
    assert_eq!(
        read,
        json!({
            "name": "Ada Lovelace",
            "why": "Line one\nLine two",
            "message": "Hello, \"Grace\"",
            "file": ["resume.txt", "made-up resume"],
            "seen": ["name input true", "why input true", "message input true", "send click true", "far click true"],
        })
    );

    assert_eq!(
        page.click(&Locator::css("#covered")).await,
        Err(PageError::Covered {
            element: "#covered".into()
        })
    );
    assert_eq!(
        page.fill(&Locator::css("#absent"), "text").await,
        Err(PageError::NoElement {
            element: "#absent".into()
        })
    );
    assert_eq!(
        page.upload(&Locator::css("#absent"), &[&resume]).await,
        Err(PageError::NoElement {
            element: "#absent".into()
        })
    );
    assert!(matches!(
        page.evaluate("missingFunction()").await,
        Err(PageError::Script(problem)) if problem.contains("missingFunction")
    ));

    page.detach().await.unwrap();
    assert!(matches!(
        Page::attach(bridge, &background).await.unwrap().detach().await,
        Ok(())
    ));
}

/// Fills a live Ashby form with made-up answers and prints what happened to each field.
/// It uploads a blank PDF and never submits. Set `MAVERICK_E2E_ASHBY` to a posting's address.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs MAVERICK_E2E_CHROME and MAVERICK_E2E_ASHBY, and reaches a live site"]
async fn a_live_ashby_form_is_filled_and_not_submitted() {
    let posting = std::env::var("MAVERICK_E2E_ASHBY").expect("MAVERICK_E2E_ASHBY");
    let Running {
        bridge,
        mut states,
        scratch,
        _browser,
    } = launch("data:text/html,<title>First</title>");
    let tabs = wait_for(&mut states, "the tab listed", |tabs| titled(tabs, "First").is_some()).await;

    let resume = scratch.path().join("Ada-Example-Resume.pdf");
    fs::write(&resume, BLANK_PDF).unwrap();
    let answers: Answers = serde_json::from_value(json!({
        "name": "Ada Example",
        "email": "ada@example.com",
        "phone": "+1 415 555 0100",
        "resume": resume,
        "location": "Springfield, Illinois",
        "rules": [
            { "any": ["linkedin"], "answer": "https://www.linkedin.com/in/ada-example" },
            { "any": ["start date", "when can you start"], "answer": "2027-01-18" },
            { "any": ["how did you hear"], "answer": "LinkedIn" },
            { "any": ["relocat"], "answer": "yes" },
        ],
        "facts": [
            "Lives in Springfield, Illinois, United States, in the Central time zone.",
            "Citizen of Canada. Authorized to work in the United States today.",
            "Will need employer visa sponsorship in the future.",
            "Can work in an office five days a week.",
            "Current job: Software Engineer at Example Robotics.",
        ],
        "background": [
            "Ada Example. Software Engineer at Example Robotics, 2023 to present.",
            "Built a job queue in Rust that cut nightly batch time from 6 hours to 40 minutes.",
            "Shipped a retrieval feature for support agents with reranking, judged on a held-out set of 300 tickets.",
        ],
    }))
    .unwrap();

    let tab = titled(&tabs, "First").unwrap();
    let page = Page::attach(bridge.clone(), &tab).await.unwrap();
    page.navigate(&posting).await.unwrap();
    let report = ashby::fill(&page, &answers).await.unwrap();
    assert_eq!(report.model_problem, None);
    let outcomes = report.outcomes;
    for outcome in &outcomes {
        let required = if outcome.required { "required" } else { "optional" };
        match &outcome.result {
            FieldResult::Filled { shown, by } => {
                let by = match by {
                    Source::StandardAnswer => "filled ".to_owned(),
                    Source::Model { basis } => format!("model   [{basis}]"),
                    Source::Draft { basis } => format!("draft   [{basis}]"),
                };
                println!("{by} ({required}) {}: {shown}", outcome.title)
            }
            FieldResult::Left(why) => println!("left    ({required}) {}: {why}", outcome.title),
            FieldResult::Failed(problem) => println!("FAILED  ({required}) {}: {problem}", outcome.title),
        }
    }
    let failed = outcomes
        .iter()
        .filter(|outcome| matches!(outcome.result, FieldResult::Failed(_)))
        .count();
    assert_eq!(failed, 0, "every field with an answer was entered");
    assert!(
        outcomes
            .iter()
            .any(|outcome| matches!(outcome.result, FieldResult::Filled { .. })),
        "at least one field was filled"
    );

    // The extension can look at the page without the debugger. The form is there and unsent.
    let has = |selector: &str| {
        let check = Command::HasElement {
            tab_id: tab.tab.id,
            selector: selector.to_owned(),
        };
        let bridge = bridge.clone();
        async move { bridge.request(tab.connection, &check).await.unwrap() }
    };
    assert_eq!(has(".ashby-application-form-submit-button").await, true);
    assert_eq!(has(ashby::SUBMITTED).await, false);

    // A second run finds the standard answers already in place and changes none of them.
    let again = ashby::fill(&page, &answers).await.unwrap().outcomes;
    let standard = |outcomes: &[ashby::Outcome]| -> Vec<ashby::Outcome> {
        let by_standard = |outcome: &&ashby::Outcome| {
            matches!(
                &outcome.result,
                FieldResult::Filled {
                    by: Source::StandardAnswer,
                    ..
                }
            )
        };
        outcomes.iter().filter(by_standard).cloned().collect()
    };
    assert_eq!(standard(&again), standard(&outcomes));
    assert!(
        page.evaluate("location.pathname")
            .await
            .unwrap()
            .as_str()
            .unwrap()
            .ends_with("/application"),
        "the form was not submitted"
    );
}

/// A one-page PDF with nothing on it.
const BLANK_PDF: &str = "%PDF-1.4\n1 0 obj<</Type/Catalog/Pages 2 0 R>>endobj\n\
    2 0 obj<</Type/Pages/Kids[3 0 R]/Count 1>>endobj\n\
    3 0 obj<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]>>endobj\n\
    trailer<</Root 1 0 R>>\n%%EOF\n";
