//! The searchable list of browser tabs.

use gpui_kit::component::list::{ListDelegate, ListItem, ListState};
use gpui_kit::component::{IndexPath, h_flex, v_flex};
use gpui_kit::{App, Context, ElementId, Entity, ParentElement as _, Styled as _, Task, Window};
use maverick_core::apply::System;
use maverick_core::bridge::{ConnectionId, Tab};

use crate::browser::{Browser, Fill, Subject};
use crate::report::Tally;
use crate::ui::{self, Palette, Signal};

pub struct TabList {
    browser: Entity<Browser>,
    tabs: Vec<Tab>,
    query: String,
    matches: Vec<Tab>,
    selected: Option<IndexPath>,
}

impl TabList {
    pub fn new(browser: Entity<Browser>) -> Self {
        Self {
            browser,
            tabs: Vec::new(),
            query: String::new(),
            matches: Vec::new(),
            selected: None,
        }
    }

    pub fn set_tabs(&mut self, tabs: Vec<Tab>) {
        self.tabs = tabs;
        self.matches = matching(&self.tabs, &self.query);
    }

    pub fn tab_at(&self, ix: IndexPath) -> Option<&Tab> {
        self.matches.get(ix.row)
    }

    pub fn selected_tab(&self) -> Option<&Tab> {
        self.tab_at(self.selected?)
    }

    pub fn len(&self) -> usize {
        self.matches.len()
    }
}

/// Tabs whose title or address contains every word of the query, in any order.
pub fn matching(tabs: &[Tab], query: &str) -> Vec<Tab> {
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    tabs.iter()
        .filter(|held| {
            let text = format!("{} {}", held.tab.title, held.tab.url).to_lowercase();
            words.iter().all(|word| text.contains(word))
        })
        .cloned()
        .collect()
}

/// The part of an address a person recognizes a site by.
pub fn host_of(url: &str) -> &str {
    let Some((_, rest)) = url.split_once("://") else {
        return url;
    };
    let host = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    if host.is_empty() { url } else { host }
}

pub(crate) fn row_id(connection: ConnectionId, tab_id: i32) -> ElementId {
    tab_element("tab", connection, tab_id)
}

/// The button that fills the application form in a tab.
pub(crate) fn fill_button_id(connection: ConnectionId, tab_id: i32) -> ElementId {
    tab_element("fill-form", connection, tab_id)
}

fn tab_element(name: &'static str, connection: ConnectionId, tab_id: i32) -> ElementId {
    // Tab ids repeat across profiles, so the connection is part of the identity.
    let tab_id = u64::from(u32::from_ne_bytes(tab_id.to_ne_bytes()));
    ElementId::NamedInteger(name.into(), (u64::from(connection) << 32) | tab_id)
}

impl ListDelegate for TabList {
    type Item = ListItem;

    fn items_count(&self, _section: usize, _: &App) -> usize {
        self.matches.len()
    }

    fn perform_search(&mut self, query: &str, _: &mut Window, _: &mut Context<ListState<Self>>) -> Task<()> {
        self.query = query.to_owned();
        self.matches = matching(&self.tabs, &self.query);
        Task::ready(())
    }

    fn render_item(&mut self, ix: IndexPath, _: &mut Window, cx: &mut Context<ListState<Self>>) -> Option<ListItem> {
        let held = self.matches.get(ix.row)?;
        let palette = Palette::of(cx);
        let selected = self.selected == Some(ix);
        let tone = if selected { palette.ink } else { palette.muted };
        let title = if held.tab.title.is_empty() {
            &held.tab.url
        } else {
            &held.tab.title
        };
        let fill = self
            .browser
            .read(cx)
            .fill_of(&Subject::Tab(held.connection, held.tab.id));
        let mark = match fill {
            Some(Fill::Running { .. }) => Some(ui::boxed("filling", palette.muted)),
            Some(Fill::Failed { .. }) => Some(ui::chip(Signal::Failed, "failed", &palette)),
            Some(Fill::Done { report, .. }) => Some(match Tally::of(report) {
                Tally { failed: 1.., .. } => ui::chip(Signal::Failed, "failed", &palette),
                Tally { yours: 0, .. } => ui::chip(Signal::Done, "filled", &palette),
                Tally { yours, .. } => ui::chip(Signal::Yours, format!("{yours} yours"), &palette),
            }),
            Some(Fill::Idle) | None => None,
        };
        let fill_button = System::of(&held.tab.url).is_some().then(|| {
            let browser = self.browser.clone();
            let tab = held.clone();
            let list = cx.entity().downgrade();
            ui::button(
                fill_button_id(held.connection, held.tab.id),
                "Fill form",
                ui::quiet_face("fill form", &palette),
            )
            .tip("Fill this application with your standard answers. You review it and click Submit.")
            .on_click(move |_, window, cx| {
                browser.update(cx, |browser, cx| browser.fill_form(&tab, cx));
                // The report shows for the selected row, so the row that was filled becomes it.
                list.update(cx, |list, cx| list.set_selected_index(Some(ix), window, cx))
                    .ok();
            })
        });
        let row = h_flex()
            .gap_3()
            // The square is solid for the tab Chrome shows in its window.
            .child(ui::square(if held.tab.active { tone } else { palette.line }))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(ui::serif(title.clone()).truncate().text_color(tone))
                    .child(ui::caps(host_of(&held.tab.url)).truncate().text_color(palette.faint)),
            )
            .children(mark)
            .children(fill_button);
        Some(ui::row(row_id(held.connection, held.tab.id), selected, row, &palette))
    }

    fn render_empty(&mut self, _: &mut Window, cx: &mut Context<ListState<Self>>) -> impl gpui_kit::IntoElement {
        ui::serif(format!("No tab matches \"{}\".", self.query))
            .p_4()
            .text_color(Palette::of(cx).muted)
    }

    fn set_selected_index(&mut self, ix: Option<IndexPath>, _: &mut Window, cx: &mut Context<ListState<Self>>) {
        self.selected = ix;
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use maverick_core::bridge::BrowserTab;

    use super::*;

    fn tab(title: &str, url: &str) -> Tab {
        Tab {
            connection: 1,
            tab: BrowserTab {
                id: 1,
                window_id: 1,
                title: title.into(),
                url: url.into(),
                active: false,
            },
        }
    }

    #[test]
    fn a_query_matches_every_word_in_the_title_or_the_address() {
        let tabs = [
            tab("Software Engineer, Infrastructure", "https://jobs.example.com/4821"),
            tab("Inbox", "https://mail.example.com/"),
            tab("Infrastructure notes", "https://docs.example.com/infra"),
        ];

        assert_eq!(matching(&tabs, "INFRA jobs"), [tabs[0].clone()]);
        assert_eq!(matching(&tabs, "infrastructure"), [tabs[0].clone(), tabs[2].clone()]);
        assert_eq!(matching(&tabs, "  "), tabs);
        assert_eq!(matching(&tabs, "payroll"), []);
    }

    #[test]
    fn the_host_is_the_site_part_of_an_address() {
        assert_eq!(host_of("https://jobs.example.com/4821?ref=x"), "jobs.example.com");
        assert_eq!(host_of("chrome://extensions/"), "extensions");
        assert_eq!(host_of("about:blank"), "about:blank");
        assert_eq!(host_of("file:///Users/me/a.html"), "file:///Users/me/a.html");
    }
}
