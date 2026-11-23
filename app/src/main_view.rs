//! The main window: a bar, the band, and either a list beside a detail pane or the steps to connect Chrome.

use std::path::PathBuf;
use std::time::Duration;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::list::{List, ListEvent, ListState};
use gpui_kit::component::popover::Popover;
use gpui_kit::component::{IndexPath, Theme, TitleBar, h_flex, v_flex};
use gpui_kit::{
    AnyElement, AppContext as _, Bounds, Context, Div, Entity, FontWeight, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, Task, Window,
    canvas, div, fill, img, point, px, size,
};
use maverick_core::bridge::{BrowserState, Tab};

use gpui_kit::TestSupportExt as _;

use crate::band::{Band, Mark};
use crate::board::{Board, Entry, system_name};
use crate::browser::{Browser, Fill, Latency, Subject};
use crate::connections::Connections;
use crate::connections::OpenSetup;
use crate::icon::AppIcon;
use crate::mav::Mav;
use crate::outreach::OutreachView;
use crate::pane_widths::Pane;
use crate::report;
use crate::schedule::{BETWEEN_PASSES, FIRST_PASS, Funnel, Lists, Refreshed};
use crate::setup::{Finished, Setup};
use crate::tabs::{TabList, host_of};
use crate::ui::{self, Palette, Signal};

/// How often the band redraws while the window is in front.
const FRAME: Duration = Duration::from_millis(66);
const BAR_HEIGHT: f32 = 40.0;
const BAND_HEIGHT: f32 = 164.0;
/// The widest the detail pane gets. A wider window gives the rest to the list.
const PANE_WIDTH: f32 = 620.0;
/// How many jobs the band tracks besides the target.
const TRACKED: usize = 6;

/// Which list the window shows.
#[derive(Clone, Copy, PartialEq)]
enum Showing {
    Tabs,
    /// The postings to approve or skip, the people to reach at each, and the applications sent.
    Jobs,
    /// What is due in TickTick.
    Mav,
    /// Hiring posts found on LinkedIn, and the notes drafted for them.
    Posts,
    /// The Google profiles, TickTick, the model and Chrome.
    Connections,
    /// The first-run guide.
    Setup,
}

pub struct MainView {
    browser: Entity<Browser>,
    showing: Showing,
    icon: AppIcon,
    icon_file: Option<PathBuf>,
    tabs: Entity<ListState<TabList>>,
    board: Entity<Board>,
    mav: Entity<Mav>,
    outreach: Entity<OutreachView>,
    connections: Entity<Connections>,
    setup: Entity<Setup>,
    /// How many of the applications sent in this run Mav has offered a follow-up for.
    followed_up: usize,
    band: Band,
    /// What the last funnel pass did, for the bar.
    funnel_status: Option<String>,
    _funnel: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
    _drift: Task<()>,
}

