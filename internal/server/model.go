package server

import (
	"encoding/json"
	"path/filepath"
	"regexp"
	"strings"
)

// ---------------------------------------------------------------- permissions

const (
	pView           = 1
	pSend           = 2
	pManageMessages = 4
	pManageChannels = 8
	pManageRoles    = 16
	pKick           = 32
	pBan            = 64
	pAdmin          = 128
	pInvite         = 256
	pAttach         = 512
	pConnect        = 1024
	pManageServer   = 2048

	pAll         = 4095
	defaultPerms = pView | pSend | pInvite | pAttach | pConnect
	channelBits  = pView | pSend | pAttach | pConnect
)

var colors = []string{"#8590ff", "#a6b0ff", "#c58af9", "#6fa8f5", "#58c7d9", "#f27ba5", "#f5b95c", "#43d39e"}

func (s *Server) memberRoles(sid, uid int64) []Row {
	return s.db.all(`SELECT * FROM roles WHERE server_id = ? AND (is_everyone = 1 OR id IN
	  (SELECT role_id FROM member_roles WHERE server_id = ? AND user_id = ?))`, sid, sid, uid)
}

// permsFor works out a member's permissions in a server, or in one of its
// channels when channelID is not 0: roles first, then the channel's
// @everyone override, then the overrides of the member's other roles.
func (s *Server) permsFor(server Row, uid, channelID int64) int64 {
	if server.Int("owner_id") == uid {
		return pAll
	}
	roles := s.memberRoles(server.Int("id"), uid)
	var p int64
	for _, r := range roles {
		p |= r.Int("perms")
	}
	if p&pAdmin != 0 {
		return pAll
	}
	if channelID != 0 {
		ovs := s.db.all("SELECT * FROM overrides WHERE channel_id = ?", channelID)
		var everyone int64
		mine := map[int64]bool{}
		for _, r := range roles {
			mine[r.Int("id")] = true
			if r.Int("is_everyone") == 1 {
				everyone = r.Int("id")
			}
		}
		for _, o := range ovs {
			if o.Int("role_id") == everyone {
				p = (p &^ o.Int("deny")) | o.Int("allow")
			}
		}
		var allow, deny int64
		for _, o := range ovs {
			if o.Int("role_id") != everyone && mine[o.Int("role_id")] {
				allow |= o.Int("allow")
				deny |= o.Int("deny")
			}
		}
		p = (p &^ deny) | allow
	}
	return p
}

func (s *Server) serverFor(uid, sid int64) Row {
	srv := s.db.get("SELECT * FROM servers WHERE id = ?", sid)
	if srv == nil || s.db.get("SELECT 1 FROM members WHERE server_id = ? AND user_id = ?", sid, uid) == nil {
		fail(404, "Servidor não encontrado")
	}
	return srv
}

func (s *Server) need(server Row, uid, bit int64) {
	if s.permsFor(server, uid, 0)&bit == 0 {
		fail(403, "Você não tem permissão para isso")
	}
}

// canModerate: the owner outranks everyone, and admins outrank members who
// are not admins.
func (s *Server) canModerate(server Row, actor, target int64) bool {
	if target == server.Int("owner_id") || actor == target {
		return false
	}
	if actor == server.Int("owner_id") {
		return true
	}
	return s.permsFor(server, target, 0) != pAll
}

type access struct {
	c     Row
	srv   Row // nil for direct messages
	perms int64
}

func (s *Server) access(uid, cid int64) access {
	c := s.db.get("SELECT * FROM channels WHERE id = ?", cid)
	if c == nil {
		fail(404, "Canal não encontrado")
	}
	if c.Str("type") == "dm" {
		if s.db.get("SELECT 1 FROM dm_members WHERE channel_id = ? AND user_id = ?", cid, uid) == nil {
			fail(404, "Canal não encontrado")
		}
		return access{c: c, perms: pView | pSend | pAttach}
	}
	srv := s.serverFor(uid, c.Int("server_id"))
	perms := s.permsFor(srv, uid, cid)
	if perms&pView == 0 {
		fail(404, "Canal não encontrado")
	}
	return access{c: c, srv: srv, perms: perms}
}

// ---------------------------------------------------------------- users and presence

func pub(u Row) obj {
	if u == nil {
		return nil
	}
	return obj{"id": u["id"], "username": u["username"], "display_name": u["display_name"], "color": u["color"], "avatar": u["avatar"]}
}

func (s *Server) userByID(id int64) Row { return s.db.get("SELECT * FROM users WHERE id = ?", id) }

