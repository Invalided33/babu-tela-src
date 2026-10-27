//! Windows pieces: the login and small secrets encrypted with the user's key
//! (DPAPI), "Iniciar com o Windows", the notification registration, the
//! single running copy, and bringing the window to the front.

use std::io;
use std::path::Path;

use babutela_buildinfo::NAME;
use windows_registry::CURRENT_USER;
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HWND, LocalFree, WAIT_OBJECT_0,
};
use windows_sys::Win32::Security::Cryptography::{
    CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
};
use windows_sys::Win32::System::Threading::{
    CreateEventW, CreateMutexW, EVENT_MODIFY_STATE, INFINITE, OpenEventW, OpenMutexW, OpenProcess,
    SYNCHRONIZATION_SYNCHRONIZE, SetEvent, WaitForMultipleObjects, WaitForSingleObject,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, GetForegroundWindow, HWND_NOTOPMOST, HWND_TOPMOST, IDYES, IsIconic,
    IsWindowVisible, IsZoomed, MB_ICONERROR, MB_ICONWARNING, MB_YESNO, MessageBoxW, SW_RESTORE,
    SW_SHOW, SWP_NOMOVE, SWP_NOSIZE, SetForegroundWindow, SetWindowPos, ShowWindow,
};

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

// ---------------------------------------------------------------- DPAPI

/// Encrypts `data` so only the same Windows user on the same machine can
/// read it back.
pub fn protect(data: &[u8]) -> io::Result<Vec<u8>> {
    dpapi(data, true)
}

