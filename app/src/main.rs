mod browser;
mod main_view;
mod tabs;

use std::error::Error;
use std::io;

use gpui_kit::component::Theme;
use gpui_kit::{AppContext as _, KeyBinding, TitlebarOptions, WindowBounds, WindowOptions, actions, px, size};
use maverick_core::bridge::{self, Bridge};
use maverick_core::extension;

use crate::browser::Browser;
use crate::main_view::MainView;

actions!(maverick, [Quit]);

fn main() -> Result<(), Box<dyn Error>> {
    // Chrome starts a native messaging host with the calling extension's origin
    // as its first argument. In that case this process is the relay, not the app.
    if std::env::args()
        .nth(1)
        .is_some_and(|arg| arg.starts_with("chrome-extension://"))
    {
        return Ok(maverick_core::relay::run()?);
    }

    let socket = maverick_core::socket_path().ok_or("this system has no application data directory")?;
    let listener = match bridge::bind(&socket) {
        Ok(listener) => listener,
        Err(error) if error.kind() == io::ErrorKind::AddrInUse => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let extension_dir = extension::install(&std::env::current_exe()?);

    // The bridge runs on tokio. GPUI has its own executor and only draws.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()?;
    let bridge = Bridge::new();
    runtime.spawn({
        let bridge = bridge.clone();
        async move {
            if let Err(error) = bridge.serve(listener).await {
                eprintln!("maverick bridge stopped: {error}");
            }
        }
    });

    let background = runtime.handle().clone();
    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
            Theme::sync_system_appearance(None, cx);
            cx.bind_keys([KeyBinding::new("cmd-q", Quit, None)]);
            cx.on_action(|_: &Quit, cx| cx.quit());
            cx.on_window_closed(|cx, _| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();

            let browser = cx.new(|cx| Browser::new(bridge, background, extension_dir, cx));
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::centered(size(px(880.), px(620.)), cx)),
                window_min_size: Some(size(px(420.), px(360.))),
                titlebar: Some(TitlebarOptions {
                    title: Some("Maverick".into()),
                    ..Default::default()
                }),
                ..Default::default()
            };
            gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| MainView::new(browser, window, cx))
            })
            .expect("the main window opens");
            cx.activate(true);
        });
    drop(runtime);
    Ok(())
}
