//! The main window: a status header over either the tab list or the steps to connect Chrome.

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::list::{List, ListEvent, ListState};
use gpui_kit::component::{ActiveTheme as _, IndexPath, Sizable as _, Theme, h_flex, v_flex};
use gpui_kit::{
    AnyElement, AppContext as _, Context, Entity, FontWeight, InteractiveElement as _, IntoElement, ParentElement as _,
    Render, SharedString, Styled as _, Subscription, Window, div,
};
use maverick_core::bridge::BrowserState;

use crate::browser::{Browser, Latency};
use crate::tabs::TabList;

pub struct MainView {
    browser: Entity<Browser>,
    tabs: Entity<ListState<TabList>>,
    _subscriptions: Vec<Subscription>,
}

impl MainView {
    pub fn new(browser: Entity<Browser>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let tabs = cx.new(|cx| ListState::new(TabList::default(), window, cx).searchable(true));
        let _subscriptions = vec![
            cx.observe_in(&browser, window, |this, _, window, cx| {
                this.show_current_tabs(window, cx);
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
            cx.observe_window_appearance(window, |_, window, cx| {
                Theme::sync_system_appearance(Some(window), cx);
            }),
        ];
        let mut view = Self {
            browser,
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
                    .child(div().font_weight(FontWeight::SEMIBOLD).child("Maverick"))
                    .child(self.status(cx)),
            )
            .children(failure.map(|reason| div().px_3().py_2().text_color(cx.theme().danger).child(reason)))
            .child(if connected {
                div()
                    .flex_1()
                    .min_h_0()
                    .child(List::new(&self.tabs).search_placeholder("Search tabs"))
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
    use crate::browser::Browser;
    use crate::tabs::row_id;

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
        cx.update(gpui_kit::init);
        let extension_dir = PathBuf::from("/data/maverick/extension");
        let background = runtime.handle().clone();
        let (window, _view) = cx.update(|cx| {
            let browser = cx.new(|cx| Browser::new(bridge.clone(), background, Ok(extension_dir), cx));
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                    Default::default(),
                    size(px(880.), px(620.)),
                ))),
                ..Default::default()
            };
            gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| MainView::new(browser, window, cx))
            })
            .expect("the test window opens")
        });
        window
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
}