pub fn unprotect(data: &[u8]) -> io::Result<Vec<u8>> {
    if data.is_empty() {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    dpapi(data, false)
}

fn dpapi(data: &[u8], encrypt: bool) -> io::Result<Vec<u8>> {
    let input = CRYPT_INTEGER_BLOB {
        cbData: data.len() as u32,
        pbData: data.as_ptr().cast_mut(),
    };
    let mut out = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    use std::ptr::{null, null_mut};
    // SAFETY: `input` points at `data`, which outlives the call; `out` is
    // filled by the system and freed below with LocalFree.
    let ok = unsafe {
        if encrypt {
            CryptProtectData(
                &input,
                null(),
                null(),
                null(),
                null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
        } else {
            CryptUnprotectData(
                &input,
                null_mut(),
                null(),
                null(),
                null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
        }
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: on success `out` holds `cbData` bytes allocated with LocalAlloc.
    unsafe {
        let bytes = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        LocalFree(out.pbData.cast());
        Ok(bytes)
    }
}

// ---------------------------------------------------------------- start with Windows

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

/// Adds or removes the app from the programs Windows starts at sign-in (per
/// user, no admin rights needed).
pub fn set_login_item(enabled: bool, minimized: bool) -> Result<(), String> {
    set_run_value(NAME, enabled, minimized)
}

fn set_run_value(name: &str, enabled: bool, minimized: bool) -> Result<(), String> {
    let key = CURRENT_USER.create(RUN_KEY).map_err(|e| e.to_string())?;
    if !enabled {
        if key.get_type(name).is_err() {
            return Ok(()); // not there
        }
        return key.remove_value(name).map_err(|e| e.to_string());
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut cmd = format!("\"{}\"", exe.display());
    if minimized {
        cmd.push_str(" --start-minimized");
    }
    key.set_string(name, cmd).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------- notifications

/// Registers the AppUserModelID notifications are sent under, so they show
/// the app's name and icon without a Start menu shortcut. It is the .exe's
/// name, as in the earlier versions, so Windows keeps the user's
/// notification settings for the app.
///
/// With `activator` (a COM class id, in braces) Windows can also start this
/// .exe when one of the notifications is clicked after the app was closed:
/// the class's server is this .exe with `notify::LAUNCH_ARG`, and the
/// notification identity names the class as its activator.
pub fn register_notifications(
    aumid: &str,
    icon: &Path,
    activator: Option<&str>,
) -> Result<(), String> {
    let key = CURRENT_USER
        .create(format!(r"Software\Classes\AppUserModelId\{aumid}"))
        .map_err(|e| e.to_string())?;
    key.set_string("DisplayName", NAME)
        .map_err(|e| e.to_string())?;
    key.set_string("IconUri", icon.display().to_string())
        .map_err(|e| e.to_string())?;
    if let Some(clsid) = activator {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        // The Wails version registered an activator class of its own, with
        // this same .exe as its server. Replaced here, it goes, or it would
        // outlive even the uninstaller.
        if let Ok(old) = key.get_string("CustomActivator")
            && !old.eq_ignore_ascii_case(clsid)
        {
            let old_class = format!(r"Software\Classes\CLSID\{old}");
            let serves_this_exe = CURRENT_USER
                .open(format!(r"{old_class}\LocalServer32"))
                .and_then(|k| k.get_string(""))
                .is_ok_and(|cmd| {
                    cmd.to_lowercase()
                        .contains(&exe.display().to_string().to_lowercase())
                });
            if serves_this_exe {
                let _ = CURRENT_USER.remove_tree(&old_class);
            }
        }
        CURRENT_USER
            .create(format!(r"Software\Classes\CLSID\{clsid}\LocalServer32"))
            .and_then(|server| {
                server.set_string(
                    "",
                    format!("\"{}\" {}", exe.display(), crate::notify::LAUNCH_ARG),
                )
            })
            .map_err(|e| e.to_string())?;
        key.set_string("CustomActivator", clsid)
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

// ---------------------------------------------------------------- COM

/// COM on this thread, in the multithreaded apartment, for as long as the
/// guard lives. A thread that already has COM in the other model (which
/// Windows reports as RPC_E_CHANGED_MODE: the audio devices set their
/// threads up for COM their way, and so do some drivers and programs that
/// load into every process) keeps it: what the app does with COM works in
/// either, and the guard then undoes nothing, so that model stays the
/// thread's own.
pub struct Com {
    started: bool,
}

impl Com {
    pub fn multithreaded() -> Result<Com, String> {
        use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
        use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
        // SAFETY: undone in Drop, on this same thread (the guard is not
        // Send), only when it started COM here.
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if hr == RPC_E_CHANGED_MODE {
            return Ok(Com { started: false });
        }
        hr.ok().map_err(|e| e.message())?;
        Ok(Com { started: true })
    }
}

impl Drop for Com {
    fn drop(&mut self) {
        if self.started {
            // SAFETY: matches the CoInitializeEx in `multithreaded`.
            unsafe { windows::Win32::System::Com::CoUninitialize() };
        }
    }
}

// ---------------------------------------------------------------- messages

/// Asks yes or no before the window exists. True for yes.
pub fn ask(text: &str) -> bool {
    let title = wide(NAME);
    let body = wide(text);
    // SAFETY: both strings are NUL-terminated and outlive the call.
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            body.as_ptr(),
            title.as_ptr(),
            MB_ICONWARNING | MB_YESNO,
        ) == IDYES
    }
}

/// Waits (up to `max`) for process `pid` to end: the copy of the app that
/// opened this one to take its place.
pub fn wait_for_exit(pid: u32, max: std::time::Duration) {
    // SAFETY: a handle to a process, only waited on and closed here.
    unsafe {
        let process = OpenProcess(SYNCHRONIZATION_SYNCHRONIZE, 0, pid);
        if process.is_null() {
            // Gone already.
            return;
        }
        WaitForSingleObject(process, u32::try_from(max.as_millis()).unwrap_or(u32::MAX));
        CloseHandle(process);
    }
}

/// Shows a message box. Used when the window itself could not open, which
/// is the one moment the app has nowhere else to say anything.
pub fn alert(text: &str) {
    let title = wide(NAME);
    let body = wide(text);
    // SAFETY: both strings are NUL-terminated and outlive the call.
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            body.as_ptr(),
            title.as_ptr(),
            MB_ICONERROR,
        );
    }
}

// ---------------------------------------------------------------- one copy at a time

/// The lock that keeps a second copy of the app from starting. A second
/// copy signals the first to show its window and then quits; the installer
/// signals it to close (`Local\<id>.quit`) before replacing its file.
pub struct Instance {
    _lock: OwnedHandle,
    event: OwnedHandle,
    quit: OwnedHandle,
}

struct OwnedHandle(HANDLE);

// SAFETY: kernel object handles can be used from any thread.
unsafe impl Send for OwnedHandle {}
unsafe impl Sync for OwnedHandle {}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: the handle came from a Create/Open call and is closed once.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

pub enum Start {
    /// This is the only copy.
    First(Instance),
    /// Another copy is running and was asked to come forward.
    Second,
}

pub fn single_instance(id: &str) -> Start {
    let lock_name = wide(&format!("Local\\{id}.lock"));
    let event_name = wide(&format!("Local\\{id}.show"));
    let quit_name = wide(&format!("Local\\{id}.quit"));
    // SAFETY: the names are NUL-terminated; the handles are owned below.
    unsafe {
        let lock = CreateMutexW(std::ptr::null(), 0, lock_name.as_ptr());
        let already = GetLastError() == ERROR_ALREADY_EXISTS;
        if !lock.is_null() && already {
            let event = OpenEventW(EVENT_MODIFY_STATE, 0, event_name.as_ptr());
            if !event.is_null() {
                SetEvent(event);
                CloseHandle(event);
            }
            CloseHandle(lock);
            return Start::Second;
        }
        // Without the lock (null) there is no way to tell; carry on alone.
        let event = CreateEventW(std::ptr::null(), 0, 0, event_name.as_ptr());
        let quit = CreateEventW(std::ptr::null(), 0, 0, quit_name.as_ptr());
        Start::First(Instance {
            _lock: OwnedHandle(lock),
            event: OwnedHandle(event),
            quit: OwnedHandle(quit),
        })
    }
}

/// Whether the installer is changing the app's files right now
/// (`babutela_buildinfo::setup_lock_name`), as during an update, which opens
/// the app again once it is done.
pub fn installer_running() -> bool {
    let name = wide(&babutela_buildinfo::setup_lock_name());
    // SAFETY: opens and closes a handle to a named kernel object; the name
    // is NUL-terminated.
    unsafe {
        let lock = OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, 0, name.as_ptr());
        if lock.is_null() {
            return false;
        }
        CloseHandle(lock);
        true
    }
}

/// Lets a process this one started take the foreground: the installer of
/// an update, whose window the person is waiting for.
pub fn allow_foreground(pid: u32) {
    // SAFETY: a plain call with a process id.
    unsafe {
        AllowSetForegroundWindow(pid);
    }
}

impl Instance {
    /// Calls `show` every time another copy of the app is started, and
    /// `quit` when the installer asks the app to close.
    pub fn listen(self, show: impl Fn() + Send + 'static, quit: impl Fn() + Send + 'static) {
        std::thread::Builder::new()
            .name("single-instance".into())
            .spawn(move || {
                let me = self;
                if me.event.0.is_null() || me.quit.0.is_null() {
                    return;
                }
                let handles = [me.event.0, me.quit.0];
                loop {
                    // SAFETY: both handles stay open while `me` lives.
                    let r = unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, INFINITE) };
                    match r {
                        WAIT_OBJECT_0 => show(),
                        r if r == WAIT_OBJECT_0 + 1 => quit(),
                        _ => return,
                    }
                }
            })
            .ok();
    }
}

// ---------------------------------------------------------------- the window

/// Whether the window is on screen and the one being used: shown, not
/// minimized, and in the foreground.
pub fn is_foreground(hwnd: isize) -> bool {
    let hwnd = hwnd as HWND;
    if hwnd.is_null() {
        return false;
    }
    // SAFETY: read-only queries on a window handle; a stale one just
    // answers no.
    unsafe { IsWindowVisible(hwnd) != 0 && IsIconic(hwnd) == 0 && GetForegroundWindow() == hwnd }
}

/// Whether the window is shown at all (not hidden in the tray).
pub fn is_visible(hwnd: isize) -> bool {
    let hwnd = hwnd as HWND;
    // SAFETY: a read-only query on a window handle.
    !hwnd.is_null() && unsafe { IsWindowVisible(hwnd) != 0 }
}

/// Whether the window fills the screen (maximized), for the tests.
pub fn is_maximized(hwnd: isize) -> bool {
    let hwnd = hwnd as HWND;
    // SAFETY: a read-only query on a window handle.
    !hwnd.is_null() && unsafe { IsZoomed(hwnd) != 0 }
}

/// Brings the window back and in front of the others. Windows only lets an
/// app take the foreground in some situations, so it is briefly made
/// topmost first, which is what the earlier versions did too.
pub fn bring_to_front(hwnd: isize) {
    let hwnd = hwnd as HWND;
    if hwnd.is_null() {
        return;
    }
    // SAFETY: plain window calls on a handle the app owns; a stale handle
    // makes them fail harmlessly.
    unsafe {
        if IsIconic(hwnd) != 0 {
            ShowWindow(hwnd, SW_RESTORE);
        } else {
            ShowWindow(hwnd, SW_SHOW);
        }
        let flags = SWP_NOMOVE | SWP_NOSIZE;
        SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0, flags);
        SetWindowPos(hwnd, HWND_NOTOPMOST, 0, 0, 0, 0, flags);
        SetForegroundWindow(hwnd);
    }
}

