//! The first-run guide: model, TickTick, Google, then Chrome, one step at a time.
//! It shows the matching part of Connections for each step, so there is one place to change each.

use gpui_kit::component::{h_flex, v_flex};
use gpui_kit::{
    Context, Entity, InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, TestSupportExt as _, Window, div, px,
};

use crate::connections::{Connections, Section};
use crate::ui::{self, Palette, Signal};

/// The steps, in order, with what each one is for.
const STEPS: [(Section, &str, &str, bool); 4] = [
    (
        Section::Models,
        "Choose a model",
        "Maverick needs one model to read postings, fill forms and talk as Mav. Use a command line tool you are \
         already signed in to, such as Claude Code or Codex, or paste an API key.",
        true,
    ),
    (
        Section::TickTick,
        "Connect TickTick",
        "Mav adds tasks and reminders to TickTick when you confirm them. Skip this if you do not use TickTick.",
        false,
    ),
    (
        Section::Google,
        "Connect Google",
        "Mav reads your calendars to find free time and adds the blocks you confirm. Connect up to 5 accounts and \
         name each one, such as Work or School.",
        false,
    ),
    (
        Section::Chrome,
        "Connect Chrome",
        "Maverick opens job applications and fills their forms in your Chrome, through its extension. You click \
         submit.",
        false,
    ),
];

/// The file whose presence means the guide was finished or dismissed.
fn marker() -> Option<std::path::PathBuf> {
    maverick_core::data_dir().map(|dir| dir.join("setup-done"))
}

/// True once the user finished or dismissed the guide.
pub fn done() -> bool {
    marker().is_some_and(|file| file.exists())
}

/// What the window does when the guide ends.
pub struct Finished;

impl gpui_kit::EventEmitter<Finished> for Setup {}

pub struct Setup {
    connections: Entity<Connections>,
    step: usize,
    _observed: Subscription,
}

impl Setup {
    pub fn new(connections: Entity<Connections>, cx: &mut Context<Self>) -> Self {
        let _observed = cx.observe(&connections, |_, _, cx| cx.notify());
        Self {
            connections,
            step: 0,
            _observed,
        }
    }

    /// Starts the guide from the first step.
    pub fn start(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.go(0, window, cx);
    }

    fn go(&mut self, step: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.step = step.min(STEPS.len() - 1);
        let section = STEPS[self.step].0;
        self.connections.update(cx, |connections, cx| {
            connections.show_only(Some(section), cx);
            connections.refresh(window, cx);
        });
        cx.notify();
    }

    fn finish(&mut self, cx: &mut Context<Self>) {
        if let Some(file) = marker() {
            if let Some(folder) = file.parent() {
                let _ = std::fs::create_dir_all(folder);
            }
            let _ = std::fs::write(file, "");
        }
        self.connections
            .update(cx, |connections, cx| connections.show_only(None, cx));
        cx.emit(Finished);
    }

    #[cfg(test)]
    pub fn step(&self) -> usize {
        self.step
    }
}

