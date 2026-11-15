//! Detección de discos y marcador que identifica un disco de respaldo.
//!
//! El disco se reconoce por el archivo `RespaldoUSB/disco.json` que lleva dentro,
//! no por su letra: Windows puede asignarle una letra distinta en cada servidor.

use crate::config::APP_DIR;
use serde::{Deserialize, Serialize};
use std::io;
use std::path::{Path, PathBuf};

const MARKER_NAME: &str = "disco.json";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DiskMarker {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct Drive {
    pub root: PathBuf,
    pub label: String,
    pub filesystem: String,
    pub total: Option<u64>,
    pub free: Option<u64>,
    /// Windows lo reporta como extraíble (memorias USB). Los discos duros USB salen como fijos.
    pub removable: bool,
    pub marker: Option<DiskMarker>,
}

pub fn backup_root(disk_root: &Path) -> PathBuf {
    disk_root.join(APP_DIR)
}

fn read_marker(root: &Path) -> Option<DiskMarker> {
    let bytes = std::fs::read(backup_root(root).join(MARKER_NAME)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Marca el disco como destino de respaldo. Si ya tenía marcador, lo reutiliza.
pub fn register(root: &Path, name: &str) -> io::Result<DiskMarker> {
    if let Some(m) = read_marker(root) {
        return Ok(m);
    }
    let seed = format!(
        "{:?}|{}|{}",
        std::time::SystemTime::now(),
        std::process::id(),
        root.display()
    );
    let marker = DiskMarker {
        id: blake3::hash(seed.as_bytes()).to_hex()[..16].to_string(),
        name: name.to_string(),
    };
    let dir = backup_root(root);
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join(MARKER_NAME), serde_json::to_vec_pretty(&marker)?)?;
    Ok(marker)
}

/// Foto de los discos conectados, con lo necesario para saber sin tocar los
/// discos si algo cambió desde entonces.
pub struct Snapshot {
    pub drives: Vec<Drive>,
    /// Huella barata de las unidades presentes (en Windows, máscara de letras).
    token: u64,
    /// Unidades con letra pero aún sin medio legible (lector vacío, disco bloqueado o arrancando).
    pending: Vec<PathBuf>,
}

impl Snapshot {
    /// Lee todos los discos: los despierta si estaban en reposo. Usar solo cuando haga falta.
    pub fn take() -> Self {
        let token = imp::token();
        let (mut drives, pending) = imp::list();
        // Para pruebas: carpetas que se tratan como discos (separadas por ';').
        if let Ok(extra) = std::env::var("RESPALDO_DISCOS_PRUEBA") {
            for p in extra.split(';').filter(|p| !p.is_empty()) {
                let root = PathBuf::from(p);
                if root.is_dir() {
                    drives.push(Drive {
                        label: "prueba".into(),
                        filesystem: String::new(),
                        total: None,
                        free: None,
                        removable: true,
                        marker: None,
                        root,
                    });
                }
            }
        }
        for d in &mut drives {
            d.marker = read_marker(&d.root);
        }
        Self { drives, token, pending }
    }

    /// `true` si se conectó o quitó una unidad, o si una pendiente ya está lista.
    /// No lee los discos ya conocidos, así que no les impide entrar en reposo.
    pub fn is_stale(&self) -> bool {
        imp::token() != self.token || self.pending.iter().any(|root| imp::is_ready(root))
    }
}

#[cfg(windows)]
mod imp {
    use super::Drive;
    use std::path::{Path, PathBuf};
    use windows_sys::Win32::Storage::FileSystem::{
        GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW,
    };
    use windows_sys::Win32::System::Diagnostics::Debug::SetErrorMode;

    const DRIVE_REMOVABLE: u32 = 2;
    const DRIVE_FIXED: u32 = 3;
    const SEM_FAILCRITICALERRORS: u32 = 1;

    fn from_wide(buf: &[u16]) -> String {
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..len])
    }

    fn wide(root: &Path) -> Vec<u16> {
        root.to_string_lossy()
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect()
    }

    /// Máscara de letras de unidad: se consulta en memoria, sin acceso a disco.
    pub fn token() -> u64 {
        unsafe { GetLogicalDrives() as u64 }
    }

    /// Etiqueta y sistema de archivos si el volumen ya es legible.
    fn volume_info(root: &Path) -> Option<(String, String)> {
        let mut label = [0u16; 261];
        let mut fs = [0u16; 261];
        let ok = unsafe {
            GetVolumeInformationW(
                wide(root).as_ptr(),
                label.as_mut_ptr(),
                label.len() as u32,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                fs.as_mut_ptr(),
                fs.len() as u32,
            )
        };
        (ok != 0).then(|| (from_wide(&label), from_wide(&fs)))
    }

    pub fn is_ready(root: &Path) -> bool {
        volume_info(root).is_some()
    }

    pub fn list() -> (Vec<Drive>, Vec<PathBuf>) {
        let system = std::env::var("SystemDrive")
            .unwrap_or_else(|_| "C:".into())
            .to_ascii_uppercase();
        let (mut ready, mut pending) = (Vec::new(), Vec::new());
        // Evita el diálogo "no hay disco en la unidad" de lectores de tarjetas vacíos.
        unsafe { SetErrorMode(SEM_FAILCRITICALERRORS) };
        let mask = unsafe { GetLogicalDrives() };
        for i in 0..26u32 {
            if mask & (1 << i) == 0 {
                continue;
            }
            let letter = (b'A' + i as u8) as char;
            if system.starts_with(letter) {
                continue;
            }
            let root = PathBuf::from(format!("{letter}:\\"));
            let w = wide(&root);
            let kind = unsafe { GetDriveTypeW(w.as_ptr()) };
            if kind != DRIVE_REMOVABLE && kind != DRIVE_FIXED {
                continue;
            }
            let Some((label, filesystem)) = volume_info(&root) else {
                pending.push(root);
                continue;
            };
            let (mut avail, mut total, mut free) = (0u64, 0u64, 0u64);
            let space = unsafe { GetDiskFreeSpaceExW(w.as_ptr(), &mut avail, &mut total, &mut free) } != 0;
            ready.push(Drive {
                root,
                label,
                filesystem,
                total: space.then_some(total),
                free: space.then_some(avail),
                removable: kind == DRIVE_REMOVABLE,
                marker: None,
            });
        }
        (ready, pending)
    }
}

