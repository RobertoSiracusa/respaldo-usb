//! Estado compartido entre la interfaz, el vigilante de discos y el hilo de respaldo.
//! Vigilante y respaldo no dependen de la interfaz: funcionan con la ventana oculta.

use crate::config::{self, Config};
use crate::drives::{self, Drive, Snapshot};
use crate::engine::{self, Job, Reporter, Summary};
use crate::{lock, platform};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const POLL: Duration = Duration::from_secs(3);
/// Incidencias que se conservan en el estado guardado; la lista completa va en el reporte.
const STORED_ISSUES: usize = 300;
/// Reportes que se conservan; al superar el tope se borran los más antiguos.
const KEEP_REPORTS: usize = 200;

#[derive(Clone, Serialize, Deserialize)]
pub struct LastRun {
    /// Segundos Unix.
    pub when: i64,
    pub disk: String,
    pub summary: Summary,
    pub report: Option<PathBuf>,
}

/// Lo que se recuerda entre reinicios de la app.
#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    pub last_run: Option<LastRun>,
    /// Último respaldo que llegó al final (segundos Unix), con o sin incidencias.
    pub last_backup_finished: Option<i64>,
}

impl State {
    fn path() -> PathBuf {
        config::data_dir().join("estado.json")
    }

    fn load() -> Self {
        std::fs::read(Self::path())
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    fn save(&self) {
        let path = Self::path();
        let tmp = path.with_extension("json.tmp");
        let written = serde_json::to_vec(self).ok().and_then(|bytes| {
            std::fs::create_dir_all(config::data_dir()).ok()?;
            std::fs::write(&tmp, bytes).ok()
        });
        if written.is_some() {
            let _ = std::fs::rename(tmp, path);
        }
    }
}

#[derive(Default)]
pub struct Controller {
    pub config: Mutex<Config>,
    pub reporter: Reporter,
    pub running: AtomicBool,
    pub state: Mutex<State>,
    pub drives: Mutex<Vec<Drive>>,
    /// HWND de la ventana principal (0 si no aplica).
    pub hwnd: AtomicIsize,
    /// Despierta la interfaz para que repinte.
    pub wake: Mutex<Option<Box<dyn Fn() + Send>>>,
}

#[derive(Clone, Copy, PartialEq)]
enum Op {
    Backup { auto: bool },
    Verify,
}

/// Disco registrado cuya unidad no estaba presente en el sondeo anterior.
/// Se decide por la unidad, no por el marcador: registrar un disco ya conectado
/// no cuenta como conectarlo.
fn newly_connected(prev_roots: &HashSet<PathBuf>, drives: &[Drive], cfg: &Config) -> Option<PathBuf> {
    drives
        .iter()
        .find(|d| !prev_roots.contains(&d.root) && d.marker.as_ref().is_some_and(|m| cfg.is_registered(&m.id)))
        .map(|d| d.root.clone())
}

fn panic_text(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "causa desconocida".into())
}

impl Controller {
    pub fn new(config: Config) -> Arc<Self> {
        Arc::new(Self {
            config: Mutex::new(config),
            state: Mutex::new(State::load()),
            ..Default::default()
        })
    }

