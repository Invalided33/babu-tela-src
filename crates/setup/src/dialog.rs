//! The installer's window: a Windows task dialog (the same kind Windows
//! uses for its own questions), moving from page to page.

use windows::Win32::Foundation::{HWND, LPARAM, S_FALSE, S_OK, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{
    LoadIconWithScaleDown, TASKDIALOG_BUTTON, TASKDIALOG_COMMON_BUTTON_FLAGS,
    TASKDIALOG_NOTIFICATIONS, TASKDIALOGCONFIG, TDE_CONTENT, TDF_ALLOW_DIALOG_CANCELLATION,
    TDF_CALLBACK_TIMER, TDF_SHOW_PROGRESS_BAR, TDF_USE_HICON_MAIN, TDF_VERIFICATION_FLAG_CHECKED,
    TDM_ENABLE_BUTTON, TDM_NAVIGATE_PAGE, TDM_SET_ELEMENT_TEXT, TDM_SET_PROGRESS_BAR_POS,
    TDM_SET_PROGRESS_BAR_RANGE, TDN_BUTTON_CLICKED, TDN_CREATED, TDN_NAVIGATED, TDN_TIMER,
    TDN_VERIFICATION_CLICKED, TaskDialogIndirect,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, HICON, IDCANCEL, SM_CXICON, SM_CYICON, SendMessageW,
};
use windows::core::{HRESULT, PCWSTR};

pub use windows::Win32::UI::Controls::{
    TDCBF_CANCEL_BUTTON as CANCEL, TDCBF_CLOSE_BUTTON as CLOSE, TDCBF_OK_BUTTON as OK,
};

/// The id the window's own close button and Esc report.
pub const ID_CANCEL: i32 = IDCANCEL.0;

/// One screen of the dialog.
#[derive(Default)]
pub struct Page {
    pub heading: String,
    pub text: String,
    /// Buttons of our own: (id, label). Ids from 100 up.
    pub buttons: Vec<(i32, String)>,
    /// Windows' own buttons (OK, Cancelar, Fechar), in the system language.
    pub common: TASKDIALOG_COMMON_BUTTON_FLAGS,
    pub default_button: i32,
    /// A checkbox at the bottom, and whether it starts ticked.
    pub check: Option<(String, bool)>,
    /// A progress bar, updated through [`Dialog::set_progress`] (0 to 1000).
    pub progress: bool,
    /// Buttons that start greyed out.
    pub disabled: Vec<i32>,
}

/// What happens after a click or a tick.
pub enum Next {
    Stay,
    Go(Page),
    Close,
}

/// The installer's logic, told about what the person does.
pub trait Flow {
    fn clicked(&mut self, dialog: &Dialog, id: i32) -> Next;
    /// Five times a second.
    fn tick(&mut self, _dialog: &Dialog) -> Next {
        Next::Stay
    }
    fn checked(&mut self, _on: bool) {}
}

/// The open dialog, for a flow to change what it shows.
pub struct Dialog {
    hwnd: HWND,
}

impl Dialog {
    pub fn hwnd(&self) -> HWND {
        self.hwnd
    }

    pub fn set_text(&self, text: &str) {
        let text = wide(text);
        // SAFETY: the dialog copies the string before SendMessage returns.
        unsafe {
            SendMessageW(
                self.hwnd,
                TDM_SET_ELEMENT_TEXT.0 as u32,
                Some(WPARAM(TDE_CONTENT.0 as usize)),
                Some(LPARAM(text.as_ptr() as isize)),
            );
        }
    }

    pub fn set_progress(&self, permille: u32) {
        // SAFETY: a plain message to the dialog's own window.
        unsafe {
            SendMessageW(
                self.hwnd,
                TDM_SET_PROGRESS_BAR_POS.0 as u32,
                Some(WPARAM(permille.min(1000) as usize)),
                None,
            );
        }
    }

    pub fn enable(&self, id: i32, on: bool) {
        // SAFETY: a plain message to the dialog's own window.
        unsafe {
            SendMessageW(
                self.hwnd,
                TDM_ENABLE_BUTTON.0 as u32,
                Some(WPARAM(id as usize)),
                Some(LPARAM(on as isize)),
            );
        }
    }
}

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// A page turned into what Windows reads. Its strings and buttons live in
/// vectors whose buffers never move, so `config` can point into them for as
/// long as this is kept.
struct Built {
    _strings: Vec<Vec<u16>>,
    _buttons: Vec<TASKDIALOG_BUTTON>,
    config: TASKDIALOGCONFIG,
    progress: bool,
    disabled: Vec<i32>,
}

struct Shared<'a> {
    flow: &'a mut dyn Flow,
    title: Vec<u16>,
    icon: HICON,
    /// Every page shown so far: Windows may still read a page's strings
    /// while moving to the next one, so none is dropped before the end.
    /// Boxed so each page's TASKDIALOGCONFIG keeps its address when the
    /// vector grows: the dialog was handed a pointer to it.
    #[allow(clippy::vec_box)]
    pages: Vec<Box<Built>>,
}

