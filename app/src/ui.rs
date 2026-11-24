//! The Design System: three fonts, a dark and a light palette, four signals, and the parts every view shares.
//!
//! Hue means state and nothing else. Text you read is serif, labels and figures are mono,
//! and scores are dot-matrix. Corners are square.

use std::borrow::Cow;
use std::cell::Cell;

use gpui_kit::base::TestSupportExt as _;
use gpui_kit::component::list::ListItem;
use gpui_kit::component::text::TextView;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{ActiveTheme as _, Theme, ThemeRegistry, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, AppContext as _, ClickEvent, Context, Div, DragMoveEvent, ElementId, FontWeight, Hsla,
    InteractiveElement as _, IntoElement, MouseButton, ParentElement as _, Pixels, Role, SharedString,
    StatefulInteractiveElement as _, Styled as _, Window, div, px, rgb,
};

use crate::pane_widths::{Pane, PaneWidths};

/// Labels, counts, navigation and anything that lines up in columns.
pub const MONO: &str = "Geist Mono";
/// Text the user reads: titles, field names, sentences. It has one weight.
pub const SERIF: &str = "Geist Serif";
/// The wordmark and scores.
pub const DOTS: &str = "Doto";

/// Geist Mono and Geist Serif (the Square cut) come from vercel/geist-font 1.9.2.
/// Doto is the weight 900, square dot instance of google/fonts ofl/doto.
/// Both are under the Open Font License, which sits next to the files.
const FONTS: [&[u8]; 5] = [
    include_bytes!("../assets/fonts/GeistMono-Regular.ttf"),
    include_bytes!("../assets/fonts/GeistMono-Medium.ttf"),
    include_bytes!("../assets/fonts/GeistMono-SemiBold.ttf"),
    include_bytes!("../assets/fonts/GeistSerif-Square.ttf"),
    include_bytes!("../assets/fonts/Doto-Black.ttf"),
];

const THEMES: &str = include_str!("../assets/theme.json");

/// Loads the fonts and makes the Maverick themes the ones the system appearance picks between.
/// Call it once, after `gpui_kit::init`.
pub fn install(cx: &mut App) {
    cx.text_system()
        .add_fonts(FONTS.iter().map(|font| Cow::Borrowed(*font)).collect())
        .expect("the bundled fonts load");
    // A family that did not load would be swapped for a system font without a word.
    let loaded = cx.text_system().all_font_names();
    for family in [MONO, SERIF, DOTS] {
        if !loaded.iter().any(|name| name == family) {
            eprintln!("maverick: the {family} font did not load");
        }
    }
    let registry = ThemeRegistry::global_mut(cx);
    registry
        .load_themes_from_str(THEMES)
        .expect("the bundled theme file parses");
    let themes = registry.themes();
    let (light, dark) = (themes["Maverick Light"].clone(), themes["Maverick Dark"].clone());
    let theme = Theme::global_mut(cx);
    theme.light_theme = light;
    theme.dark_theme = dark;
    Theme::sync_system_appearance(None, cx);
}

/// The colors of the current appearance.
#[derive(Clone, Copy)]
pub struct Palette {
    pub background: Hsla,
    /// The faint grid behind everything.
    pub grid: Hsla,
    pub line: Hsla,
    pub ink: Hsla,
    pub muted: Hsla,
    pub faint: Hsla,
    yours: Hsla,
    /// Text on a `Signal::Yours` fill.
    pub on_yours: Hsla,
    model: Hsla,
    done: Hsla,
    failed: Hsla,
}

impl Palette {
    pub fn of(cx: &App) -> Self {
        let theme = cx.theme();
        let dark = theme.mode.is_dark();
        Self {
            background: theme.background,
            grid: theme.foreground.opacity(if dark { 0.035 } else { 0.04 }),
            line: theme.border,
            ink: theme.foreground,
            muted: theme.muted_foreground,
            faint: rgb(if dark { 0x575551 } else { 0xA9A7A2 }).into(),
            yours: theme.warning,
            on_yours: theme.warning_foreground,
            model: theme.info,
            done: theme.success,
            failed: theme.danger,
        }
    }

    pub fn signal(&self, signal: Signal) -> Hsla {
        match signal {
            Signal::Yours => self.yours,
            Signal::Model => self.model,
            Signal::Done => self.done,
            Signal::Failed => self.failed,
        }
    }
}

/// The four states that get a hue.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Signal {
    /// Needs the user: a field left open, a draft to read, the Submit click.
    Yours,
    /// A model answered or drafted this, so the user should check it.
    Model,
    /// It worked: Chrome is connected, a form opened, an application went through.
    Done,
    /// Maverick tried and could not.
    Failed,
}

