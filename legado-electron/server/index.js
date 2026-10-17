'use strict';
const http = require('http');
const path = require('path');
const fs = require('fs');
const crypto = require('crypto');
const express = require('express');
const multer = require('multer');
const { WebSocketServer } = require('ws');
const db = require('./db');
const { get, all, run, insert, tx } = db;
const PKG = require('../package.json');

const PORT = Number(process.env.PORT) || 3210;
// On a dedicated server Caddy sits in front: HOST=127.0.0.1 keeps the Node
// port private and TRUST_PROXY=1 makes req.ip the visitor's address (used by
// the rate limits) instead of the proxy's.
const HOST = process.env.HOST || undefined;

// ---------------------------------------------------------------- permissions
const P = {
  VIEW: 1, SEND: 2, MANAGE_MESSAGES: 4, MANAGE_CHANNELS: 8, MANAGE_ROLES: 16, KICK: 32,
  BAN: 64, ADMIN: 128, INVITE: 256, ATTACH: 512, CONNECT: 1024, MANAGE_SERVER: 2048,
};
const ALL = 4095;
const DEFAULT_PERMS = P.VIEW | P.SEND | P.INVITE | P.ATTACH | P.CONNECT;
const CHANNEL_BITS = P.VIEW | P.SEND | P.ATTACH | P.CONNECT;
const COLORS = ['#8590ff', '#a6b0ff', '#c58af9', '#6fa8f5', '#58c7d9', '#f27ba5', '#f5b95c', '#43d39e'];

const now = () => Date.now();
const SESSION_IDLE = 30 * 24 * 3600 * 1000; // sessions unused for 30 days expire
const fail = (status, message) => {
  throw Object.assign(new Error(message), { status });
};
const int = v => {
  const n = Number(v);
  if (!Number.isInteger(n)) fail(400, 'ID inválido');
  return n;
};
const text = (v, max, label) => {
  const s = String(v ?? '').trim();
  if (!s || s.length > max) fail(400, `${label} precisa ter entre 1 e ${max} caracteres`);
  return s;
};
const pick = arr => arr[crypto.randomInt(arr.length)];

function memberRoles(sid, uid) {
  return all(
    `SELECT * FROM roles WHERE server_id = ? AND (is_everyone = 1 OR id IN
       (SELECT role_id FROM member_roles WHERE server_id = ? AND user_id = ?))`,
    sid, sid, uid);
}

function permsFor(server, uid, channelId) {
  if (server.owner_id === uid) return ALL;
  const roles = memberRoles(server.id, uid);
  let p = 0;
  for (const r of roles) p |= r.perms;
  if (p & P.ADMIN) return ALL;
  if (channelId) {
    const ovs = all('SELECT * FROM overrides WHERE channel_id = ?', channelId);
    const everyone = roles.find(r => r.is_everyone);
    const mine = new Set(roles.map(r => r.id));
    const base = ovs.find(o => o.role_id === everyone.id);
    if (base) p = (p & ~base.deny) | base.allow;
    let allow = 0, deny = 0;
    for (const o of ovs) {
      if (o.role_id !== everyone.id && mine.has(o.role_id)) {
        allow |= o.allow;
        deny |= o.deny;
      }
    }
    p = (p & ~deny) | allow;
  }
  return p;
}

function serverFor(uid, sid) {
  const s = get('SELECT * FROM servers WHERE id = ?', sid);
  if (!s || !get('SELECT 1 FROM members WHERE server_id = ? AND user_id = ?', sid, uid)) {
    fail(404, 'Servidor não encontrado');
  }
  return s;
}

function need(server, uid, bit) {
  if (!(permsFor(server, uid) & bit)) fail(403, 'Você não tem permissão para isso');
}

// Only someone who outranks the target may moderate them: the owner outranks
// everyone, and admins outrank members who are not admins.
function canModerate(server, actor, target) {
  if (target === server.owner_id || actor === target) return false;
  if (actor === server.owner_id) return true;
  return permsFor(server, target) !== ALL;
}

function access(uid, cid) {
  const c = get('SELECT * FROM channels WHERE id = ?', cid);
  if (!c) fail(404, 'Canal não encontrado');
  if (c.type === 'dm') {
    if (!get('SELECT 1 FROM dm_members WHERE channel_id = ? AND user_id = ?', cid, uid)) {
      fail(404, 'Canal não encontrado');
    }
    return { c, s: null, perms: P.VIEW | P.SEND | P.ATTACH };
  }
  const s = serverFor(uid, c.server_id);
  const perms = permsFor(s, uid, c.id);
  if (!(perms & P.VIEW)) fail(404, 'Canal não encontrado');
  return { c, s, perms };
}

// ---------------------------------------------------------------- gateway state
const conns = new Map(); // userId -> Set<ws>
const statuses = new Map(); // userId -> chosen status
const voice = new Map(); // channelId -> Map<userId, state>
const voiceConn = new Map(); // userId -> ws

const statusOf = uid => {
  if (!conns.get(uid)?.size) return 'offline';
  const s = statuses.get(uid) || 'online';
  return s === 'invisible' ? 'offline' : s;
};

function send(uid, t, d) {
  const set = conns.get(uid);
  if (!set) return;
  const raw = JSON.stringify({ t, d });
  for (const ws of set) if (ws.readyState === 1) ws.send(raw);
}

const pub = u => u && { id: u.id, username: u.username, display_name: u.display_name, color: u.color, avatar: u.avatar };
const userById = id => get('SELECT * FROM users WHERE id = ?', id);
const withStatus = u => ({ ...pub(u), status: statusOf(u.id) });

function related(uid) {
  return all(
    `SELECT m2.user_id AS id FROM members m1 JOIN members m2 ON m1.server_id = m2.server_id WHERE m1.user_id = ?
     UNION SELECT CASE WHEN a = ? THEN b ELSE a END FROM friends WHERE a = ? OR b = ?
     UNION SELECT d2.user_id FROM dm_members d1 JOIN dm_members d2 ON d1.channel_id = d2.channel_id WHERE d1.user_id = ?`,
    uid, uid, uid, uid, uid).map(r => r.id);
}

function voiceUsers(cid) {
  return [...(voice.get(cid) || [])].map(([userId, s]) => ({ userId, ...s }));
}

