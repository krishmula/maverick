//! The Mav view: talk to Mav on the left, see the calendar and TickTick on the right.
//!
//! Mav only proposes. Each proposal is a card, and nothing is written until the user confirms it.

use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{h_flex, v_flex};
use gpui_kit::{
    AppContext as _, Context, Div, Entity, InteractiveElement as _, IntoElement, ParentElement as _, Render,
    StatefulInteractiveElement as _, Styled as _, Subscription, TestSupportExt as _, Window, div, px,
};
use jiff::ZonedDateTime;
use maverick_core::calendar::{self, Event, Slot, Stretch};
use maverick_core::mav::{self, Action, Context as Seen, Speaker};
use maverick_core::ticktick::{self, Board, Change, Entry, NewTask, Priority, Project};
use serde::{Deserialize, Serialize};
use tokio::runtime::Handle;

use crate::ui::{self, Palette, Signal};

const RAIL_WIDTH: f32 = 380.0;

/// How the side panel looks, kept between runs in `mav-view.json`.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct View {
    /// True when the side panel is folded into a strip.
    #[serde(default)]
    pub collapsed: bool,
    /// True when tasks are grouped by date instead of priority.
    #[serde(default)]
    pub by_date: bool,
    /// True when the calendar shows the coming week instead of one day.
    #[serde(default)]
    pub week: bool,
}

impl View {
    /// Tests never read or write the user's file.
    fn file() -> Option<std::path::PathBuf> {
        if cfg!(test) {
            return None;
        }
        maverick_core::data_dir().map(|dir| dir.join("mav-view.json"))
    }

    fn load() -> Self {
        Self::file()
            .and_then(|file| std::fs::read_to_string(file).ok())
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    fn save(self) {
        if let Some(file) = Self::file() {
            let _ = std::fs::write(file, serde_json::to_string(&self).expect("a view always serializes"));
        }
    }
}

/// What Mav has read from TickTick and the calendar.
#[derive(Clone, Debug)]
pub enum Loaded {
    /// Not read yet: the view has not been shown.
    Not,
    Loading,
    Failed(String),
    Ready {
        seen: Box<Seen>,
        at: String,
    },
}

/// Where one proposal stands.
#[derive(Clone, Debug, PartialEq)]
pub enum Card {
    Proposed,
    Adding,
    Done(String),
    Failed(String),
    Skipped,
}

/// One entry in the conversation.
#[derive(Clone, Debug)]
pub enum Item {
    Said(Speaker, String),
    Proposal(Box<Action>, Card),
    /// A proposal Maverick could not use, and why.
    Refused(String),
}

pub struct Mav {
    loaded: Loaded,
    items: Vec<Item>,
    /// True while Mav works out an answer.
    thinking: bool,
    input: Entity<InputState>,
    background: Handle,
    view: View,
    /// Tasks the user ticked, shown as done until the next read drops them.
    finished: Vec<String>,
    new_task: Entity<InputState>,
    new_priority: Priority,
    /// Why the last change from the side panel failed.
    rail_problem: Option<String>,
    /// The day the calendar shows, in days from today: 0 to 6.
    day: i64,
    /// The free stretch the user is naming a block for.
    blocking: Option<Slot>,
    block_title: Entity<InputState>,
    /// The event whose delete was clicked once and waits for a second click.
    deleting: Option<String>,
    _typed: Subscription,
    _added: Subscription,
    _blocked: Subscription,
}

impl Mav {
    pub fn new(background: Handle, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Tell Mav what to remember or plan"));
        let _typed = cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| {
            if let InputEvent::PressEnter { .. } = event {
                this.send(window, cx);
            }
        });
        let new_task = cx.new(|cx| InputState::new(window, cx).placeholder("Add a task"));
        let _added = cx.subscribe_in(&new_task, window, |this, _, event: &InputEvent, window, cx| {
            if let InputEvent::PressEnter { .. } = event {
                this.add_task(window, cx);
            }
        });
        let block_title = cx.new(|cx| InputState::new(window, cx).placeholder("Name the block, then press Enter"));
        let _blocked = cx.subscribe_in(&block_title, window, |this, _, event: &InputEvent, window, cx| {
            if let InputEvent::PressEnter { .. } = event {
                this.add_block(window, cx);
            }
        });
        Self {
            loaded: Loaded::Not,
            items: Vec::new(),
            thinking: false,
            input,
            background,
            view: View::load(),
            finished: Vec::new(),
            new_task,
            new_priority: Priority::Medium,
            rail_problem: None,
            day: 0,
            blocking: None,
            block_title,
            deleting: None,
            _typed,
            _added,
            _blocked,
        }
    }

    /// Runs one TickTick write from the side panel, then reads everything again.
    fn write(
        &mut self,
        work: impl std::future::Future<Output = Result<(), String>> + Send + 'static,
        cx: &mut Context<Self>,
    ) {
        self.rail_problem = None;
        let done = self.background.spawn(work);
        cx.spawn(async move |this, cx| {
            let done = done.await.map_err(|error| error.to_string()).and_then(|done| done);
            this.update(cx, |this, cx| {
                if let Err(error) = done {
                    this.rail_problem = Some(error);
                    this.finished.clear();
                }
                this.load(cx);
            })
            .ok();
        })
        .detach();
    }