func (s *Server) statusOf(uid int64) string {
	if len(s.conns[uid]) == 0 {
		return "offline"
	}
	st := s.stat[uid]
	if st == "" {
		st = "online"
	}
	if st == "invisible" {
		return "offline"
	}
	return st
}

func (s *Server) withStatus(u Row) obj {
	o := pub(u)
	if o != nil {
		o["status"] = s.statusOf(u.Int("id"))
	}
	return o
}

// related lists everyone who shares a server, a friendship or a direct
// conversation with uid: the people who see their presence.
func (s *Server) related(uid int64) []int64 {
	return ids(s.db.all(`SELECT m2.user_id AS id FROM members m1 JOIN members m2 ON m1.server_id = m2.server_id WHERE m1.user_id = ?
	  UNION SELECT CASE WHEN a = ? THEN b ELSE a END FROM friends WHERE a = ? OR b = ?
	  UNION SELECT d2.user_id FROM dm_members d1 JOIN dm_members d2 ON d1.channel_id = d2.channel_id WHERE d1.user_id = ?`,
		uid, uid, uid, uid, uid), "id")
}

func contains(list []int64, v int64) bool {
	for _, x := range list {
		if x == v {
			return true
		}
	}
	return false
}

// ---------------------------------------------------------------- snapshots

func (s *Server) serverSnapshot(sid, uid int64) obj {
	srv := s.db.get("SELECT * FROM servers WHERE id = ?", sid)
	perms := s.permsFor(srv, uid, 0)
	members := []obj{}
	for _, u := range s.db.all("SELECT u.* FROM members m JOIN users u ON u.id = m.user_id WHERE m.server_id = ? ORDER BY u.display_name COLLATE NOCASE", sid) {
		m := s.withStatus(u)
		m["roles"] = ids(s.db.all("SELECT role_id FROM member_roles WHERE server_id = ? AND user_id = ?", sid, u.Int("id")), "role_id")
		members = append(members, m)
	}
	voiceMap := obj{}
	channels := []obj{}
	for _, c := range s.db.all("SELECT * FROM channels WHERE server_id = ? ORDER BY position, id", sid) {
		cp := s.permsFor(srv, uid, c.Int("id"))
		if cp&pView == 0 {
			continue
		}
		ch := obj{}
		for k, v := range c {
			ch[k] = v
		}
		ch["perms"] = cp
		ch["overrides"] = orEmpty(s.db.all("SELECT role_id, allow, deny FROM overrides WHERE channel_id = ?", c.Int("id")))
		channels = append(channels, ch)
		if c.Str("type") == "voice" {
			voiceMap[jsonKey(c.Int("id"))] = s.voiceUsers(c.Int("id"))
		}
	}
	bans := []obj{}
	if perms&pBan != 0 {
		for _, u := range s.db.all("SELECT u.* FROM bans b JOIN users u ON u.id = b.user_id WHERE b.server_id = ?", sid) {
			bans = append(bans, pub(u))
		}
	}
	reports := []obj{}
	if perms&pManageMessages != 0 {
		for _, r := range s.db.all("SELECT * FROM reports WHERE server_id = ? ORDER BY id DESC LIMIT 50", sid) {
			m := s.db.get("SELECT * FROM messages WHERE id = ?", r.Int("message_id"))
			var channel any
			if c := s.db.get("SELECT name FROM channels WHERE id = ?", m.Int("channel_id")); c != nil {
				channel = c["name"]
			}
			reports = append(reports, obj{
				"id": r["id"], "reason": r["reason"], "created": r["created"],
				"reporter": pub(s.userByID(r.Int("reporter_id"))), "channel": channel, "message": s.serialize(m),
			})
		}
	}
	out := obj{}
	for k, v := range srv {
		out[k] = v
	}
	out["perms"] = perms
	out["roles"] = orEmpty(s.db.all("SELECT * FROM roles WHERE server_id = ? ORDER BY is_everyone, position DESC", sid))
	out["members"] = members
	out["channels"] = channels
	out["voice"] = voiceMap
	out["bans"] = bans
	out["reports"] = reports
	return out
}

func orEmpty(rows []Row) []Row {
	if rows == nil {
		return []Row{}
	}
	return rows
}

func jsonKey(id int64) string {
	b, _ := json.Marshal(id)
	return string(b)
}

func (s *Server) syncServer(sid int64) {
	for _, uid := range ids(s.db.all("SELECT user_id FROM members WHERE server_id = ?", sid), "user_id") {
		if len(s.conns[uid]) > 0 {
			s.send(uid, "SERVER_UPDATE", s.serverSnapshot(sid, uid))
		}
	}
}

