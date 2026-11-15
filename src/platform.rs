//! Integración con Windows: instancia única, ventana, inicio automático y bandeja.
//! En otros sistemas son funciones vacías (solo para desarrollo).

pub const WINDOW_TITLE: &str = "Respaldo USB";

#[cfg_attr(not(windows), allow(dead_code))]
pub enum Instance {
    /// No hay otra copia: esta sigue.
    First,
    /// Ya abierta en esta sesión; se trajo su ventana al frente.
    SameSession,
    /// Abierta por otro usuario del mismo servidor.
    OtherSession,
}

#[cfg(windows)]
mod imp {
    use super::{Instance, WINDOW_TITLE};
    use std::ffi::c_void;
    use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
    use windows_sys::Win32::System::Threading::CreateMutexW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        FindWindowW, IsIconic, MB_ICONINFORMATION, MB_OK, MessageBoxW, SW_HIDE, SW_RESTORE, SW_SHOW,
        SetForegroundWindow, ShowWindow,
    };

    const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    const RUN_VALUE: &str = "RespaldoUSB";

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Reserva la instancia única en todo el equipo (todas las sesiones de usuario).
    pub fn claim_instance() -> Instance {
        let name = wide(r"Global\RespaldoUSB-instancia-unica");
        // El handle se deja abierto a propósito: vive lo que viva el proceso.
        let handle = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
        let error = unsafe { GetLastError() };
        if !handle.is_null() && error != ERROR_ALREADY_EXISTS {
            return Instance::First;
        }
        // Existe: o es de esta sesión (hay ventana que mostrar) o de otro usuario (acceso denegado o sin ventana).
        let title = wide(WINDOW_TITLE);
        let hwnd = unsafe { FindWindowW(std::ptr::null(), title.as_ptr()) };
        if hwnd.is_null() {
            return Instance::OtherSession;
        }
        show_window(hwnd as isize);
        Instance::SameSession
    }

    pub fn message_box(text: &str) {
        let (text, title) = (wide(text), wide(WINDOW_TITLE));
        unsafe {
            MessageBoxW(
                std::ptr::null_mut(),
                text.as_ptr(),
                title.as_ptr(),
                MB_OK | MB_ICONINFORMATION,
            )
        };
    }

    /// Identificador estable de la instalación de Windows.
    pub fn machine_guid() -> Option<String> {
        use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_64KEY};
        winreg::RegKey::predef(HKEY_LOCAL_MACHINE)
            .open_subkey_with_flags(r"SOFTWARE\Microsoft\Cryptography", KEY_READ | KEY_WOW64_64KEY)
            .and_then(|k| k.get_value::<String, _>("MachineGuid"))
            .ok()
    }

    pub fn show_window(hwnd: isize) {
        if hwnd == 0 {
            return;
        }
        let hwnd = hwnd as *mut c_void;
        unsafe {
            ShowWindow(hwnd, SW_SHOW);
            if IsIconic(hwnd) != 0 {
                ShowWindow(hwnd, SW_RESTORE);
            }
            SetForegroundWindow(hwnd);
        }
    }

    pub fn hide_window(hwnd: isize) {
        if hwnd != 0 {
            unsafe { ShowWindow(hwnd as *mut c_void, SW_HIDE) };
        }
    }

    pub fn autostart_enabled() -> bool {
        winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER)
            .open_subkey(RUN_KEY)
            .and_then(|k| k.get_value::<String, _>(RUN_VALUE))
            .is_ok()
    }

    pub fn set_autostart(enabled: bool) -> std::io::Result<()> {
        let (key, _) = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER).create_subkey(RUN_KEY)?;
        if enabled {
            let exe = std::env::current_exe()?;
            key.set_value(RUN_VALUE, &format!("\"{}\" {}", exe.display(), super::ARG_MINIMIZED))
        } else {
            match key.delete_value(RUN_VALUE) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            }
        }
    }
}

#[cfg(not(windows))]
mod imp {
    pub fn claim_instance() -> super::Instance {
        super::Instance::First
    }
    pub fn message_box(text: &str) {
        eprintln!("{text}");
    }
    pub fn machine_guid() -> Option<String> {
        None
    }
    pub fn show_window(_hwnd: isize) {}
    pub fn hide_window(_hwnd: isize) {}
    pub fn autostart_enabled() -> bool {
        false
    }
    pub fn set_autostart(_enabled: bool) -> std::io::Result<()> {
        Err(std::io::Error::other("solo disponible en Windows"))
    }
}

pub use imp::*;

/// Argumento con el que el inicio automático lanza la app oculta en la bandeja.
pub const ARG_MINIMIZED: &str = "--minimizado";

/// Icono de bandeja. Sus eventos actúan directo sobre la ventana con Win32,
/// porque el bucle de la interfaz no corre mientras la ventana está oculta.
#[cfg(windows)]
pub mod tray {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tray_icon::menu::{Menu, MenuEvent, MenuItem};
    use tray_icon::{Icon, MouseButton, TrayIcon, TrayIconBuilder, TrayIconEvent};

    fn icon() -> Icon {
        // Disco azul sobre fondo transparente, generado en código.
        const N: u32 = 32;
        let mut rgba = Vec::with_capacity((N * N * 4) as usize);
        for y in 0..N {
            for x in 0..N {
                let (dx, dy) = (x as f32 - 15.5, y as f32 - 15.5);
                let d = (dx * dx + dy * dy).sqrt();
                let px: [u8; 4] = if d < 4.0 {
                    [255, 255, 255, 255]
                } else if d < 15.0 {
                    [30, 110, 200, 255]
                } else {
                    [0, 0, 0, 0]
                };
                rgba.extend_from_slice(&px);
            }
        }
        Icon::from_rgba(rgba, N, N).expect("icono válido")
    }

    /// Crea el icono. `quit` se activa al elegir "Salir"; `wake` despierta la interfaz.
    pub fn create(hwnd: isize, quit: Arc<AtomicBool>, wake: impl Fn() + Send + Sync + 'static) -> Option<TrayIcon> {
        let menu = Menu::new();
        let open = MenuItem::new("Abrir", true, None);
        let exit = MenuItem::new("Salir", true, None);
        menu.append(&open).ok()?;
        menu.append(&exit).ok()?;
        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip(super::WINDOW_TITLE)
            .with_icon(icon())
            .build()
            .ok()?;

        let wake = Arc::new(wake);
        let (open_id, exit_id) = (open.id().clone(), exit.id().clone());
        let wake_menu = wake.clone();
        MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
            if e.id == exit_id {
                quit.store(true, Ordering::Relaxed);
            } else if e.id != open_id {
                return;
            }
            super::show_window(hwnd);
            wake_menu();
        }));
        TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| {
            if let TrayIconEvent::DoubleClick {
                button: MouseButton::Left,
                ..
            } = e
            {
                super::show_window(hwnd);
                wake();
            }
        }));
        Some(tray)
    }
}