    /// Finishes a task in one click. It shows as done until the next read drops it.
    fn finish(&mut self, entry: &Entry, cx: &mut Context<Self>) {
        if self.finished.contains(&entry.id) {
            return;
        }
        self.finished.push(entry.id.clone());
        cx.notify();
        let (list, id) = (entry.list_id.clone(), entry.id.clone());
        self.write(
            async move {
                let token = ticktick::stored_token().await?.ok_or("TickTick is not connected")?;
                ticktick::complete(&token, &list, &id).await
            },
            cx,
        );
    }

    /// Moves a task to the next priority: high, medium, low, none, then high again.
    fn cycle_priority(&mut self, entry: &Entry, cx: &mut Context<Self>) {
        let next = entry.priority.next();
        if let Loaded::Ready { seen, .. } = &mut self.loaded
            && let Some(shown) = seen.tasks.iter_mut().find(|task| task.id == entry.id)
        {
            shown.priority = next;
        }
        cx.notify();
        let (list, id) = (entry.list_id.clone(), entry.id.clone());
        self.write(
            async move {
                let token = ticktick::stored_token().await?.ok_or("TickTick is not connected")?;
                let change = Change {
                    priority: Some(next),
                    ..Change::default()
                };
                ticktick::change(&token, &id, &list, &change, &jiff::tz::TimeZone::system()).await
            },
            cx,
        );
    }

    /// Adds the typed task to the default list, with the chosen priority.
    fn add_task(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let title = self.new_task.read(cx).value().trim().to_owned();
        if title.is_empty() {
            return;
        }
        self.new_task.update(cx, |input, cx| input.set_value("", window, cx));
        let list_id = ticktick::Settings::current()
            .default_list
            .unwrap_or_else(|| "inbox".to_owned());
        let task = NewTask {
            title,
            list_id,
            due: None,
            remind_before: None,
            priority: self.new_priority,
        };
        self.write(
            async move {
                let token = ticktick::stored_token().await?.ok_or("TickTick is not connected")?;
                ticktick::create(&token, &task, &jiff::tz::TimeZone::system())
                    .await
                    .map(drop)
            },
            cx,
        );
    }

    /// Starts naming a block for a free stretch.
    fn start_block(&mut self, slot: Slot, window: &mut Window, cx: &mut Context<Self>) {
        self.blocking = Some(slot);
        self.block_title.update(cx, |input, cx| {
            input.set_value("", window, cx);
            input.focus(window, cx);
        });
        cx.notify();
    }

    /// Adds the named block for the chosen free stretch, to the profile marked for new blocks.
    fn add_block(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let title = self.block_title.read(cx).value().trim().to_owned();
        let Some(slot) = self.blocking.take() else {
            return;
        };
        if title.is_empty() {
            self.blocking = Some(slot);
            return;
        }
        self.block_title.update(cx, |input, cx| input.set_value("", window, cx));
        cx.notify();
        self.write(
            async move { calendar::add(&title, &slot.start, &slot.end, None).await.map(drop) },
            cx,
        );
    }

    /// Deletes an event on the second click; the first only asks.
    fn delete_event(&mut self, event: &Event, cx: &mut Context<Self>) {
        if self.deleting.as_deref() != Some(&event.source.id) {
            self.deleting = Some(event.source.id.clone());
            cx.notify();
            return;
        }
        self.deleting = None;
        let event = event.clone();
        self.write(async move { calendar::delete(&event).await }, cx);
    }

    fn show_day(&mut self, day: i64, cx: &mut Context<Self>) {
        self.day = day.clamp(0, 6);
        self.blocking = None;
        self.deleting = None;
        cx.notify();
    }

    fn change_view(&mut self, change: impl FnOnce(&mut View), cx: &mut Context<Self>) {
        change(&mut self.view);
        self.view.save();
        cx.notify();
    }

