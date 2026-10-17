//go:build !public

package main

import (
	"crypto/tls"
	"errors"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/url"
	"os"
	"strings"
	"sync"
	"sync/atomic"
	"time"

	"github.com/huin/goupnp/dcps/internetgateway2"
	"github.com/pion/logging"
	"github.com/pion/turn/v4"
)

// Public server on this computer, no VPS: the built-in server is reachable at
// https://<name>.duckdns.org for friends using the public build.
//
//   - DuckDNS keeps the name pointing at this connection's public IP.
//   - Let's Encrypt issues and renews the HTTPS certificate (cert.go).
//   - A TURN relay on the public IP carries every call, from any network.
//   - UPnP (optional) opens the needed ports on the router.
//
// The relay announces the public IP for every allocation. Allocations on this
// same machine (this PC's own calls and friends' calls) would then send to
// their own public IP, which needs the router to support "hairpin" NAT; the
// hairpinConn below keeps that traffic inside the machine instead.

const (
	publicRelayMin = 50200
	publicRelayMax = 50239
)

type check struct {
	OK  bool   `json:"ok"`
	Msg string `json:"msg"`
}

type publicState struct {
	mu       sync.Mutex
	state    string // off | starting | online | error
	ip       string
	checks   map[string]check
	tlsLn    net.Listener // HTTPS on publicHTTPSPort
	turn     *turn.Server
	stop     chan struct{}
	domain   string
	mappings []portMapping
}

func (p *publicState) setCheck(name string, ok bool, msg string) {
	p.mu.Lock()
	if p.checks == nil {
		p.checks = map[string]check{}
	}
	p.checks[name] = check{ok, msg}
	p.mu.Unlock()
}

// normalizeDomain accepts "nome", "nome.duckdns.org" or any full domain.
func normalizeDomain(d string) string {
	d = strings.ToLower(strings.TrimSpace(d))
	d = strings.TrimPrefix(strings.TrimPrefix(d, "https://"), "http://")
	d = strings.Trim(strings.SplitN(d, "/", 2)[0], ".")
	if d != "" && !strings.Contains(d, ".") {
		d += ".duckdns.org"
	}
	return d
}

func publicIPv4() (string, error) {
	c := http.Client{Timeout: 8 * time.Second}
	res, err := c.Get("https://api.ipify.org")
	if err != nil {
		return "", err
	}
	defer res.Body.Close()
	b, _ := io.ReadAll(io.LimitReader(res.Body, 64))
	ip := net.ParseIP(strings.TrimSpace(string(b)))
	if ip == nil || ip.To4() == nil {
		return "", errors.New("resposta inesperada")
	}
	return ip.String(), nil
}

func updateDuckDNS(domain, token, ip string) error {
	sub := strings.TrimSuffix(domain, ".duckdns.org")
	c := http.Client{Timeout: 10 * time.Second}
	res, err := c.Get("https://www.duckdns.org/update?domains=" + url.QueryEscape(sub) + "&token=" + url.QueryEscape(token) + "&ip=" + url.QueryEscape(ip))
	if err != nil {
		return err
	}
	defer res.Body.Close()
	b, _ := io.ReadAll(io.LimitReader(res.Body, 64))
	if strings.TrimSpace(string(b)) != "OK" {
		return errors.New("o DuckDNS recusou: confira o nome e o token")
	}
	return nil
}

// ---------------------------------------------------------------- UPnP

type portMapping struct {
	port  uint16
	proto string
}

type igd interface {
	AddPortMapping(string, uint16, string, uint16, string, bool, string, uint32) error
	DeletePortMapping(string, uint16, string) error
}

func findGateway() (igd, error) {
	if c, _, err := internetgateway2.NewWANIPConnection2Clients(); err == nil && len(c) > 0 {
		return c[0], nil
	}
	if c, _, err := internetgateway2.NewWANIPConnection1Clients(); err == nil && len(c) > 0 {
		return c[0], nil
	}
	if c, _, err := internetgateway2.NewWANPPPConnection1Clients(); err == nil && len(c) > 0 {
		return c[0], nil
	}
	return nil, errors.New("o roteador não respondeu ao UPnP")
}

// HTTPS uses this port instead of 443: many home routers keep 80 and 443 for
// their own admin page and refuse to forward them. The friends' build carries
// the full address, port included.
const publicHTTPSPort = 33443