function serverSnapshot(sid, uid) {
  const s = get('SELECT * FROM servers WHERE id = ?', sid);
  const perms = permsFor(s, uid);
  const roles = all('SELECT * FROM roles WHERE server_id = ? ORDER BY is_everyone, position DESC', sid);
  const members = all(
    `SELECT u.* FROM members m JOIN users u ON u.id = m.user_id WHERE m.server_id = ? ORDER BY u.display_name COLLATE NOCASE`,
    sid).map(u => ({
    ...withStatus(u),
    roles: all('SELECT role_id FROM member_roles WHERE server_id = ? AND user_id = ?', sid, u.id).map(r => r.role_id),
  }));
  const voiceMap = {};
  const channels = [];
  for (const c of all('SELECT * FROM channels WHERE server_id = ? ORDER BY position, id', sid)) {
    const cp = permsFor(s, uid, c.id);
    if (!(cp & P.VIEW)) continue;
    channels.push({ ...c, perms: cp, overrides: all('SELECT role_id, allow, deny FROM overrides WHERE channel_id = ?', c.id) });
    if (c.type === 'voice') voiceMap[c.id] = voiceUsers(c.id);
  }
  const bans = perms & P.BAN
    ? all('SELECT u.* FROM bans b JOIN users u ON u.id = b.user_id WHERE b.server_id = ?', sid).map(pub)
    : [];
  const reports = perms & P.MANAGE_MESSAGES
    ? all('SELECT * FROM reports WHERE server_id = ? ORDER BY id DESC LIMIT 50', sid).map(r => {
      const m = get('SELECT * FROM messages WHERE id = ?', r.message_id);
      return {
        id: r.id, reason: r.reason, created: r.created, reporter: pub(userById(r.reporter_id)),
        channel: get('SELECT name FROM channels WHERE id = ?', m.channel_id)?.name, message: serialize(m),
      };
    })
    : [];
  return { ...s, perms, roles, members, channels, voice: voiceMap, bans, reports };
}

function syncServer(sid) {
  for (const m of all('SELECT user_id FROM members WHERE server_id = ?', sid)) {
    if (conns.has(m.user_id)) send(m.user_id, 'SERVER_UPDATE', serverSnapshot(sid, m.user_id));
  }
}

function dmSnapshot(cid, uid) {
  const other = get(
    'SELECT u.* FROM dm_members d JOIN users u ON u.id = d.user_id WHERE d.channel_id = ? AND d.user_id != ?', cid, uid);
  return { id: cid, type: 'dm', user: withStatus(other) };
}

function friendList(uid) {
  return all('SELECT * FROM friends WHERE a = ? OR b = ?', uid, uid).map(f => ({
    user: withStatus(userById(f.a === uid ? f.b : f.a)),
    state: f.status === 'accepted' ? 'accepted' : f.requester === uid ? 'outgoing' : 'incoming',
  }));
}

function ready(uid) {
  return {
    user: { ...pub(userById(uid)), status: statuses.get(uid) || 'online' },
    servers: all('SELECT server_id FROM members WHERE user_id = ? ORDER BY joined', uid)
      .map(m => serverSnapshot(m.server_id, uid)),
    dms: all(
      `SELECT d.channel_id FROM dm_members d WHERE d.user_id = ?
       ORDER BY (SELECT MAX(id) FROM messages WHERE channel_id = d.channel_id) DESC`, uid)
      .map(d => dmSnapshot(d.channel_id, uid)),
    friends: friendList(uid),
    blocks: blockList(uid),
    version: PKG.version,
  };
}

function audience(c) {
  if (c.type === 'dm') return all('SELECT user_id FROM dm_members WHERE channel_id = ?', c.id).map(r => r.user_id);
  const s = get('SELECT * FROM servers WHERE id = ?', c.server_id);
  return all('SELECT user_id FROM members WHERE server_id = ?', c.server_id)
    .map(r => r.user_id)
    .filter(uid => conns.has(uid) && permsFor(s, uid, c.id) & P.VIEW);
}

function emitChannel(c, t, d, except) {
  for (const uid of audience(c)) if (uid !== except) send(uid, t, d);
}

function emitPresence(uid) {
  const d = { userId: uid, status: statusOf(uid) };
  for (const id of related(uid)) if (id !== uid) send(id, 'PRESENCE', d);
}

function emitVoice(c) {
  const d = { serverId: c.server_id, channelId: c.id, users: voiceUsers(c.id) };
  for (const uid of audience(c)) send(uid, 'VOICE_STATE', d);
}

function voiceLeave(uid) {
  const ws = voiceConn.get(uid);
  if (!ws) return;
  const cid = ws.voiceChannel;
  voiceConn.delete(uid);
  ws.voiceChannel = null;
  const room = voice.get(cid);
  if (room) {
    room.delete(uid);
    if (!room.size) voice.delete(cid);
  }
  const c = get('SELECT * FROM channels WHERE id = ?', cid);
  if (c) emitVoice(c);
}

function removeMember(sid, uid) {
  const ws = voiceConn.get(uid);
  if (ws && get('SELECT 1 FROM channels WHERE id = ? AND server_id = ?', ws.voiceChannel, sid)) voiceLeave(uid);
  run('DELETE FROM member_roles WHERE server_id = ? AND user_id = ?', sid, uid);
  run('DELETE FROM members WHERE server_id = ? AND user_id = ?', sid, uid);
  send(uid, 'SERVER_REMOVE', { id: sid });
  syncServer(sid);
}

// ---------------------------------------------------------------- messages
const UPLOAD_RE = /^\/uploads\/[a-f0-9]{32}(\.[a-z0-9]{1,8})?$/;
const KINDS = {
  image: ['png', 'jpg', 'jpeg', 'gif', 'webp'],
  video: ['mp4', 'webm'],
  audio: ['mp3', 'ogg', 'wav', 'm4a'],
};
const kindOf = url => {
  const ext = path.extname(url).slice(1);
  return Object.keys(KINDS).find(k => KINDS[k].includes(ext)) || 'file';
};

function serialize(m) {
  const grouped = new Map();
  for (const r of all('SELECT emoji, user_id FROM reactions WHERE message_id = ? ORDER BY rowid', m.id)) {
    if (!grouped.has(r.emoji)) grouped.set(r.emoji, []);
    grouped.get(r.emoji).push(r.user_id);
  }
  let reply = null;
  if (m.reply_to) {
    const r = get('SELECT id, author_id, content FROM messages WHERE id = ?', m.reply_to);
    if (r) reply = { id: r.id, author: pub(userById(r.author_id)), content: r.content.slice(0, 140) };
  }
  return {
    id: m.id, channel_id: m.channel_id, author: pub(userById(m.author_id)), content: m.content,
    created: m.created, edited: m.edited, reply, attachments: JSON.parse(m.attachments || '[]'),
    reactions: [...grouped].map(([emoji, users]) => ({ emoji, users })),
  };
}

function messageFor(uid, mid) {
  const m = get('SELECT * FROM messages WHERE id = ?', mid);
  if (!m) fail(404, 'Mensagem não encontrada');
  return { m, ...access(uid, m.channel_id) };
}

