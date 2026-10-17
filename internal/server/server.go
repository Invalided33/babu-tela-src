// Package server is the Babu Tela chat server: REST API, real-time gateway,
// voice signalling and file uploads over one SQLite database. It runs
// standalone (cmd/babutela-server) or inside the desktop app.
package server

import (
	"context"
	"crypto/rand"
	"crypto/sha256"
	"crypto/subtle"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io/fs"
	"log"
	"math/big"
	"net"
	"net/http"
	"os"
	"strings"
	"sync"
	"time"
	"unicode/utf16"

	"golang.org/x/crypto/scrypt"
)

type Options struct {
	DataDir    string // database, uploads and update files
	Host       string // "" listens on every interface
	Port       int
	TrustProxy bool  // take the visitor's IP from X-Forwarded-For (behind Caddy)
	Static     fs.FS // web version of the app, served at /
	Name       string
	Version    string
	// TURN relay for calls, used instead of the TURN_* environment variables
	// when set (the desktop app runs one next to its built-in server).
	TURNURL    string
	TURNSecret string
}

type Server struct {
	opts  Options
	db    *store
	http  *http.Server
	ln    net.Listener
	mu    sync.Mutex // every request and gateway message runs under it, in order
	conns map[int64]map[*conn]bool
	stat  map[int64]string      // status each user picked
	voice map[int64]*room       // voice channel -> participants
	vconn map[int64]*conn       // user -> the connection that is in voice
	lim   map[string]*bucket    // rate limit windows
	fails map[string]*loginFail // wrong passwords per username
	stopc chan struct{}
}

func New(opts Options) (*Server, error) {
	db, err := openStore(opts.DataDir)
	if err != nil {
		return nil, err
	}
	s := &Server{
		opts: opts, db: db,
		conns: map[int64]map[*conn]bool{}, stat: map[int64]string{},
		voice: map[int64]*room{}, vconn: map[int64]*conn{},
		lim: map[string]*bucket{}, fails: map[string]*loginFail{},
	}
	s.http = &http.Server{Handler: s.routes(), ReadHeaderTimeout: 15 * time.Second}
	return s, nil
}

// Start listens on the configured port and serves in the background.
func (s *Server) Start() error {
	ln, err := net.Listen("tcp", net.JoinHostPort(s.opts.Host, fmt.Sprint(s.opts.Port)))
	if err != nil {
		return err
	}
	s.ln = ln
	s.stopc = make(chan struct{})
	go s.janitor()
	go func() {
		if err := s.http.Serve(ln); err != nil && !errors.Is(err, http.ErrServerClosed) {
			log.Println("servidor:", err)
		}
	}()
	log.Printf("%s %s em http://localhost:%d", s.opts.Name, s.opts.Version, s.opts.Port)
	return nil
}

// SetTURN changes the call relay the server hands out (the desktop app
// switches between its localhost relay and the public one).
func (s *Server) SetTURN(url, secret string) {
	s.mu.Lock()
	s.opts.TURNURL, s.opts.TURNSecret = url, secret
	s.mu.Unlock()
}

// Serve also answers on another listener, such as the internet tunnel.
func (s *Server) Serve(ln net.Listener) {
	go s.http.Serve(ln)
}

func (s *Server) Stop() {
	if s.stopc != nil {
		close(s.stopc)
		s.stopc = nil
	}
	s.mu.Lock()
	for _, set := range s.conns {
		for c := range set {
			c.close()
		}
	}
	s.mu.Unlock()
	ctx, cancel := context.WithTimeout(context.Background(), 3*time.Second)
	defer cancel()
	s.http.Shutdown(ctx)
	s.db.db.Close()
}

// IsAddrInUse reports whether Start failed because the port is taken.
func IsAddrInUse(err error) bool {
	if err == nil {
		return false
	}
	msg := err.Error()
	return strings.Contains(msg, "address already in use") || strings.Contains(msg, "Only one usage of each socket address")
}

// ---------------------------------------------------------------- errors and input

type httpError struct {
	status int
	msg    string
}

func fail(status int, msg string) { panic(httpError{status, msg}) }

func now() int64 { return time.Now().UnixMilli() }

// jsLen counts like JavaScript's String.length (UTF-16 units), which the
// earlier server and the app's maxlength attributes use.
func jsLen(s string) int { return len(utf16.Encode([]rune(s))) }

func truncate(s string, n int) string {
	r := []rune(s)
	if len(r) > n {
		return string(r[:n])
	}
	return s
}

// str mirrors String(value ?? ”) for JSON values.
func str(v any) string {
	switch t := v.(type) {
	case nil:
		return ""
	case string:
		return t
	case float64:
		if t == float64(int64(t)) {
			return fmt.Sprint(int64(t))
		}
		return fmt.Sprint(t)
	case bool:
		return fmt.Sprint(t)
	}
	b, _ := json.Marshal(v)
	return string(b)
}