func neededPorts() []portMapping {
	m := []portMapping{{publicHTTPSPort, "TCP"}, {3478, "TCP"}, {3478, "UDP"}}
	for p := publicRelayMin; p <= publicRelayMax; p++ {
		m = append(m, portMapping{uint16(p), "UDP"})
	}
	return m
}

// openPorts forwards every needed port to this PC, going on past refusals,
// and reports the ones the router refused.
func openPorts(lanIP string) ([]portMapping, error) {
	gw, err := findGateway()
	if err != nil {
		return nil, err
	}
	var done []portMapping
	var refused []string
	for _, m := range neededPorts() {
		if err := gw.AddPortMapping("", m.port, m.proto, m.port, lanIP, true, "Babu Tela", 0); err != nil {
			refused = append(refused, fmt.Sprintf("%d/%s", m.port, m.proto))
			continue
		}
		done = append(done, m)
	}
	if len(refused) > 0 {
		return done, fmt.Errorf("o roteador recusou abrir %s", strings.Join(refused, ", "))
	}
	return done, nil
}

func closePorts(ms []portMapping) {
	if len(ms) == 0 {
		return
	}
	if gw, err := findGateway(); err == nil {
		for _, m := range ms {
			gw.DeletePortMapping("", m.port, m.proto)
		}
	}
}

// ---------------------------------------------------------------- relay

type hairpinConn struct {
	net.PacketConn
	public net.IP
}

var loopback = net.IPv4(127, 0, 0, 1)

func (c hairpinConn) WriteTo(p []byte, addr net.Addr) (int, error) {
	if ua, ok := addr.(*net.UDPAddr); ok && ua.IP.Equal(c.public) {
		addr = &net.UDPAddr{IP: loopback, Port: ua.Port}
	}
	return c.PacketConn.WriteTo(p, addr)
}

func (c hairpinConn) ReadFrom(p []byte) (int, net.Addr, error) {
	n, addr, err := c.PacketConn.ReadFrom(p)
	if ua, ok := addr.(*net.UDPAddr); ok && ua.IP.IsLoopback() {
		addr = &net.UDPAddr{IP: c.public, Port: ua.Port}
	}
	return n, addr, err
}

type hairpinGenerator struct {
	*turn.RelayAddressGeneratorPortRange
	public net.IP
}

func (g hairpinGenerator) AllocatePacketConn(network string, port int) (net.PacketConn, net.Addr, error) {
	conn, addr, err := g.RelayAddressGeneratorPortRange.AllocatePacketConn(network, port)
	if err != nil {
		return nil, nil, err
	}
	return hairpinConn{conn, g.public}, addr, nil
}

func startPublicTURN(publicIP net.IP, secret string, port int) (*turn.Server, error) {
	udp, err := net.ListenPacket("udp4", fmt.Sprintf("0.0.0.0:%d", port))
	if err != nil {
		return nil, err
	}
	tcp, err := net.Listen("tcp4", fmt.Sprintf("0.0.0.0:%d", port))
	if err != nil {
		udp.Close()
		return nil, err
	}
	gen := func() turn.RelayAddressGenerator {
		return hairpinGenerator{&turn.RelayAddressGeneratorPortRange{
			RelayAddress: publicIP, Address: "0.0.0.0", MinPort: publicRelayMin, MaxPort: publicRelayMax,
		}, publicIP}
	}
	logs := logging.NewDefaultLoggerFactory()
	logs.DefaultLogLevel = logging.LogLevelError
	srv, err := turn.NewServer(turn.ServerConfig{
		Realm:             "babutela",
		AuthHandler:       turn.LongTermTURNRESTAuthHandler(secret, logs.NewLogger("turn")),
		LoggerFactory:     logs,
		PacketConnConfigs: []turn.PacketConnConfig{{PacketConn: udp, RelayAddressGenerator: gen()}},
		ListenerConfigs:   []turn.ListenerConfig{{Listener: tcp, RelayAddressGenerator: gen()}},
	})
	if err != nil {
		udp.Close()
		tcp.Close()
	}
	return srv, err
}

// ---------------------------------------------------------------- lifecycle

func (a *App) publicURL() string { return fmt.Sprintf("https://%s:%d", a.pub.domain, publicHTTPSPort) }

