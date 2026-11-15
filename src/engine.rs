//! Motor de respaldo: espejo incremental por hash BLAKE3.
//!
//! Disposición en el disco:
//!   <disco>/RespaldoUSB/<servidor>/manifiesto.json
//!   <disco>/RespaldoUSB/<servidor>/servidor.json      (a qué equipo pertenece la carpeta)
//!   <disco>/RespaldoUSB/<servidor>/C/Datos/...        (archivos normales)
//!
//! Reglas:
//! - Se copia un archivo si su hash difiere del manifiesto o si falta en el disco.
//! - Nunca se borra nada del disco, aunque desaparezca del origen.
//! - Copia a temporal + renombrado: un corte no deja archivos a medias.
//! - Un solo proceso a la vez por carpeta de servidor (archivo de bloqueo).

use crate::fsx;
use crate::lock;
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File, TryLockError};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf, Prefix};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use walkdir::WalkDir;

const MANIFEST_NAME: &str = "manifiesto.json";
const SERVER_MARKER: &str = "servidor.json";
const LOCK_NAME: &str = "en-uso.lock";
const BUF_SIZE: usize = 1 << 20;
/// Alineación del búfer de lectura; cubre sectores de 512 y 4096 bytes.
const ALIGN: usize = 4096;
const MANIFEST_SAVE_EVERY: Duration = Duration::from_secs(300);
/// Tope de incidencias guardadas en memoria; el resto solo se cuenta.
const MAX_ISSUES: usize = 5000;
const MAX_LINKS: usize = 200;
/// Espacio que se deja siempre libre en el disco de respaldo.
const RESERVE: u64 = 32 << 20;
const HIDDEN_OR_SYSTEM: u32 = 0x2 | 0x4;

const MSG_DISCONNECTED: &str = "el disco de respaldo se desconectó";
const MSG_FULL: &str = "el disco de respaldo está lleno";