    pub fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.input.update(cx, |input, cx| input.focus(window, cx));
    }

    /// Reads TickTick and the calendar again. What is on screen stays until the new read arrives.
    pub fn load(&mut self, cx: &mut Context<Self>) {
        if matches!(self.loaded, Loaded::Not | Loaded::Failed(_)) {
            self.loaded = Loaded::Loading;
            cx.notify();
        }
        let read = self.background.spawn(Seen::read(ZonedDateTime::now()));
        cx.spawn(async move |this, cx| {
            let loaded = match read.await {
                Ok(Ok(seen)) => ready(seen),
                Ok(Err(error)) => Loaded::Failed(error),
                Err(error) => Loaded::Failed(error.to_string()),
            };
            this.update(cx, |this, cx| this.set(loaded, cx)).ok();
        })
        .detach();
    }

    pub fn set(&mut self, loaded: Loaded, cx: &mut Context<Self>) {
        self.loaded = loaded;
        cx.notify();
    }

    /// The tasks that need the user now: overdue and due today.
    pub fn pressing(&self) -> Option<usize> {
        match &self.loaded {
            Loaded::Ready { seen, .. } => Some(seen.agenda.overdue.len() + seen.agenda.today.len()),
            _ => None,
        }
    }

    /// The lists Mav last read, for proposals made outside a conversation.
    pub fn lists(&self) -> Vec<Project> {
        match &self.loaded {
            Loaded::Ready { seen, .. } => seen.lists.clone(),
            _ => Vec::new(),
        }
    }

    /// Sends what the user typed to Mav.
    fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let message = self.input.read(cx).value().trim().to_owned();
        if message.is_empty() || self.thinking {
            return;
        }
        self.input.update(cx, |input, cx| input.set_value("", window, cx));
        let said: Vec<(Speaker, String)> = self
            .items
            .iter()
            .filter_map(|item| match item {
                Item::Said(speaker, text) => Some((*speaker, text.clone())),
                _ => None,
            })
            .collect();
        self.items.push(Item::Said(Speaker::User, message.clone()));
        self.thinking = true;
        cx.notify();
        let asked = self.background.spawn(async move {
            // A fresh read, so Mav plans around what is true now.
            let seen = Seen::read(ZonedDateTime::now()).await?;
            let reply = mav::ask(&message, &said, &seen).await?;
            Ok::<_, String>((seen, reply))
        });
        cx.spawn(async move |this, cx| {
            let answered = asked
                .await
                .map_err(|error| error.to_string())
                .and_then(|answered| answered);
            this.update(cx, |this, cx| {
                this.thinking = false;
                match answered {
                    Ok((seen, reply)) => {
                        this.loaded = ready(seen);
                        this.answer(reply, cx);
                    }
                    Err(error) => this.items.push(Item::Refused(format!("Mav could not answer: {error}"))),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub fn answer(&mut self, reply: mav::Reply, cx: &mut Context<Self>) {
        if !reply.text.is_empty() {
            self.items.push(Item::Said(Speaker::Mav, reply.text));
        }
        for proposal in reply.proposals {
            self.items.push(match proposal {
                Ok(action) => Item::Proposal(Box::new(action), Card::Proposed),
                Err(why) => Item::Refused(why),
            });
        }
        cx.notify();
    }

    /// Offers a proposal Mav came up with itself, such as a follow-up after an application.
    pub fn suggest(&mut self, why: String, action: Action, cx: &mut Context<Self>) {
        self.items.push(Item::Said(Speaker::Mav, why));
        self.items.push(Item::Proposal(Box::new(action), Card::Proposed));
        cx.notify();
    }

    fn confirm(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(Item::Proposal(action, card)) = self.items.get_mut(index) else {
            return;
        };
        if !matches!(card, Card::Proposed | Card::Failed(_)) {
            return;
        }
        *card = Card::Adding;
        cx.notify();
        let action = (**action).clone();
        let added = self.background.spawn(async move { action.carry_out().await });
        cx.spawn(async move |this, cx| {
            let card = match added.await {
                Ok(Ok(done)) => Card::Done(done),
                Ok(Err(error)) => Card::Failed(error),
                Err(error) => Card::Failed(error.to_string()),
            };
            this.update(cx, |this, cx| {
                let done = matches!(card, Card::Done(_));
                if let Some(Item::Proposal(_, shown)) = this.items.get_mut(index) {
                    *shown = card;
                }
                if done {
                    this.load(cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn skip(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(Item::Proposal(_, card)) = self.items.get_mut(index)
            && matches!(card, Card::Proposed | Card::Failed(_))
        {
            *card = Card::Skipped;
            cx.notify();
        }
    }

    #[cfg(test)]
    pub fn items(&self) -> &[Item] {
        &self.items
    }
}

fn ready(seen: Seen) -> Loaded {
    let at = seen.now.strftime("%-I:%M %p").to_string();
    Loaded::Ready {
        seen: Box::new(seen),
        at,
    }
}

impl Render for Mav {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::of(cx);
        h_flex()
            .size_full()
            .items_start()
            .child(self.chat(&palette, cx))
            .child(if self.view.collapsed {
                self.strip(&palette, cx).into_any_element()
            } else {
                self.rail(&palette, cx).into_any_element()
            })
    }
}

impl Mav {
    fn chat(&self, palette: &Palette, cx: &mut Context<Self>) -> Div {
        let intro = "Tell Mav what to remember or plan. It proposes TickTick tasks and calendar blocks, \
                     and adds each one only when you confirm it.";
        let thread = v_flex()
            .id("mav-thread")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px(px(28.))
            .py(px(20.))
            .gap(px(16.))
            .children(
                self.items
                    .is_empty()
                    .then(|| ui::serif(intro).text_color(palette.faint).max_w(px(520.))),
            )
            .children(
                self.items
                    .iter()
                    .enumerate()
                    .map(|(index, item)| self.item(index, item, palette, cx)),
            )
            .children(
                self.thinking
                    .then(|| ui::caps("mav is thinking").text_color(palette.signal(Signal::Model))),
            );
        v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .border_r_1()
            .border_color(palette.line)
            .child(thread)
            .child(
                h_flex()
                    .flex_none()
                    .gap(px(12.))
                    .px(px(16.))
                    .py(px(12.))
                    .border_t_1()
                    .border_color(palette.line)
                    .child(
                        div()
                            .flex_1()
                            .child(Input::new(&self.input).id("mav-input").aria_label("Message to Mav")),
                    )
                    .child(ui::caps("enter sends").text_color(palette.faint)),
            )
    }

    fn item(&self, index: usize, item: &Item, palette: &Palette, cx: &mut Context<Self>) -> Div {
        match item {
            Item::Said(Speaker::User, text) => v_flex().items_end().child(
                ui::serif(text.clone())
                    .max_w(px(520.))
                    .px(px(12.))
                    .py(px(9.))
                    .border_1()
                    .border_color(palette.line),
            ),
            Item::Said(Speaker::Mav, text) => v_flex()
                .gap(px(4.))
                .child(ui::caps("mav").text_color(palette.signal(Signal::Model)))
                .child(ui::serif(text.clone()).max_w(px(560.))),
            Item::Refused(why) => v_flex().child(ui::serif(why.clone()).text_color(palette.signal(Signal::Failed))),
            Item::Proposal(action, card) => self.card(index, action, card, palette, cx),
        }
    }

    fn card(&self, index: usize, action: &Action, card: &Card, palette: &Palette, cx: &mut Context<Self>) -> Div {
        let model = palette.signal(Signal::Model);
        let failed = palette.signal(Signal::Failed);
        let state = match card {
            Card::Proposed => "proposed".to_owned(),
            Card::Adding => "adding".to_owned(),
            Card::Done(done) => done.to_lowercase(),
            Card::Failed(_) => "failed".to_owned(),
            Card::Skipped => "skipped".to_owned(),
        };
        let facts = action.facts().into_iter().map(|(label, value)| {
            h_flex()
                .gap(px(10.))
                .child(ui::caps(label).w(px(70.)).flex_none().text_color(palette.faint))
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(if label == "Clashes" { failed } else { palette.ink })
                        .child(value),
                )
        });
        let actions = match card {
            Card::Proposed | Card::Failed(_) => {
                let word = if matches!(card, Card::Failed(_)) {
                    "try again"
                } else {
                    "confirm"
                };
                h_flex()
                    .gap(px(8.))
                    .child(
                        ui::button(("confirm", index), "Confirm", ui::primary_face(word, None, palette))
                            .on_click(cx.listener(move |this, _, _, cx| this.confirm(index, cx))),
                    )
                    .child(
                        ui::button(("skip", index), "Skip", ui::quiet_face("skip", palette))
                            .on_click(cx.listener(move |this, _, _, cx| this.skip(index, cx))),
                    )
            }
            Card::Adding => h_flex().child(ui::caps("adding").text_color(model)),
            Card::Done(done) => h_flex().child(ui::chip(Signal::Done, done, palette)),
            Card::Skipped => h_flex().child(ui::caps("skipped").text_color(palette.faint)),
        };
        let mut shown = v_flex()
            .relative()
            .max_w(px(560.))
            .gap(px(8.))
            .px(px(14.))
            .py(px(12.))
            .border_1()
            .border_color(palette.line)
            .bg(palette.background.opacity(0.85));
        if matches!(card, Card::Proposed | Card::Failed(_)) {
            shown = shown.children(ui::brackets(model));
        }
        shown
            .child(
                div()
                    .id(("card", index))
                    .test_support()
                    .role(gpui_kit::Role::Label)
                    .aria_label(format!("{} · {} · {state}", action.kind(), action.title()))
                    .child(ui::chip(Signal::Model, action.kind(), palette)),
            )
            .child(
                ui::serif(action.title().to_owned())
                    .text_size(px(16.))
                    .line_height(px(21.)),
            )
            .children(facts)
            .children(match card {
                Card::Failed(why) => Some(ui::serif(why.clone()).text_size(px(13.)).text_color(failed)),
                _ => None,
            })
            .child(actions)
    }

    /// The folded side panel: a strip with the count of pressing tasks.
    fn strip(&self, palette: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let pressing = self.pressing().map(|count| count.to_string()).unwrap_or_default();
        v_flex()
            .flex_none()
            .w(px(44.))
            .h_full()
            .items_center()
            .py(px(16.))
            .gap(px(10.))
            .child(
                ui::button(
                    "expand-rail",
                    "Show the calendar and TickTick",
                    ui::quiet_face("‹", palette),
                )
                .on_click(cx.listener(|this, _, _, cx| this.change_view(|view| view.collapsed = false, cx))),
            )
            .child(ui::caps(pressing).text_color(palette.signal(Signal::Failed)))
    }

    fn rail(&self, palette: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let status = match &self.loaded {
            Loaded::Not | Loaded::Loading => "reading".to_owned(),
            Loaded::Failed(_) => "could not read".to_owned(),
            Loaded::Ready { at, .. } => format!("read at {at}"),
        };
        let body = match &self.loaded {
            Loaded::Ready { seen, .. } => v_flex()
                .gap(px(20.))
                .child(self.calendar(seen, palette, cx))
                .child(div().h(px(1.)).bg(palette.line))
                .child(self.tasks(seen, palette, cx)),
            Loaded::Failed(error) => ui::serif(error.clone()).text_color(palette.signal(Signal::Failed)),
            Loaded::Not | Loaded::Loading => v_flex(),
        };
        v_flex()
            .id("mav-rail")
            .flex_none()
            .w(px(RAIL_WIDTH))
            .h_full()
            .overflow_y_scroll()
            .px(px(16.))
            .py(px(20.))
            .gap(px(18.))
            .child(
                h_flex()
                    .justify_between()
                    .child(
                        div()
                            .id("mav-status")
                            .test_support()
                            .role(gpui_kit::Role::Label)
                            .aria_label(status.clone())
                            .child(ui::caps(status).text_color(palette.faint)),
                    )
                    .child(
                        ui::button(
                            "collapse-rail",
                            "Hide the calendar and TickTick",
                            ui::quiet_face("collapse ›", palette),
                        )
                        .on_click(cx.listener(|this, _, _, cx| this.change_view(|view| view.collapsed = true, cx))),
                    ),
            )
            .child(body)
    }

    /// The calendar part of the side panel: one day with its free time, or the coming week.
    fn calendar(&self, seen: &Seen, palette: &Palette, cx: &mut Context<Self>) -> Div {
        let week = self.view.week;
        let day = seen
            .now
            .date()
            .checked_add(jiff::ToSpan::days(self.day))
            .unwrap_or(seen.now.date());
        let title = if week {
            "the coming week".to_owned()
        } else if self.day == 0 {
            format!("today · {}", day.strftime("%a %-d %b"))
        } else {
            day.strftime("%a %-d %b").to_string()
        };
        let nav = |id: &'static str,
                   word: &'static str,
                   name: &'static str,
                   to: i64,
                   enabled: bool,
                   cx: &mut Context<Self>| {
            let face = ui::caps(word)
                .px(px(4.))
                .text_color(if enabled { palette.ink } else { palette.line });
            ui::button(id, name, face)
                .on_click(cx.listener(move |this, _, _, cx| {
                    if enabled {
                        this.show_day(to, cx);
                    }
                }))
                .into_any_element()
        };
        let switch = |id: &'static str, word: &'static str, on: bool, cx: &mut Context<Self>| -> gpui_kit::AnyElement {
            let face = ui::caps(word)
                .px(px(8.))
                .py(px(1.))
                .text_color(if on { palette.ink } else { palette.faint });
            let face = if on { face.bg(palette.line) } else { face };
            ui::button(id, format!("Show one {word}"), face)
                .on_click(cx.listener(move |this, _, _, cx| this.change_view(|view| view.week = word == "week", cx)))
                .into_any_element()
        };
        let mut section = v_flex().gap(px(6.)).child(
            h_flex()
                .justify_between()
                .child(
                    h_flex()
                        .gap(px(4.))
                        .children((!week).then(|| nav("day-back", "‹", "Previous day", self.day - 1, self.day > 0, cx)))
                        .child(heading("calendar-day", title, palette.ink))
                        .children((!week).then(|| nav("day-next", "›", "Next day", self.day + 1, self.day < 6, cx)))
                        .children(
                            (!week && self.day > 0).then(|| nav("day-today", "today", "Back to today", 0, true, cx)),
                        ),
                )
                .child(
                    h_flex()
                        .border_1()
                        .border_color(palette.line)
                        .child(switch("calendar-day-view", "day", !week, cx))
                        .child(switch("calendar-week-view", "week", week, cx)),
                ),
        );
        if week {
            for ahead in 0..7 {
                let date = seen
                    .now
                    .date()
                    .checked_add(jiff::ToSpan::days(ahead))
                    .unwrap_or(seen.now.date());
                let events: Vec<&Event> = seen.events.iter().filter(|event| event.touches(date)).collect();
                section = section.child(
                    ui::caps(format!("{} · {}", date.strftime("%a %-d"), events.len()))
                        .pt(px(4.))
                        .text_color(palette.muted),
                );
                for event in events {
                    section = section.child(self.event_row(event, palette, cx));
                }
            }
            return section;
        }
        let plan = calendar::day_plan(&seen.events, day, &seen.now);
        if plan.is_empty() {
            section = section.child(ui::caps("nothing").text_color(palette.faint));
        }
        for (index, stretch) in plan.into_iter().enumerate() {
            section = section.child(match stretch {
                Stretch::Event(event) => self.event_row(event, palette, cx).into_any_element(),
                Stretch::Free(slot) => self.free_row(index, slot, palette, cx).into_any_element(),
            });
        }
        section
    }

    fn event_row(&self, event: &Event, palette: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let asking = self.deleting.as_deref() == Some(&event.source.id);
        let doomed = event.clone();
        h_flex()
            .id(gpui_kit::SharedString::from(format!("event-{}", event.source.id)))
            .test_support()
            .role(gpui_kit::Role::Label)
            .aria_label(format!("{} · {} · {}", event.title, event.hours(), event.profile))
            .gap(px(10.))
            .py(px(4.))
            .border_b_1()
            .border_color(palette.line)
            .child(
                div()
                    .flex_none()
                    .w(px(2.))
                    .h(px(16.))
                    .bg(if event.busy { palette.muted } else { palette.faint }),
            )
            .child(
                ui::serif(event.title.clone())
                    .text_size(px(13.5))
                    .flex_1()
                    .min_w_0()
                    .truncate(),
            )
            .child(ui::caps(event.hours()).flex_none().text_color(palette.faint))
            .child(ui::caps(event.profile.clone()).flex_none().text_color(palette.faint))
            .child(
                ui::button(
                    gpui_kit::SharedString::from(format!("delete-event-{}", event.source.id)),
                    if asking {
                        format!("Really delete {}", event.title)
                    } else {
                        format!("Delete {}", event.title)
                    },
                    ui::caps(if asking { "sure?" } else { "delete" }).text_color(if asking {
                        palette.signal(Signal::Failed)
                    } else {
                        palette.faint
                    }),
                )
                .on_click(cx.listener(move |this, _, _, cx| this.delete_event(&doomed, cx))),
            )
    }

    fn free_row(&self, index: usize, slot: Slot, palette: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let naming = self.blocking.as_ref() == Some(&slot);
        let words = format!(
            "free {} to {} · {}",
            slot.start.strftime("%H:%M"),
            slot.end.strftime("%H:%M"),
            calendar::length(slot.minutes())
        );
        let row = v_flex()
            .id(("free", index))
            .test_support()
            .role(gpui_kit::Role::Label)
            .aria_label(words.clone())
            .gap(px(4.))
            .px(px(8.))
            .py(px(4.))
            .border_1()
            .border_dashed()
            .border_color(palette.line)
            .child(
                h_flex()
                    .justify_between()
                    .child(ui::caps(words).text_color(palette.faint))
                    .children((!naming).then(|| {
                        ui::button(
                            ("add-block", index),
                            "Add a block here",
                            ui::caps("+ block").text_color(palette.ink),
                        )
                        .on_click(cx.listener(move |this, _, window, cx| this.start_block(slot.clone(), window, cx)))
                    })),
            );
        if naming {
            row.child(Input::new(&self.block_title).id("block-title"))
        } else {
            row
        }
    }

    /// The TickTick part of the side panel: add a task, and every open task by priority or by date.
    fn tasks(&self, seen: &Seen, palette: &Palette, cx: &mut Context<Self>) -> Div {
        let by_date = self.view.by_date;
        let switch = |id: &'static str, word: &'static str, on: bool, cx: &mut Context<Self>| -> gpui_kit::AnyElement {
            let face = ui::caps(word)
                .px(px(8.))
                .py(px(1.))
                .text_color(if on { palette.ink } else { palette.faint });
            let face = if on { face.bg(palette.line) } else { face };
            ui::button(id, format!("Group tasks by {word}"), face)
                .on_click(cx.listener(move |this, _, _, cx| this.change_view(|view| view.by_date = word == "date", cx)))
                .into_any_element()
        };
        let new_priority = self.new_priority;
        let mut section = v_flex()
            .gap(px(8.))
            .child(
                h_flex()
                    .justify_between()
                    .child(heading(
                        "ticktick-open",
                        format!("ticktick · {} open", seen.tasks.len()),
                        palette.muted,
                    ))
                    .child(
                        h_flex()
                            .border_1()
                            .border_color(palette.line)
                            .child(switch("by-priority", "priority", !by_date, cx))
                            .child(switch("by-date", "date", by_date, cx)),
                    ),
            )
            .child(
                h_flex()
                    .gap(px(6.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(Input::new(&self.new_task).id("new-task")),
                    )
                    .child(
                        ui::button(
                            "new-task-priority",
                            format!("New task priority {}", new_priority.name()),
                            priority_tag(new_priority, palette),
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.new_priority = new_priority.next();
                            cx.notify();
                        })),
                    ),
            )
            .children(self.rail_problem.clone().map(|problem| {
                ui::serif(problem)
                    .text_size(px(13.))
                    .text_color(palette.signal(Signal::Failed))
            }));
        let overdue: Vec<&str> = seen.agenda.overdue.iter().map(|entry| entry.id.as_str()).collect();
        let groups: Vec<(String, Vec<Entry>)> = if by_date {
            let undated: Vec<Entry> = seen.tasks.iter().filter(|entry| entry.due.is_none()).cloned().collect();
            vec![
                ("overdue".to_owned(), seen.agenda.overdue.clone()),
                ("today".to_owned(), seen.agenda.today.clone()),
                ("coming week".to_owned(), seen.agenda.upcoming.clone()),
                ("no date".to_owned(), undated),
            ]
        } else {
            Board::of(&seen.tasks)
                .groups()
                .into_iter()
                .map(|(priority, entries)| (priority.name().to_owned(), entries.to_vec()))
                .collect()
        };
        for (index, (name, entries)) in groups.into_iter().enumerate() {
            if entries.is_empty() {
                continue;
            }
            section = section.child(
                div()
                    .id(("task-group", index))
                    .test_support()
                    .role(gpui_kit::Role::Label)
                    .aria_label(format!("{name} · {}", entries.len()))
                    .pt(px(6.))
                    .child(ui::caps(format!("{name} · {}", entries.len())).text_color(palette.muted)),
            );
            for entry in entries {
                section = section.child(self.task_row(&entry, overdue.contains(&entry.id.as_str()), palette, cx));
            }
        }
        section
    }

    fn task_row(&self, entry: &Entry, overdue: bool, palette: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let finished = self.finished.contains(&entry.id);
        let done = palette.signal(Signal::Done);
        let mark = div()
            .size(px(11.))
            .border_1()
            .border_color(if finished { done } else { palette.muted });
        let mark = if finished { mark.bg(done) } else { mark };
        let (finish, cycle) = (entry.clone(), entry.clone());
        let when = if finished { "done".to_owned() } else { entry.when() };
        let when_color = if finished {
            done
        } else if overdue {
            palette.signal(Signal::Failed)
        } else {
            palette.faint
        };
        let title = ui::serif(entry.title.clone())
            .text_size(px(13.5))
            .flex_1()
            .min_w_0()
            .truncate();
        let title = if finished {
            title.line_through().text_color(palette.faint)
        } else {
            title
        };
        h_flex()
            .id(gpui_kit::SharedString::from(format!("task-{}", entry.id)))
            .test_support()
            .role(gpui_kit::Role::Label)
            .aria_label(format!("{} · {} · {}", entry.title, entry.priority.name(), when))
            .gap(px(10.))
            .py(px(4.))
            .border_b_1()
            .border_color(palette.line)
            .child(
                ui::button(
                    gpui_kit::SharedString::from(format!("finish-{}", entry.id)),
                    format!("Finish {}", entry.title),
                    mark,
                )
                .on_click(cx.listener(move |this, _, _, cx| this.finish(&finish, cx))),
            )
            .child(title)
            .child(ui::caps(when).flex_none().text_color(when_color))
            .child(
                ui::button(
                    gpui_kit::SharedString::from(format!("priority-{}", entry.id)),
                    format!("Change the priority of {}", entry.title),
                    priority_tag(entry.priority, palette),
                )
                .on_click(cx.listener(move |this, _, _, cx| this.cycle_priority(&cycle, cx))),
            )
    }
}

/// The small priority tag: high in ink, medium muted, low and none faint.
fn priority_tag(priority: Priority, palette: &Palette) -> Div {
    let (word, color) = match priority {
        Priority::High => ("high", palette.ink),
        Priority::Medium => ("med", palette.muted),
        Priority::Low => ("low", palette.faint),
        Priority::None => ("none", palette.faint),
    };
    ui::boxed(word, color).w(px(40.)).flex().justify_center()
}

fn heading(id: &'static str, text: String, color: gpui_kit::Hsla) -> impl IntoElement {
    div()
        .id(id)
        .test_support()
        .role(gpui_kit::Role::Label)
        .aria_label(text.clone())
        .pb(px(4.))
        .child(ui::caps(text).text_color(color))
}

#[cfg(test)]
mod tests {
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{AppContext as _, TestAppContext, WindowOptions};
    use jiff::civil::date;
    use maverick_core::calendar::{Event, Source, Span};
    use maverick_core::mav::{Action, Context as Seen, Reply};
    use maverick_core::ticktick::{Agenda, Due, Entry, NewTask, Priority};

    use super::{Card, Item, Loaded, Mav, ready};

    fn at(day: i8, hour: i8) -> jiff::ZonedDateTime {
        date(2026, 11, day)
            .at(hour, 0, 0, 0)
            .in_tz("America/Los_Angeles")
            .unwrap()
    }

    fn entry(id: &str, title: &str, due: Option<Due>, priority: Priority) -> Entry {
        Entry {
            id: id.into(),
            list_id: "inbox1".into(),
            title: title.into(),
            list: "Inbox".into(),
            due,
            priority,
        }
    }

    fn task(title: &str) -> Action {
        Action::Task {
            task: NewTask {
                title: title.into(),
                list_id: "inbox".into(),
                due: None,
                remind_before: None,
                priority: Priority::None,
            },
            list: "Inbox".into(),
        }
    }

    #[gpui_kit::test]
    fn the_rail_shows_tasks_and_proposals_become_cards(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::ui::install(cx);
        });
        let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let background = runtime.handle().clone();
        let (window, mav) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Mav::new(background, window, cx))
            })
            .unwrap()
        });
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(window.find("mav-status").label(), Some("reading"));

            let now = date(2026, 11, 22)
                .at(16, 12, 0, 0)
                .in_tz("America/Los_Angeles")
                .unwrap();
            let tasks = vec![
                entry("a", "Renew lease", Some(Due::Day(date(2026, 11, 20))), Priority::High),
                entry(
                    "b",
                    "Send references",
                    Some(Due::Day(date(2026, 11, 23))),
                    Priority::Medium,
                ),
                entry("c", "Read a book", None, Priority::Low),
            ];
            let seen = Seen {
                agenda: Agenda::from_entries(&tasks, &now),
                now,
                lists: Vec::new(),
                tasks,
                events: vec![Event {
                    title: "Screen".into(),
                    profile: "Work".into(),
                    span: Span::At(at(23, 10), at(23, 11)),
                    busy: true,
                    source: Source {
                        account: "w@example.com".into(),
                        calendar: "primary".into(),
                        id: "s1".into(),
                    },
                }],
                profiles: Vec::new(),
                ticktick: Default::default(),
            };
            mav.update(cx, |mav, cx| mav.set(ready(seen), cx));
            window.render_frame(cx);
            assert_eq!(window.find("mav-status").label(), Some("read at 4:12 PM"));
            assert_eq!(window.find("ticktick-open").label(), Some("ticktick · 3 open"));
            assert_eq!(window.find(("task-group", 0usize)).label(), Some("high · 1"));
            assert_eq!(window.find("task-a").label(), Some("Renew lease · high · Fri 20"));
            assert_eq!(window.find("task-c").label(), Some("Read a book · low · no date"));
            assert_eq!(window.find("calendar-day").label(), Some("today · Sun 22 Nov"));
            assert_eq!(
                window.find(("free", 0usize)).label(),
                Some("free 16:12 to 22:00 · 5 h 48 min")
            );
            assert!(window.try_find("day-today").is_none());

            mav.update(cx, |mav, cx| mav.show_day(1, cx));
            window.render_frame(cx);
            assert_eq!(window.find("calendar-day").label(), Some("Mon 23 Nov"));
            assert_eq!(window.find(("free", 0usize)).label(), Some("free 08:00 to 10:00 · 2 h"));
            assert_eq!(window.find("event-s1").label(), Some("Screen · 10:00 to 11:00 · Work"));
            assert!(window.try_find("day-today").is_some());

            // The first delete click only asks.
            let screen = match &mav.read(cx).loaded {
                Loaded::Ready { seen, .. } => seen.events[0].clone(),
                _ => panic!("not read"),
            };
            mav.update(cx, |mav, cx| mav.delete_event(&screen, cx));
            window.render_frame(cx);
            assert_eq!(window.find("delete-event-s1").label(), Some("Really delete Screen"));

            mav.update(cx, |mav, cx| mav.change_view(|view| view.week = true, cx));
            window.render_frame(cx);
            assert_eq!(window.find("calendar-day").label(), Some("the coming week"));
            mav.update(cx, |mav, cx| mav.change_view(|view| view.week = false, cx));
            assert_eq!(mav.read(cx).pressing(), Some(1));

            mav.update(cx, |mav, cx| mav.change_view(|view| view.by_date = true, cx));
            window.render_frame(cx);
            assert_eq!(window.find(("task-group", 0usize)).label(), Some("overdue · 1"));
            assert_eq!(window.find(("task-group", 3usize)).label(), Some("no date · 1"));

            mav.update(cx, |mav, cx| mav.change_view(|view| view.collapsed = true, cx));
            window.render_frame(cx);
            assert!(window.try_find("task-a").is_none());
            assert!(window.try_find("expand-rail").is_some());
            mav.update(cx, |mav, cx| mav.change_view(|view| view.collapsed = false, cx));

            let reply = Reply {
                text: "I propose a task.".into(),
                proposals: vec![
                    Ok(task("Call the bank")),
                    Err("Mav gave a date Maverick cannot read: soon".into()),
                ],
            };
            mav.update(cx, |mav, cx| mav.answer(reply, cx));
            window.render_frame(cx);
            assert_eq!(
                window.find(("card", 1usize)).label(),
                Some("TickTick · New task · Call the bank · proposed")
            );
            assert!(matches!(mav.read(cx).items()[2], Item::Refused(_)));

            mav.update(cx, |mav, cx| mav.skip(1, cx));
            window.render_frame(cx);
            assert_eq!(
                window.find(("card", 1usize)).label(),
                Some("TickTick · New task · Call the bank · skipped")
            );
            assert!(matches!(mav.read(cx).items()[1], Item::Proposal(_, Card::Skipped)));
        })
        .unwrap();
    }
}