// ---------------------------------------------------------------- http
const app = express();
if (process.env.TRUST_PROXY) app.set('trust proxy', Number(process.env.TRUST_PROXY) || process.env.TRUST_PROXY);
app.disable('x-powered-by');
app.use((req, res, next) => {
  // The desktop client loads its UI from disk, so API calls are cross-origin.
  // Auth is a bearer token rather than a cookie, which keeps this safe.
  res.set({
    'Access-Control-Allow-Origin': '*',
    'Access-Control-Allow-Headers': 'Authorization, Content-Type',
    'Access-Control-Allow-Methods': 'GET, POST, PUT, PATCH, DELETE',
    'X-Content-Type-Options': 'nosniff',
  });
  if (req.method === 'OPTIONS') return res.sendStatus(204);
  next();
});
app.use(express.json({ limit: '200kb' }));
app.use((req, res, next) => {
  req.body ??= {};
  next();
});

// ---- rate limiting: fixed windows kept in memory per key
const buckets = new Map();
function hit(key, max, windowMs) {
  const t = now();
  let b = buckets.get(key);
  if (!b || t - b.start > windowMs) buckets.set(key, (b = { start: t, n: 0 }));
  return ++b.n <= max;
}
function limit(key, max, windowMs, message = 'Muitas tentativas seguidas. Espere um pouco e tente de novo.') {
  if (!hit(key, max, windowMs)) fail(429, message);
}
setInterval(() => {
  const t = now();
  for (const [k, b] of buckets) if (t - b.start > 3600 * 1000) buckets.delete(k);
}, 10 * 60 * 1000).unref();

// Wrong passwords per username: five in a row lock that login for 15 minutes.
const loginFails = new Map();

function checkPassword(u, password) {
  const given = crypto.scryptSync(String(password || ''), u ? u.salt : 'x', 64);
  return !!u && crypto.timingSafeEqual(given, Buffer.from(u.pass_hash, 'hex'));
}

const h = fn => (req, res) => {
  try {
    const out = fn(req, res);
    if (out !== undefined) res.json(out);
  } catch (e) {
    if (e.status) return res.status(e.status).json({ error: e.message });
    console.error(e);
    res.status(500).json({ error: 'Erro interno do servidor' });
  }
};

const hashToken = t => crypto.createHash('sha256').update(t).digest('hex');
function newSession(uid) {
  const token = crypto.randomBytes(32).toString('hex');
  run('INSERT INTO sessions(token_hash, user_id, created, last_used) VALUES(?, ?, ?, ?)', hashToken(token), uid, now(), now());
  return token;
}
function userForToken(token) {
  if (!token) return undefined;
  const th = hashToken(String(token));
  const row = get('SELECT user_id, last_used FROM sessions WHERE token_hash = ?', th);
  if (!row) return undefined;
  const t = now();
  if (t - row.last_used > SESSION_IDLE) {
    run('DELETE FROM sessions WHERE token_hash = ?', th);
    return undefined;
  }
  if (t - row.last_used > 3600 * 1000) run('UPDATE sessions SET last_used = ? WHERE token_hash = ?', t, th);
  return row.user_id;
}

// Ends every connection of a user except the one on `keepHash`.
function dropSockets(uid, keepHash) {
  for (const ws of conns.get(uid) || []) {
    if (ws.tokenHash === keepHash) continue;
    if (ws.readyState === 1) ws.send('{"t":"AUTH_FAIL"}');
    ws.close();
  }
}

const api = express.Router();
app.use('/api', api);

api.get('/version', h(() => ({ name: PKG.productName, version: PKG.version })));

api.post('/register', h(req => {
  limit('register:' + req.ip, 5, 3600 * 1000, 'Muitas contas criadas daqui. Tente de novo mais tarde.');
  const username = String(req.body.username || '').trim().toLowerCase();
  const password = String(req.body.password || '');
  if (!/^[a-z0-9_.]{3,20}$/.test(username)) fail(400, 'Usuário inválido: use de 3 a 20 letras, números, _ ou .');
  if (password.length < 6 || password.length > 200) fail(400, 'A senha precisa ter pelo menos 6 caracteres');
  if (get('SELECT 1 FROM users WHERE username = ?', username)) fail(409, 'Esse nome de usuário já está em uso');
  const display = String(req.body.displayName || '').trim().slice(0, 32) || username;
  const salt = crypto.randomBytes(16).toString('hex');
  const hash = crypto.scryptSync(password, salt, 64).toString('hex');
  const id = insert(
    'INSERT INTO users(username, display_name, pass_hash, salt, color, created) VALUES(?, ?, ?, ?, ?, ?)',
    username, display, hash, salt, pick(COLORS), now());
  return { token: newSession(id) };
}));

api.post('/login', h(req => {
  limit('login:' + req.ip, 30, 10 * 60 * 1000);
  const name = String(req.body.username || '').trim().toLowerCase();
  if ((loginFails.get(name)?.until || 0) > now()) fail(429, 'Muitas senhas erradas para essa conta. Tente de novo em alguns minutos.');
  const u = get('SELECT * FROM users WHERE username = ?', name);
  if (!checkPassword(u, req.body.password)) {
    const f = loginFails.get(name) || { n: 0, until: 0 };
    if (++f.n >= 5) Object.assign(f, { n: 0, until: now() + 15 * 60 * 1000 });
    loginFails.set(name, f);
    fail(401, 'Usuário ou senha incorretos');
  }
  loginFails.delete(name);
  return { token: newSession(u.id) };
}));

api.use((req, res, next) => {
  const uid = userForToken((req.headers.authorization || '').replace(/^Bearer /, ''));
  if (!uid) return res.status(401).json({ error: 'Sessão expirada. Entre novamente.' });
  req.uid = uid;
  req.tokenHash = hashToken((req.headers.authorization || '').replace(/^Bearer /, ''));
  next();
});

api.post('/logout', h(req => {
  run('DELETE FROM sessions WHERE token_hash = ?', req.tokenHash);
  return {};
}));

// ICE servers for calls. With a TURN server configured (TURN_URLS and the
// coturn static-auth-secret in TURN_SECRET), each user gets credentials that
// expire in 12 hours. TURN_RELAY_ONLY=1 sends all call media through TURN,
// which hides participants' IP addresses from each other.
api.get('/ice', h(req => {
  const iceServers = [{ urls: (process.env.STUN_URLS || 'stun:stun.l.google.com:19302').split(',') }];
  const turn = process.env.TURN_URLS, secret = process.env.TURN_SECRET;
  if (turn && secret) {
    const username = `${Math.floor(now() / 1000) + 12 * 3600}:${req.uid}`;
    const credential = crypto.createHmac('sha1', secret).update(username).digest('base64');
    iceServers.push({ urls: turn.split(','), username, credential });
  }
  return { iceServers, relayOnly: !!(turn && secret) && process.env.TURN_RELAY_ONLY === '1' };
}));

api.get('/sessions', h(req => ({ count: get('SELECT COUNT(*) AS n FROM sessions WHERE user_id = ?', req.uid).n })));

api.post('/sessions/logout-others', h(req => {
  const n = run('DELETE FROM sessions WHERE user_id = ? AND token_hash != ?', req.uid, req.tokenHash).changes;
  dropSockets(req.uid, req.tokenHash);
  return { ended: Number(n) };
}));

