//! Every OS-specific call lives here. Non-Windows stubs keep the workspace building and testable elsewhere.

/// Held for the life of the process; the OS releases the mutex on exit.
pub struct InstanceGuard(#[allow(dead_code)] isize);

/// `None` when another instance already holds `name`.
#[cfg(windows)]
pub fn single_instance(name: &str) -> Option<InstanceGuard> {
    use windows_sys::Win32::{
        Foundation::{ERROR_ALREADY_EXISTS, GetLastError},
        System::Threading::CreateMutexW,
    };
    let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
    let handle = unsafe { CreateMutexW(std::ptr::null(), 0, wide.as_ptr()) };
    if handle.is_null() || unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        return None;
    }
    Some(InstanceGuard(handle as isize))
}

#[cfg(not(windows))]
pub fn single_instance(_name: &str) -> Option<InstanceGuard> {
    Some(InstanceGuard(0))
}

/// Shows and focuses the top-level window titled `title`, hidden or not.
#[cfg(windows)]
pub fn focus_window(title: &str) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        FindWindowW, SW_RESTORE, SetForegroundWindow, ShowWindow,
    };
    let wide: Vec<u16> = title.encode_utf16().chain(Some(0)).collect();
    unsafe {
        let hwnd = FindWindowW(std::ptr::null(), wide.as_ptr());
        if !hwnd.is_null() {
            ShowWindow(hwnd, SW_RESTORE);
            SetForegroundWindow(hwnd);
        }
    }
}

#[cfg(not(windows))]
pub fn focus_window(_title: &str) {}

/// Dispatches pending window messages for the calling thread (the tray icon's thread).
#[cfg(windows)]
pub fn pump_messages() {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, MSG, PM_REMOVE, PeekMessageW, TranslateMessage,
    };
    unsafe {
        let mut msg: MSG = std::mem::zeroed();
        while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

#[cfg(not(windows))]
pub fn pump_messages() {}

/// Binds a non-blocking UDP socket. On Windows the bind is exclusive, so a GCS starting later gets "port in use"
/// instead of silently sharing the port.
pub fn bind_udp_exclusive(addr: std::net::SocketAddr) -> std::io::Result<std::net::UdpSocket> {
    let sock = socket2::Socket::new(
        socket2::Domain::for_address(addr),
        socket2::Type::DGRAM,
        None,
    )?;
    #[cfg(windows)]
    exclusive(&sock)?;
    sock.set_nonblocking(true)?;
    sock.bind(&addr.into())?;
    Ok(sock.into())
}

#[cfg(windows)]
fn exclusive(sock: &socket2::Socket) -> std::io::Result<()> {
    use std::os::windows::io::AsRawSocket;
    use windows_sys::Win32::Networking::WinSock::{SO_EXCLUSIVEADDRUSE, SOL_SOCKET, setsockopt};
    let on: i32 = 1;
    let rc = unsafe {
        setsockopt(
            sock.as_raw_socket() as _,
            SOL_SOCKET,
            SO_EXCLUSIVEADDRUSE,
            &on as *const i32 as *const u8,
            4,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Which app this process is: its data folder, autostart entry and toast identity.
pub struct App {
    /// Folder under %APPDATA%\Pinged and the autostart entry's name.
    pub name: &'static str,
    /// AppUserModelID; the installer's Start-menu shortcut registers it so toasts carry the app's name.
    pub id: &'static str,
}

static APP: std::sync::OnceLock<App> = std::sync::OnceLock::new();

/// Call first thing in `main`, before logging starts: the data folders, autostart and toasts need it.
pub fn set_app(app: App) {
    let _ = APP.set(app);
}

fn app() -> &'static App {
    APP.get()
        .expect("call charon::platform::set_app first thing in main")
}

fn dirs() -> directories::ProjectDirs {
    directories::ProjectDirs::from("", "Pinged", app().name).expect("user home directory")
}

/// `%APPDATA%\Pinged\<app name>\config\config.toml` on Windows.
pub fn config_path() -> std::path::PathBuf {
    dirs().config_dir().join("config.toml")
}

pub fn log_dir() -> std::path::PathBuf {
    dirs().data_dir().join("logs")
}

pub fn tlog_dir() -> std::path::PathBuf {
    dirs().data_dir().join("tlogs")
}

/// HKCU Run key on Windows. Login launches pass `--hidden`. Only with the `autostart` feature.
#[cfg(feature = "autostart")]
pub fn set_autostart(enabled: bool) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let launcher = auto_launch::AutoLaunchBuilder::new()
        .set_app_name(app().name)
        .set_app_path(&exe.to_string_lossy())
        .set_args(&["--hidden"])
        .build()
        .map_err(|e| e.to_string())?;
    if launcher.is_enabled().unwrap_or(false) == enabled {
        return Ok(());
    }
    if enabled {
        launcher.enable()
    } else {
        launcher.disable()
    }
    .map_err(|e| e.to_string())
}

/// Opens a folder in the file manager.
pub fn open_folder(dir: &std::path::Path) {
    let opener = if cfg!(windows) { "explorer" } else { "open" };
    if let Err(e) = std::process::Command::new(opener).arg(dir).spawn() {
        tracing::warn!("can't open {}: {e}", dir.display());
    }
}

/// Windows toast. Before install (no registered shortcut) Windows may drop it silently, so the text is logged too.
pub fn notify(title: &str, body: &str) {
    tracing::warn!("{title}: {body}");
    let mut n = notify_rust::Notification::new();
    n.summary(title).body(body);
    #[cfg(windows)]
    n.app_id(app().id);
    if let Err(e) = n.show() {
        tracing::warn!("notification failed: {e}");
    }
}
