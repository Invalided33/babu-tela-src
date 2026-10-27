//! Windows notifications (toasts). Clicking one brings the window back and
//! opens the conversation it is about: while the toast is on screen, and
//! also later, from the notification center, even after the app was closed.
//!
//! For those later clicks Windows looks up the app's activator (the
//! `CustomActivator` of its AppUserModelID: a COM class whose server is this
//! .exe), starts the app if it is not running, and hands the activator what
//! the toast carries in its `launch` attribute. The running app serves that
//! class from the start, so a click reaches it instead of a new copy.

use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use babutela_buildinfo::{ACTIVATOR_CLSID, EXE};
use sha2::{Digest, Sha256};
use windows::Data::Xml::Dom::XmlDocument;
use windows::Foundation::TypedEventHandler;
use windows::UI::Notifications::{
    ToastActivatedEventArgs, ToastNotification, ToastNotificationManager,
};
use windows::Win32::Foundation::{CLASS_E_NOAGGREGATION, E_POINTER};
use windows::Win32::System::Com::{
    CLSCTX_LOCAL_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
    CoRegisterClassObject, CoUninitialize, IClassFactory, IClassFactory_Impl, REGCLS_MULTIPLEUSE,
};
use windows::Win32::UI::Notifications::{
    INotificationActivationCallback, INotificationActivationCallback_Impl,
    NOTIFICATION_USER_INPUT_DATA,
};
use windows_core::{
    BOOL, GUID, HSTRING, IInspectable, IUnknown, Interface, PCWSTR, Ref, implement,
};

use crate::shell::Shell;

/// What the command line carries when Windows starts the app for a click.
/// (COM adds `-Embedding` after it.)
pub const LAUNCH_ARG: &str = "-ToastActivated";

/// Whether Windows started the app for a click on one of its notifications.
pub fn launched_by_toast() -> bool {
    std::env::args()
        .skip(1)
        .any(|a| a.eq_ignore_ascii_case(LAUNCH_ARG))
}

/// Who the notifications come from.
struct Identity {
    /// The AppUserModelID: the .exe's file name, as the earlier versions
    /// used, so Windows keeps the person's settings for the app.
    aumid: String,
    /// The activator's COM class.
    clsid: GUID,
    /// Whether Windows may start this copy for a click. Only for the normal
    /// profile: a copy Windows starts has no PROSA_USERDATA, so it would
    /// open the person's real profile instead of the test one.
    startable: bool,
    /// The icon the notifications show, kept with the profile.
    icon: PathBuf,
}

static IDENTITY: OnceLock<Identity> = OnceLock::new();
static REGISTERED: OnceLock<Result<(), String>> = OnceLock::new();

/// What a click hands over: the toast's `launch` attribute.
type OnClick = Arc<dyn Fn(String) + Send + Sync>;

fn identity() -> Result<&'static Identity, String> {
    IDENTITY
        .get()
        .ok_or_else(|| "as notificações não foram iniciadas".to_owned())
}

/// Serves the activator for as long as the app runs, so clicks Windows
/// hands over arrive at `on_click` (on a thread of COM's). Called once, as
/// the window opens: when Windows started the app for a click, it is
/// waiting for exactly this.
pub fn start(shell: &Shell, on_click: impl Fn(String) + Send + Sync + 'static) {
    let id = IDENTITY.get_or_init(|| {
        let aumid = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_else(|| EXE.into());
        Identity {
            clsid: activator_clsid(&aumid, &shell.instance_id(), shell.own_profile()),
            aumid,
            startable: shell.own_profile(),
            icon: shell.path("notification-icon.png"),
        }
    });
    if let Err(e) = serve(id.clsid, Arc::new(on_click)) {
        crate::logln!("notificações: ativador: {e}");
    }
}

