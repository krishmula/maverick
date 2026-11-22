//! The main window: a status header over either the tab list or the steps to connect Chrome.

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::list::{List, ListEvent, ListState};
use gpui_kit::component::{ActiveTheme as _, IndexPath, Sizable as _, Theme, h_flex, v_flex};
use gpui_kit::{
    AnyElement, AppContext as _, Context, Entity, FontWeight, InteractiveElement as _, IntoElement, ParentElement as _,
    Render, SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, Window, div,
};
use maverick_core::apply::{FieldResult, Outcome, Source};
use maverick_core::bridge::BrowserState;

use crate::browser::{Browser, Fill, Latency};
use crate::jobs::{Job, JobList};
use crate::tabs::TabList;

/// Which list the window shows.
#[derive(Clone, Copy, PartialEq)]
enum Showing {
    Tabs,
    Jobs,
}

pub struct MainView {
    browser: Entity<Browser>,
    showing: Showing,
    jobs: Entity<ListState<JobList>>,
    tabs: Entity<ListState<TabList>>,
    _subscriptions: Vec<Subscription>,
}

impl MainView {
    pub fn new(browser: Entity<Browser>, jobs: Vec<Job>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let jobs = cx.new(|cx| ListState::new(JobList::new(jobs), window, cx).searchable(true));
        let tabs = cx.new(|cx| ListState::new(TabList::new(browser.clone()), window, cx).searchable(true));
        let _subscriptions = vec![
            cx.observe_in(&browser, window, |this, browser, window, cx| {
                this.show_current_tabs(window, cx);
                // A job leaves the list once its application went through.
                let applied: Vec<String> = browser
                    .read(cx)
                    .applied()
                    .iter()
                    .map(|sent| sent.address.clone())
                    .collect();
                this.jobs.update(cx, |jobs, cx| {
                    for address in &applied {
                        jobs.delegate_mut().remove(address);
                    }
                    cx.notify();
                });
                cx.notify();
            }),
            cx.subscribe_in(&tabs, window, |this, tabs, event: &ListEvent, window, cx| {
                if let ListEvent::Confirm(ix) = event
                    && let Some(tab) = tabs.read(cx).delegate().tab_at(*ix).cloned()
                {
                    this.browser.update(cx, |browser, cx| browser.activate(&tab, cx));
                    // A click moves focus to the row. Typing should always search.
                    tabs.update(cx, |tabs, cx| tabs.focus(window, cx));
                }
            }),
            cx.subscribe_in(&jobs, window, |this, jobs, event: &ListEvent, window, cx| {
                if let ListEvent::Confirm(ix) = event
                    && let Some(job) = jobs.read(cx).delegate().job_at(*ix).cloned()
                {
                    let title = format!("{} at {}", job.title, job.company);
                    this.browser
                        .update(cx, |browser, cx| browser.apply(title, job.address.clone(), cx));
                    jobs.update(cx, |jobs, cx| {
                        jobs.delegate_mut().mark_opened(&job.address);
                        jobs.focus(window, cx);
                        cx.notify();
                    });
                }
            }),
            cx.observe_window_appearance(window, |_, window, cx| {
                Theme::sync_system_appearance(Some(window), cx);
            }),
        ];
        let mut view = Self {
            browser,
            showing: Showing::Tabs,
            jobs,
            tabs,
            _subscriptions,
        };
        view.show_current_tabs(window, cx);
        view.tabs.update(cx, |tabs, cx| tabs.focus(window, cx));
        view
    }

