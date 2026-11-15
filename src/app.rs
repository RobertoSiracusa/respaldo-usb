//! Interfaz gráfica (egui).

use crate::config::{self, RegisteredDisk};
use crate::controller::Controller;
use crate::drives::{self, Drive};
use crate::engine::{self, Phase};
use crate::{fsx, lock, platform};
use eframe::egui;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const GREEN: egui::Color32 = egui::Color32::from_rgb(40, 150, 70);
const ORANGE: egui::Color32 = egui::Color32::from_rgb(200, 120, 0);
const RED: egui::Color32 = egui::Color32::from_rgb(210, 50, 50);
/// Días sin respaldo terminado a partir de los cuales se avisa en color.
const STALE_DAYS: i64 = 7;
/// Plazo para ocultar la ventana al iniciar con Windows; pasado ese tiempo ya no se oculta sola.
const HIDE_WINDOW_WITHIN: Duration = Duration::from_secs(10);

fn local_time(unix_secs: i64) -> Option<chrono::DateTime<chrono::Local>> {
    chrono::DateTime::from_timestamp(unix_secs, 0).map(|t| t.with_timezone(&chrono::Local))
}

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Backup,
    Folders,
    Disks,
    Settings,
}

pub struct App {
    ctl: Arc<Controller>,
    tab: Tab,
    excludes_text: String,
    message: Option<String>,
    autostart: bool,
    quit: Arc<AtomicBool>,
    /// Ocultar la ventana en cuanto aparezca (inicio con Windows). Se anula si el usuario la pide antes.
    hide_pending: Arc<AtomicBool>,
    started: Instant,
    /// Disco elegido cuando hay más de uno conectado (id del marcador).
    selected_disk: Option<String>,
    has_tray: bool,
    #[cfg(windows)]
    _tray: Option<tray_icon::TrayIcon>,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, ctl: Arc<Controller>, start_hidden: bool) -> Self {
        let ctx = cc.egui_ctx.clone();
        *lock(&ctl.wake) = Some(Box::new({
            let ctx = ctx.clone();
            move || ctx.request_repaint()
        }));
        let quit = Arc::new(AtomicBool::new(false));
        let hide_pending = Arc::new(AtomicBool::new(start_hidden));

        #[cfg(windows)]
        let tray = {
            use raw_window_handle::{HasWindowHandle, RawWindowHandle};
            let hwnd = match cc.window_handle().map(|h| h.as_raw()) {
                Ok(RawWindowHandle::Win32(h)) => h.hwnd.get(),
                _ => 0,
            };
            ctl.hwnd.store(hwnd, Ordering::Relaxed);
            // El usuario pide la ventana: ya no hay que ocultarla, y la interfaz debe despertar.
            let on_show = {
                let (ctx, pending) = (ctx.clone(), hide_pending.clone());
                move || {
                    pending.store(false, Ordering::Relaxed);
                    ctx.request_repaint();
                }
            };
            platform::listen_show_requests(hwnd, on_show.clone());
            platform::tray::create(hwnd, quit.clone(), on_show)
        };
        #[cfg(windows)]
        let has_tray = tray.is_some();
        #[cfg(not(windows))]
        let has_tray = false;

        ctl.refresh_drives();
        ctl.spawn_watcher();
        let excludes_text = lock(&ctl.config).excludes.join("\n");
        Self {
            ctl,
            tab: Tab::Backup,
            excludes_text,
            message: None,
            autostart: platform::autostart_enabled(),
            quit,
            hide_pending,
            started: Instant::now(),
            selected_disk: None,
            has_tray,
            #[cfg(windows)]
            _tray: tray,
        }
    }

    fn save_config(&mut self) {
        if let Err(e) = lock(&self.ctl.config).save() {
            self.message = Some(format!("No se pudo guardar la configuración: {e}"));
        }
    }

    fn hwnd(&self) -> isize {
        self.ctl.hwnd.load(Ordering::Relaxed)
    }

    fn tab_backup(&mut self, ui: &mut egui::Ui) {
        let ready = self.ctl.ready_disks();
        let running = self.ctl.is_running();
        let n_sources = lock(&self.ctl.config).sources.len();

        let chosen = ready
            .iter()
            .position(|d| d.marker.as_ref().map(|m| &m.id) == self.selected_disk.as_ref())
            .unwrap_or(0);
        match ready.get(chosen) {
            Some(d) if ready.len() > 1 => {
                ui.add_enabled_ui(!running, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("Disco de destino:").color(GREEN).strong());
                        egui::ComboBox::from_id_salt("disco_destino")
                            .selected_text(drive_title(d))
                            .show_ui(ui, |ui| {
                                for option in &ready {
                                    let id = option.marker.as_ref().map(|m| m.id.clone());
                                    ui.selectable_value(&mut self.selected_disk, id, drive_title(option));
                                }
                            });
                    });
                });
            }
            Some(d) => {
                ui.label(
                    egui::RichText::new(format!("Disco conectado: {}", drive_title(d)))
                        .color(GREEN)
                        .strong(),
                );
            }
            None => {
                ui.label(
                    egui::RichText::new("Ningún disco de respaldo conectado")
                        .color(ORANGE)
                        .strong(),
                );
                ui.label("Conecta un disco registrado, o registra uno en la pestaña Discos.");
            }
        }
        if n_sources == 0 {
            ui.label("No hay carpetas seleccionadas. Añádelas en la pestaña Carpetas.");
        }
        ui.add_space(8.0);

        if running {
            let p = lock(&self.ctl.reporter.progress).clone();
            let title = match p.phase {
                Phase::Scanning => "Contando archivos…",
                Phase::Copying => "Respaldando…",
                Phase::Verifying => "Verificando disco…",
                Phase::Idle => "Terminando…",
            };
            ui.heading(title);
            let frac = if p.phase == Phase::Scanning || p.bytes_total == 0 {
                if p.files_total > 0 && p.phase != Phase::Scanning {
                    p.files_done as f32 / p.files_total as f32
                } else {
                    0.0
                }
            } else {
                p.bytes_done as f32 / p.bytes_total as f32
            };
            ui.add(egui::ProgressBar::new(frac.clamp(0.0, 1.0)).show_percentage());
            ui.label(format!(
                "{} de {} archivos · {} de {}",
                p.files_done,
                p.files_total,
                human(p.bytes_done.min(p.bytes_total)),
                human(p.bytes_total)
            ));
            ui.add(egui::Label::new(egui::RichText::new(&p.current).small()).truncate());
            if ui.button("Cancelar").clicked() {
                self.ctl.cancel();
            }
            ui.label("No desconectes el disco mientras trabaja.");
        } else {
            ui.horizontal(|ui| {
                let can = !ready.is_empty() && n_sources > 0;
                let big = egui::Button::new(egui::RichText::new("Respaldar ahora").size(18.0));
                if ui.add_enabled(can, big).clicked() {
                    self.ctl.start_backup(&ready[chosen].root);
                }
                if ui
                    .add_enabled(!ready.is_empty(), egui::Button::new("Verificar disco"))
                    .on_hover_text("Relee todo el respaldo de este servidor y lo compara con los hashes guardados. Lo dañado se vuelve a copiar en el siguiente respaldo.")
                    .clicked()
                {
                    self.ctl.start_verify(&ready[chosen].root);
                }
            });
        }

        ui.add_space(8.0);
        ui.separator();
        let state = lock(&self.ctl.state);
        // Lo primero que debe verse: cuánto hace que este servidor tiene un respaldo terminado.
        match state.last_backup_finished.and_then(local_time) {
            Some(when) => {
                let days = (chrono::Local::now() - when).num_days();
                let age = match days {
                    0 => "hoy".to_string(),
                    1 => "ayer".to_string(),
                    n => format!("hace {n} días"),
                };
                let color = if days > STALE_DAYS {
                    ORANGE
                } else {
                    ui.visuals().text_color()
                };
                ui.label(
                    egui::RichText::new(format!(
                        "Último respaldo terminado: {} ({age})",
                        when.format("%Y-%m-%d %H:%M")
                    ))
                    .color(color),
                );
            }
            None => {
                ui.label(egui::RichText::new("Este servidor aún no tiene ningún respaldo terminado.").color(ORANGE));
            }
        }
        let Some(last) = state.last_run.as_ref() else {
            return;
        };
        let s = &last.summary;
        let kind = if s.verify { "Verificación" } else { "Respaldo" };
        let (text, color) = if let Some(f) = &s.fatal {
            (format!("{kind} DETENIDO: {f}"), RED)
        } else if s.cancelled {
            (format!("{kind} cancelado"), ORANGE)
        } else if s.issues_total > 0 {
            (format!("{kind} terminado con {} incidencias", s.issues_total), ORANGE)
        } else {
            (format!("{kind} completo"), GREEN)
        };
        ui.add_space(4.0);
        ui.label(egui::RichText::new(text).color(color).strong().size(16.0));
        let when = local_time(last.when)
            .map(|w| w.format("%Y-%m-%d %H:%M").to_string())
            .unwrap_or_default();
        ui.label(format!("{when} · disco {} · {:.0} s", last.disk, s.seconds));
        if s.verify {
            ui.label(format!("Archivos correctos: {}", s.unchanged));
        } else {
            ui.label(format!(
                "Copiados: {} ({}) · Sin cambios: {} · Conservados sin origen: {}",
                s.copied,
                human(s.copied_bytes),
                s.unchanged + s.adopted,
                s.only_in_backup
            ));
            if s.links_skipped > 0 {
                ui.label(format!(
                    "Enlaces y uniones omitidos: {} (lista en el reporte)",
                    s.links_skipped
                ));
            }
        }
        for note in &s.notes {
            ui.label(egui::RichText::new(format!("Aviso: {note}")).color(ORANGE));
        }
        if s.ok() && !s.verify {
            ui.label("Antes de desconectar, usa \"Quitar hardware de forma segura\".");
        }
        if let Some(report) = &last.report
            && ui.link("Abrir reporte").clicked()
        {
            open_path(report);
        }
        if !s.issues.is_empty() {
            ui.add_space(4.0);
            let shown = s.issues.len() as u64;
            let title = if shown < s.issues_total {
                format!(
                    "Omitidos o con error (primeros {shown} de {}; todos en el reporte):",
                    s.issues_total
                )
            } else {
                "Omitidos o con error:".to_string()
            };
            ui.label(egui::RichText::new(title).strong());
            egui::ScrollArea::vertical().auto_shrink([false, false]).show_rows(
                ui,
                ui.text_style_height(&egui::TextStyle::Body),
                s.issues.len(),
                |ui, range| {
                    for (path, why) in &s.issues[range] {
                        ui.add(egui::Label::new(format!("{path} — {why}")).truncate());
                    }
                },
            );
        }
    }

    /// Añade carpetas evitando las que no tienen sentido: el propio disco de respaldo o duplicados anidados.
    fn add_sources(&mut self, picked: Vec<PathBuf>) {
        let backup_disks: Vec<PathBuf> = self.ctl.ready_disks().into_iter().map(|d| d.root).collect();
        let mut rejected = Vec::new();
        {
            let mut cfg = lock(&self.ctl.config);
            for p in picked {
                if backup_disks.iter().any(|d| engine::path_contains(d, &p)) {
                    rejected.push(format!("{} está en el disco de respaldo", p.display()));
                } else if let Some(parent) = cfg.sources.iter().find(|s| engine::path_contains(s, &p)) {
                    rejected.push(format!("{} ya está incluida en {}", p.display(), parent.display()));
                } else {
                    // La nueva carpeta abarca a las que ya estaban dentro de ella.
                    cfg.sources.retain(|s| !engine::path_contains(&p, s));
                    cfg.sources.push(p);
                }
            }
        }
        if !rejected.is_empty() {
            self.message = Some(format!("No se añadió: {}", rejected.join("; ")));
        }
        self.save_config();
    }

    fn tab_folders(&mut self, ui: &mut egui::Ui) {
        let running = self.ctl.is_running();
        ui.label("Carpetas de este servidor que se respaldan:");
        let mut remove = None;
        let sources = lock(&self.ctl.config).sources.clone();
        egui::ScrollArea::vertical()
            .max_height(220.0)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                for (i, src) in sources.iter().enumerate() {
                    ui.horizontal(|ui| {
                        if ui.add_enabled(!running, egui::Button::new("Quitar")).clicked() {
                            remove = Some(i);
                        }
                        ui.label(src.display().to_string());
                        if !src.is_dir() {
                            ui.label(egui::RichText::new("(no accesible)").color(RED));
                        }
                    });
                }
            });
        if let Some(i) = remove {
            lock(&self.ctl.config).sources.remove(i);
            self.save_config();
        }
        if ui.add_enabled(!running, egui::Button::new("Añadir carpeta…")).clicked()
            && let Some(picked) = rfd::FileDialog::new().set_title("Carpetas a respaldar").pick_folders()
        {
            self.add_sources(picked);
        }
        ui.label("Quitar una carpeta no borra lo ya respaldado en el disco.");

        ui.add_space(10.0);
        ui.separator();
        ui.label("Exclusiones (un patrón por línea; * y ? como comodines, sin distinguir mayúsculas):");
        let edit = egui::TextEdit::multiline(&mut self.excludes_text)
            .desired_rows(6)
            .desired_width(f32::INFINITY);
        if ui.add_enabled(!running, edit).changed() {
            lock(&self.ctl.config).excludes = self
                .excludes_text
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(String::from)
                .collect();
            self.save_config();
        }
        ui.label(egui::RichText::new("Ejemplos: *.tmp   ~$*   node_modules   *\\Cache\\*").small());
        let bad = engine::invalid_patterns(&lock(&self.ctl.config).excludes);
        if !bad.is_empty() {
            ui.label(egui::RichText::new(format!("Patrones mal escritos, se ignoran: {}", bad.join("   "))).color(RED));
        }
    }

    fn tab_disks(&mut self, ui: &mut egui::Ui) {
        let running = self.ctl.is_running();
        let list = lock(&self.ctl.drives).clone();
        ui.horizontal(|ui| {
            ui.label("Discos detectados:");
            if ui.small_button("Actualizar").clicked() {
                self.ctl.refresh_drives();
            }
        });
        if list.is_empty() {
            ui.label("Ninguno. Conecta el disco extraíble.");
        }
        for d in &list {
            let registered = d
                .marker
                .as_ref()
                .is_some_and(|m| lock(&self.ctl.config).is_registered(&m.id));
            ui.horizontal(|ui| {
                if registered {
                    ui.label(egui::RichText::new("✔ registrado").color(GREEN));
                } else if ui
                    .add_enabled(!running, egui::Button::new("Usar para respaldo"))
                    .clicked()
                {
                    self.register(d);
                }
                ui.label(drive_title(d));
                if !d.removable {
                    ui.label(egui::RichText::new("(disco fijo o disco duro USB)").small());
                }
            });
            if d.filesystem.eq_ignore_ascii_case("FAT32") || d.filesystem.eq_ignore_ascii_case("FAT") {
                ui.label(
                    egui::RichText::new("    Formato FAT32: no admite archivos de más de 4 GB. Mejor NTFS o exFAT.")
                        .color(ORANGE),
                );
            }
        }

        ui.add_space(10.0);
        ui.separator();
        ui.label("Discos autorizados en este servidor:");
        let disks = lock(&self.ctl.config).disks.clone();
        if disks.is_empty() {
            ui.label("Ninguno todavía.");
        }
        let mut forget = None;
        for (i, disk) in disks.iter().enumerate() {
            ui.horizontal(|ui| {
                if ui.add_enabled(!running, egui::Button::new("Olvidar")).clicked() {
                    forget = Some(i);
                }
                ui.label(format!("{} (id {})", disk.name, disk.id));
            });
        }
        if let Some(i) = forget {
            lock(&self.ctl.config).disks.remove(i);
            self.save_config();
        }
        ui.label("Olvidar un disco no borra su contenido. El mismo disco puede registrarse en varios servidores: cada uno guarda en su propia carpeta.");
    }

    fn register(&mut self, d: &Drive) {
        let name = if d.label.is_empty() {
            d.root.display().to_string()
        } else {
            d.label.clone()
        };
        match drives::register(&d.root, &name) {
            Ok(marker) => {
                {
                    let mut cfg = lock(&self.ctl.config);
                    if !cfg.is_registered(&marker.id) {
                        cfg.disks.push(RegisteredDisk {
                            id: marker.id,
                            name: marker.name,
                        });
                    }
                }
                self.save_config();
                self.ctl.refresh_drives();
            }
            Err(e) => self.message = Some(format!("No se pudo preparar el disco {}: {e}", d.root.display())),
        }
    }

    fn tab_settings(&mut self, ui: &mut egui::Ui) {
        let mut cfg = lock(&self.ctl.config).clone();
        let mut changed = false;
        changed |= ui
            .checkbox(
                &mut cfg.auto_on_connect,
                "Respaldar automáticamente al conectar un disco registrado",
            )
            .on_hover_text("La app debe estar abierta (puede estar oculta en la bandeja).")
            .changed();
        changed |= ui
            .checkbox(&mut cfg.verify_after_copy, "Releer cada archivo copiado para confirmar la escritura")
            .on_hover_text("Más lento, sobre todo con muchos archivos pequeños, pero detecta fallos del disco o del cable al momento.")
            .changed();
        if cfg!(windows) {
            changed |= ui
                .checkbox(&mut cfg.preserve_acl, "Copiar también los permisos NTFS (solo discos NTFS)")
                .on_hover_text("Guarda propietario y permisos de cada archivo y carpeta. Úsalo ejecutando la app como administrador; para restaurar con permisos, copia de vuelta con robocopy /COPYALL.")
                .changed();
        }
        if changed {
            *lock(&self.ctl.config) = cfg;
            self.save_config();
        }

        if cfg!(windows)
            && ui
                .checkbox(&mut self.autostart, "Iniciar con Windows, oculta en la bandeja")
                .on_hover_text(
                    "Se inicia al abrir sesión con este usuario. Sin sesión abierta no hay respaldo automático.",
                )
                .changed()
            && let Err(e) = platform::set_autostart(self.autostart)
        {
            self.message = Some(format!("No se pudo cambiar el inicio automático: {e}"));
            self.autostart = platform::autostart_enabled();
        }
        if cfg!(windows) {
            if fsx::has_backup_privileges() {
                ui.label("Ejecutando como administrador: puede leer archivos aunque sus permisos no lo permitan.");
            } else {
                ui.label(
                    egui::RichText::new(
                        "Sin privilegios de administrador: los archivos que este usuario no puede leer se omiten.",
                    )
                    .color(ORANGE),
                );
            }
        }

        ui.add_space(10.0);
        ui.separator();
        ui.label(format!("Servidor: {}", config::hostname()));
        ui.label(format!(
            "Destino en el disco: {}\\{}\\…",
            config::APP_DIR,
            config::hostname()
        ));
        ui.label(format!("Configuración y reportes: {}", config::data_dir().display()));
        if ui.link("Abrir carpeta de reportes").clicked() {
            let dir = config::logs_dir();
            let _ = std::fs::create_dir_all(&dir);
            open_path(&dir);
        }
    }
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // eframe muestra la ventana tras pintar el primer cuadro; se oculta en cuanto aparece.
        // Sin bandeja no se oculta: no habría forma de volver a abrirla.
        if self.hide_pending.load(Ordering::Relaxed) {
            if !self.has_tray || self.started.elapsed() > HIDE_WINDOW_WITHIN {
                self.hide_pending.store(false, Ordering::Relaxed);
            } else if platform::is_visible(self.hwnd()) {
                self.hide_pending.store(false, Ordering::Relaxed);
                platform::hide_window(self.hwnd());
            } else {
                ctx.request_repaint_after(Duration::from_millis(50));
            }
        }

        let quitting = self.quit.load(Ordering::Relaxed);
        if ctx.input(|i| i.viewport().close_requested()) && !quitting {
            if self.has_tray {
                // Cerrar la ventana solo la oculta: la app sigue en la bandeja esperando discos.
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                platform::hide_window(self.hwnd());
            } else if self.ctl.is_running() {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                self.quit.store(true, Ordering::Relaxed);
            }
        }
        if quitting {
            // Salida ordenada: cancelar, esperar a que el hilo guarde el manifiesto y cerrar.
            if self.ctl.is_running() {
                self.ctl.cancel();
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                ctx.request_repaint_after(Duration::from_millis(100));
            } else {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
        if self.ctl.is_running() {
            ctx.request_repaint_after(Duration::from_millis(200));
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.tab, Tab::Backup, "Respaldo");
                ui.selectable_value(&mut self.tab, Tab::Folders, "Carpetas");
                ui.selectable_value(&mut self.tab, Tab::Disks, "Discos");
                ui.selectable_value(&mut self.tab, Tab::Settings, "Ajustes");
            });
            ui.separator();
            if let Some(msg) = self.message.clone() {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(msg).color(RED));
                    if ui.small_button("Cerrar aviso").clicked() {
                        self.message = None;
                    }
                });
            }
            match self.tab {
                Tab::Backup => self.tab_backup(ui),
                Tab::Folders => self.tab_folders(ui),
                Tab::Disks => self.tab_disks(ui),
                Tab::Settings => self.tab_settings(ui),
            }
        });
    }
}

fn drive_title(d: &Drive) -> String {
    let mut s = d.root.display().to_string();
    if !d.label.is_empty() {
        s.push_str(&format!(" [{}]", d.label));
    }
    if let (Some(free), Some(total)) = (d.free, d.total) {
        s.push_str(&format!(" · {} libres de {}", human(free), human(total)));
    }
    if !d.filesystem.is_empty() {
        s.push_str(&format!(" · {}", d.filesystem));
    }
    s
}

fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

fn open_path(path: &Path) {
    let program = if cfg!(windows) { "explorer" } else { "xdg-open" };
    let _ = std::process::Command::new(program).arg(path).spawn();
}
