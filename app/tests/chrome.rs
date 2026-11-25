//! Drives a real Chrome with the real extension and the real app binary as the relay.
//!
//! Run with a Chrome for Testing binary, which still accepts `--load-extension`:
//! `MAVERICK_E2E_CHROME=/path/to/chrome cargo test -p maverick --test chrome -- --ignored --nocapture`

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command as Process};
use std::sync::Arc;
use std::time::Duration;

use maverick_core::answers::Answers;
use maverick_core::apply::{End, FieldResult, FillError, Outcome, Report, Source, System};
use maverick_core::ashby;
use maverick_core::bridge::{self, Bridge, BrowserState, Command, Tab};
use maverick_core::credentials::{Password, SignIn};
use maverick_core::extension::{NATIVE_HOST, native_host_manifest};
use maverick_core::greenhouse;
use maverick_core::page::{Locator, Page, PageError};
use maverick_core::workday;
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
    launch_beside(start_page, |_| Vec::new())
}

/// `launch`, in a Chrome that also runs the extensions `others` writes under the folder it is given.
fn launch_beside(start_page: &str, others: impl Fn(&Path) -> Vec<PathBuf>) -> Running {
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

    let mut extensions = vec![Path::new(env!("CARGO_MANIFEST_DIR")).join("../extension")];
    extensions.extend(others(scratch.path()));
    let extensions: Vec<String> = extensions.iter().map(|folder| folder.display().to_string()).collect();
    let browser = Browser(
        Process::new(chrome)
            .arg(format!("--user-data-dir={}", profile.display()))
            .arg(format!("--load-extension={}", extensions.join(",")))
            // A headless Chrome draws every tab. Only a Chrome with a window hides its background tabs.
            .args((!headed()).then_some("--headless=new"))
            .args([
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

/// True when `MAVERICK_E2E_HEADED` asks for a Chrome with a window, as the user runs it.
fn headed() -> bool {
    std::env::var_os("MAVERICK_E2E_HEADED").is_some()
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

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs MAVERICK_E2E_CHROME set to a Chrome for Testing binary"]
async fn several_background_tabs_are_driven_at_the_same_time() {
    let Running {
        bridge,
        mut states,
        scratch: _scratch,
        _browser,
    } = launch("data:text/html,<title>Start</title>");
    let form = serve(FORM).await;
    let tabs = wait_for(&mut states, "the start tab listed", |tabs| {
        titled(tabs, "Start").is_some()
    })
    .await;
    let connection = tabs[0].connection;

    let mut runs = Vec::new();
    for number in 0..4 {
        let (bridge, form) = (bridge.clone(), form.clone());
        runs.push(tokio::spawn(async move {
            let blank = Command::OpenTab {
                url: "about:blank".into(),
                active: false,
            };
            let opened = bridge.request(connection, &blank).await.unwrap();
            let tab = Tab {
                connection,
                tab: maverick_core::bridge::BrowserTab {
                    id: opened["id"].as_i64().unwrap() as i32,
                    window_id: opened["windowId"].as_i64().unwrap() as i32,
                    title: String::new(),
                    url: String::new(),
                    active: false,
                },
            };
            let page = Page::attach(bridge, &tab).await.unwrap();
            page.navigate(&format!("{form}?tab={number}")).await.unwrap();
            let name = format!("Ada number {number}");
            for _ in 0..5 {
                page.fill(&Locator::css("#name"), &name).await.unwrap();
                page.fill(&Locator::css("#why"), &format!("Tab {number}"))
                    .await
                    .unwrap();
                page.click(&Locator::css("#send")).await.unwrap();
            }
            let read = page
                .evaluate(
                    r#"({
                        name: document.getElementById("name").value,
                        why: document.getElementById("why").value,
                        at: location.search,
                        seen: [...new Set(window.seen)],
                    })"#,
                )
                .await
                .unwrap();
            page.detach().await.unwrap();
            (number, read)
        }));
    }
    for run in runs {
        let (number, read) = run.await.unwrap();
        assert_eq!(
            read,
            json!({
                "name": format!("Ada number {number}"),
                "why": format!("Tab {number}"),
                "at": format!("?tab={number}"),
                "seen": ["name input true", "why input true", "send click true"],
            })
        );
    }
}

/// Writes an extension that does what Bitwarden does: when a field takes the focus, it shows a
/// page of its own in a frame, as a menu next to the field. With `keeps_the_setter`, it holds on
/// to the browser's own way to point a frame at a page from the moment it starts, which no
/// script that arrives later can take away.
fn menu_extension(under: &Path, keeps_the_setter: bool) -> PathBuf {
    let folder = under.join("menu-extension");
    fs::create_dir_all(&folder).unwrap();
    let manifest = json!({
        "manifest_version": 3,
        "name": "Menu on fields",
        "version": "1.0",
        "content_scripts": [{ "matches": ["<all_urls>"], "js": ["menu.js"], "run_at": "document_start" }],
        "web_accessible_resources": [{ "resources": ["menu.html"], "matches": ["<all_urls>"] }],
    });
    fs::write(folder.join("manifest.json"), manifest.to_string()).unwrap();
    fs::write(folder.join("menu.html"), "<p>Suggestions</p>").unwrap();
    let script = r#"const keepsTheSetter = KEEPS_THE_SETTER;
        const point = Object.getOwnPropertyDescriptor(HTMLIFrameElement.prototype, "src").set;
        document.addEventListener("focusin", () => {
            const menu = document.createElement("iframe");
            const url = chrome.runtime.getURL("menu.html");
            if (keepsTheSetter) point.call(menu, url);
            else menu.setAttribute("src", url);
            document.body.append(menu);
        });"#;
    fs::write(
        folder.join("menu.js"),
        script.replace("KEEPS_THE_SETTER", &keeps_the_setter.to_string()),
    )
    .unwrap();
    folder
}

/// Opens the form in a background tab of a Chrome that also runs the menu extension.
async fn form_beside_a_menu_extension(keeps_the_setter: bool) -> (Running, Page) {
    let mut running = launch_beside("data:text/html,<title>Start</title>", |under| {
        vec![menu_extension(under, keeps_the_setter)]
    });
    let form = serve(FORM).await;
    let tabs = wait_for(&mut running.states, "the start tab listed", |tabs| {
        titled(tabs, "Start").is_some()
    })
    .await;
    let tab = background_tab(&running.bridge, tabs[0].connection).await;
    let page = Page::attach(running.bridge.clone(), &tab).await.unwrap();
    page.navigate(&form).await.unwrap();
    (running, page)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs MAVERICK_E2E_CHROME set to a Chrome for Testing binary"]
async fn a_tab_that_another_extension_draws_in_says_so() {
    let (_running, page) = form_beside_a_menu_extension(true).await;
    // No field has taken the focus yet, so the other extension has drawn nothing.
    assert_eq!(page.evaluate("document.title").await.unwrap(), "Form");

    // Typing gives a field the focus. Chrome takes the debugger away once the menu's frame
    // starts to load, which is a moment later, so the typing itself may still go through.
    let typed = page.fill(&Locator::css("#name"), "Ada Lovelace").await;
    let stopped = match typed {
        Ok(()) => timeout(Duration::from_secs(10), async {
            loop {
                if let Err(error) = page.evaluate("document.title").await {
                    return error;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("Chrome takes the debugger away"),
        Err(error) => error,
    };
    assert_eq!(stopped, PageError::OtherExtension);
    assert!(stopped.ends_the_work());
    assert!(stopped.to_string().contains("another extension"), "{stopped}");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs MAVERICK_E2E_CHROME set to a Chrome for Testing binary"]
async fn another_extensions_menu_stays_out_of_a_tab_maverick_fills() {
    let (_running, page) = form_beside_a_menu_extension(false).await;
    for (field, text) in [("#name", "Ada Lovelace"), ("#why", "Line one")] {
        page.fill(&Locator::css(field), text).await.unwrap();
        // A menu's page would cost the debugger a moment after the focus.
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let read = page
        .evaluate(
            r#"({
                name: document.getElementById("name").value,
                why: document.getElementById("why").value,
                menus: document.querySelectorAll("iframe").length > 0,
                pages: [...document.querySelectorAll("iframe")].filter((menu) => menu.hasAttribute("src")).length,
            })"#,
        )
        .await
        .unwrap();
    // The other extension still draws its frames, and none of them holds a page.
    assert_eq!(
        read,
        json!({ "name": "Ada Lovelace", "why": "Line one", "menus": true, "pages": 0 })
    );
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
    let standard = |outcomes: &[Outcome]| -> Vec<Outcome> {
        let by_standard = |outcome: &&Outcome| {
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

/// A page built to look like one page of a Workday application. Nobody signed in to Workday to
/// make it: see the comment at its top.
const WORKDAY_PAGE: &str = include_str!("workday_page.html");

/// Most of a Workday application cannot be reached without an account, so this runs the page
/// filler against `WORKDAY_PAGE`, a reconstruction. It proves the filler against the markup the
/// sources describe, not against Workday.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs MAVERICK_E2E_CHROME set to a Chrome for Testing binary"]
async fn a_reconstructed_workday_application_is_filled_page_by_page_and_never_submitted() {
    let Running {
        bridge,
        mut states,
        scratch,
        _browser,
    } = launch("data:text/html,<title>First</title>");
    let form = serve(WORKDAY_PAGE).await;
    let tabs = wait_for(&mut states, "the tab listed", |tabs| titled(tabs, "First").is_some()).await;
    let page = Page::attach(bridge, &titled(&tabs, "First").unwrap()).await.unwrap();
    page.navigate(&form).await.unwrap();

    let resume = scratch.path().join("Ada-Example-Resume.pdf");
    fs::write(&resume, BLANK_PDF).unwrap();
    let answers: Answers = serde_json::from_value(json!({
        "name": "Ada Example",
        "email": "ada@example.com",
        "phone": "+1 415 555 0100",
        "resume": resume,
        "rules": [
            { "any": ["first name"], "answer": "Ada" },
            { "any": ["state"], "answer": "illinois" },
            { "any": ["worked for"], "answer": "no" },
            { "any": ["preferred name"], "answer": "no" },
            { "any": ["which offices"], "answer": "remote" },
            { "any": ["anything else"], "answer": "Nothing to add." },
            { "any": ["when can you start"], "answer": "2027-01-18" },
            { "any": ["legally able"], "answer": "yes" },
            { "any": ["how did you hear"], "answer": "linkedin" },
        ],
        "work": [
            { "title": "Engineer", "company": "Example Robotics", "location": "Springfield", "from": "2023-06" },
            { "title": "Intern", "company": "Example Labs", "location": "Chicago", "from": "2022-01", "to": "2022-08" },
        ],
    }))
    .unwrap();
    let job = workday::Job {
        job: "Backend Engineer, Springfield".into(),
        description: "Build things.".into(),
    };
    let captures = scratch.path().join("captures");
    let report = workday::fill_page(&page, &answers, job.clone(), Some(&captures))
        .await
        .unwrap();

    let told = |outcomes: &[Outcome]| -> Vec<(String, String)> {
        let result = |outcome: &Outcome| match &outcome.result {
            FieldResult::Filled { shown, .. } => format!("filled: {shown}"),
            FieldResult::Left(why) => format!("left: {why}"),
            FieldResult::Failed(problem) => format!("FAILED: {problem}"),
        };
        outcomes
            .iter()
            .map(|outcome| (outcome.title.clone(), result(outcome)))
            .collect()
    };
    let expected: Vec<(String, String)> = [
        ("First Name", "filled: Ada".to_owned()),
        (
            "Country Phone Code",
            "left: the form already held an answer, and Maverick kept it".to_owned(),
        ),
        ("Phone Number", "filled: 415 555 0100".to_owned()),
        ("State", "filled: Illinois".to_owned()),
        ("Have you worked for Example Co before?", "filled: No".to_owned()),
        ("I have a preferred name", "filled: No".to_owned()),
        ("Which offices could you work from?", "filled: Remote".to_owned()),
        ("Anything else we should know?", "filled: Nothing to add.".to_owned()),
        ("When can you start?", "filled: 01/18/2027".to_owned()),
        ("Resume/CV", "filled: Ada-Example-Resume.pdf".to_owned()),
        // The search finds two options, and the one spelled like the answer is picked.
        ("How Did You Hear About Us?", "filled: LinkedIn".to_owned()),
        // The page showed one job. The second one was added for the user's second job.
        ("Job Title", "filled: Engineer".to_owned()),
        ("I currently work here", "filled: Yes".to_owned()),
        (
            "To",
            "left: another answer on this page makes this one unnecessary".to_owned(),
        ),
        ("Job Title", "filled: Intern".to_owned()),
        ("I currently work here", "filled: No".to_owned()),
        ("To", "filled: 08/2022".to_owned()),
        (
            "Are you legally able to work in this country?",
            "filled: Yes".to_owned(),
        ),
    ]
    .into_iter()
    .map(|(title, result)| (title.to_owned(), result))
    .collect();
    assert_eq!(told(&report.outcomes), expected);
    let required: Vec<bool> = report.outcomes.iter().map(|outcome| outcome.required).collect();
    assert_eq!(
        required,
        [
            true, true, true, true, true, false, false, false, false, true, true, true, false, true, true, false, true,
            false
        ]
    );

    // The page holds the answers, each list was opened once to read it, and nothing moved on.
    let read = page
        .evaluate(
            r#"(() => {
                const value = (id) => document.getElementById(id).value;
                const ticked = [...document.querySelectorAll("input:checked")].map((input) => input.labels[0].textContent);
                return {
                    first: value("name--legalName--firstName"),
                    phone: value("phoneNumber--phoneNumber"),
                    state: document.getElementById("address--countryRegion").textContent,
                    ticked,
                    more: value("primaryQuestionnaire--q2"),
                    month: value("primaryQuestionnaire--q3-dateSectionMonth-input"),
                    heard: document.querySelector('[data-automation-id="formField-source"] [data-automation-id="selectedItem"]').textContent.trim(),
                    title: value("workExperience-2--jobTitle"),
                    second: [value("workExperience-7--jobTitle"), value("workExperience-7--endDate-dateSectionMonth-input")],
                    file: document.querySelector('input[type="file"]').files[0].name,
                    opened: window.opened,
                    lists: document.querySelectorAll('ul[role="listbox"]:not([data-automation-id]), [data-automation-id="activeListContainer"]').length,
                    advanced: window.advanced === true,
                };
            })()"#,
        )
        .await
        .unwrap();
    assert_eq!(
        read,
        json!({
            "first": "Ada",
            "phone": "415 555 0100",
            "state": "Illinois",
            "ticked": ["No", "Remote", "I currently work here"],
            "more": "Nothing to add.",
            "month": "1",
            "heard": "LinkedIn",
            "title": "Engineer",
            "second": ["Intern", "8"],
            "file": "Ada-Example-Resume.pdf",
            "opened": ["address--countryRegion", "primaryQuestionnaire--q4", "address--countryRegion"],
            "lists": 0,
            "advanced": false,
        })
    );

    // The capture describes the page and holds nothing a field showed or the fill typed.
    let note = report.note.expect("a Workday report says where the capture is");
    let saved: Vec<_> = fs::read_dir(&captures)
        .unwrap()
        .map(|file| file.unwrap().path())
        .collect();
    assert_eq!(saved.len(), 1);
    assert!(note.contains(&saved[0].display().to_string()), "{note}");
    let capture = fs::read_to_string(&saved[0]).unwrap();
    for private in ["Prefilled Person", "(+1)", "Ada", "415", "Nothing to add"] {
        assert!(!capture.contains(private), "the capture holds {private:?}");
    }
    let capture: serde_json::Value = serde_json::from_str(&capture).unwrap();
    assert_eq!(capture["page"], "applyFlowMyInfoPage");
    assert_eq!(capture["step"], "My Information");
    assert_eq!(capture["structure"]["fields"].as_array().unwrap().len(), 18);
    assert_eq!(capture["structure"]["fields"][3]["label"], "State*");
    assert_eq!(
        capture["structure"]["fields"][3]["controls"][0],
        json!({
            "tag": "button",
            "type": "button",
            "role": null,
            "automationId": null,
            "fkitId": null,
            "id": "address--countryRegion",
            "aria": { "aria-haspopup": "listbox", "aria-label": null },
            "label": "State*",
            "options": null,
        })
    );
    assert_eq!(
        capture["read"][3]["kind"],
        r#"Choice { options: ["Illinois", "Ohio"] }"#
    );
    let footer = capture["structure"]["other"].as_array().unwrap();
    assert!(
        footer
            .iter()
            .any(|control| control["automationId"] == "pageFooterNextButton")
    );

    // A second run finds every answer in place and changes none of them. The last month of the
    // job the user holds now is no longer on the page.
    let again = workday::fill_page(&page, &answers, job.clone(), None).await.unwrap();
    let mut in_place = expected.clone();
    in_place.remove(13);
    assert_eq!(told(&again.outcomes), in_place);
    assert_eq!(page.evaluate("window.advanced === true").await.unwrap(), false);

    // The whole fill goes on from there. Workday does not take this page, so the fill stops on it
    // and says what Workday said.
    page.navigate(&format!("{form}?refuse")).await.unwrap();
    let refused = workday::fill_application(&page, &answers, job.clone(), None, &NO_SIGN_IN)
        .await
        .unwrap();
    let End::Stopped(why) = &refused.end else {
        panic!("the fill ended with {:?}", refused.end);
    };
    assert!(
        why.contains(
            "Workday did not take \"My Information\": The field Postal Code is required and must have a value."
        ),
        "{why}"
    );
    assert_eq!(told(&refused.outcomes), expected);
    assert_eq!(page.evaluate("window.advanced === true").await.unwrap(), false);

    // Workday takes this page. The next page is the last one, and the fill presses nothing on it.
    page.navigate(&form).await.unwrap();
    let whole = workday::fill_application(&page, &answers, job, None, &NO_SIGN_IN)
        .await
        .unwrap();
    assert_eq!(whole.end, End::Review);
    assert_eq!(told(&whole.outcomes), expected);
    let note = whole.note.unwrap();
    assert!(note.contains("It filled: My Information."), "{note}");
    assert!(note.contains("click Submit yourself"), "{note}");
    assert_eq!(
        page.evaluate("[window.advanced === true, window.submitted === true]")
            .await
            .unwrap(),
        json!([true, false])
    );
}

/// Proves the part of Workday that needs no account, on a live posting: the job is read, the
/// application is opened by way of "Apply Manually", and the sign-in wall is reported.
/// It is given no sign-in and never reads the Keychain, so it types nothing, creates no account
/// and never signs in. Set `MAVERICK_E2E_WORKDAY` to a posting's address.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs MAVERICK_E2E_CHROME and MAVERICK_E2E_WORKDAY, and reaches a live site"]
async fn a_live_workday_posting_is_read_and_stops_at_the_sign_in_wall() {
    let posting = std::env::var("MAVERICK_E2E_WORKDAY").expect("MAVERICK_E2E_WORKDAY");
    let Running {
        bridge,
        mut states,
        scratch: _scratch,
        _browser,
    } = launch("data:text/html,<title>First</title>");
    let tabs = wait_for(&mut states, "the tab listed", |tabs| titled(tabs, "First").is_some()).await;
    let answers: Answers = serde_json::from_value(json!({
        "name": "Ada Example",
        "email": "ada@example.com",
        "phone": "+1 415 555 0100",
        "resume": "/nowhere/Ada-Example-Resume.pdf",
    }))
    .unwrap();

    let page = Page::attach(bridge, &titled(&tabs, "First").unwrap()).await.unwrap();
    page.navigate(&posting).await.unwrap();
    let job = workday::read_job(&page).await.unwrap();
    println!("job: {}", job.job);
    println!("description: {} characters", job.description.len());
    assert!(!job.job.is_empty() && !job.description.is_empty());

    let wall = Err(FillError::SignInNeeded {
        system: "Workday",
        service: workday::SIGN_IN_SERVICE,
    });
    assert_eq!(workday::fill_with(&page, &answers, None, &NO_SIGN_IN).await, wall);
    // The tab is inside the application now, where the user signs in. A second run stops there too.
    assert_eq!(workday::fill_with(&page, &answers, None, &NO_SIGN_IN).await, wall);
    let seen = page
        .evaluate(
            r#"({
                path: location.pathname,
                wall: document.querySelector('[data-automation-id="signInContent"]') !== null,
                typed: [...document.querySelectorAll('input[type="text"], input[type="email"], input[type="password"]')]
                    .map((input) => input.value)
                    .join(""),
            })"#,
        )
        .await
        .unwrap();
    println!("the wall is at {}", seen["path"]);
    assert!(seen["path"].as_str().unwrap().ends_with("/apply/applyManually"));
    assert_eq!(seen["wall"], true);
    assert_eq!(seen["typed"], "", "nothing was typed into the sign-in form");
    let address = page.evaluate("location.href").await.unwrap();
    assert_eq!(
        workday::posting_address(address.as_str().unwrap()),
        workday::posting_address(&posting),
        "the wall's address still names the posting"
    );
}

/// No stored sign-in. A fill given this never reads the Keychain and stops at a sign-in wall.
const NO_SIGN_IN: Option<SignIn> = None;

/// A made-up password with a quote, a backslash, a space and a character outside ASCII.
const MADE_UP_PASSWORD: &str = "Made-up 1 \"pass\\word\" é";

/// A made-up sign-in, handed to a fill in place of the Keychain.
fn made_up_sign_in() -> Option<SignIn> {
    Some(SignIn {
        account: "ada@example.com".into(),
        password: Password::new(MADE_UP_PASSWORD),
    })
}

/// A page built to look like Workday's sign-in wall, with a script that plays Workday's part
/// after a press. Nobody signed in to Workday or created an account to make it: see the comment
/// at its top.
const WORKDAY_WALL: &str = include_str!("workday_wall.html");

/// Opens the reconstructed wall, with what it should do in `outcome`, and runs a fill against it.
/// Returns the fill's result and what the page saw: each press of a submit button, in order.
async fn fill_behind_the_reconstructed_wall(
    page: &Page,
    wall: &str,
    outcome: &str,
    sign_in: &Option<SignIn>,
) -> (Result<Report, FillError>, serde_json::Value) {
    page.navigate(&format!("{wall}?{outcome}")).await.unwrap();
    let answers: Answers = serde_json::from_value(json!({
        "name": "Ada Example",
        "email": "ada@example.com",
        "phone": "+1 415 555 0100",
        "resume": "/nowhere/Ada-Example-Resume.pdf",
        "rules": [{ "any": ["first name"], "answer": "Ada" }],
    }))
    .unwrap();
    let job = workday::Job {
        job: "Backend Engineer, Springfield".into(),
        description: "Build things.".into(),
    };
    let result = workday::fill_application(page, &answers, job, None, sign_in).await;
    let seen = page
        .evaluate(
            r#"({
                presses: JSON.parse(sessionStorage.presses ?? "[]"),
                path: location.pathname,
                hidden: document.querySelector('[data-automation-id="beecatcher"]').value,
                typed: [...document.querySelectorAll('[data-automation-id="signInContent"] input')].map((input) => input.value).join(""),
                first: window.saved ?? document.getElementById("name--legalName--firstName")?.value ?? null,
                advanced: window.advanced === true,
                submitted: window.submitted === true,
            })"#,
        )
        .await
        .unwrap();
    assert_eq!(seen["hidden"], "", "nothing was typed into the field for robots");
    // A fill that got past the wall goes through the application to its last page.
    assert_eq!(
        seen["advanced"],
        result.is_ok(),
        "the application moved on only after a fill"
    );
    assert_eq!(seen["submitted"], false, "nothing was submitted");
    (result, seen)
}

/// What the reconstructed wall records for a press that reached its made-up server.
fn sent(form: &str, extra: serde_json::Value) -> serde_json::Value {
    let mut press = json!({ "form": form, "email": "ada@example.com", "password": MADE_UP_PASSWORD, "sent": true });
    press
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    press
}

/// Runs the whole sign-in and create-account flow against `WORKDAY_WALL`, a RECONSTRUCTION, with
/// a made-up sign-in in place of the Keychain. Nothing after a press of Sign In or Create Account
/// was ever seen on a live Workday site, so this proves the flow against what Workday's public
/// script says the wall does, not against Workday.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs MAVERICK_E2E_CHROME set to a Chrome for Testing binary"]
async fn a_reconstructed_workday_wall_is_passed_by_signing_in_or_by_creating_the_account() {
    let Running {
        bridge,
        mut states,
        scratch: _scratch,
        _browser,
    } = launch("data:text/html,<title>First</title>");
    let wall = serve(WORKDAY_WALL).await;
    let tabs = wait_for(&mut states, "the tab listed", |tabs| titled(tabs, "First").is_some()).await;
    let page = Page::attach(bridge, &titled(&tabs, "First").unwrap()).await.unwrap();
    let sign_in = made_up_sign_in();

    // The wall opens on the create-account form, as most live walls do. The sign-in goes through.
    let (result, seen) = fill_behind_the_reconstructed_wall(&page, &wall, "signin=ok", &sign_in).await;
    let report = result.unwrap();
    let note = report.note.unwrap();
    assert!(
        note.starts_with("Signed in to Workday with the sign-in in your Keychain. "),
        "{note}"
    );
    assert!(note.contains("Check every answer"), "{note}");
    assert_eq!(seen["presses"], json!([sent("sign-in", json!({}))]));
    assert_eq!(seen["first"], "Ada", "the application page behind the wall was filled");

    // The wall opens on the chooser. Workday refuses the sign-in, so the account is created, with
    // the consent box ticked. A notice that shows for a moment is not taken for a stop.
    let outcome = "start=chooser&consent=1&signin=wrong&create=greets";
    let (result, seen) = fill_behind_the_reconstructed_wall(&page, &wall, outcome, &sign_in).await;
    let note = result.unwrap().note.unwrap();
    assert!(
        note.starts_with(
            "Created your Workday account at this employer with the sign-in in your Keychain, \
             and ticked the consent box on its form. "
        ),
        "{note}"
    );
    let created = json!({ "verify": MADE_UP_PASSWORD, "consent": true });
    assert_eq!(
        seen["presses"],
        json!([sent("sign-in", json!({})), sent("create", created)])
    );
    assert_eq!(seen["first"], "Ada");

    // A form without a consent box.
    let (result, seen) = fill_behind_the_reconstructed_wall(&page, &wall, "signin=wrong&create=ok", &sign_in).await;
    let note = result.unwrap().note.unwrap();
    assert!(note.starts_with("Created your Workday account at this employer with the sign-in in your Keychain. "));
    assert_eq!(
        seen["presses"][1],
        sent("create", json!({ "verify": MADE_UP_PASSWORD, "consent": null }))
    );
}

/// The stops of the flow, against `WORKDAY_WALL`, the same RECONSTRUCTION: no stored sign-in, an
/// email address to verify, and refusals. Each fill presses Sign In at most once and Create
/// Account at most once, and no error holds the password.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs MAVERICK_E2E_CHROME set to a Chrome for Testing binary"]
async fn a_reconstructed_workday_wall_stops_the_fill_after_one_try_of_each_kind() {
    let Running {
        bridge,
        mut states,
        scratch: _scratch,
        _browser,
    } = launch("data:text/html,<title>First</title>");
    let wall = serve(WORKDAY_WALL).await;
    let tabs = wait_for(&mut states, "the tab listed", |tabs| titled(tabs, "First").is_some()).await;
    let page = Page::attach(bridge, &titled(&tabs, "First").unwrap()).await.unwrap();
    let sign_in = made_up_sign_in();
    let forms = |seen: &serde_json::Value| -> Vec<(String, bool)> {
        let press = |press: &serde_json::Value| (press["form"].as_str().unwrap().to_owned(), press["sent"] == true);
        seen["presses"].as_array().unwrap().iter().map(press).collect()
    };
    let (signed_in, created) = (("sign-in".to_owned(), true), ("create".to_owned(), true));
    let stopped = |result: Result<Report, FillError>| -> String {
        let error = result.expect_err("the fill stopped at the wall");
        let told = format!("{error}");
        assert!(matches!(error, FillError::SignIn(_)), "{error:?}");
        assert!(
            !format!("{error:?} {told}").contains("Made-up 1"),
            "an error holds the password: {told}"
        );
        told
    };

    // With no stored sign-in the fill stops as it always did, types nothing, and says how to store one.
    let (result, seen) = fill_behind_the_reconstructed_wall(&page, &wall, "signin=ok", &NO_SIGN_IN).await;
    let needed = result.unwrap_err();
    assert_eq!(
        needed,
        FillError::SignInNeeded {
            system: "Workday",
            service: "dev.maverick.workday"
        }
    );
    let told = needed.to_string();
    assert!(
        told.contains("`security add-generic-password -s dev.maverick.workday -a <email> -w`"),
        "{told}"
    );
    assert!(told.contains("type the password at the prompt"), "{told}");
    assert_eq!((forms(&seen), &seen["typed"]), (vec![], &json!("")));

    // Workday wants the new account's email address verified, and moves to its sign-in page.
    let (result, seen) = fill_behind_the_reconstructed_wall(&page, &wall, "signin=wrong&create=verify", &sign_in).await;
    let told = stopped(result);
    assert!(told.contains("asked it to create your account"), "{told}");
    assert!(told.contains("We sent an email to verify your account."), "{told}");
    assert!(
        told.contains("open the email from Workday and verify. Then click Fill form again."),
        "{told}"
    );
    assert_eq!(forms(&seen), [signed_in.clone(), created.clone()]);
    assert_eq!(seen["path"], "/login");

    // The account exists and its email address is unverified. No account is created.
    let (result, seen) = fill_behind_the_reconstructed_wall(&page, &wall, "signin=unverified", &sign_in).await;
    let told = stopped(result);
    assert!(told.contains("may need its email address verified"), "{told}");
    assert!(told.contains("your account may need verification"), "{told}");
    assert_eq!(forms(&seen), std::slice::from_ref(&signed_in));

    // Both are refused: a wrong password for an account that exists. Each was tried once.
    let (result, seen) = fill_behind_the_reconstructed_wall(&page, &wall, "signin=wrong&create=exists", &sign_in).await;
    let told = stopped(result);
    assert!(told.contains("Wrong email address or password."), "{told}");
    assert!(
        told.contains("An account with this email address exists already."),
        "{told}"
    );
    assert!(told.contains("tried each once"), "{told}");
    assert_eq!(forms(&seen), [signed_in.clone(), created.clone()]);

    // The employer's rules refuse the password before anything is sent.
    let outcome = "consent=1&signin=wrong&create=weak";
    let (result, seen) = fill_behind_the_reconstructed_wall(&page, &wall, outcome, &sign_in).await;
    let told = stopped(result);
    assert!(told.contains("The password must hold a made-up thing."), "{told}");
    assert_eq!(forms(&seen), [signed_in.clone(), ("create".to_owned(), false)]);

    // The sign-in form itself refuses what was typed. That says nothing about an account, so none is created.
    let (result, seen) = fill_behind_the_reconstructed_wall(&page, &wall, "signin=invalid", &sign_in).await;
    let told = stopped(result);
    assert!(
        told.contains("Enter a valid email address.") && told.contains("created no account"),
        "{told}"
    );
    assert_eq!(forms(&seen), [("sign-in".to_owned(), false)]);

    // Should Workday ever repeat the password in its own message, the message is dropped.
    let (result, seen) = fill_behind_the_reconstructed_wall(&page, &wall, "signin=echo&create=exists", &sign_in).await;
    let told = stopped(result);
    assert!(told.contains("held the password, so it is left out"), "{told}");
    assert_eq!(forms(&seen), [signed_in, created]);

    // A form Maverick does not know gets no typing and no press.
    let (result, seen) = fill_behind_the_reconstructed_wall(&page, &wall, "start=other", &sign_in).await;
    let told = stopped(result);
    assert!(told.contains("a sign-in page that Maverick does not know"), "{told}");
    assert_eq!((forms(&seen), &seen["typed"]), (vec![], &json!("")));
}

/// Proves, on a live posting, every step of the sign-in flow that comes BEFORE a press: the wall's
/// forms are brought up, a MADE-UP email and password are typed into them and read back, the
/// consent box is ticked, and a press would land on Workday's click filter.
/// It presses neither Sign In nor Create Account, so it tries no sign-in and creates no account,
/// and it never reads the Keychain. Set `MAVERICK_E2E_WORKDAY` to a posting's address.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs MAVERICK_E2E_CHROME and MAVERICK_E2E_WORKDAY, and reaches a live site"]
async fn a_live_workday_wall_takes_a_made_up_sign_in_and_nothing_is_pressed() {
    let posting = std::env::var("MAVERICK_E2E_WORKDAY").expect("MAVERICK_E2E_WORKDAY");
    let Running {
        bridge,
        mut states,
        scratch: _scratch,
        _browser,
    } = launch("data:text/html,<title>First</title>");
    let tabs = wait_for(&mut states, "the tab listed", |tabs| titled(tabs, "First").is_some()).await;
    let answers: Answers = serde_json::from_value(json!({
        "name": "Ada Example",
        "email": "ada@example.com",
        "phone": "+1 415 555 0100",
        "resume": "/nowhere/Ada-Example-Resume.pdf",
    }))
    .unwrap();
    let page = Page::attach(bridge, &titled(&tabs, "First").unwrap()).await.unwrap();
    page.navigate(&posting).await.unwrap();
    let wall = workday::fill_with(&page, &answers, None, &NO_SIGN_IN).await;
    assert!(matches!(wall, Err(FillError::SignInNeeded { .. })), "{wall:?}");

    let rehearsal = workday::rehearse_wall(&page, &made_up_sign_in().unwrap())
        .await
        .unwrap();
    println!("consent box: {}", rehearsal.consented);
    assert_eq!(
        (rehearsal.create_press.as_str(), rehearsal.sign_in_press.as_str()),
        ("click_filter", "click_filter")
    );
    let seen = page
        .evaluate(
            r#"({
                path: location.pathname,
                title: document.querySelector('[data-automation-id="signInContent"] #authViewTitle')?.textContent,
                typed: [...document.querySelectorAll('[data-automation-id="signInContent"] input')].map((input) => input.value),
                hidden: document.querySelector('[data-automation-id="beecatcher"]')?.value ?? null,
                said: document.querySelectorAll('[data-automation-id="errorMessage"], [data-automation-id="alertMessage"], [data-automation-id="inputAlert"]').length,
            })"#,
        )
        .await
        .unwrap();
    println!("the wall has a field for robots: {}", !seen["hidden"].is_null());
    assert!(
        seen["path"].as_str().unwrap().ends_with("/apply/applyManually"),
        "the wall was not left"
    );
    assert_eq!(seen["title"], "Sign In");
    assert_eq!(seen["typed"], json!(["ada@example.com", MADE_UP_PASSWORD]));
    assert!(
        seen["hidden"].is_null() || seen["hidden"] == "",
        "nothing was typed into the field for robots"
    );
    assert_eq!(seen["said"], 0, "nothing was sent, so Workday said nothing");
}

/// A few employers show a Workday application without an account. This fills the first page of
/// one with made-up answers and prints what happened to each field. It uploads nothing, never
/// clicks "Save and Continue" and never submits, so the employer receives no application.
/// Set `MAVERICK_E2E_WORKDAY_OPEN` to such a posting's address.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs MAVERICK_E2E_CHROME and MAVERICK_E2E_WORKDAY_OPEN, and reaches a live site"]
async fn a_live_workday_page_that_needs_no_account_is_filled_and_not_left() {
    let posting = std::env::var("MAVERICK_E2E_WORKDAY_OPEN").expect("MAVERICK_E2E_WORKDAY_OPEN");
    let Running {
        bridge,
        mut states,
        scratch,
        _browser,
    } = launch("data:text/html,<title>First</title>");
    let tabs = wait_for(&mut states, "the tab listed", |tabs| titled(tabs, "First").is_some()).await;
    let answers: Answers = serde_json::from_value(json!({
        "name": "Ada Example",
        "email": "ada@example.com",
        "phone": "+1 415 555 0100",
        "resume": "/nowhere/Ada-Example-Resume.pdf",
        "rules": [
            { "any": ["first name"], "answer": "Ada" },
            { "any": ["last name"], "answer": "Example" },
            { "any": ["address line 1"], "answer": "1 Example Street" },
            { "any": ["city"], "answer": "Springfield" },
            { "any": ["postal code"], "answer": "62701" },
            { "any": ["state"], "answer": "Illinois" },
            { "any": ["previously employed", "previously worked", "worked for"], "answer": "no" },
            { "any": ["preferred name"], "answer": "no" },
        ],
    }))
    .unwrap();

    let page = Page::attach(bridge, &titled(&tabs, "First").unwrap()).await.unwrap();
    page.navigate(&posting).await.unwrap();
    let captures = scratch.path().join("captures");
    let fill = || workday::fill_with(&page, &answers, Some(&captures), &NO_SIGN_IN);
    let outcomes = fill().await.unwrap().outcomes;
    for outcome in &outcomes {
        let required = if outcome.required { "required" } else { "optional" };
        match &outcome.result {
            FieldResult::Filled { shown, .. } => println!("filled  ({required}) {}: {shown}", outcome.title),
            FieldResult::Left(why) => println!("left    ({required}) {}: {why}", outcome.title),
            FieldResult::Failed(problem) => println!("FAILED  ({required}) {}: {problem}", outcome.title),
        }
    }
    let failed = outcomes
        .iter()
        .filter(|outcome| matches!(outcome.result, FieldResult::Failed(_)))
        .count();
    assert_eq!(failed, 0, "every field with an answer was entered");
    let filled = |outcomes: &[Outcome]| -> Vec<Outcome> {
        let entered = |outcome: &&Outcome| matches!(outcome.result, FieldResult::Filled { .. });
        outcomes.iter().filter(entered).cloned().collect()
    };
    assert!(
        filled(&outcomes).len() >= 3,
        "the name, the email and the phone number went in"
    );

    // A second run finds the answers in place and changes none of them.
    assert_eq!(filled(&fill().await.unwrap().outcomes), filled(&outcomes));
    let seen = page
        .evaluate(
            r#"({
                path: location.pathname,
                open: document.querySelectorAll('button[aria-expanded="true"]').length,
                errors: document.querySelectorAll('[data-automation-id="errorHeading"]').length,
            })"#,
        )
        .await
        .unwrap();
    assert!(
        seen["path"].as_str().unwrap().ends_with("/apply/applyManually"),
        "the page was not left"
    );
    assert_eq!(seen["open"], 0, "no list was left open");
    assert_eq!(seen["errors"], 0, "the page was not sent for checking");
    // The second capture was taken with the answers on the page, and holds none of them.
    for file in fs::read_dir(&captures).unwrap() {
        let capture = fs::read_to_string(file.unwrap().path()).unwrap();
        for private in ["Ada", "ada@example.com", "555", "1 Example Street", "62701"] {
            assert!(!capture.contains(private), "a capture holds {private:?}");
        }
    }
}

/// Fills a live Greenhouse form with made-up answers and prints what happened to each field.
/// It uploads a blank PDF and never submits. Set `MAVERICK_E2E_GREENHOUSE` to a posting's address.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs MAVERICK_E2E_CHROME and MAVERICK_E2E_GREENHOUSE, and reaches a live site"]
async fn a_live_greenhouse_form_is_filled_and_not_submitted() {
    let posting = std::env::var("MAVERICK_E2E_GREENHOUSE").expect("MAVERICK_E2E_GREENHOUSE");
    let system = System::of(&posting).expect("MAVERICK_E2E_GREENHOUSE holds a posting's address");
    assert_eq!(system, System::Greenhouse);
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

    // The app fills an approved posting in a tab it opens in the background.
    let tab = background_tab(&bridge, tabs[0].connection).await;
    let page = Page::attach(bridge.clone(), &tab).await.unwrap();
    // The app opens the form's address, whatever shape the posting's link had.
    page.navigate(&system.form_address(&posting).unwrap()).await.unwrap();
    let report = system.fill(&page, &answers).await.unwrap();
    assert_eq!(report.model_problem, None);
    let outcomes = report.outcomes;
    print_outcomes(&outcomes);
    let failed = outcomes
        .iter()
        .filter(|outcome| matches!(outcome.result, FieldResult::Failed(_)))
        .count();
    assert_eq!(failed, 0, "every field with an answer was entered");
    let filled_by_standard = |title: &str| {
        outcomes.iter().any(|outcome| {
            let by_standard = matches!(
                &outcome.result,
                FieldResult::Filled {
                    by: Source::StandardAnswer,
                    ..
                }
            );
            by_standard && outcome.title == title
        })
    };
    assert!(
        filled_by_standard("First Name") && filled_by_standard("Last Name") && filled_by_standard("Email"),
        "the fields every Greenhouse form has were filled"
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
    assert_eq!(has("#application-form .application--submit button").await, true);
    assert_eq!(has(system.submitted()).await, false);

    // A second run finds the standard answers already in place and changes none of them.
    let again = system.fill(&page, &answers).await.unwrap().outcomes;
    let standard = |outcomes: &[Outcome]| -> Vec<Outcome> {
        let by_standard = |outcome: &&Outcome| {
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
    // A list refuses an option it already holds, so a second pick of one would show up here.
    let failed_again: Vec<&Outcome> = again
        .iter()
        .filter(|outcome| matches!(outcome.result, FieldResult::Failed(_)))
        .collect();
    assert_eq!(failed_again, Vec::<&Outcome>::new());
    assert_eq!(
        greenhouse::form_address(page.evaluate("location.href").await.unwrap().as_str().unwrap()),
        system.form_address(&posting),
        "the tab is still on the form"
    );
    assert_eq!(has(system.submitted()).await, false, "the form was not submitted");
}

/// Opens an empty tab behind the one on screen, as the app does for an approved posting.
async fn background_tab(bridge: &Bridge, connection: bridge::ConnectionId) -> Tab {
    let blank = Command::OpenTab {
        url: "about:blank".into(),
        active: false,
    };
    let opened = bridge.request(connection, &blank).await.unwrap();
    let number = |key: &str| i32::try_from(opened[key].as_i64().unwrap()).unwrap();
    Tab {
        connection,
        tab: bridge::BrowserTab {
            id: number("id"),
            window_id: number("windowId"),
            title: String::new(),
            url: String::new(),
            active: false,
        },
    }
}

/// Prints what a fill did to each field.
fn print_outcomes(outcomes: &[Outcome]) {
    for outcome in outcomes {
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
}

/// A one-page PDF with nothing on it.
const BLANK_PDF: &str = "%PDF-1.4\n1 0 obj<</Type/Catalog/Pages 2 0 R>>endobj\n\
    2 0 obj<</Type/Pages/Kids[3 0 R]/Count 1>>endobj\n\
    3 0 obj<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]>>endobj\n\
    trailer<</Root 1 0 R>>\n%%EOF\n";