    fn wake_ui(&self) {
        if let Some(f) = lock(&self.wake).as_ref() {
            f();
        }
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    pub fn cancel(&self) {
        self.reporter.cancel.store(true, Ordering::Relaxed);
    }

    pub fn refresh_drives(&self) {
        *lock(&self.drives) = Snapshot::take().drives;
    }

    /// Discos conectados que están registrados en este servidor.
    pub fn ready_disks(&self) -> Vec<Drive> {
        let cfg = lock(&self.config);
        lock(&self.drives)
            .iter()
            .filter(|d| d.marker.as_ref().is_some_and(|m| cfg.is_registered(&m.id)))
            .cloned()
            .collect()
    }

    pub fn start_backup(self: &Arc<Self>, disk_root: &Path) -> bool {
        self.start(disk_root, Op::Backup { auto: false })
    }

    pub fn start_verify(self: &Arc<Self>, disk_root: &Path) -> bool {
        self.start(disk_root, Op::Verify)
    }

    fn start(self: &Arc<Self>, disk_root: &Path, op: Op) -> bool {
        if self.running.swap(true, Ordering::SeqCst) {
            return false;
        }
        self.reporter.cancel.store(false, Ordering::Relaxed);
        let this = self.clone();
        let disk_root = disk_root.to_path_buf();
        std::thread::spawn(move || {
            let verify = op == Op::Verify;
            // Un fallo interno no debe dejar la app en "respaldando" para siempre.
            let summary = catch_unwind(AssertUnwindSafe(|| this.run(&disk_root, op))).unwrap_or_else(|payload| {
                Summary::failed(
                    format!(
                        "error interno: {}. Detalle en errores.log de la carpeta de reportes.",
                        panic_text(&*payload)
                    ),
                    verify,
                )
            });
            this.finish(&disk_root, summary);
            this.running.store(false, Ordering::SeqCst);
            if op == (Op::Backup { auto: true }) {
                // Respaldo automático terminado: mostrar el resultado a quien conectó el disco.
                platform::show_window(this.hwnd.load(Ordering::Relaxed));
            }
            this.wake_ui();
        });
        true
    }

    fn run(&self, disk_root: &Path, op: Op) -> Summary {
        let cfg = lock(&self.config).clone();
        let verify = op == Op::Verify;
        let host_dir =
            match engine::resolve_host_dir(&drives::backup_root(disk_root), &config::hostname(), &cfg.machine_id) {
                Ok(dir) => dir,
                Err(e) => return Summary::failed(format!("no se pudo preparar la carpeta del servidor: {e}"), verify),
            };
        if verify {
            return engine::run_verify(&host_dir, &self.reporter);
        }
        let job = Job {
            sources: cfg.sources,
            excludes: cfg.excludes,
            host_dir,
            verify_after_copy: cfg.verify_after_copy,
            preserve_acl: cfg.preserve_acl,
        };
        engine::run_backup(&job, &self.reporter)
    }

    /// Guarda reporte y estado; refresca el espacio libre mostrado.
    fn finish(&self, disk_root: &Path, summary: Summary) {
        let now = SystemTime::now();
        let when = now.duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
        let report = write_report(&summary, now);
        let finished_backup = !summary.verify && summary.finished();
        let mut stored = summary;
        stored.issues.truncate(STORED_ISSUES);
        {
            let mut state = lock(&self.state);
            if finished_backup {
                state.last_backup_finished = Some(when);
            }
            state.last_run = Some(LastRun {
                when,
                disk: disk_root.display().to_string(),
                summary: stored,
                report,
            });
            state.save();
        }
        self.refresh_drives();
    }

    /// Hilo que detecta discos conectados y lanza el respaldo automático.
    /// Solo vuelve a leer los discos cuando cambia el conjunto de unidades.
    pub fn spawn_watcher(self: &Arc<Self>) {
        let this = self.clone();
        std::thread::spawn(move || {
            let mut snap = Snapshot::take();
            *lock(&this.drives) = snap.drives.clone();
            this.wake_ui();
            loop {
                std::thread::sleep(POLL);
                if !snap.is_stale() {
                    continue;
                }
                let prev_roots: HashSet<PathBuf> = snap.drives.iter().map(|d| d.root.clone()).collect();
                snap = Snapshot::take();
                let target = {
                    let cfg = lock(&this.config);
                    (cfg.auto_on_connect && !cfg.sources.is_empty())
                        .then(|| newly_connected(&prev_roots, &snap.drives, &cfg))
                        .flatten()
                };
                *lock(&this.drives) = snap.drives.clone();
                if let Some(root) = target {
                    this.start(&root, Op::Backup { auto: true });
                }
                this.wake_ui();
            }
        });
    }
}

fn write_report(summary: &Summary, when: SystemTime) -> Option<PathBuf> {
    let dir = config::logs_dir();
    std::fs::create_dir_all(&dir).ok()?;
    let stamp: chrono::DateTime<chrono::Local> = when.into();
    let kind = if summary.verify { "verificacion" } else { "respaldo" };
    let path = dir.join(format!("{}_{kind}.txt", stamp.format("%Y-%m-%d_%H%M%S")));
    std::fs::write(&path, engine::report_text(summary, when)).ok()?;
    prune_reports(&dir, KEEP_REPORTS);
    Some(path)
}

/// Deja solo los `keep` reportes más recientes. El nombre empieza por la fecha, así que ordenar por nombre basta.
fn prune_reports(dir: &Path, keep: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut reports: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            name.ends_with("_respaldo.txt") || name.ends_with("_verificacion.txt")
        })
        .collect();
    reports.sort();
    let excess = reports.len().saturating_sub(keep);
    for old in &reports[..excess] {
        let _ = std::fs::remove_file(old);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RegisteredDisk;
    use crate::drives::DiskMarker;

    fn drive(root: &str, id: Option<&str>) -> Drive {
        Drive {
            root: PathBuf::from(root),
            label: String::new(),
            filesystem: String::new(),
            total: None,
            free: None,
            removable: true,
            marker: id.map(|id| DiskMarker {
                id: id.into(),
                name: "d".into(),
            }),
        }
    }

    #[test]
    fn auto_backup_only_for_newly_plugged_registered_disks() {
        let mut cfg = Config::default();
        cfg.disks.push(RegisteredDisk {
            id: "abc".into(),
            name: "d".into(),
        });
        let none: HashSet<PathBuf> = HashSet::new();
        let has_e: HashSet<PathBuf> = [PathBuf::from("E:\\")].into();

        // Disco registrado recién conectado: sí.
        assert_eq!(
            newly_connected(&none, &[drive("E:\\", Some("abc"))], &cfg),
            Some(PathBuf::from("E:\\"))
        );
        // Ya estaba conectado y se acaba de registrar: no.
        assert_eq!(newly_connected(&has_e, &[drive("E:\\", Some("abc"))], &cfg), None);
        // Recién conectado pero no registrado en este servidor, o sin marcador: no.
        assert_eq!(
            newly_connected(&none, &[drive("E:\\", Some("otro")), drive("F:\\", None)], &cfg),
            None
        );
    }

    #[test]
    fn old_reports_are_pruned_and_other_files_kept() {
        let dir = tempfile::tempdir().unwrap();
        for day in 1..=5 {
            std::fs::write(dir.path().join(format!("2026-01-0{day}_100000_respaldo.txt")), "r").unwrap();
        }
        std::fs::write(dir.path().join("2026-01-03_110000_verificacion.txt"), "v").unwrap();
        std::fs::write(dir.path().join("errores.log"), "e").unwrap();

        prune_reports(dir.path(), 2);
        let mut left: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(
            left,
            [
                "2026-01-04_100000_respaldo.txt",
                "2026-01-05_100000_respaldo.txt",
                "errores.log"
            ]
        );
    }

    #[test]
    fn panic_payload_is_readable() {
        let err = catch_unwind(|| panic!("se rompió {}", 7)).unwrap_err();
        assert_eq!(panic_text(&*err), "se rompió 7");
    }
}
