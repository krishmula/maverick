//! The Models section of Connections: which providers can run, their API keys, and which
//! provider and model each job uses.

use std::collections::HashMap;

use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{h_flex, v_flex};
use gpui_kit::{
    AppContext as _, Context, Div, Entity, InteractiveElement as _, IntoElement, ParentElement as _, Render,
    SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, TestSupportExt as _, Window, div, px,
};
use maverick_core::credentials::Password;
use maverick_core::providers::{self, Choice, Choices, Job, Provider, Status};
use tokio::runtime::Handle;

use crate::ui::{self, Palette, Signal};

pub struct Models {
    background: Handle,
    statuses: HashMap<Provider, Status>,
    keys: HashMap<Provider, Entity<InputState>>,
    choices: Choices,
    model_inputs: HashMap<Job, Entity<InputState>>,
    problem: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl Models {
    pub fn new(background: Handle, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut keys = HashMap::new();
        let mut model_inputs = HashMap::new();
        let mut _subscriptions = Vec::new();
        for provider in Provider::ALL
            .into_iter()
            .filter(|provider| provider.command().is_none())
        {
            let input =
                cx.new(|cx| InputState::new(window, cx).placeholder(format!("Paste your {} API key", provider.name())));
            _subscriptions.push(
                cx.subscribe_in(&input, window, move |this, _, event: &InputEvent, window, cx| {
                    if let InputEvent::PressEnter { .. } = event {
                        this.save_key(provider, window, cx);
                    }
                }),
            );
            keys.insert(provider, input);
        }
        for job in Job::ALL {
            let input = cx.new(|cx| InputState::new(window, cx));
            _subscriptions.push(
                cx.subscribe_in(&input, window, move |this, input, event: &InputEvent, _, cx| {
                    if matches!(event, InputEvent::Change) {
                        let model = input.read(cx).value().trim().to_owned();
                        this.set_model(job, model, cx);
                    }
                }),
            );
            model_inputs.insert(job, input);
        }
        Self {
            background,
            statuses: HashMap::new(),
            keys,
            choices: Choices::default(),
            model_inputs,
            problem: None,
            _subscriptions,
        }
    }

    /// Reads the saved choices and takes a quick look at every provider, at no cost.
    pub fn refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.choices = Choices::current();
        self.show_choices(window, cx);
        for provider in Provider::ALL {
            let looked = self.background.spawn(providers::look(provider));
            self.settle(provider, looked, cx);
        }
    }

    fn show_choices(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for job in Job::ALL {
            let choice = self.choices.for_job(job);
            let default = choice.provider.default_model();
            let placeholder = if default.is_empty() {
                "its default model".to_owned()
            } else {
                default.to_owned()
            };
            self.model_inputs[&job].update(cx, |input, cx| {
                input.set_placeholder(placeholder, window, cx);
                input.set_value(choice.model.clone(), window, cx);
            });
        }
    }

