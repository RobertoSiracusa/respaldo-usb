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
        }
    }
}

/// Carpeta de datos de la app (config y reportes).
pub fn data_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(APP_DIR)
}

pub fn logs_dir() -> PathBuf {
    data_dir().join("reportes")
}

fn config_path() -> PathBuf {
    data_dir().join("config.json")
}

impl Config {
    pub fn load() -> Self {
        std::fs::read(config_path())
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
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
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' { c } else { '_' })
        .collect();
    if clean.is_empty() { "equipo".into() } else { clean }
}
