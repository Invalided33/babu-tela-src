package server

import (
	"crypto/hmac"
	"crypto/rand"
	"crypto/sha1"
	"encoding/base64"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"os"
	"path/filepath"
	"regexp"
	"strings"
	"time"
)

var (
	usernameRe = regexp.MustCompile(`^[a-z0-9_.]{3,20}$`)
	colorRe    = regexp.MustCompile(`(?i)^#[0-9a-f]{6}$`)
	spacesRe   = regexp.MustCompile(`\s+`)
	extCleanRe = regexp.MustCompile(`[^.a-z0-9]`)
)

var reportReasons = map[string]string{"spam": "Spam", "assedio": "Assédio ou ódio", "improprio": "Conteúdo impróprio", "outro": "Outro motivo"}

func pair(x, y int64) (int64, int64) {
	if x < y {
		return x, y
	}
	return y, x
}

func (s *Server) apiRoutes(mux *http.ServeMux) {
	open := func(pattern string, fn func(q *req) any) { mux.HandleFunc(pattern, s.handle(false, fn)) }
	authed := func(pattern string, fn func(q *req) any) { mux.HandleFunc(pattern, s.handle(true, fn)) }

	open("GET /api/version", func(q *req) any { return obj{"name": s.opts.Name, "version": s.opts.Version} })

	// ---- accounts
	open("POST /api/register", func(q *req) any {
		s.limit("register:"+s.clientIP(q.r), 5, time.Hour, "Muitas contas criadas daqui. Tente de novo mais tarde.")
		username := strings.ToLower(strings.TrimSpace(str(q.body["username"])))
		password := str(q.body["password"])
		if !usernameRe.MatchString(username) {
			fail(400, "Usuário inválido: use de 3 a 20 letras, números, _ ou .")
		}
		if n := jsLen(password); n < 6 || n > 200 {
			fail(400, "A senha precisa ter pelo menos 6 caracteres")
		}
		if s.db.get("SELECT 1 FROM users WHERE username = ?", username) != nil {
			fail(409, "Esse nome de usuário já está em uso")
		}
		display := truncate(strings.TrimSpace(str(q.body["displayName"])), 32)
		if display == "" {
			display = username
		}
		salt := randomHex(16)
		id := s.db.insert("INSERT INTO users(username, display_name, pass_hash, salt, color, created) VALUES(?, ?, ?, ?, ?, ?)",
			username, display, fmt.Sprintf("%x", passwordHash(password, salt)), salt, pick(colors), now())
		return obj{"token": s.newSession(id)}
	})

	open("POST /api/login", func(q *req) any {
		s.limit("login:"+s.clientIP(q.r), 30, 10*time.Minute)
		name := strings.ToLower(strings.TrimSpace(str(q.body["username"])))
		if f := s.fails[name]; f != nil && f.until > now() {
			fail(429, "Muitas senhas erradas para essa conta. Tente de novo em alguns minutos.")
		}
		u := s.db.get("SELECT * FROM users WHERE username = ?", name)
		if !checkPassword(u, q.body["password"]) {
			f := s.fails[name]
			if f == nil {
				f = &loginFail{}
				s.fails[name] = f
			}
			f.n++
			if f.n >= 5 {
				f.n, f.until = 0, now()+(15*time.Minute).Milliseconds()
			}
			fail(401, "Usuário ou senha incorretos")
		}
		delete(s.fails, name)
		return obj{"token": s.newSession(u.Int("id"))}
	})

	authed("POST /api/logout", func(q *req) any {
		s.db.run("DELETE FROM sessions WHERE token_hash = ?", q.tokenHash)
		return obj{}
	})

	// ICE servers for calls. With a TURN server configured (TURN_URLS and the
	// coturn static-auth-secret in TURN_SECRET), each user gets credentials
	// that expire in 12 hours. TURN_RELAY_ONLY=1 sends all call media through
	// TURN, which hides participants' IP addresses from each other.
	authed("GET /api/ice", func(q *req) any {
		stun := os.Getenv("STUN_URLS")
		if stun == "" {
			stun = "stun:stun.l.google.com:19302"
		}
		servers := []obj{{"urls": strings.Split(stun, ",")}}
		turn, secret := os.Getenv("TURN_URLS"), os.Getenv("TURN_SECRET")
		if s.opts.TURNURL != "" {
			turn, secret = s.opts.TURNURL, s.opts.TURNSecret
		}
		if turn != "" && secret != "" {
			username := fmt.Sprintf("%d:%d", time.Now().Unix()+12*3600, q.uid)
			mac := hmac.New(sha1.New, []byte(secret))
			mac.Write([]byte(username))
			servers = append(servers, obj{"urls": strings.Split(turn, ","), "username": username, "credential": base64.StdEncoding.EncodeToString(mac.Sum(nil))})
		}
		return obj{"iceServers": servers, "relayOnly": turn != "" && secret != "" && os.Getenv("TURN_RELAY_ONLY") == "1"}
	})

	authed("GET /api/sessions", func(q *req) any {
		return obj{"count": s.db.get("SELECT COUNT(*) AS n FROM sessions WHERE user_id = ?", q.uid)["n"]}
	})

	authed("POST /api/sessions/logout-others", func(q *req) any {
		n := s.db.run("DELETE FROM sessions WHERE user_id = ? AND token_hash != ?", q.uid, q.tokenHash)
		s.dropSockets(q.uid, q.tokenHash)
		return obj{"ended": n}
	})

	authed("PATCH /api/me/password", func(q *req) any {
		s.limit(fmt.Sprint("password:", q.uid), 5, time.Hour)
		u := s.userByID(q.uid)
		if !checkPassword(u, q.body["current"]) {
			fail(403, "A senha atual está incorreta")
		}
		next := str(q.body["password"])
		if n := jsLen(next); n < 6 || n > 200 {
			fail(400, "A nova senha precisa ter pelo menos 6 caracteres")
		}
		salt := randomHex(16)
		s.db.run("UPDATE users SET pass_hash = ?, salt = ? WHERE id = ?", fmt.Sprintf("%x", passwordHash(next, salt)), salt, q.uid)
		// A changed password signs out every other device.
		s.db.run("DELETE FROM sessions WHERE user_id = ? AND token_hash != ?", q.uid, q.tokenHash)
		s.dropSockets(q.uid, q.tokenHash)
		return obj{}
	})

	// Deletes the account and the data tied to it: the user's messages, direct
	// conversations, servers they own and uploaded files.
	authed("DELETE /api/me", func(q *req) any {
		s.limit(fmt.Sprint("delete:", q.uid), 5, time.Hour)
		u := s.userByID(q.uid)
		if !checkPassword(u, q.body["password"]) {
			fail(403, "Senha incorreta")
		}
		uid := q.uid
		s.voiceLeave(uid)
		owned := ids(s.db.all("SELECT id FROM servers WHERE owner_id = ?", uid), "id")
		ownedMembers := map[int64][]int64{}
		for _, sid := range owned {
			ownedMembers[sid] = ids(s.db.all("SELECT user_id FROM members WHERE server_id = ?", sid), "user_id")
		}
		var joined []int64
		for _, sid := range ids(s.db.all("SELECT server_id FROM members WHERE user_id = ?", uid), "server_id") {
			if !contains(owned, sid) {
				joined = append(joined, sid)
			}
		}
		type dm struct{ cid, other int64 }
		var dms []dm
		for _, cid := range ids(s.db.all("SELECT channel_id FROM dm_members WHERE user_id = ?", uid), "channel_id") {
			o := s.db.get("SELECT user_id FROM dm_members WHERE channel_id = ? AND user_id != ?", cid, uid)
			var other int64
			if o != nil {
				other = o.Int("user_id")
			}
			dms = append(dms, dm{cid, other})
		}
		var audience []int64
		for _, id := range s.related(uid) {
			if id != uid {
				audience = append(audience, id)
			}
		}
		var files []string
		if a := u.Str("avatar"); a != "" {
			files = append(files, a)
		}
		for _, r := range s.db.all(`SELECT attachments FROM messages WHERE author_id = ?
		  OR channel_id IN (SELECT id FROM channels WHERE server_id IN (SELECT id FROM servers WHERE owner_id = ?))
		  OR channel_id IN (SELECT channel_id FROM dm_members WHERE user_id = ?)`, uid, uid, uid) {
			var atts []struct {
				URL string `json:"url"`
			}
			json.Unmarshal([]byte(r.Str("attachments")), &atts)
			for _, a := range atts {
				files = append(files, a.URL)
			}
		}
		s.db.tx(func() {
			s.db.run("DELETE FROM servers WHERE owner_id = ?", uid)
			for _, d := range dms {
				s.db.run("DELETE FROM channels WHERE id = ?", d.cid)
			}
			s.db.run("DELETE FROM messages WHERE author_id = ?", uid)
			s.db.run("DELETE FROM reactions WHERE user_id = ?", uid)
			s.db.run("DELETE FROM member_roles WHERE user_id = ?", uid)
			s.db.run("DELETE FROM members WHERE user_id = ?", uid)
			s.db.run("DELETE FROM friends WHERE a = ? OR b = ?", uid, uid)
			s.db.run("DELETE FROM blocks WHERE user_id = ? OR blocked_id = ?", uid, uid)
			s.db.run("DELETE FROM reports WHERE reporter_id = ?", uid)
			s.db.run("DELETE FROM invites WHERE creator_id = ?", uid)
			s.db.run("DELETE FROM bans WHERE user_id = ?", uid)
			s.db.run("DELETE FROM users WHERE id = ?", uid) // sessions go with it
		})
		for _, f := range files {
			if uploadRe.MatchString(f) {
				os.Remove(filepath.Join(s.opts.DataDir, filepath.FromSlash(strings.TrimPrefix(f, "/"))))
			}
		}
		s.dropSockets(uid, "")
		for sid, members := range ownedMembers {
			for _, m := range members {
				if m != uid {
					s.send(m, "SERVER_REMOVE", obj{"id": sid})
				}
			}
		}
		for _, d := range dms {
			if d.other != 0 {
				s.send(d.other, "DM_REMOVE", obj{"id": d.cid})
			}
		}
		for _, id := range audience {
			s.send(id, "USER_DELETED", obj{"userId": uid})
		}
		for _, sid := range joined {
			s.syncServer(sid)
		}
		return obj{}
	})

	authed("PATCH /api/me", func(q *req) any {
		if q.has("displayName") {
			s.db.run("UPDATE users SET display_name = ? WHERE id = ?", text(q.body["displayName"], 32, "O nome"), q.uid)
		}
		if q.has("avatar") {
			av := q.body["avatar"]
			if av != nil {
				a, ok := av.(string)
				if !ok || !uploadRe.MatchString(a) || kindOf(a) != "image" {
					fail(400, "Avatar inválido")
				}
			}
			s.db.run("UPDATE users SET avatar = ? WHERE id = ?", av, q.uid)
		}
		u := pub(s.userByID(q.uid))
		seen := map[int64]bool{}
		for _, id := range append([]int64{q.uid}, s.related(q.uid)...) {
			if !seen[id] {
				seen[id] = true
				s.send(id, "USER_UPDATE", u)
			}
		}
		return u
	})

	mux.HandleFunc("POST /api/upload", s.upload)

	// ---- servers
	authed("POST /api/servers", func(q *req) any {
		s.limit(fmt.Sprint("servers:", q.uid), 10, time.Hour)
		name := text(q.body["name"], 60, "O nome do servidor")
		var sid int64
		s.db.tx(func() {
			sid = s.db.insert("INSERT INTO servers(name, owner_id, color, created) VALUES(?, ?, ?, ?)", name, q.uid, pick(colors), now())
			s.db.run("INSERT INTO members(server_id, user_id, joined) VALUES(?, ?, ?)", sid, q.uid, now())
			s.db.run("INSERT INTO roles(server_id, name, perms, position, is_everyone) VALUES(?, '@todos', ?, 0, 1)", sid, defaultPerms)
			s.db.run("INSERT INTO channels(server_id, name, type, category, position) VALUES(?, 'geral', 'text', 'Canais de texto', 0)", sid)
			s.db.run("INSERT INTO channels(server_id, name, type, category, position) VALUES(?, 'Sala de voz', 'voice', 'Canais de voz', 1)", sid)
		})
		s.syncServer(sid)
		return obj{"id": sid}
	})

	authed("PATCH /api/servers/{sid}", func(q *req) any {
		srv := s.serverFor(q.uid, q.param("sid"))
		s.need(srv, q.uid, pManageServer)
		s.db.run("UPDATE servers SET name = ? WHERE id = ?", text(q.body["name"], 60, "O nome do servidor"), srv.Int("id"))
		s.syncServer(srv.Int("id"))
		return obj{}
	})

	authed("DELETE /api/servers/{sid}", func(q *req) any {
		srv := s.serverFor(q.uid, q.param("sid"))
		if srv.Int("owner_id") != q.uid {
			fail(403, "Só o dono pode excluir o servidor")
		}
		sid := srv.Int("id")
		members := ids(s.db.all("SELECT user_id FROM members WHERE server_id = ?", sid), "user_id")
		for _, cid := range ids(s.db.all("SELECT id FROM channels WHERE server_id = ? AND type = 'voice'", sid), "id") {
			for _, uid := range s.voiceMembers(cid) {
				s.voiceLeave(uid)
			}
		}
		s.db.run("DELETE FROM servers WHERE id = ?", sid)
		for _, m := range members {
			s.send(m, "SERVER_REMOVE", obj{"id": sid})
		}
		return obj{}
	})

	authed("POST /api/servers/{sid}/leave", func(q *req) any {
		srv := s.serverFor(q.uid, q.param("sid"))
		if srv.Int("owner_id") == q.uid {
			fail(400, "O dono não pode sair. Exclua o servidor.")
		}
		s.removeMember(srv.Int("id"), q.uid)
		return obj{}
	})

	// ---- channels
	channelName := func(name any, typ string) string {
		n := text(name, 40, "O nome do canal")
		if typ == "text" {
			return spacesRe.ReplaceAllString(strings.ToLower(n), "-")
		}
		return n
	}
	managedChannel := func(q *req) (Row, Row) {
		a := s.access(q.uid, q.param("cid"))
		if a.srv == nil {
			fail(400, "Canal inválido")
		}
		s.need(a.srv, q.uid, pManageChannels)
		return a.c, a.srv
	}

	authed("POST /api/servers/{sid}/channels", func(q *req) any {
		srv := s.serverFor(q.uid, q.param("sid"))
		s.need(srv, q.uid, pManageChannels)
		typ := "text"
		if q.body["type"] == "voice" {
			typ = "voice"
		}
		pos := s.db.get("SELECT COALESCE(MAX(position), 0) + 1 AS p FROM channels WHERE server_id = ?", srv.Int("id")).Int("p")
		category := truncate(strings.TrimSpace(str(q.body["category"])), 40)
		if category == "" {
			category = map[string]string{"text": "Canais de texto", "voice": "Canais de voz"}[typ]
		}
		id := s.db.insert("INSERT INTO channels(server_id, name, type, category, position) VALUES(?, ?, ?, ?, ?)",
			srv.Int("id"), channelName(q.body["name"], typ), typ, category, pos)
		s.syncServer(srv.Int("id"))
		return obj{"id": id}
	})

	authed("PATCH /api/channels/{cid}", func(q *req) any {
		c, srv := managedChannel(q)
		name, category, topic := c["name"], c["category"], c["topic"]
		if q.has("name") {
			name = channelName(q.body["name"], c.Str("type"))
		}
		if q.has("category") {
			if cat := truncate(strings.TrimSpace(str(q.body["category"])), 40); cat != "" {
				category = cat
			}
		}
		if q.has("topic") {
			topic = truncate(strings.TrimSpace(str(q.body["topic"])), 200)
		}
		s.db.run("UPDATE channels SET name = ?, category = ?, topic = ? WHERE id = ?", name, category, topic, c.Int("id"))
		s.syncServer(srv.Int("id"))
		return obj{}
	})

	authed("DELETE /api/channels/{cid}", func(q *req) any {
		c, srv := managedChannel(q)
		for _, uid := range s.voiceMembers(c.Int("id")) {
			s.voiceLeave(uid)
		}
		s.db.run("DELETE FROM channels WHERE id = ?", c.Int("id"))
		s.syncServer(srv.Int("id"))
		return obj{}
	})

	authed("PUT /api/channels/{cid}/overrides/{rid}", func(q *req) any {
		c, srv := managedChannel(q)
		s.need(srv, q.uid, pManageRoles)
		role := s.db.get("SELECT * FROM roles WHERE id = ? AND server_id = ?", q.param("rid"), srv.Int("id"))
		if role == nil {
			fail(404, "Cargo não encontrado")
		}
		allow := toInt(q.body["allow"]) & channelBits
		deny := toInt(q.body["deny"]) & channelBits &^ allow
		if allow != 0 || deny != 0 {
			s.db.run(`INSERT INTO overrides(channel_id, role_id, allow, deny) VALUES(?, ?, ?, ?)
			  ON CONFLICT(channel_id, role_id) DO UPDATE SET allow = excluded.allow, deny = excluded.deny`, c.Int("id"), role.Int("id"), allow, deny)
		} else {
			s.db.run("DELETE FROM overrides WHERE channel_id = ? AND role_id = ?", c.Int("id"), role.Int("id"))
		}
		// Anyone who just lost access to a voice channel is dropped from it.
		for _, uid := range s.voiceMembers(c.Int("id")) {
			p := s.permsFor(srv, uid, c.Int("id"))
			if p&pView == 0 || p&pConnect == 0 {
				s.voiceLeave(uid)
			}
		}
		s.syncServer(srv.Int("id"))
		return obj{}
	})

	// ---- roles
	// A role manager can never hand out permissions they do not hold themselves.
	roleInput := func(srv Row, uid int64, body map[string]any, current Row) (int64, any) {
		mine := s.permsFor(srv, uid, 0)
		var perms int64
		if v, ok := body["perms"]; ok {
			perms = toInt(v) & pAll
		} else if current != nil {
			perms = current.Int("perms")
		}
		if perms&^mine != 0 {
			fail(403, "Você não pode conceder permissões que não tem")
		}
		if current != nil && current.Int("perms")&^mine != 0 {
			fail(403, "Esse cargo está acima do seu")
		}
		var color any
		if v, ok := body["color"]; ok {
			if c, isStr := v.(string); isStr && colorRe.MatchString(c) {
				color = c
			}
		} else if current != nil {
			color = current["color"]
		}
		return perms, color
	}
	managedRole := func(q *req) (Row, Row) {
		role := s.db.get("SELECT * FROM roles WHERE id = ?", q.param("rid"))
		if role == nil {
			fail(404, "Cargo não encontrado")
		}
		srv := s.serverFor(q.uid, role.Int("server_id"))
		s.need(srv, q.uid, pManageRoles)
		return role, srv
	}

	authed("POST /api/servers/{sid}/roles", func(q *req) any {
		srv := s.serverFor(q.uid, q.param("sid"))
		s.need(srv, q.uid, pManageRoles)
		body := map[string]any{"perms": float64(defaultPerms & s.permsFor(srv, q.uid, 0))}
		for k, v := range q.body {
			body[k] = v
		}
		perms, color := roleInput(srv, q.uid, body, nil)
		pos := s.db.get("SELECT COALESCE(MAX(position), 0) + 1 AS p FROM roles WHERE server_id = ?", srv.Int("id")).Int("p")
		id := s.db.insert("INSERT INTO roles(server_id, name, color, perms, position) VALUES(?, ?, ?, ?, ?)",
			srv.Int("id"), text(q.body["name"], 32, "O nome do cargo"), color, perms, pos)
		s.syncServer(srv.Int("id"))
		return obj{"id": id}
	})

	authed("PATCH /api/roles/{rid}", func(q *req) any {
		role, srv := managedRole(q)
		perms, color := roleInput(srv, q.uid, q.body, role)
		name := role["name"]
		if role.Int("is_everyone") == 0 && q.has("name") {
			name = text(q.body["name"], 32, "O nome do cargo")
		}
		s.db.run("UPDATE roles SET name = ?, color = ?, perms = ? WHERE id = ?", name, color, perms, role.Int("id"))
		s.syncServer(srv.Int("id"))
		return obj{}
	})

	authed("DELETE /api/roles/{rid}", func(q *req) any {
		role, srv := managedRole(q)
		if role.Int("is_everyone") == 1 {
			fail(400, "O cargo @todos não pode ser excluído")
		}
		roleInput(srv, q.uid, map[string]any{}, role)
		s.db.run("DELETE FROM roles WHERE id = ?", role.Int("id"))
		s.syncServer(srv.Int("id"))
		return obj{}
	})

	memberRole := func(add bool) func(q *req) any {
		return func(q *req) any {
			srv := s.serverFor(q.uid, q.param("sid"))
			s.need(srv, q.uid, pManageRoles)
			target := q.param("uid")
			s.serverFor(target, srv.Int("id"))
			role := s.db.get("SELECT * FROM roles WHERE id = ? AND server_id = ? AND is_everyone = 0", q.param("rid"), srv.Int("id"))
			if role == nil {
				fail(404, "Cargo não encontrado")
			}
			roleInput(srv, q.uid, map[string]any{}, role)
			if add {
				s.db.run("INSERT OR IGNORE INTO member_roles(server_id, user_id, role_id) VALUES(?, ?, ?)", srv.Int("id"), target, role.Int("id"))
			} else {
				s.db.run("DELETE FROM member_roles WHERE user_id = ? AND role_id = ?", target, role.Int("id"))
			}
			s.syncServer(srv.Int("id"))
			return obj{}
		}
	}
	authed("PUT /api/servers/{sid}/members/{uid}/roles/{rid}", memberRole(true))
	authed("DELETE /api/servers/{sid}/members/{uid}/roles/{rid}", memberRole(false))

	// ---- moderation
	authed("DELETE /api/servers/{sid}/members/{uid}", func(q *req) any {
		srv := s.serverFor(q.uid, q.param("sid"))
		s.need(srv, q.uid, pKick)
		target := q.param("uid")
		s.serverFor(target, srv.Int("id"))
		if !s.canModerate(srv, q.uid, target) {
			fail(403, "Você não pode expulsar esse membro")
		}
		s.removeMember(srv.Int("id"), target)
		return obj{}
	})

	authed("PUT /api/servers/{sid}/bans/{uid}", func(q *req) any {
		srv := s.serverFor(q.uid, q.param("sid"))
		s.need(srv, q.uid, pBan)
		target := q.param("uid")
		if s.userByID(target) == nil {
			fail(404, "Usuário não encontrado")
		}
		if !s.canModerate(srv, q.uid, target) {
			fail(403, "Você não pode banir esse membro")
		}
		s.db.run("INSERT OR IGNORE INTO bans(server_id, user_id) VALUES(?, ?)", srv.Int("id"), target)
		s.removeMember(srv.Int("id"), target)
		return obj{}
	})

	authed("DELETE /api/servers/{sid}/bans/{uid}", func(q *req) any {
		srv := s.serverFor(q.uid, q.param("sid"))
		s.need(srv, q.uid, pBan)
		s.db.run("DELETE FROM bans WHERE server_id = ? AND user_id = ?", srv.Int("id"), q.param("uid"))
		s.syncServer(srv.Int("id"))
		return obj{}
	})

	// ---- invites
	authed("POST /api/servers/{sid}/invites", func(q *req) any {
		srv := s.serverFor(q.uid, q.param("sid"))
		s.need(srv, q.uid, pInvite)
		s.limit(fmt.Sprint("invite:", q.uid), 30, time.Hour)
		b := make([]byte, 6)
		rand.Read(b)
		code := base64.RawURLEncoding.EncodeToString(b)
		s.db.run("INSERT INTO invites(code, server_id, creator_id, created) VALUES(?, ?, ?, ?)", code, srv.Int("id"), q.uid, now())
		return obj{"code": code}
	})

	authed("POST /api/invites/{code}/join", func(q *req) any {
		s.limit(fmt.Sprint("join:", q.uid), 20, time.Hour)
		parts := strings.Split(strings.TrimSpace(q.r.PathValue("code")), "/")
		inv := s.db.get("SELECT * FROM invites WHERE code = ?", parts[len(parts)-1])
		if inv == nil {
			fail(404, "Convite inválido ou expirado")
		}
		if s.db.get("SELECT 1 FROM bans WHERE server_id = ? AND user_id = ?", inv.Int("server_id"), q.uid) != nil {
			fail(403, "Você foi banido desse servidor")
		}
		s.db.run("INSERT OR IGNORE INTO members(server_id, user_id, joined) VALUES(?, ?, ?)", inv.Int("server_id"), q.uid, now())
		s.syncServer(inv.Int("server_id"))
		s.emitPresence(q.uid)
		return obj{"id": inv["server_id"]}
	})

	// ---- messages
	authed("GET /api/channels/{cid}/messages", func(q *req) any {
		a := s.access(q.uid, q.param("cid"))
		before := int64(1<<53 - 1)
		if b := q.r.URL.Query().Get("before"); b != "" {
			before = toInt(b)
		}
		rows := s.db.all("SELECT * FROM messages WHERE channel_id = ? AND id < ? ORDER BY id DESC LIMIT 51", a.c.Int("id"), before)
		more := len(rows) > 50
		if more {
			rows = rows[:50]
		}
		msgs := []obj{}
		for i := len(rows) - 1; i >= 0; i-- {
			msgs = append(msgs, s.serialize(rows[i]))
		}
		return obj{"more": more, "messages": msgs}
	})

	authed("POST /api/channels/{cid}/messages", func(q *req) any {
		a := s.access(q.uid, q.param("cid"))
		c := a.c
		if c.Str("type") == "voice" {
			fail(400, "Canais de voz não aceitam mensagens")
		}
		if a.perms&pSend == 0 {
			fail(403, "Você não pode enviar mensagens neste canal")
		}
		if c.Str("type") == "dm" {
			if o := s.db.get("SELECT user_id FROM dm_members WHERE channel_id = ? AND user_id != ?", c.Int("id"), q.uid); o != nil && s.blockedBetween(q.uid, o.Int("user_id")) {
				fail(403, "Você não pode enviar mensagens para essa pessoa")
			}
		}
		s.limit(fmt.Sprint("msg:", q.uid), 10, 10*time.Second, "Você está enviando mensagens rápido demais.")
		content := strings.TrimSpace(str(q.body["content"]))
		if jsLen(content) > 4000 {
			fail(400, "Mensagem muito longa (máximo de 4000 caracteres)")
		}
		raw, _ := q.body["attachments"].([]any)
		if len(raw) > 10 {
			raw = raw[:10]
		}
		if len(raw) > 0 && a.perms&pAttach == 0 {
			fail(403, "Você não pode enviar arquivos neste canal")
		}
		attachments := []obj{}
		for _, item := range raw {
			at, _ := item.(map[string]any)
			url := str(at["url"])
			if !uploadRe.MatchString(url) {
				fail(400, "Anexo inválido")
			}
			if _, err := os.Stat(filepath.Join(s.opts.DataDir, filepath.FromSlash(strings.TrimPrefix(url, "/")))); err != nil {
				fail(400, "Anexo inválido")
			}
			name := truncate(str(at["name"]), 200)
			if name == "" {
				name = "arquivo"
			}
			size, _ := at["size"].(float64)
			attachments = append(attachments, obj{"url": url, "name": name, "size": int64(size), "kind": kindOf(url)})
		}
		if content == "" && len(attachments) == 0 {
			fail(400, "Mensagem vazia")
		}
		var replyTo any
		if truthy(q.body["replyTo"]) {
			if r := s.db.get("SELECT id FROM messages WHERE id = ? AND channel_id = ?", toInt(q.body["replyTo"]), c.Int("id")); r != nil {
				replyTo = r["id"]
			}
		}
		atts, _ := json.Marshal(attachments)
		id := s.db.insert("INSERT INTO messages(channel_id, author_id, content, attachments, reply_to, created) VALUES(?, ?, ?, ?, ?, ?)",
			c.Int("id"), q.uid, content, string(atts), replyTo, now())
		m := s.serialize(s.db.get("SELECT * FROM messages WHERE id = ?", id))
		s.emitChannel(c, "MESSAGE_CREATE", m, 0)
		return m
	})

	authed("PATCH /api/messages/{mid}", func(q *req) any {
		ma := s.messageFor(q.uid, q.param("mid"))
		if ma.m.Int("author_id") != q.uid {
			fail(403, "Só o autor pode editar a mensagem")
		}
		content := strings.TrimSpace(str(q.body["content"]))
		if jsLen(content) > 4000 || (content == "" && ma.m.Str("attachments") == "[]") {
			fail(400, "Conteúdo inválido")
		}
		s.db.run("UPDATE messages SET content = ?, edited = ? WHERE id = ?", content, now(), ma.m.Int("id"))
		s.emitChannel(ma.c, "MESSAGE_UPDATE", s.serialize(s.db.get("SELECT * FROM messages WHERE id = ?", ma.m.Int("id"))), 0)
		return obj{}
	})

	authed("DELETE /api/messages/{mid}", func(q *req) any {
		ma := s.messageFor(q.uid, q.param("mid"))
		if ma.m.Int("author_id") != q.uid && !(ma.c.Str("type") != "dm" && ma.perms&pManageMessages != 0) {
			fail(403, "Você não pode apagar essa mensagem")
		}
		reported := s.db.get("SELECT 1 FROM reports WHERE message_id = ?", ma.m.Int("id")) != nil
		s.db.run("DELETE FROM messages WHERE id = ?", ma.m.Int("id"))
		s.emitChannel(ma.c, "MESSAGE_DELETE", obj{"id": ma.m["id"], "channel_id": ma.c["id"]}, 0)
		if reported {
			s.syncServer(ma.c.Int("server_id"))
		}
		return obj{}
	})

	authed("POST /api/messages/{mid}/report", func(q *req) any {
		ma := s.messageFor(q.uid, q.param("mid"))
		if ma.c.Str("type") == "dm" {
			fail(400, "Em mensagens diretas, use “Bloquear”.")
		}
		if ma.m.Int("author_id") == q.uid {
			fail(400, "Você não pode denunciar a própria mensagem")
		}
		s.limit(fmt.Sprint("report:", q.uid), 20, time.Hour)
		label, ok := reportReasons[str(q.body["reason"])]
		if !ok {
			label = reportReasons["outro"]
		}
		reason := label
		if detail := truncate(strings.TrimSpace(str(q.body["detail"])), 300); detail != "" {
			reason = label + ": " + detail
		}
		s.db.run("INSERT OR IGNORE INTO reports(message_id, server_id, reporter_id, reason, created) VALUES(?, ?, ?, ?, ?)",
			ma.m.Int("id"), ma.c.Int("server_id"), q.uid, reason, now())
		s.syncServer(ma.c.Int("server_id"))
		return obj{}
	})

	authed("DELETE /api/reports/{rid}", func(q *req) any {
		r := s.db.get("SELECT * FROM reports WHERE id = ?", q.param("rid"))
		if r == nil {
			fail(404, "Denúncia não encontrada")
		}
		s.need(s.serverFor(q.uid, r.Int("server_id")), q.uid, pManageMessages)
		s.db.run("DELETE FROM reports WHERE id = ?", r.Int("id"))
		s.syncServer(r.Int("server_id"))
		return obj{}
	})

	react := func(add bool) func(q *req) any {
		return func(q *req) any {
			ma := s.messageFor(q.uid, q.param("mid"))
			emoji := q.r.PathValue("emoji")
			if emoji == "" || jsLen(emoji) > 16 {
				fail(400, "Emoji inválido")
			}
			if add {
				s.db.run("INSERT OR IGNORE INTO reactions(message_id, user_id, emoji) VALUES(?, ?, ?)", ma.m.Int("id"), q.uid, emoji)
			} else {
				s.db.run("DELETE FROM reactions WHERE message_id = ? AND user_id = ? AND emoji = ?", ma.m.Int("id"), q.uid, emoji)
			}
			s.emitChannel(ma.c, "MESSAGE_UPDATE", s.serialize(ma.m), 0)
			return obj{}
		}
	}
	authed("PUT /api/messages/{mid}/reactions/{emoji}", react(true))
	authed("DELETE /api/messages/{mid}/reactions/{emoji}", react(false))

	// ---- friends and direct messages
	authed("POST /api/friends", func(q *req) any {
		name := strings.TrimPrefix(strings.ToLower(strings.TrimSpace(str(q.body["username"]))), "@")
		target := s.db.get("SELECT * FROM users WHERE username = ?", name)
		if target == nil {
			fail(404, "Não encontrei ninguém com esse nome de usuário")
		}
		tid := target.Int("id")
		if tid == q.uid {
			fail(400, "Você não pode adicionar a si mesmo")
		}
		if s.blockedBetween(q.uid, tid) {
			fail(403, "Não foi possível enviar o pedido de amizade")
		}
		s.limit(fmt.Sprint("friend:", q.uid), 20, time.Hour)
		a, b := pair(q.uid, tid)
		existing := s.db.get("SELECT * FROM friends WHERE a = ? AND b = ?", a, b)
		if existing != nil && existing.Str("status") == "accepted" {
			fail(400, "Vocês já são amigos")
		}
		if existing != nil && existing.Int("requester") == q.uid {
			fail(400, "Pedido já enviado")
		}
		if existing != nil {
			s.db.run("UPDATE friends SET status = 'accepted' WHERE a = ? AND b = ?", a, b)
		} else {
			s.db.run("INSERT INTO friends(a, b, requester, status) VALUES(?, ?, ?, 'pending')", a, b, q.uid)
		}
		s.emitFriend(q.uid, tid)
		s.emitFriend(tid, q.uid)
		return obj{}
	})

	authed("PUT /api/friends/{uid}", func(q *req) any {
		other := q.param("uid")
		a, b := pair(q.uid, other)
		f := s.db.get("SELECT * FROM friends WHERE a = ? AND b = ?", a, b)
		if f == nil || f.Str("status") != "pending" || f.Int("requester") == q.uid {
			fail(404, "Pedido não encontrado")
		}
		s.db.run("UPDATE friends SET status = 'accepted' WHERE a = ? AND b = ?", a, b)
		s.emitFriend(q.uid, other)
		s.emitFriend(other, q.uid)
		return obj{}
	})

	authed("DELETE /api/friends/{uid}", func(q *req) any {
		other := q.param("uid")
		a, b := pair(q.uid, other)
		s.db.run("DELETE FROM friends WHERE a = ? AND b = ?", a, b)
		s.emitFriend(q.uid, other)
		s.emitFriend(other, q.uid)
		return obj{}
	})

	authed("POST /api/dms", func(q *req) any {
		other := toInt(q.body["userId"])
		if other == q.uid || s.userByID(other) == nil {
			fail(404, "Usuário não encontrado")
		}
		if e := s.db.get(`SELECT d1.channel_id AS id FROM dm_members d1 JOIN dm_members d2 ON d1.channel_id = d2.channel_id
		  WHERE d1.user_id = ? AND d2.user_id = ?`, q.uid, other); e != nil {
			return s.dmSnapshot(e.Int("id"), q.uid)
		}
		if !contains(s.related(q.uid), other) {
			fail(403, "Vocês precisam ser amigos ou dividir um servidor")
		}
		if s.blockedBetween(q.uid, other) {
			fail(403, "Não é possível conversar com essa pessoa")
		}
		s.limit(fmt.Sprint("dm:", q.uid), 30, time.Hour)
		var cid int64
		s.db.tx(func() {
			cid = s.db.insert("INSERT INTO channels(type) VALUES('dm')")
			s.db.run("INSERT INTO dm_members(channel_id, user_id) VALUES(?, ?), (?, ?)", cid, q.uid, cid, other)
		})
		s.send(q.uid, "DM_CREATE", s.dmSnapshot(cid, q.uid))
		s.send(other, "DM_CREATE", s.dmSnapshot(cid, other))
		return s.dmSnapshot(cid, q.uid)
	})

	// Blocking hides the other person's messages for you, ends any friendship
	// and stops direct messages and friend requests in both directions.
	authed("PUT /api/blocks/{uid}", func(q *req) any {
		other := q.param("uid")
		if other == q.uid || s.userByID(other) == nil {
			fail(404, "Usuário não encontrado")
		}
		s.db.run("INSERT OR IGNORE INTO blocks(user_id, blocked_id) VALUES(?, ?)", q.uid, other)
		a, b := pair(q.uid, other)
		s.db.run("DELETE FROM friends WHERE a = ? AND b = ?", a, b)
		s.emitFriend(q.uid, other)
		s.emitFriend(other, q.uid)
		s.send(q.uid, "BLOCKS", obj{"ids": s.blockList(q.uid)})
		return obj{}
	})

	authed("DELETE /api/blocks/{uid}", func(q *req) any {
		s.db.run("DELETE FROM blocks WHERE user_id = ? AND blocked_id = ?", q.uid, q.param("uid"))
		s.send(q.uid, "BLOCKS", obj{"ids": s.blockList(q.uid)})
		return obj{}
	})
}

