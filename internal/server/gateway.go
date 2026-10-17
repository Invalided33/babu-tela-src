package server

import (
	"encoding/json"
	"log"
	"net/http"
	"time"

	"github.com/gorilla/websocket"
)

var upgrader = websocket.Upgrader{
	ReadBufferSize:  4096,
	WriteBufferSize: 4096,
	// The desktop app connects from its own origin; the session token, not the
	// origin, is what authorizes a connection.
	CheckOrigin: func(r *http.Request) bool { return true },
}

const (
	pongWait   = 70 * time.Second
	writeWait  = 10 * time.Second
	maxPayload = 256 * 1024
)

// conn is one gateway connection. Everything is sent through out, drained by
// a single writer goroutine, so senders never block while holding the lock.
type conn struct {
	ws           *websocket.Conn
	out          chan []byte
	done         chan struct{}
	closed       bool
	uid          int64
	tokenHash    string
	voiceChannel int64
}

func (c *conn) sendRaw(b []byte) {
	if c.closed {
		return
	}
	select {
	case c.out <- b:
	default:
		// A client this far behind is gone; drop it rather than stall everyone.
		c.close()
	}
}

func (c *conn) ping() {
	if !c.closed {
		select {
		case c.out <- nil: // nil asks the writer for a ping frame
		default:
		}
	}
}

func (c *conn) close() {
	if !c.closed {
		c.closed = true
		close(c.done)
	}
}

func (c *conn) writer() {
	defer c.ws.Close()
	for {
		select {
		case b := <-c.out:
			c.ws.SetWriteDeadline(time.Now().Add(writeWait))
			var err error
			if b == nil {
				err = c.ws.WriteMessage(websocket.PingMessage, nil)
			} else {
				err = c.ws.WriteMessage(websocket.TextMessage, b)
			}
			if err != nil {
				return
			}
		case <-c.done:
			// Flush what was queued (an AUTH_FAIL, say) before closing.
			for {
				select {
				case b := <-c.out:
					if b != nil {
						c.ws.SetWriteDeadline(time.Now().Add(writeWait))
						c.ws.WriteMessage(websocket.TextMessage, b)
					}
				default:
					c.ws.WriteControl(websocket.CloseMessage, websocket.FormatCloseMessage(websocket.CloseNormalClosure, ""), time.Now().Add(time.Second))
					return
				}
			}
		}
	}
}

func (s *Server) send(uid int64, t string, d any) {
	set := s.conns[uid]
	if len(set) == 0 {
		return
	}
	b, err := json.Marshal(obj{"t": t, "d": d})
	if err != nil {
		log.Println("evento:", err)
		return
	}
	for c := range set {
		c.sendRaw(b)
	}
}

func (s *Server) gateway(w http.ResponseWriter, r *http.Request) {
	ws, err := upgrader.Upgrade(w, r, nil)
	if err != nil {
		return
	}
	ws.SetReadLimit(maxPayload)
	ws.SetReadDeadline(time.Now().Add(pongWait))
	ws.SetPongHandler(func(string) error { return ws.SetReadDeadline(time.Now().Add(pongWait)) })
	c := &conn{ws: ws, out: make(chan []byte, 256), done: make(chan struct{})}
	go c.writer()

	// A connection that never identifies is closed after 10 seconds.
	identifyTimer := time.AfterFunc(10*time.Second, func() {
		s.mu.Lock()
		if c.uid == 0 {
			c.close()
		}
		s.mu.Unlock()
	})

	defer func() {
		identifyTimer.Stop()
		s.mu.Lock()
		defer s.mu.Unlock()
		c.close()
		uid := c.uid
		if uid == 0 {
			return
		}
		if s.vconn[uid] == c {
			s.voiceLeave(uid)
		}
		if set := s.conns[uid]; set != nil {
			delete(set, c)
			if len(set) == 0 {
				delete(s.conns, uid)
				s.emitPresence(uid)
			}
		}
	}()

	for {
		_, raw, err := ws.ReadMessage()
		if err != nil {
			return
		}
		ws.SetReadDeadline(time.Now().Add(pongWait))
		var m map[string]any
		if json.Unmarshal(raw, &m) != nil {
			continue
		}
		s.mu.Lock()
		if c.closed {
			s.mu.Unlock()
			return
		}
		s.dispatch(c, m)
		s.mu.Unlock()
	}
}

