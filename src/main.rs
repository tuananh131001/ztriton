//! ztriton: a native macOS Jellyfin client.
#![expect(
    unused_crate_dependencies,
    reason = "Cargo shares one dependency list between the library, the app binary and the tests"
)]

mod ui;

use gpui::{
    App, AppContext, Application, Bounds, KeyBinding, Menu, MenuItem, SharedString, TitlebarOptions,
    WindowBounds, WindowOptions, point, px, size,
};
use std::path::PathBuf;
use ui::app::{Back, Quit, SignOut, ToggleFullscreen, ZtritonApp};

/// `ZTRITON_PROOF=1`: scripted run that records verification artifacts.
pub fn proof_mode() -> bool {
    std::env::var("ZTRITON_PROOF").is_ok_and(|v| v == "1")
}

/// Where proof mode writes its artifacts.
pub fn proof_dir() -> PathBuf {
    let d = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("proof/playback");
    if let Err(e) = std::fs::create_dir_all(&d) {
        log::warn!("could not create {}: {e}", d.display());
    }
    d
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info,naga=warn,blade_graphics=warn")).init();

    let args: Vec<String> = std::env::args().collect();
    let autoplay = args
        .iter()
        .position(|a| a == "--autoplay")
        .map(|i| args.get(i + 1).cloned().unwrap_or_else(|| "first".into()));

    if proof_mode() {
        if let Err(e) = std::fs::remove_file(proof_dir().join("run.log"))
            && e.kind() != std::io::ErrorKind::NotFound
        {
            log::warn!("could not clear the previous proof log: {e}");
        }
        let (maj, min) = ztriton::player::client_api_version();
        ui::proof_log(format!(
            "start: ztriton {} | rustc target {} | libmpv client API {maj}.{min} | autoplay={autoplay:?}",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::ARCH
        ));
    }

    Application::new().run(move |cx: &mut App| {
        cx.activate(true);
        cx.on_action(|_: &Quit, cx| cx.quit());
        // Closing the (only) window quits; on_app_quit hooks then stop/report playback.
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        cx.bind_keys([
            KeyBinding::new("cmd-q", Quit, None),
            KeyBinding::new("cmd-ctrl-f", ToggleFullscreen, None),
            KeyBinding::new("escape", Back, Some("Ztriton")),
            KeyBinding::new("backspace", Back, Some("Ztriton")),
        ]);
        ui::player_view::bind_keys(cx);
        ui::text_input::bind_keys(cx);
        ui::login::bind_keys(cx);
        cx.set_menus(vec![Menu {
            name: "ztriton".into(),
            items: vec![MenuItem::action("Sign Out", SignOut), MenuItem::separator(), MenuItem::action("Quit ztriton", Quit)],
        }]);

        let bounds = Bounds {
            origin: point(px(60.), px(60.)),
            size: size(px(1280.), px(760.)),
        };
        let opened = cx.open_window(
            WindowOptions {
                titlebar: Some(TitlebarOptions {
                    title: Some(SharedString::from("ztriton")),
                    appears_transparent: false,
                    ..Default::default()
                }),
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |window, cx| cx.new(|cx| ZtritonApp::new(autoplay.clone(), window, cx)),
        );
        if let Err(e) = opened {
            log::error!("could not open the main window: {e:#}");
            cx.quit();
        }
    });
}
