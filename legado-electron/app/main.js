'use strict';
const { app, BrowserWindow, ipcMain, Tray, Menu, nativeImage, Notification, shell, session, desktopCapturer, safeStorage } = require('electron');
const path = require('path');
const fs = require('fs');
const os = require('os');

// build/prepare.js writes config.json into the installer: the public build
// points at your own server and leaves the built-in server out. Without the
// file (development, local build) the app hosts its own server.
const BUILD = { serverUrl: 'http://localhost:3210', embeddedServer: true };
try {
  Object.assign(BUILD, JSON.parse(fs.readFileSync(path.join(__dirname, 'config.json'), 'utf8')));
} catch {
  // no config.json: local defaults
}

const DEFAULTS = {
  serverUrl: BUILD.serverUrl,
  notifications: true,
  minimizeToTray: true,
  openOnStartup: false,
  startMinimized: false,
  hostServer: BUILD.embeddedServer,
};
const INDEX = path.join(__dirname, 'renderer', 'index.html');
const SMOKE = process.argv.includes('--smoke');

// The app was called Prosa before, and another program on this machine already
// uses a "Babu Tela" profile folder. Keeping the original folder preserves
// settings, the saved login and the built-in server's data, and keeps the two
// apps from sharing a profile or a single-instance lock. --user-data-dir (used
// by tests) still wins.
if (!app.commandLine.hasSwitch('user-data-dir')) app.setPath('userData', path.join(app.getPath('appData'), 'Prosa'));

let settings = { ...DEFAULTS };
let win = null, splash = null, tray = null;
let quitting = false, badge = 0, updateReady = false;

const settingsFile = () => path.join(app.getPath('userData'), 'settings.json');
function loadSettings() {
  try {
    settings = { ...DEFAULTS, ...JSON.parse(fs.readFileSync(settingsFile(), 'utf8')) };
  } catch {
    settings = { ...DEFAULTS };
  }
  if (!BUILD.embeddedServer) {
    settings.hostServer = false;
    // Someone moving from the local build was pointed at their own machine.
    if (/^https?:\/\/(localhost|127\.0\.0\.1)(:|$)/.test(settings.serverUrl)) settings.serverUrl = BUILD.serverUrl;
  }
}
function saveSettings() {
  fs.mkdirSync(path.dirname(settingsFile()), { recursive: true });
  fs.writeFileSync(settingsFile(), JSON.stringify(settings, null, 2));
}

// App icon, optionally with the red unread dot drawn over its corner.
const ICON = nativeImage.createFromPath(path.join(__dirname, 'icon.png'));
function icon(size, withBadge) {
  const img = ICON.resize({ width: size, height: size, quality: 'best' });
  if (!withBadge) return img;
  const buf = Buffer.from(img.toBitmap());
  const r = size * 0.24, c = size * 0.76;
  for (let y = 0; y < size; y++) {
    for (let x = 0; x < size; x++) {
      const d = Math.hypot(x + 0.5 - c, y + 0.5 - c);
      const a = Math.max(0, Math.min(1, r - d + 0.5));
      const ring = Math.max(0, Math.min(1, r + size * 0.06 - d + 0.5));
      const px = (y * size + x) * 4;
      // dark ring first so the dot reads against the icon, then the dot
      for (const [k, rgb] of [[ring, [0x0d, 0x0c, 0x1d]], [a, [0xf2, 0x5f, 0x7a]]]) {
        if (!k) continue;
        buf[px] = rgb[2] * k + buf[px] * (1 - k);
        buf[px + 1] = rgb[1] * k + buf[px + 1] * (1 - k);
        buf[px + 2] = rgb[0] * k + buf[px + 2] * (1 - k);
        buf[px + 3] = Math.max(buf[px + 3], k * 255);
      }
    }
  }
  return nativeImage.createFromBitmap(buf, { width: size, height: size });
}

function applyLoginItem() {
  const args = [];
  if (!app.isPackaged) args.push(app.getAppPath());
  if (settings.startMinimized) args.push('--start-minimized');
  app.setLoginItemSettings({ openAtLogin: settings.openOnStartup, path: process.execPath, args });
}

function showMain() {
  if (!win) return;
  if (win.isMinimized()) win.restore();
  win.show();
  win.focus();
}

function splashStatus(text) {
  if (!splash || splash.isDestroyed()) return;
  splash.webContents
    .executeJavaScript(`document.getElementById('status').textContent = ${JSON.stringify(text)}`)
    .catch(() => {});
}