/// The activator's COM class: the installed app's own (the uninstaller
/// removes it), or, for any other copy (a build being tested, a test
/// profile), one made from its name and profile, so it never answers for
/// the installed app's notifications.
fn activator_clsid(aumid: &str, instance: &str, own_profile: bool) -> GUID {
    if own_profile
        && aumid.eq_ignore_ascii_case(EXE)
        && let Ok(clsid) = GUID::try_from(ACTIVATOR_CLSID.trim_matches(['{', '}']))
    {
        return clsid;
    }
    let hash = Sha256::digest(format!("babutela-ativador\n{aumid}\n{instance}"));
    let mut b = [0u8; 16];
    b.copy_from_slice(&hash[..16]);
    // An RFC 9562 version 8 ("made up by the application") identifier.
    b[6] = (b[6] & 0x0f) | 0x80;
    b[8] = (b[8] & 0x3f) | 0x80;
    GUID::from_u128(u128::from_be_bytes(b))
}

/// Registers the class with COM from a thread of its own, which stays in
/// COM's multithreaded apartment (and so keeps the class served) until the
/// app ends. Returns once the class is registered, or why it could not be.
fn serve(clsid: GUID, on_click: OnClick) -> Result<(), String> {
    let (done, registered) = mpsc::channel();
    std::thread::Builder::new()
        .name("notificacoes".into())
        .spawn(move || {
            // SAFETY: COM is started on this thread and never stopped: the
            // thread lives as long as the app, and with it the apartment
            // the class is served from.
            let result = unsafe {
                CoInitializeEx(None, COINIT_MULTITHREADED)
                    .ok()
                    .and_then(|()| {
                        let factory: IClassFactory = Factory { on_click }.into();
                        CoRegisterClassObject(
                            &clsid,
                            &factory,
                            CLSCTX_LOCAL_SERVER,
                            REGCLS_MULTIPLEUSE,
                        )
                    })
                    .map_err(|e| e.to_string())
            };
            let ok = result.is_ok();
            let _ = done.send(result);
            if ok {
                loop {
                    std::thread::park();
                }
            }
        })
        .map_err(|e| e.to_string())?;
    match registered.recv_timeout(Duration::from_secs(5)) {
        Ok(result) => result.map(|_| ()),
        Err(_) => Err("o COM não respondeu".into()),
    }
}

/// Makes activators for COM.
#[implement(IClassFactory)]
struct Factory {
    on_click: OnClick,
}

impl IClassFactory_Impl for Factory_Impl {
    fn CreateInstance(
        &self,
        outer: Ref<'_, IUnknown>,
        iid: *const GUID,
        object: *mut *mut c_void,
    ) -> windows_core::Result<()> {
        if iid.is_null() || object.is_null() {
            return Err(E_POINTER.into());
        }
        // SAFETY: COM hands a valid place for the answer (checked above).
        unsafe { *object = std::ptr::null_mut() };
        if !outer.is_null() {
            return Err(CLASS_E_NOAGGREGATION.into());
        }
        let activator: IUnknown = Activator {
            on_click: self.on_click.clone(),
        }
        .into();
        // SAFETY: `iid` and `object` come from COM and were checked above.
        unsafe { activator.query(iid, object).ok() }
    }

    fn LockServer(&self, _lock: BOOL) -> windows_core::Result<()> {
        Ok(())
    }
}

/// What Windows calls when one of the app's notifications is clicked.
#[implement(INotificationActivationCallback)]
struct Activator {
    on_click: OnClick,
}

impl INotificationActivationCallback_Impl for Activator_Impl {
    fn Activate(
        &self,
        _app: &PCWSTR,
        args: &PCWSTR,
        _data: *const NOTIFICATION_USER_INPUT_DATA,
        _count: u32,
    ) -> windows_core::Result<()> {
        let args = if args.is_null() {
            String::new()
        } else {
            // SAFETY: Windows passes a NUL-terminated string that lives for
            // the call.
            unsafe { args.to_string() }.unwrap_or_default()
        };
        (self.on_click)(args);
        Ok(())
    }
}

/// Saves the icon and tells Windows the app's name, icon and activator.
/// Done once, before the first notification.
fn register(id: &Identity) -> Result<(), String> {
    REGISTERED
        .get_or_init(|| {
            if std::fs::read(&id.icon).ok().as_deref() != Some(crate::tray::APP_ICON) {
                std::fs::write(&id.icon, crate::tray::APP_ICON).map_err(|e| e.to_string())?;
            }
            let clsid = format!("{{{:?}}}", id.clsid);
            crate::winsys::register_notifications(
                &id.aumid,
                &id.icon,
                id.startable.then_some(clsid.as_str()),
            )
        })
        .clone()
}