#[cfg(not(windows))]
mod imp {
    use super::Drive;
    use std::path::{Path, PathBuf};

    fn mounts() -> String {
        std::fs::read_to_string("/proc/mounts").unwrap_or_default()
    }

    pub fn token() -> u64 {
        let digest = blake3::hash(mounts().as_bytes());
        u64::from_le_bytes(digest.as_bytes()[..8].try_into().unwrap_or_default())
    }

    pub fn is_ready(_root: &Path) -> bool {
        false
    }

    /// Solo para desarrollo: puntos de montaje de medios extraíbles.
    pub fn list() -> (Vec<Drive>, Vec<PathBuf>) {
        let drives = mounts()
            .lines()
            .filter_map(|line| {
                let mut parts = line.split_whitespace();
                let _dev = parts.next()?;
                let mount = parts.next()?.replace("\\040", " ");
                let fs = parts.next()?;
                let removable = mount.starts_with("/run/media/") || mount.starts_with("/media/");
                (removable || mount.starts_with("/mnt/")).then(|| Drive {
                    label: mount.rsplit('/').next().unwrap_or("").to_string(),
                    root: PathBuf::from(&mount),
                    filesystem: fs.to_string(),
                    total: None,
                    free: None,
                    removable,
                    marker: None,
                })
            })
            .collect();
        (drives, Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registering_marks_the_disk_once() {
        let disk = tempfile::tempdir().unwrap();
        assert_eq!(read_marker(disk.path()), None);

        let first = register(disk.path(), "Disco A").unwrap();
        assert_eq!(first.name, "Disco A");
        assert_eq!(first.id.len(), 16);
        assert_eq!(read_marker(disk.path()), Some(first.clone()));
        assert!(backup_root(disk.path()).join(MARKER_NAME).is_file());

        // Otro servidor registra el mismo disco: conserva identidad y nombre.
        assert_eq!(register(disk.path(), "Otro nombre").unwrap(), first);
    }

    #[test]
    fn unreadable_marker_means_not_a_backup_disk() {
        let disk = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(backup_root(disk.path())).unwrap();
        std::fs::write(backup_root(disk.path()).join(MARKER_NAME), "no es json").unwrap();
        assert_eq!(read_marker(disk.path()), None);
    }

    #[test]
    fn snapshot_is_fresh_right_after_taking_it() {
        assert!(!Snapshot::take().is_stale());
    }
}
