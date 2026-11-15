//! Motor de respaldo: espejo incremental por hash BLAKE3.
//!
//! Disposición en el disco:
//!   <disco>/RespaldoUSB/<servidor>/manifiesto.json
//!   <disco>/RespaldoUSB/<servidor>/C/Datos/...        (archivos normales)
//!
//! Reglas:
//! - Se copia un archivo si su hash difiere del manifiesto o si falta en el disco.
//! - Nunca se borra nada del disco, aunque desaparezca del origen.
//! - Copia a temporal + renombrado: un corte no deja archivos a medias.

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf, Prefix};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use walkdir::WalkDir;

const MANIFEST_NAME: &str = "manifiesto.json";
const TMP_NAME: &str = "~respaldo.tmp";
const BUF_SIZE: usize = 1 << 20;
const MANIFEST_SAVE_EVERY: Duration = Duration::from_secs(300);
/// Tope de incidencias guardadas en memoria; el resto solo se cuenta.
const MAX_ISSUES: usize = 5000;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Entry {
    pub hash: String,
    pub size: u64,
    /// Fecha de modificación del origen (segundos Unix). Solo informativo.
    pub mtime: i64,
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
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self { version: 1, ..Default::default() }),
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
        f(&mut self.progress.lock().unwrap());
    }
    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

#[derive(Debug, Clone, Default)]
pub struct Summary {
    pub verify: bool,
    pub copied: u64,
    pub copied_bytes: u64,
    pub unchanged: u64,
    /// Archivos presentes en el disco que ya no existen en el origen (se conservan).
    pub only_in_backup: u64,
    /// (ruta, motivo) de archivos omitidos o con error.
    pub issues: Vec<(String, String)>,
    pub issues_total: u64,
    pub cancelled: bool,
    /// Error que detuvo la operación (disco desconectado, lleno, etc.).
    pub fatal: Option<String>,
    pub seconds: f64,
}

impl Summary {
    fn issue(&mut self, path: impl Into<String>, why: impl std::fmt::Display) {
        self.issues_total += 1;
        if self.issues.len() < MAX_ISSUES {
            self.issues.push((path.into(), why.to_string()));
        }
    }

    pub fn ok(&self) -> bool {
        self.fatal.is_none() && !self.cancelled && self.issues_total == 0
    }
}