/// The refresh rate of the screen the window is on (Hz), or None when
/// Windows does not say.
pub fn refresh_rate(hwnd: Option<isize>) -> Option<u32> {
    use windows_sys::Win32::Graphics::Gdi::{
        DEVMODEW, ENUM_CURRENT_SETTINGS, EnumDisplaySettingsW, GetMonitorInfoW,
        MONITOR_DEFAULTTOPRIMARY, MONITORINFOEXW, MonitorFromWindow,
    };
    // SAFETY: plain structs of integers, for which all zeros is valid;
    // their size fields are set before Windows writes into them, and the
    // device name Windows fills in is null-terminated.
    unsafe {
        let mut info: MONITORINFOEXW = std::mem::zeroed();
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        let monitor = MonitorFromWindow(hwnd.unwrap_or(0) as _, MONITOR_DEFAULTTOPRIMARY);
        let device = if GetMonitorInfoW(monitor, std::ptr::from_mut(&mut info).cast()) != 0 {
            info.szDevice.as_ptr()
        } else {
            std::ptr::null()
        };
        let mut mode: DEVMODEW = std::mem::zeroed();
        mode.dmSize = std::mem::size_of::<DEVMODEW>() as u16;
        if EnumDisplaySettingsW(device, ENUM_CURRENT_SETTINGS, &mut mode) == 0 {
            return None;
        }
        // 0 and 1 mean "the hardware's default".
        (mode.dmDisplayFrequency > 1).then_some(mode.dmDisplayFrequency)
    }
}

