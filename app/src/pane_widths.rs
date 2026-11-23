//! The preferred widths of the resizable panes, saved in logical pixels.

use std::collections::BTreeMap;
use std::path::PathBuf;

use gpui_kit::{App, Global, Pixels, px};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Pane {
    ChatHistory,
    MavRail,
    Details,
    ProfileDetail,
}

impl Pane {
    pub fn divider_id(self) -> &'static str {
        match self {
            Self::ChatHistory => "resize-chat-history",
            Self::MavRail => "resize-mav-rail",
            Self::Details => "resize-details",
            Self::ProfileDetail => "resize-profile-detail",
        }
    }

    pub fn width(self, cx: &App) -> Option<Pixels> {
        cx.try_global::<PaneWidths>()?
            .widths
            .get(&self)
            .copied()
            .filter(|width| width.is_finite() && *width > 0.)
            .map(px)
    }

    pub fn set_width(self, width: Option<Pixels>, cx: &mut App) {
        let state = cx.default_global::<PaneWidths>();
        let previous = match width {
            Some(width) => state.widths.insert(self, width.into()),
            None => state.widths.remove(&self),
        };
        state.dirty |= previous != width.map(f32::from);
    }
}

#[derive(Default)]
pub struct PaneWidths {
    file: Option<PathBuf>,
    widths: BTreeMap<Pane, f32>,
    dirty: bool,
}

impl Global for PaneWidths {}

impl PaneWidths {
    pub fn load(file: Option<PathBuf>) -> Self {
        let widths = file
            .as_ref()
            .and_then(|file| std::fs::read(file).ok())
            .and_then(|data| serde_json::from_slice(&data).ok())
            .unwrap_or_default();
        Self {
            file,
            widths,
            dirty: false,
        }
    }

    pub fn save(cx: &mut App) {
        if !cx.try_global::<Self>().is_some_and(|state| state.dirty) {
            return;
        }
        let state = cx.global_mut::<Self>();
        if let Some(file) = &state.file {
            let temp = file.with_extension(format!("{}.tmp", std::process::id()));
            let data = serde_json::to_vec(&state.widths).expect("pane widths serialize");
            let result = file
                .parent()
                .map_or(Ok(()), std::fs::create_dir_all)
                .and_then(|()| std::fs::write(&temp, data))
                .and_then(|()| std::fs::rename(&temp, file));
            if let Err(error) = result {
                eprintln!("maverick: could not save pane widths to {}: {error}", file.display());
                return;
            }
        }
        state.dirty = false;
    }
}
