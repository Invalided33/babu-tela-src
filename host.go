package main

import (
	"encoding/json"
	"fmt"
	"log"
	"net"
	"net/http"
	"os"
	"strconv"
	"time"

	"babutela/frontend"
	"babutela/internal/buildinfo"
	"babutela/internal/server"
)

// hostPort is 3210; PROSA_PORT changes it for tests run next to a real install.
var hostPort = func() int {
	if p, err := strconv.Atoi(os.Getenv("PROSA_PORT")); err == nil && p > 0 {
		return p
	}
	return 3210
}()

// host is the chat server running inside the app ("Hospedar servidor neste
// computador"). If another Babu Tela server already has the port, the app
// uses that one instead.
type host struct {
	srv      *server.Server
	running  bool
	external bool
	err      string
}

func (a *App) startHost() {
	a.mu.Lock()
	defer a.mu.Unlock()
	h := &a.host
	if !embeddedServer() || h.running || h.external {
		return
	}
	h.err = ""
	secret := a.turnSecret()
	srv, err := server.New(server.Options{
		DataDir: a.path("server-data"), Port: hostPort, Static: frontend.Files,
		Name: buildinfo.Name, Version: buildinfo.Version,
		TURNURL: turnURLLocal, TURNSecret: secret,
	})
	if err != nil {
		h.err = err.Error()
		return
	}
	if err := srv.Start(); err != nil {
		srv.Stop()
		switch {
		case server.IsAddrInUse(err) && isOurServerAt(hostPort):
			h.external = true
		case server.IsAddrInUse(err):
			h.err = fmt.Sprintf("A porta %d já está sendo usada por outro programa.", hostPort)
		default:
			h.err = err.Error()
		}
		return
	}
	h.srv, h.running = srv, true
	// The relay for this computer's own calls; friends use the one in tunnel.go.
	if ln, err := net.Listen("tcp", turnLocalAddr); err == nil {
		if relay, err := startTURN(ln, secret, 50000, 50099); err == nil {
			a.localTURN, a.localTURNLn = relay, ln
		} else {
			ln.Close()
		}
	}
}

func (a *App) stopHost() {
	a.stopPublic()
	a.mu.Lock()
	defer a.mu.Unlock()
	h := &a.host
	h.external, h.err = false, ""
	if h.running {
		h.srv.Stop()
		h.srv, h.running = nil, false
	}
	if a.localTURN != nil {
		a.localTURN.Close()
		a.localTURN, a.localTURNLn = nil, nil
	}
}

func (a *App) shutdownHost() { a.stopHost() }

func isOurServerAt(port int) bool {
	client := http.Client{Timeout: 1500 * time.Millisecond}
	res, err := client.Get(fmt.Sprintf("http://127.0.0.1:%d/api/version", port))
	if err != nil {
		return false
	}
	defer res.Body.Close()
	var v struct{ Name string }
	json.NewDecoder(res.Body).Decode(&v)
	// "Prosa" is the app's former name; those servers speak the same protocol.
	return v.Name == buildinfo.Name || v.Name == "Prosa"
}

// HostStatus feeds Configurações → Servidor, including the addresses other
// people on the network can use to connect.
func (a *App) HostStatus() map[string]any {
	a.mu.Lock()
	defer a.mu.Unlock()
	var addresses []string
	ifaces, _ := net.Interfaces()
	for _, iface := range ifaces {
		if iface.Flags&net.FlagUp == 0 || iface.Flags&net.FlagLoopback != 0 {
			continue
		}
		addrs, _ := iface.Addrs()
		for _, addr := range addrs {
			if ipn, ok := addr.(*net.IPNet); ok && ipn.IP.To4() != nil {
				addresses = append(addresses, fmt.Sprintf("http://%s:%d", ipn.IP, hostPort))
			}
		}
	}
	if addresses == nil {
		addresses = []string{}
	}
	var errMsg any
	if a.host.err != "" {
		errMsg = a.host.err
	}
	return map[string]any{
		"running": a.host.running, "external": a.host.external, "error": errMsg,
		"available": embeddedServer(), "enabled": a.settings.HostServer, "port": hostPort,
		"addresses": addresses, "dataDir": a.path("server-data"),
	}
}

func init() { log.SetFlags(log.LstdFlags) }
