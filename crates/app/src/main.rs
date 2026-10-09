//! GitDragon desktop app entry point.

mod actions;
mod clone_dialog;
mod panels;
mod rail;
mod repo_tab;
mod startup;
mod state;
mod welcome;
mod workspace;

use gpui_kit::component::TitleBar;
use gpui_kit::{AppContext as _, Bounds, WindowBounds, WindowOptions, px, size};
use tracing_subscriber::EnvFilter;
use ui::assets::AppAssets;
use ui::{Appearance, theme};

use crate::actions::*;
use crate::state::SavedState;
use crate::workspace::Workspace;

fn main() {
    startup::begin();
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("GITDRAGON_LOG").unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    let _startup = tracing::info_span!("startup").entered();
    startup::mark("tracing");

    gpui_kit::application().with_assets(AppAssets).run(|cx| {
        startup::mark("platform");
        gpui_kit::init(cx);
        startup::mark("kit_init");
        let saved = SavedState::load();
        // GITDRAGON_THEME overrides the saved theme (screenshots, testing).
        let appearance = std::env::var("GITDRAGON_THEME")
            .ok()
            .or_else(|| saved.appearance.clone())
            .map(|v| Appearance::parse(&v))
            .unwrap_or_default();
        theme::init(appearance, cx);
        startup::mark("theme");
        actions::bind_keys(cx);

        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.on_action(|_: &ToggleDarkMode, cx| {
            theme::toggle_mode(cx);
        });
        cx.on_action(|_: &NextSkin, cx| {
            theme::cycle_skin(cx);
        });
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();

        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                None,
                size(px(1280.), px(800.)),
                cx,
            ))),
            window_min_size: Some(size(px(800.), px(500.))),
            ..TitleBar::window_options()
        };
        gpui_kit::open_window(options, cx, |window, cx| {
            window.set_window_title("GitDragon");
            startup::on_first_frame(window);
            cx.new(|cx| Workspace::new(saved, window, cx))
        })
        .expect("failed to open the main window");
        startup::mark("window_open");
    });
}
