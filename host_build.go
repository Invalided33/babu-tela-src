//go:build !public

package main

import "os"

// The host build (the one you run): the page talks to the server on this PC,
// and your own calls use the relay on this PC.

func (a *App) serverURL() string { return a.Settings().ServerURL }

func (a *App) turnURL() string {
	a.mu.Lock()
	defer a.mu.Unlock()
	if a.localTURN != nil {
		return turnURLLocal
	}
	return ""
}

func modeOf(Settings) string { return "host" }

// secret and setSecret keep small secrets (the DuckDNS token) in the profile
// folder, encrypted with the Windows user's key (DPAPI).
func (a *App) secret(name string) string {
	b, err := os.ReadFile(a.path(name))
	if err != nil {
		return ""
	}
	plain, err := unprotect(b)
	if err != nil {
		return ""
	}
	return string(plain)
}

func (a *App) setSecret(name, value string) error {
	if value == "" {
		os.Remove(a.path(name))
		return nil
	}
	enc, err := protect([]byte(value))
	if err != nil {
		return err
	}
	return os.WriteFile(a.path(name), enc, 0o600)
}
