mod band;
mod board;
mod browser;
mod cli;
mod connections;
mod icon;
mod jobs;
mod main_view;
mod mav;
mod models;
mod pane_widths;
mod queue;
mod report;
mod schedule;
mod setup;
mod tabs;
mod ui;

use std::error::Error;
use std::io;
use std::path::Path;
use std::process::ExitCode;

use gpui_kit::component::TitleBar;
use gpui_kit::{AppContext as _, KeyBinding, TitlebarOptions, WindowBounds, WindowOptions, actions, point, px, size};
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
        Some("run") => return cli::run(),
        Some("tracker") => return cli::tracker(&arguments.collect::<Vec<_>>()),
        Some("google") => return cli::google(&arguments.collect::<Vec<_>>()),
        Some("ticktick") => return cli::ticktick(&arguments.collect::<Vec<_>>()),
        Some("models") => return cli::models(&arguments.collect::<Vec<_>>()),
        Some("mav") => return cli::mav(&arguments.collect::<Vec<_>>().join(" ")),
        Some("score") => {
            let mut limit = maverick_core::score::RUN_CAP;
            let mut dry_run = false;
            while let Some(argument) = arguments.next() {
                match argument.as_str() {
                    "--dry-run" => dry_run = true,
                    "--limit" => {
                        limit = arguments
                            .next()
                            .and_then(|count| count.parse().ok())
                            .ok_or("usage: maverick score [--limit <count>] [--dry-run]")?;
                    }
                    _ => return Err("usage: maverick score [--limit <count>] [--dry-run]".into()),
                }
            }
            return cli::score(limit, dry_run);
        }
        Some("check-scores") => {
            let count = match arguments.next() {
                Some(count) => count.parse().map_err(|_| "usage: maverick check-scores [count]")?,
                None => 20,
            };
            return cli::check_scores(count);
        }
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
            let widths_file = maverick_core::data_dir().map(|dir| dir.join("pane-widths.json"));
            cx.set_global(pane_widths::PaneWidths::load(widths_file));
            ui::install(cx);
            cx.bind_keys([KeyBinding::new("cmd-q", Quit, None)]);
            cx.on_action(|_: &Quit, cx| cx.quit());
            cx.on_window_closed(|cx, _| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();

            let answers_file = maverick_core::answers_path();
            let database = maverick_core::database_path();
            let browser = cx.new(|cx| {
                Browser::new(
                    bridge,
                    background.clone(),
                    extension_dir,
                    answers_file.clone(),
                    database.clone(),
                    cx,
                )
            });
            let lists = schedule::stored_lists(database.as_deref());
            let funnel = database
                .clone()
                .zip(answers_file.clone())
                .map(|(database, answers)| schedule::Funnel {
                    database,
                    answers,
                    background: background.clone(),
                });
            let icon_file = maverick_core::data_dir().map(|dir| dir.join("icon"));
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::centered(size(px(1080.), px(720.)), cx)),
                window_min_size: Some(size(px(760.), px(520.))),
                // The bar is the title bar, with the traffic lights centered in it.
                titlebar: Some(TitlebarOptions {
                    title: Some("Maverick".into()),
                    traffic_light_position: Some(point(px(14.), px(13.))),
                    ..TitleBar::title_bar_options()
                }),
                ..TitleBar::window_options()
            };
            gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| {
                    let mut view = MainView::new(browser, lists, icon_file, !setup::done(), window, cx);
                    if let Some(funnel) = funnel {
                        view.run_funnel(funnel, window, cx);
                    }
                    view
                })
            })
            .expect("the main window opens");
            cx.activate(true);
        });
    drop(runtime);
    Ok(())
}