fn tmp_name() -> String {
    format!("~respaldo-{}.tmp", std::process::id())
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Entry {
    pub hash: String,
    pub size: u64,
    /// Fecha de modificación del origen (segundos Unix). Solo informativo.
    pub mtime: i64,
    /// Atributos de Windows ya aplicados a la copia.
    #[serde(default)]
    pub attrs: u32,
    /// Huella de los permisos NTFS ya aplicados a la copia.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sec: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    /// Clave: ruta relativa a la carpeta del servidor, separada con '/'.
    pub files: BTreeMap<String, Entry>,
}

impl Manifest {
    pub fn load(host_dir: &Path) -> io::Result<Self> {
        let path = host_dir.join(MANIFEST_NAME);
        match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("manifiesto dañado ({}): {e}", path.display()),
                )
            }),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self {
                version: 1,
                ..Default::default()
            }),
            Err(e) => Err(e),
        }
    }

    pub fn save(&self, host_dir: &Path) -> io::Result<()> {
        let path = host_dir.join(MANIFEST_NAME);
        let tmp = host_dir.join("manifiesto.json.tmp");
        {
            let mut f = io::BufWriter::new(File::create(&tmp)?);
            serde_json::to_writer(&mut f, self)?;
            f.flush()?;
            f.get_ref().sync_all()?;
        }
        replace_file(&tmp, &path)
    }

    /// Quita la entrada `key` y todo lo que cuelgue de ella.
    fn purge(&mut self, key: &str) {
        let prefix = format!("{key}/");
        self.files.retain(|k, _| k != key && !k.starts_with(&prefix));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Phase {
    #[default]
    Idle,
    Scanning,
    Copying,
    Verifying,
}

#[derive(Debug, Clone, Default)]
pub struct Progress {
    pub phase: Phase,
    pub files_total: u64,
    pub files_done: u64,
    pub bytes_total: u64,
    pub bytes_done: u64,
    pub current: String,
}

/// Canal entre el motor y la interfaz.
#[derive(Clone, Default)]
pub struct Reporter {
    pub progress: Arc<Mutex<Progress>>,
    pub cancel: Arc<AtomicBool>,
}

impl Reporter {
    fn update(&self, f: impl FnOnce(&mut Progress)) {
        f(&mut lock(&self.progress));
    }
    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
    fn bytes_done(&self) -> u64 {
        lock(&self.progress).bytes_done
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Summary {
    pub verify: bool,
    pub copied: u64,
    pub copied_bytes: u64,
    pub unchanged: u64,
    /// Ya estaban en el disco con contenido idéntico; se anotaron en el manifiesto sin copiar.
    pub adopted: u64,
    /// Sin cambios de contenido, pero se actualizaron atributos o permisos.
    pub meta_updated: u64,
    /// Archivos presentes en el disco que ya no existen en el origen (se conservan).
    pub only_in_backup: u64,
    /// Enlaces simbólicos y uniones: se omiten a propósito, no son errores.
    pub links_skipped: u64,
    pub links: Vec<String>,
    /// Avisos que no son errores.
    pub notes: Vec<String>,
    /// (ruta, motivo) de archivos omitidos o con error.
    pub issues: Vec<(String, String)>,
    pub issues_total: u64,
    pub cancelled: bool,
    /// Error que detuvo la operación (disco desconectado, lleno, etc.).
    pub fatal: Option<String>,
    pub seconds: f64,
}

impl Summary {
    pub fn failed(msg: impl Into<String>, verify: bool) -> Self {
        Self {
            verify,
            fatal: Some(msg.into()),
            ..Default::default()
        }
    }

    fn issue(&mut self, path: impl Into<String>, why: impl std::fmt::Display) {
        self.issues_total += 1;
        if self.issues.len() < MAX_ISSUES {
            self.issues.push((path.into(), why.to_string()));
        }
    }

    /// Llegó al final: ni error fatal ni cancelación.
    pub fn finished(&self) -> bool {
        self.fatal.is_none() && !self.cancelled
    }

    pub fn ok(&self) -> bool {
        self.finished() && self.issues_total == 0
    }
}

pub struct Job {
    pub sources: Vec<PathBuf>,
    pub excludes: Vec<String>,
    /// <disco>/RespaldoUSB/<servidor>
    pub host_dir: PathBuf,
    pub verify_after_copy: bool,
    /// Copiar permisos NTFS (solo si el disco los admite).
    pub preserve_acl: bool,
}

#[derive(Serialize, Deserialize)]
struct ServerMarker {
    id: String,
    name: String,
}

/// Carpeta de este servidor dentro del disco. Si `<nombre>` ya pertenece a otro
/// equipo con el mismo nombre, usa `<nombre>-<id>` para no mezclar respaldos.
pub fn resolve_host_dir(backup_root: &Path, hostname: &str, machine_id: &str) -> io::Result<PathBuf> {
    let short: String = machine_id.chars().take(6).collect();
    for name in [hostname.to_string(), format!("{hostname}-{short}")] {
        let dir = backup_root.join(&name);
        let marker = dir.join(SERVER_MARKER);
        match fs::read(&marker) {
            Ok(bytes) => match serde_json::from_slice::<ServerMarker>(&bytes) {
                Ok(m) if m.id != machine_id => continue,
                _ => return Ok(dir),
            },
            // Sin marcador: carpeta nueva o creada por una versión anterior. Se reclama.
            Err(_) => {
                fs::create_dir_all(&dir)?;
                let m = ServerMarker {
                    id: machine_id.to_string(),
                    name: hostname.to_string(),
                };
                fs::write(&marker, serde_json::to_vec_pretty(&m)?)?;
                return Ok(dir);
            }
        }
    }
    Err(io::Error::other(format!(
        "el disco ya tiene respaldos de otros equipos llamados {hostname}; cambia el nombre de este servidor"
    )))
}

/// Convierte una ruta de origen absoluta en subcarpeta del respaldo.
/// `C:\Datos\X` -> `C/Datos/X`; `\\srv\share\X` -> `UNC/srv/share/X`; `/home/x` -> `raiz/home/x`.
pub fn map_source(src: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in src.components() {
        match comp {
            Component::Prefix(p) => match p.kind() {
                Prefix::Disk(l) | Prefix::VerbatimDisk(l) => out.push((l as char).to_ascii_uppercase().to_string()),
                Prefix::UNC(s, sh) | Prefix::VerbatimUNC(s, sh) => {
                    out.push("UNC");
                    out.push(s);
                    out.push(sh);
                }
                _ => out.push("otro"),
            },
            Component::RootDir => {
                if out.as_os_str().is_empty() {
                    out.push("raiz");
                }
            }
            Component::Normal(n) => out.push(n),
            Component::CurDir | Component::ParentDir => {}
        }
    }
    out
}

fn key_of(rel: &Path) -> String {
    rel.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// `true` si `inner` es `outer` o está dentro de `outer`. Sin distinguir mayúsculas en Windows.
pub fn path_contains(outer: &Path, inner: &Path) -> bool {
    let norm = |p: &Path| -> Vec<String> {
        p.components()
            .filter(|c| !matches!(c, Component::CurDir))
            .map(|c| {
                let s = c.as_os_str().to_string_lossy().into_owned();
                if cfg!(windows) { s.to_lowercase() } else { s }
            })
            .collect()
    };
    let (o, i) = (norm(outer), norm(inner));
    i.len() >= o.len() && i[..o.len()] == o[..]
}

/// Descarta orígenes inaccesibles, los que contienen el propio disco de respaldo
/// y los que ya están dentro de otro origen.
fn plan_sources(job: &Job, sum: &mut Summary) -> Vec<PathBuf> {
    let mut candidates: Vec<&PathBuf> = job.sources.iter().collect();
    candidates.sort_by_key(|p| p.components().count());
    let mut plan: Vec<PathBuf> = Vec::new();
    for src in candidates {
        if !src.is_dir() {
            sum.issue(
                src.display().to_string(),
                "carpeta de origen no existe o no es accesible",
            );
        } else if path_contains(src, &job.host_dir) || path_contains(&job.host_dir, src) {
            sum.issue(
                src.display().to_string(),
                "la carpeta está en el disco de respaldo o lo contiene; omitida",
            );
        } else if let Some(parent) = plan.iter().find(|kept| path_contains(kept, src)) {
            if parent != src {
                sum.notes.push(format!(
                    "{} ya está incluida dentro de {}",
                    src.display(),
                    parent.display()
                ));
            }
        } else {
            plan.push(src.clone());
        }
    }
    plan
}

fn compile_pattern(pattern: &str) -> Result<globset::Glob, globset::Error> {
    GlobBuilder::new(pattern)
        .case_insensitive(true)
        .literal_separator(false)
        .build()
}

/// Patrones de exclusión mal escritos (por ejemplo, un corchete sin cerrar).
pub fn invalid_patterns(patterns: &[String]) -> Vec<String> {
    patterns
        .iter()
        .map(|p| p.trim())
        .filter(|p| !p.is_empty() && compile_pattern(p).is_err())
        .map(String::from)
        .collect()
}

fn build_excludes(patterns: &[String]) -> GlobSet {
    let mut b = GlobSetBuilder::new();
    for p in patterns.iter().map(|p| p.trim()).filter(|p| !p.is_empty()) {
        if let Ok(g) = compile_pattern(p) {
            b.add(g);
        }
    }
    b.build().unwrap_or_else(|_| GlobSet::empty())
}

fn is_excluded(set: &GlobSet, entry: &walkdir::DirEntry) -> bool {
    set.is_match(entry.file_name()) || set.is_match(entry.path())
}

fn mtime_secs(meta: &fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn short_hash(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex()[..16].to_string()
}

/// Renombra `tmp` sobre `dest`. En Windows falla si `dest` es de solo lectura: se quita y reintenta.
fn replace_file(tmp: &Path, dest: &Path) -> io::Result<()> {
    match fs::rename(tmp, dest) {
        Ok(()) => Ok(()),
        Err(first) => {
            if let Ok(meta) = fs::metadata(dest) {
                let mut perms = meta.permissions();
                if perms.readonly() {
                    #[allow(clippy::permissions_set_readonly_false)]
                    perms.set_readonly(false);
                    fs::set_permissions(dest, perms)?;
                    return fs::rename(tmp, dest);
                }
            }
            Err(first)
        }
    }
}

/// Aparta algo que estorba en el destino (archivo donde ahora va carpeta, o al revés) sin borrarlo.
fn set_aside(path: &Path, stamp: &str) -> io::Result<PathBuf> {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".~anterior-{stamp}"));
    let to = path.with_file_name(name);
    fs::rename(path, &to)?;
    Ok(to)
}

fn is_tmp_name(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with("~respaldo") && n.ends_with(".tmp"))
}

/// Impide que dos procesos respalden o verifiquen a la vez la misma carpeta.
/// El sistema libera el bloqueo al cerrarse el archivo, incluso si el proceso muere.
///
/// El mismo archivo sirve de diario: guarda la ruta del temporal que se está
/// escribiendo, para borrarlo en el siguiente arranque si hubo un corte.
struct Lock {
    file: File,
}

impl Lock {
    fn acquire(host_dir: &Path) -> Result<Self, String> {
        let path = host_dir.join(LOCK_NAME);
        let file = fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| format!("no se pudo escribir en el disco de respaldo ({}): {e}", path.display()))?;
        // Si el sistema de archivos no admite bloqueos, se sigue sin él.
        if let Err(TryLockError::WouldBlock) = file.try_lock() {
            return Err("otro respaldo o verificación está usando este disco".into());
        }
        let mut lock = Self { file };
        lock.remove_orphan(host_dir);
        Ok(lock)
    }

    /// Borra el temporal que dejó a medias una corrida interrumpida.
    fn remove_orphan(&mut self, host_dir: &Path) {
        let mut noted = String::new();
        let _ = self.file.seek(SeekFrom::Start(0));
        let _ = self.file.read_to_string(&mut noted);
        let orphan = PathBuf::from(noted.trim());
        if is_tmp_name(&orphan) && path_contains(host_dir, &orphan) {
            let _ = fs::remove_file(&orphan);
        }
        self.clear();
    }

    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(bytes)?;
        self.file.set_len(bytes.len() as u64)
    }

    /// Anota el temporal que se va a escribir.
    fn note(&mut self, tmp: &Path) {
        let _ = self.write(tmp.to_string_lossy().as_bytes());
    }

    fn clear(&mut self) {
        let _ = self.write(b"");
    }
}

