//! "Babu Tela Setup.exe": installs the app for the current user, and, copied
//! next to it as uninstall.exe, removes it.
//!
//!   Babu Tela Setup.exe        asks where, installs, offers to open the app
//!   Babu Tela Setup.exe /S     installs without a window (the automatic
//!                              update of app 0.11.1 and older)
//!   Babu Tela Setup.exe --update --wait-pid=N --relaunch [/S] [--minimized]
//!                              the app updating itself: waits for it to
//!                              quit, replaces it with a small progress
//!                              window (none with /S) and opens it again
//!                              (babutela_buildinfo::Update)
//!   uninstall.exe [/S]         removes the app (what "Apps" runs)
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(not(windows))]
compile_error!("O instalador do Babu Tela é só para Windows.");

mod dialog;
mod sys;

use std::io::{BufWriter, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use babutela_buildinfo::{Update, VERSION};
use babutela_setup::{EXE, PRODUCT, UNINSTALLER, inside, payload, silent};

use crate::dialog::{Dialog, Flow, Next, Page};

/// The app, packed by `cargo xtask release` (empty in any other build).
const PAYLOAD: &[u8] = include_bytes!(env!("BABUTELA_PAYLOAD"));

const TITLE: &str = "Instalar o Babu Tela";
const UNINSTALL_TITLE: &str = "Remover o Babu Tela";
const UPDATE_TITLE: &str = "Atualizando o Babu Tela";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let quiet = silent(&args);
    let me = std::env::current_exe().unwrap_or_default();
    if let Some(dir) = args
        .iter()
        .find_map(|a| a.strip_prefix("--uninstall-from="))
    {
        let wipe = args.iter().any(|a| a == "--wipe");
        return finish_uninstall(Path::new(dir), quiet, wipe, &me);
    }
    let named = me
        .file_name()
        .is_some_and(|n| n.eq_ignore_ascii_case(UNINSTALLER));
    if named || args.iter().any(|a| a == "--uninstall") {
        return start_uninstall(quiet, &me);
    }
    if let Some(update) = Update::from_args(&args) {
        return update_main(&update);
    }
    install_main(quiet)
}

// ---------------------------------------------------------------- installing

/// Where the work stands, shared between the dialog and the thread doing it.
#[derive(Default)]
struct Progress {
    permille: u32,
    step: String,
    result: Option<Result<(), String>>,
}

type Shared = Arc<Mutex<Progress>>;

fn lock(p: &Shared) -> std::sync::MutexGuard<'_, Progress> {
    p.lock().unwrap_or_else(PoisonError::into_inner)
}

/// How the install treats a copy of the app that is open.
#[derive(Clone, Copy)]
enum Open {
    /// Ask it to close; if it does not, stop and say so (with a window, the
    /// person can close it and try again).
    MustClose,
    /// Ask it to close and carry on either way: its file is renamed out of
    /// the way (an update without a window has nobody to ask).
    CarryOn,
}

