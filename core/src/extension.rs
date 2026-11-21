//! Puts the browser extension on disk and registers the relay with Chrome.

use std::fs;
use std::path::{Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const MANIFEST: &str = include_str!("../../extension/manifest.json");
const BACKGROUND: &str = include_str!("../../extension/background.js");

pub const NATIVE_HOST: &str = "dev.maverick.bridge";

/// Chrome derives an extension's id from the public key in its manifest.
pub fn extension_id() -> String {
    #[derive(Deserialize)]
    struct Manifest {
        key: String,
    }
    let manifest: Manifest = serde_json::from_str(MANIFEST).expect("extension/manifest.json has a key");
    let public_key = STANDARD.decode(manifest.key).expect("the manifest key is base64");
    Sha256::digest(public_key)[..16]
        .iter()
        .flat_map(|byte| [byte >> 4, byte & 0x0f])
        .map(|nibble| char::from(b'a' + nibble))
        .collect()
}

pub fn native_host_manifest(relay: &Path) -> Value {
    json!({
        "name": NATIVE_HOST,
        "description": "Connects Chrome to the Maverick app on this computer.",
        "path": relay,
        "type": "stdio",
        "allowed_origins": [format!("chrome-extension://{}/", extension_id())],
    })
}

/// Returns the folder to load into Chrome as an unpacked extension.
/// Safe to run on every start: it rewrites a file only when its content changed.
pub fn install(relay: &Path) -> Result<PathBuf, String> {
    let extension_dir = crate::data_dir()
        .ok_or("This system has no application data directory.")?
        .join("extension");
    let chrome = chrome_dir()
        .filter(|dir| dir.is_dir())
        .ok_or("Google Chrome is not set up for this user.")?;
    install_into(&extension_dir, &chrome.join("NativeMessagingHosts"), relay)?;
    Ok(extension_dir)
}

pub fn install_into(extension_dir: &Path, hosts_dir: &Path, relay: &Path) -> Result<(), String> {
    write_if_changed(&extension_dir.join("manifest.json"), MANIFEST.as_bytes())?;
    write_if_changed(&extension_dir.join("background.js"), BACKGROUND.as_bytes())?;
    let host = serde_json::to_vec_pretty(&native_host_manifest(relay)).expect("the host manifest is plain JSON");
    write_if_changed(&hosts_dir.join(format!("{NATIVE_HOST}.json")), &host)
}

fn chrome_dir() -> Option<PathBuf> {
    if cfg!(target_os = "macos") {
        dirs::data_dir().map(|dir| dir.join("Google/Chrome"))
    } else {
        dirs::config_dir().map(|dir| dir.join("google-chrome"))
    }
}

fn write_if_changed(path: &Path, content: &[u8]) -> Result<(), String> {
    if fs::read(path).is_ok_and(|existing| existing == content) {
        return Ok(());
    }
    let describe = |error: std::io::Error| format!("Could not write {}: {error}", path.display());
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(describe)?;
    }
    fs::write(path, content).map_err(describe)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_id_is_the_one_chrome_computes() {
        // Computed from the same key with openssl and shasum, not with this code.
        assert_eq!(extension_id(), "hgfmgkeplahclefhpnifbjeglojglnlp");
    }

    #[test]
    fn install_registers_the_relay_for_this_extension_only() {
        let scratch = tempfile::tempdir().unwrap();
        let extension_dir = scratch.path().join("extension");
        let hosts_dir = scratch.path().join("hosts");

        install_into(&extension_dir, &hosts_dir, Path::new("/opt/maverick")).unwrap();

        let host: Value =
            serde_json::from_slice(&fs::read(hosts_dir.join("dev.maverick.bridge.json")).unwrap()).unwrap();
        assert_eq!(
            host,
            json!({
                "name": "dev.maverick.bridge",
                "description": "Connects Chrome to the Maverick app on this computer.",
                "path": "/opt/maverick",
                "type": "stdio",
                "allowed_origins": ["chrome-extension://hgfmgkeplahclefhpnifbjeglojglnlp/"],
            })
        );
        assert_eq!(
            fs::read_to_string(extension_dir.join("manifest.json")).unwrap(),
            MANIFEST
        );
        assert_eq!(
            fs::read_to_string(extension_dir.join("background.js")).unwrap(),
            BACKGROUND
        );
    }
}
