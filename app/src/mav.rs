//! The Mav view: what is due in TickTick, read when the view is shown.

use gpui_kit::component::{h_flex, v_flex};
use gpui_kit::{
    Context, Div, InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, TestSupportExt as _, Window, div, px,
};
use jiff::ZonedDateTime;
use maverick_core::ticktick::{self, Agenda, Entry};
use tokio::runtime::Handle;

use crate::ui::{self, Palette, Signal};

/// Where the agenda stands.
#[derive(Clone, Debug, PartialEq)]
pub enum Loaded {
    /// Not read yet: the view has not been shown.
    Not,
    Loading,
    /// No TickTick token is in the Keychain.
    NoToken,
    Failed(String),
    Ready {
        agenda: Agenda,
        at: String,
    },
}

pub struct Mav {
    loaded: Loaded,
    background: Handle,
}

impl Mav {
    pub fn new(background: Handle) -> Self {
        Self {
            loaded: Loaded::Not,
            background,
        }
    }

    /// Reads the agenda from TickTick again. The last one stays on screen until the new one arrives.
    pub fn load(&mut self, cx: &mut Context<Self>) {
        if matches!(self.loaded, Loaded::Not | Loaded::Failed(_)) {
            self.loaded = Loaded::Loading;
            cx.notify();
        }
        let read = self.background.spawn(async {
            let Some(token) = ticktick::stored_token().await? else {
                return Ok(None);
            };
            let now = ZonedDateTime::now();
            let agenda = ticktick::agenda(&token, &now).await?;
            Ok::<_, String>(Some((agenda, now.strftime("%-I:%M %p").to_string())))
        });
        cx.spawn(async move |this, cx| {
            let loaded = match read.await {
                Ok(Ok(Some((agenda, at)))) => Loaded::Ready { agenda, at },
                Ok(Ok(None)) => Loaded::NoToken,
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
            Loaded::Ready { agenda, .. } => Some(agenda.overdue.len() + agenda.today.len()),
            _ => None,
        }
    }
}

impl Render for Mav {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::of(cx);
        let status = match &self.loaded {
            Loaded::Not | Loaded::Loading => "reading ticktick".to_owned(),
            Loaded::NoToken => "not connected".to_owned(),
            Loaded::Failed(_) => "could not read ticktick".to_owned(),
            Loaded::Ready { at, .. } => format!("read at {at}"),
        };
        let body = match &self.loaded {
            Loaded::Ready { agenda, .. } => v_flex()
                .gap(px(22.))
                .child(section(
                    "overdue",
                    "Overdue",
                    &agenda.overdue,
                    Some(Signal::Failed),
                    &palette,
                ))
                .child(section("today", "Today", &agenda.today, None, &palette))
                .child(section("upcoming", "Coming week", &agenda.upcoming, None, &palette)),
            Loaded::NoToken => note(
                "Make an API token in TickTick (Settings, Account, API Token), then run \
                 `maverick ticktick token` with the token on standard input.",
                &palette,
            ),
            Loaded::Failed(error) => note(error.clone(), &palette).text_color(palette.signal(Signal::Failed)),
            Loaded::Not | Loaded::Loading => v_flex(),
        };
        v_flex()
            .id("mav")
            .size_full()
            .overflow_y_scroll()
            .px(px(28.))
            .py(px(20.))
            .gap(px(18.))
            .child(
                h_flex()
                    .justify_between()
                    .child(ui::caps("TickTick").text_color(palette.muted))
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

fn note(text: impl Into<SharedString>, palette: &Palette) -> Div {
    ui::serif(text).text_color(palette.muted).max_w(px(560.))
}

/// One heading and its tasks. An empty section says so, so the user knows it was read.
fn section(id: &'static str, name: &str, entries: &[Entry], signal: Option<Signal>, palette: &Palette) -> Div {
    let accent = signal.map_or(palette.muted, |signal| palette.signal(signal));
    let heading = format!("{name} · {}", entries.len());
    v_flex()
        .gap(px(2.))
        .child(
            div()
                .id(id)
                .test_support()
                .role(gpui_kit::Role::Label)
                .aria_label(heading.clone())
                .pb(px(6.))
                .child(ui::caps(heading).text_color(if entries.is_empty() { palette.faint } else { accent })),
        )
        .children(entries.iter().map(|entry| row(entry, signal, palette)))
        .children(
            entries
                .is_empty()
                .then(|| ui::caps("nothing").text_color(palette.faint)),
        )
}

fn row(entry: &Entry, signal: Option<Signal>, palette: &Palette) -> Div {
    let due_color = signal.map_or(palette.muted, |signal| palette.signal(signal));
    h_flex()
        .gap(px(12.))
        .py(px(5.))
        .border_b_1()
        .border_color(palette.line)
        .child(div().flex_none().size(px(10.)).border_1().border_color(palette.muted))
        .child(ui::serif(entry.title.clone()).flex_1().min_w_0().truncate())
        .child(ui::caps(&entry.list).flex_none().text_color(palette.faint))
        .child(
            ui::caps(entry.due.label())
                .flex_none()
                .w(px(110.))
                .text_right()
                .text_color(due_color),
        )
}

#[cfg(test)]
mod tests {
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{AppContext as _, TestAppContext, WindowOptions};
    use jiff::civil::date;
    use maverick_core::ticktick::{Agenda, Due, Entry};

    use super::{Loaded, Mav};

    fn entry(title: &str, due: Due) -> Entry {
        Entry {
            title: title.into(),
            list: "Inbox".into(),
            due,
            priority: 0,
        }
    }

    #[gpui_kit::test]
    fn the_agenda_shows_each_section_with_its_count(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::ui::install(cx);
        });
        let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let background = runtime.handle().clone();
        let (window, mav) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |_, cx| cx.new(|_| Mav::new(background))).unwrap()
        });
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(window.find("mav-status").label(), Some("reading ticktick"));

            let agenda = Agenda {
                overdue: vec![entry("Renew lease", Due::Day(date(2026, 11, 20)))],
                today: Vec::new(),
                upcoming: vec![entry("Send references", Due::Day(date(2026, 11, 23)))],
            };
            mav.update(cx, |mav, cx| {
                mav.set(
                    Loaded::Ready {
                        agenda,
                        at: "4:12 PM".into(),
                    },
                    cx,
                )
            });
            window.render_frame(cx);
            assert_eq!(window.find("mav-status").label(), Some("read at 4:12 PM"));
            assert_eq!(window.find("overdue").label(), Some("Overdue · 1"));
            assert_eq!(window.find("today").label(), Some("Today · 0"));
            assert_eq!(window.find("upcoming").label(), Some("Coming week · 1"));
            assert_eq!(mav.read(cx).pressing(), Some(1));
        })
        .unwrap();
    }
}