    /// Copies the browser's tabs into the list and keeps a row selected, so Enter always has a target.
    fn show_current_tabs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let tabs = match self.browser.read(cx).state() {
            BrowserState::Connected { tabs } => tabs.clone(),
            BrowserState::Disconnected => Vec::new(),
        };
        self.tabs.update(cx, |list, cx| {
            list.delegate_mut().set_tabs(tabs);
            let count = list.delegate().len();
            let selected = match list.selected_index() {
                _ if count == 0 => None,
                Some(ix) if ix.row < count => Some(ix),
                _ => Some(IndexPath::default()),
            };
            list.set_selected_index(selected, window, cx);
            cx.notify();
        });
    }

    fn status(&self, cx: &mut Context<Self>) -> AnyElement {
        let browser = self.browser.read(cx);
        let BrowserState::Connected { tabs } = browser.state() else {
            return div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child("Chrome not connected")
                .into_any_element();
        };
        let count = match tabs.len() {
            1 => "1 tab".to_owned(),
            count => format!("{count} tabs"),
        };
        let latency = match browser.latency() {
            Latency::Measuring => "measuring".to_owned(),
            Latency::Measured(took) => format!("{:.1} ms", took.as_secs_f64() * 1000.0),
            Latency::Failed(reason) => reason.clone(),
        };
        Button::new("measure-latency")
            .ghost()
            .xsmall()
            .label(SharedString::from(format!("Chrome connected · {count} · {latency}")))
            .tooltip("Round trip to Chrome and back. Click to measure again.")
            .on_click(cx.listener(|this, _, _, cx| {
                this.browser.update(cx, |browser, cx| browser.measure(cx));
            }))
            .into_any_element()
    }

    fn show(&mut self, showing: Showing, window: &mut Window, cx: &mut Context<Self>) {
        self.showing = showing;
        match showing {
            Showing::Tabs => self.tabs.update(cx, |tabs, cx| tabs.focus(window, cx)),
            Showing::Jobs => self.jobs.update(cx, |jobs, cx| jobs.focus(window, cx)),
        }
        cx.notify();
    }

    /// The two buttons that switch between the lists.
    fn switcher(&self, cx: &mut Context<Self>) -> AnyElement {
        let jobs = self.jobs.read(cx).delegate().len();
        let showing_now = self.showing;
        let button = |id: &'static str, label: String, showing: Showing, cx: &mut Context<Self>| {
            let button = Button::new(id).xsmall().label(SharedString::from(label));
            let button = if showing_now == showing { button } else { button.ghost() };
            button.on_click(cx.listener(move |this, _, window, cx| this.show(showing, window, cx)))
        };
        h_flex()
            .gap_1()
            .child(button("show-tabs", "Tabs".to_owned(), Showing::Tabs, cx))
            .child(button("show-jobs", format!("Jobs · {jobs}"), Showing::Jobs, cx))
            .into_any_element()
    }

    /// What the form filler is doing or did, shown until the user closes it.
    fn fill_report(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let muted = cx.theme().muted_foreground;
        let (heading, lines) = match self.browser.read(cx).fill() {
            Fill::Idle => return None,
            Fill::Running { title } => (format!("Filling {title}"), Vec::new()),
            Fill::Failed { title, reason } => (
                format!("Could not fill {title}"),
                vec![
                    div()
                        .text_color(cx.theme().danger)
                        .child(SharedString::from(reason.clone())),
                ],
            ),
            Fill::Done { title, report } => {
                let outcomes = &report.outcomes;
                let filled = outcomes
                    .iter()
                    .filter(|outcome| matches!(outcome.result, FieldResult::Filled { .. }))
                    .count();
                let line = |outcome: &Outcome, why: String| {
                    let required = if outcome.required { " (required)" } else { "" };
                    div().child(SharedString::from(format!("{}{required}: {why}", outcome.title)))
                };
                let mut lines = Vec::new();
                if let Some(problem) = &report.model_problem {
                    lines.push(
                        div()
                            .text_color(cx.theme().danger)
                            .child(SharedString::from(format!("The model was not asked: {problem}"))),
                    );
                }
                for outcome in outcomes {
                    if let FieldResult::Failed(problem) = &outcome.result {
                        lines.push(line(outcome, problem.clone()).text_color(cx.theme().danger));
                    }
                }
                for outcome in outcomes {
                    if let FieldResult::Left(why) = &outcome.result {
                        lines.push(line(outcome, format!("left for you, {why}")).text_color(muted));
                    }
                }
                // The model's answers are listed so the user can check them before submitting.
                for outcome in outcomes {
                    if let FieldResult::Filled {
                        shown,
                        by: Source::Model { basis },
                    } = &outcome.result
                    {
                        lines.push(line(outcome, format!("the model answered {shown:?}. {basis}")));
                    }
                    if let FieldResult::Filled {
                        by: Source::Draft { basis },
                        ..
                    } = &outcome.result
                    {
                        let note = format!("the model drafted this. Read it before you submit. {basis}");
                        lines.push(line(outcome, note));
                    }
                }
                (
                    format!(
                        "Filled {filled} of {} fields in {title}. Nothing was submitted.",
                        outcomes.len()
                    ),
                    lines,
                )
            }
        };
        let running = matches!(self.browser.read(cx).fill(), Fill::Running { .. });
        Some(
            v_flex()
                .id("fill-report")
                .px_3()
                .py_2()
                .gap_1()
                .max_h_64()
                .overflow_y_scroll()
                .border_b_1()
                .border_color(cx.theme().border)
                .child(
                    h_flex()
                        .justify_between()
                        .gap_3()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(SharedString::from(heading)),
                        )
                        .children((!running).then(|| {
                            Button::new("close-fill-report")
                                .ghost()
                                .xsmall()
                                .label("Close")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.browser.update(cx, |browser, cx| browser.dismiss_fill(cx));
                                }))
                        })),
                )
                .children(lines)
                .into_any_element(),
        )
    }

    fn connect_guide(&self, cx: &mut Context<Self>) -> AnyElement {
        let guide = v_flex().id("connect-guide").max_w_128().mx_auto().p_8().gap_3().child(
            div()
                .text_xl()
                .font_weight(FontWeight::SEMIBOLD)
                .child("Connect Chrome"),
        );
        match self.browser.read(cx).extension_dir() {
            Err(reason) => guide
                .child(
                    div()
                        .text_color(cx.theme().danger)
                        .child(SharedString::from(reason.to_owned())),
                )
                .into_any_element(),
            Ok(folder) => {
                let folder = folder.to_owned();
                guide
                    .child("1. Open chrome://extensions in Chrome and turn on Developer mode.")
                    .child("2. Show the extension folder, then drag it onto that page.")
                    .child(
                        h_flex()
                            .gap_3()
                            .p_2()
                            .rounded_lg()
                            .border_1()
                            .border_color(cx.theme().border)
                            .bg(cx.theme().muted)
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_xs()
                                    .child(SharedString::from(folder.display().to_string())),
                            )
                            .child(
                                Button::new("show-extension-folder")
                                    .small()
                                    .label("Show folder")
                                    .on_click(move |_, _, cx| cx.reveal_path(&folder)),
                            ),
                    )
                    .child(div().text_color(cx.theme().muted_foreground).child(
                        "This screen changes to your tabs as soon as Chrome connects. \
                         If the extension is already loaded, Chrome reconnects within 30 seconds.",
                    ))
                    .into_any_element()
            }
        }
    }
}

