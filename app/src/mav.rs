//! The Mav view: talk to Mav on the left, see the calendar and TickTick on the right.
//!
//! Mav only proposes. Each proposal is a card, and nothing is written until the user confirms it.

use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{h_flex, v_flex};
use gpui_kit::{
    AppContext as _, Context, Div, Entity, FontWeight, InteractiveElement as _, IntoElement, ParentElement as _,
    Render, StatefulInteractiveElement as _, Styled as _, Subscription, TestSupportExt as _, Window, div, px,
};
use jiff::ZonedDateTime;
use maverick_core::calendar::Event;
use maverick_core::mav::{self, Action, Context as Seen, Speaker};
use maverick_core::ticktick::{Entry, Project};
use tokio::runtime::Handle;

use crate::ui::{self, Palette, Signal};

const RAIL_WIDTH: f32 = 360.0;

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
    Proposal(Action, Card),
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
    _typed: Subscription,
}

impl Mav {
    pub fn new(background: Handle, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Tell Mav what to remember or plan"));
        let _typed = cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| {
            if let InputEvent::PressEnter { .. } = event {
                this.send(window, cx);
            }
        });
        Self {
            loaded: Loaded::Not,
            items: Vec::new(),
            thinking: false,
            input,
            background,
            _typed,
        }
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
                Ok(action) => Item::Proposal(action, Card::Proposed),
                Err(why) => Item::Refused(why),
            });
        }
        cx.notify();
    }

    /// Offers a proposal Mav came up with itself, such as a follow-up after an application.
    pub fn suggest(&mut self, why: String, action: Action, cx: &mut Context<Self>) {
        self.items.push(Item::Said(Speaker::Mav, why));
        self.items.push(Item::Proposal(action, Card::Proposed));
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
        let action = action.clone();
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
            .child(self.rail(&palette))
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

    fn rail(&self, palette: &Palette) -> impl IntoElement {
        let status = match &self.loaded {
            Loaded::Not | Loaded::Loading => "reading".to_owned(),
            Loaded::Failed(_) => "could not read".to_owned(),
            Loaded::Ready { at, .. } => format!("read at {at}"),
        };
        let body = match &self.loaded {
            Loaded::Ready { seen, .. } => {
                let today = seen.now.date();
                let tomorrow = today.tomorrow().unwrap_or(today);
                let on = |day| {
                    seen.events
                        .iter()
                        .filter(|event| event.touches(day))
                        .collect::<Vec<_>>()
                };
                v_flex()
                    .gap(px(20.))
                    .child(day_section("cal-today", "Today", &on(today), palette))
                    .child(day_section("cal-tomorrow", "Tomorrow", &on(tomorrow), palette))
                    .child(section(
                        "overdue",
                        "Overdue",
                        &seen.agenda.overdue,
                        Some(Signal::Failed),
                        palette,
                    ))
                    .child(section("today", "Due today", &seen.agenda.today, None, palette))
                    .child(section("upcoming", "Coming week", &seen.agenda.upcoming, None, palette))
            }
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
                    .child(ui::caps("calendar and ticktick").text_color(palette.muted))
                    .child(
                        div()
                            .id("mav-status")
                            .test_support()
                            .role(gpui_kit::Role::Label)
                            .aria_label(status.clone())
                            .child(ui::caps(status).text_color(palette.faint)),
                    ),
            )
            .child(body)
    }
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

fn day_section(id: &'static str, name: &str, events: &[&Event], palette: &Palette) -> Div {
    let color = if events.is_empty() {
        palette.faint
    } else {
        palette.muted
    };
    v_flex()
        .gap(px(2.))
        .child(heading(id, format!("{name} · {}", events.len()), color))
        .children(events.iter().map(|event| {
            h_flex()
                .gap(px(10.))
                .py(px(4.))
                .border_b_1()
                .border_color(palette.line)
                .child(div().flex_none().w(px(2.)).h(px(16.)).bg(if event.busy {
                    palette.muted
                } else {
                    palette.faint
                }))
                .child(
                    ui::serif(event.title.clone())
                        .text_size(px(13.5))
                        .flex_1()
                        .min_w_0()
                        .truncate(),
                )
                .child(ui::caps(event.hours()).flex_none().text_color(palette.faint))
        }))
        .children(events.is_empty().then(|| ui::caps("nothing").text_color(palette.faint)))
}

/// One heading and its tasks. An empty section says so, so the user knows it was read.
fn section(id: &'static str, name: &str, entries: &[Entry], signal: Option<Signal>, palette: &Palette) -> Div {
    let accent = signal.map_or(palette.muted, |signal| palette.signal(signal));
    let color = if entries.is_empty() { palette.faint } else { accent };
    v_flex()
        .gap(px(2.))
        .child(heading(id, format!("{name} · {}", entries.len()), color))
        .children(entries.iter().map(|entry| {
            h_flex()
                .gap(px(10.))
                .py(px(4.))
                .border_b_1()
                .border_color(palette.line)
                .child(div().flex_none().size(px(9.)).border_1().border_color(palette.muted))
                .child(
                    ui::serif(entry.title.clone())
                        .text_size(px(13.5))
                        .flex_1()
                        .min_w_0()
                        .truncate(),
                )
                .child(
                    ui::caps(entry.due.label())
                        .flex_none()
                        .text_color(accent)
                        .font_weight(FontWeight::MEDIUM),
                )
        }))
        .children(
            entries
                .is_empty()
                .then(|| ui::caps("nothing").text_color(palette.faint)),
        )
}

#[cfg(test)]
mod tests {
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{AppContext as _, TestAppContext, WindowOptions};
    use jiff::civil::date;
    use maverick_core::mav::{Action, Context as Seen, Reply};
    use maverick_core::ticktick::{Agenda, Due, Entry, NewTask};

    use super::{Card, Item, Mav, ready};

    fn entry(title: &str, due: Due) -> Entry {
        Entry {
            title: title.into(),
            list: "Inbox".into(),
            due,
            priority: 0,
        }
    }

    fn task(title: &str) -> Action {
        Action::Task {
            task: NewTask {
                title: title.into(),
                list_id: "inbox".into(),
                due: None,
                remind_before: None,
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

            let seen = Seen {
                now: date(2026, 11, 22)
                    .at(16, 12, 0, 0)
                    .in_tz("America/Los_Angeles")
                    .unwrap(),
                lists: Vec::new(),
                agenda: Agenda {
                    overdue: vec![entry("Renew lease", Due::Day(date(2026, 11, 20)))],
                    today: Vec::new(),
                    upcoming: vec![entry("Send references", Due::Day(date(2026, 11, 23)))],
                },
                events: Vec::new(),
            };
            mav.update(cx, |mav, cx| mav.set(ready(seen), cx));
            window.render_frame(cx);
            assert_eq!(window.find("mav-status").label(), Some("read at 4:12 PM"));
            assert_eq!(window.find("overdue").label(), Some("Overdue · 1"));
            assert_eq!(window.find("cal-tomorrow").label(), Some("Tomorrow · 0"));
            assert_eq!(mav.read(cx).pressing(), Some(1));

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