impl MainView {
    pub fn new(
        browser: Entity<Browser>,
        lists: Lists,
        icon_file: Option<PathBuf>,
        first_run: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let Lists { cells, board } = lists;
        // A first start opens the guide, so a new user sets up a model before anything needs one.
        let showing = if first_run { Showing::Setup } else { Showing::Jobs };
        let tabs = cx.new(|cx| ListState::new(TabList::new(browser.clone()), window, cx).searchable(true));
        let background = browser.read(cx).background();
        let database = browser.read(cx).database();
        let answers = browser.read(cx).answers_file();
        let outreach = cx.new(|cx| OutreachView::new(browser.clone(), answers, window, cx));
        let board = cx.new(|cx| Board::new(browser.clone(), outreach.clone(), board, database.clone(), window, cx));
        let mav = cx.new(|cx| Mav::new(background, database, window, cx));
        let connections = cx.new(|cx| Connections::new(browser.clone(), window, cx));
        let setup = cx.new(|cx| Setup::new(connections.clone(), cx));
        let _subscriptions = vec![
            cx.subscribe_in(&setup, window, |this, _, _: &Finished, window, cx| {
                this.show(Showing::Connections, window, cx);
            }),
            cx.subscribe_in(&connections, window, |this, _, _: &OpenSetup, window, cx| {
                this.show(Showing::Setup, window, cx);
            }),
            // The bar counts what is due and what waits on a decision.
            cx.observe(&mav, |_, _, cx| cx.notify()),
            cx.observe(&outreach, |_, _, cx| cx.notify()),
            cx.observe_in(&browser, window, |this, browser, window, cx| {
                this.show_current_tabs(window, cx);
                // A job's cell turns applied once its application went through.
                for sent in browser.read(cx).applied() {
                    this.band.mark_applied(&sent.address);
                }
                this.offer_follow_ups(cx);
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
                // The detail pane follows the selection.
                cx.notify();
            }),
            // The band follows the selected posting.
            cx.observe(&board, |_, _, cx| cx.notify()),
            cx.observe_window_appearance(window, |_, window, cx| {
                Theme::sync_system_appearance(Some(window), cx);
            }),
        ];
        let _drift = cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(FRAME).await;
                let drawn = this.update_in(cx, |_, window, cx| {
                    if window.is_window_active() {
                        cx.notify();
                    }
                });
                if drawn.is_err() {
                    break;
                }
            }
        });
        let icon = AppIcon::saved(icon_file.as_deref());
        icon.show_in_dock();
        let mut view = Self {
            browser,
            showing,
            icon,
            icon_file,
            tabs,
            board,
            mav,
            outreach,
            connections,
            setup,
            followed_up: 0,
            band: Band::new(cells),
            funnel_status: None,
            _funnel: None,
            _subscriptions,
            _drift,
        };
        view.show_current_tabs(window, cx);
        view.show(showing, window, cx);
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
            let shown = list.delegate().len();
            keep_a_row_selected(list, shown, window, cx);
            cx.notify();
        });
    }

    fn show(&mut self, showing: Showing, window: &mut Window, cx: &mut Context<Self>) {
        self.showing = showing;
        match showing {
            Showing::Tabs => self.tabs.update(cx, |tabs, cx| tabs.focus(window, cx)),
            Showing::Jobs => self.board.update(cx, |board, cx| board.focus(window, cx)),
            Showing::Mav => self.mav.update(cx, |mav, cx| {
                mav.load(cx);
                mav.focus(window, cx);
            }),
            Showing::Posts => self.outreach.update(cx, |outreach, cx| {
                outreach.reload();
                cx.notify();
            }),
            Showing::Connections => self.connections.update(cx, |connections, cx| {
                connections.show_only(None, cx);
                connections.refresh(window, cx);
            }),
            Showing::Setup => self.setup.update(cx, |setup, cx| setup.start(window, cx)),
        }
        cx.notify();
    }

    /// Offers a follow-up task in Mav for each application that went through since the last offer.
    fn offer_follow_ups(&mut self, cx: &mut Context<Self>) {
        let applied = self.browser.read(cx).applied();
        let new: Vec<String> = applied[self.followed_up.min(applied.len())..]
            .iter()
            .map(|sent| sent.title.clone())
            .collect();
        self.followed_up = applied.len();
        let now = jiff::ZonedDateTime::now();
        self.mav.update(cx, |mav, cx| {
            for title in new {
                if let Some(action) = maverick_core::mav::follow_up(&title, &now, &mav.lists()) {
                    let why = format!("Your application for {title} went through. Follow up in a week?");
                    mav.suggest(why, action, cx);
                }
            }
        });
    }

    /// The bar: the wordmark, the views, and the link to Chrome.
    fn bar(&self, palette: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let showing_now = self.showing;
        let tab = |id: &'static str,
                   name: String,
                   word: &'static str,
                   count: Option<usize>,
                   showing: Showing,
                   cx: &mut Context<Self>| {
            let ink = palette.ink;
            let face = h_flex()
                .gap(px(5.))
                .text_color(if showing_now == showing {
                    palette.ink
                } else {
                    palette.faint
                })
                .hover(move |face| face.text_color(ink))
                .child(word)
                .children(count.map(|count| {
                    div()
                        .text_color(palette.faint)
                        .child(SharedString::from(count.to_string()))
                }));
            ui::button(id, name, face)
                .on_click(cx.listener(move |this, _, window, cx| this.show(showing, window, cx)))
                .into_any_element()
        };
        let tabs = match self.browser.read(cx).state() {
            BrowserState::Connected { tabs } => Some(tabs.len()),
            BrowserState::Disconnected => None,
        };
        TitleBar::new()
            .h(px(BAR_HEIGHT))
            .pr(px(14.))
            .child(
                h_flex()
                    .gap_5()
                    .child(
                        h_flex().ml_1().gap(px(6.)).child(self.icon_picker(palette, cx)).child(
                            div()
                                .font_family(ui::DOTS)
                                .font_weight(FontWeight::BLACK)
                                .text_size(px(17.))
                                .text_color(palette.ink)
                                .child("MAVERICK"),
                        ),
                    )
                    .child(
                        h_flex()
                            .gap_4()
                            .child({
                                let pending = self.board.read(cx).pending(cx);
                                tab(
                                    "show-jobs",
                                    format!("Jobs · {pending}"),
                                    "jobs",
                                    Some(pending),
                                    Showing::Jobs,
                                    cx,
                                )
                            })
                            .child(tab("show-tabs", "Tabs".to_owned(), "tabs", tabs, Showing::Tabs, cx))
                            .child({
                                let pressing = self.mav.read(cx).pressing();
                                let name = pressing.map_or("Mav".to_owned(), |due| format!("Mav · {due}"));
                                tab("show-mav", name, "mav", pressing, Showing::Mav, cx)
                            })
                            .child({
                                let waiting = self.outreach.read(cx).waiting();
                                let count = (waiting > 0).then_some(waiting);
                                let name = count.map_or("Posts".to_owned(), |waiting| format!("Posts · {waiting}"));
                                tab("show-posts", name, "posts", count, Showing::Posts, cx)
                            }),
                    ),
            )
            .child(
                h_flex()
                    .gap_4()
                    .min_w_0()
                    .children(self.funnel_status.clone().map(|status| {
                        let tip = SharedString::from(status.clone());
                        div()
                            .id("funnel-status")
                            .test_support()
                            .role(gpui_kit::Role::Label)
                            .aria_label(status.clone())
                            .tooltip(move |window, cx| {
                                gpui_kit::component::tooltip::Tooltip::new(tip.clone()).build(window, cx)
                            })
                            .text_xs()
                            .text_color(palette.faint)
                            .max_w(px(380.))
                            .truncate()
                            .child(status)
                    }))
                    .child(self.link(palette, cx))
                    .child(tab(
                        "show-connections",
                        "Connections".to_owned(),
                        "connections",
                        None,
                        Showing::Connections,
                        cx,
                    )),
            )
    }

    /// Runs the funnel a minute from now and then every hour, and shows what each pass found.
    pub fn run_funnel(&mut self, funnel: Funnel, window: &mut Window, cx: &mut Context<Self>) {
        self._funnel = Some(cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(FIRST_PASS).await;
            loop {
                if let Some(refreshed) = funnel.pass().await {
                    let shown = this.update_in(cx, |this, window, cx| this.refresh(refreshed, window, cx));
                    if shown.is_err() {
                        break;
                    }
                }
                cx.background_executor().timer(BETWEEN_PASSES).await;
            }
        }));
    }

    /// Starts the hourly outreach checks and the two-hourly hiring-post sweep.
    pub fn run_outreach(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.outreach
            .update(cx, |outreach, cx| outreach.start_schedule(window, cx));
    }

    /// Shows the lists and the status a funnel pass left.
    fn refresh(&mut self, refreshed: Refreshed, window: &mut Window, cx: &mut Context<Self>) {
        self.funnel_status = Some(refreshed.status);
        if let Some(Lists { cells, board }) = refreshed.lists {
            self.band.replace(cells);
            self.board.update(cx, |shown, cx| shown.replace(board, window, cx));
        }
        cx.notify();
    }

    fn set_icon(&mut self, icon: AppIcon, cx: &mut Context<Self>) {
        self.icon = icon;
        icon.show_in_dock();
        if let Some(file) = &self.icon_file
            && let Err(error) = icon.save(file)
        {
            eprintln!("maverick: could not save the icon choice: {error}");
        }
        cx.notify();
    }

    /// The current app icon. Clicking it shows every icon to choose from.
    fn icon_picker(&self, palette: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let view = cx.entity().downgrade();
        let current = self.icon;
        let palette = *palette;
        Popover::new("icon-picker")
            .trigger(
                Button::new("pick-icon")
                    .ghost()
                    .compact()
                    .tooltip("Change the app icon")
                    .child(img(current.image()).size(px(18.))),
            )
            .content(move |_, _, cx| {
                h_flex().gap_1().children(AppIcon::ALL.into_iter().map(|icon| {
                    let view = view.clone();
                    let chosen = icon == current;
                    let ink = palette.ink;
                    let face = v_flex()
                        .relative()
                        .items_center()
                        .gap(px(6.))
                        .p_2()
                        .hover(move |face| face.bg(ink.opacity(0.05)))
                        .child(img(icon.image()).size(px(56.)))
                        .child(ui::caps(icon.name()).text_color(if chosen { palette.ink } else { palette.muted }))
                        .children(chosen.then(|| ui::brackets(palette.ink)).into_iter().flatten());
                    ui::button(icon.name(), icon.name(), face).on_click(cx.listener(move |popover, _, window, cx| {
                        view.update(cx, |this, cx| this.set_icon(icon, cx)).ok();
                        popover.dismiss(window, cx);
                    }))
                }))
            })
            .into_any_element()
    }

    /// The state of the link to Chrome. A click measures the round trip again.
    fn link(&self, palette: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let browser = self.browser.read(cx);
        let BrowserState::Connected { tabs } = browser.state() else {
            return h_flex()
                .gap_2()
                .text_color(palette.muted)
                .child(ui::square(palette.faint))
                .child("chrome not connected")
                .into_any_element();
        };
        let count = match tabs.len() {
            1 => "1 tab".to_owned(),
            count => format!("{count} tabs"),
        };
        let (latency, signal) = match browser.latency() {
            Latency::Measuring => ("measuring".to_owned(), Signal::Done),
            Latency::Measured(took) => (format!("{:.1} ms", took.as_secs_f64() * 1000.0), Signal::Done),
            Latency::Failed(reason) => (reason.clone(), Signal::Failed),
        };
        let ink = palette.ink;
        let face = h_flex()
            .gap_2()
            .text_color(palette.muted)
            .hover(move |face| face.text_color(ink))
            .child(ui::square(palette.signal(signal)))
            .child(SharedString::from(format!("chrome {latency}")));
        ui::button(
            "measure-latency",
            format!("Chrome connected · {count} · {latency}"),
            face,
        )
        .tip("Round trip to Chrome and back. Click to measure again.")
        .on_click(cx.listener(|this, _, _, cx| {
            this.browser.update(cx, |browser, cx| browser.measure(cx));
        }))
        .into_any_element()
    }

    /// One line each for what the user should know right now, whatever list is showing.
    fn notices(&self, palette: &Palette, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let browser = self.browser.read(cx);
        let notice = |signal: Signal, text: String| {
            h_flex()
                .items_start()
                .gap(px(9.))
                .px(px(14.))
                .py(px(7.))
                .border_b_1()
                .border_color(palette.line)
                .child(ui::square(palette.signal(signal)).mt(px(6.)))
                .child(
                    ui::serif(text)
                        .flex_1()
                        .min_w_0()
                        .text_size(px(13.5))
                        .text_color(palette.ink),
                )
        };
        let mut notices = Vec::new();
        if let Some(reason) = browser.failure() {
            notices.push(notice(Signal::Failed, reason.to_owned()).into_any_element());
        }
        if browser.extension_is_outdated() {
            let reload = "Chrome is running an old Maverick Bridge extension. \
                          Open chrome://extensions and click the reload arrow on Maverick Bridge.";
            notices.push(
                notice(Signal::Yours, reload.to_owned())
                    .id("reload-extension")
                    .into_any_element(),
            );
        }
        if let Some(sent) = browser.applied().last() {
            notices.push(
                match &sent.not_saved {
                    None => notice(Signal::Done, format!("Submitted: {}. Marked as applied.", sent.title)),
                    Some(reason) => notice(
                        Signal::Failed,
                        format!("Submitted: {}. It could not be saved: {reason}", sent.title),
                    ),
                }
                .into_any_element(),
            );
        }
        notices
    }

    /// The band, aimed at the selected posting and tracking the next best ones.
    fn band(&self, palette: &Palette, cx: &mut Context<Self>) -> Div {
        let mark = |entry: &Entry| Mark {
            address: entry.address.clone(),
            score: entry.score,
            title: entry.title.clone(),
        };
        let board = self.board.read(cx);
        let target = (self.showing == Showing::Jobs)
            .then(|| board.selected_entry(cx))
            .flatten();
        let tracked = board
            .best(TRACKED, target.map(|entry| entry.address.as_str()), cx)
            .into_iter()
            .map(mark)
            .collect();
        div()
            .flex_none()
            .h(px(BAND_HEIGHT))
            .border_b_1()
            .border_color(palette.line)
            .child(self.band.element(target.map(mark), tracked, cx))
    }

    /// The detail pane: the selected tab, and its fill report once it has one.
    fn pane(&self, palette: &Palette, cx: &mut Context<Self>) -> Div {
        match self.tabs.read(cx).delegate().selected_tab().cloned() {
            Some(tab) => self.tab_pane(&tab, palette, cx),
            None => v_flex(),
        }
    }

    fn tab_pane(&self, tab: &Tab, palette: &Palette, cx: &mut Context<Self>) -> Div {
        let fill = self
            .browser
            .read(cx)
            .fill_of(&Subject::Tab(tab.connection, tab.tab.id))
            .cloned();
        let host = host_of(&tab.tab.url);
        let tag = match system_name(&tab.tab.url) {
            Some(system) => format!("{host} / {system}"),
            None => host.to_owned(),
        };
        let title = if tab.tab.title.is_empty() {
            &tab.tab.url
        } else {
            &tab.tab.title
        };
        let shows = if tab.tab.active {
            "in front in its window"
        } else {
            "behind another tab"
        };
        let facts = [("address", tab.tab.url.clone()), ("chrome", shows.to_owned())];
        let note = match &fill {
            Some(Fill::Running { .. }) => "filling. nothing gets submitted.",
            Some(Fill::Done { .. }) => "you click submit.",
            Some(_) => "nothing was submitted.",
            None => "enter switches chrome to this tab.",
        };
        v_flex()
            .size_full()
            .child(header(&tag, title, None, palette))
            .children(self.pane_body(fill.as_ref(), &facts, palette))
            .child(self.footer(None, fill.as_ref(), note, palette, cx))
    }

    /// The middle of the detail pane: the fill report when there is one, the row's facts when not.
    fn pane_body(&self, fill: Option<&Fill>, facts: &[(&str, String)], palette: &Palette) -> Vec<AnyElement> {
        let scroll = |id: &'static str, content: Div| {
            div()
                .id(id)
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .px_4()
                .py_3()
                .child(content)
                .into_any_element()
        };
        let said = |signal: Signal, title: &str, why: &str| {
            h_flex()
                .items_start()
                .gap(px(9.))
                .child(ui::square(palette.signal(signal)).mt(px(7.)))
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(ui::serif(title.to_owned()).text_color(palette.ink))
                        .child(
                            div()
                                .text_size(px(11.))
                                .line_height(px(15.))
                                .text_color(palette.muted)
                                .child(SharedString::from(why.to_owned())),
                        ),
                )
        };
        match fill {
            Some(Fill::Done { report, .. }) => vec![
                div()
                    .flex_none()
                    .px_4()
                    .pb_3()
                    .border_b_1()
                    .border_color(palette.line)
                    .child(report::form_map(report, palette))
                    .into_any_element(),
                scroll("fill-report", report::sections(report, palette)),
            ],
            Some(Fill::Running { .. }) => vec![scroll(
                "fill-report",
                said(
                    Signal::Yours,
                    "Filling the form in Chrome",
                    "Maverick answers what your standard answers and facts cover.",
                ),
            )],
            Some(Fill::Failed { reason, .. }) => vec![scroll(
                "fill-report",
                said(Signal::Failed, "Maverick could not fill this form", reason),
            )],
            Some(Fill::Idle) | None => vec![scroll("facts", ui::facts(facts, palette))],
        }
    }

    /// The foot of the detail pane: the user's move, a way to put a report away, and a reminder.
    fn footer(
        &self,
        primary: Option<AnyElement>,
        fill: Option<&Fill>,
        note: &str,
        palette: &Palette,
        cx: &mut Context<Self>,
    ) -> Div {
        let closable = matches!(fill, Some(Fill::Done { .. } | Fill::Failed { .. }));
        h_flex()
            .flex_none()
            .gap_3()
            .px_4()
            .py(px(10.))
            .min_h(px(46.))
            .border_t_1()
            .border_color(palette.line)
            .children(primary)
            .children(closable.then(|| {
                ui::button("close-fill-report", "Close", ui::quiet_face("close", palette)).on_click(cx.listener(
                    |this, _, _, cx| {
                        this.browser.update(cx, |browser, cx| browser.dismiss_fill(cx));
                    },
                ))
            }))
            .child(
                ui::serif(note.to_owned())
                    .ml_auto()
                    .text_size(px(13.5))
                    .text_color(palette.muted),
            )
    }

    fn connect_guide(&self, palette: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let step = |text: &'static str| ui::serif(text).text_color(palette.ink);
        let guide = v_flex()
            .id("connect-guide")
            .w_full()
            .max_w(px(560.))
            .mx_auto()
            .p_8()
            .gap_3()
            .child(h_flex().child(ui::boxed("chrome not connected", palette.faint)))
            .child(
                div()
                    .font_family(ui::SERIF)
                    .text_size(px(26.))
                    .line_height(px(30.))
                    .text_color(palette.ink)
                    .child("Connect Chrome"),
            );
        match self.browser.read(cx).extension_dir() {
            Err(reason) => guide
                .child(
                    h_flex()
                        .items_start()
                        .gap(px(9.))
                        .child(ui::square(palette.signal(Signal::Failed)).mt(px(7.)))
                        .child(ui::serif(reason.to_owned()).flex_1().text_color(palette.ink)),
                )
                .into_any_element(),
            Ok(folder) => {
                let folder = folder.to_owned();
                guide
                    .child(step(
                        "1. Open chrome://extensions in Chrome and turn on Developer mode.",
                    ))
                    .child(step("2. Show the extension folder, then drag it onto that page."))
                    .child(
                        h_flex()
                            .gap_3()
                            .p_2()
                            .border_1()
                            .border_color(palette.line)
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_size(px(11.))
                                    .text_color(palette.muted)
                                    .child(SharedString::from(folder.display().to_string())),
                            )
                            .child(
                                ui::button(
                                    "show-extension-folder",
                                    "Show folder",
                                    ui::primary_face("show folder", None, palette),
                                )
                                .on_click(move |_, _, cx| cx.reveal_path(&folder)),
                            ),
                    )
                    .child(
                        ui::serif(
                            "This screen changes to your lists as soon as Chrome connects. \
                             If the extension is already loaded, Chrome reconnects within 30 seconds.",
                        )
                        .text_size(px(13.5))
                        .text_color(palette.muted),
                    )
                    .into_any_element()
            }
        }
    }
}