/// Shows a notification. Clicking it hands `launch` back: to `on_click`
/// while the toast is on screen, and through the activator (see `start`)
/// after that.
pub fn show(
    title: &str,
    body: &str,
    launch: &str,
    on_click: impl Fn(String) + Send + 'static,
) -> Result<(), String> {
    let id = identity()?;
    register(id)?;
    let title: String = title.chars().take(80).collect();
    let body: String = body.chars().take(240).collect();
    let toast = toast(&toast_xml(&title, &body, launch)).map_err(|e| e.to_string())?;
    let handler = TypedEventHandler::<ToastNotification, IInspectable>::new(move |_, args| {
        let launch = args
            .as_ref()
            .and_then(|a| a.cast::<ToastActivatedEventArgs>().ok())
            .and_then(|a| a.Arguments().ok())
            .map(|a| a.to_string())
            .unwrap_or_default();
        on_click(launch);
        Ok(())
    });
    toast.Activated(&handler).map_err(|e| e.to_string())?;
    ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(&id.aumid))
        .and_then(|n| n.Show(&toast))
        .map_err(|e| e.to_string())
}

fn toast(xml: &str) -> windows_core::Result<ToastNotification> {
    let doc = XmlDocument::new()?;
    doc.LoadXml(&HSTRING::from(xml))?;
    ToastNotification::CreateToastNotification(&doc)
}

/// The toast: a title, a line of text, and what clicking it hands back.
/// Without an <audio> element Windows plays its usual sound.
fn toast_xml(title: &str, body: &str, launch: &str) -> String {
    format!(
        r#"<toast launch="{}"><visual><binding template="ToastGeneric"><text>{}</text><text>{}</text></binding></visual></toast>"#,
        xml_escape(launch),
        xml_escape(title),
        xml_escape(body)
    )
}

/// Escapes text for XML, dropping the characters XML cannot hold at all
/// (most control characters), which would make Windows refuse the toast.
fn xml_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            '\t' | '\n' | '\r' => out.push(c),
            c if c < ' ' || c == '\u{FFFE}' || c == '\u{FFFF}' => {}
            c => out.push(c),
        }
    }
    out
}

/// Clicks one of the app's notifications the way Windows does it: asks COM
/// for the activator by its class and calls it with `launch`. For the app's
/// own tests (this reaches the running app, this very process).
pub fn click_like_windows(launch: &str) -> Result<(), String> {
    let id = identity()?;
    click(id.clsid, &id.aumid, launch)
}

fn click(clsid: GUID, aumid: &str, launch: &str) -> Result<(), String> {
    let (app, args) = (HSTRING::from(aumid), HSTRING::from(launch));
    // SAFETY: COM is started and stopped around the call, on this thread;
    // the strings outlive the call.
    unsafe {
        let started = CoInitializeEx(None, COINIT_MULTITHREADED).is_ok();
        let result = CoCreateInstance::<_, INotificationActivationCallback>(
            &clsid,
            None,
            CLSCTX_LOCAL_SERVER,
        )
        .and_then(|a| a.Activate(PCWSTR(app.as_ptr()), PCWSTR(args.as_ptr()), &[]))
        .map_err(|e| e.to_string());
        if started {
            CoUninitialize();
        }
        result
    }
}

