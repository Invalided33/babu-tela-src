package main

import (
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"
	"syscall"
	"time"

	wruntime "github.com/wailsapp/wails/v2/pkg/runtime"

	"babutela/internal/buildinfo"
)

// Updates: the server publishes updates/latest.json
//   {"version": "0.5.1", "file": "Babu Tela-amd64-installer.exe", "sha256": "..."}
// next to the installer (build/publicar-atualizacao.ps1 writes it). The app
// checks at startup, downloads a newer installer, verifies its hash, and
// offers "Reiniciar e instalar", which runs it silently and reopens the app.

type latestInfo struct {
	Version string `json:"version"`
	File    string `json:"file"`
	SHA256  string `json:"sha256"`
}

type pendingUpdate struct {
	version string
	path    string
}

func emitEvent(ctx context.Context, name string, data any) { wruntime.EventsEmit(ctx, name, data) }

func newer(a, b string) bool {
	pa, pb := strings.Split(a, "."), strings.Split(b, ".")
	for i := 0; i < 3; i++ {
		var x, y int
		if i < len(pa) {
			x, _ = strconv.Atoi(pa[i])
		}
		if i < len(pb) {
			y, _ = strconv.Atoi(pb[i])
		}
		if x != y {
			return x > y
		}
	}
	return false
}

// The public build has its own feed (updates/public/), so the host's app and
// the friends' app never install each other's version.
func (a *App) feedURL() string {
	feed := strings.TrimRight(a.serverURL(), "/") + "/updates/"
	if publicServerURL != "" {
		feed += "public/"
	}
	return feed
}

// fetchUpdate downloads and verifies a newer installer, if there is one.
func (a *App) fetchUpdate() (string, error) {
	a.mu.Lock()
	if a.update != nil {
		a.mu.Unlock()
		return "ready", nil
	}
	a.mu.Unlock()
	client := http.Client{Timeout: 10 * time.Second}
	res, err := client.Get(a.feedURL() + "latest.json")
	if err != nil {
		return "", err
	}
	defer res.Body.Close()
	if res.StatusCode == 404 {
		return "none", nil
	}
	var info latestInfo
	if err := json.NewDecoder(res.Body).Decode(&info); err != nil || info.File == "" || len(info.SHA256) != 64 {
		return "", fmt.Errorf("arquivo de atualização inválido")
	}
	if !newer(info.Version, buildinfo.Version) {
		return "none", nil
	}
	dl := http.Client{Timeout: 10 * time.Minute}
	bin, err := dl.Get(a.feedURL() + url.PathEscape(info.File))
	if err != nil {
		return "", err
	}
	defer bin.Body.Close()
	if bin.StatusCode != 200 {
		return "", fmt.Errorf("download falhou (%d)", bin.StatusCode)
	}
	dest := filepath.Join(os.TempDir(), fmt.Sprintf("babutela-update-%s.exe", info.Version))
	f, err := os.Create(dest)
	if err != nil {
		return "", err
	}
	h := sha256.New()
	_, err = io.Copy(io.MultiWriter(f, h), bin.Body)
	f.Close()
	if err != nil || !strings.EqualFold(hex.EncodeToString(h.Sum(nil)), info.SHA256) {
		os.Remove(dest)
		return "", fmt.Errorf("o instalador baixado não confere")
	}
	a.mu.Lock()
	a.update = &pendingUpdate{version: info.Version, path: dest}
	a.mu.Unlock()
	if a.ctx != nil {
		wruntime.EventsEmit(a.ctx, "update:status", map[string]string{"state": "downloaded", "version": info.Version})
	}
	return "ready", nil
}

func (a *App) checkForUpdatesInBackground() {
	time.Sleep(5 * time.Second)
	a.fetchUpdate()
}

func (a *App) CheckForUpdates() string {
	switch status, err := a.fetchUpdate(); {
	case err != nil:
		return "Não foi possível procurar atualizações."
	case status == "ready":
		return "Atualização pronta. Use o botão “Reiniciar e instalar”."
	default:
		return "Você já está na versão mais recente."
	}
}

// InstallUpdate runs the downloaded installer silently, then reopens the app.
func (a *App) InstallUpdate() {
	a.mu.Lock()
	u := a.update
	a.mu.Unlock()
	if u == nil {
		return
	}
	exe, _ := os.Executable()
	// The pause lets this app exit so the installer can replace its files.
	script := fmt.Sprintf(`timeout /t 2 /nobreak >nul & "%s" /S & start "" "%s"`, u.path, exe)
	cmd := exec.Command("cmd", "/c", script)
	cmd.SysProcAttr = &syscall.SysProcAttr{HideWindow: true, CreationFlags: 0x00000008} // DETACHED_PROCESS
	if err := cmd.Start(); err == nil {
		a.quit()
	}
}