// startPublic runs in the background; each step reports into checks.
func (a *App) startPublic() {
	s := a.Settings()
	domain := normalizeDomain(s.PublicDomain)
	a.mu.Lock()
	srv := a.host.srv
	a.mu.Unlock()
	p := &a.pub
	a.stopPublic()
	p.mu.Lock()
	p.state, p.domain, p.ip, p.checks, p.stop = "starting", domain, "", map[string]check{}, make(chan struct{})
	stop := p.stop
	p.mu.Unlock()
	if srv == nil || domain == "" {
		p.setCheck("server", false, "Ligue o servidor embutido e informe o endereço.")
		p.mu.Lock()
		p.state = "error"
		p.mu.Unlock()
		return
	}

	go func() {
		fail := func(name, msg string) {
			p.setCheck(name, false, msg)
			p.mu.Lock()
			p.state = "error"
			p.mu.Unlock()
		}
		ip, err := publicIPv4()
		if err != nil {
			fail("ip", "Não consegui descobrir seu IP público. Confira a internet.")
			return
		}
		p.mu.Lock()
		p.ip = ip
		p.mu.Unlock()
		lan := relayIP().String()

		// DuckDNS: point the name at this IP now and every 30 minutes.
		if token := a.duckToken(); strings.HasSuffix(domain, ".duckdns.org") && token != "" {
			if err := updateDuckDNS(domain, token, ip); err != nil {
				fail("dns", err.Error())
				return
			}
			go func() {
				t := time.NewTicker(30 * time.Minute)
				defer t.Stop()
				for {
					select {
					case <-stop:
						return
					case <-t.C:
						if cur, err := publicIPv4(); err == nil {
							updateDuckDNS(domain, token, cur)
						}
					}
				}
			}()
		}
		if addrs, err := net.LookupHost(domain); err != nil || !containsStr(addrs, ip) {
			p.setCheck("dns", false, fmt.Sprintf("%s ainda não aponta para %s. Se acabou de configurar, espere alguns minutos.", domain, ip))
		} else {
			p.setCheck("dns", true, fmt.Sprintf("%s aponta para %s", domain, ip))
		}

		// Router ports.
		if s.UPnP {
			done, err := openPorts(lan)
			p.mu.Lock()
			p.mappings = done
			p.mu.Unlock()
			if err != nil {
				p.setCheck("ports", false, err.Error()+". Abra as portas manualmente no roteador.")
			} else {
				p.setCheck("ports", true, "Portas abertas no roteador pelo UPnP")
			}
		} else {
			p.setCheck("ports", true, "Portas abertas manualmente no roteador (UPnP desligado)")
		}

		// Call relay on the public IP, replacing the localhost-only one.
		a.mu.Lock()
		if a.localTURN != nil {
			a.localTURN.Close()
			a.localTURN = nil
		}
		a.mu.Unlock()
		relay, err := startPublicTURN(net.ParseIP(ip), a.turnSecret(), 3478)
		if err != nil {
			fail("turn", "Não consegui abrir a porta 3478 das chamadas: "+err.Error())
			return
		}
		a.mu.Lock()
		a.localTURN = relay
		a.mu.Unlock()
		srv.SetTURN(fmt.Sprintf("turn:%s:3478?transport=udp,turn:%s:3478?transport=tcp", domain, domain), a.turnSecret())
		p.mu.Lock()
		p.turn = relay
		p.mu.Unlock()
		p.setCheck("turn", true, "Relé de chamadas no ar (porta 3478)")

		// HTTPS: the saved certificate, or a new one proven through DuckDNS.
		acmeDir := a.path("acme")
		cert, notAfter, err := certificateFor(acmeDir, domain, a.duckToken(), s.PublicEmail)
		if err != nil {
			fail("https", "Não consegui o certificado HTTPS: "+err.Error())
			return
		}
		var current atomic.Pointer[tls.Certificate]
		current.Store(cert)
		ln, err := net.Listen("tcp", fmt.Sprintf(":%d", publicHTTPSPort))
		if err != nil {
			fail("https", fmt.Sprintf("A porta %d está ocupada por outro programa.", publicHTTPSPort))
			return
		}
		tlsLn := tls.NewListener(ln, &tls.Config{
			MinVersion:     tls.VersionTLS12,
			GetCertificate: func(*tls.ClientHelloInfo) (*tls.Certificate, error) { return current.Load(), nil },
		})
		srv.Serve(tlsLn)
		p.mu.Lock()
		p.tlsLn = tlsLn
		p.state = "online"
		p.mu.Unlock()
		p.setCheck("https", true, fmt.Sprintf("Certificado válido até %s (porta %d)", notAfter.Format("02/01/2006"), publicHTTPSPort))

		// Renew in the background when fewer than 30 days remain.
		t := time.NewTicker(12 * time.Hour)
		defer t.Stop()
		for {
			select {
			case <-stop:
				return
			case <-t.C:
				if c, n, err := certificateFor(acmeDir, domain, a.duckToken(), s.PublicEmail); err == nil {
					current.Store(c)
					p.setCheck("https", true, fmt.Sprintf("Certificado válido até %s (porta %d)", n.Format("02/01/2006"), publicHTTPSPort))
				}
			}
		}
	}()
}