api.patch('/me/password', h(req => {
  limit('password:' + req.uid, 5, 3600 * 1000);
  const u = userById(req.uid);
  if (!checkPassword(u, req.body.current)) fail(403, 'A senha atual está incorreta');
  const next = String(req.body.password || '');
  if (next.length < 6 || next.length > 200) fail(400, 'A nova senha precisa ter pelo menos 6 caracteres');
  const salt = crypto.randomBytes(16).toString('hex');
  run('UPDATE users SET pass_hash = ?, salt = ? WHERE id = ?', crypto.scryptSync(next, salt, 64).toString('hex'), salt, u.id);
  // A changed password signs out every other device.
  run('DELETE FROM sessions WHERE user_id = ? AND token_hash != ?', u.id, req.tokenHash);
  dropSockets(u.id, req.tokenHash);
  return {};
}));

// Deletes the account and the data tied to it: the user's messages, direct
// conversations, servers they own and uploaded files.
api.delete('/me', h(req => {
  limit('delete:' + req.uid, 5, 3600 * 1000);
  const u = userById(req.uid);
  if (!checkPassword(u, req.body.password)) fail(403, 'Senha incorreta');
  const uid = u.id;
  voiceLeave(uid);
  const owned = all('SELECT id FROM servers WHERE owner_id = ?', uid).map(r => r.id);
  const ownedMembers = owned.map(sid => [sid, all('SELECT user_id FROM members WHERE server_id = ?', sid).map(r => r.user_id)]);
  const joined = all('SELECT server_id FROM members WHERE user_id = ?', uid).map(r => r.server_id).filter(sid => !owned.includes(sid));
  const dms = all('SELECT channel_id FROM dm_members WHERE user_id = ?', uid).map(r => ({
    cid: r.channel_id,
    other: get('SELECT user_id FROM dm_members WHERE channel_id = ? AND user_id != ?', r.channel_id, uid)?.user_id,
  }));
  const audienceIds = related(uid).filter(id => id !== uid);
  const files = u.avatar ? [u.avatar] : [];
  const doomed = all(
    `SELECT attachments FROM messages WHERE author_id = ?
       OR channel_id IN (SELECT id FROM channels WHERE server_id IN (SELECT id FROM servers WHERE owner_id = ?))
       OR channel_id IN (SELECT channel_id FROM dm_members WHERE user_id = ?)`, uid, uid, uid);
  for (const r of doomed) for (const a of JSON.parse(r.attachments || '[]')) files.push(a.url);
  tx(() => {
    run('DELETE FROM servers WHERE owner_id = ?', uid);
    for (const { cid } of dms) run('DELETE FROM channels WHERE id = ?', cid);
    run('DELETE FROM messages WHERE author_id = ?', uid);
    run('DELETE FROM reactions WHERE user_id = ?', uid);
    run('DELETE FROM member_roles WHERE user_id = ?', uid);
    run('DELETE FROM members WHERE user_id = ?', uid);
    run('DELETE FROM friends WHERE a = ? OR b = ?', uid, uid);
    run('DELETE FROM blocks WHERE user_id = ? OR blocked_id = ?', uid, uid);
    run('DELETE FROM reports WHERE reporter_id = ?', uid);
    run('DELETE FROM invites WHERE creator_id = ?', uid);
    run('DELETE FROM bans WHERE user_id = ?', uid);
    run('DELETE FROM users WHERE id = ?', uid); // sessions go with it
  });
  for (const f of files) if (UPLOAD_RE.test(f)) fs.rm(path.join(db.dir, f), { force: true }, () => {});
  dropSockets(uid, null);
  for (const [sid, members] of ownedMembers) for (const m of members) if (m !== uid) send(m, 'SERVER_REMOVE', { id: sid });
  for (const { cid, other } of dms) if (other) send(other, 'DM_REMOVE', { id: cid });
  for (const id of audienceIds) send(id, 'USER_DELETED', { userId: uid });
  for (const sid of joined) syncServer(sid);
  return {};
}));

api.patch('/me', h(req => {
  const { displayName, avatar } = req.body;
  if (displayName !== undefined) {
    run('UPDATE users SET display_name = ? WHERE id = ?', text(displayName, 32, 'O nome'), req.uid);
  }
  if (avatar !== undefined) {
    if (avatar !== null && !(UPLOAD_RE.test(avatar) && kindOf(avatar) === 'image')) fail(400, 'Avatar inválido');
    run('UPDATE users SET avatar = ? WHERE id = ?', avatar, req.uid);
  }
  const u = pub(userById(req.uid));
  for (const id of new Set([req.uid, ...related(req.uid)])) send(id, 'USER_UPDATE', u);
  return u;
}));

// ---- uploads
const upload = multer({
  storage: multer.diskStorage({
    destination: path.join(db.dir, 'uploads'),
    filename(req, file, cb) {
      const ext = path.extname(file.originalname).toLowerCase().replace(/[^.a-z0-9]/g, '').slice(0, 9);
      cb(null, crypto.randomBytes(16).toString('hex') + (ext.length > 1 ? ext : ''));
    },
  }),
  limits: { fileSize: 25 * 1024 * 1024, files: 1 },
});

api.post('/upload', (req, res) => {
  if (!hit('upload:' + req.uid, 20, 60 * 1000)) return res.status(429).json({ error: 'Muitos arquivos seguidos. Espere um pouco.' });
  upload.single('file')(req, res, err => {
    if (err) return res.status(400).json({ error: err.code === 'LIMIT_FILE_SIZE' ? 'Arquivo maior que 25 MB' : 'Falha no envio' });
    if (!req.file) return res.status(400).json({ error: 'Nenhum arquivo enviado' });
    const url = '/uploads/' + req.file.filename;
    const name = Buffer.from(req.file.originalname, 'latin1').toString('utf8').slice(0, 200);
    res.json({ url, name, size: req.file.size, kind: kindOf(url) });
  });
});

// ---- servers
api.post('/servers', h(req => {
  limit('servers:' + req.uid, 10, 3600 * 1000);
  const name = text(req.body.name, 60, 'O nome do servidor');
  const sid = tx(() => {
    const id = insert('INSERT INTO servers(name, owner_id, color, created) VALUES(?, ?, ?, ?)', name, req.uid, pick(COLORS), now());
    run('INSERT INTO members(server_id, user_id, joined) VALUES(?, ?, ?)', id, req.uid, now());
    run("INSERT INTO roles(server_id, name, perms, position, is_everyone) VALUES(?, '@todos', ?, 0, 1)", id, DEFAULT_PERMS);
    run("INSERT INTO channels(server_id, name, type, category, position) VALUES(?, 'geral', 'text', 'Canais de texto', 0)", id);
    run("INSERT INTO channels(server_id, name, type, category, position) VALUES(?, 'Sala de voz', 'voice', 'Canais de voz', 1)", id);
    return id;
  });
  syncServer(sid);
  return { id: sid };
}));

