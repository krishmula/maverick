mod browser;
mod cli;
mod jobs;
mod main_view;
mod tabs;

use std::error::Error;
use std::io;
use std::path::Path;
use std::process::ExitCode;

use gpui_kit::component::Theme;
use gpui_kit::{AppContext as _, KeyBinding, TitlebarOptions, WindowBounds, WindowOptions, actions, px, size};
use maverick_core::bridge::{self, Bridge};
use maverick_core::extension;

use crate::browser::Browser;
use crate::main_view::MainView;

actions!(maverick, [Quit]);

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("maverick: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let mut arguments = std::env::args().skip(1);
    match arguments.next().as_deref() {
        // Chrome starts a native messaging host with the calling extension's origin
        // as its first argument. In that case this process is the relay, not the app.
        Some(origin) if origin.starts_with("chrome-extension://") => return Ok(maverick_core::relay::run()?),
        Some("import-tracker") => {
            let file = arguments
                .next()
                .ok_or("usage: maverick import-tracker <tracker.xlsx>")?;
            return cli::import_tracker(Path::new(&file));
        }
        Some("check-rules") => return cli::check_rules(),
        Some(other) => return Err(format!("{other:?} is not a maverick command").into()),
        None => {}
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

            let answers_file = maverick_core::answers_path();
            let browser = cx.new(|cx| Browser::new(bridge, background, extension_dir, answers_file, cx));
            let jobs = stored_jobs();
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
                cx.new(|cx| MainView::new(browser, jobs, window, cx))
            })
            .expect("the main window opens");
            cx.activate(true);
        });
    drop(runtime);
    Ok(())
}

/// The open postings from the local database. A missing or unreadable database gives an empty list.
fn stored_jobs() -> Vec<jobs::Job> {
    let postings = maverick_core::database_path()
        .filter(|path| path.is_file())
        .and_then(|path| maverick_core::store::Store::open(&path).ok())
        .and_then(|store| store.postings().ok())
        .unwrap_or_default();
    jobs::open_jobs(&postings)
}
