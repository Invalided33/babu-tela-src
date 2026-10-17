//go:build !public

package main

import (
	"crypto/hmac"
	"crypto/sha1"
	"encoding/base64"
	"fmt"
	"net"
	"testing"
	"time"

	"github.com/pion/turn/v4"
)

// Two participants on the same machine (this PC's own call and a friend's)
// relay through the public relay. Their relay addresses carry the public IP,
// which a router without hairpin NAT would never deliver back; hairpinConn
// must keep that traffic inside the machine.
func TestPublicRelayWithoutHairpin(t *testing.T) {
	public := net.ParseIP("203.0.113.7") // not on this machine on purpose
	const port = 34780
	srv, err := startPublicTURN(public, "segredo", port)
	if err != nil {
		t.Fatal(err)
	}
	defer srv.Close()

	newClient := func() (*turn.Client, net.PacketConn) {
		conn, err := net.ListenPacket("udp4", "127.0.0.1:0")
		if err != nil {
			t.Fatal(err)
		}
		user := fmt.Sprintf("%d:teste", time.Now().Add(time.Hour).Unix())
		mac := hmac.New(sha1.New, []byte("segredo"))
		mac.Write([]byte(user))
		c, err := turn.NewClient(&turn.ClientConfig{
			TURNServerAddr: fmt.Sprintf("127.0.0.1:%d", port), Conn: conn,
			Username: user, Password: base64.StdEncoding.EncodeToString(mac.Sum(nil)), Realm: "babutela",
		})
		if err != nil {
			t.Fatal(err)
		}
		if err := c.Listen(); err != nil {
			t.Fatal(err)
		}
		relay, err := c.Allocate()
		if err != nil {
			t.Fatal(err)
		}
		return c, relay
	}
	ca, a := newClient()
	defer ca.Close()
	cb, b := newClient()
	defer cb.Close()

	if !a.LocalAddr().(*net.UDPAddr).IP.Equal(public) {
		t.Fatalf("o relé devia anunciar o IP público, anunciou %v", a.LocalAddr())
	}
	// Each side writes once so both permissions exist, then A sends for real.
	b.WriteTo([]byte("oi"), a.LocalAddr())
	got := make(chan string, 1)
	go func() {
		buf := make([]byte, 64)
		for {
			n, from, err := b.ReadFrom(buf)
			if err != nil {
				return
			}
			if !from.(*net.UDPAddr).IP.Equal(public) {
				t.Errorf("a origem devia aparecer como o IP público, apareceu %v", from)
			}
			got <- string(buf[:n])
			return
		}
	}()
	for i := 0; i < 20; i++ {
		a.WriteTo([]byte("chamada pelo relé"), b.LocalAddr())
		select {
		case msg := <-got:
			if msg != "chamada pelo relé" {
				t.Fatalf("recebeu %q", msg)
			}
			return
		case <-time.After(200 * time.Millisecond):
		}
	}
	t.Fatal("o pacote não chegou: o tráfego entre relés da mesma máquina não ficou local")
}

func TestNormalizeDomain(t *testing.T) {
	for in, want := range map[string]string{
		"babutela":                      "babutela.duckdns.org",
		"BabuTela.DuckDNS.org":          "babutela.duckdns.org",
		"https://babutela.duckdns.org/": "babutela.duckdns.org",
		"chat.exemplo.com.br":           "chat.exemplo.com.br",
	} {
		if got := normalizeDomain(in); got != want {
			t.Errorf("normalizeDomain(%q) = %q, queria %q", in, got, want)
		}
	}
}