    /// Shows `status` for `provider` once the background work gives it.
    fn settle(&mut self, provider: Provider, status: tokio::task::JoinHandle<Status>, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let status = status.await.unwrap_or_else(|error| Status::Failed(error.to_string()));
            this.update(cx, |this, cx| {
                this.statuses.insert(provider, status);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Runs a real check: a key is tried against the provider, a command answers a tiny question.
    fn test(&mut self, provider: Provider, cx: &mut Context<Self>) {
        self.statuses.insert(provider, Status::Present("testing".to_owned()));
        cx.notify();
        let model = Job::ALL
            .into_iter()
            .map(|job| self.choices.for_job(job))
            .find(|choice| choice.provider == provider)
            .map(|choice| choice.model)
            .unwrap_or_default();
        let checked = self
            .background
            .spawn(async move { providers::check(provider, &model).await });
        self.settle(provider, checked, cx);
    }

    fn save_key(&mut self, provider: Provider, window: &mut Window, cx: &mut Context<Self>) {
        let Some(input) = self.keys.get(&provider) else {
            return;
        };
        let key = input.read(cx).value().trim().to_owned();
        if key.is_empty() {
            return;
        }
        input.update(cx, |input, cx| input.set_value("", window, cx));
        self.statuses
            .insert(provider, Status::Present("checking the key".to_owned()));
        cx.notify();
        let saved = self.background.spawn(async move {
            match providers::save_key(provider, &Password::new(key)).await {
                Ok(()) => providers::check(provider, "").await,
                Err(error) => Status::Failed(error),
            }
        });
        self.settle(provider, saved, cx);
    }

    fn forget_key(&mut self, provider: Provider, cx: &mut Context<Self>) {
        let forgot = self.background.spawn(async move {
            match providers::forget_key(provider).await {
                Ok(()) => Status::Missing("no key".to_owned()),
                Err(error) => Status::Failed(error),
            }
        });
        self.settle(provider, forgot, cx);
    }

    fn pick(&mut self, job: Job, provider: Provider, window: &mut Window, cx: &mut Context<Self>) {
        self.choices.set(
            job,
            Choice {
                provider,
                model: String::new(),
            },
        );
        self.save_choices(cx);
        self.show_choices(window, cx);
    }

    fn set_model(&mut self, job: Job, model: String, cx: &mut Context<Self>) {
        let mut choice = self.choices.for_job(job);
        if choice.model == model {
            return;
        }
        choice.model = model;
        self.choices.set(job, choice);
        self.save_choices(cx);
    }

    fn save_choices(&mut self, cx: &mut Context<Self>) {
        self.problem = match providers::choices_path() {
            Some(file) => self.choices.save(&file).err(),
            None => Some("this system has no application data directory".to_owned()),
        };
        cx.notify();
    }

    #[cfg(test)]
    pub fn show(
        &mut self,
        statuses: HashMap<Provider, Status>,
        choices: Choices,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.statuses = statuses;
        self.choices = choices;
        self.show_choices(window, cx);
        cx.notify();
    }
}

fn status_of(status: Option<&Status>) -> (String, Signal) {
    match status {
        None => ("looking".to_owned(), Signal::Model),
        Some(Status::Ready(detail)) => (detail.clone(), Signal::Done),
        Some(Status::Present(detail)) => (detail.clone(), Signal::Model),
        Some(Status::Missing(detail)) => (detail.clone(), Signal::Yours),
        Some(Status::Failed(detail)) => (detail.clone(), Signal::Failed),
    }
}

impl Render for Models {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::of(cx);
        let ready = Provider::ALL
            .iter()
            .filter(|provider| matches!(self.statuses.get(provider), Some(Status::Ready(_) | Status::Present(_))))
            .count();
        let mut section = v_flex().gap(px(8.)).child(
            h_flex()
                .justify_between()
                .child(
                    div()
                        .id("models-heading")
                        .test_support()
                        .role(gpui_kit::Role::Label)
                        .aria_label(format!("Models · {ready} ready"))
                        .child(ui::caps(format!("models · {ready} ready")).text_color(palette.muted)),
                )
                .child(ui::caps("keys stay in the keychain").text_color(palette.faint)),
        );
        for provider in Provider::ALL {
            section = section.child(self.provider_row(provider, &palette, cx));
        }
        section = section.child(div().h(px(8.)));
        for job in Job::ALL {
            section = section.child(self.job_row(job, &palette, cx));
        }
        section.children(
            self.problem
                .clone()
                .map(|problem| ui::serif(problem).text_color(palette.signal(Signal::Failed))),
        )
    }
}

impl Models {
    fn provider_row(&self, provider: Provider, palette: &Palette, cx: &mut Context<Self>) -> Div {
        let status = self.statuses.get(&provider);
        let (text, signal) = status_of(status);
        let is_api = provider.command().is_none();
        let has_key = matches!(
            status,
            Some(Status::Ready(_) | Status::Present(_)) | Some(Status::Failed(_))
        ) && is_api;
        let id = SharedString::from(format!("provider-{}", provider.name()));
        let mut actions = h_flex().gap(px(8.)).flex_none();
        let mut key_entry = None;
        if is_api && !has_key {
            key_entry = Some(
                h_flex()
                    .gap(px(8.))
                    .pl(px(40.))
                    .pt(px(4.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .max_w(px(360.))
                            .child(Input::new(&self.keys[&provider]).mask_toggle()),
                    )
                    .child(
                        ui::button(
                            SharedString::from(format!("save-key-{}", provider.name())),
                            format!("Save the {} key", provider.name()),
                            ui::quiet_face("save", palette),
                        )
                        .on_click(cx.listener(move |this, _, window, cx| this.save_key(provider, window, cx))),
                    ),
            );
        } else if !matches!(status, Some(Status::Missing(_))) {
            actions = actions.child(
                ui::button(
                    SharedString::from(format!("test-{}", provider.name())),
                    format!("Test {}", provider.name()),
                    ui::quiet_face("test", palette),
                )
                .on_click(cx.listener(move |this, _, _, cx| this.test(provider, cx))),
            );
            if is_api {
                actions = actions.child(
                    ui::button(
                        SharedString::from(format!("forget-key-{}", provider.name())),
                        format!("Remove the {} key", provider.name()),
                        ui::quiet_face("remove", palette),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| this.forget_key(provider, cx))),
                );
            }
        }
        let help = matches!(status, Some(Status::Missing(_)))
            .then(|| ui::serif(provider.help()).text_size(px(12.5)).text_color(palette.faint));
        v_flex()
            .py(px(6.))
            .border_b_1()
            .border_color(palette.line)
            .child(
                h_flex()
                    .gap(px(12.))
                    .child(
                        ui::caps(if is_api { "api" } else { "cli" })
                            .w(px(28.))
                            .flex_none()
                            .text_color(palette.faint),
                    )
                    .child(ui::serif(provider.name()).w(px(110.)).flex_none())
                    .child(
                        div()
                            .id(id)
                            .test_support()
                            .role(gpui_kit::Role::Label)
                            .aria_label(format!("{} · {text}", provider.name()))
                            .flex_1()
                            .min_w_0()
                            .child(
                                h_flex()
                                    .gap(px(6.))
                                    .child(ui::square(palette.signal(signal)))
                                    .child(ui::caps(text).truncate().text_color(palette.signal(signal))),
                            ),
                    )
                    .child(actions),
            )
            .children(key_entry)
            .children(help.map(|help| help.pl(px(40.))))
    }

    fn job_row(&self, job: Job, palette: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let chosen = self.choices.for_job(job);
        let chips = Provider::ALL.into_iter().enumerate().map(|(index, provider)| {
            let on = chosen.provider == provider;
            let usable = !matches!(self.statuses.get(&provider), Some(Status::Missing(_)));
            let color = if on {
                palette.ink
            } else if usable {
                palette.muted
            } else {
                palette.faint
            };
            let id = (SharedString::from(format!("pick-{job:?}")), index);
            let mut chip = ui::button(
                id,
                format!("{} uses {}", job.name(), provider.name()),
                ui::boxed(provider.name(), color),
            );
            if !usable && !on {
                chip = chip.tip(provider.help());
            }
            chip.on_click(cx.listener(move |this, _, window, cx| this.pick(job, provider, window, cx)))
        });
        v_flex()
            .id(SharedString::from(format!("job-{job:?}")))
            .test_support()
            .role(gpui_kit::Role::Label)
            .aria_label(format!("{} uses {}", job.name(), chosen.label()))
            .gap(px(6.))
            .py(px(6.))
            .child(
                h_flex()
                    .gap(px(10.))
                    .child(ui::caps(job.name()).flex_1().text_color(palette.muted))
                    .child(ui::caps("model").flex_none().text_color(palette.faint))
                    .child(
                        div()
                            .w(px(180.))
                            .flex_none()
                            .child(Input::new(&self.model_inputs[&job]).aria_label(format!("{} model", job.name()))),
                    ),
            )
            .child(h_flex().w_full().gap(px(4.)).flex_wrap().children(chips))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{AppContext as _, TestAppContext, WindowOptions};
    use maverick_core::providers::{Choice, Choices, Job, Provider, Status};

    use super::Models;

    #[gpui_kit::test]
    fn providers_show_their_status_and_jobs_their_choice(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::ui::install(cx);
        });
        let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let background = runtime.handle().clone();
        let (window, view) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Models::new(background, window, cx))
            })
            .unwrap()
        });
        let statuses = HashMap::from([
            (Provider::ClaudeCode, Status::Ready("answered a test".into())),
            (Provider::Codex, Status::Present("installed".into())),
            (Provider::OpenCode, Status::Missing("not installed".into())),
            (Provider::Anthropic, Status::Missing("no key".into())),
            (
                Provider::OpenAi,
                Status::Failed("OpenAI answered with status 401: bad key".into()),
            ),
            (Provider::OpenRouter, Status::Missing("no key".into())),
        ]);
        let mut choices = Choices::default();
        choices.set(
            Job::Scoring,
            Choice {
                provider: Provider::Codex,
                model: "gpt-5-codex".into(),
            },
        );
        cx.update_window(window, |_, window, cx| {
            view.update(cx, |view, cx| view.show(statuses, choices, window, cx));
            window.render_frame(cx);
            assert_eq!(window.find("models-heading").label(), Some("Models · 2 ready"));
            assert_eq!(
                window.find("provider-Claude Code").label(),
                Some("Claude Code · answered a test")
            );
            assert_eq!(
                window.find("provider-OpenAI").label(),
                Some("OpenAI · OpenAI answered with status 401: bad key")
            );
            assert_eq!(
                window.find("job-Mav").label(),
                Some("Mav chat uses Claude Code · sonnet")
            );
            assert_eq!(
                window.find("job-Scoring").label(),
                Some("Scoring uses Codex · gpt-5-codex")
            );
            assert!(window.try_find("save-key-Anthropic").is_some());
            assert!(window.try_find("test-OpenAI").is_some());
        })
        .unwrap();
    }
}