/// Small uppercase mono text, for labels and counts.
pub fn caps(text: impl AsRef<str>) -> Div {
    div()
        .font_family(MONO)
        .text_size(px(10.))
        .line_height(px(14.))
        .child(SharedString::from(text.as_ref().to_uppercase()))
}

/// A label in an outlined box.
pub fn boxed(text: impl AsRef<str>, color: Hsla) -> Div {
    caps(text)
        .flex_none()
        .px(px(5.))
        .border_1()
        .border_color(color.opacity(0.55))
        .text_color(color)
}

/// A boxed label in a signal's color.
pub fn chip(signal: Signal, text: impl AsRef<str>, palette: &Palette) -> Div {
    boxed(text, palette.signal(signal))
}

/// The square that marks one thing: a form field, a connection, a list entry.
pub fn square(color: Hsla) -> Div {
    div().flex_none().size(px(6.)).bg(color)
}

/// Serif text at reading size.
pub fn serif(text: impl Into<SharedString>) -> Div {
    div()
        .font_family(SERIF)
        .text_size(px(14.5))
        .line_height(px(19.))
        .child(text.into())
}

/// A score in dot-matrix figures. An unscored posting shows two dashes.
pub fn score(score: Option<u8>, size: f32) -> Div {
    div()
        .flex_none()
        .font_family(DOTS)
        .font_weight(FontWeight::BLACK)
        .text_size(px(size))
        .line_height(px(size))
        .child(SharedString::from(
            score.map_or("--".to_owned(), |score| score.to_string()),
        ))
}

/// Labelled values, one per line. A value that is empty is left out. Values can be selected and copied.
pub fn facts(facts: &[(&str, String)], palette: &Palette) -> Div {
    v_flex().gap(px(7.)).children(
        facts
            .iter()
            .filter(|(_, value)| !value.is_empty())
            .map(|(label, value)| {
                h_flex()
                    .items_start()
                    .gap_3()
                    .child(caps(label).w(px(72.)).pt(px(1.)).text_color(palette.faint))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_color(palette.muted)
                            .child(selectable(format!("fact-{label}"), value)),
                    )
            }),
    )
}

/// Text the user can select with the pointer and copy with ⌘C. It takes the color and font around it.
pub fn selectable(id: impl Into<ElementId>, text: &str) -> TextView {
    TextView::markdown(id, literal_markdown(text)).selectable(true)
}

/// Markdown that shows `text` as it is: no emphasis, links or lists, and each line break kept.
fn literal_markdown(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len() + text.len() / 8);
    for (ix, line) in text.lines().enumerate() {
        if ix > 0 {
            escaped.push_str("\\\n");
        }
        for ch in line.chars() {
            if ch.is_ascii_punctuation() {
                escaped.push('\\');
            }
            escaped.push(ch);
        }
    }
    escaped
}

/// Something to click. `name` is what a screen reader and the tests call it.
/// The click stays with the button: a row or a title bar under it does not also act on it.
pub struct Button {
    id: ElementId,
    name: SharedString,
    face: Div,
    tip: Option<SharedString>,
}

pub fn button(id: impl Into<ElementId>, name: impl Into<SharedString>, face: Div) -> Button {
    Button {
        id: id.into(),
        name: name.into(),
        face,
        tip: None,
    }
}

impl Button {
    /// What the pointer resting on the button explains.
    pub fn tip(mut self, tip: impl Into<SharedString>) -> Self {
        self.tip = Some(tip.into());
        self
    }

    pub fn on_click(self, on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static) -> impl IntoElement {
        self.face
            .id(self.id)
            .when_some(self.tip, |face, tip| {
                face.tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
            })
            .test_support()
            .role(Role::Button)
            .aria_label(self.name)
            .cursor_pointer()
            .on_mouse_down(MouseButton::Left, |_, window, cx| {
                window.prevent_default();
                cx.stop_propagation();
            })
            .on_click(move |event, window, cx| {
                cx.stop_propagation();
                on_click(event, window, cx);
            })
    }
}

/// The face of the one loud button: the user's move. `key` is the key that does the same.
pub fn primary_face(text: impl AsRef<str>, key: Option<&str>, palette: &Palette) -> Div {
    let yours = palette.signal(Signal::Yours);
    div()
        .flex()
        .flex_none()
        .items_center()
        .gap_2()
        .px(px(10.))
        .py(px(5.))
        .rounded(px(2.))
        .bg(yours)
        .hover(|face| face.bg(yours.opacity(0.88)))
        .text_color(palette.on_yours)
        .child(caps(text).font_weight(FontWeight::SEMIBOLD))
        .children(key.map(|key| {
            caps(key)
                .px(px(4.))
                .border_1()
                .border_color(palette.on_yours.opacity(0.4))
        }))
}

