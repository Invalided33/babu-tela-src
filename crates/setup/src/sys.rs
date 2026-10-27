//! The Windows side of installing: folders, shortcuts, the registry, the
//! copy of the app that may be running, and files that are in use.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use babutela_setup::{DIR, EXE, PRODUCT, UNINSTALLER, instance_id, keys, test_root};
use windows::Win32::Foundation::{CloseHandle, HWND};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
    CoTaskMemFree, CoUninitialize, IPersistFile,
};
use windows::Win32::System::Threading::{
    EVENT_MODIFY_STATE, OpenEventW, OpenMutexW, SYNCHRONIZATION_SYNCHRONIZE, SetEvent,
};
use windows::Win32::UI::Shell::{
    FOLDERID_Desktop, FOLDERID_LocalAppData, FOLDERID_Programs, FOLDERID_RoamingAppData,
    FOS_FORCEFILESYSTEM, FOS_PATHMUSTEXIST, FOS_PICKFOLDERS, FileOpenDialog, IFileOpenDialog,
    IShellItem, IShellLinkW, KF_FLAG_DEFAULT, SHCreateItemFromParsingName, SHGetKnownFolderPath,
    SIGDN_FILESYSPATH, ShellLink,
};
use windows::core::{GUID, HSTRING, Interface, w};
use windows_registry::CURRENT_USER;

/// COM, for the shortcuts and the folder picker, on the current thread.
pub struct Com;

impl Com {
    pub fn init() -> Com {
        // SAFETY: paired with CoUninitialize in Drop, on the same thread.
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        }
        Com
    }
}

impl Drop for Com {
    fn drop(&mut self) {
        // SAFETY: matches the CoInitializeEx in `init`.
        unsafe { CoUninitialize() };
    }
}

fn known_folder(id: &GUID) -> Option<PathBuf> {
    // SAFETY: the returned string is the caller's to free, done right here.
    unsafe {
        let p = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None).ok()?;
        let path = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as *const _));
        path.map(PathBuf::from)
    }
}

/// A user folder, or its stand-in under the test folder.
fn folder(id: &GUID, env: &str, test_name: &str) -> PathBuf {
    if let Some(root) = test_root() {
        return root.join(test_name);
    }
    known_folder(id)
        .or_else(|| std::env::var_os(env).map(PathBuf::from))
        .unwrap_or_default()
}

/// %LOCALAPPDATA%\Programs\BabuTela.
pub fn default_dir() -> PathBuf {
    folder(&FOLDERID_LocalAppData, "LOCALAPPDATA", "Local").join(DIR)
}

/// %APPDATA%, where the app keeps its profile ("Prosa").
pub fn roaming() -> PathBuf {
    folder(&FOLDERID_RoamingAppData, "APPDATA", "Roaming")
}

/// %LOCALAPPDATA%, where the app keeps the updates it downloaded
/// ("Prosa\updates").
pub fn local() -> PathBuf {
    folder(&FOLDERID_LocalAppData, "LOCALAPPDATA", "Local")
}

pub fn start_menu_link() -> PathBuf {
    folder(&FOLDERID_Programs, "APPDATA", "Iniciar").join(format!("{PRODUCT}.lnk"))
}

pub fn desktop_link() -> PathBuf {
    folder(&FOLDERID_Desktop, "USERPROFILE", "Desktop").join(format!("{PRODUCT}.lnk"))
}

/// A shortcut to `target`, with its icon.
pub fn shortcut(link: &Path, target: &Path) -> windows::core::Result<()> {
    if let Some(dir) = link.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    // SAFETY: plain COM calls on objects this function owns.
    unsafe {
        let shell: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
        shell.SetPath(&HSTRING::from(target))?;
        if let Some(dir) = target.parent() {
            shell.SetWorkingDirectory(&HSTRING::from(dir))?;
        }
        shell.SetIconLocation(&HSTRING::from(target), 0)?;
        shell.SetDescription(&HSTRING::from("Chat de comunidades com texto, voz e vídeo"))?;
        let file: IPersistFile = shell.cast()?;
        file.Save(&HSTRING::from(link), true)
    }
}

