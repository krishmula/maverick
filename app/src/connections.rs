//! The Connections view: the user's Google profiles, TickTick, the model and Chrome, in one place.

use std::collections::HashMap;
use std::path::PathBuf;

use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{h_flex, v_flex};
use gpui_kit::{
    AppContext as _, Context, Div, Entity, FontWeight, Hsla, InteractiveElement as _, IntoElement, ParentElement as _,
    Render, SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, TestSupportExt as _, Window, div,
    px,
};
use maverick_core::bridge::BrowserState;
use maverick_core::calendar::{self, Calendar};
use maverick_core::credentials::Password;
use maverick_core::google::{self, Account, Client, MOST_ACCOUNTS};
use maverick_core::ticktick::{self, Project, Settings};
use tokio::runtime::Handle;

use crate::browser::Browser;
use crate::ui::{self, Palette, Signal};

const DETAIL_WIDTH: f32 = 400.0;

/// Where TickTick stands.
#[derive(Clone, Debug, PartialEq)]
pub enum TickTick {
    Reading,
    NoToken,
    Failed(String),
    Ready(Vec<Project>),
}

pub struct Connections {
    browser: Entity<Browser>,
    background: Handle,
    accounts_file: Option<PathBuf>,
    accounts: Vec<Account>,
    /// `None` until the Keychain answers.
    client_stored: Option<bool>,
    selected: Option<String>,
    calendars: HashMap<String, Result<Vec<Calendar>, String>>,
    /// What is running now, such as a sign-in, shown so the user knows to look at the browser.
    working: Option<String>,
    problem: Option<String>,
    ticktick: TickTick,
    settings: Settings,
    claude: bool,
    label: Entity<InputState>,
    token: Entity<InputState>,
    _subscriptions: Vec<Subscription>,
}

