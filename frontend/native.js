'use strict';
// Desktop bridge: builds window.prosaNative, the API app.js expects, on top of
// the Wails runtime and the Go methods in app.go. In the web version
// config.js leaves __BOOT undefined and nothing is defined here.
(() => {
  const boot = window.__BOOT;
  if (!boot) return;
  const app = () => window.go.main.App;
  const on = name => cb => window.runtime.EventsOn(name, cb);
  window.prosaNative = {
    serverUrl: boot.serverUrl,
    version: boot.version,
    mode: boot.mode, // 'host', or 'guest' after joining a friend's server with an invite
    turnUrl: boot.turnUrl, // call relay reachable from this computer ('' if none)
    public: !!boot.public, // public build: always your own server, no server options
    window: {
      minimize: () => window.runtime.WindowMinimise(),
      maximize: () => window.runtime.WindowToggleMaximise(),
      close: () => app().CloseWindow(),
    },
    settings: {
      get: () => app().GetSettings(),
      set: patch => app().SetSettings(patch),
    },
    setBadge: n => app().SetBadge(n),
    notify: n => app().Notify(String(n.title), String(n.body), JSON.stringify(n.view || null)),
    // WebView2 shows its own picker for screen sharing.
    pickScreen: () => {},
    onScreenSources: () => {},
    hostStatus: () => app().HostStatus(),
    session: {
      get: () => app().SessionGet(),
      set: token => app().SessionSet(token || ''),
    },
    checkForUpdates: () => app().CheckForUpdates(),
    quitAndInstall: () => app().InstallUpdate(),
    publicServer: {
      status: () => app().PublicStatus(),
      set: (enabled, o) => app().SetPublic(enabled, o.domain || '', o.token || '', o.email || '', !!o.upnp),
    },
    openExternal: url => window.runtime.BrowserOpenURL(url),
    onTray: on('tray:action'),
    onNavigate: cb => window.runtime.EventsOn('navigate', v => cb(typeof v === 'string' ? JSON.parse(v) : v)),
    onUpdate: on('update:status'),
  };

  // Links meant for a new tab open in the user's browser; the app window
  // itself never navigates away from the app.
  document.addEventListener('click', e => {
    const a = e.target.closest && e.target.closest('a[href]');
    if (!a || !/^https?:/i.test(a.href) || a.target !== '_blank') return;
    e.preventDefault();
    window.runtime.BrowserOpenURL(a.href);
  }, true);
})();
