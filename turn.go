//go:build !public

package main

import (
	"crypto/rand"
	"encoding/hex"
	"net"
	"os"
	"strings"

	"github.com/pion/logging"
	"github.com/pion/turn/v4"
)

// The built-in browser cannot reach the embedded Tailscale network, so call
// media goes through a TURN relay that runs inside the hosting app: friends
// reach it through the tunnel (see tunnel.go), the host's own window over
// localhost. Credentials are the time-limited ones /api/ice hands out.

const (
	turnLocalAddr = "127.0.0.1:3478"
	turnURLLocal  = "turn:127.0.0.1:3478?transport=tcp"
	turnURLGuest  = "turn:127.0.0.1:3479?transport=tcp"
)

// turnSecret is shared by the relay and the server that issues credentials.
func (a *App) turnSecret() string {
	file := a.path("turn-secret")
	if b, err := os.ReadFile(file); err == nil && len(b) >= 32 {
		return strings.TrimSpace(string(b))
	}
	buf := make([]byte, 32)
	rand.Read(buf)
	secret := hex.EncodeToString(buf)
	os.WriteFile(file, []byte(secret), 0o600)
	return secret
}

// relayIP is the address relayed media is sent from. The machine's LAN
// address (not 127.0.0.1) lets a relayed participant also reach people who
// joined directly over the local network.
func relayIP() net.IP {
	if c, err := net.Dial("udp", "192.0.2.1:9"); err == nil {
		defer c.Close()
		if ua, ok := c.LocalAddr().(*net.UDPAddr); ok && ua.IP.To4() != nil {
			return ua.IP
		}
	}
	return net.IPv4(127, 0, 0, 1)
}

// startTURN serves TURN over TCP on ln, allocating relay ports in
// [minPort, maxPort] (separate ranges per listener so they never collide).
func startTURN(ln net.Listener, secret string, minPort, maxPort uint16) (*turn.Server, error) {
	logs := logging.NewDefaultLoggerFactory()
	logs.DefaultLogLevel = logging.LogLevelError
	return turn.NewServer(turn.ServerConfig{
		Realm:         "babutela",
		AuthHandler:   turn.LongTermTURNRESTAuthHandler(secret, logs.NewLogger("turn")),
		LoggerFactory: logs,
		ListenerConfigs: []turn.ListenerConfig{{
			Listener: ln,
			RelayAddressGenerator: &turn.RelayAddressGeneratorPortRange{
				RelayAddress: relayIP(), Address: "0.0.0.0", MinPort: minPort, MaxPort: maxPort,
			},
		}},
	})
}