/// Asks where to install. Starts from the folder the path is in (or the
/// nearest one that exists).
pub fn pick_folder(owner: HWND, start: &Path) -> Option<PathBuf> {
    // SAFETY: plain COM calls on objects this function owns; the returned
    // string is freed here.
    unsafe {
        let dialog: IFileOpenDialog =
            CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        let options = dialog.GetOptions().ok()?;
        dialog
            .SetOptions(options | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM | FOS_PATHMUSTEXIST)
            .ok()?;
        let _ = dialog.SetTitle(w!("Onde instalar o Babu Tela"));
        if let Some(existing) = start.ancestors().find(|p| p.is_dir())
            && let Ok(item) =
                SHCreateItemFromParsingName::<_, _, IShellItem>(&HSTRING::from(existing), None)
        {
            let _ = dialog.SetFolder(&item);
        }
        dialog.Show(Some(owner)).ok()?;
        let item = dialog.GetResult().ok()?;
        let name = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let path = name.to_string().ok();
        CoTaskMemFree(Some(name.0 as *const _));
        path.map(PathBuf::from)
    }
}

// ---------------------------------------------------------------- the running app

fn object_name(suffix: &str) -> HSTRING {
    HSTRING::from(format!("Local\\{}.{suffix}", instance_id()))
}

/// Whether the app is open (it holds its single-instance lock while it is).
pub fn app_running() -> bool {
    // SAFETY: opens and closes a handle to a named kernel object.
    unsafe {
        match OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, false, &object_name("lock")) {
            Ok(h) => {
                let _ = CloseHandle(h);
                true
            }
            Err(_) => false,
        }
    }
}

/// Held while the installer changes the app's files (`install`): an app
/// started meanwhile sees it and leaves it alone (crates/app/src/main.rs),
/// and an update opens the app again once it is done.
pub struct SetupLock(windows_sys::Win32::Foundation::HANDLE);

impl SetupLock {
    pub fn take() -> SetupLock {
        let name = HSTRING::from(babutela_buildinfo::setup_lock_name());
        // SAFETY: the name is NUL-terminated and outlives the call; the
        // handle is closed in Drop.
        SetupLock(unsafe {
            windows_sys::Win32::System::Threading::CreateMutexW(std::ptr::null(), 0, name.as_ptr())
        })
    }
}

impl Drop for SetupLock {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: the handle came from CreateMutexW and is closed once.
            unsafe { windows_sys::Win32::Foundation::CloseHandle(self.0) };
        }
    }
}

/// Waits for a process to exit: the app that started an update quits right
/// after. Whether it did within `wait` (or was gone already).
pub fn wait_for_exit(pid: u32, wait: Duration) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
    };
    let ms = u32::try_from(wait.as_millis()).unwrap_or(u32::MAX);
    // SAFETY: a handle opened only to wait on, and closed right after.
    unsafe {
        let process = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
        if process.is_null() {
            return true;
        }
        let r = WaitForSingleObject(process, ms);
        CloseHandle(process);
        r == WAIT_OBJECT_0
    }
}

/// Lets a process this one started bring its window to the front (the app
/// opened again after an update, which the person is waiting for).
pub fn allow_foreground(pid: u32) {
    // SAFETY: a plain call with a process id.
    unsafe {
        windows_sys::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow(pid);
    }
}