func truthy(v any) bool {
	switch t := v.(type) {
	case nil:
		return false
	case bool:
		return t
	case string:
		return t != ""
	case float64:
		return t != 0
	}
	return true
}

// toInt accepts a whole number given as a JSON number or a numeric string.
func toInt(v any) int64 {
	switch t := v.(type) {
	case float64:
		if t == float64(int64(t)) {
			return int64(t)
		}
	case string:
		s := strings.TrimSpace(t)
		if s == "" {
			return 0
		}
		var n int64
		if _, err := fmt.Sscan(s, &n); err == nil && fmt.Sprint(n) == strings.TrimLeft(s, "+") {
			return n
		}
	case int64:
		return t
	}
	fail(400, "ID inválido")
	return 0
}

func text(v any, max int, label string) string {
	s := strings.TrimSpace(str(v))
	if s == "" || jsLen(s) > max {
		fail(400, fmt.Sprintf("%s precisa ter entre 1 e %d caracteres", label, max))
	}
	return s
}

func randomHex(n int) string {
	b := make([]byte, n)
	rand.Read(b)
	return hex.EncodeToString(b)
}

func pick(list []string) string {
	n, _ := rand.Int(rand.Reader, big.NewInt(int64(len(list))))
	return list[n.Int64()]
}

func hashToken(t string) string {
	h := sha256.Sum256([]byte(t))
	return hex.EncodeToString(h[:])
}

// Same scrypt parameters as Node's crypto.scryptSync defaults, so passwords
// saved by the earlier server still verify.
func passwordHash(password, salt string) []byte {
	k, err := scrypt.Key([]byte(password), []byte(salt), 16384, 8, 1, 64)
	if err != nil {
		panic(err)
	}
	return k
}

func checkPassword(u Row, password any) bool {
	salt := "x"
	if u != nil {
		salt = u.Str("salt")
	}
	given := passwordHash(str(password), salt)
	if u == nil {
		return false
	}
	want, err := hex.DecodeString(u.Str("pass_hash"))
	return err == nil && subtle.ConstantTimeCompare(given, want) == 1
}

// ---------------------------------------------------------------- rate limiting

type bucket struct {
	start int64
	n     int
}

type loginFail struct {
	n     int
	until int64
}

// hit counts one use of key in a fixed window and reports whether it is allowed.
func (s *Server) hit(key string, max int, window time.Duration) bool {
	t := now()
	b := s.lim[key]
	if b == nil || t-b.start > window.Milliseconds() {
		b = &bucket{start: t}
		s.lim[key] = b
	}
	b.n++
	return b.n <= max
}

const tooMany = "Muitas tentativas seguidas. Espere um pouco e tente de novo."

func (s *Server) limit(key string, max int, window time.Duration, msg ...string) {
	if !s.hit(key, max, window) {
		m := tooMany
		if len(msg) > 0 {
			m = msg[0]
		}
		fail(429, m)
	}
}

// janitor drops old rate-limit windows and pings gateway connections.
func (s *Server) janitor() {
	clean := time.NewTicker(10 * time.Minute)
	ping := time.NewTicker(30 * time.Second)
	defer clean.Stop()
	defer ping.Stop()
	stop := s.stopc
	for {
		select {
		case <-stop:
			return
		case <-clean.C:
			s.mu.Lock()
			t := now()
			for k, b := range s.lim {
				if t-b.start > time.Hour.Milliseconds() {
					delete(s.lim, k)
				}
			}
			s.mu.Unlock()
		case <-ping.C:
			s.mu.Lock()
			for _, set := range s.conns {
				for c := range set {
					c.ping()
				}
			}
			s.mu.Unlock()
		}
	}
}

// ---------------------------------------------------------------- sessions

const sessionIdle = 30 * 24 * time.Hour // sessions unused for 30 days expire

func (s *Server) newSession(uid int64) string {
	token := randomHex(32)
	s.db.run("INSERT INTO sessions(token_hash, user_id, created, last_used) VALUES(?, ?, ?, ?)", hashToken(token), uid, now(), now())
	return token
}

func (s *Server) userForToken(token string) int64 {
	if token == "" {
		return 0
	}
	th := hashToken(token)
	row := s.db.get("SELECT user_id, last_used FROM sessions WHERE token_hash = ?", th)
	if row == nil {
		return 0
	}
	t := now()
	if t-row.Int("last_used") > sessionIdle.Milliseconds() {
		s.db.run("DELETE FROM sessions WHERE token_hash = ?", th)
		return 0
	}
	if t-row.Int("last_used") > time.Hour.Milliseconds() {
		s.db.run("UPDATE sessions SET last_used = ? WHERE token_hash = ?", t, th)
	}
	return row.Int("user_id")
}

