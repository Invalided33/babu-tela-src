package main

import (
	"context"
	"crypto/rand"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"io"
	"log"
	"net"
	"os"
	"path/filepath"
	"regexp"
	"strings"
	"sync"
	"time"

	wruntime "github.com/wailsapp/wails/v2/pkg/runtime"

	"babutela/internal/buildinfo"
)

// Settings is saved as settings.json in the profile folder, with the same keys
// the earlier Electron version used.
type Settings struct {
	ServerURL      string `json:"serverUrl"`
	Notifications  bool   `json:"notifications"`
	MinimizeToTray bool   `json:"minimizeToTray"`
	OpenOnStartup  bool   `json:"openOnStartup"`
	StartMinimized bool   `json:"startMinimized"`
	HostServer     bool   `json:"hostServer"` // always on in the host build
	// Public server on this PC, no VPS (publichost.go).
	Public       bool   `json:"public"`
	PublicDomain string `json:"publicDomain"`
	PublicEmail  string `json:"publicEmail"`
	UPnP         bool   `json:"upnp"`
}

type App struct {
	ctx         context.Context
	dir         string // profile folder: settings, login, built-in server data
	mu          sync.Mutex
	settings    Settings
	quitting    bool
	badge       int
	host        host
	pub         publicState // public server on this PC
	localTURN   io.Closer   // relay for this computer's own calls (turn.go)
	localTURNLn net.Listener
	views       map[string]string // notification id -> where clicking it should go
	update      *pendingUpdate
}

var localURLRe = regexp.MustCompile(`^https?://(localhost|127\.0\.0\.1)(:|$)`)

func NewApp() *App {
	a := &App{dir: profileDir(), views: map[string]string{}}
	os.MkdirAll(a.dir, 0o755)
	a.loadSettings()
	return a
}

// profileDir is %APPDATA%\Prosa, the folder the app has used since it was
// called Prosa (another program on this machine already has a "Babu Tela"
// folder). PROSA_USERDATA points it elsewhere, for tests.
func profileDir() string {
	if d := os.Getenv("PROSA_USERDATA"); d != "" {
		return d
	}
	base, err := os.UserConfigDir()
	if err != nil {
		base = "."
	}
	return filepath.Join(base, "Prosa")
}

// instanceID keeps a test profile (PROSA_USERDATA) from handing off to an
// installed copy that is already open.
func (a *App) instanceID() string {
	if os.Getenv("PROSA_USERDATA") == "" {
		return "com.prosa.app"
	}
	h := sha256.Sum256([]byte(a.dir))
	return "com.prosa.app." + hex.EncodeToString(h[:6])
}

func (a *App) path(parts ...string) string {
	return filepath.Join(append([]string{a.dir}, parts...)...)
}

func embeddedServer() bool { return publicServerURL == "" }

func (a *App) loadSettings() {
	s := Settings{ServerURL: fmt.Sprintf("http://localhost:%d", hostPort), Notifications: true, MinimizeToTray: true, HostServer: true}
	if !embeddedServer() {
		s.ServerURL, s.HostServer = publicServerURL, false
	}
	if b, err := os.ReadFile(a.path("settings.json")); err == nil {
		json.Unmarshal(b, &s)
	}
	// Only one server option exists: the host build always serves (and talks
	// to) the server on this PC; the friends' build always talks to yours.
	if embeddedServer() {
		s.HostServer, s.ServerURL = true, fmt.Sprintf("http://localhost:%d", hostPort)
		if hostDomain != "" {
			s.Public, s.PublicDomain, s.UPnP = true, hostDomain, true
			if s.PublicEmail == "" {
				s.PublicEmail = hostEmail
			}
		}
	} else {
		s.HostServer, s.ServerURL = false, publicServerURL
	}
	a.settings = s
}

