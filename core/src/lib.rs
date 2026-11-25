//! Everything in Maverick except the window.

pub mod amazon;
pub mod answers;
pub mod apply;
pub mod ashby;
pub mod auto;
pub mod bridge;
pub mod calendar;
pub mod chats;
pub mod credentials;
pub mod description;
pub mod extension;
pub mod funnel;
pub mod google;
pub mod greenhouse;
pub mod http;
pub mod icims;
pub mod intake;
pub mod jobs;
pub mod linkedin;
pub mod mail;
pub mod mav;
pub mod model;
pub mod outreach;
pub mod page;
pub mod providers;
pub mod relay;
pub mod rules;
pub mod score;
pub mod sheet;
pub mod store;
pub mod text;
pub mod ticktick;
pub mod tracker;
pub mod voice;
pub mod workday;
pub mod xlsx;

use std::path::PathBuf;

const APP_ID: &str = "dev.maverick.app";

const SOCKET_ENV: &str = "MAVERICK_BRIDGE_SOCKET";

pub fn data_dir() -> Option<PathBuf> {
    dirs::data_dir().map(|dir| dir.join(APP_ID))
}

pub fn database_path() -> Option<PathBuf> {
    data_dir().map(|dir| dir.join("maverick.db"))
}

/// The user's standard answers for application forms.
pub fn answers_path() -> Option<PathBuf> {
    data_dir().map(|dir| dir.join("answers.json"))
}

/// The socket the app listens on and the relay connects to.
/// `MAVERICK_BRIDGE_SOCKET` moves both ends, which is how tests avoid the real app.
pub fn socket_path() -> Option<PathBuf> {
    match std::env::var_os(SOCKET_ENV) {
        Some(path) => Some(path.into()),
        None => data_dir().map(|dir| dir.join("bridge.sock")),
    }
}
