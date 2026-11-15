// Sin consola en Windows en compilación de release.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod config;
mod controller;
mod drives;
mod engine;
mod platform;

use eframe::egui;

fn main() -> eframe::Result {
    if !platform::single_instance() {
        platform::show_existing_window();
        return Ok(());
    }
    let start_hidden = std::env::args().any(|a| a == platform::ARG_MINIMIZED);
    let ctl = controller::Controller::new(config::Config::load());

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(platform::WINDOW_TITLE)
            .with_inner_size([720.0, 560.0])
            .with_min_inner_size([520.0, 400.0]),
        ..Default::default()
    };
    eframe::run_native(
        platform::WINDOW_TITLE,
        options,
        Box::new(move |cc| Ok(Box::new(app::App::new(cc, ctl, start_hidden)))),
    )
}