/// Keeps the selection on a row that exists, so Enter always has a target.
fn keep_a_row_selected<D: gpui_kit::component::list::ListDelegate>(
    list: &mut ListState<D>,
    shown: usize,
    window: &mut Window,
    cx: &mut Context<ListState<D>>,
) {
    let selected = match list.selected_index() {
        _ if shown == 0 => None,
        Some(ix) if ix.row < shown => Some(ix),
        _ => Some(IndexPath::default()),
    };
    list.set_selected_index(selected, window, cx);
}

/// The head of the detail pane: where the row is from, its name, and its score when it has one.
pub(crate) fn header(tag: &str, title: &str, score: Option<Option<u8>>, palette: &Palette) -> Div {
    h_flex()
        .flex_none()
        .items_end()
        .justify_between()
        .gap_4()
        .px_4()
        .pt(px(14.))
        .pb(px(12.))
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap(px(7.))
                .child(h_flex().child(ui::boxed(tag, palette.faint).truncate()))
                .child(
                    div()
                        .font_family(ui::SERIF)
                        .text_size(px(26.))
                        .line_height(px(30.))
                        .text_color(palette.ink)
                        .child(SharedString::from(title.to_owned())),
                ),
        )
        .children(score.map(|score| ui::score(score, 54.).text_color(palette.ink)))
}

