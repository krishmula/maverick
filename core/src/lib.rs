//! Everything in Maverick except the window.

pub mod answers;
pub mod apply;
pub mod ashby;
pub mod bridge;
pub mod credentials;
pub mod description;
pub mod extension;
pub mod funnel;
pub mod greenhouse;
pub mod intake;
pub mod jobs;
pub mod model;
pub mod page;
pub mod relay;
pub mod rules;
pub mod score;
pub mod store;
pub mod text;
pub mod tracker;
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