func (s *Server) dmSnapshot(cid, uid int64) obj {
	other := s.db.get("SELECT u.* FROM dm_members d JOIN users u ON u.id = d.user_id WHERE d.channel_id = ? AND d.user_id != ?", cid, uid)
	return obj{"id": cid, "type": "dm", "user": s.withStatus(other)}
}

func (s *Server) friendList(uid int64) []obj {
	out := []obj{}
	for _, f := range s.db.all("SELECT * FROM friends WHERE a = ? OR b = ?", uid, uid) {
		other := f.Int("a")
		if other == uid {
			other = f.Int("b")
		}
		state := "incoming"
		if f.Str("status") == "accepted" {
			state = "accepted"
		} else if f.Int("requester") == uid {
			state = "outgoing"
		}
		out = append(out, obj{"user": s.withStatus(s.userByID(other)), "state": state})
	}
	return out
}

func (s *Server) blockList(uid int64) []int64 {
	return ids(s.db.all("SELECT blocked_id FROM blocks WHERE user_id = ?", uid), "blocked_id")
}

func (s *Server) blockedBetween(x, y int64) bool {
	return s.db.get("SELECT 1 FROM blocks WHERE (user_id = ? AND blocked_id = ?) OR (user_id = ? AND blocked_id = ?)", x, y, y, x) != nil
}

func (s *Server) ready(uid int64) obj {
	user := pub(s.userByID(uid))
	st := s.stat[uid]
	if st == "" {
		st = "online"
	}
	user["status"] = st
	servers := []obj{}
	for _, sid := range ids(s.db.all("SELECT server_id FROM members WHERE user_id = ? ORDER BY joined", uid), "server_id") {
		servers = append(servers, s.serverSnapshot(sid, uid))
	}
	dms := []obj{}
	for _, cid := range ids(s.db.all(`SELECT d.channel_id FROM dm_members d WHERE d.user_id = ?
	  ORDER BY (SELECT MAX(id) FROM messages WHERE channel_id = d.channel_id) DESC`, uid), "channel_id") {
		dms = append(dms, s.dmSnapshot(cid, uid))
	}
	return obj{
		"user": user, "servers": servers, "dms": dms, "friends": s.friendList(uid),
		"blocks": s.blockList(uid), "version": s.opts.Version,
	}
}

// ---------------------------------------------------------------- events

// audience: who should receive events about a channel right now.
func (s *Server) audience(c Row) []int64 {
	if c.Str("type") == "dm" {
		return ids(s.db.all("SELECT user_id FROM dm_members WHERE channel_id = ?", c.Int("id")), "user_id")
	}
	srv := s.db.get("SELECT * FROM servers WHERE id = ?", c.Int("server_id"))
	var out []int64
	for _, uid := range ids(s.db.all("SELECT user_id FROM members WHERE server_id = ?", c.Int("server_id")), "user_id") {
		if len(s.conns[uid]) > 0 && s.permsFor(srv, uid, c.Int("id"))&pView != 0 {
			out = append(out, uid)
		}
	}
	return out
}

func (s *Server) emitChannel(c Row, t string, d any, except int64) {
	for _, uid := range s.audience(c) {
		if uid != except {
			s.send(uid, t, d)
		}
	}
}

func (s *Server) emitPresence(uid int64) {
	d := obj{"userId": uid, "status": s.statusOf(uid)}
	for _, id := range s.related(uid) {
		if id != uid {
			s.send(id, "PRESENCE", d)
		}
	}
}

func (s *Server) emitFriend(uid, other int64) {
	for _, f := range s.friendList(uid) {
		if u, _ := f["user"].(obj); u != nil && toInt64(u["id"]) == other {
			s.send(uid, "FRIEND_UPDATE", f)
			return
		}
	}
	s.send(uid, "FRIEND_REMOVE", obj{"userId": other})
}

func toInt64(v any) int64 {
	if n, ok := v.(int64); ok {
		return n
	}
	return 0
}

// ---------------------------------------------------------------- voice

type voiceState struct {
	Mute  bool    `json:"mute"`
	Deaf  bool    `json:"deaf"`
	Video *string `json:"video"`
}

// room keeps participants in the order they joined.
type room struct {
	order  []int64
	states map[int64]voiceState
}

func (r *room) set(uid int64, st voiceState) {
	if _, ok := r.states[uid]; !ok {
		r.order = append(r.order, uid)
	}
	r.states[uid] = st
}