func (a *App) saveSettings() {
	b, _ := json.MarshalIndent(a.settings, "", "  ")
	if err := os.WriteFile(a.path("settings.json"), b, 0o644); err != nil {
		log.Println("salvando configuraÃ§Ãµes:", err)
	}
}

func (a *App) startHidden() bool {
	if a.settings.StartMinimized {
		return true
	}
	for _, arg := range os.Args[1:] {
		if arg == "--start-minimized" {
			return true
		}
	}
	return false
}

// ---------------------------------------------------------------- lifecycle

func (a *App) startup(ctx context.Context) {
	a.ctx = ctx
	if err := wruntime.InitializeNotifications(ctx); err != nil {
		log.Println("notificaÃ§Ãµes:", err)
	}
	wruntime.OnNotificationResponse(ctx, func(res wruntime.NotificationResult) {
		a.showWindow()
		a.mu.Lock()
		view := a.views[res.Response.ID]
		a.mu.Unlock()
		if view != "" && view != "null" {
			wruntime.EventsEmit(ctx, "navigate", view)
		}
	})
	if os.Getenv("PROSA_NO_TRAY") == "" {
		startTray(a)
	}
	go a.checkForUpdatesInBackground()
}

func (a *App) domReady(ctx context.Context) {
	if file := smokeReportFile(); file != "" {
		a.runSmoke(file)
	}
}

// beforeClose: closing the window keeps the app (and the built-in server) in
// the tray unless that is turned off or the user chose Quit.
func (a *App) beforeClose(ctx context.Context) bool {
	a.mu.Lock()
	keep := !a.quitting && a.settings.MinimizeToTray
	a.mu.Unlock()
	if keep {
		wruntime.WindowHide(ctx)
	}
	return keep
}

func (a *App) shutdown(ctx context.Context) {
	stopTray()
	wruntime.CleanupNotifications(ctx)
}

func (a *App) showWindow() {
	if a.ctx == nil {
		return
	}
	wruntime.WindowUnminimise(a.ctx)
	wruntime.WindowShow(a.ctx)
	wruntime.WindowSetAlwaysOnTop(a.ctx, true)
	wruntime.WindowSetAlwaysOnTop(a.ctx, false)
}

func (a *App) quit() {
	a.mu.Lock()
	a.quitting = true
	a.mu.Unlock()
	wruntime.Quit(a.ctx)
}

// ---------------------------------------------------------------- methods called by the page (native.js)

func (a *App) Settings() Settings {
	a.mu.Lock()
	defer a.mu.Unlock()
	return a.settings
}

func (a *App) GetSettings() Settings { return a.Settings() }

func (a *App) SetSettings(patch map[string]any) Settings {
	a.mu.Lock()
	s := &a.settings
	for key, v := range patch {
		b, _ := v.(bool)
		switch key {
		case "notifications":
			s.Notifications = b
		case "minimizeToTray":
			s.MinimizeToTray = b
		case "openOnStartup":
			s.OpenOnStartup = b
		case "startMinimized":
			s.StartMinimized = b
		}
	}
	a.saveSettings()
	out := *s
	a.mu.Unlock()
	if err := setLoginItem(out.OpenOnStartup, out.StartMinimized); err != nil {
		log.Println("iniciar com o Windows:", err)
	}
	return out
}

func (a *App) CloseWindow() {
	if a.Settings().MinimizeToTray {
		wruntime.WindowHide(a.ctx)
		return
	}
	a.quit()
}

func (a *App) SessionGet() string {
	b, err := os.ReadFile(a.path("session.bin"))
	if err != nil {
		return ""
	}
	plain, err := unprotect(b)
	if err != nil {
		return ""
	}
	return string(plain)
}

// SessionSet stores the login token encrypted with the Windows user's key, so
// a copy of the profile folder on another account or machine is useless.
func (a *App) SessionSet(token string) bool {
	if token == "" {
		os.Remove(a.path("session.bin"))
		return true
	}
	enc, err := protect([]byte(token))
	if err != nil {
		return false
	}
	return os.WriteFile(a.path("session.bin"), enc, 0o600) == nil
}