impl Render for MainView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let connected = matches!(self.browser.read(cx).state(), BrowserState::Connected { .. });
        let failure = self
            .browser
            .read(cx)
            .failure()
            .map(|reason| SharedString::from(reason.to_owned()));
        v_flex()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .text_sm()
            .child(
                h_flex()
                    .justify_between()
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(
                        h_flex()
                            .gap_3()
                            .child(div().font_weight(FontWeight::SEMIBOLD).child("Maverick"))
                            .child(self.switcher(cx)),
                    )
                    .child(self.status(cx)),
            )
            .children(failure.map(|reason| div().px_3().py_2().text_color(cx.theme().danger).child(reason)))
            .children(self.browser.read(cx).extension_is_outdated().then(|| {
                div()
                    .id("reload-extension")
                    .px_3()
                    .py_2()
                    .text_color(cx.theme().danger)
                    .child(
                        "Chrome is running an old Maverick Bridge extension. \
                     Open chrome://extensions and click the reload arrow on Maverick Bridge.",
                    )
            }))
            .children(self.browser.read(cx).applied().last().map(|sent| {
                let note = match &sent.not_saved {
                    None => format!("Submitted: {}. Marked as applied.", sent.title),
                    Some(reason) => format!("Submitted: {}. It could not be saved: {reason}", sent.title),
                };
                div()
                    .px_3()
                    .py_2()
                    .text_color(cx.theme().success)
                    .child(SharedString::from(note))
            }))
            .children(self.fill_report(cx))
            .child(if connected {
                div()
                    .flex_1()
                    .min_h_0()
                    .child(match self.showing {
                        Showing::Tabs => List::new(&self.tabs)
                            .search_placeholder("Search tabs")
                            .into_any_element(),
                        Showing::Jobs => List::new(&self.jobs)
                            .search_placeholder("Search jobs. Enter opens the posting and fills its form.")
                            .into_any_element(),
                    })
                    .into_any_element()
            } else {
                self.connect_guide(cx)
            })
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{AppContext as _, Bounds, TestAppContext, WindowBounds, WindowOptions, px, size};
    use maverick_core::bridge::{self, Bridge, BrowserState};
    use serde_json::{Value, json};

    use super::MainView;
    use crate::browser::{Browser, Fill};
    use crate::jobs::{Job, job_row_id};
    use crate::tabs::{fill_button_id, row_id};

    /// Stands in for Chrome on the other end of the bridge.
    struct FakeBrowser(UnixStream);

    impl FakeBrowser {
        fn send(&mut self, message: Value) {
            let body = serde_json::to_vec(&message).unwrap();
            self.0
                .write_all(&u32::try_from(body.len()).unwrap().to_ne_bytes())
                .unwrap();
            self.0.write_all(&body).unwrap();
        }

        fn next_request(&mut self, wait: Duration) -> Option<Value> {
            self.0.set_read_timeout(Some(wait)).unwrap();
            let mut length = [0u8; 4];
            self.0.read_exact(&mut length).ok()?;
            let mut body = vec![0u8; u32::from_ne_bytes(length) as usize];
            self.0.read_exact(&mut body).unwrap();
            Some(serde_json::from_slice(&body).unwrap())
        }

        /// Reads requests until one is not a latency ping.
        fn next_command(&mut self, wait: Duration) -> Option<Value> {
            loop {
                let request = self.next_request(wait)?;
                if request["method"] != "ping" {
                    return Some(request);
                }
                self.send(json!({ "kind": "result", "id": request["id"], "value": null }));
            }
        }
    }

    fn wait_until(what: &str, reached: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !reached() {
            assert!(Instant::now() < deadline, "timed out waiting until {what}");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn open_main_window(
        bridge: &Arc<Bridge>,
        runtime: &tokio::runtime::Runtime,
        cx: &mut TestAppContext,
    ) -> gpui_kit::AnyWindowHandle {
        open_window_with_answers(bridge, runtime, None, Vec::new(), cx).0
    }

    fn open_window_with_answers(
        bridge: &Arc<Bridge>,
        runtime: &tokio::runtime::Runtime,
        answers_file: Option<PathBuf>,
        jobs: Vec<Job>,
        cx: &mut TestAppContext,
    ) -> (gpui_kit::AnyWindowHandle, gpui_kit::Entity<Browser>) {
        cx.update(gpui_kit::init);
        let extension_dir = PathBuf::from("/data/maverick/extension");
        let background = runtime.handle().clone();
        let browser = cx.update(|cx| {
            cx.new(|cx| Browser::new(bridge.clone(), background, Ok(extension_dir), answers_file, None, cx))
        });
        let shown = browser.clone();
        let (window, _view) = cx.update(|cx| {
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                    Default::default(),
                    size(px(880.), px(620.)),
                ))),
                ..Default::default()
            };
            gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| MainView::new(browser, jobs, window, cx))
            })
            .expect("the test window opens")
        });
        (window, shown)
    }

    /// Runs the app until the form filler stops, and returns how it ended.
    fn fill_ended(browser: &gpui_kit::Entity<Browser>, cx: &mut TestAppContext) -> Fill {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            cx.run_until_parked();
            let fill = browser.read_with(cx, |browser, _| browser.fill().clone());
            if !matches!(fill, Fill::Running { .. }) {
                return fill;
            }
            assert!(Instant::now() < deadline, "timed out waiting for the form filler");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[gpui_kit::test]
    fn fill_form_shows_on_an_application_tab_and_reports_why_it_could_not_run(cx: &mut TestAppContext) {
        // The filler finishes on the bridge's thread, which the test scheduler otherwise forbids.
        cx.executor().allow_parking();
        let scratch = tempfile::tempdir().unwrap();
        let socket = scratch.path().join("bridge.sock");
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let bridge: Arc<Bridge> = Bridge::new();
        runtime.spawn(bridge.clone().serve(bridge::bind(&socket).unwrap()));
        let mut chrome = FakeBrowser(UnixStream::connect(&socket).unwrap());
        let posting = "https://jobs.ashbyhq.com/example-co/0f3b8231-3a32-478a-b994-3441f4bb9455";
        chrome.send(json!({ "kind": "tabs", "tabs": [
            { "id": 21, "windowId": 5, "title": "Backend Engineer @ Example Co", "url": posting, "active": false },
            { "id": 22, "windowId": 5, "title": "Inbox", "url": "https://mail.example.com/", "active": true },
        ] }));
        wait_until(
            "the bridge holds two tabs",
            || matches!(bridge.state(), BrowserState::Connected { tabs } if tabs.len() == 2),
        );
        let answers_file = scratch.path().join("answers.json");
        let (window, browser) = open_window_with_answers(&bridge, &runtime, Some(answers_file.clone()), Vec::new(), cx);

        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(window.find(fill_button_id(1, 21)).label(), Some("Fill form"));
            // This Chrome never said its extension version, as versions before 0.3.0 do not.
            assert!(browser.read(cx).extension_is_outdated());
            assert!(window.try_find(fill_button_id(1, 22)).is_none());
            assert!(window.try_find("close-fill-report").is_none());
            window.click(fill_button_id(1, 21), cx);
        })
        .unwrap();
        let Fill::Failed { title, reason } = fill_ended(&browser, cx) else {
            panic!("the fill cannot run without an answers file");
        };
        assert_eq!(title, "Backend Engineer @ Example Co");
        assert!(reason.starts_with("could not read"), "unexpected reason: {reason}");

        std::fs::write(
            &answers_file,
            r#"{ "name": "Ada Example", "email": "ada@example.com", "phone": "+1 555 0100", "resume": "/tmp/ada.pdf" }"#,
        )
        .unwrap();
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.click("close-fill-report", cx);
            window.render_frame(cx);
            assert!(window.try_find("close-fill-report").is_none());
            window.click(fill_button_id(1, 21), cx);
        })
        .unwrap();
        // Chrome is asked to bring the tab forward, then to attach. Here Chrome refuses the attach.
        let mut asked = Vec::new();
        while asked.last().map(String::as_str) != Some("debugger.attach") {
            let request = chrome
                .next_command(Duration::from_secs(5))
                .expect("the fill reaches Chrome");
            assert_eq!(request["tabId"], json!(21));
            asked.push(request["method"].as_str().unwrap().to_owned());
            let reply = match asked.last().unwrap().as_str() {
                "debugger.attach" => json!({ "kind": "error", "id": request["id"], "message": "Chrome said no" }),
                _ => json!({ "kind": "result", "id": request["id"], "value": null }),
            };
            chrome.send(reply);
        }
        assert!(asked.contains(&"tabs.activate".to_owned()), "asked: {asked:?}");
        assert_eq!(
            fill_ended(&browser, cx),
            Fill::Failed {
                title: "Backend Engineer @ Example Co".into(),
                reason: "Chrome said no".into()
            }
        );
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("close-fill-report").is_some());
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn the_window_shows_how_to_connect_when_chrome_is_not_connected(cx: &mut TestAppContext) {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let window = open_main_window(&Bridge::new(), &runtime, cx);

        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(window.find("show-extension-folder").label(), Some("Show folder"));
            assert!(window.try_find("measure-latency").is_none());
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn a_click_or_enter_switches_chrome_to_the_chosen_tab(cx: &mut TestAppContext) {
        let scratch = tempfile::tempdir().unwrap();
        let socket = scratch.path().join("bridge.sock");
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let bridge: Arc<Bridge> = Bridge::new();
        runtime.spawn(bridge.clone().serve(bridge::bind(&socket).unwrap()));

        let mut chrome = FakeBrowser(UnixStream::connect(&socket).unwrap());
        chrome.send(json!({ "kind": "tabs", "tabs": [
            { "id": 12, "windowId": 5, "title": "Backend Engineer", "url": "https://jobs.example.com/1", "active": false },
            { "id": 11, "windowId": 5, "title": "Inbox", "url": "https://mail.example.com/", "active": true },
            { "id": 13, "windowId": 6, "title": "Infra Engineer", "url": "https://jobs.example.com/2", "active": false },
        ] }));
        wait_until(
            "the bridge holds three tabs",
            || matches!(bridge.state(), BrowserState::Connected { tabs } if tabs.len() == 3),
        );

        let window = open_main_window(&bridge, &runtime, cx);

        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            let status = window.find("measure-latency").label().map(str::to_owned);
            assert!(
                status
                    .as_deref()
                    .is_some_and(|label| label.starts_with("Chrome connected · 3 tabs")),
                "unexpected status: {status:?}"
            );

            window.click(row_id(1, 12), cx);
        })
        .unwrap();
        let clicked = chrome
            .next_command(Duration::from_secs(5))
            .expect("a click asks Chrome to switch tabs");
        assert_eq!(
            (clicked["tabId"].clone(), clicked["windowId"].clone()),
            (json!(12), json!(5))
        );

        cx.update_window(window, |_, window, cx| {
            // Typing after a click still searches. Without the filter, Down would land on the inbox.
            window.input("jobs", cx);
            window.press("down", cx);
            window.press("enter", cx);
        })
        .unwrap();
        let switched = chrome
            .next_command(Duration::from_secs(5))
            .expect("Chrome is asked to switch tabs");
        assert_eq!(switched["method"], "tabs.activate");
        assert_eq!(
            (switched["tabId"].clone(), switched["windowId"].clone()),
            (json!(13), json!(6))
        );

        cx.update_window(window, |_, window, cx| {
            window.input(" payroll", cx);
            window.press("enter", cx);
        })
        .unwrap();
        assert_eq!(
            chrome.next_command(Duration::from_millis(300)),
            None,
            "no tab matches, so Enter does nothing"
        );
    }

    #[gpui_kit::test]
    fn choosing_a_job_opens_a_new_tab_for_its_form(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let scratch = tempfile::tempdir().unwrap();
        let socket = scratch.path().join("bridge.sock");
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let bridge: Arc<Bridge> = Bridge::new();
        runtime.spawn(bridge.clone().serve(bridge::bind(&socket).unwrap()));
        let mut chrome = FakeBrowser(UnixStream::connect(&socket).unwrap());
        chrome.send(json!({ "kind": "tabs", "tabs": [
            { "id": 22, "windowId": 5, "title": "Inbox", "url": "https://mail.example.com/", "active": true },
        ] }));
        wait_until(
            "the bridge holds the tab",
            || matches!(bridge.state(), BrowserState::Connected { tabs } if tabs.len() == 1),
        );
        let answers_file = scratch.path().join("answers.json");
        std::fs::write(
            &answers_file,
            r#"{ "name": "Ada Example", "email": "ada@example.com", "phone": "+1 555 0100", "resume": "/tmp/ada.pdf" }"#,
        )
        .unwrap();
        let posting = "https://jobs.ashbyhq.com/example-co/0f3b8231-3a32-478a-b994-3441f4bb9455";
        let jobs = vec![Job {
            company: "Example Co".into(),
            title: "Backend Engineer".into(),
            score: Some(82),
            address: posting.into(),
        }];
        let (window, browser) = open_window_with_answers(&bridge, &runtime, Some(answers_file), jobs, cx);

        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(window.find("show-jobs").label(), Some("Jobs · 1"));
            window.click("show-jobs", cx);
            window.render_frame(cx);
            window.click(job_row_id(0), cx);
        })
        .unwrap();

        // Chrome opens an empty tab, the debugger attaches, and only then does the form load.
        let mut asked = Vec::new();
        loop {
            let request = chrome
                .next_command(Duration::from_secs(5))
                .expect("the fill reaches Chrome");
            let id = request["id"].clone();
            let method = request["method"].as_str().unwrap().to_owned();
            let command = request["command"].as_str().unwrap_or_default().to_owned();
            asked.push(if command.is_empty() {
                method.clone()
            } else {
                command.clone()
            });
            match (method.as_str(), command.as_str()) {
                ("tabs.create", _) => {
                    assert_eq!(
                        (request["url"].clone(), request["active"].clone()),
                        (json!("about:blank"), json!(true))
                    );
                    chrome.send(json!({ "kind": "result", "id": id, "value": { "id": 31, "windowId": 5 } }));
                }
                (_, "Page.navigate") => {
                    assert_eq!(request["tabId"], json!(31));
                    assert_eq!(request["params"]["url"], json!(format!("{posting}/application")));
                    chrome.send(json!({ "kind": "result", "id": id, "value": { "errorText": "net::ERR_FAILED" } }));
                }
                ("debugger.detach", _) => {
                    chrome.send(json!({ "kind": "result", "id": id, "value": null }));
                    break;
                }
                _ => chrome.send(json!({ "kind": "result", "id": id, "value": {} })),
            }
        }
        assert_eq!(
            asked,
            [
                "tabs.create",
                "debugger.attach",
                "Page.enable",
                "Emulation.setFocusEmulationEnabled",
                "Page.navigate",
                "debugger.detach"
            ]
        );
        assert_eq!(
            fill_ended(&browser, cx),
            Fill::Failed {
                title: "Backend Engineer at Example Co".into(),
                reason: "the page did not load: net::ERR_FAILED".into()
            }
        );
    }
}
