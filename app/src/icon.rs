//! The app icon the user picked. The bundle ships Sunset; the others replace it in the Dock while the app runs.

use std::fs;
use std::path::Path;
use std::sync::{Arc, LazyLock};

use gpui_kit::{Image, ImageFormat};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AppIcon {
    Stealth,
    #[default]
    Sunset,
    Arctic,
    Neon,
    Crimson,
}

impl AppIcon {
    pub const ALL: [AppIcon; 5] = [Self::Stealth, Self::Sunset, Self::Arctic, Self::Neon, Self::Crimson];

    pub fn name(self) -> &'static str {
        match self {
            Self::Stealth => "Stealth",
            Self::Sunset => "Sunset",
            Self::Arctic => "Arctic",
            Self::Neon => "Neon",
            Self::Crimson => "Crimson",
        }
    }

    fn png(self) -> &'static [u8] {
        match self {
            Self::Stealth => include_bytes!("../../assets/icon/stealth.png"),
            Self::Sunset => include_bytes!("../../assets/icon/sunset.png"),
            Self::Arctic => include_bytes!("../../assets/icon/arctic.png"),
            Self::Neon => include_bytes!("../../assets/icon/neon.png"),
            Self::Crimson => include_bytes!("../../assets/icon/crimson.png"),
        }
    }

    pub fn image(self) -> Arc<Image> {
        static IMAGES: LazyLock<Vec<Arc<Image>>> = LazyLock::new(|| {
            AppIcon::ALL
                .into_iter()
                .map(|icon| Arc::new(Image::from_bytes(ImageFormat::Png, icon.png().to_vec())))
                .collect()
        });
        IMAGES[self as usize].clone()
    }

    fn parse(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|icon| icon.name().eq_ignore_ascii_case(name.trim()))
    }

    /// The choice saved in `file`. A missing file or unknown name gives the default.
    pub fn saved(file: Option<&Path>) -> Self {
        file.and_then(|file| fs::read_to_string(file).ok())
            .and_then(|name| Self::parse(&name))
            .unwrap_or_default()
    }

    pub fn save(self, file: &Path) -> std::io::Result<()> {
        if let Some(dir) = file.parent() {
            fs::create_dir_all(dir)?;
        }
        fs::write(file, self.name())
    }

    /// Shows this icon in the Dock until the app quits. Call it on the main thread.
    pub fn show_in_dock(self) {
        use objc2::{AllocAnyThread as _, MainThreadMarker};
        use objc2_app_kit::{NSApplication, NSImage};
        use objc2_foundation::NSData;

        let Some(main_thread) = MainThreadMarker::new() else {
            return;
        };
        let data = NSData::with_bytes(self.png());
        let Some(image) = NSImage::initWithData(NSImage::alloc(), &data) else {
            return;
        };
        // SAFETY: on the main thread, with an image AppKit decoded itself.
        unsafe { NSApplication::sharedApplication(main_thread).setApplicationIconImage(Some(&image)) };
    }
}

#[cfg(test)]
mod tests {
    use super::AppIcon;

    #[test]
    fn a_saved_name_reads_back_as_its_icon() {
        for icon in AppIcon::ALL {
            assert_eq!(AppIcon::parse(&format!("{}\n", icon.name().to_lowercase())), Some(icon));
        }
        assert_eq!(AppIcon::parse("Afterburner"), None);
    }
}