func (a *App) SetBadge(n int) {
	if n < 0 {
		n = 0
	}
	a.mu.Lock()
	a.badge = n
	a.mu.Unlock()
	setTrayBadge(n)
}

func (a *App) Notify(title, body, view string) {
	if !a.Settings().Notifications {
		return
	}
	id := make([]byte, 8)
	rand.Read(id)
	key := hex.EncodeToString(id)
	a.mu.Lock()
	a.views[key] = view
	a.mu.Unlock()
	err := wruntime.SendNotification(a.ctx, wruntime.NotificationOptions{
		ID: key, Title: truncate(title, 80), Body: truncate(body, 240),
	})
	if err != nil {
		log.Println("notificaÃ§Ã£o:", err)
	}
}

func truncate(s string, n int) string {
	r := []rune(s)
	if len(r) > n {
		return string(r[:n])
	}
	return s
}

// ---------------------------------------------------------------- smoke test
// `"Babu Tela.exe" --smoke-report=FILE` writes what the page shows to FILE and
// quits; PROSA_SMOKE_TOKEN signs in first.

func smokeReportFile() string {
	for _, arg := range os.Args[1:] {
		if strings.HasPrefix(arg, "--smoke-report=") {
			return strings.TrimPrefix(arg, "--smoke-report=")
		}
	}
	return ""
}

func (a *App) runSmoke(file string) {
	login := ""
	if t := os.Getenv("PROSA_SMOKE_TOKEN"); t != "" {
		b, _ := json.Marshal(t)
		login = fmt.Sprintf("startSession(%s);", b)
	}
	// Sent as one line: the WebView2 bridge drops multi-line scripts.
	wruntime.WindowExecJS(a.ctx, strings.ReplaceAll(`window.__smokeErrors = []; window.addEventListener('error', e => __smokeErrors.push(e.message));`+login+`setTimeout(async () => {
	  const base = { boot: typeof window.__BOOT, go: typeof window.go, runtime: typeof window.runtime, errors: window.__smokeErrors };
	  try {
	  base.test = await (async () => { `+os.Getenv("PROSA_SMOKE_EVAL")+` })();
	  const r = { ...base,
	    native: typeof window.prosaNative, serverUrl: window.prosaNative && window.prosaNative.serverUrl,
	    version: window.prosaNative && window.prosaNative.version,
	    titlebar: !document.getElementById('titlebar').hidden,
	    auth: !document.getElementById('auth').hidden, app: !document.getElementById('app').hidden,
	    heading: document.querySelector('#auth h1')?.textContent || null,
	    user: window.S && S.me && S.me.username, servers: window.S ? S.servers.size : null,
	    messages: document.querySelectorAll('#msgs .msg').length,
	    host: await window.prosaNative.hostStatus(),
	    api: await fetch(window.prosaNative.serverUrl + '/api/version', { signal: AbortSignal.timeout(4000) }).then(r => r.json()).catch(e => String(e)),
	    session: !!(await window.prosaNative.session.get()), plainToken: localStorage.getItem('prosa.token'),
	    update: await window.prosaNative.checkForUpdates(), banner: document.getElementById('banner').textContent,
	  };
	  window.go.main.App.SmokeReport(JSON.stringify(r));
	  } catch (e) {
	    window.go?.main?.App?.SmokeReport(JSON.stringify({ ...base, failed: String(e) }));
	  }
	}, 3000);`, "\n", " "))
	// Never hang a test run.
	time.AfterFunc(45*time.Second, func() { a.SmokeReport(`{"error":"tempo esgotado"}`) })
}

var smokeOnce sync.Once

func (a *App) SmokeReport(report string) {
	smokeOnce.Do(func() {
		os.WriteFile(smokeReportFile(), []byte(report), 0o644)
		a.quit()
	})
}

var _ = buildinfo.Name