/// The faint grid behind the whole window.
fn grid(color: gpui_kit::Hsla) -> impl IntoElement {
    const STEP: f32 = 24.0;
    canvas(
        |_, _, _| (),
        move |bounds, (), window, _| {
            let (width, height) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
            let mut x = STEP;
            while x < width {
                let from = point(bounds.origin.x + px(x), bounds.origin.y);
                window.paint_quad(fill(Bounds::new(from, size(px(1.), bounds.size.height)), color));
                x += STEP;
            }
            let mut y = STEP;
            while y < height {
                let from = point(bounds.origin.x, bounds.origin.y + px(y));
                window.paint_quad(fill(Bounds::new(from, size(bounds.size.width, px(1.))), color));
                y += STEP;
            }
        },
    )
    .absolute()
    .size_full()
}

impl Render for MainView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::of(cx);
        let available = window.viewport_size().width - ui::DIVIDER_WIDTH;
        let pane_max = (available * 0.68).clamp(px(280.), px(900.));
        let pane_default = (available * 0.52).min(px(PANE_WIDTH));
        let pane_width = Pane::Details
            .width(cx)
            .unwrap_or(pane_default)
            .clamp(px(280.), pane_max);
        let connected = matches!(self.browser.read(cx).state(), BrowserState::Connected { .. });
        v_flex()
            .relative()
            .size_full()
            .bg(palette.background)
            .text_color(palette.ink)
            .font_family(ui::MONO)
            .text_size(px(12.))
            .line_height(px(16.))
            .child(grid(palette.grid))
            .child(self.bar(&palette, cx))
            .children(self.notices(&palette, cx))
            // The band tracks jobs, so the Mav view leaves it out.
            .children(matches!(self.showing, Showing::Jobs | Showing::Tabs).then(|| self.band(&palette, cx)))
            .child(if self.showing == Showing::Jobs {
                div().flex_1().min_h_0().child(self.board.clone()).into_any_element()
            } else if self.showing == Showing::Mav {
                div().flex_1().min_h_0().child(self.mav.clone()).into_any_element()
            } else if self.showing == Showing::Posts {
                div().flex_1().min_h_0().child(self.outreach.clone()).into_any_element()
            } else if self.showing == Showing::Connections {
                div()
                    .flex_1()
                    .min_h_0()
                    .child(self.connections.clone())
                    .into_any_element()
            } else if self.showing == Showing::Setup {
                div().flex_1().min_h_0().child(self.setup.clone()).into_any_element()
            } else if connected {
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .items_start()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .child(List::new(&self.tabs).search_placeholder("search tabs")),
                    )
                    .child(ui::pane_divider(
                        Pane::Details,
                        pane_width,
                        px(280.)..=pane_max,
                        true,
                        &palette,
                        cx,
                    ))
                    .child(div().flex_none().w(pane_width).h_full().child(self.pane(&palette, cx)))
                    .into_any_element()
            } else {
                self.connect_guide(&palette, cx)
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
    use std::collections::HashMap;

    use gpui_kit::{AppContext as _, Bounds, TestAppContext, WindowBounds, WindowOptions, px, size};
    use maverick_core::bridge::{self, Bridge, BrowserState};
    use maverick_core::jobs::{Posting, Stage};
    use serde_json::{Value, json};

    use super::MainView;
    use crate::board::BoardData;
    use crate::browser::{Browser, Fill};
    use crate::icon::AppIcon;
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
        open_window_with_answers(bridge, runtime, None, BoardData::default(), cx).0
    }

    fn open_window_with_icon_file(icon_file: PathBuf, cx: &mut TestAppContext) -> gpui_kit::AnyWindowHandle {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        open_window(
            &Bridge::new(),
            &runtime,
            None,
            BoardData::default(),
            Some(icon_file),
            cx,
        )
        .0
    }

    fn open_window_with_answers(
        bridge: &Arc<Bridge>,
        runtime: &tokio::runtime::Runtime,
        answers_file: Option<PathBuf>,
        board: BoardData,
        cx: &mut TestAppContext,
    ) -> (gpui_kit::AnyWindowHandle, gpui_kit::Entity<Browser>) {
        open_window(bridge, runtime, answers_file, board, None, cx)
    }

    fn open_window(
        bridge: &Arc<Bridge>,
        runtime: &tokio::runtime::Runtime,
        answers_file: Option<PathBuf>,
        board: BoardData,
        icon_file: Option<PathBuf>,
        cx: &mut TestAppContext,
    ) -> (gpui_kit::AnyWindowHandle, gpui_kit::Entity<Browser>) {
        let (window, browser, _) = open_window_and_view(bridge, runtime, answers_file, board, icon_file, cx);
        (window, browser)
    }

    fn open_window_and_view(
        bridge: &Arc<Bridge>,
        runtime: &tokio::runtime::Runtime,
        answers_file: Option<PathBuf>,
        board: BoardData,
        icon_file: Option<PathBuf>,
        cx: &mut TestAppContext,
    ) -> (
        gpui_kit::AnyWindowHandle,
        gpui_kit::Entity<Browser>,
        gpui_kit::Entity<MainView>,
    ) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::ui::install(cx);
        });
        let extension_dir = PathBuf::from("/data/maverick/extension");
        let background = runtime.handle().clone();
        let browser = cx.update(|cx| {
            cx.new(|cx| Browser::new(bridge.clone(), background, Ok(extension_dir), answers_file, None, cx))
        });
        let shown = browser.clone();
        let (window, view) = cx.update(|cx| {
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                    Default::default(),
                    size(px(880.), px(620.)),
                ))),
                ..Default::default()
            };
            gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| {
                    let lists = crate::schedule::Lists {
                        board,
                        ..Default::default()
                    };
                    MainView::new(browser, lists, icon_file, false, window, cx)
                })
            })
            .expect("the test window opens")
        });
        (window, shown, view)
    }

    #[gpui_kit::test]
    fn a_picked_icon_is_saved_for_the_next_start(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let icon_file = dir.path().join("icon");
        let window = open_window_with_icon_file(icon_file.clone(), cx);

        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("Neon").is_none());
            window.click("pick-icon", cx);
            window.render_frame(cx);
            for name in ["Stealth", "Sunset", "Arctic", "Neon", "Crimson"] {
                assert!(window.try_find(name).is_some(), "the picker offers {name}");
            }
            window.click("Neon", cx);
            window.render_frame(cx);
            assert!(window.try_find("Neon").is_none(), "choosing an icon closes the picker");
        })
        .unwrap();
        assert_eq!(AppIcon::saved(Some(&icon_file)), AppIcon::Neon);
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
        let (window, browser) =
            open_window_with_answers(&bridge, &runtime, Some(answers_file.clone()), BoardData::default(), cx);

        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            // The window opens on the Jobs tab.
            window.click("show-tabs", cx);
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

    /// The board for postings named `titles`, none decided yet.
    fn board_of(titles: &[&str]) -> BoardData {
        let postings: Vec<Posting> = titles
            .iter()
            .enumerate()
            .map(|(id, title)| Posting {
                link: format!("https://jobs.ashbyhq.com/example-co/0f3b8231-3a32-478a-b994-3441f4bb94{id:02}"),
                found_at: "2026-11-20T12:00:00Z".parse().unwrap(),
                source: "Simplify".into(),
                company: "Example Co".into(),
                title: (*title).into(),
                location: None,
                email_subject: None,
                stage: Stage::Review,
                score: Some(80),
                variant: None,
                flags: None,
                notes: None,
                address: None,
                sponsorship: None,
            })
            .collect();
        BoardData::new(&postings, Vec::new(), &HashMap::new())
    }

    #[gpui_kit::test]
    fn a_funnel_pass_replaces_the_jobs_and_a_failed_pass_keeps_them(cx: &mut TestAppContext) {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let (window, _, view) = open_window_and_view(&Bridge::new(), &runtime, None, board_of(&["First"]), None, cx);

        let pass = |status: &str, board: Option<BoardData>| crate::schedule::Refreshed {
            status: status.into(),
            lists: board.map(|board| crate::schedule::Lists {
                board,
                ..Default::default()
            }),
        };
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(window.find("show-jobs").label(), Some("Jobs · 1"));
            view.update(cx, |view, cx| {
                view.refresh(
                    pass(
                        "Checked at 9:00 AM: 2 new postings",
                        Some(board_of(&["First", "Second"])),
                    ),
                    window,
                    cx,
                )
            });
            window.render_frame(cx);
            assert_eq!(window.find("show-jobs").label(), Some("Jobs · 2"));
            assert_eq!(
                window.find("funnel-status").label(),
                Some("Checked at 9:00 AM: 2 new postings")
            );

            view.update(cx, |view, cx| {
                view.refresh(pass("The 10:00 AM check failed: offline", None), window, cx)
            });
            window.render_frame(cx);
            assert_eq!(window.find("show-jobs").label(), Some("Jobs · 2"));
            assert_eq!(
                window.find("funnel-status").label(),
                Some("The 10:00 AM check failed: offline")
            );
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn the_window_opens_on_the_jobs_tab_and_the_bar_names_posts_not_outreach(cx: &mut TestAppContext) {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let (window, _, _) = open_window_and_view(&Bridge::new(), &runtime, None, board_of(&["First"]), None, cx);
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            // The Jobs tab is the board, whether or not Chrome is connected.
            assert_eq!(window.find("board-pending").label(), Some("Pending · 1"));
            for sort in ["sort-latest", "sort-score", "sort-outreach"] {
                assert!(window.try_find(sort).is_some(), "the Jobs tab sorts by {sort}");
            }
            assert!(window.try_find("show-approvals").is_none());
            assert!(window.try_find("show-outreach").is_none());
            assert_eq!(window.find("show-posts").label(), Some("Posts"));
            window.click("show-posts", cx);
            window.render_frame(cx);
            assert!(window.try_find("run-outreach").is_some());
            assert!(window.try_find("board-pending").is_none());
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
            // The window opens on the Jobs tab.
            window.click("show-tabs", cx);
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
            // The window opens on the Jobs tab.
            window.click("show-tabs", cx);
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