// dispatch runs one gateway message; fail() inside becomes an ERROR event.
func (s *Server) dispatch(c *conn, m map[string]any) {
	defer func() {
		if p := recover(); p != nil {
			msg := "Erro interno do servidor"
			if e, ok := p.(httpError); ok {
				msg = e.msg
			} else {
				log.Println("gateway:", p)
			}
			b, _ := json.Marshal(obj{"t": "ERROR", "d": obj{"message": msg}})
			c.sendRaw(b)
		}
	}()
	op := str(m["op"])
	if c.uid == 0 {
		if op == "identify" {
			s.identify(c, str(m["token"]))
		}
		return
	}
	switch op {
	case "ping":
		c.sendRaw([]byte(`{"t":"PONG"}`))
	case "typing":
		if !s.hit("typing:"+jsonKey(c.uid), 30, time.Minute) {
			return
		}
		a := s.access(c.uid, toInt(m["channelId"]))
		if a.perms&pSend != 0 {
			s.emitChannel(a.c, "TYPING", obj{"channelId": a.c["id"], "userId": c.uid}, c.uid)
		}
	case "status":
		st := str(m["status"])
		if st != "online" && st != "idle" && st != "dnd" && st != "invisible" {
			return
		}
		s.stat[c.uid] = st
		s.send(c.uid, "STATUS", obj{"status": st})
		s.emitPresence(c.uid)
	case "voice_join":
		a := s.access(c.uid, toInt(m["channelId"]))
		if a.c.Str("type") != "voice" {
			fail(400, "Esse não é um canal de voz")
		}
		if a.perms&pConnect == 0 {
			fail(403, "Você não pode entrar neste canal de voz")
		}
		if old := s.vconn[c.uid]; old != nil {
			if old != c {
				old.sendRaw([]byte(`{"t":"VOICE_KICK"}`))
			}
			s.voiceLeave(c.uid)
		}
		cid := a.c.Int("id")
		if s.voice[cid] == nil {
			s.voice[cid] = &room{states: map[int64]voiceState{}}
		}
		s.voice[cid].set(c.uid, voiceFlags(m))
		s.vconn[c.uid] = c
		c.voiceChannel = cid
		s.emitVoice(a.c)
	case "voice_state":
		if s.vconn[c.uid] != c {
			return
		}
		s.voice[c.voiceChannel].set(c.uid, voiceFlags(m))
		s.emitVoice(s.db.get("SELECT * FROM channels WHERE id = ?", c.voiceChannel))
	case "voice_leave":
		if s.vconn[c.uid] == c {
			s.voiceLeave(c.uid)
		}
	case "signal":
		peer := s.vconn[toInt(m["to"])]
		if s.vconn[c.uid] != c || peer == nil || peer.voiceChannel != c.voiceChannel {
			return
		}
		b, _ := json.Marshal(obj{"t": "SIGNAL", "d": obj{"from": c.uid, "data": m["data"]}})
		peer.sendRaw(b)
	}
}

func voiceFlags(m map[string]any) voiceState {
	st := voiceState{Mute: truthy(m["mute"]), Deaf: truthy(m["deaf"])}
	if v := str(m["video"]); v == "camera" || v == "screen" {
		st.Video = &v
	}
	return st
}

func (s *Server) identify(c *conn, token string) {
	uid := s.userForToken(token)
	if uid == 0 {
		c.sendRaw([]byte(`{"t":"AUTH_FAIL"}`))
		c.close()
		return
	}
	c.uid = uid
	c.tokenHash = hashToken(token)
	first := len(s.conns[uid]) == 0
	if s.conns[uid] == nil {
		s.conns[uid] = map[*conn]bool{}
	}
	s.conns[uid][c] = true
	b, _ := json.Marshal(obj{"t": "READY", "d": s.ready(uid)})
	c.sendRaw(b)
	if first {
		s.emitPresence(uid)
	}
}