/// Búfer de 1 MiB alineado a sector, requisito de la lectura sin caché.
struct Buf {
    raw: Vec<u8>,
    off: usize,
}

impl Buf {
    fn new() -> Self {
        let raw = vec![0u8; BUF_SIZE + ALIGN];
        let off = raw.as_ptr().align_offset(ALIGN);
        Self {
            raw,
            off: if off < ALIGN { off } else { 0 },
        }
    }
    fn get(&mut self) -> &mut [u8] {
        &mut self.raw[self.off..self.off + BUF_SIZE]
    }
}

struct Cancelled;

enum ReadErr {
    Cancelled,
    Io(io::Error),
}

#[derive(Clone, Copy, PartialEq)]
enum Open {
    /// Lectura normal del origen.
    Source,
    /// Lectura sin caché: comprueba lo que de verdad hay en el disco de respaldo.
    Direct,
}

fn hash_stream(mut f: File, rep: &Reporter, buf: &mut [u8], count_bytes: bool) -> Result<String, ReadErr> {
    let mut hasher = blake3::Hasher::new();
    loop {
        if rep.cancelled() {
            return Err(ReadErr::Cancelled);
        }
        let n = f.read(buf).map_err(ReadErr::Io)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        if count_bytes {
            rep.update(|p| p.bytes_done += n as u64);
        }
    }
    Ok(hasher.finalize().to_hex().to_string())
}

fn hash_file(path: &Path, rep: &Reporter, buf: &mut [u8], count_bytes: bool, mode: Open) -> Result<String, ReadErr> {
    if mode == Open::Direct
        && let Ok(f) = fsx::open_direct(path)
    {
        match hash_stream(f, rep, buf, count_bytes) {
            // El dispositivo no acepta lectura sin caché con este búfer: se repite con caché.
            Err(ReadErr::Io(e)) if e.kind() == io::ErrorKind::InvalidInput => {}
            other => return other,
        }
    }
    let f = fsx::open_source(path).map_err(ReadErr::Io)?;
    hash_stream(f, rep, buf, count_bytes)
}

enum CopyErr {
    Cancelled,
    /// Fallo leyendo el origen (bloqueado, sin permiso): se omite el archivo.
    Source(io::Error),
    /// Fallo escribiendo en el disco de respaldo.
    Dest(io::Error),
    /// La copia releída no coincide con lo leído del origen.
    Mismatch,
}

