//! Everything in Maverick except the window.

pub mod bridge;
pub mod extension;
pub mod relay;

use std::path::PathBuf;

const APP_ID: &str = "dev.maverick.app";

const SOCKET_ENV: &str = "MAVERICK_BRIDGE_SOCKET";

pub fn data_dir() -> Option<PathBuf> {
    dirs::data_dir().map(|dir| dir.join(APP_ID))
}

/// The socket the app listens on and the relay connects to.
/// `MAVERICK_BRIDGE_SOCKET` moves both ends, which is how tests avoid the real app.
pub fn socket_path() -> Option<PathBuf> {
    match std::env::var_os(SOCKET_ENV) {
        Some(path) => Some(path.into()),
        None => data_dir().map(|dir| dir.join("bridge.sock")),
    }
}