/// Asks the open app to close the way "Sair" does, and waits for it.
/// Versions older than this installer cannot be asked; for those this just
/// waits.
pub fn close_app(wait: Duration) -> bool {
    if !app_running() {
        return true;
    }
    // SAFETY: opens, sets and closes a named event.
    unsafe {
        if let Ok(h) = OpenEventW(EVENT_MODIFY_STATE, false, &object_name("quit")) {
            let _ = SetEvent(h);
            let _ = CloseHandle(h);
        }
    }
    let until = Instant::now() + wait;
    while Instant::now() < until {
        if !app_running() {
            // A moment for Windows to let go of the file.
            std::thread::sleep(Duration::from_millis(300));
            return true;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    !app_running()
}

// ---------------------------------------------------------------- files

/// Moves `from` over `to`. A copy of the app that is still running keeps
/// its file open, but Windows lets it be renamed out of the way, which is
/// what happens then; the renamed file goes on the next install.
pub fn replace(from: &Path, to: &Path) -> io::Result<()> {
    match std::fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(first) if to.exists() => {
            let aside = to.with_file_name(format!(
                "{}.old-{}",
                to.file_name().unwrap_or_default().to_string_lossy(),
                std::process::id()
            ));
            if std::fs::rename(to, &aside).is_err() {
                return Err(first);
            }
            std::fs::rename(from, to)
        }
        Err(e) => Err(e),
    }
}

/// Leftovers of earlier installs: files renamed aside while in use, and a
/// half-written copy from one that was interrupted.
pub fn sweep(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_lowercase();
        if name.contains(".old-") || name.ends_with(".new") {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

// ---------------------------------------------------------------- registry

/// What an earlier install left in "Apps": where it went and its version.
pub struct Previous {
    pub dir: PathBuf,
    pub version: Option<String>,
}

pub fn previous() -> Option<Previous> {
    let key = CURRENT_USER.open(keys().uninstall).ok()?;
    let version = key.get_string("DisplayVersion").ok();
    let from_file = |value: &str| {
        let path = PathBuf::from(value.trim().trim_matches('"'));
        path.parent().map(Path::to_path_buf)
    };
    // This installer writes InstallLocation; NSIS only wrote the icon and
    // the uninstaller, both inside the folder.
    let dir = key
        .get_string("InstallLocation")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            key.get_string("DisplayIcon")
                .ok()
                .and_then(|v| from_file(&v))
        })
        .or_else(|| {
            key.get_string("UninstallString")
                .ok()
                .and_then(|v| from_file(&v))
        })?;
    Some(Previous { dir, version })
}

/// The entry in "Apps" (Configurações > Aplicativos).
pub fn register(dir: &Path, version: &str, size_kb: u32) -> windows_registry::Result<()> {
    let exe = dir.join(EXE);
    let uninstaller = dir.join(UNINSTALLER);
    let key = CURRENT_USER.create(keys().uninstall)?;
    key.set_string("DisplayName", PRODUCT)?;
    key.set_string("Publisher", PRODUCT)?;
    key.set_string("DisplayVersion", version)?;
    key.set_string("DisplayIcon", exe.display().to_string())?;
    key.set_string("InstallLocation", dir.display().to_string())?;
    key.set_string("UninstallString", format!("\"{}\"", uninstaller.display()))?;
    key.set_string(
        "QuietUninstallString",
        format!("\"{}\" /S", uninstaller.display()),
    )?;
    key.set_u32("EstimatedSize", size_kb)?;
    key.set_u32("NoModify", 1)?;
    key.set_u32("NoRepair", 1)?;
    Ok(())
}

/// Everything the app and its installers wrote to the registry: the "Apps"
/// entry, "Iniciar com o Windows", and the notification registration (the
/// app's name and icon, and the activator that opens a conversation from an
/// old notification).
pub fn unregister() {
    let keys = keys();
    let _ = CURRENT_USER.remove_tree(&keys.uninstall);
    if let Ok(run) = CURRENT_USER.create(&keys.run)
        && run.get_type(PRODUCT).is_ok()
    {
        let _ = run.remove_value(PRODUCT);
    }
    let classes = &keys.classes;
    let _ = CURRENT_USER.remove_tree(format!(r"{classes}\AppUserModelId\{EXE}"));
    let _ = CURRENT_USER.remove_tree(format!(
        r"{classes}\CLSID\{}",
        babutela_setup::ACTIVATOR_CLSID
    ));
}
