// Command babutela-server runs the Babu Tela server on its own, without the
// desktop app (a dedicated machine, or Iniciar servidor.bat).
//
// Settings come from the environment: PROSA_DATA (data folder, default
// "server-data" next to the executable), PORT (3210), HOST (all interfaces),
// TRUST_PROXY=1 behind a reverse proxy, and TURN_URLS / TURN_SECRET /
// TURN_RELAY_ONLY for calls through a TURN server.
package main

import (
	"log"
	"os"
	"os/signal"
	"path/filepath"
	"strconv"

	"babutela/frontend"
	"babutela/internal/buildinfo"
	"babutela/internal/server"
)

func main() {
	data := os.Getenv("PROSA_DATA")
	if data == "" {
		exe, _ := os.Executable()
		data = filepath.Join(filepath.Dir(exe), "server-data")
	}
	port := 3210
	if p, err := strconv.Atoi(os.Getenv("PORT")); err == nil && p > 0 {
		port = p
	}
	srv, err := server.New(server.Options{
		DataDir: data, Host: os.Getenv("HOST"), Port: port, TrustProxy: os.Getenv("TRUST_PROXY") != "",
		Static: frontend.Files, Name: buildinfo.Name, Version: buildinfo.Version,
	})
	if err != nil {
		log.Fatal(err)
	}
	if err := srv.Start(); err != nil {
		if server.IsAddrInUse(err) {
			log.Fatalf("A porta %d já está em uso.", port)
		}
		log.Fatal(err)
	}
	stop := make(chan os.Signal, 1)
	signal.Notify(stop, os.Interrupt)
	<-stop
	srv.Stop()
}