function createSplash() {
  splash = new BrowserWindow({
    width: 300, height: 340, frame: false, resizable: false, show: false, center: true,
    backgroundColor: '#141330', icon: icon(64), skipTaskbar: false,
    webPreferences: { contextIsolation: true, sandbox: true },
  });
  splash.loadFile(path.join(__dirname, 'splash.html'));
  const minimized = settings.startMinimized || process.argv.includes('--start-minimized');
  if (!minimized) splash.once('ready-to-show', () => splash.show());
  return new Promise(resolve => splash.webContents.once('did-finish-load', resolve));
}

function createMain() {
  const minimized = settings.startMinimized || process.argv.includes('--start-minimized');
  win = new BrowserWindow({
    width: 1240, height: 780, minWidth: 760, minHeight: 480, frame: false, show: false,
    backgroundColor: '#0d0c1d', icon: icon(64),
    webPreferences: { preload: path.join(__dirname, 'preload.js'), contextIsolation: true, sandbox: true, nodeIntegration: false },
  });
  if (SMOKE) win.webContents.on('console-message', e => console.log('CONSOLE ' + e.message));
  win.loadFile(INDEX);
  win.once('ready-to-show', () => {
    splash?.destroy();
    splash = null;
    if (!minimized) win.show();
    if (SMOKE) setTimeout(smokeReport, 1500);
  });
  win.on('close', e => {
    if (quitting || !settings.minimizeToTray) return;
    e.preventDefault();
    win.hide();
  });
  win.on('closed', () => (win = null));
  // Links open in the user's browser; the window itself never leaves the app.
  win.webContents.setWindowOpenHandler(({ url }) => {
    if (/^https?:\/\//i.test(url)) shell.openExternal(url);
    return { action: 'deny' };
  });
  win.webContents.on('will-navigate', (e, url) => {
    if (!url.startsWith('file://')) e.preventDefault();
  });
}

function refreshTray() {
  if (!tray) return;
  tray.setImage(icon(32, badge > 0));
  tray.setToolTip(badge ? `Babu Tela — ${badge} ${badge === 1 ? 'mensagem nova' : 'mensagens novas'}` : 'Babu Tela');
  if (win && process.platform === 'win32') win.setOverlayIcon(badge ? icon(32, true) : null, badge ? `${badge} novas` : '');
}

function createTray() {
  tray = new Tray(icon(32));
  const act = action => () => win?.webContents.send('tray:action', action);
  tray.setContextMenu(Menu.buildFromTemplate([
    { label: 'Abrir Babu Tela', click: showMain },
    { type: 'separator' },
    { label: 'Ligar/desligar microfone', click: act('mute') },
    { label: 'Ligar/desligar áudio', click: act('deafen') },
    { type: 'separator' },
    { label: 'Sair', click: () => { quitting = true; app.quit(); } },
  ]));
  tray.on('click', showMain);
  refreshTray();
}

function setupSession() {
  const ses = session.defaultSession;
  const allowed = new Set(['media', 'notifications', 'clipboard-sanitized-write', 'fullscreen', 'display-capture']);
  ses.setPermissionRequestHandler((wc, permission, cb) => cb(allowed.has(permission)));
  ses.setPermissionCheckHandler((wc, permission) => allowed.has(permission));
  // Screen sharing: the renderer shows a picker and answers with the chosen source.
  ses.setDisplayMediaRequestHandler(async (request, callback) => {
    try {
      const sources = await desktopCapturer.getSources({ types: ['screen', 'window'], thumbnailSize: { width: 320, height: 180 } });
      win.webContents.send('screen:sources', sources.map(s => ({ id: s.id, name: s.name, thumb: s.thumbnail.toDataURL() })));
      ipcMain.removeAllListeners('screen:picked');
      ipcMain.once('screen:picked', (e, id) => {
        const source = sources.find(s => s.id === id);
        try {
          callback(source ? { video: source } : null);
        } catch {
          // Cancelling surfaces as a rejected getDisplayMedia() in the renderer.
        }
      });
    } catch {
      try { callback(null); } catch {}
    }
  });
}

// ---------------------------------------------------------------- updates
function updater() {
  const { autoUpdater } = require('electron-updater');
  autoUpdater.setFeedURL({ provider: 'generic', url: settings.serverUrl.replace(/\/$/, '') + '/updates' });
  autoUpdater.autoDownload = true;
  return autoUpdater;
}

// Runs behind the splash screen. An update that finishes downloading there is
// installed right away; a slow one keeps downloading and is offered in-app.
function startupUpdateCheck() {
  if (!app.isPackaged) {
    splashStatus('Modo de desenvolvimento');
    return new Promise(r => setTimeout(r, 500));
  }
  splashStatus('Procurando atualizações…');
  return new Promise(resolve => {
    const au = updater();
    let downloading = false;
    const done = () => resolve();
    au.on('update-not-available', done);
    au.on('error', done);
    au.on('update-available', () => {
      downloading = true;
      splashStatus('Baixando atualização…');
      setTimeout(done, 20000);
    });
    au.on('download-progress', p => splashStatus(`Baixando atualização… ${Math.round(p.percent)}%`));
    au.on('update-downloaded', info => {
      updateReady = true;
      if (!win) {
        splashStatus('Instalando atualização…');
        quitting = true;
        return au.quitAndInstall(true, true);
      }
      win.webContents.send('update:status', { state: 'downloaded', version: info.version });
    });
    au.checkForUpdates().catch(done);
    setTimeout(() => !downloading && done(), 8000);
  });
}

// ---------------------------------------------------------------- built-in server
const HOST_PORT = 3210;
const host = { running: false, external: false, error: null };
let hostModule = null;

async function isProsaAt(port) {
  try {
    const ctl = new AbortController();
    setTimeout(() => ctl.abort(), 1500);
    const r = await fetch(`http://127.0.0.1:${port}/api/version`, { signal: ctl.signal });
    // 'Prosa' is the app's former name; those servers speak the same protocol.
    return ['Babu Tela', 'Prosa'].includes((await r.json()).name);
  } catch {
    return false;
  }
}

async function startHost() {
  if (!BUILD.embeddedServer || host.running || host.external) return;
  host.error = null;
  try {
    // The database, uploads and update files live in the app's profile folder.
    process.env.PROSA_DATA = path.join(app.getPath('userData'), 'server-data');
    hostModule ||= require('../server/index.js');
    await hostModule.start(HOST_PORT);
    host.running = true;
  } catch (e) {
    // Another Babu Tela server (the standalone one, say) already has the port: use it.
    if (e.code === 'EADDRINUSE' && (await isProsaAt(HOST_PORT))) host.external = true;
    else host.error = e.code === 'EADDRINUSE' ? `A porta ${HOST_PORT} já está sendo usada por outro programa.` : String(e.message || e);
  }
}

async function stopHost() {
  host.external = false;
  host.error = null;
  if (!host.running) return;
  await hostModule.stop();
  host.running = false;
}

function hostStatus() {
  const addresses = [];
  for (const list of Object.values(os.networkInterfaces())) {
    for (const a of list || []) if (a.family === 'IPv4' && !a.internal) addresses.push(`http://${a.address}:${HOST_PORT}`);
  }
  return { ...host, available: BUILD.embeddedServer, enabled: settings.hostServer, port: HOST_PORT, addresses, dataDir: path.join(app.getPath('userData'), 'server-data') };
}

async function pingServer() {
  splashStatus('Conectando ao servidor…');
  try {
    const ctl = new AbortController();
    setTimeout(() => ctl.abort(), 3000);
    await fetch(settings.serverUrl.replace(/\/$/, '') + '/api/version', { signal: ctl.signal });
  } catch {
    splashStatus('Servidor fora do ar. Abrindo mesmo assim…');
    await new Promise(r => setTimeout(r, 900));
  }
}

// ---------------------------------------------------------------- ipc
ipcMain.on('app:boot', e => (e.returnValue = { serverUrl: settings.serverUrl, version: app.getVersion() }));
ipcMain.on('window:minimize', () => win?.minimize());
ipcMain.on('window:maximize', () => (win?.isMaximized() ? win.unmaximize() : win?.maximize()));
ipcMain.on('window:close', () => win?.close());
ipcMain.handle('settings:get', () => settings);
ipcMain.handle('host:status', () => hostStatus());

// The login token is encrypted with the Windows user's key (DPAPI via
// safeStorage), so copying the profile folder to another account or machine
// does not carry a usable session along. Returns false when encryption is
// unavailable and the renderer falls back to its own storage.
const sessionFile = () => path.join(app.getPath('userData'), 'session.bin');
ipcMain.handle('session:get', () => {
  try {
    return safeStorage.isEncryptionAvailable() ? safeStorage.decryptString(fs.readFileSync(sessionFile())) : null;
  } catch {
    return null;
  }
});
ipcMain.handle('session:set', (e, token) => {
  if (!token) {
    fs.rmSync(sessionFile(), { force: true });
    return true;
  }
  if (!safeStorage.isEncryptionAvailable()) return false;
  fs.writeFileSync(sessionFile(), safeStorage.encryptString(String(token)));
  return true;
});
ipcMain.handle('settings:set', async (e, patch) => {
  const wasHosting = settings.hostServer;
  for (const key of Object.keys(DEFAULTS)) {
    if (!(key in Object(patch))) continue;
    if (key === 'serverUrl') {
      if (typeof patch.serverUrl === 'string' && /^https?:\/\/[^\s/]+$/.test(patch.serverUrl)) settings.serverUrl = patch.serverUrl;
    } else {
      settings[key] = !!patch[key];
    }
  }
  saveSettings();
  applyLoginItem();
  if (settings.hostServer !== wasHosting) await (settings.hostServer ? startHost() : stopHost());
  return settings;
});
ipcMain.on('badge:set', (e, n) => {
  const next = Math.max(0, Number(n) || 0);
  if (next > badge && win && !win.isFocused()) win.flashFrame(true);
  if (next === 0) win?.flashFrame(false);
  badge = next;
  refreshTray();
});
ipcMain.on('notify:show', (e, n) => {
  if (!settings.notifications || !Notification.isSupported() || win?.isFocused()) return;
  const note = new Notification({ title: String(n.title).slice(0, 80), body: String(n.body).slice(0, 240), icon: icon(128) });
  note.on('click', () => {
    showMain();
    win?.webContents.send('navigate', n.view);
  });
  note.show();
});
ipcMain.handle('update:check', async () => {
  if (!app.isPackaged) return 'A atualização automática só funciona no app instalado.';
  if (updateReady) return 'Uma atualização já está pronta para instalar.';
  try {
    const r = await updater().checkForUpdates();
    return r?.isUpdateAvailable ? 'Atualização encontrada. Baixando…' : 'Você já está na versão mais recente.';
  } catch {
    return 'Não foi possível procurar atualizações.';
  }
});
ipcMain.on('update:install', () => {
  if (!updateReady) return;
  quitting = true;
  require('electron-updater').autoUpdater.quitAndInstall(true, true);
});

// Used by `electron . --smoke`: prints what the renderer looks like, then quits.
async function smokeReport() {
  // With PROSA_SMOKE_TOKEN set, also signs in to check the gateway from the desktop shell.
  const token = process.env.PROSA_SMOKE_TOKEN;
  if (token) {
    await win.webContents.executeJavaScript(`startSession(${JSON.stringify(token)}); new Promise(r => setTimeout(r, 2500))`);
  }
  const report = await win.webContents.executeJavaScript(`(async () => ({
    user: S.me && S.me.username, plainToken: localStorage.getItem('prosa.token'), servers: S.servers.size, messages: document.querySelectorAll('#msgs .msg').length,
    settings: typeof (await prosaNative.settings.get()).minimizeToTray,
    host: await prosaNative.hostStatus(), api: await fetch(prosaNative.serverUrl + '/api/version').then(r => r.json()).catch(e => String(e)),
    native: typeof window.prosaNative, serverUrl: window.prosaNative && window.prosaNative.serverUrl,
    titlebar: !document.getElementById('titlebar').hidden,
    auth: !document.getElementById('auth').hidden, app: !document.getElementById('app').hidden,
    heading: document.querySelector('#auth h1')?.textContent || null,
  }))()`).catch(e => ({ error: String(e) }));
  console.log('SMOKE ' + JSON.stringify(report));
  quitting = true;
  app.quit();
}

// ---------------------------------------------------------------- lifecycle
if (!app.requestSingleInstanceLock()) {
  app.quit();
} else {
  app.on('second-instance', showMain);
  app.on('before-quit', () => (quitting = true));
  app.on('window-all-closed', () => app.quit());
  app.whenReady().then(async () => {
    app.setAppUserModelId('com.prosa.app'); // kept from the old name so updates replace the old install
    loadSettings();
    setupSession();
    createTray();
    await createSplash();
    if (settings.hostServer) {
      splashStatus('Iniciando servidor…');
      await startHost();
    }
    await startupUpdateCheck();
    await pingServer();
    createMain();
  });
}