impl Shared<'_> {
    fn build(&mut self, page: Page) -> *const TASKDIALOGCONFIG {
        let ptr = |v: &Vec<u16>| PCWSTR(v.as_ptr());
        let mut strings = vec![wide(&page.heading), wide(&page.text)];
        let mut buttons = Vec::with_capacity(page.buttons.len());
        for (id, label) in &page.buttons {
            strings.push(wide(label));
            buttons.push(TASKDIALOG_BUTTON {
                nButtonID: *id,
                pszButtonText: ptr(strings.last().expect("just pushed")),
            });
        }
        let mut flags = TDF_USE_HICON_MAIN | TDF_ALLOW_DIALOG_CANCELLATION | TDF_CALLBACK_TIMER;
        let mut check = PCWSTR::null();
        if let Some((label, on)) = &page.check {
            strings.push(wide(label));
            check = ptr(strings.last().expect("just pushed"));
            if *on {
                flags |= TDF_VERIFICATION_FLAG_CHECKED;
            }
        }
        if page.progress {
            flags |= TDF_SHOW_PROGRESS_BAR;
        }
        let mut config = TASKDIALOGCONFIG {
            cbSize: size_of::<TASKDIALOGCONFIG>() as u32,
            dwFlags: flags,
            dwCommonButtons: page.common,
            pszWindowTitle: ptr(&self.title),
            pszMainInstruction: ptr(&strings[0]),
            pszContent: ptr(&strings[1]),
            cButtons: buttons.len() as u32,
            pButtons: if buttons.is_empty() {
                std::ptr::null()
            } else {
                buttons.as_ptr()
            },
            nDefaultButton: page.default_button,
            pszVerificationText: check,
            pfCallback: Some(callback),
            lpCallbackData: self as *mut Shared as isize,
            // The same width on every page (in dialog units), so the window
            // does not jump around while moving between them.
            cxWidth: 260,
            ..Default::default()
        };
        config.Anonymous1.hMainIcon = self.icon;
        self.pages.push(Box::new(Built {
            _strings: strings,
            _buttons: buttons,
            config,
            progress: page.progress,
            disabled: page.disabled,
        }));
        &self.pages.last().expect("just pushed").config
    }

    fn current(&self) -> Option<&Built> {
        self.pages.last().map(|b| &**b)
    }
}