/// For the app's tests: shows a notification that goes straight to the
/// notification center (no pop-up on the screen), reads back what Windows
/// keeps of it there (what a click would hand over) and removes it again.
pub fn round_trip(title: &str, launch: &str) -> Result<Vec<String>, String> {
    let id = identity()?;
    register(id)?;
    let fail = |e: windows_core::Error| e.to_string();
    let (tag, group) = (HSTRING::from("teste"), HSTRING::from("babutela-teste"));
    let app = HSTRING::from(&id.aumid);
    let toast = toast(&toast_xml(title, "", launch)).map_err(fail)?;
    toast.SetTag(&tag).map_err(fail)?;
    toast.SetGroup(&group).map_err(fail)?;
    toast.SetSuppressPopup(true).map_err(fail)?;
    let notifier = ToastNotificationManager::CreateToastNotifierWithId(&app).map_err(fail)?;
    notifier.Show(&toast).map_err(fail)?;
    let history = ToastNotificationManager::History().map_err(fail)?;
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut found = Vec::new();
    while found.is_empty() && Instant::now() < deadline {
        let list = history.GetHistoryWithId(&app).map_err(fail)?;
        for i in 0..list.Size().map_err(fail)? {
            let t = list.GetAt(i).map_err(fail)?;
            if t.Tag().is_ok_and(|t| t == tag) && t.Group().is_ok_and(|g| g == group) {
                let launch = t
                    .Content()
                    .and_then(|doc| doc.DocumentElement())
                    .and_then(|e| e.GetAttribute(&HSTRING::from("launch")))
                    .map_err(fail)?;
                found.push(launch.to_string());
            }
        }
        if found.is_empty() {
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    let _ = history.RemoveGroupedTagWithId(&tag, &group, &app);
    if found.is_empty() {
        let setting = notifier.Setting().map(|s| s.0).unwrap_or(-1);
        return Err(format!(
            "o Windows não guardou a notificação (configuração {setting})"
        ));
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toasts_carry_any_text_and_hand_back_their_launch() {
        let launch = r#"{"type":"server","serverId":1,"channelId":2} & <'x'>"#;
        let xml = toast_xml(
            "Ana (#geral)",
            "oi <b>\u{7}\u{1}tudo & \"bem\"?\nsim",
            launch,
        );
        assert!(!xml.contains('\u{7}') && !xml.contains('\u{1}'));
        // Windows' own XML reader takes it and gives the same text back.
        let doc = XmlDocument::new().unwrap();
        doc.LoadXml(&HSTRING::from(&xml)).unwrap();
        let root = doc.DocumentElement().unwrap();
        assert_eq!(
            root.GetAttribute(&HSTRING::from("launch")).unwrap(),
            HSTRING::from(launch)
        );
        let texts = doc.GetElementsByTagName(&HSTRING::from("text")).unwrap();
        assert_eq!(texts.Length().unwrap(), 2);
        let body = texts.Item(1).unwrap().InnerText().unwrap().to_string();
        assert_eq!(body, "oi <b>tudo & \"bem\"?\nsim");
        assert!(toast(&xml).is_ok());
    }

    #[test]
    fn only_the_installed_app_uses_the_installed_class() {
        let installed = GUID::try_from(ACTIVATOR_CLSID.trim_matches(['{', '}'])).unwrap();
        assert_eq!(activator_clsid(EXE, "com.prosa.app", true), installed);
        assert_eq!(
            activator_clsid("babu tela.EXE", "com.prosa.app", true),
            installed
        );
        // A test build, or a test profile, has a class of its own...
        let dev = activator_clsid("babutela.exe", "com.prosa.app", true);
        let test = activator_clsid(EXE, "com.prosa.app.abc123", false);
        assert_ne!(dev, installed);
        assert_ne!(test, installed);
        assert_ne!(dev, test);
        // ...always the same one, shaped like a proper identifier.
        assert_eq!(dev, activator_clsid("babutela.exe", "com.prosa.app", true));
        assert_eq!(dev.data3 >> 12, 8);
        assert_eq!(dev.data4[0] >> 6, 0b10);
    }

    /// The path a click from the notification center takes: COM finds the
    /// class this process serves and the activator gets the toast's launch.
    #[test]
    fn a_click_reaches_the_activator_through_com() {
        let clsid = activator_clsid(
            "babutela-teste.exe",
            &format!("teste.{}", std::process::id()),
            false,
        );
        let (tx, rx) = mpsc::channel();
        serve(
            clsid,
            Arc::new(move |args| {
                let _ = tx.send(args);
            }),
        )
        .unwrap();
        let launch = r#"{"type":"home","channelId":7}"#;
        click(clsid, "babutela-teste.exe", launch).unwrap();
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            launch.to_owned()
        );
        // A class nobody serves fails instead of starting anything.
        let nobody = activator_clsid("ninguem.exe", "teste", false);
        assert!(click(nobody, "ninguem.exe", "{}").is_err());
    }
}