impl Render for Setup {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::of(cx);
        let (section, title, why, required) = STEPS[self.step];
        let ready = self.connections.read(cx).is_set_up(section, cx);
        let last = self.step + 1 == STEPS.len();
        let rail = STEPS.iter().enumerate().map(|(index, (part, name, _, _))| {
            let set = self.connections.read(cx).is_set_up(*part, cx);
            let color = if index == self.step {
                palette.ink
            } else if set {
                palette.signal(Signal::Done)
            } else {
                palette.faint
            };
            ui::button(("setup-step", index), format!("Step {}: {name}", index + 1), {
                h_flex()
                    .gap(px(8.))
                    .py(px(4.))
                    .child(ui::square(if set {
                        palette.signal(Signal::Done)
                    } else {
                        palette.line
                    }))
                    .child(ui::caps(format!("{} {name}", index + 1)).text_color(color))
            })
            .on_click(cx.listener(move |this, _, window, cx| this.go(index, window, cx)))
        });
        let next_word = if last {
            "finish"
        } else if ready || required {
            "next"
        } else {
            "skip"
        };
        let can_go = ready || !required;
        let next = can_go.then(|| {
            ui::button(
                "setup-next",
                if last { "Finish setup" } else { "Next step" },
                ui::primary_face(next_word, None, &palette),
            )
            .on_click(cx.listener(move |this, _, window, cx| {
                if last {
                    this.finish(cx);
                } else {
                    this.go(this.step + 1, window, cx);
                }
            }))
        });
        let back = (self.step > 0).then(|| {
            ui::button("setup-back", "Previous step", ui::quiet_face("back", &palette))
                .on_click(cx.listener(|this, _, window, cx| this.go(this.step.saturating_sub(1), window, cx)))
        });
        h_flex()
            .size_full()
            .items_start()
            .child(
                v_flex()
                    .flex_none()
                    .w(px(220.))
                    .h_full()
                    .px(px(20.))
                    .py(px(24.))
                    .gap(px(6.))
                    .border_r_1()
                    .border_color(palette.line)
                    .child(ui::caps("set up maverick").text_color(palette.muted).pb(px(10.)))
                    .children(rail)
                    .child(div().flex_1())
                    .child(
                        h_flex().child(
                            ui::button("setup-later", "Set up later", ui::quiet_face("later", &palette))
                                .on_click(cx.listener(|this, _, _, cx| this.finish(cx))),
                        ),
                    ),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .child(
                        v_flex()
                            .px(px(28.))
                            .pt(px(24.))
                            .gap(px(8.))
                            .child(
                                div()
                                    .id("setup-title")
                                    .test_support()
                                    .role(gpui_kit::Role::Label)
                                    .aria_label(SharedString::from(format!(
                                        "{title} · {}",
                                        if ready { "ready" } else { "not yet" }
                                    )))
                                    .child(ui::serif(title).text_size(px(22.)).line_height(px(28.))),
                            )
                            .child(ui::serif(why).text_color(palette.muted).max_w(px(640.)))
                            .child(
                                h_flex()
                                    .gap(px(8.))
                                    .pt(px(6.))
                                    .children(back)
                                    .children(next)
                                    .children((!can_go).then(|| {
                                        ui::caps("set up one to go on").text_color(palette.signal(Signal::Yours))
                                    })),
                            ),
                    )
                    .child(div().flex_1().min_h_0().child(self.connections.clone())),
            )
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{AppContext as _, TestAppContext, WindowOptions};
    use maverick_core::providers::{Choices, Provider, Status};

    use super::Setup;
    use crate::connections::Connections;

    #[gpui_kit::test]
    fn the_first_step_waits_for_a_model(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::ui::install(cx);
        });
        let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let background = runtime.handle().clone();
        let browser = cx.update(|cx| {
            cx.new(|cx| {
                crate::browser::Browser::new(
                    maverick_core::bridge::Bridge::new(),
                    background,
                    Ok("/x".into()),
                    None,
                    None,
                    cx,
                )
            })
        });
        let (window, setup) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                let connections = cx.new(|cx| Connections::new(browser, window, cx));
                cx.new(|cx| Setup::new(connections, cx))
            })
            .unwrap()
        });
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(setup.read(cx).step(), 0);
            assert_eq!(window.find("setup-title").label(), Some("Choose a model · not yet"));
            assert!(window.try_find("setup-next").is_none());

            let models = setup.read(cx).connections.read(cx).models();
            let ready = HashMap::from([(Provider::Codex, Status::Present("installed".into()))]);
            models.update(cx, |models, cx| models.show(ready, Choices::default(), window, cx));
            window.render_frame(cx);
            assert_eq!(window.find("setup-title").label(), Some("Choose a model · ready"));
            assert!(window.try_find("setup-next").is_some());
            assert!(window.try_find("setup-later").is_some());
        })
        .unwrap();
    }
}
