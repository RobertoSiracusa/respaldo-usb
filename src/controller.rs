//! Estado compartido entre la interfaz, el vigilante de discos y el hilo de respaldo.
//! Vigilante y respaldo no dependen de la interfaz: funcionan con la ventana oculta.

use crate::config::{self, Config};
use crate::drives::{self, Drive};
use crate::engine::{self, Job, Reporter, Summary};
use crate::platform;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

const POLL: Duration = Duration::from_secs(3);

pub struct LastRun {
    pub when: SystemTime,
    pub disk: String,
    pub summary: Summary,
    pub report: Option<PathBuf>,
}

#[derive(Default)]
pub struct Controller {
    pub config: Mutex<Config>,
    pub reporter: Reporter,
    pub running: AtomicBool,
    pub last: Mutex<Option<LastRun>>,
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

impl Controller {
    pub fn new(config: Config) -> Arc<Self> {
        Arc::new(Self { config: Mutex::new(config), ..Default::default() })
    }

    fn wake_ui(&self) {
        if let Some(f) = self.wake.lock().unwrap().as_ref() {
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
        *self.drives.lock().unwrap() = drives::list_drives();
    }

    /// Discos conectados que están registrados en este servidor.
    pub fn ready_disks(&self) -> Vec<Drive> {
        let cfg = self.config.lock().unwrap();
        self.drives
            .lock()
            .unwrap()
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
            let host_dir = drives::backup_root(&disk_root).join(config::hostname());
            let summary = match op {
                Op::Backup { .. } => {
                    let cfg = this.config.lock().unwrap().clone();
                    let job = Job {
                        sources: cfg.sources,
                        excludes: cfg.excludes,
                        host_dir,
                        verify_after_copy: cfg.verify_after_copy,
                    };
                    engine::run_backup(&job, &this.reporter)
                }
                Op::Verify => engine::run_verify(&host_dir, &this.reporter),
            };
            let when = SystemTime::now();
            let report = write_report(&summary, when);
            *this.last.lock().unwrap() = Some(LastRun {
                when,
                disk: disk_root.display().to_string(),
                summary,
                report,
            });
            this.running.store(false, Ordering::SeqCst);
            if op == (Op::Backup { auto: true }) {
                // Respaldo automático terminado: mostrar el resultado a quien conectó el disco.
                platform::show_window(this.hwnd.load(Ordering::Relaxed));
            }
            this.wake_ui();
        });
        true
    }

    /// Hilo que detecta discos conectados y lanza el respaldo automático.
    pub fn spawn_watcher(self: &Arc<Self>) {
        let this = self.clone();
        std::thread::spawn(move || {
            let mut known: Option<HashSet<String>> = None;
            loop {
                let list = drives::list_drives();
                let (auto, has_sources, present) = {
                    let cfg = this.config.lock().unwrap();
                    let present: Vec<(String, PathBuf)> = list
                        .iter()
                        .filter_map(|d| {
                            let m = d.marker.as_ref()?;
                            cfg.is_registered(&m.id).then(|| (m.id.clone(), d.root.clone()))
                        })
                        .collect();
                    (cfg.auto_on_connect, !cfg.sources.is_empty(), present)
                };
                let changed = {
                    let mut current = this.drives.lock().unwrap();
                    let changed = current.len() != list.len()
                        || current.iter().zip(&list).any(|(a, b)| a.root != b.root || a.marker != b.marker || a.free != b.free);
                    *current = list;
                    changed
                };
                // Solo cuenta como "recién conectado" lo que aparece después del primer sondeo.
                if let Some(prev) = &known {
                    if auto && has_sources {
                        if let Some((_, root)) = present.iter().find(|(id, _)| !prev.contains(id)) {
                            this.start(root, Op::Backup { auto: true });
                        }
                    }
                }
                known = Some(present.into_iter().map(|(id, _)| id).collect());
                if changed || this.is_running() {
                    this.wake_ui();
                }
                std::thread::sleep(POLL);
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
    Some(path)
}