api.patch('/servers/:sid', h(req => {
  const s = serverFor(req.uid, int(req.params.sid));
  need(s, req.uid, P.MANAGE_SERVER);
  run('UPDATE servers SET name = ? WHERE id = ?', text(req.body.name, 60, 'O nome do servidor'), s.id);
  syncServer(s.id);
  return {};
}));

api.delete('/servers/:sid', h(req => {
  const s = serverFor(req.uid, int(req.params.sid));
  if (s.owner_id !== req.uid) fail(403, 'Só o dono pode excluir o servidor');
  const members = all('SELECT user_id FROM members WHERE server_id = ?', s.id);
  for (const c of all("SELECT id FROM channels WHERE server_id = ? AND type = 'voice'", s.id)) {
    for (const uid of [...(voice.get(c.id)?.keys() || [])]) voiceLeave(uid);
  }
  run('DELETE FROM servers WHERE id = ?', s.id);
  for (const m of members) send(m.user_id, 'SERVER_REMOVE', { id: s.id });
  return {};
}));

api.post('/servers/:sid/leave', h(req => {
  const s = serverFor(req.uid, int(req.params.sid));
  if (s.owner_id === req.uid) fail(400, 'O dono não pode sair. Exclua o servidor.');
  removeMember(s.id, req.uid);
  return {};
}));

// ---- channels
const channelType = t => (t === 'voice' ? 'voice' : 'text');
const channelName = (name, type) => {
  const n = text(name, 40, 'O nome do canal');
  return type === 'text' ? n.toLowerCase().replace(/\s+/g, '-') : n;
};

api.post('/servers/:sid/channels', h(req => {
  const s = serverFor(req.uid, int(req.params.sid));
  need(s, req.uid, P.MANAGE_CHANNELS);
  const type = channelType(req.body.type);
  const pos = get('SELECT COALESCE(MAX(position), 0) + 1 AS p FROM channels WHERE server_id = ?', s.id).p;
  const category = String(req.body.category || '').trim().slice(0, 40) || (type === 'voice' ? 'Canais de voz' : 'Canais de texto');
  const id = insert('INSERT INTO channels(server_id, name, type, category, position) VALUES(?, ?, ?, ?, ?)',
    s.id, channelName(req.body.name, type), type, category, pos);
  syncServer(s.id);
  return { id };
}));

function managedChannel(req) {
  const { c, s } = access(req.uid, int(req.params.cid));
  if (!s) fail(400, 'Canal inválido');
  need(s, req.uid, P.MANAGE_CHANNELS);
  return { c, s };
}

api.patch('/channels/:cid', h(req => {
  const { c, s } = managedChannel(req);
  const name = req.body.name !== undefined ? channelName(req.body.name, c.type) : c.name;
  const category = req.body.category !== undefined ? String(req.body.category).trim().slice(0, 40) || c.category : c.category;
  const topic = req.body.topic !== undefined ? String(req.body.topic).trim().slice(0, 200) : c.topic;
  run('UPDATE channels SET name = ?, category = ?, topic = ? WHERE id = ?', name, category, topic, c.id);
  syncServer(s.id);
  return {};
}));

api.delete('/channels/:cid', h(req => {
  const { c, s } = managedChannel(req);
  for (const uid of [...(voice.get(c.id)?.keys() || [])]) voiceLeave(uid);
  run('DELETE FROM channels WHERE id = ?', c.id);
  syncServer(s.id);
  return {};
}));

api.put('/channels/:cid/overrides/:rid', h(req => {
  const { c, s } = managedChannel(req);
  need(s, req.uid, P.MANAGE_ROLES);
  const role = get('SELECT * FROM roles WHERE id = ? AND server_id = ?', int(req.params.rid), s.id);
  if (!role) fail(404, 'Cargo não encontrado');
  const allow = int(req.body.allow) & CHANNEL_BITS;
  const deny = int(req.body.deny) & CHANNEL_BITS & ~allow;
  if (allow || deny) {
    run(`INSERT INTO overrides(channel_id, role_id, allow, deny) VALUES(?, ?, ?, ?)
         ON CONFLICT(channel_id, role_id) DO UPDATE SET allow = excluded.allow, deny = excluded.deny`,
      c.id, role.id, allow, deny);
  } else {
    run('DELETE FROM overrides WHERE channel_id = ? AND role_id = ?', c.id, role.id);
  }
  // Anyone who just lost access to a voice channel is dropped from it.
  for (const uid of [...(voice.get(c.id)?.keys() || [])]) {
    const p = permsFor(s, uid, c.id);
    if (!(p & P.VIEW) || !(p & P.CONNECT)) voiceLeave(uid);
  }
  syncServer(s.id);
  return {};
}));