func (r *room) remove(uid int64) {
	delete(r.states, uid)
	for i, id := range r.order {
		if id == uid {
			r.order = append(r.order[:i], r.order[i+1:]...)
			break
		}
	}
}

func (s *Server) voiceUsers(cid int64) []obj {
	out := []obj{}
	if r := s.voice[cid]; r != nil {
		for _, uid := range r.order {
			st := r.states[uid]
			out = append(out, obj{"userId": uid, "mute": st.Mute, "deaf": st.Deaf, "video": st.Video})
		}
	}
	return out
}

func (s *Server) voiceMembers(cid int64) []int64 {
	if r := s.voice[cid]; r != nil {
		return append([]int64(nil), r.order...)
	}
	return nil
}

func (s *Server) emitVoice(c Row) {
	d := obj{"serverId": c["server_id"], "channelId": c["id"], "users": s.voiceUsers(c.Int("id"))}
	for _, uid := range s.audience(c) {
		s.send(uid, "VOICE_STATE", d)
	}
}

func (s *Server) voiceLeave(uid int64) {
	c := s.vconn[uid]
	if c == nil {
		return
	}
	cid := c.voiceChannel
	delete(s.vconn, uid)
	c.voiceChannel = 0
	if r := s.voice[cid]; r != nil {
		r.remove(uid)
		if len(r.order) == 0 {
			delete(s.voice, cid)
		}
	}
	if ch := s.db.get("SELECT * FROM channels WHERE id = ?", cid); ch != nil {
		s.emitVoice(ch)
	}
}

func (s *Server) removeMember(sid, uid int64) {
	if c := s.vconn[uid]; c != nil && s.db.get("SELECT 1 FROM channels WHERE id = ? AND server_id = ?", c.voiceChannel, sid) != nil {
		s.voiceLeave(uid)
	}
	s.db.run("DELETE FROM member_roles WHERE server_id = ? AND user_id = ?", sid, uid)
	s.db.run("DELETE FROM members WHERE server_id = ? AND user_id = ?", sid, uid)
	s.send(uid, "SERVER_REMOVE", obj{"id": sid})
	s.syncServer(sid)
}

// ---------------------------------------------------------------- messages

var uploadRe = regexp.MustCompile(`^/uploads/[a-f0-9]{32}(\.[a-z0-9]{1,8})?$`)

var kinds = map[string][]string{
	"image": {"png", "jpg", "jpeg", "gif", "webp"},
	"video": {"mp4", "webm"},
	"audio": {"mp3", "ogg", "wav", "m4a"},
}

func kindOf(name string) string {
	ext := strings.TrimPrefix(filepath.Ext(name), ".")
	for _, k := range []string{"image", "video", "audio"} {
		for _, e := range kinds[k] {
			if e == ext {
				return k
			}
		}
	}
	return "file"
}

func (s *Server) serialize(m Row) obj {
	order := []string{}
	grouped := map[string][]int64{}
	for _, r := range s.db.all("SELECT emoji, user_id FROM reactions WHERE message_id = ? ORDER BY rowid", m.Int("id")) {
		e := r.Str("emoji")
		if _, ok := grouped[e]; !ok {
			order = append(order, e)
		}
		grouped[e] = append(grouped[e], r.Int("user_id"))
	}
	reactions := []obj{}
	for _, e := range order {
		reactions = append(reactions, obj{"emoji": e, "users": grouped[e]})
	}
	var reply any
	if m.Int("reply_to") != 0 {
		if r := s.db.get("SELECT id, author_id, content FROM messages WHERE id = ?", m.Int("reply_to")); r != nil {
			reply = obj{"id": r["id"], "author": pub(s.userByID(r.Int("author_id"))), "content": truncate(r.Str("content"), 140)}
		}
	}
	attachments := []any{}
	if a := m.Str("attachments"); a != "" {
		json.Unmarshal([]byte(a), &attachments)
	}
	return obj{
		"id": m["id"], "channel_id": m["channel_id"], "author": pub(s.userByID(m.Int("author_id"))),
		"content": m["content"], "created": m["created"], "edited": m["edited"], "reply": reply,
		"attachments": attachments, "reactions": reactions,
	}
}

type messageAccess struct {
	m Row
	access
}

func (s *Server) messageFor(uid, mid int64) messageAccess {
	m := s.db.get("SELECT * FROM messages WHERE id = ?", mid)
	if m == nil {
		fail(404, "Mensagem não encontrada")
	}
	return messageAccess{m, s.access(uid, m.Int("channel_id"))}
}