// ---------------------------------------------------------------- uploads

const maxUpload = 25 << 20

func (s *Server) upload(w http.ResponseWriter, r *http.Request) {
	token := strings.TrimPrefix(r.Header.Get("Authorization"), "Bearer ")
	s.mu.Lock()
	uid := s.userForToken(token)
	allowed := uid != 0 && s.hit(fmt.Sprint("upload:", uid), 20, time.Minute)
	s.mu.Unlock()
	if uid == 0 {
		writeJSON(w, 401, map[string]string{"error": "Sessão expirada. Entre novamente."})
		return
	}
	if !allowed {
		writeJSON(w, 429, map[string]string{"error": "Muitos arquivos seguidos. Espere um pouco."})
		return
	}
	r.Body = http.MaxBytesReader(w, r.Body, maxUpload+1<<20)
	mr, err := r.MultipartReader()
	if err != nil {
		writeJSON(w, 400, map[string]string{"error": "Falha no envio"})
		return
	}
	for {
		part, err := mr.NextPart()
		if err != nil {
			writeJSON(w, 400, map[string]string{"error": "Nenhum arquivo enviado"})
			return
		}
		if part.FormName() != "file" || part.FileName() == "" {
			continue
		}
		original := part.FileName()
		ext := extCleanRe.ReplaceAllString(strings.ToLower(filepath.Ext(original)), "")
		if len(ext) > 9 {
			ext = ext[:9]
		}
		if len(ext) <= 1 {
			ext = ""
		}
		name := randomHex(16) + ext
		dest := filepath.Join(s.opts.DataDir, "uploads", name)
		f, err := os.Create(dest)
		if err != nil {
			writeJSON(w, 500, map[string]string{"error": "Erro interno do servidor"})
			return
		}
		n, err := io.Copy(f, io.LimitReader(part, maxUpload+1))
		f.Close()
		if err != nil || n > maxUpload {
			os.Remove(dest)
			msg := "Falha no envio"
			if n > maxUpload {
				msg = "Arquivo maior que 25 MB"
			}
			writeJSON(w, 400, map[string]string{"error": msg})
			return
		}
		url := "/uploads/" + name
		writeJSON(w, 200, obj{"url": url, "name": truncate(original, 200), "size": n, "kind": kindOf(url)})
		return
	}
}

// uploadsHandler serves uploaded files. Only known media types render inline;
// everything else downloads, so an uploaded HTML or SVG file can never run in
// the app's origin.
func uploadsHandler(dataDir string) http.Handler {
	files := http.FileServer(http.Dir(filepath.Join(dataDir, "uploads")))
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if kindOf(r.URL.Path) == "file" {
			w.Header().Set("Content-Disposition", "attachment")
		}
		w.Header().Set("Cache-Control", "public, max-age=2592000")
		files.ServeHTTP(w, r)
	})
}