// ---- roles
// A role manager can never hand out permissions they do not hold themselves.
function roleInput(s, uid, body, current) {
  const mine = permsFor(s, uid);
  const perms = body.perms !== undefined ? int(body.perms) & ALL : current.perms;
  if (perms & ~mine) fail(403, 'Você não pode conceder permissões que não tem');
  if (current && current.perms & ~mine) fail(403, 'Esse cargo está acima do seu');
  const color = body.color !== undefined
    ? (/^#[0-9a-f]{6}$/i.test(body.color || '') ? body.color : null)
    : current?.color ?? null;
  return { perms, color };
}

api.post('/servers/:sid/roles', h(req => {
  const s = serverFor(req.uid, int(req.params.sid));
  need(s, req.uid, P.MANAGE_ROLES);
  const { perms, color } = roleInput(s, req.uid, { perms: DEFAULT_PERMS & permsFor(s, req.uid), ...req.body }, null);
  const pos = get('SELECT COALESCE(MAX(position), 0) + 1 AS p FROM roles WHERE server_id = ?', s.id).p;
  const id = insert('INSERT INTO roles(server_id, name, color, perms, position) VALUES(?, ?, ?, ?, ?)',
    s.id, text(req.body.name, 32, 'O nome do cargo'), color, perms, pos);
  syncServer(s.id);
  return { id };
}));

function managedRole(req) {
  const role = get('SELECT * FROM roles WHERE id = ?', int(req.params.rid));
  if (!role) fail(404, 'Cargo não encontrado');
  const s = serverFor(req.uid, role.server_id);
  need(s, req.uid, P.MANAGE_ROLES);
  return { role, s };
}

api.patch('/roles/:rid', h(req => {
  const { role, s } = managedRole(req);
  const { perms, color } = roleInput(s, req.uid, req.body, role);
  const name = role.is_everyone || req.body.name === undefined ? role.name : text(req.body.name, 32, 'O nome do cargo');
  run('UPDATE roles SET name = ?, color = ?, perms = ? WHERE id = ?', name, color, perms, role.id);
  syncServer(s.id);
  return {};
}));

api.delete('/roles/:rid', h(req => {
  const { role, s } = managedRole(req);
  if (role.is_everyone) fail(400, 'O cargo @todos não pode ser excluído');
  roleInput(s, req.uid, {}, role);
  run('DELETE FROM roles WHERE id = ?', role.id);
  syncServer(s.id);
  return {};
}));

function memberRoleChange(req, add) {
  const s = serverFor(req.uid, int(req.params.sid));
  need(s, req.uid, P.MANAGE_ROLES);
  const target = int(req.params.uid);
  serverFor(target, s.id);
  const role = get('SELECT * FROM roles WHERE id = ? AND server_id = ? AND is_everyone = 0', int(req.params.rid), s.id);
  if (!role) fail(404, 'Cargo não encontrado');
  roleInput(s, req.uid, {}, role);
  if (add) run('INSERT OR IGNORE INTO member_roles(server_id, user_id, role_id) VALUES(?, ?, ?)', s.id, target, role.id);
  else run('DELETE FROM member_roles WHERE user_id = ? AND role_id = ?', target, role.id);
  syncServer(s.id);
  return {};
}
api.put('/servers/:sid/members/:uid/roles/:rid', h(req => memberRoleChange(req, true)));
api.delete('/servers/:sid/members/:uid/roles/:rid', h(req => memberRoleChange(req, false)));

// ---- moderation
api.delete('/servers/:sid/members/:uid', h(req => {
  const s = serverFor(req.uid, int(req.params.sid));
  need(s, req.uid, P.KICK);
  const target = int(req.params.uid);
  serverFor(target, s.id);
  if (!canModerate(s, req.uid, target)) fail(403, 'Você não pode expulsar esse membro');
  removeMember(s.id, target);
  return {};
}));

api.put('/servers/:sid/bans/:uid', h(req => {
  const s = serverFor(req.uid, int(req.params.sid));
  need(s, req.uid, P.BAN);
  const target = int(req.params.uid);
  if (!userById(target)) fail(404, 'Usuário não encontrado');
  if (!canModerate(s, req.uid, target)) fail(403, 'Você não pode banir esse membro');
  run('INSERT OR IGNORE INTO bans(server_id, user_id) VALUES(?, ?)', s.id, target);
  removeMember(s.id, target);
  return {};
}));

api.delete('/servers/:sid/bans/:uid', h(req => {
  const s = serverFor(req.uid, int(req.params.sid));
  need(s, req.uid, P.BAN);
  run('DELETE FROM bans WHERE server_id = ? AND user_id = ?', s.id, int(req.params.uid));
  syncServer(s.id);
  return {};
}));

// ---- invites
api.post('/servers/:sid/invites', h(req => {
  const s = serverFor(req.uid, int(req.params.sid));
  need(s, req.uid, P.INVITE);
  limit('invite:' + req.uid, 30, 3600 * 1000);
  const code = crypto.randomBytes(6).toString('base64url');
  run('INSERT INTO invites(code, server_id, creator_id, created) VALUES(?, ?, ?, ?)', code, s.id, req.uid, now());
  return { code };
}));

api.post('/invites/:code/join', h(req => {
  limit('join:' + req.uid, 20, 3600 * 1000);
  const code = String(req.params.code).trim().split('/').pop();
  const inv = get('SELECT * FROM invites WHERE code = ?', code);
  if (!inv) fail(404, 'Convite inválido ou expirado');
  if (get('SELECT 1 FROM bans WHERE server_id = ? AND user_id = ?', inv.server_id, req.uid)) {
    fail(403, 'Você foi banido desse servidor');
  }
  run('INSERT OR IGNORE INTO members(server_id, user_id, joined) VALUES(?, ?, ?)', inv.server_id, req.uid, now());
  syncServer(inv.server_id);
  emitPresence(req.uid);
  return { id: inv.server_id };
}));

// ---- messages
api.get('/channels/:cid/messages', h(req => {
  const { c } = access(req.uid, int(req.params.cid));
  const before = req.query.before ? int(req.query.before) : Number.MAX_SAFE_INTEGER;
  const rows = all('SELECT * FROM messages WHERE channel_id = ? AND id < ? ORDER BY id DESC LIMIT 51', c.id, before);
  return { more: rows.length > 50, messages: rows.slice(0, 50).reverse().map(serialize) };
}));

api.post('/channels/:cid/messages', h(req => {
  const { c, perms } = access(req.uid, int(req.params.cid));
  if (c.type === 'voice') fail(400, 'Canais de voz não aceitam mensagens');
  if (!(perms & P.SEND)) fail(403, 'Você não pode enviar mensagens neste canal');
  if (c.type === 'dm') {
    const other = get('SELECT user_id FROM dm_members WHERE channel_id = ? AND user_id != ?', c.id, req.uid)?.user_id;
    if (blockedBetween(req.uid, other)) fail(403, 'Você não pode enviar mensagens para essa pessoa');
  }
  limit('msg:' + req.uid, 10, 10 * 1000, 'Você está enviando mensagens rápido demais.');
  const content = String(req.body.content ?? '').trim();
  if (content.length > 4000) fail(400, 'Mensagem muito longa (máximo de 4000 caracteres)');
  const raw = Array.isArray(req.body.attachments) ? req.body.attachments.slice(0, 10) : [];
  if (raw.length && !(perms & P.ATTACH)) fail(403, 'Você não pode enviar arquivos neste canal');
  const attachments = raw.map(a => {
    const url = String(a?.url || '');
    if (!UPLOAD_RE.test(url) || !fs.existsSync(path.join(db.dir, url))) fail(400, 'Anexo inválido');
    return { url, name: String(a.name || 'arquivo').slice(0, 200), size: Number(a.size) || 0, kind: kindOf(url) };
  });
  if (!content && !attachments.length) fail(400, 'Mensagem vazia');
  let replyTo = null;
  if (req.body.replyTo) {
    replyTo = get('SELECT id FROM messages WHERE id = ? AND channel_id = ?', int(req.body.replyTo), c.id)?.id ?? null;
  }
  const id = insert(
    'INSERT INTO messages(channel_id, author_id, content, attachments, reply_to, created) VALUES(?, ?, ?, ?, ?, ?)',
    c.id, req.uid, content, JSON.stringify(attachments), replyTo, now());
  const m = serialize(get('SELECT * FROM messages WHERE id = ?', id));
  emitChannel(c, 'MESSAGE_CREATE', m);
  return m;
}));

api.patch('/messages/:mid', h(req => {
  const { m, c } = messageFor(req.uid, int(req.params.mid));
  if (m.author_id !== req.uid) fail(403, 'Só o autor pode editar a mensagem');
  const content = String(req.body.content ?? '').trim();
  if (content.length > 4000 || (!content && m.attachments === '[]')) fail(400, 'Conteúdo inválido');
  run('UPDATE messages SET content = ?, edited = ? WHERE id = ?', content, now(), m.id);
  emitChannel(c, 'MESSAGE_UPDATE', serialize(get('SELECT * FROM messages WHERE id = ?', m.id)));
  return {};
}));

api.delete('/messages/:mid', h(req => {
  const { m, c, perms } = messageFor(req.uid, int(req.params.mid));
  if (m.author_id !== req.uid && !(c.type !== 'dm' && perms & P.MANAGE_MESSAGES)) {
    fail(403, 'Você não pode apagar essa mensagem');
  }
  const reported = get('SELECT 1 FROM reports WHERE message_id = ?', m.id);
  run('DELETE FROM messages WHERE id = ?', m.id);
  emitChannel(c, 'MESSAGE_DELETE', { id: m.id, channel_id: c.id });
  if (reported) syncServer(c.server_id);
  return {};
}));

const REPORT_REASONS = { spam: 'Spam', assedio: 'Assédio ou ódio', improprio: 'Conteúdo impróprio', outro: 'Outro motivo' };
api.post('/messages/:mid/report', h(req => {
  const { m, c } = messageFor(req.uid, int(req.params.mid));
  if (c.type === 'dm') fail(400, 'Em mensagens diretas, use “Bloquear”.');
  if (m.author_id === req.uid) fail(400, 'Você não pode denunciar a própria mensagem');
  limit('report:' + req.uid, 20, 3600 * 1000);
  const label = REPORT_REASONS[req.body.reason] || REPORT_REASONS.outro;
  const detail = String(req.body.detail || '').trim().slice(0, 300);
  run('INSERT OR IGNORE INTO reports(message_id, server_id, reporter_id, reason, created) VALUES(?, ?, ?, ?, ?)',
    m.id, c.server_id, req.uid, detail ? `${label}: ${detail}` : label, now());
  syncServer(c.server_id);
  return {};
}));

api.delete('/reports/:rid', h(req => {
  const r = get('SELECT * FROM reports WHERE id = ?', int(req.params.rid));
  if (!r) fail(404, 'Denúncia não encontrada');
  need(serverFor(req.uid, r.server_id), req.uid, P.MANAGE_MESSAGES);
  run('DELETE FROM reports WHERE id = ?', r.id);
  syncServer(r.server_id);
  return {};
}));

function react(req, add) {
  const { m, c } = messageFor(req.uid, int(req.params.mid));
  const emoji = String(req.params.emoji);
  if (!emoji || emoji.length > 16) fail(400, 'Emoji inválido');
  if (add) run('INSERT OR IGNORE INTO reactions(message_id, user_id, emoji) VALUES(?, ?, ?)', m.id, req.uid, emoji);
  else run('DELETE FROM reactions WHERE message_id = ? AND user_id = ? AND emoji = ?', m.id, req.uid, emoji);
  emitChannel(c, 'MESSAGE_UPDATE', serialize(m));
  return {};
}
api.put('/messages/:mid/reactions/:emoji', h(req => react(req, true)));
api.delete('/messages/:mid/reactions/:emoji', h(req => react(req, false)));

// ---- friends and direct messages
const pair = (x, y) => (x < y ? [x, y] : [y, x]);
const blockedBetween = (x, y) =>
  !!get('SELECT 1 FROM blocks WHERE (user_id = ? AND blocked_id = ?) OR (user_id = ? AND blocked_id = ?)', x, y, y, x);
const blockList = uid => all('SELECT blocked_id FROM blocks WHERE user_id = ?', uid).map(r => r.blocked_id);

function emitFriend(uid, otherId) {
  const f = friendList(uid).find(x => x.user.id === otherId);
  if (f) send(uid, 'FRIEND_UPDATE', f);
  else send(uid, 'FRIEND_REMOVE', { userId: otherId });
}

api.post('/friends', h(req => {
  const target = get('SELECT * FROM users WHERE username = ?', String(req.body.username || '').trim().toLowerCase().replace(/^@/, ''));
  if (!target) fail(404, 'Não encontrei ninguém com esse nome de usuário');
  if (target.id === req.uid) fail(400, 'Você não pode adicionar a si mesmo');
  if (blockedBetween(req.uid, target.id)) fail(403, 'Não foi possível enviar o pedido de amizade');
  limit('friend:' + req.uid, 20, 3600 * 1000);
  const [a, b] = pair(req.uid, target.id);
  const existing = get('SELECT * FROM friends WHERE a = ? AND b = ?', a, b);
  if (existing?.status === 'accepted') fail(400, 'Vocês já são amigos');
  if (existing && existing.requester === req.uid) fail(400, 'Pedido já enviado');
  if (existing) run("UPDATE friends SET status = 'accepted' WHERE a = ? AND b = ?", a, b);
  else run("INSERT INTO friends(a, b, requester, status) VALUES(?, ?, ?, 'pending')", a, b, req.uid);
  emitFriend(req.uid, target.id);
  emitFriend(target.id, req.uid);
  return {};
}));

api.put('/friends/:uid', h(req => {
  const other = int(req.params.uid);
  const [a, b] = pair(req.uid, other);
  const f = get('SELECT * FROM friends WHERE a = ? AND b = ?', a, b);
  if (!f || f.status !== 'pending' || f.requester === req.uid) fail(404, 'Pedido não encontrado');
  run("UPDATE friends SET status = 'accepted' WHERE a = ? AND b = ?", a, b);
  emitFriend(req.uid, other);
  emitFriend(other, req.uid);
  return {};
}));

api.delete('/friends/:uid', h(req => {
  const other = int(req.params.uid);
  const [a, b] = pair(req.uid, other);
  run('DELETE FROM friends WHERE a = ? AND b = ?', a, b);
  emitFriend(req.uid, other);
  emitFriend(other, req.uid);
  return {};
}));

api.post('/dms', h(req => {
  const other = int(req.body.userId);
  if (other === req.uid || !userById(other)) fail(404, 'Usuário não encontrado');
  const existing = get(
    `SELECT d1.channel_id AS id FROM dm_members d1 JOIN dm_members d2 ON d1.channel_id = d2.channel_id
     WHERE d1.user_id = ? AND d2.user_id = ?`, req.uid, other);
  if (existing) return dmSnapshot(existing.id, req.uid);
  if (!related(req.uid).includes(other)) fail(403, 'Vocês precisam ser amigos ou dividir um servidor');
  if (blockedBetween(req.uid, other)) fail(403, 'Não é possível conversar com essa pessoa');
  limit('dm:' + req.uid, 30, 3600 * 1000);
  const cid = tx(() => {
    const id = insert("INSERT INTO channels(type) VALUES('dm')");
    run('INSERT INTO dm_members(channel_id, user_id) VALUES(?, ?), (?, ?)', id, req.uid, id, other);
    return id;
  });
  send(req.uid, 'DM_CREATE', dmSnapshot(cid, req.uid));
  send(other, 'DM_CREATE', dmSnapshot(cid, other));
  return dmSnapshot(cid, req.uid);
}));

// Blocking hides the other person's messages for you, ends any friendship and
// stops direct messages and friend requests in both directions.
api.put('/blocks/:uid', h(req => {
  const other = int(req.params.uid);
  if (other === req.uid || !userById(other)) fail(404, 'Usuário não encontrado');
  run('INSERT OR IGNORE INTO blocks(user_id, blocked_id) VALUES(?, ?)', req.uid, other);
  const [a, b] = pair(req.uid, other);
  run('DELETE FROM friends WHERE a = ? AND b = ?', a, b);
  emitFriend(req.uid, other);
  emitFriend(other, req.uid);
  send(req.uid, 'BLOCKS', { ids: blockList(req.uid) });
  return {};
}));

api.delete('/blocks/:uid', h(req => {
  run('DELETE FROM blocks WHERE user_id = ? AND blocked_id = ?', req.uid, int(req.params.uid));
  send(req.uid, 'BLOCKS', { ids: blockList(req.uid) });
  return {};
}));

api.use((req, res) => res.status(404).json({ error: 'Rota não encontrada' }));

// ---- static files
app.use('/uploads', express.static(path.join(db.dir, 'uploads'), {
  maxAge: '30d',
  setHeaders(res, file) {
    // Only known media types render inline; everything else downloads, so an
    // uploaded HTML or SVG file can never run in the app's origin.
    if (kindOf(file) === 'file') res.set('Content-Disposition', 'attachment');
  },
}));
app.use('/updates', express.static(path.join(db.dir, 'updates')));
app.use(express.static(path.join(__dirname, '..', 'app', 'renderer')));

// ---------------------------------------------------------------- gateway
const server = http.createServer(app);
const wss = new WebSocketServer({ server, path: '/gateway', maxPayload: 256 * 1024 });
// The ws library re-emits HTTP listen errors; start() already reports those.
wss.on('error', () => {});

const ops = {
  ping(ws) {
    ws.send('{"t":"PONG"}');
  },
  typing(ws, m) {
    if (!hit('typing:' + ws.uid, 30, 60 * 1000)) return;
    const { c, perms } = access(ws.uid, int(m.channelId));
    if (perms & P.SEND) emitChannel(c, 'TYPING', { channelId: c.id, userId: ws.uid }, ws.uid);
  },
  status(ws, m) {
    if (!['online', 'idle', 'dnd', 'invisible'].includes(m.status)) return;
    statuses.set(ws.uid, m.status);
    send(ws.uid, 'STATUS', { status: m.status });
    emitPresence(ws.uid);
  },
  voice_join(ws, m) {
    const { c, perms } = access(ws.uid, int(m.channelId));
    if (c.type !== 'voice') fail(400, 'Esse não é um canal de voz');
    if (!(perms & P.CONNECT)) fail(403, 'Você não pode entrar neste canal de voz');
    const old = voiceConn.get(ws.uid);
    if (old) {
      if (old !== ws && old.readyState === 1) old.send('{"t":"VOICE_KICK"}');
      voiceLeave(ws.uid);
    }
    if (!voice.has(c.id)) voice.set(c.id, new Map());
    voice.get(c.id).set(ws.uid, voiceFlags(m));
    voiceConn.set(ws.uid, ws);
    ws.voiceChannel = c.id;
    emitVoice(c);
  },
  voice_state(ws, m) {
    if (voiceConn.get(ws.uid) !== ws) return;
    voice.get(ws.voiceChannel).set(ws.uid, voiceFlags(m));
    emitVoice(get('SELECT * FROM channels WHERE id = ?', ws.voiceChannel));
  },
  voice_leave(ws) {
    if (voiceConn.get(ws.uid) === ws) voiceLeave(ws.uid);
  },
  signal(ws, m) {
    const peer = voiceConn.get(int(m.to));
    if (voiceConn.get(ws.uid) !== ws || !peer || peer.voiceChannel !== ws.voiceChannel) return;
    peer.send(JSON.stringify({ t: 'SIGNAL', d: { from: ws.uid, data: m.data } }));
  },
};

const voiceFlags = m => ({
  mute: !!m.mute,
  deaf: !!m.deaf,
  video: ['camera', 'screen'].includes(m.video) ? m.video : null,
});

function identify(ws, token) {
  const uid = userForToken(token);
  if (!uid) {
    ws.send('{"t":"AUTH_FAIL"}');
    return ws.close();
  }
  ws.uid = uid;
  ws.tokenHash = hashToken(String(token));
  const first = !conns.get(uid)?.size;
  if (!conns.has(uid)) conns.set(uid, new Set());
  conns.get(uid).add(ws);
  ws.send(JSON.stringify({ t: 'READY', d: ready(uid) }));
  if (first) emitPresence(uid);
}

wss.on('connection', ws => {
  ws.alive = true;
  ws.on('pong', () => (ws.alive = true));
  const timer = setTimeout(() => !ws.uid && ws.close(), 10000);
  ws.on('message', raw => {
    let m;
    try {
      m = JSON.parse(raw);
    } catch {
      return;
    }
    try {
      if (!ws.uid) return m.op === 'identify' ? identify(ws, m.token) : undefined;
      if (Object.hasOwn(ops, m.op)) ops[m.op](ws, m);
    } catch (e) {
      if (!e.status) console.error(e);
      ws.send(JSON.stringify({ t: 'ERROR', d: { message: e.status ? e.message : 'Erro interno do servidor' } }));
    }
  });
  ws.on('close', () => {
    clearTimeout(timer);
    const uid = ws.uid;
    if (!uid) return;
    if (voiceConn.get(uid) === ws) voiceLeave(uid);
    const set = conns.get(uid);
    set?.delete(ws);
    if (set && !set.size) {
      conns.delete(uid);
      emitPresence(uid);
    }
  });
});

let heartbeat = null;

// The server runs standalone (`node server/index.js`) or inside the desktop
// app, which starts and stops it from its settings.
function start(port = PORT) {
  return new Promise((resolve, reject) => {
    const onError = e => reject(e);
    server.once('error', onError);
    server.listen(port, HOST, () => {
      server.off('error', onError);
      heartbeat = setInterval(() => {
        for (const ws of wss.clients) {
          if (!ws.alive) ws.terminate();
          else {
            ws.alive = false;
            ws.ping();
          }
        }
      }, 30000);
      console.log(`${PKG.productName} ${PKG.version} em http://localhost:${port}`);
      resolve(port);
    });
  });
}

function stop() {
  clearInterval(heartbeat);
  for (const ws of wss.clients) ws.terminate();
  return new Promise(resolve => {
    server.close(() => resolve());
    server.closeAllConnections();
  });
}

if (require.main === module) {
  start().catch(e => {
    console.error(e.code === 'EADDRINUSE' ? `A porta ${PORT} já está em uso.` : e.message);
    process.exit(1);
  });
}

module.exports = { start, stop, dataDir: db.dir };