pub struct Job {
    pub sources: Vec<PathBuf>,
    pub excludes: Vec<String>,
    /// <disco>/RespaldoUSB/<servidor>
    pub host_dir: PathBuf,
    pub verify_after_copy: bool,
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

fn build_excludes(patterns: &[String]) -> GlobSet {
    let mut b = GlobSetBuilder::new();
    for p in patterns {
        let p = p.trim();
        if p.is_empty() {
            continue;
        }
        if let Ok(g) = GlobBuilder::new(p).case_insensitive(true).literal_separator(false).build() {
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

struct Cancelled;

enum ReadErr {
    Cancelled,
    Io(io::Error),
}

fn hash_file(path: &Path, rep: &Reporter, buf: &mut [u8], count_bytes: bool) -> Result<String, ReadErr> {
    let mut f = File::open(path).map_err(ReadErr::Io)?;
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
    let mut input = File::open(src).map_err(CopyErr::Source)?;
    let src_mtime = input.metadata().ok().and_then(|m| m.modified().ok());
    let tmp = dest.with_file_name(TMP_NAME);
    let result = (|| {
        let mut out = File::create(&tmp).map_err(CopyErr::Dest)?;
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
        if let Some(t) = src_mtime {
            let _ = out.set_modified(t);
        }
        let hash = hasher.finalize().to_hex().to_string();
        if verify {
            out.sync_all().map_err(CopyErr::Dest)?;
            drop(out);
            match hash_file(&tmp, rep, buf, false) {
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
    let mut manifest = match Manifest::load(&job.host_dir) {
        Ok(m) => m,
        Err(e) => {
            sum.fatal = Some(e.to_string());
            return Ok(());
        }
    };

    // Pasada 1: contar para la barra de progreso.
    rep.update(|p| {
        *p = Progress { phase: Phase::Scanning, ..Default::default() };
    });
    for src in &job.sources {
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
    let mut buf = vec![0u8; BUF_SIZE];
    let mut seen: HashSet<String> = HashSet::new();
    let mut prefixes: Vec<String> = Vec::new();
    let mut last_save = Instant::now();
    let mut dirty = false;

    'sources: for src in &job.sources {
        if !src.is_dir() {
            sum.issue(src.display().to_string(), "carpeta de origen no existe o no es accesible");
            continue;
        }
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
            let rel = mapped.join(entry.path().strip_prefix(src).unwrap_or(Path::new("")));
            let dest = job.host_dir.join(&rel);
            let ftype = entry.file_type();
            if ftype.is_dir() {
                let _ = fs::create_dir_all(&dest);
                continue;
            }
            if !ftype.is_file() {
                sum.issue(entry.path().display().to_string(), "enlace simbólico o archivo especial, omitido");
                continue;
            }

            let src_path = entry.path();
            let key = key_of(&rel);
            let meta = entry.metadata().ok();
            let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
            rep.update(|p| p.current = src_path.display().to_string());
            let bytes_before = rep.progress.lock().unwrap().bytes_done;

            // ¿Sin cambios? Solo si el manifiesto lo conoce y la copia sigue en el disco.
            let mut need_copy = true;
            let mut already_read = false;
            if let Some(known) = manifest.files.get(&key) {
                let dest_ok = fs::metadata(&dest).map(|m| m.is_file() && m.len() == known.size).unwrap_or(false);
                if dest_ok {
                    match hash_file(src_path, rep, &mut buf, true) {
                        Ok(h) => {
                            already_read = true;
                            need_copy = h != known.hash;
                        }
                        Err(ReadErr::Cancelled) => break 'sources,
                        Err(ReadErr::Io(e)) => {
                            seen.insert(key);
                            sum.issue(src_path.display().to_string(), describe(&e));
                            finish_file(rep, bytes_before, size);
                            continue;
                        }
                    }
                }
            }

            if need_copy {
                if let Some(parent) = dest.parent() {
                    let _ = fs::create_dir_all(parent);
                }
                match copy_file(src_path, &dest, job.verify_after_copy, rep, &mut buf, !already_read) {
                    Ok((hash, copied)) => {
                        let mtime = meta.as_ref().map(mtime_secs).unwrap_or(0);
                        manifest.files.insert(key.clone(), Entry { hash, size: copied, mtime });
                        dirty = true;
                        sum.copied += 1;
                        sum.copied_bytes += copied;
                    }
                    Err(CopyErr::Cancelled) => break 'sources,
                    Err(CopyErr::Source(e)) => sum.issue(src_path.display().to_string(), describe(&e)),
                    Err(CopyErr::Mismatch) => sum.issue(
                        src_path.display().to_string(),
                        "la copia no coincide al releerla (archivo cambiando o disco con fallos)",
                    ),
                    Err(CopyErr::Dest(e)) => {
                        // Disco desconectado o lleno: no tiene sentido seguir.
                        if !job.host_dir.is_dir() {
                            sum.fatal = Some("el disco de respaldo se desconectó".into());
                            break 'sources;
                        }
                        if e.kind() == io::ErrorKind::StorageFull {
                            sum.fatal = Some("el disco de respaldo está lleno".into());
                            break 'sources;
                        }
                        sum.issue(dest.display().to_string(), format!("error al escribir: {}", describe(&e)));
                    }
                }
            } else {
                sum.unchanged += 1;
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

    sum.only_in_backup = manifest
        .files
        .keys()
        .filter(|k| prefixes.iter().any(|p| k.starts_with(p.as_str())) && !seen.contains(*k))
        .count() as u64;

    if dirty && sum.fatal.as_deref() != Some("el disco de respaldo se desconectó") {
        if let Err(e) = manifest.save(&job.host_dir) {
            sum.fatal.get_or_insert(format!("no se pudo guardar el manifiesto: {e}"));
        }
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

/// Relee el disco y lo compara con el manifiesto. Lo dañado o faltante se
/// quita del manifiesto para que el siguiente respaldo lo vuelva a copiar.
pub fn run_verify(host_dir: &Path, rep: &Reporter) -> Summary {
    let start = Instant::now();
    let mut sum = Summary { verify: true, ..Default::default() };
    let mut manifest = match Manifest::load(host_dir) {
        Ok(m) => m,
        Err(e) => {
            sum.fatal = Some(e.to_string());
            return sum;
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

    let mut buf = vec![0u8; BUF_SIZE];
    let mut bad: Vec<String> = Vec::new();
    for (key, entry) in &manifest.files {
        if rep.cancelled() {
            sum.cancelled = true;
            break;
        }
        let path = host_dir.join(key);
        rep.update(|p| p.current = path.display().to_string());
        let bytes_before = rep.progress.lock().unwrap().bytes_done;
        match hash_file(&path, rep, &mut buf, true) {
            Ok(h) if h == entry.hash => sum.unchanged += 1,
            Ok(_) => {
                sum.issue(path.display().to_string(), "contenido dañado: no coincide con el manifiesto");
                bad.push(key.clone());
            }
            Err(ReadErr::Cancelled) => {
                sum.cancelled = true;
                break;
            }
            Err(ReadErr::Io(e)) => {
                if !host_dir.is_dir() {
                    sum.fatal = Some("el disco de respaldo se desconectó".into());
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
    sum.seconds = start.elapsed().as_secs_f64();
    rep.update(|p| *p = Progress::default());
    sum
}

/// Texto del reporte para guardar en disco.
pub fn report_text(sum: &Summary, when: SystemTime) -> String {
    use std::fmt::Write as _;
    let when: chrono::DateTime<chrono::Local> = when.into();
    let mut s = String::new();
    let _ = writeln!(s, "{} - {}", if sum.verify { "Verificación" } else { "Respaldo" }, when.format("%Y-%m-%d %H:%M:%S"));
    let _ = writeln!(s, "Duración: {:.0} s", sum.seconds);
    if sum.verify {
        let _ = writeln!(s, "Archivos correctos: {}", sum.unchanged);
    } else {
        let _ = writeln!(s, "Copiados: {} ({} bytes)", sum.copied, sum.copied_bytes);
        let _ = writeln!(s, "Sin cambios: {}", sum.unchanged);
        let _ = writeln!(s, "Solo en respaldo (borrados del origen, conservados): {}", sum.only_in_backup);
    }
    if sum.cancelled {
        let _ = writeln!(s, "CANCELADO por el usuario");
    }
    if let Some(f) = &sum.fatal {
        let _ = writeln!(s, "ERROR: {f}");
    }
    let _ = writeln!(s, "Incidencias: {}", sum.issues_total);
    for (path, why) in &sum.issues {
        let _ = writeln!(s, "  {path}\n      {why}");
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
        assert_eq!(fs::metadata(dest_of(src, &host, "sub/b.bin")).unwrap().len(), (3 * BUF_SIZE + 5) as u64);
        assert!(dest_of(src, &host, "sub/vacia").is_dir());
        assert!(!dest_of(src, &host, "x.tmp").exists());
        assert!(!dest_of(src, &host, "cache").exists());
        assert!(!dest_of(src, &host, TMP_NAME).exists());

        // Segunda: nada cambió.
        let s = run_backup(&job(src, &host), &rep);
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
        assert!(v.ok());
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
        j.sources.push(src.join("no-existe"));
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
    fn maps_sources() {
        #[cfg(windows)]
        {
            assert_eq!(map_source(Path::new(r"c:\Datos\X")), PathBuf::from(r"C\Datos\X"));
            assert_eq!(map_source(Path::new(r"\\srv\share\X")), PathBuf::from(r"UNC\srv\share\X"));
        }
        #[cfg(not(windows))]
        assert_eq!(map_source(Path::new("/home/x")), PathBuf::from("raiz/home/x"));
    }
}