/// Copia `src` a `dest` vía temporal, calculando el hash en la misma pasada.
fn copy_file(
    src: &Path,
    dest: &Path,
    verify: bool,
    rep: &Reporter,
    buf: &mut [u8],
    count_bytes: bool,
) -> Result<(String, u64), CopyErr> {
    let mut input = fsx::open_source(src).map_err(CopyErr::Source)?;
    let src_meta = input.metadata().ok();
    let tmp = dest.with_file_name(tmp_name());
    let result = (|| {
        let mut out = fsx::create_dest(&tmp).map_err(CopyErr::Dest)?;
        let mut hasher = blake3::Hasher::new();
        let mut total = 0u64;
        loop {
            if rep.cancelled() {
                return Err(CopyErr::Cancelled);
            }
            let n = input.read(buf).map_err(CopyErr::Source)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            out.write_all(&buf[..n]).map_err(CopyErr::Dest)?;
            total += n as u64;
            if count_bytes {
                rep.update(|p| p.bytes_done += n as u64);
            }
        }
        if let Some(meta) = &src_meta {
            fsx::copy_times(meta, &out);
        }
        let hash = hasher.finalize().to_hex().to_string();
        if verify {
            // Volcar al dispositivo y releer sin caché: prueba lo escrito, no la memoria.
            out.sync_all().map_err(CopyErr::Dest)?;
            drop(out);
            match hash_file(&tmp, rep, buf, false, Open::Direct) {
                Ok(h) if h == hash => {}
                Ok(_) => return Err(CopyErr::Mismatch),
                Err(ReadErr::Cancelled) => return Err(CopyErr::Cancelled),
                Err(ReadErr::Io(e)) => return Err(CopyErr::Dest(e)),
            }
        } else {
            drop(out);
        }
        replace_file(&tmp, dest).map_err(CopyErr::Dest)?;
        Ok((hash, total))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Iguala atributos y permisos de la copia con los del origen. Devuelve si cambió algo.
fn sync_meta(dest: &Path, attrs: u32, sec: Option<&Vec<u8>>, entry: &mut Entry, sum: &mut Summary) -> bool {
    let mut changed = false;
    if let Some(sd) = sec {
        let h = short_hash(sd);
        if entry.sec.as_deref() != Some(h.as_str()) {
            match fsx::write_security(dest, sd) {
                Ok(()) => {
                    entry.sec = Some(h);
                    changed = true;
                }
                Err(e) => sum.issue(
                    dest.display().to_string(),
                    format!("no se pudieron copiar los permisos: {}", describe(&e)),
                ),
            }
        }
    }
    if cfg!(windows) && entry.attrs != attrs && fsx::set_attributes(dest, attrs).is_ok() {
        entry.attrs = attrs;
        changed = true;
    }
    changed
}

fn describe(e: &io::Error) -> String {
    match e.raw_os_error() {
        #[cfg(windows)]
        Some(32) | Some(33) => "archivo en uso por otro programa".into(),
        #[cfg(windows)]
        Some(5) => "acceso denegado".into(),
        _ => e.to_string(),
    }
}

fn walker(src: &Path, excludes: &GlobSet) -> impl Iterator<Item = walkdir::Result<walkdir::DirEntry>> {
    WalkDir::new(src)
        .follow_links(false)
        .into_iter()
        .filter_entry(move |e| e.depth() == 0 || !is_excluded(excludes, e))
}

pub fn run_backup(job: &Job, rep: &Reporter) -> Summary {
    let start = Instant::now();
    let mut sum = Summary::default();
    if let Err(Cancelled) = backup_inner(job, rep, &mut sum) {
        sum.cancelled = true;
    }
    sum.seconds = start.elapsed().as_secs_f64();
    rep.update(|p| *p = Progress::default());
    sum
}

fn backup_inner(job: &Job, rep: &Reporter, sum: &mut Summary) -> Result<(), Cancelled> {
    let excludes = build_excludes(&job.excludes);
    if let Err(e) = fs::create_dir_all(&job.host_dir) {
        sum.fatal = Some(format!("no se pudo crear {}: {e}", job.host_dir.display()));
        return Ok(());
    }
    let mut lock = match Lock::acquire(&job.host_dir) {
        Ok(f) => f,
        Err(msg) => {
            sum.fatal = Some(msg);
            return Ok(());
        }
    };
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    let mut manifest = match Manifest::load(&job.host_dir) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::InvalidData => {
            // Manifiesto ilegible: se aparta y se reconstruye comparando origen con lo que ya hay en el disco.
            let bad = job.host_dir.join(MANIFEST_NAME);
            let _ = fs::rename(&bad, job.host_dir.join(format!("manifiesto.dañado-{stamp}.json")));
            sum.notes
                .push("El manifiesto estaba dañado. Se reconstruyó comparando cada archivo con su copia.".into());
            Manifest {
                version: 1,
                ..Default::default()
            }
        }
        Err(e) => {
            sum.fatal = Some(format!("no se pudo leer el manifiesto: {e}"));
            return Ok(());
        }
    };

    let acl_on = job.preserve_acl && fsx::volume_supports_acl(&job.host_dir);
    if job.preserve_acl && !acl_on {
        sum.notes
            .push("El disco no es NTFS: los permisos no se pueden guardar en él.".into());
    }
    if acl_on && !fsx::has_backup_privileges() {
        sum.notes.push(
            "Permisos NTFS sin privilegios de administrador: puede haber archivos con acceso denegado. Ejecuta la app como administrador."
                .into(),
        );
    }
    for bad in invalid_patterns(&job.excludes) {
        sum.notes
            .push(format!("Patrón de exclusión mal escrito, ignorado: {bad}"));
    }
    let sources = plan_sources(job, sum);

    // Pasada 1: contar para la barra de progreso.
    rep.update(|p| {
        *p = Progress {
            phase: Phase::Scanning,
            ..Default::default()
        };
    });
    for src in &sources {
        for entry in walker(src, &excludes).flatten() {
            if rep.cancelled() {
                return Err(Cancelled);
            }
            if entry.file_type().is_file() {
                let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                rep.update(|p| {
                    p.files_total += 1;
                    p.bytes_total += size;
                });
            }
        }
    }

    // Pasada 2: comparar y copiar.
    rep.update(|p| p.phase = Phase::Copying);
    let mut buf = Buf::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut prefixes: Vec<String> = Vec::new();
    let mut last_save = Instant::now();
    let mut dirty = false;

    'sources: for src in &sources {
        let mapped = map_source(src);
        prefixes.push(format!("{}/", key_of(&mapped)));
        for entry in walker(src, &excludes) {
            if rep.cancelled() {
                break 'sources;
            }
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    let path = e.path().map(|p| p.display().to_string()).unwrap_or_default();
                    sum.issue(path, e.io_error().map(describe).unwrap_or_else(|| e.to_string()));
                    continue;
                }
            };
            let src_path = entry.path();
            let rel = mapped.join(src_path.strip_prefix(src).unwrap_or(Path::new("")));
            let dest = job.host_dir.join(&rel);
            let key = key_of(&rel);
            let ftype = entry.file_type();
            let meta = entry.metadata().ok();
            let attrs = meta.as_ref().map(fsx::attributes).unwrap_or(0);

            if ftype.is_dir() {
                // Donde ahora hay carpeta antes había un archivo: se aparta, no se borra.
                if let Ok(m) = fs::symlink_metadata(&dest)
                    && !m.is_dir()
                {
                    match set_aside(&dest, &stamp) {
                        Ok(to) => {
                            manifest.purge(&key);
                            dirty = true;
                            sum.notes.push(format!(
                                "{} pasó a ser carpeta; la copia anterior quedó en {}",
                                key,
                                to.display()
                            ));
                        }
                        Err(e) => sum.issue(
                            dest.display().to_string(),
                            format!("no se pudo apartar: {}", describe(&e)),
                        ),
                    }
                }
                let _ = fs::create_dir_all(&dest);
                if entry.depth() > 0
                    && cfg!(windows)
                    && let Ok(dm) = fs::metadata(&dest)
                    && (fsx::attributes(&dm) ^ attrs) & HIDDEN_OR_SYSTEM != 0
                {
                    let _ = fsx::set_attributes(&dest, attrs & HIDDEN_OR_SYSTEM);
                }
                if acl_on
                    && let Ok(sd) = fsx::read_security(src_path)
                    && fsx::read_security(&dest).ok().as_ref() != Some(&sd)
                    && let Err(e) = fsx::write_security(&dest, &sd)
                {
                    sum.issue(
                        dest.display().to_string(),
                        format!("no se pudieron copiar los permisos: {}", describe(&e)),
                    );
                }
                continue;
            }
            if !ftype.is_file() {
                sum.links_skipped += 1;
                if sum.links.len() < MAX_LINKS {
                    sum.links.push(src_path.display().to_string());
                }
                continue;
            }

            let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
            rep.update(|p| p.current = src_path.display().to_string());
            let bytes_before = rep.bytes_done();
            // Cierra la cuenta del archivo y pasa al siguiente, anotando el problema.
            macro_rules! skip {
                ($path:expr, $why:expr) => {{
                    sum.issue($path, $why);
                    seen.insert(key);
                    finish_file(rep, bytes_before, size);
                    continue;
                }};
            }

            let sec = if acl_on {
                match fsx::read_security(src_path) {
                    Ok(sd) => Some(sd),
                    Err(e) => {
                        sum.issue(
                            src_path.display().to_string(),
                            format!("no se pudieron leer los permisos: {}", describe(&e)),
                        );
                        None
                    }
                }
            } else {
                None
            };

            // ¿Qué hay ahora en el destino?
            let dest_len = match fs::symlink_metadata(&dest) {
                Ok(m) if m.is_dir() => match set_aside(&dest, &stamp) {
                    Ok(to) => {
                        manifest.purge(&key);
                        dirty = true;
                        sum.notes.push(format!(
                            "{} pasó a ser archivo; la carpeta anterior quedó en {}",
                            key,
                            to.display()
                        ));
                        None
                    }
                    Err(e) => skip!(
                        dest.display().to_string(),
                        format!("no se pudo apartar: {}", describe(&e))
                    ),
                },
                Ok(m) => Some(m.len()),
                Err(_) => None,
            };

            // Decidir: conservar (con su entrada de manifiesto) o copiar.
            let known = manifest.files.get(&key).cloned();
            let mut already_read = false;
            let mut adopted = false;
            let mut keep: Option<Entry> = None;
            let comparable = match &known {
                Some(k) => dest_len == Some(k.size),
                None => dest_len == Some(size),
            };
            if comparable {
                let h = match hash_file(src_path, rep, buf.get(), true, Open::Source) {
                    Ok(h) => h,
                    Err(ReadErr::Cancelled) => break 'sources,
                    Err(ReadErr::Io(e)) => skip!(src_path.display().to_string(), describe(&e)),
                };
                already_read = true;
                match known {
                    Some(k) if k.hash == h => keep = Some(k),
                    Some(_) => {}
                    // Copia sin entrada en el manifiesto: si es idéntica al origen, se anota sin reescribirla.
                    None => match hash_file(&dest, rep, buf.get(), false, Open::Direct) {
                        Ok(d) if d == h => {
                            adopted = true;
                            let mtime = meta.as_ref().map(mtime_secs).unwrap_or(0);
                            keep = Some(Entry {
                                hash: h,
                                size,
                                mtime,
                                ..Default::default()
                            });
                        }
                        Err(ReadErr::Cancelled) => break 'sources,
                        _ => {}
                    },
                }
            }

            if let Some(mut e) = keep {
                let changed = sync_meta(&dest, attrs, sec.as_ref(), &mut e, sum);
                if adopted {
                    sum.adopted += 1;
                } else {
                    sum.unchanged += 1;
                    if changed {
                        sum.meta_updated += 1;
                    }
                }
                if adopted || changed {
                    manifest.files.insert(key.clone(), e);
                    dirty = true;
                }
            } else {
                if fsx::free_space(&job.host_dir).is_some_and(|free| free < size.saturating_add(RESERVE)) {
                    sum.fatal = Some(MSG_FULL.into());
                    break 'sources;
                }
                if let Some(parent) = dest.parent() {
                    let _ = fs::create_dir_all(parent);
                }
                lock.note(&dest.with_file_name(tmp_name()));
                match copy_file(src_path, &dest, job.verify_after_copy, rep, buf.get(), !already_read) {
                    Ok((hash, copied)) => {
                        let mtime = meta.as_ref().map(mtime_secs).unwrap_or(0);
                        let mut e = Entry {
                            hash,
                            size: copied,
                            mtime,
                            ..Default::default()
                        };
                        sync_meta(&dest, attrs, sec.as_ref(), &mut e, sum);
                        manifest.files.insert(key.clone(), e);
                        dirty = true;
                        sum.copied += 1;
                        sum.copied_bytes += copied;
                    }
                    Err(CopyErr::Cancelled) => break 'sources,
                    Err(CopyErr::Source(e)) => sum.issue(src_path.display().to_string(), describe(&e)),
                    Err(CopyErr::Mismatch) => sum.issue(
                        src_path.display().to_string(),
                        "la copia no coincide al releerla del disco (archivo cambiando o disco con fallos)",
                    ),
                    Err(CopyErr::Dest(e)) => {
                        // Disco desconectado o lleno: no tiene sentido seguir.
                        if !job.host_dir.is_dir() {
                            sum.fatal = Some(MSG_DISCONNECTED.into());
                            break 'sources;
                        }
                        if e.kind() == io::ErrorKind::StorageFull {
                            sum.fatal = Some(MSG_FULL.into());
                            break 'sources;
                        }
                        sum.issue(
                            dest.display().to_string(),
                            format!("error al escribir: {}", describe(&e)),
                        );
                    }
                }
            }
            seen.insert(key);
            finish_file(rep, bytes_before, size);

            if dirty && last_save.elapsed() >= MANIFEST_SAVE_EVERY {
                let _ = manifest.save(&job.host_dir);
                last_save = Instant::now();
                dirty = false;
            }
        }
    }

    lock.clear();
    sum.only_in_backup = manifest
        .files
        .keys()
        .filter(|k| prefixes.iter().any(|p| k.starts_with(p.as_str())) && !seen.contains(*k))
        .count() as u64;

    if dirty
        && sum.fatal.as_deref() != Some(MSG_DISCONNECTED)
        && let Err(e) = manifest.save(&job.host_dir)
    {
        sum.fatal
            .get_or_insert(format!("no se pudo guardar el manifiesto: {e}"));
    }
    if rep.cancelled() {
        return Err(Cancelled);
    }
    Ok(())
}

