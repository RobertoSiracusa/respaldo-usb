//! Operaciones de archivo específicas de Windows: lectura sin caché, atributos,
//! permisos NTFS, espacio libre y privilegios de respaldo.
//! En otros sistemas son equivalentes simples o no hacen nada (solo desarrollo).

use std::sync::atomic::{AtomicBool, Ordering};

static PRIVILEGED: AtomicBool = AtomicBool::new(false);

/// `true` si el proceso tiene activos los privilegios de respaldo (ejecutado como administrador).
pub fn has_backup_privileges() -> bool {
    PRIVILEGED.load(Ordering::Relaxed)
}

/// Intenta activar SeBackupPrivilege y SeRestorePrivilege. Solo funciona con la app elevada.
pub fn enable_backup_privileges() -> bool {
    let ok = imp::enable_backup_privileges();
    PRIVILEGED.store(ok, Ordering::Relaxed);
    ok
}

pub use imp::{
    attributes, copy_times, create_dest, free_space, open_direct, open_source, read_security, set_attributes,
    volume_supports_acl, write_security,
};

#[cfg(windows)]
mod imp {
    use std::ffi::c_void;
    use std::fs::{File, FileTimes, Metadata, OpenOptions};
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::fs::{FileTimesExt, MetadataExt, OpenOptionsExt};
    use std::path::Path;
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, LUID};
    use windows_sys::Win32::Security::{
        AdjustTokenPrivileges, GetFileSecurityW, LUID_AND_ATTRIBUTES, LookupPrivilegeValueW, SetFileSecurityW,
        TOKEN_PRIVILEGES,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        GetDiskFreeSpaceExW, GetVolumeInformationW, GetVolumePathNameW, SetFileAttributesW,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_FLAG_NO_BUFFERING: u32 = 0x2000_0000;
    const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;
    /// Solo lectura, oculto, sistema, archivo, no indexar.
    const ATTR_MASK: u32 = 0x1 | 0x2 | 0x4 | 0x20 | 0x2000;
    const FILE_PERSISTENT_ACLS: u32 = 0x8;
    const OWNER_GROUP_DACL: u32 = 0x1 | 0x2 | 0x4;
    const DACL_ONLY: u32 = 0x4;
    const TOKEN_ADJUST_PRIVILEGES: u32 = 0x20;
    const TOKEN_QUERY: u32 = 0x8;
    const SE_PRIVILEGE_ENABLED: u32 = 0x2;

    /// Ruta en UTF-16 con prefijo `\\?\` para que las API Win32 directas acepten rutas largas.
    fn wide(path: &Path) -> Vec<u16> {
        let text = path.to_string_lossy();
        let mut out: Vec<u16> = Vec::with_capacity(text.len() + 8);
        if text.starts_with(r"\\?\") || !path.is_absolute() {
            out.extend(path.as_os_str().encode_wide());
        } else if text.starts_with(r"\\") {
            out.extend(r"\\?\UNC".encode_utf16());
            out.extend(path.as_os_str().encode_wide().skip(1));
        } else {
            out.extend(r"\\?\".encode_utf16());
            out.extend(path.as_os_str().encode_wide());
        }
        out.push(0);
        out
    }

    fn wide_str(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Abre para leer con intención de respaldo: con el privilegio activo, lee aunque los permisos no lo concedan.
    pub fn open_source(path: &Path) -> io::Result<File> {
        OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)
    }

    /// Abre sin caché de Windows: lo leído viene del dispositivo, no de la memoria.
    /// Exige lecturas alineadas a sector (búfer y tamaño).
    pub fn open_direct(path: &Path) -> io::Result<File> {
        OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_NO_BUFFERING | FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)
    }

    pub fn create_dest(path: &Path) -> io::Result<File> {
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)
    }

    pub fn attributes(meta: &Metadata) -> u32 {
        meta.file_attributes() & ATTR_MASK
    }

    pub fn set_attributes(path: &Path, attrs: u32) -> io::Result<()> {
        let attrs = if attrs & ATTR_MASK == 0 {
            FILE_ATTRIBUTE_NORMAL
        } else {
            attrs & ATTR_MASK
        };
        if unsafe { SetFileAttributesW(wide(path).as_ptr(), attrs) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Copia fechas de creación y modificación del origen al archivo destino.
    pub fn copy_times(src: &Metadata, dest: &File) {
        let mut times = FileTimes::new();
        if let Ok(t) = src.modified() {
            times = times.set_modified(t);
        }
        if let Ok(t) = src.created() {
            times = times.set_created(t);
        }
        let _ = dest.set_times(times);
    }

    /// Descriptor de seguridad (propietario, grupo y DACL) en formato binario.
    pub fn read_security(path: &Path) -> io::Result<Vec<u8>> {
        let w = wide(path);
        let mut needed = 0u32;
        unsafe { GetFileSecurityW(w.as_ptr(), OWNER_GROUP_DACL, std::ptr::null_mut(), 0, &mut needed) };
        if needed == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut buf = vec![0u8; needed as usize];
        let ok = unsafe {
            GetFileSecurityW(
                w.as_ptr(),
                OWNER_GROUP_DACL,
                buf.as_mut_ptr() as *mut c_void,
                needed,
                &mut needed,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(buf)
    }

    /// Aplica el descriptor. Si no se puede fijar el propietario (falta privilegio), aplica solo la DACL.
    pub fn write_security(path: &Path, sd: &[u8]) -> io::Result<()> {
        let w = wide(path);
        let ptr = sd.as_ptr() as *mut c_void;
        if unsafe { SetFileSecurityW(w.as_ptr(), OWNER_GROUP_DACL, ptr) } != 0 {
            return Ok(());
        }
        if unsafe { SetFileSecurityW(w.as_ptr(), DACL_ONLY, ptr) } != 0 {
            return Ok(());
        }
        Err(io::Error::last_os_error())
    }

    /// `true` si el volumen que contiene `path` guarda permisos (NTFS/ReFS; no exFAT ni FAT32).
    pub fn volume_supports_acl(path: &Path) -> bool {
        let mut root = [0u16; 300];
        if unsafe { GetVolumePathNameW(wide(path).as_ptr(), root.as_mut_ptr(), root.len() as u32) } == 0 {
            return false;
        }
        let mut flags = 0u32;
        let ok = unsafe {
            GetVolumeInformationW(
                root.as_ptr(),
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut flags,
                std::ptr::null_mut(),
                0,
            )
        };
        ok != 0 && flags & FILE_PERSISTENT_ACLS != 0
    }

    /// Bytes libres para el usuario en el volumen de `dir`.
    pub fn free_space(dir: &Path) -> Option<u64> {
        let mut avail = 0u64;
        let ok = unsafe {
            GetDiskFreeSpaceExW(
                wide(dir).as_ptr(),
                &mut avail,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        (ok != 0).then_some(avail)
    }

    pub fn enable_backup_privileges() -> bool {
        unsafe {
            let mut token = std::ptr::null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY, &mut token) == 0 {
                return false;
            }
            let mut all = true;
            for name in ["SeBackupPrivilege", "SeRestorePrivilege"] {
                let mut luid = LUID {
                    LowPart: 0,
                    HighPart: 0,
                };
                if LookupPrivilegeValueW(std::ptr::null(), wide_str(name).as_ptr(), &mut luid) == 0 {
                    all = false;
                    continue;
                }
                let privileges = TOKEN_PRIVILEGES {
                    PrivilegeCount: 1,
                    Privileges: [LUID_AND_ATTRIBUTES {
                        Luid: luid,
                        Attributes: SE_PRIVILEGE_ENABLED,
                    }],
                };
                let ok = AdjustTokenPrivileges(token, 0, &privileges, 0, std::ptr::null_mut(), std::ptr::null_mut());
                // Devuelve éxito aunque no asigne el privilegio; el detalle está en GetLastError.
                if ok == 0 || GetLastError() != 0 {
                    all = false;
                }
            }
            CloseHandle(token);
            all
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use std::fs::{File, Metadata};
    use std::io;
    use std::path::Path;

    pub fn open_source(path: &Path) -> io::Result<File> {
        File::open(path)
    }
    pub fn open_direct(path: &Path) -> io::Result<File> {
        File::open(path)
    }
    pub fn create_dest(path: &Path) -> io::Result<File> {
        File::create(path)
    }
    pub fn attributes(_meta: &Metadata) -> u32 {
        0
    }
    pub fn set_attributes(_path: &Path, _attrs: u32) -> io::Result<()> {
        Ok(())
    }
    pub fn copy_times(src: &Metadata, dest: &File) {
        if let Ok(t) = src.modified() {
            let _ = dest.set_modified(t);
        }
    }
    pub fn read_security(_path: &Path) -> io::Result<Vec<u8>> {
        Err(io::Error::other("solo disponible en Windows"))
    }
    pub fn write_security(_path: &Path, _sd: &[u8]) -> io::Result<()> {
        Err(io::Error::other("solo disponible en Windows"))
    }
    pub fn volume_supports_acl(_path: &Path) -> bool {
        false
    }
    pub fn free_space(_dir: &Path) -> Option<u64> {
        None
    }
    pub fn enable_backup_privileges() -> bool {
        false
    }
}