/// Windows calls this for everything that happens in the dialog, on the
/// thread that opened it. Messages sent from here can call it again
/// (moving to another page does), so the shared state is only ever
/// borrowed for a moment, never across a SendMessage.
unsafe extern "system" fn callback(
    hwnd: HWND,
    msg: TASKDIALOG_NOTIFICATIONS,
    wparam: WPARAM,
    _lparam: LPARAM,
    data: isize,
) -> HRESULT {
    let shared = data as *mut Shared;
    let dialog = Dialog { hwnd };
    let next = match msg {
        TDN_CREATED | TDN_NAVIGATED => {
            // SAFETY: `data` is the `Shared` that `run` keeps alive, and
            // untouched, while the dialog is open.
            let (progress, disabled) = match unsafe { (*shared).current() } {
                Some(page) => (page.progress, page.disabled.clone()),
                None => (false, Vec::new()),
            };
            if progress {
                // SAFETY: a plain message to the dialog's own window.
                unsafe {
                    SendMessageW(
                        hwnd,
                        TDM_SET_PROGRESS_BAR_RANGE.0 as u32,
                        None,
                        Some(LPARAM((1000 << 16) as isize)),
                    );
                }
            }
            for id in disabled {
                dialog.enable(id, false);
            }
            Next::Stay
        }
        TDN_BUTTON_CLICKED => {
            // SAFETY: as above; the flow's own messages do not move pages.
            let next = unsafe { (*shared).flow.clicked(&dialog, wparam.0 as i32) };
            if let Next::Close = next {
                return S_OK;
            }
            go(shared, hwnd, next);
            return S_FALSE;
        }
        // SAFETY: as above.
        TDN_TIMER => unsafe { (*shared).flow.tick(&dialog) },
        TDN_VERIFICATION_CLICKED => {
            // SAFETY: as above.
            unsafe { (*shared).flow.checked(wparam.0 != 0) };
            Next::Stay
        }
        _ => Next::Stay,
    };
    go(shared, hwnd, next);
    S_OK
}

fn go(shared: *mut Shared, hwnd: HWND, next: Next) {
    match next {
        Next::Go(page) => {
            // SAFETY: see `callback`; the borrow ends before the message.
            let config = unsafe { (*shared).build(page) };
            // SAFETY: `config` and everything it points to stay alive in
            // `shared.pages` until the dialog is gone.
            unsafe {
                SendMessageW(
                    hwnd,
                    TDM_NAVIGATE_PAGE.0 as u32,
                    None,
                    Some(LPARAM(config as isize)),
                );
            }
        }
        Next::Close => {
            // Same as the person pressing Esc; the flow answers Close again.
            // SAFETY: a plain message to the dialog's own window.
            unsafe {
                SendMessageW(
                    hwnd,
                    windows::Win32::UI::Controls::TDM_CLICK_BUTTON.0 as u32,
                    Some(WPARAM(ID_CANCEL as usize)),
                    None,
                );
            }
        }
        Next::Stay => {}
    }
}

/// How the dialog was closed.
pub struct Closed {
    /// The id of the button that closed it.
    pub button: i32,
    /// Whether the last page's checkbox was ticked.
    pub checked: bool,
}

/// Shows the dialog, starting at `first`, until a page closes it.
pub fn run(title: &str, first: Page, flow: &mut dyn Flow) -> Closed {
    // SAFETY: icon 1 is the .exe's own (see build.rs); the size is the
    // system's for large icons.
    let icon = unsafe {
        let module = GetModuleHandleW(None).ok();
        LoadIconWithScaleDown(
            module.map(Into::into),
            PCWSTR(1 as _),
            GetSystemMetrics(SM_CXICON),
            GetSystemMetrics(SM_CYICON),
        )
        .unwrap_or_default()
    };
    let mut shared = Shared {
        flow,
        title: wide(title),
        icon,
        pages: Vec::new(),
    };
    let config = shared.build(first);
    let mut button = 0;
    let mut checked = windows::core::BOOL(0);
    // SAFETY: `config` points into `shared`, which outlives the call; the
    // callback gets `shared` through lpCallbackData.
    let shown = unsafe { TaskDialogIndirect(config, Some(&mut button), None, Some(&mut checked)) };
    if shown.is_err() {
        return Closed {
            button: ID_CANCEL,
            checked: false,
        };
    }
    Closed {
        button,
        checked: checked.as_bool(),
    }
}

/// A message with one button.
pub fn message(title: &str, heading: &str, text: &str) {
    struct Done;
    impl Flow for Done {
        fn clicked(&mut self, _: &Dialog, _: i32) -> Next {
            Next::Close
        }
    }
    run(
        title,
        Page {
            heading: heading.into(),
            text: text.into(),
            common: OK,
            ..Default::default()
        },
        &mut Done,
    );
}