/// Cierra la cuenta de un archivo: fija los bytes en `antes + tamaño` aunque se haya leído de más o de menos.
fn finish_file(rep: &Reporter, bytes_before: u64, size: u64) {
    rep.update(|p| {
        p.files_done += 1;
        p.bytes_done = bytes_before + size;
    });
}

/// Relee el disco (sin caché) y lo compara con el manifiesto. Lo dañado o faltante
/// se quita del manifiesto para que el siguiente respaldo lo vuelva a copiar.
pub fn run_verify(host_dir: &Path, rep: &Reporter) -> Summary {
    let start = Instant::now();
    let mut sum = Summary {
        verify: true,
        ..Default::default()
    };
    verify_inner(host_dir, rep, &mut sum);
    sum.seconds = start.elapsed().as_secs_f64();
    rep.update(|p| *p = Progress::default());
    sum
}

fn verify_inner(host_dir: &Path, rep: &Reporter, sum: &mut Summary) {
    let _lock = match Lock::acquire(host_dir) {
        Ok(f) => f,
        Err(msg) => {
            sum.fatal = Some(msg);
            return;
        }
    };
    let mut manifest = match Manifest::load(host_dir) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::InvalidData => {
            sum.fatal = Some("el manifiesto está dañado; ejecuta un respaldo para reconstruirlo".into());
            return;
        }
        Err(e) => {
            sum.fatal = Some(format!("no se pudo leer el manifiesto: {e}"));
            return;
        }
    };
    rep.update(|p| {
        *p = Progress {
            phase: Phase::Verifying,
            files_total: manifest.files.len() as u64,
            bytes_total: manifest.files.values().map(|e| e.size).sum(),
            ..Default::default()
        };
    });

    let mut buf = Buf::new();
    let mut bad: Vec<String> = Vec::new();
    for (key, entry) in &manifest.files {
        if rep.cancelled() {
            sum.cancelled = true;
            break;
        }
        let path = host_dir.join(key);
        rep.update(|p| p.current = path.display().to_string());
        let bytes_before = rep.bytes_done();
        match hash_file(&path, rep, buf.get(), true, Open::Direct) {
            Ok(h) if h == entry.hash => sum.unchanged += 1,
            Ok(_) => {
                sum.issue(
                    path.display().to_string(),
                    "contenido dañado: no coincide con el manifiesto",
                );
                bad.push(key.clone());
            }
            Err(ReadErr::Cancelled) => {
                sum.cancelled = true;
                break;
            }
            Err(ReadErr::Io(e)) => {
                if !host_dir.is_dir() {
                    sum.fatal = Some(MSG_DISCONNECTED.into());
                    break;
                }
                let why = if e.kind() == io::ErrorKind::NotFound {
                    "falta en el disco".to_string()
                } else {
                    format!("no se pudo leer: {}", describe(&e))
                };
                sum.issue(path.display().to_string(), why);
                bad.push(key.clone());
            }
        }
        finish_file(rep, bytes_before, entry.size);
    }

    if !bad.is_empty() && sum.fatal.is_none() {
        for k in &bad {
            manifest.files.remove(k);
        }
        if let Err(e) = manifest.save(host_dir) {
            sum.fatal = Some(format!("no se pudo guardar el manifiesto: {e}"));
        }
    }
}

