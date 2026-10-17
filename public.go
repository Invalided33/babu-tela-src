//go:build public

package main

import (
	"errors"
	"io"
	"net"
)

// The public build (-tags public, with main.publicServerURL set) is the one
// your friends install: it talks only to your server, with no built-in
// server and no local relay. These stand in for publichost.go and turn.go.

type publicState struct{}

const (
	turnLocalAddr = "127.0.0.1:3478"
	turnURLLocal  = ""
)

func (a *App) startPublic()       {}
func (a *App) stopPublic()        {}
func (a *App) turnSecret() string { return "" }

func startTURN(net.Listener, string, uint16, uint16) (io.Closer, error) {
	return nil, errors.New("a versão dos amigos não tem relé local")
}

func (a *App) serverURL() string { return a.Settings().ServerURL }

// turnURL is empty: calls use the relay your server announces.
func (a *App) turnURL() string { return "" }

func modeOf(Settings) string { return "public" }