func (a *App) stopPublic() {
	p := &a.pub
	p.mu.Lock()
	stop, tlsLn, relay, mappings := p.stop, p.tlsLn, p.turn, p.mappings
	p.stop, p.tlsLn, p.turn, p.mappings = nil, nil, nil, nil
	p.state, p.checks = "off", nil
	p.mu.Unlock()
	if stop != nil {
		close(stop)
	}
	if tlsLn != nil {
		tlsLn.Close()
	}
	if relay != nil {
		relay.Close()
		a.mu.Lock()
		if a.localTURN == io.Closer(relay) {
			a.localTURN = nil
		}
		srv := a.host.srv
		a.mu.Unlock()
		// Back to the localhost-only relay for this PC's own calls.
		if srv != nil {
			srv.SetTURN(turnURLLocal, a.turnSecret())
			if ln, err := net.Listen("tcp", turnLocalAddr); err == nil {
				if r, err := startTURN(ln, a.turnSecret(), 50000, 50099); err == nil {
					a.mu.Lock()
					a.localTURN = r
					a.mu.Unlock()
				} else {
					ln.Close()
				}
			}
		}
	}
	go closePorts(mappings)
}

func containsStr(list []string, v string) bool {
	for _, x := range list {
		if x == v {
			return true
		}
	}
	return false
}

// duckToken is the token saved in Settings, or the one built into your app.
func (a *App) duckToken() string {
	if t := a.secret("duckdns-token.bin"); t != "" {
		return t
	}
	return hostDuckToken
}

// ---------------------------------------------------------------- methods called by the page

func (a *App) PublicStatus() map[string]any {
	s := a.Settings()
	p := &a.pub
	p.mu.Lock()
	defer p.mu.Unlock()
	state := p.state
	if state == "" {
		state = "off"
	}
	checks := map[string]check{}
	for k, v := range p.checks {
		checks[k] = v
	}
	domain := normalizeDomain(s.PublicDomain)
	return map[string]any{
		"enabled": s.Public, "state": state, "domain": domain, "url": fmt.Sprintf("https://%s:%d", domain, publicHTTPSPort), "ip": p.ip,
		"checks": checks, "upnp": s.UPnP, "email": s.PublicEmail,
		"hasToken": a.duckToken() != "",
		"baked":    hostDomain != "", // address and token come built into this app
	}
}

// SetPublic turns the public server on or off. A non-empty token replaces the
// saved DuckDNS token.
func (a *App) SetPublic(enabled bool, domain, token, email string, upnp bool) (map[string]any, error) {
	domain = normalizeDomain(domain)
	if enabled && (domain == "" || strings.ContainsAny(domain, " /\\")) {
		return nil, errors.New("Informe o endereço, por exemplo babutela.duckdns.org")
	}
	if token = strings.TrimSpace(token); token != "" {
		if err := a.setSecret("duckdns-token.bin", token); err != nil {
			return nil, errors.New("Não foi possível guardar o token.")
		}
	}
	if enabled && strings.HasSuffix(domain, ".duckdns.org") && a.duckToken() == "" {
		return nil, errors.New("Cole o token do DuckDNS (aparece no topo de duckdns.org depois de entrar).")
	}
	a.mu.Lock()
	a.settings.Public, a.settings.PublicDomain, a.settings.PublicEmail, a.settings.UPnP = enabled, domain, strings.TrimSpace(email), upnp
	a.saveSettings()
	a.mu.Unlock()
	if enabled {
		a.startPublic()
	} else {
		a.stopPublic()
	}
	return a.PublicStatus(), nil
}

var _ = os.Getenv