/// Installs into `dir`. `desktop` also puts a shortcut on the desktop.
/// Without a window (`CarryOn`), each step also goes to the log, the only
/// place anyone can look when an automatic update goes wrong.
fn install(dir: &Path, desktop: bool, open: Open, progress: &Shared) -> Result<(), String> {
    let started = std::time::Instant::now();
    let set = |permille: u32, step: &str| {
        let mut p = lock(progress);
        p.permille = permille;
        if p.step != step {
            p.step = step.to_owned();
            if matches!(open, Open::CarryOn) {
                log(&format!("{} ms: {step}", started.elapsed().as_millis()));
            }
        }
    };
    let _com = sys::Com::init();
    // From here until the files are in place, an app that starts leaves
    // them alone.
    let _lock = sys::SetupLock::take();
    if sys::app_running() {
        set(0, "Fechando o Babu Tela que está aberto");
        let closed = sys::close_app(Duration::from_secs(15));
        if !closed && matches!(open, Open::MustClose) {
            return Err(
                "O Babu Tela ainda está aberto. Feche-o (no ícone perto do relógio, \"Sair\") e tente de novo."
                    .into(),
            );
        }
    }
    set(0, "Copiando o app");
    std::fs::create_dir_all(dir)
        .map_err(|e| format!("Não deu para criar a pasta {}: {e}", dir.display()))?;
    sys::sweep(dir);

    let exe = dir.join(EXE);
    let fresh = dir.join(format!("{EXE}.new"));
    let unpacked = (|| -> std::io::Result<()> {
        let mut out = BufWriter::new(std::fs::File::create(&fresh)?);
        payload::unpack(PAYLOAD, &mut out, |done, total| {
            set((done * 900 / total.max(1)) as u32, "Copiando o app");
        })?;
        out.flush()?;
        out.into_inner().map_err(|e| e.into_error())?.sync_all()
    })();
    if let Err(e) = unpacked {
        let _ = std::fs::remove_file(&fresh);
        return Err(format!("Não deu para copiar o app: {e}"));
    }
    sys::replace(&fresh, &exe).map_err(|e| {
        let _ = std::fs::remove_file(&fresh);
        format!("Não deu para trocar {}: {e}", exe.display())
    })?;

    // The uninstaller is this same program under another name.
    set(920, "Preparando o desinstalador");
    let uninstaller = dir.join(UNINSTALLER);
    let me = std::env::current_exe().map_err(|e| e.to_string())?;
    if !same_file(&me, &uninstaller) {
        let staged = dir.join(format!("{UNINSTALLER}.new"));
        std::fs::copy(&me, &staged)
            .and_then(|_| sys::replace(&staged, &uninstaller))
            .map_err(|e| format!("Não deu para gravar o desinstalador: {e}"))?;
    }

    set(950, "Criando os atalhos");
    sys::shortcut(&sys::start_menu_link(), &exe)
        .map_err(|e| format!("Não deu para criar o atalho no menu Iniciar: {e}"))?;
    if desktop {
        // A desktop shortcut is a nicety: failing to make one is no reason
        // to call the install a failure.
        let _ = sys::shortcut(&sys::desktop_link(), &exe);
    }

    set(980, "Registrando no Windows");
    let size = [&exe, &uninstaller]
        .iter()
        .filter_map(|p| std::fs::metadata(p).ok())
        .map(|m| m.len())
        .sum::<u64>();
    sys::register(dir, VERSION, (size / 1024) as u32)
        .map_err(|e| format!("Não deu para registrar o app no Windows: {e}"))?;
    set(1000, "Pronto");
    Ok(())
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

fn install_main(quiet: bool) -> ExitCode {
    let has_app = payload::info(PAYLOAD).is_some();
    let previous = sys::previous();
    let dir = previous
        .as_ref()
        .map(|p| p.dir.clone())
        .unwrap_or_else(sys::default_dir);

    if quiet {
        if !has_app {
            return ExitCode::from(2);
        }
        // An update: same folder, and the desktop shortcut only if it is
        // still there (whoever deleted it does not want it back).
        let desktop = previous.is_none() || sys::desktop_link().exists();
        let progress = Shared::default();
        return match install(&dir, desktop, Open::CarryOn, &progress) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                log(&format!("instalação silenciosa falhou: {e}"));
                ExitCode::FAILURE
            }
        };
    }

    if !has_app {
        dialog::message(
            TITLE,
            "Este instalador está vazio",
            "Ele foi gerado sem o app dentro. Gere o instalador com: cargo xtask release",
        );
        return ExitCode::from(2);
    }
    let _com = sys::Com::init();
    let mut flow = Installer {
        dir,
        previous: previous.and_then(|p| p.version),
        desktop: true,
        open_after: true,
        progress: Shared::default(),
        working: false,
        finished: false,
    };
    let first = flow.welcome();
    let closed = dialog::run(TITLE, first, &mut flow);
    // The last page is "Pronto" with its "Abrir o Babu Tela agora".
    if flow.finished && closed.checked {
        let exe = flow.dir.join(EXE);
        let mut open = detached(&exe);
        open.current_dir(&flow.dir);
        // A test of the installer opens the app on a profile of its own,
        // never the real login of whoever runs the test.
        if let Some(root) = babutela_setup::test_root() {
            open.env("PROSA_USERDATA", root.join("perfil"));
        }
        let _ = open.spawn();
    }
    if flow.finished {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

// ---------------------------------------------------------------- updating

/// `--update`: the app updating itself as it starts, the way Discord's
/// does (crates/app/src/update.rs). The app quits right after starting
/// this; once it is gone its file is replaced, and it opens again whether
/// that worked or not (the app does not try the same update forever).
fn update_main(update: &Update) -> ExitCode {
    let dir = sys::previous()
        .map(|p| p.dir)
        .unwrap_or_else(sys::default_dir);
    log(&format!("atualizando {} para a {VERSION}", dir.display()));
    let result = if payload::info(PAYLOAD).is_none() {
        Err("este instalador está vazio".to_owned())
    } else {
        if let Some(pid) = update.wait_pid
            && !sys::wait_for_exit(pid, Duration::from_secs(20))
        {
            log(&format!(
                "o app (processo {pid}) não fechou em 20 s: atualizando assim mesmo"
            ));
        }
        // The desktop shortcut only if it is still there (whoever deleted
        // it does not want it back).
        let desktop = sys::desktop_link().exists();
        if update.silent {
            install(&dir, desktop, Open::CarryOn, &Shared::default())
        } else {
            update_with_window(&dir, desktop)
        }
    };
    match &result {
        Ok(()) => log("atualização instalada"),
        Err(e) => log(&format!("a atualização falhou: {e}")),
    }
    if update.relaunch {
        relaunch(&dir, update.minimized);
    }
    if result.is_ok() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// The update with a small window showing where it is (Discord's splash
/// screen, in the installer's own kind of window). Done like `/S`; the
/// window only follows, and says so when it fails.
fn update_with_window(dir: &Path, desktop: bool) -> Result<(), String> {
    let progress = Shared::default();
    let worker = {
        let (dir, progress) = (dir.to_path_buf(), progress.clone());
        std::thread::spawn(move || {
            let result = install(&dir, desktop, Open::CarryOn, &progress);
            lock(&progress).result = Some(result.clone());
            result
        })
    };
    let mut flow = Updating {
        progress,
        working: true,
    };
    // Should the window not open at all, the work goes on just the same.
    dialog::run(UPDATE_TITLE, Updating::page(), &mut flow);
    worker
        .join()
        .unwrap_or_else(|_| Err("a atualização parou no meio".into()))
}

/// The update's window: a progress bar, and the error if there is one.
struct Updating {
    progress: Shared,
    working: bool,
}

impl Updating {
    fn page() -> Page {
        Page {
            heading: format!("Atualizando o {PRODUCT} para a versão {VERSION}"),
            text: "Preparando".into(),
            common: dialog::CANCEL,
            progress: true,
            disabled: vec![dialog::ID_CANCEL],
            ..Default::default()
        }
    }
}

impl Flow for Updating {
    fn clicked(&mut self, _: &Dialog, _: i32) -> Next {
        // Esc or the window's X: not while files are moving.
        if self.working {
            Next::Stay
        } else {
            Next::Close
        }
    }

    fn tick(&mut self, d: &Dialog) -> Next {
        if !self.working {
            return Next::Stay;
        }
        let (permille, step, result) = {
            let mut p = lock(&self.progress);
            (p.permille, p.step.clone(), p.result.take())
        };
        d.set_progress(permille);
        if !step.is_empty() {
            d.set_text(&step);
        }
        let Some(result) = result else {
            return Next::Stay;
        };
        self.working = false;
        match result {
            Ok(()) => {
                // Closing goes through the Cancelar button, greyed out
                // while the files moved.
                d.enable(dialog::ID_CANCEL, true);
                Next::Close
            }
            Err(e) => Next::Go(Page {
                heading: "Não deu para atualizar".into(),
                text: format!("{e}\n\nO {PRODUCT} vai abrir na versão de antes."),
                common: dialog::CLOSE,
                ..Default::default()
            }),
        }
    }
}

/// Opens the app again after an update, in the tray if it had been
/// started there.
fn relaunch(dir: &Path, minimized: bool) {
    let mut open = detached(dir.join(EXE));
    open.current_dir(dir);
    if minimized {
        open.arg("--start-minimized");
    }
    match open.spawn() {
        Ok(child) => sys::allow_foreground(child.id()),
        Err(e) => log(&format!("abrir o app de novo: {e}")),
    }
}

const ID_INSTALL: i32 = 100;
const ID_FOLDER: i32 = 101;
const ID_DONE: i32 = 102;
const ID_RETRY: i32 = 103;
const ID_REMOVE: i32 = 110;

struct Installer {
    dir: PathBuf,
    previous: Option<String>,
    desktop: bool,
    open_after: bool,
    progress: Shared,
    working: bool,
    finished: bool,
}

impl Installer {
    fn welcome(&self) -> Page {
        let (heading, intro) = match self.previous.as_deref() {
            Some(v) if v == VERSION => (
                format!("Reinstalar o {PRODUCT} {VERSION}"),
                "Esta versão já está instalada; instalar de novo refaz o app e os atalhos."
                    .to_owned(),
            ),
            Some(v) => (
                format!("Atualizar o {PRODUCT} para a versão {VERSION}"),
                format!(
                    "A versão {v} instalada vai ser trocada pela nova. O login e as configurações continuam como estão."
                ),
            ),
            None => (
                format!("Instalar o {PRODUCT} {VERSION}"),
                "Chat de comunidades com texto, voz, vídeo e compartilhamento de tela.".to_owned(),
            ),
        };
        let mut text = format!("{intro}\n\nPasta: {}", self.dir.display());
        if sys::app_running() {
            text.push_str("\n\nO Babu Tela está aberto e vai ser fechado para instalar.");
        }
        Page {
            heading,
            text,
            buttons: vec![
                (ID_INSTALL, "Instalar".into()),
                (ID_FOLDER, "Mudar a pasta…".into()),
            ],
            common: dialog::CANCEL,
            default_button: ID_INSTALL,
            check: Some(("Criar um atalho na área de trabalho".into(), self.desktop)),
            ..Default::default()
        }
    }

    fn start(&mut self) -> Page {
        *lock(&self.progress) = Progress::default();
        self.working = true;
        let (dir, desktop, progress) = (self.dir.clone(), self.desktop, self.progress.clone());
        std::thread::spawn(move || {
            let result = install(&dir, desktop, Open::MustClose, &progress);
            lock(&progress).result = Some(result);
        });
        Page {
            heading: "Instalando…".into(),
            text: "Preparando".into(),
            common: dialog::CANCEL,
            progress: true,
            disabled: vec![dialog::ID_CANCEL],
            ..Default::default()
        }
    }
}

impl Flow for Installer {
    fn clicked(&mut self, d: &Dialog, id: i32) -> Next {
        match id {
            ID_INSTALL => Next::Go(self.start()),
            ID_RETRY => Next::Go(self.start()),
            ID_FOLDER => {
                if let Some(chosen) = sys::pick_folder(d.hwnd(), &self.dir) {
                    self.dir = inside(&chosen);
                    Next::Go(self.welcome())
                } else {
                    Next::Stay
                }
            }
            ID_DONE => Next::Close,
            // Esc, the window's X or "Cancelar": not while files are moving.
            _ if self.working => Next::Stay,
            _ => Next::Close,
        }
    }

    fn checked(&mut self, on: bool) {
        // One checkbox per page: the desktop shortcut before installing,
        // opening the app after.
        if self.finished {
            self.open_after = on;
        } else if !self.working {
            self.desktop = on;
        }
    }

    fn tick(&mut self, d: &Dialog) -> Next {
        if !self.working {
            return Next::Stay;
        }
        let (permille, step, result) = {
            let mut p = lock(&self.progress);
            (p.permille, p.step.clone(), p.result.take())
        };
        d.set_progress(permille);
        if !step.is_empty() {
            d.set_text(&step);
        }
        let Some(result) = result else {
            return Next::Stay;
        };
        self.working = false;
        match result {
            Ok(()) => {
                self.finished = true;
                let shortcuts = if self.desktop {
                    "no menu Iniciar e na área de trabalho"
                } else {
                    "no menu Iniciar"
                };
                Next::Go(Page {
                    heading: "Pronto".into(),
                    text: format!(
                        "O {PRODUCT} {VERSION} está instalado. Há um atalho {shortcuts}.\n\n\
                         Para entrar, use a conta que você criou ou crie uma na primeira tela."
                    ),
                    buttons: vec![(ID_DONE, "Concluir".into())],
                    default_button: ID_DONE,
                    check: Some(("Abrir o Babu Tela agora".into(), self.open_after)),
                    ..Default::default()
                })
            }
            Err(e) => Next::Go(Page {
                heading: "Não deu para instalar".into(),
                text: e,
                buttons: vec![(ID_RETRY, "Tentar de novo".into())],
                common: dialog::CLOSE,
                default_button: ID_RETRY,
                ..Default::default()
            }),
        }
    }
}

// ---------------------------------------------------------------- removing

/// First half: asks, then hands over to a copy of this program outside the
/// folder, which can then delete the folder (a running program cannot
/// delete its own file).
fn start_uninstall(quiet: bool, me: &Path) -> ExitCode {
    let dir = me
        .parent()
        .filter(|d| {
            d.join(EXE).exists()
                || me
                    .file_name()
                    .is_some_and(|n| n.eq_ignore_ascii_case(UNINSTALLER))
        })
        .map(Path::to_path_buf)
        .or_else(|| sys::previous().map(|p| p.dir))
        .unwrap_or_else(sys::default_dir);
    let mut wipe = false;
    if !quiet {
        struct Ask;
        impl Flow for Ask {
            fn clicked(&mut self, _: &Dialog, _: i32) -> Next {
                Next::Close
            }
        }
        let closed = dialog::run(
            UNINSTALL_TITLE,
            Page {
                heading: format!("Remover o {PRODUCT} deste computador?"),
                text: "Suas contas, servidores e mensagens continuam guardados no servidor: \
                       é só instalar de novo para voltar."
                    .into(),
                buttons: vec![(ID_REMOVE, "Remover".into())],
                common: dialog::CANCEL,
                default_button: dialog::ID_CANCEL,
                check: Some((
                    "Apagar também o login salvo e as configurações deste computador".into(),
                    false,
                )),
                ..Default::default()
            },
            &mut Ask,
        );
        if closed.button != ID_REMOVE {
            return ExitCode::FAILURE;
        }
        wipe = closed.checked;
    }
    let temp = std::env::temp_dir().join(format!("babutela-uninstall-{}.exe", std::process::id()));
    if std::fs::copy(me, &temp).is_err() {
        return finish_uninstall(&dir, quiet, wipe, me);
    }
    let mut cmd = detached(&temp);
    cmd.arg(format!("--uninstall-from={}", dir.display()));
    if quiet {
        cmd.arg("/S");
    }
    if wipe {
        cmd.arg("--wipe");
    }
    match cmd.spawn() {
        Ok(_) => ExitCode::SUCCESS,
        Err(_) => finish_uninstall(&dir, quiet, wipe, me),
    }
}

/// Second half, running from %TEMP%.
fn finish_uninstall(dir: &Path, quiet: bool, wipe: bool, me: &Path) -> ExitCode {
    if !sys::close_app(Duration::from_secs(15)) {
        if !quiet {
            dialog::message(
                UNINSTALL_TITLE,
                "O Babu Tela ainda está aberto",
                "Feche-o (no ícone perto do relógio, \"Sair\") e tente remover de novo.",
            );
        }
        delete_later(me, Vec::new(), None);
        return ExitCode::FAILURE;
    }
    // Only the files this installer put there: the folder may have been
    // chosen by hand, and anything else in it is not ours to delete.
    // Whatever is still held (Windows Defender keeps a program that has just
    // run, unsigned like this one, for some seconds) goes a little later.
    let mut later = Vec::new();
    for name in [EXE, UNINSTALLER] {
        let path = dir.join(name);
        let mut held = false;
        for _ in 0..10 {
            match std::fs::remove_file(&path) {
                Ok(()) => {
                    held = false;
                    break;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    held = false;
                    break;
                }
                Err(_) => held = true,
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        if held {
            later.push(path);
        }
    }
    sys::sweep(dir);
    let _ = std::fs::remove_dir(dir);
    for link in [sys::start_menu_link(), sys::desktop_link()] {
        let _ = std::fs::remove_file(link);
    }
    sys::unregister();
    let roaming = sys::roaming();
    // The browser data of the Wails and Tauri builds.
    let _ = std::fs::remove_dir_all(roaming.join(EXE));
    let _ = std::fs::remove_dir_all(roaming.join("Prosa").join("webview"));
    if wipe {
        let _ = std::fs::remove_dir_all(roaming.join("Prosa"));
    }
    // An update the app downloaded and never installed.
    let local = sys::local().join("Prosa");
    let _ = std::fs::remove_dir_all(local.join("updates"));
    let _ = std::fs::remove_dir(&local);
    if !quiet {
        dialog::message(
            UNINSTALL_TITLE,
            "Pronto",
            &format!("O {PRODUCT} foi removido deste computador."),
        );
    }
    delete_later(me, later, Some(dir));
    ExitCode::SUCCESS
}

/// Deletes, once this program has exited, the files that could not go now:
/// the copy in %TEMP% that is running, and anything an antivirus was still
/// reading. Then the folder, if that left it empty (rmdir without /s never
/// takes anything else).
fn delete_later(me: &Path, mut files: Vec<PathBuf>, dir: Option<&Path>) {
    let in_temp = me
        .parent()
        .is_some_and(|p| same_file(p, &std::env::temp_dir()));
    if in_temp {
        files.push(me.to_path_buf());
    }
    // cmd would expand %...% inside the paths and stop at a quote; such a
    // name cannot come from Windows' own folders, but never risk it.
    files.retain(|f| !f.to_string_lossy().contains(['%', '"']));
    if files.is_empty() {
        return;
    }
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let del: String = files
        .iter()
        .map(|f| format!(r#"del /f /q "{}" 2>nul & "#, f.display()))
        .collect();
    let rmdir = dir
        .filter(|d| !d.to_string_lossy().contains(['%', '"']))
        .map(|d| format!(r#"rmdir "{}" 2>nul & "#, d.display()))
        .unwrap_or_default();
    let done: String = files
        .iter()
        .map(|f| format!(r#"if not exist "{}" "#, f.display()))
        .collect();
    // Up to a minute, a try a second; `ping` is the pause that needs no
    // console.
    let script = format!(
        r#""for /l %i in (1,1,60) do (ping -n 2 127.0.0.1 >nul & {del}{rmdir}{done}exit)""#
    );
    let spawned = detached("cmd")
        .raw_arg("/c")
        .raw_arg(&script)
        .creation_flags(CREATE_NO_WINDOW)
        .spawn();
    if let Err(e) = spawned {
        log(&format!("apagar depois: {e}"));
    }
    if test_mode() {
        log(&format!("apagar depois: {script}"));
    }
}

fn test_mode() -> bool {
    babutela_setup::test_root().is_some()
}

/// A program started on its own, with nothing of this one's console: a
/// windowless program started from a terminal holds handles Windows will
/// not pass on (CreateProcess then fails with "not supported").
fn detached(program: impl AsRef<std::ffi::OsStr>) -> Command {
    use std::process::Stdio;
    let mut cmd = Command::new(program);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    cmd
}

/// Silent runs have nobody to tell; this is where to look afterwards.
fn log(line: &str) {
    let path = std::env::temp_dir().join("babutela-setup.log");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(f, "{VERSION}: {line}");
    }
}