/// Texto del reporte para guardar en disco.
pub fn report_text(sum: &Summary, when: SystemTime) -> String {
    use std::fmt::Write as _;
    let when: chrono::DateTime<chrono::Local> = when.into();
    let mut s = String::new();
    let _ = writeln!(
        s,
        "{} - {}",
        if sum.verify { "Verificación" } else { "Respaldo" },
        when.format("%Y-%m-%d %H:%M:%S")
    );
    let _ = writeln!(s, "Duración: {:.0} s", sum.seconds);
    if sum.verify {
        let _ = writeln!(s, "Archivos correctos: {}", sum.unchanged);
    } else {
        let _ = writeln!(s, "Copiados: {} ({} bytes)", sum.copied, sum.copied_bytes);
        let _ = writeln!(s, "Sin cambios: {}", sum.unchanged);
        let _ = writeln!(s, "Ya en el disco, anotados sin copiar: {}", sum.adopted);
        let _ = writeln!(s, "Atributos o permisos actualizados: {}", sum.meta_updated);
        let _ = writeln!(
            s,
            "Solo en respaldo (borrados del origen, conservados): {}",
            sum.only_in_backup
        );
        let _ = writeln!(s, "Enlaces y uniones omitidos: {}", sum.links_skipped);
    }
    if sum.cancelled {
        let _ = writeln!(s, "CANCELADO por el usuario");
    }
    if let Some(f) = &sum.fatal {
        let _ = writeln!(s, "ERROR: {f}");
    }
    for note in &sum.notes {
        let _ = writeln!(s, "Aviso: {note}");
    }
    let _ = writeln!(s, "Incidencias: {}", sum.issues_total);
    for (path, why) in &sum.issues {
        let _ = writeln!(s, "  {path}\n      {why}");
    }
    if !sum.links.is_empty() {
        let _ = writeln!(s, "Enlaces omitidos (primeros {}):", sum.links.len());
        for l in &sum.links {
            let _ = writeln!(s, "  {l}");
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(src: &Path, host: &Path) -> Job {
        Job {
            sources: vec![src.to_path_buf()],
            excludes: vec!["*.tmp".into(), "cache".into()],
            host_dir: host.to_path_buf(),
            verify_after_copy: true,
            preserve_acl: false,
        }
    }

    fn dest_of(src: &Path, host: &Path, rel: &str) -> PathBuf {
        host.join(map_source(src)).join(rel)
    }

    #[test]
    fn incremental_mirror() {
        let src_dir = tempfile::tempdir().unwrap();
        let disk = tempfile::tempdir().unwrap();
        let src = src_dir.path();
        let host = disk.path().join("RespaldoUSB").join("srv1");
        fs::create_dir_all(src.join("sub/vacia")).unwrap();
        fs::create_dir_all(src.join("cache")).unwrap();
        fs::write(src.join("a.txt"), "uno").unwrap();
        fs::write(src.join("sub/b.bin"), vec![7u8; 3 * BUF_SIZE + 5]).unwrap();
        fs::write(src.join("x.tmp"), "no").unwrap();
        fs::write(src.join("cache/c.txt"), "no").unwrap();
        let rep = Reporter::default();

        // Primera corrida: copia todo salvo exclusiones.
        let s = run_backup(&job(src, &host), &rep);
        assert!(s.ok(), "{s:?}");
        assert_eq!((s.copied, s.unchanged), (2, 0));
        assert_eq!(fs::read_to_string(dest_of(src, &host, "a.txt")).unwrap(), "uno");
        assert_eq!(
            fs::metadata(dest_of(src, &host, "sub/b.bin")).unwrap().len(),
            (3 * BUF_SIZE + 5) as u64
        );
        assert!(dest_of(src, &host, "sub/vacia").is_dir());
        assert!(!dest_of(src, &host, "x.tmp").exists());
        assert!(!dest_of(src, &host, "cache").exists());
        assert!(!dest_of(src, &host, &tmp_name()).exists());

        // Segunda: nada cambió.
        let s = run_backup(&job(src, &host), &rep);
        assert!(s.ok(), "{s:?}");
        assert_eq!((s.copied, s.unchanged), (0, 2));

        // Cambio de contenido con mismo tamaño: el hash lo detecta.
        fs::write(src.join("a.txt"), "dos").unwrap();
        fs::write(src.join("nuevo.txt"), "n").unwrap();
        let s = run_backup(&job(src, &host), &rep);
        assert_eq!((s.copied, s.unchanged), (2, 1));
        assert_eq!(fs::read_to_string(dest_of(src, &host, "a.txt")).unwrap(), "dos");

        // Borrado en origen: se conserva en el disco.
        fs::remove_file(src.join("nuevo.txt")).unwrap();
        let s = run_backup(&job(src, &host), &rep);
        assert_eq!((s.copied, s.unchanged, s.only_in_backup), (0, 2, 1));
        assert!(dest_of(src, &host, "nuevo.txt").exists());

        // Copia borrada del disco: se repone.
        fs::remove_file(dest_of(src, &host, "a.txt")).unwrap();
        let s = run_backup(&job(src, &host), &rep);
        assert_eq!(s.copied, 1);
        assert!(dest_of(src, &host, "a.txt").exists());
    }

    #[test]
    fn verify_detects_and_backup_repairs() {
        let src_dir = tempfile::tempdir().unwrap();
        let disk = tempfile::tempdir().unwrap();
        let src = src_dir.path();
        let host = disk.path().join("srv1");
        fs::write(src.join("a.txt"), "hola").unwrap();
        fs::write(src.join("b.txt"), "mundo").unwrap();
        let rep = Reporter::default();
        assert!(run_backup(&job(src, &host), &rep).ok());

        let v = run_verify(&host, &rep);
        assert!(v.ok(), "{v:?}");
        assert_eq!(v.unchanged, 2);

        // Corrupción silenciosa: mismo tamaño, otro contenido.
        fs::write(dest_of(src, &host, "a.txt"), "hxla").unwrap();
        let v = run_verify(&host, &rep);
        assert_eq!((v.unchanged, v.issues_total), (1, 1));

        let s = run_backup(&job(src, &host), &rep);
        assert_eq!((s.copied, s.unchanged), (1, 1));
        assert_eq!(fs::read_to_string(dest_of(src, &host, "a.txt")).unwrap(), "hola");
        assert!(run_verify(&host, &rep).ok());
    }

    #[test]
    fn readonly_dest_is_replaced_and_missing_source_reported() {
        let src_dir = tempfile::tempdir().unwrap();
        let disk = tempfile::tempdir().unwrap();
        let src = src_dir.path();
        let host = disk.path().join("srv1");
        fs::write(src.join("a.txt"), "v1").unwrap();
        let rep = Reporter::default();
        assert!(run_backup(&job(src, &host), &rep).ok());

        let dest = dest_of(src, &host, "a.txt");
        let mut perms = fs::metadata(&dest).unwrap().permissions();
        perms.set_readonly(true);
        fs::set_permissions(&dest, perms).unwrap();
        fs::write(src.join("a.txt"), "v2").unwrap();

        let mut j = job(src, &host);
        j.sources
            .push(src_dir.path().parent().unwrap().join("no-existe-respaldo-usb"));
        let s = run_backup(&j, &rep);
        assert_eq!(s.copied, 1);
        assert_eq!(s.issues_total, 1);
        assert_eq!(fs::read_to_string(&dest).unwrap(), "v2");
    }

    #[test]
    fn cancel_stops_early() {
        let src_dir = tempfile::tempdir().unwrap();
        let disk = tempfile::tempdir().unwrap();
        fs::write(src_dir.path().join("a.txt"), "x").unwrap();
        let rep = Reporter::default();
        rep.cancel.store(true, Ordering::Relaxed);
        let s = run_backup(&job(src_dir.path(), &disk.path().join("h")), &rep);
        assert!(s.cancelled);
        assert_eq!(s.copied, 0);
    }

    #[test]
    fn damaged_manifest_is_rebuilt_without_recopying() {
        let src_dir = tempfile::tempdir().unwrap();
        let disk = tempfile::tempdir().unwrap();
        let src = src_dir.path();
        let host = disk.path().join("srv1");
        fs::write(src.join("a.txt"), "hola").unwrap();
        fs::write(src.join("b.txt"), "mundo").unwrap();
        let rep = Reporter::default();
        assert!(run_backup(&job(src, &host), &rep).ok());

        fs::write(host.join(MANIFEST_NAME), "{ esto no es json").unwrap();
        assert!(run_verify(&host, &rep).fatal.is_some());

        // Un archivo cambió mientras tanto: ese sí se copia, el otro solo se anota.
        fs::write(src.join("b.txt"), "MUNDO").unwrap();
        let s = run_backup(&job(src, &host), &rep);
        assert!(s.ok(), "{s:?}");
        assert_eq!((s.copied, s.adopted, s.unchanged), (1, 1, 0));
        assert_eq!(s.notes.len(), 1);
        assert!(run_verify(&host, &rep).ok());
        let s = run_backup(&job(src, &host), &rep);
        assert_eq!((s.copied, s.adopted, s.unchanged), (0, 0, 2));
    }

    #[test]
    fn second_process_is_locked_out() {
        let src_dir = tempfile::tempdir().unwrap();
        let disk = tempfile::tempdir().unwrap();
        let host = disk.path().join("srv1");
        fs::write(src_dir.path().join("a.txt"), "x").unwrap();
        fs::create_dir_all(&host).unwrap();
        let rep = Reporter::default();

        let held = Lock::acquire(&host).unwrap();
        let s = run_backup(&job(src_dir.path(), &host), &rep);
        assert!(s.fatal.is_some());
        assert_eq!(s.copied, 0);
        assert!(run_verify(&host, &rep).fatal.is_some());
        drop(held);
        assert!(run_backup(&job(src_dir.path(), &host), &rep).ok());
    }

    #[test]
    fn orphan_temp_from_interrupted_run_is_removed() {
        let src_dir = tempfile::tempdir().unwrap();
        let disk = tempfile::tempdir().unwrap();
        let src = src_dir.path();
        let host = disk.path().join("srv1");
        fs::write(src.join("a.txt"), "a").unwrap();
        let rep = Reporter::default();
        assert!(run_backup(&job(src, &host), &rep).ok());

        // Simula un corte: quedó un temporal a medias y su ruta anotada en el diario.
        let orphan = dest_of(src, &host, "~respaldo-99999.tmp");
        fs::write(&orphan, "a medias").unwrap();
        fs::write(host.join(LOCK_NAME), orphan.to_string_lossy().as_bytes()).unwrap();
        // El diario nunca debe servir para borrar otra cosa.
        let innocent = dest_of(src, &host, "a.txt");

        assert!(run_backup(&job(src, &host), &rep).ok());
        assert!(!orphan.exists());
        assert!(innocent.exists());
        assert_eq!(fs::read(host.join(LOCK_NAME)).unwrap(), b"");

        fs::write(host.join(LOCK_NAME), innocent.to_string_lossy().as_bytes()).unwrap();
        assert!(run_verify(&host, &rep).ok());
        assert!(innocent.exists());
    }

    #[test]
    fn invalid_exclude_patterns_are_reported() {
        let patterns = vec!["*.tmp".to_string(), "[abc".to_string(), "  ".to_string()];
        assert_eq!(invalid_patterns(&patterns), vec!["[abc".to_string()]);

        let src_dir = tempfile::tempdir().unwrap();
        let disk = tempfile::tempdir().unwrap();
        fs::write(src_dir.path().join("a.txt"), "a").unwrap();
        fs::write(src_dir.path().join("b.tmp"), "b").unwrap();
        let mut j = job(src_dir.path(), &disk.path().join("h"));
        j.excludes = patterns;
        let s = run_backup(&j, &Reporter::default());
        assert!(s.ok(), "{s:?}");
        assert_eq!((s.copied, s.notes.len()), (1, 1));
    }

    #[test]
    fn type_change_sets_old_copy_aside() {
        let src_dir = tempfile::tempdir().unwrap();
        let disk = tempfile::tempdir().unwrap();
        let src = src_dir.path();
        let host = disk.path().join("srv1");
        fs::write(src.join("cosa"), "archivo").unwrap();
        fs::create_dir_all(src.join("otra")).unwrap();
        fs::write(src.join("otra/dentro.txt"), "d").unwrap();
        let rep = Reporter::default();
        assert!(run_backup(&job(src, &host), &rep).ok());

        // Archivo -> carpeta y carpeta -> archivo.
        fs::remove_file(src.join("cosa")).unwrap();
        fs::create_dir_all(src.join("cosa")).unwrap();
        fs::write(src.join("cosa/hijo.txt"), "h").unwrap();
        fs::remove_dir_all(src.join("otra")).unwrap();
        fs::write(src.join("otra"), "ahora archivo").unwrap();

        let s = run_backup(&job(src, &host), &rep);
        assert!(s.ok(), "{s:?}");
        assert_eq!(s.notes.len(), 2);
        assert_eq!(fs::read_to_string(dest_of(src, &host, "cosa/hijo.txt")).unwrap(), "h");
        assert_eq!(
            fs::read_to_string(dest_of(src, &host, "otra")).unwrap(),
            "ahora archivo"
        );
        // Nada se borró: las copias anteriores siguen, renombradas.
        let names: Vec<String> = fs::read_dir(dest_of(src, &host, ""))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names.iter().filter(|n| n.contains(".~anterior-")).count(),
            2,
            "{names:?}"
        );
        assert!(run_verify(&host, &rep).ok());
    }

    #[test]
    fn nested_and_self_containing_sources() {
        let src_dir = tempfile::tempdir().unwrap();
        let src = src_dir.path();
        fs::create_dir_all(src.join("datos/sub")).unwrap();
        fs::write(src.join("datos/sub/a.txt"), "a").unwrap();
        let rep = Reporter::default();

        // Origen anidado dentro de otro: se procesa una sola vez.
        let disk = tempfile::tempdir().unwrap();
        let host = disk.path().join("srv1");
        let mut j = job(&src.join("datos"), &host);
        j.sources.push(src.join("datos/sub"));
        let s = run_backup(&j, &rep);
        assert!(s.ok(), "{s:?}");
        assert_eq!(s.copied, 1);
        assert_eq!(s.notes.len(), 1);

        // Origen que contiene el destino: se omite, no hay recursión.
        let host_inside = src.join("datos/RespaldoUSB/srv1");
        let s = run_backup(&job(&src.join("datos"), &host_inside), &rep);
        assert_eq!((s.copied, s.issues_total), (0, 1));
    }

    #[test]
    fn links_are_counted_not_reported_as_errors() {
        let src_dir = tempfile::tempdir().unwrap();
        let disk = tempfile::tempdir().unwrap();
        let src = src_dir.path();
        fs::write(src.join("a.txt"), "a").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(src.join("a.txt"), src.join("enlace")).unwrap();
        #[cfg(windows)]
        if std::os::windows::fs::symlink_file(src.join("a.txt"), src.join("enlace")).is_err() {
            return; // sin permiso para crear enlaces en este equipo
        }
        let s = run_backup(&job(src, &disk.path().join("h")), &Reporter::default());
        assert!(s.ok(), "{s:?}");
        assert_eq!((s.copied, s.links_skipped), (1, 1));
    }

    #[test]
    fn same_hostname_different_machine_gets_own_folder() {
        let disk = tempfile::tempdir().unwrap();
        let root = disk.path();
        let a = resolve_host_dir(root, "SRV", "aaaaaaaaaaaa").unwrap();
        assert_eq!(a, root.join("SRV"));
        assert_eq!(resolve_host_dir(root, "SRV", "aaaaaaaaaaaa").unwrap(), a);
        let b = resolve_host_dir(root, "SRV", "bbbbbbbbbbbb").unwrap();
        assert_eq!(b, root.join("SRV-bbbbbb"));
        assert_eq!(resolve_host_dir(root, "SRV", "bbbbbbbbbbbb").unwrap(), b);
        // Carpeta de una versión anterior, sin marcador: la reclama el primero que llega.
        fs::create_dir_all(root.join("VIEJO")).unwrap();
        assert_eq!(
            resolve_host_dir(root, "VIEJO", "cccccccccccc").unwrap(),
            root.join("VIEJO")
        );
        assert_eq!(
            resolve_host_dir(root, "VIEJO", "dddddddddddd").unwrap(),
            root.join("VIEJO-dddddd")
        );
    }

    #[test]
    fn maps_sources() {
        #[cfg(windows)]
        {
            assert_eq!(map_source(Path::new(r"c:\Datos\X")), PathBuf::from(r"C\Datos\X"));
            assert_eq!(
                map_source(Path::new(r"\\srv\share\X")),
                PathBuf::from(r"UNC\srv\share\X")
            );
            assert!(path_contains(Path::new(r"C:\Datos"), Path::new(r"c:\datos\Sub")));
        }
        #[cfg(not(windows))]
        assert_eq!(map_source(Path::new("/home/x")), PathBuf::from("raiz/home/x"));
        assert!(path_contains(Path::new("/a/b"), Path::new("/a/b/c")));
        assert!(!path_contains(Path::new("/a/b"), Path::new("/a/bc")));
    }

    #[cfg(windows)]
    #[test]
    fn windows_attributes_and_permissions_are_copied() {
        use std::os::windows::fs::MetadataExt;
        use std::process::Command;
        let src_dir = tempfile::tempdir().unwrap();
        let disk = tempfile::tempdir().unwrap();
        let src = src_dir.path();
        let host = disk.path().join("srv1");
        fs::write(src.join("oculto.txt"), "o").unwrap();
        fs::write(src.join("acl.txt"), "p").unwrap();
        fsx::set_attributes(&src.join("oculto.txt"), 0x1 | 0x2 | 0x20).unwrap();
        // Concede lectura a ANONYMOUS LOGON (S-1-5-7): un ACE fácil de reconocer.
        let grant = |p: &Path| {
            assert!(
                Command::new("icacls")
                    .arg(p)
                    .args(["/grant", "*S-1-5-7:(R)"])
                    .output()
                    .unwrap()
                    .status
                    .success()
            );
        };
        let has_ace = |p: &Path| {
            let out = Command::new("icacls").arg(p).output().unwrap();
            String::from_utf8_lossy(&out.stdout).contains("ANONYMOUS LOGON")
        };
        grant(&src.join("acl.txt"));
        fsx::enable_backup_privileges();

        let mut j = job(src, &host);
        j.preserve_acl = true;
        let rep = Reporter::default();
        let s = run_backup(&j, &rep);
        assert!(s.fatal.is_none() && s.issues_total == 0, "{s:?}");
        let attrs = fs::metadata(dest_of(src, &host, "oculto.txt"))
            .unwrap()
            .file_attributes();
        assert_eq!(attrs & 0x3, 0x3, "solo lectura y oculto");
        assert!(has_ace(&dest_of(src, &host, "acl.txt")));
        assert!(!has_ace(&dest_of(src, &host, "oculto.txt")));

        // Cambio solo de permisos y atributos, mismo contenido: se actualizan sin recopiar.
        grant(&src.join("oculto.txt"));
        fsx::set_attributes(&src.join("oculto.txt"), 0x20).unwrap();
        let s = run_backup(&j, &rep);
        assert_eq!((s.copied, s.unchanged, s.meta_updated), (0, 2, 1), "{s:?}");
        assert!(has_ace(&dest_of(src, &host, "oculto.txt")));
        let attrs = fs::metadata(dest_of(src, &host, "oculto.txt"))
            .unwrap()
            .file_attributes();
        assert_eq!(attrs & 0x3, 0);

        // Contenido cambiado sobre copia de solo lectura y oculta: se reemplaza igual.
        fsx::set_attributes(&src.join("oculto.txt"), 0x1 | 0x2).unwrap();
        assert_eq!(run_backup(&j, &rep).meta_updated, 1);
        fsx::set_attributes(&src.join("oculto.txt"), 0x20).unwrap();
        fs::write(src.join("oculto.txt"), "nuevo").unwrap();
        fsx::set_attributes(&src.join("oculto.txt"), 0x1 | 0x2).unwrap();
        let s = run_backup(&j, &rep);
        assert_eq!(s.copied, 1, "{s:?}");
        assert!(run_verify(&host, &rep).ok());
    }
}