impl Connections {
    pub fn new(browser: Entity<Browser>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let background = browser.read(cx).background();
        let label = cx.new(|cx| InputState::new(window, cx).placeholder("Name this profile, such as Work"));
        let token = cx.new(|cx| InputState::new(window, cx).placeholder("Paste your TickTick API token"));
        let _subscriptions = vec![
            cx.subscribe_in(&label, window, |this, input, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::Change) {
                    let name = input.read(cx).value().trim().to_owned();
                    this.rename(name, cx);
                }
            }),
            cx.subscribe_in(&token, window, |this, _, event: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    this.connect_ticktick(window, cx);
                }
            }),
            cx.observe(&browser, |_, _, cx| cx.notify()),
        ];
        Self {
            browser,
            background,
            accounts_file: google::accounts_path(),
            accounts: Vec::new(),
            client_stored: None,
            selected: None,
            calendars: HashMap::new(),
            working: None,
            problem: None,
            ticktick: TickTick::Reading,
            settings: Settings::current(),
            claude: false,
            label,
            token,
            _subscriptions,
        }
    }

    /// Reads every connection again.
    pub fn refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.reload_accounts(window, cx);
        self.settings = Settings::current();
        self.claude = maverick_core::model::claude_found();
        let client = self.background.spawn(Client::stored());
        let lists = self.background.spawn(async {
            match ticktick::stored_token().await? {
                Some(token) => ticktick::projects(&token).await.map(Some),
                None => Ok(None),
            }
        });
        cx.spawn(async move |this, cx| {
            let stored = matches!(client.await, Ok(Ok(Some(_))));
            let ticktick = match lists.await {
                Ok(Ok(Some(lists))) => TickTick::Ready(lists),
                Ok(Ok(None)) => TickTick::NoToken,
                Ok(Err(error)) => TickTick::Failed(error),
                Err(error) => TickTick::Failed(error.to_string()),
            };
            this.update(cx, |this, cx| {
                this.client_stored = Some(stored);
                this.ticktick = ticktick;
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn reload_accounts(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let read = self.accounts_file.as_deref().map(google::accounts).transpose();
        match read {
            Ok(accounts) => self.accounts = accounts.unwrap_or_default(),
            Err(error) => self.problem = Some(error),
        }
        let still_there = self
            .selected
            .as_ref()
            .is_some_and(|email| self.accounts.iter().any(|account| &account.email == email));
        if !still_there {
            let first = self.accounts.first().map(|account| account.email.clone());
            self.select(first, window, cx);
        }
    }

    fn select(&mut self, email: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        self.selected = email.clone();
        let label = self
            .account(email.as_deref())
            .and_then(|account| account.label.clone())
            .unwrap_or_default();
        self.label.update(cx, |input, cx| input.set_value(label, window, cx));
        if let Some(email) = email
            && !self.calendars.contains_key(&email)
        {
            self.load_calendars(email, cx);
        }
        cx.notify();
    }

    fn account(&self, email: Option<&str>) -> Option<&Account> {
        let email = email?;
        self.accounts.iter().find(|account| account.email == email)
    }

    fn load_calendars(&mut self, email: String, cx: &mut Context<Self>) {
        let asked = email.clone();
        let read = self.background.spawn(async move { calendar::calendars(&asked).await });
        cx.spawn(async move |this, cx| {
            let calendars = read.await.map_err(|error| error.to_string()).and_then(|read| read);
            this.update(cx, |this, cx| {
                this.calendars.insert(email, calendars);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Changes the selected account and saves it.
    fn change(&mut self, email: &str, change: impl FnOnce(&mut Account), cx: &mut Context<Self>) {
        let Some(file) = self.accounts_file.clone() else {
            return;
        };
        match google::update(&file, email, change) {
            Ok(_) => self.accounts = google::accounts(&file).unwrap_or_default(),
            Err(error) => self.problem = Some(error),
        }
        cx.notify();
    }

    fn rename(&mut self, name: String, cx: &mut Context<Self>) {
        let Some(email) = self.selected.clone() else {
            return;
        };
        let label = (!name.is_empty()).then_some(name);
        if self.account(Some(&email)).map(|account| &account.label) == Some(&label) {
            return;
        }
        self.change(&email, |account| account.label = label, cx);
    }

    fn toggle_calendar(&mut self, email: &str, id: String, cx: &mut Context<Self>) {
        let primary = match self.calendars.get(email) {
            Some(Ok(calendars)) => calendars
                .iter()
                .find(|calendar| calendar.primary)
                .map(|calendar| calendar.id.clone()),
            _ => None,
        };
        self.change(
            email,
            move |account| {
                if account.calendars.is_empty() {
                    account.calendars.extend(primary);
                }
                if let Some(at) = account.calendars.iter().position(|chosen| *chosen == id) {
                    account.calendars.remove(at);
                } else {
                    account.calendars.push(id);
                }
            },
            cx,
        );
    }

    /// Signs an account in through the browser: a new one, or `email` again.
    fn sign_in(&mut self, email: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(file) = self.accounts_file.clone() else {
            return;
        };
        self.working = Some(match &email {
            Some(email) => format!("signing in to {email} in your browser"),
            None => "signing in to a new account in your browser".to_owned(),
        });
        self.problem = None;
        cx.notify();
        let signed = self.background.spawn(async move {
            let client = Client::stored()
                .await?
                .ok_or("Google needs an OAuth client first; see the steps under Google")?;
            google::connect(&client, &file, |url| {
                let _ = std::process::Command::new("/usr/bin/open").arg(url).status();
            })
            .await
        });
        cx.spawn_in(window, async move |this, cx| {
            let signed = signed
                .await
                .map_err(|error| error.to_string())
                .and_then(|signed| signed);
            this.update_in(cx, |this, window, cx| {
                this.working = None;
                match signed {
                    Ok(account) => {
                        this.calendars.remove(&account.email);
                        this.reload_accounts(window, cx);
                        this.select(Some(account.email), window, cx);
                    }
                    Err(error) => this.problem = Some(error),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn disconnect(&mut self, email: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(file) = self.accounts_file.clone() else {
            return;
        };
        self.working = Some(format!("disconnecting {email}"));
        cx.notify();
        let gone = email.clone();
        let done = self
            .background
            .spawn(async move { google::disconnect(&file, &gone).await });
        cx.spawn_in(window, async move |this, cx| {
            let done = done.await.map_err(|error| error.to_string()).and_then(|done| done);
            this.update_in(cx, |this, window, cx| {
                this.working = None;
                if let Err(error) = done {
                    this.problem = Some(error);
                }
                this.calendars.remove(&email);
                this.reload_accounts(window, cx);
            })
            .ok();
        })
        .detach();
    }

    fn connect_ticktick(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let token = self.token.read(cx).value().trim().to_owned();
        if token.is_empty() {
            return;
        }
        self.token.update(cx, |input, cx| input.set_value("", window, cx));
        self.ticktick = TickTick::Reading;
        cx.notify();
        let checked = self.background.spawn(async move {
            let token = Password::new(token);
            // A token TickTick refuses is not worth keeping.
            let lists = ticktick::projects(&token).await?;
            ticktick::save_token(&token).await?;
            Ok::<_, String>(lists)
        });
        cx.spawn(async move |this, cx| {
            let ticktick = match checked.await {
                Ok(Ok(lists)) => TickTick::Ready(lists),
                Ok(Err(error)) => TickTick::Failed(error),
                Err(error) => TickTick::Failed(error.to_string()),
            };
            this.update(cx, |this, cx| {
                this.ticktick = ticktick;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn forget_ticktick(&mut self, cx: &mut Context<Self>) {
        let forgot = self.background.spawn(ticktick::forget_token());
        cx.spawn(async move |this, cx| {
            let ticktick = match forgot.await {
                Ok(Ok(())) => TickTick::NoToken,
                Ok(Err(error)) => TickTick::Failed(error),
                Err(error) => TickTick::Failed(error.to_string()),
            };
            this.update(cx, |this, cx| {
                this.ticktick = ticktick;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn change_settings(&mut self, change: impl FnOnce(&mut Settings), cx: &mut Context<Self>) {
        change(&mut self.settings);
        if let Some(file) = ticktick::settings_path()
            && let Err(error) = self.settings.save(&file)
        {
            self.problem = Some(error);
        }
        cx.notify();
    }

    #[cfg(test)]
    pub fn show(&mut self, accounts: Vec<Account>, ticktick: TickTick, window: &mut Window, cx: &mut Context<Self>) {
        self.accounts_file = None;
        self.accounts = accounts;
        self.client_stored = Some(true);
        self.ticktick = ticktick;
        let first = self.accounts.first().map(|account| account.email.clone());
        self.selected = first.clone();
        if let Some(email) = first {
            self.calendars.insert(email, Ok(Vec::new()));
        }
        self.select(self.selected.clone(), window, cx);
    }
}

impl Render for Connections {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::of(cx);
        h_flex()
            .size_full()
            .items_start()
            .child(
                v_flex()
                    .id("connections")
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .overflow_y_scroll()
                    .px(px(28.))
                    .py(px(20.))
                    .gap(px(22.))
                    .border_r_1()
                    .border_color(palette.line)
                    .children(
                        self.working
                            .clone()
                            .map(|working| ui::caps(working).text_color(palette.signal(Signal::Model))),
                    )
                    .children(
                        self.problem
                            .clone()
                            .map(|problem| ui::serif(problem).text_color(palette.signal(Signal::Failed))),
                    )
                    .child(self.google(&palette, cx))
                    .child(rule(&palette))
                    .child(self.ticktick(&palette, cx))
                    .child(rule(&palette))
                    .child(self.others(&palette, cx)),
            )
            .child(self.detail(&palette, cx))
    }
}

impl Connections {
    fn google(&self, palette: &Palette, cx: &mut Context<Self>) -> Div {
        let count = self.accounts.len();
        let full = count >= MOST_ACCOUNTS;
        let add = (!full && self.client_stored == Some(true) && self.working.is_none()).then(|| {
            ui::button(
                "add-account",
                "Add a Google account",
                ui::primary_face("+ add account", None, palette),
            )
            .on_click(cx.listener(|this, _, window, cx| this.sign_in(None, window, cx)))
        });
        let mut section = v_flex().gap(px(8.)).child(
            h_flex()
                .justify_between()
                .child(heading(
                    "google-count",
                    format!("Google · {count} of {MOST_ACCOUNTS}"),
                    palette.muted,
                ))
                .children(add),
        );
        if self.client_stored == Some(false) {
            section = section.child(
                ui::serif(
                    "Google needs your own OAuth client first. Create a Desktop app client in the Google Cloud \
                     console, then run `maverick google client <client id>` with the secret on standard input. \
                     The README has the steps.",
                )
                .text_color(palette.muted)
                .max_w(px(620.)),
            );
        }
        for account in &self.accounts {
            section = section.child(self.profile_row(account, palette, cx));
        }
        if !full {
            let left = MOST_ACCOUNTS - count;
            let words = if left == 1 {
                "1 more account can be connected".to_owned()
            } else {
                format!("{left} more accounts can be connected")
            };
            section = section.child(
                div()
                    .px(px(14.))
                    .py(px(10.))
                    .border_1()
                    .border_dashed()
                    .border_color(palette.line)
                    .child(ui::caps(words).text_color(palette.faint)),
            );
        }
        section
    }

    fn profile_row(&self, account: &Account, palette: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let selected = self.selected.as_deref() == Some(&account.email);
        let done = palette.signal(Signal::Done);
        let mut uses = Vec::new();
        if account.can(google::CALENDAR_READ) || account.can(google::CALENDAR_EVENTS) {
            uses.push(ui::chip(Signal::Done, "calendar", palette));
        }
        if account.adds_blocks {
            uses.push(ui::boxed("new blocks go here", palette.ink));
        }
        let (status, signal) = if account.needs_sign_in() {
            ("sign in again", Signal::Yours)
        } else {
            ("connected", Signal::Done)
        };
        let name = account.label.clone().unwrap_or_else(|| "unnamed".to_owned());
        let email = account.email.clone();
        let mut row = h_flex()
            .id(SharedString::from(format!("profile-{}", account.email)))
            .test_support()
            .role(gpui_kit::Role::Button)
            .aria_label(format!("{} · {} · {status}", account.name(), account.email))
            .relative()
            .gap(px(14.))
            .px(px(14.))
            .py(px(12.))
            .border_1()
            .border_color(palette.line)
            .bg(palette.background.opacity(0.85))
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, window, cx| this.select(Some(email.clone()), window, cx)));
        if selected {
            row = row.children(ui::brackets(palette.ink));
        }
        row.child(
            div()
                .w(px(110.))
                .flex_none()
                .font_family(ui::DOTS)
                .font_weight(FontWeight::BLACK)
                .text_size(px(18.))
                .text_color(if account.label.is_some() {
                    palette.ink
                } else {
                    palette.faint
                })
                .child(name.to_uppercase()),
        )
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap(px(4.))
                .child(ui::serif(account.email.clone()))
                .child(h_flex().gap(px(6.)).children(uses)),
        )
        .child(
            h_flex()
                .flex_none()
                .gap(px(6.))
                .child(ui::square(palette.signal(signal)))
                .child(ui::caps(status).text_color(if signal == Signal::Done {
                    done
                } else {
                    palette.signal(signal)
                })),
        )
    }

    fn ticktick(&self, palette: &Palette, cx: &mut Context<Self>) -> Div {
        let (status, signal) = match &self.ticktick {
            TickTick::Reading => ("reading".to_owned(), Signal::Model),
            TickTick::NoToken => ("not connected".to_owned(), Signal::Yours),
            TickTick::Failed(_) => ("could not connect".to_owned(), Signal::Failed),
            TickTick::Ready(lists) => (format!("connected · {} lists", lists.len()), Signal::Done),
        };
        let mut section = v_flex().gap(px(10.)).child(
            h_flex()
                .justify_between()
                .child(heading("ticktick-heading", "TickTick".to_owned(), palette.muted))
                .child(status_line("ticktick-status", &status, palette.signal(signal))),
        );
        match &self.ticktick {
            TickTick::NoToken | TickTick::Failed(_) => {
                if let TickTick::Failed(error) = &self.ticktick {
                    section = section.child(ui::serif(error.clone()).text_color(palette.signal(Signal::Failed)));
                }
                section = section
                    .child(
                        ui::serif("In TickTick on the web, open Settings, then Account, then API Token. Create a token and paste it here.")
                            .text_color(palette.muted)
                            .max_w(px(620.)),
                    )
                    .child(
                        h_flex()
                            .gap(px(10.))
                            .child(div().flex_1().max_w(px(420.)).child(Input::new(&self.token).id("ticktick-token").mask_toggle()))
                            .child(
                                ui::button("connect-ticktick", "Connect TickTick", ui::primary_face("connect", Some("enter"), palette))
                                    .on_click(cx.listener(|this, _, window, cx| this.connect_ticktick(window, cx))),
                            ),
                    );
            }
            TickTick::Reading => {}
            TickTick::Ready(lists) => {
                let mut all = vec![Project {
                    id: "inbox".into(),
                    name: "Inbox".into(),
                    closed: false,
                }];
                all.extend(lists.iter().filter(|list| !list.closed).cloned());
                let default = self.settings.default_list.clone().unwrap_or_else(|| "inbox".to_owned());
                let rows = all.into_iter().enumerate().map(|(index, list)| {
                    let is_default = list.id == default;
                    let shown = self.settings.shows(&list.id);
                    let (pick, show) = (list.id.clone(), list.id.clone());
                    h_flex()
                        .gap(px(14.))
                        .py(px(3.))
                        .child(
                            ui::button(
                                ("default-list", index),
                                format!("New tasks go to {}", list.name),
                                radio(is_default, palette),
                            )
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let pick = (pick != "inbox").then(|| pick.clone());
                                this.change_settings(|settings| settings.default_list = pick, cx);
                            })),
                        )
                        .child(
                            ui::button(
                                ("show-list", index),
                                format!("Mav shows {}", list.name),
                                checkbox(shown, palette),
                            )
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let id = show.clone();
                                this.change_settings(|settings| settings.set_shown(&id, !shown), cx);
                            })),
                        )
                        .child(ui::serif(list.name.clone()).text_size(px(14.)))
                });
                section = section
                    .child(ui::caps("circle: new tasks go here · square: mav shows the list").text_color(palette.faint))
                    .children(rows)
                    .child(
                        h_flex().pt(px(4.)).child(
                            ui::button(
                                "forget-ticktick",
                                "Disconnect TickTick",
                                ui::quiet_face("disconnect", palette),
                            )
                            .on_click(cx.listener(|this, _, _, cx| this.forget_ticktick(cx))),
                        ),
                    );
            }
        }
        section
    }

    fn others(&self, palette: &Palette, cx: &mut Context<Self>) -> Div {
        let (model, model_signal) = if self.claude {
            ("claude found · sonnet", Signal::Done)
        } else {
            ("claude not found · install claude code and sign in", Signal::Yours)
        };
        let (chrome, chrome_signal) = match self.browser.read(cx).state() {
            BrowserState::Connected { tabs } => (format!("connected · {} tabs", tabs.len()), Signal::Done),
            BrowserState::Disconnected => ("not connected".to_owned(), Signal::Yours),
        };
        v_flex()
            .gap(px(12.))
            .child(
                h_flex()
                    .justify_between()
                    .child(ui::caps("model").text_color(palette.muted))
                    .child(status_line("model-status", model, palette.signal(model_signal))),
            )
            .child(
                h_flex()
                    .justify_between()
                    .child(ui::caps("chrome").text_color(palette.muted))
                    .child(status_line("chrome-status", &chrome, palette.signal(chrome_signal))),
            )
    }

    fn detail(&self, palette: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let pane = v_flex()
            .id("profile-detail")
            .flex_none()
            .w(px(DETAIL_WIDTH))
            .h_full()
            .overflow_y_scroll()
            .px(px(20.))
            .py(px(20.))
            .gap(px(14.));
        let Some(account) = self.account(self.selected.as_deref()).cloned() else {
            return pane.child(ui::serif("Connect a Google account to set up its profile.").text_color(palette.faint));
        };
        let email = account.email.clone();
        let calendars: Div = match self.calendars.get(&email) {
            None => v_flex().child(ui::caps("reading calendars").text_color(palette.faint)),
            Some(Err(error)) => v_flex().child(
                ui::serif(error.clone())
                    .text_size(px(13.))
                    .text_color(palette.signal(Signal::Failed)),
            ),
            Some(Ok(calendars)) => {
                v_flex()
                    .gap(px(4.))
                    .children(calendars.iter().enumerate().map(|(index, calendar)| {
                        let read = if account.calendars.is_empty() {
                            calendar.primary
                        } else {
                            account.calendars.contains(&calendar.id)
                        };
                        let (owner, id) = (email.clone(), calendar.id.clone());
                        h_flex()
                            .gap(px(10.))
                            .child(
                                ui::button(
                                    ("read-calendar", index),
                                    format!("Mav reads {}", calendar.name),
                                    checkbox(read, palette),
                                )
                                .on_click(
                                    cx.listener(move |this, _, _, cx| this.toggle_calendar(&owner, id.clone(), cx)),
                                ),
                            )
                            .child(
                                ui::serif(if calendar.primary {
                                    format!("{} (primary)", calendar.name)
                                } else {
                                    calendar.name.clone()
                                })
                                .text_size(px(14.)),
                            )
                    }))
            }
        };
        let can_add = account.can(google::CALENDAR_EVENTS);
        let marked = email.clone();
        let blocks = h_flex()
            .gap(px(10.))
            .child(
                ui::button(
                    "adds-blocks",
                    "New blocks go to this profile",
                    radio(account.adds_blocks, palette),
                )
                .on_click(
                    cx.listener(move |this, _, _, cx| this.change(&marked, |account| account.adds_blocks = true, cx)),
                ),
            )
            .child(ui::serif("New blocks go to this profile").text_size(px(14.)))
            .children((!can_add).then(|| ui::caps("needs sign in").text_color(palette.signal(Signal::Yours))));
        let (again, gone) = (email.clone(), email.clone());
        pane.child(ui::caps("profile").text_color(palette.muted))
            .child(Input::new(&self.label).id("profile-name").aria_label("Profile name"))
            .child(ui::caps(account.email.clone()).text_color(palette.faint))
            .child(rule(palette))
            .child(ui::caps("calendars mav reads").text_color(palette.muted))
            .child(calendars)
            .child(blocks)
            .child(rule(palette))
            .children(account.needs_sign_in().then(|| {
                ui::serif("Maverick now asks for more than this account granted, such as adding events. Sign in again to grant it.")
                    .text_size(px(13.))
                    .text_color(palette.signal(Signal::Yours))
            }))
            .child(
                h_flex()
                    .gap(px(8.))
                    .child(
                        ui::button("sign-in-again", "Sign in again", ui::quiet_face("sign in again", palette))
                            .on_click(cx.listener(move |this, _, window, cx| this.sign_in(Some(again.clone()), window, cx))),
                    )
                    .child(
                        ui::button("disconnect", "Disconnect", ui::boxed("disconnect", palette.signal(Signal::Failed)).py(px(1.)))
                            .on_click(cx.listener(move |this, _, window, cx| this.disconnect(gone.clone(), window, cx))),
                    ),
            )
    }
}

fn rule(palette: &Palette) -> Div {
    div().h(px(1.)).w_full().bg(palette.line)
}

fn heading(id: &'static str, text: String, color: Hsla) -> impl IntoElement {
    div()
        .id(id)
        .test_support()
        .role(gpui_kit::Role::Label)
        .aria_label(text.clone())
        .child(ui::caps(text).text_color(color))
}

fn status_line(id: &'static str, text: &str, color: Hsla) -> impl IntoElement {
    div()
        .id(id)
        .test_support()
        .role(gpui_kit::Role::Label)
        .aria_label(text.to_owned())
        .child(
            h_flex()
                .gap(px(6.))
                .child(ui::square(color))
                .child(ui::caps(text).text_color(color)),
        )
}

fn checkbox(on: bool, palette: &Palette) -> Div {
    let mark = div()
        .size(px(11.))
        .border_1()
        .border_color(if on { palette.ink } else { palette.muted });
    if on { mark.bg(palette.ink) } else { mark }
}

fn radio(on: bool, palette: &Palette) -> Div {
    let ring = div()
        .size(px(11.))
        .rounded_full()
        .border_1()
        .border_color(if on { palette.ink } else { palette.muted })
        .flex()
        .items_center()
        .justify_center();
    if on {
        ring.child(div().size(px(5.)).rounded_full().bg(palette.ink))
    } else {
        ring
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{AppContext as _, TestAppContext, WindowOptions};
    use maverick_core::google::{Account, CALENDAR_READ, SCOPES};
    use maverick_core::ticktick::Project;

    use super::{Connections, TickTick};

    #[gpui_kit::test]
    fn profiles_show_their_names_status_and_the_room_left(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::ui::install(cx);
        });
        let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let bridge = maverick_core::bridge::Bridge::new();
        let background = runtime.handle().clone();
        let browser = cx.update(|cx| {
            cx.new(|cx| crate::browser::Browser::new(bridge, background, Ok("/x".into()), None, None, cx))
        });
        let (window, view) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Connections::new(browser, window, cx))
            })
            .unwrap()
        });
        let work = Account {
            email: "w@example.com".into(),
            label: Some("Work".into()),
            scopes: SCOPES.iter().map(|scope| (*scope).to_owned()).collect(),
            ..Account::default()
        };
        let old = Account {
            email: "o@example.com".into(),
            scopes: vec![CALENDAR_READ.into()],
            ..Account::default()
        };
        let lists = vec![Project {
            id: "p1".into(),
            name: "Job search".into(),
            closed: false,
        }];
        cx.update_window(window, |_, window, cx| {
            view.update(cx, |view, cx| {
                view.show(vec![work, old], TickTick::Ready(lists), window, cx)
            });
            window.render_frame(cx);
            assert_eq!(window.find("google-count").label(), Some("Google · 2 of 5"));
            assert_eq!(
                window.find("profile-w@example.com").label(),
                Some("Work · w@example.com · connected")
            );
            assert_eq!(
                window.find("profile-o@example.com").label(),
                Some("o@example.com · o@example.com · sign in again")
            );
            assert_eq!(window.find("ticktick-status").label(), Some("connected · 1 lists"));
            assert!(window.try_find("add-account").is_some());
        })
        .unwrap();
    }
}