// dropSockets ends every connection of a user except the one on keepHash.
func (s *Server) dropSockets(uid int64, keepHash string) {
	for c := range s.conns[uid] {
		if keepHash != "" && c.tokenHash == keepHash {
			continue
		}
		c.sendRaw([]byte(`{"t":"AUTH_FAIL"}`))
		c.close()
	}
}

// ---------------------------------------------------------------- request handling

type req struct {
	w         http.ResponseWriter
	r         *http.Request
	body      map[string]any
	uid       int64
	tokenHash string
}

func (q *req) param(name string) int64 { return toInt(q.r.PathValue(name)) }

func (q *req) has(key string) bool {
	_, ok := q.body[key]
	return ok
}

func (s *Server) clientIP(r *http.Request) string {
	if s.opts.TrustProxy {
		if xff := r.Header.Get("X-Forwarded-For"); xff != "" {
			parts := strings.Split(xff, ",")
			return strings.TrimSpace(parts[len(parts)-1])
		}
	}
	host, _, err := net.SplitHostPort(r.RemoteAddr)
	if err != nil {
		return r.RemoteAddr
	}
	return host
}

func writeJSON(w http.ResponseWriter, status int, v any) {
	w.Header().Set("Content-Type", "application/json; charset=utf-8")
	w.WriteHeader(status)
	json.NewEncoder(w).Encode(v)
}

// handle wraps an API handler: parses the JSON body, checks the session when
// auth is set, runs under the server lock and turns fail() into a response.
func (s *Server) handle(auth bool, fn func(q *req) any) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		q := &req{w: w, r: r, body: map[string]any{}}
		if strings.HasPrefix(r.Header.Get("Content-Type"), "application/json") {
			r.Body = http.MaxBytesReader(w, r.Body, 200*1024)
			var body any
			if err := json.NewDecoder(r.Body).Decode(&body); err != nil && err.Error() != "EOF" {
				writeJSON(w, 400, map[string]string{"error": "Pedido inválido"})
				return
			}
			if m, ok := body.(map[string]any); ok {
				q.body = m
			}
		}
		s.mu.Lock()
		defer s.mu.Unlock()
		defer func() {
			if p := recover(); p != nil {
				if e, ok := p.(httpError); ok {
					writeJSON(w, e.status, map[string]string{"error": e.msg})
					return
				}
				log.Println("erro interno:", p)
				writeJSON(w, 500, map[string]string{"error": "Erro interno do servidor"})
			}
		}()
		if auth {
			token := strings.TrimPrefix(r.Header.Get("Authorization"), "Bearer ")
			q.uid = s.userForToken(token)
			if q.uid == 0 {
				writeJSON(w, 401, map[string]string{"error": "Sessão expirada. Entre novamente."})
				return
			}
			q.tokenHash = hashToken(token)
		}
		out := fn(q)
		if out != nil {
			writeJSON(w, 200, out)
		}
	}
}

type obj = map[string]any

func (s *Server) routes() http.Handler {
	mux := http.NewServeMux()
	s.apiRoutes(mux)
	mux.HandleFunc("/api/", func(w http.ResponseWriter, r *http.Request) {
		writeJSON(w, 404, map[string]string{"error": "Rota não encontrada"})
	})
	mux.HandleFunc("GET /gateway", s.gateway)
	mux.Handle("GET /uploads/", http.StripPrefix("/uploads/", uploadsHandler(s.opts.DataDir)))
	mux.Handle("GET /updates/", http.StripPrefix("/updates/", http.FileServer(http.Dir(s.dataPath("updates")))))
	if s.opts.Static != nil {
		mux.Handle("/", http.FileServer(http.FS(s.opts.Static)))
	}
	// The desktop app loads its UI from its own origin, so API calls are
	// cross-origin. Auth is a bearer token rather than a cookie, which keeps
	// this safe.
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		h := w.Header()
		h.Set("Access-Control-Allow-Origin", "*")
		h.Set("Access-Control-Allow-Headers", "Authorization, Content-Type")
		h.Set("Access-Control-Allow-Methods", "GET, POST, PUT, PATCH, DELETE")
		h.Set("X-Content-Type-Options", "nosniff")
		if r.Method == http.MethodOptions {
			w.WriteHeader(204)
			return
		}
		mux.ServeHTTP(w, r)
	})
}

func (s *Server) dataPath(parts ...string) string {
	return strings.Join(append([]string{s.opts.DataDir}, parts...), string(os.PathSeparator))
}
