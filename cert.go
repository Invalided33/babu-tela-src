//go:build !public

package main

import (
	"crypto"
	"crypto/ecdsa"
	"crypto/elliptic"
	"crypto/rand"
	"crypto/tls"
	"crypto/x509"
	"encoding/json"
	"encoding/pem"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"time"

	"github.com/go-acme/lego/v4/certcrypto"
	"github.com/go-acme/lego/v4/certificate"
	"github.com/go-acme/lego/v4/lego"
	"github.com/go-acme/lego/v4/providers/dns/duckdns"
	"github.com/go-acme/lego/v4/registration"
)

// HTTPS certificate from Let's Encrypt, proven through DuckDNS (a DNS TXT
// record set with the DuckDNS token) instead of an inbound connection, so it
// works even when the router keeps ports 80 and 443 for itself.
//
// Files in <profile>/acme: account.key and account.json (the Let's Encrypt
// account), <domain>.crt and <domain>.key, and <domain>.fail with the time of
// the last failure (retries wait 10 minutes, well inside Let's Encrypt's limits).

const certRetryWait = 10 * time.Minute

type acmeUser struct {
	Email        string                 `json:"email"`
	Registration *registration.Resource `json:"registration"`
	key          crypto.PrivateKey
}

func (u *acmeUser) GetEmail() string                        { return u.Email }
func (u *acmeUser) GetRegistration() *registration.Resource { return u.Registration }
func (u *acmeUser) GetPrivateKey() crypto.PrivateKey        { return u.key }

// loadCert returns the saved certificate and when it expires.
func loadCert(dir, domain string) (*tls.Certificate, time.Time, error) {
	c, err := tls.LoadX509KeyPair(filepath.Join(dir, domain+".crt"), filepath.Join(dir, domain+".key"))
	if err != nil {
		return nil, time.Time{}, err
	}
	leaf, err := x509.ParseCertificate(c.Certificate[0])
	if err != nil {
		return nil, time.Time{}, err
	}
	return &c, leaf.NotAfter, nil
}

// certRetryIn is how long to wait before asking again after a failure.
func certRetryIn(dir, domain string) time.Duration {
	b, err := os.ReadFile(filepath.Join(dir, domain+".fail"))
	if err != nil {
		return 0
	}
	at, err := strconv.ParseInt(strings.TrimSpace(string(b)), 10, 64)
	if err != nil {
		return 0
	}
	return time.Until(time.Unix(at, 0).Add(certRetryWait))
}

func accountKey(dir string) (crypto.PrivateKey, error) {
	file := filepath.Join(dir, "account.key")
	if b, err := os.ReadFile(file); err == nil {
		if block, _ := pem.Decode(b); block != nil {
			return x509.ParseECPrivateKey(block.Bytes)
		}
	}
	key, err := ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
	if err != nil {
		return nil, err
	}
	der, _ := x509.MarshalECPrivateKey(key)
	err = os.WriteFile(file, pem.EncodeToMemory(&pem.Block{Type: "EC PRIVATE KEY", Bytes: der}), 0o600)
	return key, err
}

// obtainCert asks Let's Encrypt for a certificate for domain, proving control
// of it through DuckDNS. It takes a minute or two.
func obtainCert(dir, domain, duckToken, email string) (*tls.Certificate, error) {
	if err := os.MkdirAll(dir, 0o700); err != nil {
		return nil, err
	}
	fail := func(err error) (*tls.Certificate, error) {
		os.WriteFile(filepath.Join(dir, domain+".fail"), []byte(strconv.FormatInt(time.Now().Unix(), 10)), 0o600)
		return nil, err
	}
	key, err := accountKey(dir)
	if err != nil {
		return fail(err)
	}
	user := &acmeUser{Email: email, key: key}
	accountFile := filepath.Join(dir, "account.json")
	if b, err := os.ReadFile(accountFile); err == nil {
		json.Unmarshal(b, user)
		user.key = key
	}

	cfg := lego.NewConfig(user)
	cfg.Certificate.KeyType = certcrypto.EC256
	client, err := lego.NewClient(cfg)
	if err != nil {
		return fail(err)
	}
	dc := duckdns.NewDefaultConfig()
	dc.Token = duckToken
	dc.PropagationTimeout = 4 * time.Minute
	provider, err := duckdns.NewDNSProviderConfig(dc)
	if err != nil {
		return fail(err)
	}
	if err := client.Challenge.SetDNS01Provider(provider); err != nil {
		return fail(err)
	}
	if user.Registration == nil {
		reg, err := client.Registration.Register(registration.RegisterOptions{TermsOfServiceAgreed: true})
		if err != nil {
			return fail(fmt.Errorf("conta no Let's Encrypt: %w", err))
		}
		user.Registration = reg
		if b, err := json.Marshal(user); err == nil {
			os.WriteFile(accountFile, b, 0o600)
		}
	}
	res, err := client.Certificate.Obtain(certificate.ObtainRequest{Domains: []string{domain}, Bundle: true})
	if err != nil {
		return fail(err)
	}
	if err := os.WriteFile(filepath.Join(dir, domain+".crt"), res.Certificate, 0o600); err != nil {
		return fail(err)
	}
	if err := os.WriteFile(filepath.Join(dir, domain+".key"), res.PrivateKey, 0o600); err != nil {
		return fail(err)
	}
	os.Remove(filepath.Join(dir, domain+".fail"))
	c, err := tls.X509KeyPair(res.Certificate, res.PrivateKey)
	if err != nil {
		return nil, err
	}
	return &c, nil
}

// certificateFor returns a usable certificate: the saved one while it has more
// than 30 days left, otherwise a new one (respecting the retry wait).
func certificateFor(dir, domain, duckToken, email string) (*tls.Certificate, time.Time, error) {
	if c, notAfter, err := loadCert(dir, domain); err == nil && time.Until(notAfter) > 30*24*time.Hour {
		return c, notAfter, nil
	}
	if wait := certRetryIn(dir, domain); wait > 0 {
		return nil, time.Time{}, fmt.Errorf("a última tentativa falhou; tentando de novo em %d min", int(wait.Minutes())+1)
	}
	if duckToken == "" {
		return nil, time.Time{}, errors.New("falta o token do DuckDNS")
	}
	c, err := obtainCert(dir, domain, duckToken, email)
	if err != nil {
		return nil, time.Time{}, err
	}
	leaf, _ := x509.ParseCertificate(c.Certificate[0])
	return c, leaf.NotAfter, nil
}