/// The face of a quiet button: a boxed label that brightens under the pointer.
pub fn quiet_face(text: impl AsRef<str>, palette: &Palette) -> Div {
    let ink = palette.ink;
    boxed(text, palette.muted)
        .py(px(1.))
        .hover(move |face| face.text_color(ink).border_color(ink.opacity(0.55)))
}

/// A list row. The selected row is framed by corner marks, not filled.
pub fn row(id: impl Into<ElementId>, selected: bool, content: Div, palette: &Palette) -> ListItem {
    ListItem::new(id).p(px(2.)).selected(selected).child(
        div()
            .relative()
            .px(px(12.))
            .py(px(5.))
            .child(content)
            .children(selected.then(|| brackets(palette.ink)).into_iter().flatten()),
    )
}

/// Four corner marks that frame the selected row. The parent must be `relative`.
pub fn brackets(color: Hsla) -> [Div; 4] {
    let mark = || div().absolute().size(px(7.)).border_color(color);
    [
        mark().top_0().left_0().border_t_1().border_l_1(),
        mark().top_0().right_0().border_t_1().border_r_1(),
        mark().bottom_0().left_0().border_b_1().border_l_1(),
        mark().bottom_0().right_0().border_b_1().border_r_1(),
    ]
}

pub const DIVIDER_WIDTH: Pixels = px(8.);

struct PaneDrag {
    pane: Pane,
    start: Cell<Pixels>,
    width: Pixels,
}

struct EmptyDragPreview;

impl gpui_kit::Render for EmptyDragPreview {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

/// A divider's leading pane grows to the right, and its trailing pane grows to the left.
pub fn pane_divider<T: 'static>(
    pane: Pane,
    width: Pixels,
    limits: std::ops::RangeInclusive<Pixels>,
    trailing: bool,
    palette: &Palette,
    cx: &mut Context<T>,
) -> impl IntoElement {
    let id = pane.divider_id();
    let drag = PaneDrag {
        pane,
        start: Cell::new(px(0.)),
        width,
    };
    let direction = if trailing { -1. } else { 1. };
    let line = palette.line;
    let ink = palette.muted;
    div()
        .id(id)
        .test_support()
        .flex_none()
        .w(DIVIDER_WIDTH)
        .h_full()
        .flex()
        .justify_center()
        .cursor_col_resize()
        .group(id)
        .child(
            div()
                .w(px(1.))
                .h_full()
                .bg(line)
                .group_hover(id, move |style| style.bg(ink)),
        )
        .on_drag(drag, |drag, offset, window, cx| {
            // GPUI gives the pointer's offset from the divider when the drag starts.
            let center = window.mouse_position().x - offset.x + DIVIDER_WIDTH / 2.;
            drag.start.set(center);
            cx.new(|_| EmptyDragPreview)
        })
        .on_drag_move(cx.listener(move |_, event: &DragMoveEvent<PaneDrag>, _, cx| {
            let drag = event.drag(cx);
            if drag.pane == pane {
                let width = drag.width + (event.event.position.x - drag.start.get()) * direction;
                pane.set_width(Some(width.clamp(*limits.start(), *limits.end())), cx);
                cx.notify();
            }
        }))
        .on_mouse_up(MouseButton::Left, |_, _, cx| PaneWidths::save(cx))
        .on_mouse_up_out(MouseButton::Left, |_, _, cx| PaneWidths::save(cx))
        .on_click(cx.listener(move |_, event: &ClickEvent, _, cx| {
            if event.click_count() == 2 {
                pane.set_width(None, cx);
                PaneWidths::save(cx);
                cx.notify();
            }
        }))
}

#[cfg(test)]
mod tests {
    use gpui_kit::TestAppContext;
    use gpui_kit::component::ThemeMode;

    use super::*;

    #[gpui_kit::test]
    fn each_appearance_has_its_own_palette_and_four_distinct_signals(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            install(cx);
            let mut inks = Vec::new();
            for mode in [ThemeMode::Dark, ThemeMode::Light] {
                Theme::change(mode, None, cx);
                assert_eq!(
                    cx.theme().theme_name().as_ref(),
                    if mode.is_dark() {
                        "Maverick Dark"
                    } else {
                        "Maverick Light"
                    }
                );
                let palette = Palette::of(cx);
                let hues =
                    [Signal::Yours, Signal::Model, Signal::Done, Signal::Failed].map(|signal| palette.signal(signal));
                for (ix, hue) in hues.iter().enumerate() {
                    assert!(!hues[..ix].contains(hue), "two signals share a color");
                    assert_ne!(*hue, palette.ink);
                }
                inks.push(palette.ink);
            }
            assert_ne!(inks[0], inks[1]);
        });
    }
}
