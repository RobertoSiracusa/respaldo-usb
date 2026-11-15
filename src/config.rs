//! Configuración local del servidor (carpetas, exclusiones, discos registrados).

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const APP_DIR: &str = "RespaldoUSB";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RegisteredDisk {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Carpetas a respaldar.
    pub sources: Vec<PathBuf>,
    /// Patrones glob a excluir (por nombre o por ruta).
    pub excludes: Vec<String>,
    /// Discos autorizados como destino.
    pub disks: Vec<RegisteredDisk>,
    /// Arrancar respaldo al conectar un disco registrado.
    pub auto_on_connect: bool,
    /// Releer cada archivo copiado para confirmar que quedó bien escrito.
    pub verify_after_copy: bool,
    /// Copiar permisos NTFS a la copia (requiere disco NTFS).
    pub preserve_acl: bool,
    /// Identificador de este equipo; distingue servidores con el mismo nombre.
    pub machine_id: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            sources: Vec::new(),
            excludes: vec![
                "Thumbs.db".into(),
                "desktop.ini".into(),
                "$RECYCLE.BIN".into(),
                "System Volume Information".into(),
            ],
            disks: Vec::new(),
            auto_on_connect: true,
            verify_after_copy: true,
            preserve_acl: false,
            machine_id: String::new(),
        }
    }
}

/// Carpeta de datos de la app (config, estado y reportes). Es de la máquina, no del
/// usuario: todos los administradores del servidor ven la misma configuración.
pub fn data_dir() -> PathBuf {
    if cfg!(windows)
        && let Some(base) = std::env::var_os("ProgramData")
    {
        return PathBuf::from(base).join(APP_DIR);
    }
    legacy_data_dir()
}

/// Ubicación por usuario que usaba la versión 0.1.0.
fn legacy_data_dir() -> PathBuf {
    dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join(APP_DIR)
}

pub fn logs_dir() -> PathBuf {
    data_dir().join("reportes")
}

fn config_path() -> PathBuf {
    data_dir().join("config.json")
}

impl Config {
    pub fn load() -> Self {
        let read = |path: PathBuf| -> Option<Config> { serde_json::from_slice(&std::fs::read(path).ok()?).ok() };
        let current = read(config_path());
        let migrated = current.is_none();
        let mut cfg = current
            .or_else(|| read(legacy_data_dir().join("config.json")))
            .unwrap_or_default();
        if cfg.machine_id.is_empty() {
            // Preferir el identificador de Windows: es el mismo aunque no se pueda guardar la configuración.
            let seed = crate::platform::machine_guid().unwrap_or_else(|| {
                format!(
                    "{:?}|{}|{}",
                    std::time::SystemTime::now(),
                    std::process::id(),
                    hostname()
                )
            });
            cfg.machine_id = blake3::hash(seed.as_bytes()).to_hex()[..16].to_string();
            let _ = cfg.save();
        } else if migrated {
            let _ = cfg.save();
        }
        cfg
    }

    pub fn save(&self) -> std::io::Result<()> {
        let path = config_path();
        std::fs::create_dir_all(data_dir())?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(tmp, path)
    }

    pub fn is_registered(&self, id: &str) -> bool {
        self.disks.iter().any(|d| d.id == id)
    }
}

/// Nombre de este servidor; separa los respaldos de cada equipo en el disco.
pub fn hostname() -> String {
    let raw = std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
        .unwrap_or_default();
    let clean: String = raw
        .trim()
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if clean.is_empty() { "equipo".into() } else { clean }
}
