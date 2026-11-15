// Sin consola en Windows en compilación de release.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod config;
mod controller;
mod drives;
mod engine;
mod fsx;
mod platform;

use eframe::egui;
use std::io::Write;
use std::sync::{Mutex, MutexGuard};

/// Toma un mutex aunque otro hilo haya fallado con él tomado: un fallo aislado
/// no debe tumbar también la interfaz.
pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Deja constancia de cualquier fallo interno en `errores.log`: en release no hay consola.
fn install_panic_log() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let dir = config::logs_dir();
        let _ = std::fs::create_dir_all(&dir);
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("errores.log"))
        {
            let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
            let trace = std::backtrace::Backtrace::force_capture();
            let _ = writeln!(f, "[{now}] v{} {info}\n{trace}\n", env!("CARGO_PKG_VERSION"));
        }
        default(info);
    }));
}

fn main() -> eframe::Result {
    install_panic_log();
    match platform::claim_instance() {
        platform::Instance::First => {}
        platform::Instance::SameSession => return Ok(()),
        platform::Instance::OtherSession => {
            platform::message_box(
                "Respaldo USB ya está abierto en otra sesión de usuario de este servidor.\n\n\
                 Para usarlo aquí, ciérralo allí: icono de la bandeja, Salir.",
            );
            return Ok(());
        }
    }
    fsx::enable_backup_privileges();
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