// ---------------------------------------------------------------- memory

/// Where this process's memory is, in MB, for `cargo xtask medir`: what the
/// heaps have handed out (the app's own allocations and those of every
/// library sharing a heap) and hold from Windows, and all the private
/// memory, the heaps included. What is private but not in a heap is mostly
/// the GPU driver's and the thread stacks; `gpu` is the part the driver
/// mapped write-combined (buffers it uploads from).
pub fn memory_parts() -> serde_json::Value {
    use serde_json::json;
    use windows_sys::Win32::System::Memory::{
        GetProcessHeap, GetProcessHeaps, HeapLock, HeapUnlock, HeapWalk, MEM_COMMIT, MEM_IMAGE,
        MEM_MAPPED, MEM_PRIVATE, MEMORY_BASIC_INFORMATION, PAGE_NOCACHE, PAGE_WRITECOMBINE,
        PROCESS_HEAP_ENTRY, VirtualQuery,
    };
    // PROCESS_HEAP_REGION and PROCESS_HEAP_ENTRY_BUSY (winbase.h).
    const REGION: u16 = 0x0001;
    const BUSY: u16 = 0x0004;
    let mb = |bytes: u64| (bytes as f64 / 1048576.0 * 10.0).round() / 10.0;
    let mut heaps = Vec::new();
    let (mut used_all, mut held_all) = (0u64, 0u64);
    let (mut private, mut gpu, mut image, mut mapped) = (0u64, 0u64, 0u64, 0u64);
    // SAFETY: the heap handles come from Windows and each walk runs with
    // its heap locked; the structs are plain data, valid when zeroed, and
    // Windows fills them. The address-space walk only asks about addresses.
    unsafe {
        let mut handles = [std::ptr::null_mut(); 256];
        let n = GetProcessHeaps(handles.len() as u32, handles.as_mut_ptr()) as usize;
        let main = GetProcessHeap();
        for &heap in &handles[..n.min(handles.len())] {
            if HeapLock(heap) == 0 {
                continue;
            }
            let mut entry: PROCESS_HEAP_ENTRY = std::mem::zeroed();
            let (mut used, mut held) = (0u64, 0u64);
            while HeapWalk(heap, &mut entry) != 0 {
                if entry.wFlags & BUSY != 0 {
                    used += u64::from(entry.cbData);
                }
                if entry.wFlags & REGION != 0 {
                    held += u64::from(entry.Anonymous.Region.dwCommittedSize);
                }
            }
            HeapUnlock(heap);
            used_all += used;
            held_all += held;
            if heap == main || used.max(held) > 4 << 20 {
                heaps.push(
                    json!({ "principal": heap == main, "usado": mb(used), "retido": mb(held) }),
                );
            }
        }
        let mut at = 0usize;
        let mut info: MEMORY_BASIC_INFORMATION = std::mem::zeroed();
        while VirtualQuery(
            at as *const _,
            &mut info,
            size_of::<MEMORY_BASIC_INFORMATION>(),
        ) != 0
        {
            if info.State == MEM_COMMIT {
                let size = info.RegionSize as u64;
                match info.Type {
                    MEM_PRIVATE => {
                        private += size;
                        if info.Protect & (PAGE_WRITECOMBINE | PAGE_NOCACHE) != 0 {
                            gpu += size;
                        }
                    }
                    MEM_IMAGE => image += size,
                    MEM_MAPPED => mapped += size,
                    _ => {}
                }
            }
            let next = (info.BaseAddress as usize).saturating_add(info.RegionSize);
            if next <= at {
                break;
            }
            at = next;
        }
    }
    json!({
        "heaps_usado": mb(used_all),
        "heaps_retido": mb(held_all),
        "privada": mb(private),
        "gpu": mb(gpu),
        "imagens": mb(image),
        "mapeada": mb(mapped),
        "heaps": heaps,
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn dpapi_round_trip() {
        let enc = super::protect(b"token secreto").unwrap();
        assert_ne!(enc, b"token secreto");
        assert_eq!(super::unprotect(&enc).unwrap(), b"token secreto");
        assert!(super::unprotect(b"lixo").is_err());
        assert!(super::unprotect(b"").is_err());
    }

    /// Under a throwaway name, so the real "Babu Tela" entry is never touched.
    #[test]
    fn login_item_add_and_remove() {
        const TEST_NAME: &str = "Babu Tela (teste automatizado)";
        let read = || {
            super::CURRENT_USER
                .open(super::RUN_KEY)
                .ok()
                .and_then(|k| k.get_string(TEST_NAME).ok())
        };
        super::set_run_value(TEST_NAME, true, true).unwrap();
        let exe = std::env::current_exe().unwrap();
        assert_eq!(
            read(),
            Some(format!("\"{}\" --start-minimized", exe.display()))
        );
        super::set_run_value(TEST_NAME, true, false).unwrap();
        assert_eq!(read(), Some(format!("\"{}\"", exe.display())));
        super::set_run_value(TEST_NAME, false, false).unwrap();
        assert_eq!(read(), None);
        super::set_run_value(TEST_NAME, false, false).unwrap(); // already gone
    }

    /// Under a throwaway identity and class, so the installed app's
    /// registration is never touched.
    #[test]
    fn notifications_name_their_activator() {
        let aumid = format!("Babu Tela (teste automatizado {}).exe", std::process::id());
        let clsid = "{0B0B0B0B-1234-4567-89AB-0123456789AB}";
        let classes = r"Software\Classes";
        let identity = format!(r"{classes}\AppUserModelId\{aumid}");
        let class = format!(r"{classes}\CLSID\{clsid}");
        let icon = std::env::temp_dir().join("icone-teste.png");
        let custom = || {
            super::CURRENT_USER
                .open(&identity)
                .ok()
                .and_then(|k| k.get_string("CustomActivator").ok())
        };
        // A test profile: name and icon only, no activator.
        super::register_notifications(&aumid, &icon, None).unwrap();
        // Writable: the test plants an older activator below.
        let key = super::CURRENT_USER.create(&identity).unwrap();
        assert_eq!(key.get_string("DisplayName").unwrap(), super::NAME);
        assert_eq!(
            key.get_string("IconUri").unwrap(),
            icon.display().to_string()
        );
        assert_eq!(custom(), None);
        super::register_notifications(&aumid, &icon, Some(clsid)).unwrap();
        assert_eq!(custom().as_deref(), Some(clsid));
        let server = super::CURRENT_USER
            .open(format!(r"{class}\LocalServer32"))
            .and_then(|k| k.get_string(""))
            .unwrap();
        let exe = std::env::current_exe().unwrap();
        assert_eq!(server, format!("\"{}\" -ToastActivated", exe.display()));

        // An earlier activator class served by this same .exe (what the
        // Wails version left) goes when a new one takes its place; one
        // served by another program stays.
        let old = "{0C0C0C0C-1234-4567-89AB-0123456789AB}";
        let old_class = format!(r"{classes}\CLSID\{old}");
        let other = "{0D0D0D0D-1234-4567-89AB-0123456789AB}";
        let other_class = format!(r"{classes}\CLSID\{other}");
        let make = |class: &str, cmd: String| {
            super::CURRENT_USER
                .create(format!(r"{class}\LocalServer32"))
                .and_then(|k| k.set_string("", cmd))
                .unwrap();
        };
        make(&old_class, format!("\"{}\" %1", exe.display()));
        make(&other_class, r#""C:\Outro\outro.exe" %1"#.to_owned());
        key.set_string("CustomActivator", old).unwrap();
        super::register_notifications(&aumid, &icon, Some(clsid)).unwrap();
        assert!(super::CURRENT_USER.open(&old_class).is_err());
        key.set_string("CustomActivator", other).unwrap();
        super::register_notifications(&aumid, &icon, Some(clsid)).unwrap();
        assert!(super::CURRENT_USER.open(&other_class).is_ok());
        assert_eq!(custom().as_deref(), Some(clsid));

        for k in [&identity, &class, &old_class, &other_class] {
            let _ = super::CURRENT_USER.remove_tree(k);
        }
        assert!(super::CURRENT_USER.open(&class).is_err());
        assert!(super::CURRENT_USER.open(&identity).is_err());
    }

    #[test]
    fn a_second_copy_signals_the_first() {
        let id = format!("com.prosa.app.teste.{}", std::process::id());
        let super::Start::First(first) = super::single_instance(&id) else {
            panic!("a primeira cópia devia ficar com a trava");
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let tx2 = tx.clone();
        first.listen(
            move || {
                let _ = tx.send("mostrar");
            },
            move || {
                let _ = tx2.send("sair");
            },
        );
        assert!(matches!(super::single_instance(&id), super::Start::Second));
        let wait = std::time::Duration::from_secs(5);
        assert_eq!(rx.recv_timeout(wait), Ok("mostrar"));
        // What the installer does before replacing the app's file.
        let name = super::wide(&format!("Local\\{id}.quit"));
        // SAFETY: opens, sets and closes a named event.
        unsafe {
            let quit = super::OpenEventW(super::EVENT_MODIFY_STATE, 0, name.as_ptr());
            assert!(!quit.is_null(), "o evento de sair não existe");
            super::SetEvent(quit);
            super::CloseHandle(quit);
        }
        assert_eq!(rx.recv_timeout(wait), Ok("sair"));
    }
}
