// Babu Tela desktop app: a Wails window around the HTML/JS interface in
// frontend/, with the tray, notifications, encrypted login, updates and the
// built-in chat server written in Go.
package main

import (
	"embed"
	"encoding/json"
	"log"
	"net/http"
	"os"

	"github.com/wailsapp/wails/v2"
	"github.com/wailsapp/wails/v2/pkg/options"
	"github.com/wailsapp/wails/v2/pkg/options/assetserver"
	"github.com/wailsapp/wails/v2/pkg/options/windows"

	"babutela/frontend"
	"babutela/internal/buildinfo"
)

//go:embed build/appicon.png
var appIcon []byte

var _ embed.FS // keeps the embed import for the directive above

// publicServerURL is set at build time for the public build
// (-ldflags "-X main.publicServerURL=https://..."): the app then connects to
// that server and leaves the built-in one out.
var publicServerURL string

// Your own (host) build gets the public address and DuckDNS token at build
// time from build/host-config.json (see build/release.ps1), so it sets itself
// up with nothing to fill in. They are never put in the friends' build.
var hostDomain, hostDuckToken, hostEmail string

func main() {
	app := NewApp()
	defer app.shutdownHost()
	// The app always hosts the server; with an address configured it is also
	// the public server friends connect to (publichost.go).
	if embeddedServer() {
		app.startHost()
		if app.settings.Public {
			app.startPublic()
		}
	}

	err := wails.Run(&options.App{
		Title:            buildinfo.Name,
		Width:            1240,
		Height:           780,
		MinWidth:         760,
		MinHeight:        480,
		Frameless:        true,
		StartHidden:      app.startHidden(),
		BackgroundColour: &options.RGBA{R: 13, G: 12, B: 29, A: 255},
		AssetServer: &assetserver.Options{
			Assets:     frontend.Files,
			Middleware: app.bootScript,
		},
		OnStartup:     app.startup,
		OnDomReady:    app.domReady,
		OnBeforeClose: app.beforeClose,
		OnShutdown:    app.shutdown,
		Bind:          []interface{}{app},
		SingleInstanceLock: &options.SingleInstanceLock{
			UniqueId:               app.instanceID(),
			OnSecondInstanceLaunch: func(options.SecondInstanceData) { app.showWindow() },
		},
		Windows: &windows.Options{
			WebviewUserDataPath: app.path("webview"),
			Theme:               windows.Dark,
		},
	})
	if err != nil {
		log.Println(err)
		os.Exit(1)
	}
}

// bootScript serves /config.js with what the page needs before app.js runs:
// the server address and the app version (see frontend/native.js).
func (a *App) bootScript(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path != "/config.js" {
			next.ServeHTTP(w, r)
			return
		}
		boot, _ := json.Marshal(map[string]any{
			"serverUrl": a.serverURL(), "version": buildinfo.Version, "turnUrl": a.turnURL(), "mode": modeOf(a.Settings()),
			"public": publicServerURL != "", // the public build talks only to your own server
		})
		w.Header().Set("Content-Type", "text/javascript; charset=utf-8")
		w.Header().Set("Cache-Control", "no-store")
		w.Write([]byte("window.__BOOT = " + string(boot) + ";\n"))
	})
}
