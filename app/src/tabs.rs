//! The searchable list of browser tabs.

use gpui_kit::component::list::{ListDelegate, ListItem, ListState};
use gpui_kit::component::{ActiveTheme as _, IndexPath, h_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{App, Context, ElementId, ParentElement as _, SharedString, Styled as _, Task, Window, div};
use maverick_core::bridge::{ConnectionId, Tab};

#[derive(Default)]
pub struct TabList {
    tabs: Vec<Tab>,
    query: String,
    matches: Vec<Tab>,
    selected: Option<IndexPath>,
}

impl TabList {
    pub fn set_tabs(&mut self, tabs: Vec<Tab>) {
        self.tabs = tabs;
        self.matches = matching(&self.tabs, &self.query);
    }

    pub fn tab_at(&self, ix: IndexPath) -> Option<&Tab> {
        self.matches.get(ix.row)
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
    // Tab ids repeat across profiles, so the connection is part of the identity.
    let tab_id = u64::from(u32::from_ne_bytes(tab_id.to_ne_bytes()));
    ElementId::NamedInteger("tab".into(), (u64::from(connection) << 32) | tab_id)
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
        let title = if held.tab.title.is_empty() {
            &held.tab.url
        } else {
            &held.tab.title
        };
        let row = h_flex()
            .gap_3()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .child(SharedString::from(title.clone())),
            )
            .child(
                div()
                    .flex_none()
                    .max_w_2_5()
                    .truncate()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(SharedString::from(host_of(&held.tab.url).to_owned())),
            )
            .when(held.tab.active, |row| {
                row.child(
                    div()
                        .flex_none()
                        .text_xs()
                        .text_color(cx.theme().success)
                        .child("Active"),
                )
            });
        Some(
            ListItem::new(row_id(held.connection, held.tab.id))
                .selected(self.selected == Some(ix))
                .child(row),
        )
    }

    fn render_empty(&mut self, _: &mut Window, cx: &mut Context<ListState<Self>>) -> impl gpui_kit::IntoElement {
        div()
            .p_3()
            .text_color(cx.theme().muted_foreground)
            .child(SharedString::from(format!("No tab matches \"{}\".", self.query)))
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
