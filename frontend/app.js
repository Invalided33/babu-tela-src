'use strict';
// Not named `native` on window: a top-level const cannot shadow a bridged global.
const native = window.prosaNative || null;
const BASE = (native ? native.serverUrl : location.origin).replace(/\/$/, '');

const P = {
  VIEW: 1, SEND: 2, MANAGE_MESSAGES: 4, MANAGE_CHANNELS: 8, MANAGE_ROLES: 16, KICK: 32,
  BAN: 64, ADMIN: 128, INVITE: 256, ATTACH: 512, CONNECT: 1024, MANAGE_SERVER: 2048,
};
const PERM_LABELS = [
  [P.ADMIN, 'Administrador', 'Concede todas as permissões'],
  [P.MANAGE_SERVER, 'Gerenciar servidor', 'Renomear o servidor'],
  [P.MANAGE_CHANNELS, 'Gerenciar canais', 'Criar, editar e excluir canais'],
  [P.MANAGE_ROLES, 'Gerenciar cargos', 'Criar cargos e atribuí-los a membros'],
  [P.MANAGE_MESSAGES, 'Gerenciar mensagens', 'Apagar mensagens de outras pessoas'],
  [P.KICK, 'Expulsar membros', ''],
  [P.BAN, 'Banir membros', ''],
  [P.INVITE, 'Criar convites', ''],
  [P.VIEW, 'Ver canais', ''],
  [P.SEND, 'Enviar mensagens', ''],
  [P.ATTACH, 'Anexar arquivos', ''],
  [P.CONNECT, 'Conectar aos canais de voz', ''],
];
const STATUS = { online: 'Disponível', idle: 'Ausente', dnd: 'Não perturbe', invisible: 'Invisível', offline: 'Offline' };
const EMOJIS = '😀 😂 🥲 😊 😍 😎 🤔 😴 😭 😡 👍 👎 👏 🙏 💪 👀 ❤️ 🔥 ✨ 🎉 ✅ ❌ ⚠️ 💯 🚀 🎮 🎧 🎵 ☕ 🍕 🐱 🐶 🌙 ☀️ 💤 💬 📌 📎 🧠 🫡'.split(' ');

const svg = d => `<svg class="i" viewBox="0 0 24 24" aria-hidden="true">${d}</svg>`;
const I = {
  back: svg('<path d="M15 18l-6-6 6-6"/>'),
  hash: svg('<path d="M4 9h16M4 15h16M10 3 8 21M16 3l-2 18"/>'),
  speaker: svg('<path d="M11 5 6 9H2v6h4l5 4V5z"/><path d="M15.5 8.5a5 5 0 0 1 0 7M19 5a10 10 0 0 1 0 14"/>'),
  mic: svg('<path d="M12 2a3 3 0 0 0-3 3v7a3 3 0 0 0 6 0V5a3 3 0 0 0-3-3Z"/><path d="M19 10v2a7 7 0 0 1-14 0v-2M12 19v3"/>'),
  micOff: svg('<path d="M12 2a3 3 0 0 0-3 3v7a3 3 0 0 0 6 0V5a3 3 0 0 0-3-3Z"/><path d="M19 10v2a7 7 0 0 1-14 0v-2M12 19v3M3 3l18 18"/>'),
  head: svg('<path d="M3 14h3a2 2 0 0 1 2 2v3a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-7a9 9 0 0 1 18 0v7a2 2 0 0 1-2 2h-1a2 2 0 0 1-2-2v-3a2 2 0 0 1 2-2h3"/>'),
  headOff: svg('<path d="M3 14h3a2 2 0 0 1 2 2v3a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-7a9 9 0 0 1 18 0v7a2 2 0 0 1-2 2h-1a2 2 0 0 1-2-2v-3a2 2 0 0 1 2-2h3M3 3l18 18"/>'),
  video: svg('<path d="m22 8-6 4 6 4V8Z"/><rect width="14" height="12" x="2" y="6" rx="2"/>'),
  screen: svg('<rect width="20" height="14" x="2" y="3" rx="2"/><path d="M8 21h8M12 17v4"/>'),
  hang: svg('<path d="M10.7 13.3a16 16 0 0 0 3.4 2.6l1.3-1.3a2 2 0 0 1 2.1-.4c.9.3 1.8.6 2.8.7a2 2 0 0 1 1.7 2v3a2 2 0 0 1-2.2 2A19.8 19.8 0 0 1 2 4.1 2 2 0 0 1 4.1 2h3a2 2 0 0 1 2 1.7c.1 1 .4 1.9.7 2.8a2 2 0 0 1-.5 2.1L8.1 9.9M22 2 2 22"/>'),
  gear: svg('<circle cx="12" cy="12" r="3"/><path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-4 0v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1 0-4h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06a1.65 1.65 0 0 0 1.82.33H9a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06a1.65 1.65 0 0 0-.33 1.82V9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z"/>'),
  plus: svg('<path d="M12 5v14M5 12h14"/>'),
  x: svg('<path d="M18 6 6 18M6 6l12 12"/>'),
  reply: svg('<path d="M9 14 4 9l5-5"/><path d="M20 20v-7a4 4 0 0 0-4-4H4"/>'),
  edit: svg('<path d="M17 3a2.83 2.83 0 1 1 4 4L7.5 20.5 2 22l1.5-5.5Z"/>'),
  trash: svg('<path d="M3 6h18M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6M8 6V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2"/>'),
  smile: svg('<circle cx="12" cy="12" r="10"/><path d="M8 14s1.5 2 4 2 4-2 4-2M9 9h.01M15 9h.01"/>'),
  clip: svg('<path d="m21.4 11-9.2 9.2a6 6 0 0 1-8.5-8.5l8.6-8.5A4 4 0 1 1 18 8.8l-8.6 8.6a2 2 0 0 1-2.8-2.8l8.5-8.5"/>'),
  users: svg('<path d="M16 21v-2a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v2"/><circle cx="9" cy="7" r="4"/><path d="M22 21v-2a4 4 0 0 0-3-3.9M16 3.1a4 4 0 0 1 0 7.8"/>'),
  down: svg('<path d="m6 9 6 6 6-6"/>'),
  chat: svg('<path d="M21 15a2 2 0 0 1-2 2H7l-4 4V5a2 2 0 0 1 2-2h14a2 2 0 0 1 2 2z"/>'),
  file: svg('<path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z"/><path d="M14 2v6h6"/>'),
  flag: svg('<path d="M4 15s1-1 4-1 5 2 8 2 4-1 4-1V3s-1 1-4 1-5-2-8-2-4 1-4 1z"/><path d="M4 22v-7"/>'),
};
// Back to the list; only shown on phones (see "phone layout").
const NAV = `<button class="ib navbtn" data-act="nav" title="Voltar" aria-label="Voltar">${I.back}</button>`;

const S = {
  token: null, me: null, version: '',
  servers: new Map(), dms: new Map(), friends: new Map(), users: new Map(),
  view: { type: 'home', channelId: null },
  msgs: new Map(), unread: new Map(), typing: new Map(),
  reply: null, editing: null, files: [],
  ws: null, tries: 0, retry: null, focused: document.hasFocus(),
  blocks: new Set(), revealed: new Set(), sessions: null,
  friendTab: 'online', showMembers: !matchMedia('(max-width: 720px)').matches, modal: null, pop: null, roleSel: null, pendingGo: null, lastTyping: 0,
};

const store = {
  get(k) { try { return localStorage.getItem('prosa.' + k); } catch { return null; } },
  set(k, v) { try { v == null ? localStorage.removeItem('prosa.' + k) : localStorage.setItem('prosa.' + k, v); } catch {} },
};

// The session token: encrypted by Windows in the desktop app (see main.js),
// localStorage in the browser.
const tokenStore = {
  async load() {
    if (!native) return store.get('token');
    const token = await native.session.get();
    if (token) return token;
    // Older versions kept the token in localStorage; move it over.
    const legacy = store.get('token');
    if (legacy && (await native.session.set(legacy))) store.set('token', null);
    return legacy;
  },
  async save(token) {
    if (native && (await native.session.set(token || null))) return store.set('token', null);
    store.set('token', token || null);
  },
};

const $ = (sel, root = document) => root.querySelector(sel);
const esc = s => String(s ?? '').replace(/[&<>"']/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));

// ------------------------------------------------------------------ network
async function api(method, url, body) {
  let res;
  try {
    res = await fetch(BASE + '/api' + url, {
      method,
      headers: { 'Content-Type': 'application/json', ...(S.token && { Authorization: 'Bearer ' + S.token }) },
      body: body === undefined ? undefined : JSON.stringify(body),
    });
  } catch {
    throw new Error('Não foi possível falar com o servidor em ' + BASE);
  }
  const data = await res.json().catch(() => ({}));
  if (res.status === 401 && S.token) logout(true);
  if (!res.ok) throw new Error(data.error || 'Algo deu errado');
  return data;
}

async function uploadFile(file) {
  const body = new FormData();
  body.append('file', file);
  const res = await fetch(BASE + '/api/upload', { method: 'POST', headers: { Authorization: 'Bearer ' + S.token }, body });
  const data = await res.json().catch(() => ({}));
  if (!res.ok) throw new Error(data.error || 'Falha no envio do arquivo');
  return data;
}

function sendOp(m) {
  if (S.ws?.readyState === 1) S.ws.send(JSON.stringify(m));
}

function connect() {
  clearTimeout(S.retry);
  const ws = new WebSocket(BASE.replace(/^http/, 'ws') + '/gateway');
  S.ws = ws;
  ws.onopen = () => ws.send(JSON.stringify({ op: 'identify', token: S.token }));
  ws.onmessage = e => {
    const { t, d } = JSON.parse(e.data);
    if (EV[t]) EV[t](d);
  };
  ws.onclose = () => {
    if (S.ws !== ws || !S.token) return;
    Voice.leave(true, 'disconnect');
    const delay = Math.min(1000 * 2 ** S.tries++, 15000);
    // Before the first READY there is no app to show behind a banner.
    if ($('#app').hidden) showConnecting(delay);
    else banner('Conexão perdida. Reconectando…');
    S.retry = setTimeout(connect, delay);
  };
}

function logout(silent) {
  if (!silent && S.token) api('POST', '/logout').catch(() => {});
  S.token = null;
  tokenStore.save(null);
  Voice.leave(true);
  S.ws?.close();
  closeModal();
  $('#app').hidden = true;
  setBadge(0);
  showAuth();
}

// ------------------------------------------------------------------ helpers
const server = () => (S.view.type === 'server' ? S.servers.get(S.view.serverId) : null);
function channel() {
  if (S.view.type === 'home') return S.dms.get(S.view.channelId) || null;
  return server()?.channels.find(c => c.id === S.view.channelId) || null;
}
const nameOf = u => u?.display_name || u?.username || 'Desconhecido';
const can = (bit, s = server()) => !!s && !!(s.perms & bit);

function indexUsers() {
  for (const s of S.servers.values()) for (const m of s.members) S.users.set(m.id, m);
  for (const d of S.dms.values()) S.users.set(d.user.id, d.user);
  for (const f of S.friends.values()) S.users.set(f.user.id, f.user);
  if (S.me) S.users.set(S.me.id, { ...S.me, status: S.me.status === 'invisible' ? 'offline' : S.me.status });
}
const knownName = name => [...S.users.values()].some(u => u.username === name);

function roleColor(s, member) {
  if (!s || !member) return '';
  return s.roles.find(r => !r.is_everyone && r.color && member.roles.includes(r.id))?.color || '';
}

function avatar(u, size = 32, dot = false, ring = '') {
  const inner = u?.avatar ? `<img src="${esc(BASE + u.avatar)}" alt="">` : esc(nameOf(u).trim()[0]?.toUpperCase() || '?');
  const status = dot ? `<i class="dot ${esc(u?.status || 'offline')}"></i>` : '';
  return `<span class="av" data-speak="${u?.id}" style="--s:${size}px;--c:${esc(u?.color || '')};${ring && `--ring:${ring}`}">${inner}${status}</span>`;
}

function fmtTime(ts) {
  const d = new Date(ts);
  const hm = d.toLocaleTimeString('pt-BR', { hour: '2-digit', minute: '2-digit' });
  return d.toDateString() === new Date().toDateString() ? `Hoje às ${hm}` : `${d.toLocaleDateString('pt-BR')} ${hm}`;
}
const fmtSize = n => (n > 1048576 ? (n / 1048576).toFixed(1) + ' MB' : Math.max(1, Math.round(n / 1024)) + ' KB');

const mentionsMe = content => !!S.me && new RegExp(`(^|\\s)@${S.me.username.replace(/\./g, '\\.')}(?![a-z0-9_.])`).test(content);

function md(text) {
  const keep = [];
  const stash = html => `\u0000${keep.push(html) - 1}\u0000`;
  let s = esc(text);
  s = s.replace(/```(?:[a-z]*\n)?([\s\S]+?)```/g, (_, c) => stash(`<pre>${c.trim()}</pre>`));
  s = s.replace(/`([^`\n]+)`/g, (_, c) => stash(`<code>${c}</code>`));
  s = s.replace(/https?:\/\/[^\s<]+/g, u => stash(`<a href="${u}" target="_blank" rel="noopener noreferrer">${u}</a>`));
  s = s
    .replace(/\*\*([^*]+)\*\*/g, '<b>$1</b>')
    .replace(/\*([^*\n]+)\*/g, '<i>$1</i>')
    .replace(/__([^_]+)__/g, '<u>$1</u>')
    .replace(/~~([^~]+)~~/g, '<s>$1</s>')
    .replace(/\|\|([^|]+)\|\|/g, '<span class="spoiler" data-act="spoiler">$1</span>');
  s = s.replace(/(^|\s)@([a-z0-9_.]{3,20})/g, (m, pre, name) =>
    knownName(name) ? `${pre}<span class="mention">@${name}</span>` : m);
  return s.replace(/\n/g, '<br>').replace(/\u0000(\d+)\u0000/g, (_, i) => keep[i]);
}

function toast(message) {
  const el = document.createElement('div');
  el.className = 'toast';
  el.textContent = message;
  $('#toasts').append(el);
  setTimeout(() => el.remove(), 4000);
}

function banner(html) {
  const el = $('#banner');
  el.hidden = !html;
  el.innerHTML = html || '';
}

// ------------------------------------------------------------------ unread
function totalHot() {
  let n = 0;
  for (const u of S.unread.values()) n += u.hot;
  return n;
}
function setBadge(n) {
  document.title = n ? `(${n}) Babu Tela` : 'Babu Tela';
  native?.setBadge(n);
}
function markRead(cid) {
  if (S.unread.delete(cid)) {
    setBadge(totalHot());
    renderRail();
    renderSidebar();
  }
}

function notify(title, body, view) {
  if (native) return native.notify({ title, body, view });
  if (store.get('notify') !== '1' || !('Notification' in window) || Notification.permission !== 'granted') return;
  const n = new Notification(title, { body });
  n.onclick = () => {
    window.focus();
    goTo(view);
  };
}

// ------------------------------------------------------------------ navigation
function validView(v) {
  if (v?.type === 'server') {
    const s = S.servers.get(v.serverId);
    if (!s) return false;
    return s.channels.some(c => c.id === v.channelId);
  }
  return v?.type === 'home' && (v.channelId == null || S.dms.has(v.channelId));
}

function defaultChannel(s) {
  const last = Number(store.get('last.' + s.id));
  return (s.channels.find(c => c.id === last) || s.channels.find(c => c.type === 'text') || s.channels[0])?.id ?? null;
}

function goTo(v) {
  if (v.type === 'server' && v.channelId == null) {
    const s = S.servers.get(v.serverId);
    if (!s) return;
    v.channelId = defaultChannel(s);
  }
  S.view = v;
  S.reply = S.editing = null;
  S.files = [];
  store.set('view', JSON.stringify(v));
  if (v.type === 'server' && v.channelId) store.set('last.' + v.serverId, v.channelId);
  const c = channel();
  if (c && c.type !== 'voice') {
    S.unread.delete(c.id);
    setBadge(totalHot());
    if (!S.msgs.has(c.id)) loadMessages(c.id);
  }
  renderAll();
  $('#composer textarea')?.focus();
}

async function loadMessages(cid, before) {
  const st = S.msgs.get(cid) || { list: [], more: false, loading: false };
  if (st.loading) return;
  st.loading = true;
  S.msgs.set(cid, st);
  try {
    const data = await api('GET', `/channels/${cid}/messages${before ? '?before=' + before : ''}`);
    st.list = before ? [...data.messages, ...st.list] : data.messages;
    st.more = data.more;
  } catch (e) {
    toast(e.message);
  }
  st.loading = false;
  if (S.view.channelId === cid) renderMsgs(before ? 'keep' : 'bottom');
}

// ------------------------------------------------------------------ rendering
function renderAll() {
  renderRail();
  renderSidebar();
  renderMain();
  renderMembers();
  renderVoicePanel();
  renderUserPanel();
}

function serverUnread(s) {
  let n = 0, any = false;
  for (const c of s.channels) {
    const u = S.unread.get(c.id);
    if (u) {
      any = true;
      n += u.hot;
    }
  }
  return { n, any };
}

function renderRail() {
  let dmHot = 0;
  for (const d of S.dms.values()) dmHot += S.unread.get(d.id)?.hot || 0;
  const pending = [...S.friends.values()].filter(f => f.state === 'incoming').length;
  let html = `<button class="srv ${S.view.type === 'home' ? 'on' : ''}" data-act="home" title="Mensagens diretas" aria-label="Mensagens diretas">${I.chat}${dmHot + pending ? `<span class="badge">${dmHot + pending}</span>` : ''}</button><span class="rail-sep"></span>`;
  for (const s of S.servers.values()) {
    const u = serverUnread(s);
    const on = S.view.type === 'server' && S.view.serverId === s.id;
    html += `<button class="srv ${on ? 'on' : ''} ${u.any && !on ? 'unread' : ''}" style="--c:${esc(s.color)}" data-act="server" data-id="${s.id}" title="${esc(s.name)}">${esc(initials(s.name))}${u.n ? `<span class="badge">${u.n}</span>` : ''}</button>`;
  }
  html += `<button class="srv add" data-act="addServer" title="Criar ou entrar em um servidor" aria-label="Criar ou entrar em um servidor">${I.plus}</button>`;
  $('#rail').innerHTML = html;
}
const initials = name => name.trim().split(/\s+/).slice(0, 2).map(w => w[0]).join('').toUpperCase();

function renderSidebar() {
  const s = server();
  if (!s) {
    const pending = [...S.friends.values()].filter(f => f.state === 'incoming').length;
    $('#side-head').innerHTML = '<span>Mensagens diretas</span>';
    let html = `<button class="row ${S.view.channelId == null ? 'on' : ''}" data-act="friends">${I.users}<span class="name">Amigos</span>${pending ? `<span class="badge">${pending}</span>` : ''}</button><div class="cat">Conversas</div>`;
    for (const d of S.dms.values()) {
      const u = S.unread.get(d.id);
      html += `<button class="row ${S.view.channelId === d.id ? 'on' : ''} ${u ? 'unread' : ''}" data-act="dm" data-id="${d.id}">${avatar(d.user, 28, true)}<span class="name">${esc(nameOf(d.user))}</span>${u ? `<span class="badge">${u.n}</span>` : ''}</button>`;
    }
    if (!S.dms.size) html += '<div class="cat" style="text-transform:none;letter-spacing:0;font-weight:400">Nenhuma conversa ainda.</div>';
    $('#side-list').innerHTML = html;
    return;
  }
  const reports = can(P.MANAGE_MESSAGES) ? s.reports?.length || 0 : 0;
  $('#side-head').innerHTML = `<button class="menu" data-act="serverMenu"><span>${esc(s.name)}</span>${reports ? `<span class="badge" title="Denúncias pendentes">${reports}</span>` : ''}${I.down}</button>`;
  const cats = new Map();
  for (const c of s.channels) {
    const k = c.category || 'Canais';
    if (!cats.has(k)) cats.set(k, []);
    cats.get(k).push(c);
  }
  let html = '';
  for (const [cat, list] of cats) {
    html += `<div class="cat"><span>${esc(cat)}</span>${can(P.MANAGE_CHANNELS) ? `<button data-act="newChannel" data-cat="${esc(cat)}" data-type="${list[0].type}" title="Criar canal" aria-label="Criar canal em ${esc(cat)}">${I.plus}</button>` : ''}</div>`;
    for (const c of list) {
      const u = S.unread.get(c.id);
      const gear = can(P.MANAGE_CHANNELS) ? `<span class="gear" data-act="editChannel" data-id="${c.id}" title="Editar canal" role="button">${I.gear}</span>` : '';
      html += `<button class="row ${S.view.channelId === c.id ? 'on' : ''} ${u ? 'unread' : ''}" data-act="channel" data-id="${c.id}">${c.type === 'voice' ? I.speaker : I.hash}<span class="name">${esc(c.name)}</span>${u?.hot ? `<span class="badge">${u.hot}</span>` : ''}${gear}</button>`;
      if (c.type === 'voice' && s.voice[c.id]?.length) {
        html += '<div class="vusers">' + s.voice[c.id].map(v => {
          const m = S.users.get(v.userId);
          return `<div class="vuser">${avatar(m, 22)}<span class="name">${esc(nameOf(m))}</span>${v.video ? (v.video === 'screen' ? I.screen : I.video) : ''}${v.deaf ? `<span class="off">${I.headOff}</span>` : v.mute ? `<span class="off">${I.micOff}</span>` : ''}</div>`;
        }).join('') + '</div>';
      }
    }
  }
  $('#side-list').innerHTML = html || '<div class="cat">Nenhum canal visível</div>';
}

function renderUserPanel() {
  const me = S.me;
  $('#user-panel').innerHTML = `${avatar({ ...me, status: me.status === 'invisible' ? 'offline' : me.status }, 34, true, 'var(--bg0)')}
    <div class="who"><b>${esc(nameOf(me))}</b><small>${esc(STATUS[me.status] || '')}</small></div>
    <button class="ib" data-act="settings" title="Configurações" aria-label="Configurações">${I.gear}</button>`;
}

function voiceButtons() {
  return `<button class="ib ${Voice.mute || Voice.deaf ? 'off' : ''}" data-act="mute" title="Microfone" aria-label="Ligar ou desligar o microfone">${Voice.mute || Voice.deaf ? I.micOff : I.mic}</button>
    <button class="ib ${Voice.deaf ? 'off' : ''}" data-act="deafen" title="Áudio" aria-label="Ligar ou desligar o áudio">${Voice.deaf ? I.headOff : I.head}</button>
    <button class="ib ${Voice.video === 'camera' ? 'on' : ''}" data-act="camera" title="Câmera" aria-label="Ligar ou desligar a câmera">${I.video}</button>
    <button class="ib ${Voice.video === 'screen' ? 'on' : ''}" data-act="share" title="Compartilhar tela" aria-label="Compartilhar tela">${I.screen}</button>
    <button class="ib hang" data-act="hangup" title="Desconectar" aria-label="Desconectar da chamada">${I.hang}</button>`;
}

function voiceLocation() {
  for (const s of S.servers.values()) {
    const c = s.channels.find(x => x.id === Voice.channelId);
    if (c) return { s, c };
  }
  return null;
}

function renderVoicePanel() {
  const loc = Voice.channelId && voiceLocation();
  $('#voice-panel').innerHTML = loc
    ? `<div class="vp"><div class="where"><button data-act="voiceGoto"><b>Voz conectada</b><small>${esc(loc.c.name)} · ${esc(loc.s.name)}</small></button></div><div class="btns">${voiceButtons()}</div></div>`
    : '';
}

function renderMembers() {
  const s = server();
  const el = $('#members');
  if (!s || !S.showMembers) return (el.innerHTML = '');
  const on = s.members.filter(m => m.status !== 'offline');
  const off = s.members.filter(m => m.status === 'offline');
  const row = m => `<button class="mrow ${m.status === 'offline' ? 'offline' : ''}" data-act="profile" data-id="${m.id}">${avatar(m, 32, true)}<span class="name" style="color:${esc(roleColor(s, m))}">${esc(nameOf(m))}</span>${m.id === s.owner_id ? '<span class="crown" title="Dono do servidor">★</span>' : ''}</button>`;
  el.innerHTML = `<div class="cat">Online — ${on.length}</div>${on.map(row).join('')}${off.length ? `<div class="cat">Offline — ${off.length}</div>${off.map(row).join('')}` : ''}`;
}

function renderMain() {
  const main = $('#main');
  const c = channel();
  const old = $('#composer textarea');
  const draft = old && main.dataset.key === String(c?.id) ? { value: old.value, focus: document.activeElement === old } : null;
  main.dataset.key = c?.id ?? '';
  if (S.view.type === 'home' && !c) return renderFriends();
  if (!c) {
    main.innerHTML = `<div class="head">${NAV}</div><div class="empty"><div><h3>Nada por aqui</h3>Este servidor não tem canais que você possa ver.</div></div>`;
    return;
  }
  if (c.type === 'voice') return renderStage(true);
  const dm = c.type === 'dm';
  const title = dm ? `${avatar(c.user, 26, true, 'var(--bg2)')}<h2>${esc(nameOf(c.user))}</h2>` : `${I.hash}<h2>${esc(c.name)}</h2>`;
  const topic = !dm && c.topic ? `<span class="topic">${esc(c.topic)}</span>` : '<span class="sp"></span>';
  const toggle = dm ? '' : `<button class="ib ${S.showMembers ? 'on' : ''}" data-act="toggleMembers" title="Lista de membros" aria-label="Mostrar ou ocultar membros">${I.users}</button>`;
  const blocked = dm && S.blocks.has(c.user.id);
  const canSend = (dm && !blocked) || (!dm && c.perms & P.SEND);
  const canAttach = dm || c.perms & P.ATTACH;
  const place = dm ? `Conversar com ${nameOf(c.user)}` : `Conversar em #${c.name}`;
  main.innerHTML = `<div class="head">${NAV}${title}${topic}${toggle}</div>
    <div id="msgs"></div><div id="typing"></div>
    ${canSend
      ? `<div id="composer"><div id="extras"></div><div class="line">
          ${canAttach ? `<button class="ib" data-act="attach" title="Anexar arquivo" aria-label="Anexar arquivo">${I.clip}</button><input type="file" id="filepick" multiple hidden>` : ''}
          <textarea rows="1" placeholder="${esc(place)}" aria-label="Mensagem" maxlength="4000"></textarea>
          <button class="ib" data-act="emoji" title="Emoji" aria-label="Inserir emoji">${I.smile}</button></div></div>`
      : `<div id="composer" class="locked">${blocked ? 'Você bloqueou essa pessoa. Desbloqueie pelo perfil dela para conversar.' : 'Você não tem permissão para enviar mensagens neste canal.'}</div>`}`;
  const ta = $('#composer textarea');
  if (ta && draft) {
    ta.value = draft.value;
    growComposer(ta);
    if (draft.focus) ta.focus();
  }
  renderExtras();
  renderMsgs('bottom');
  renderTyping();
}

function attachmentHTML(a) {
  const url = esc(BASE + a.url);
  if (a.kind === 'image') return `<div class="att"><a href="${url}" target="_blank" rel="noopener"><img src="${url}" alt="${esc(a.name)}" loading="lazy"></a></div>`;
  if (a.kind === 'video') return `<div class="att"><video src="${url}" controls preload="metadata"></video></div>`;
  if (a.kind === 'audio') return `<div class="att"><audio src="${url}" controls preload="metadata"></audio></div>`;
  return `<div class="att"><a class="file" href="${url}" target="_blank" rel="noopener">${I.file}<span>${esc(a.name)}<br><small>${fmtSize(a.size)}</small></span></a></div>`;
}

function msgHTML(m, prev, s, c) {
  if (S.blocks.has(m.author.id) && !S.revealed.has(m.id)) {
    return `<div class="msg first blocked"><div class="gut"></div><div class="body"><button class="link" data-act="reveal" data-id="${m.id}">Mensagem de alguém que você bloqueou · mostrar</button></div></div>`;
  }
  const mine = m.author.id === S.me.id;
  const canDelete = mine || (c.type !== 'dm' && c.perms & P.MANAGE_MESSAGES);
  const grouped = prev && prev.author.id === m.author.id && m.created - prev.created < 300000 && !m.reply;
  const member = s?.members.find(x => x.id === m.author.id);
  const author = S.users.get(m.author.id) || m.author;
  const body = S.editing === m.id
    ? `<textarea class="edit" data-id="${m.id}" maxlength="4000">${esc(m.content)}</textarea><small class="hint">Enter para salvar · Esc para cancelar</small>`
    : m.content ? `<div class="text">${md(m.content)}${m.edited ? ' <span class="edited">(editado)</span>' : ''}</div>` : '';
  const rx = m.reactions.length
    ? `<div class="rx">${m.reactions.map(r => `<button class="${r.users.includes(S.me.id) ? 'on' : ''}" data-act="react" data-id="${m.id}" data-emoji="${esc(r.emoji)}" title="${esc(r.users.map(id => nameOf(S.users.get(id))).join(', '))}">${esc(r.emoji)} ${r.users.length}</button>`).join('')}</div>`
    : '';
  const reply = m.reply ? `<div class="replyref">${I.reply}<b>${esc(nameOf(m.reply.author))}</b><span>${esc(m.reply.content || 'anexo')}</span></div>` : '';
  const tools = `<div class="tools">
    <button data-act="pickReact" data-id="${m.id}" title="Reagir" aria-label="Reagir">${I.smile}</button>
    <button data-act="reply" data-id="${m.id}" title="Responder" aria-label="Responder">${I.reply}</button>
    ${mine ? `<button data-act="edit" data-id="${m.id}" title="Editar" aria-label="Editar">${I.edit}</button>` : ''}
    ${!mine && c.type !== 'dm' ? `<button data-act="report" data-id="${m.id}" title="Denunciar" aria-label="Denunciar">${I.flag}</button>` : ''}
    ${canDelete ? `<button class="del" data-act="delete" data-id="${m.id}" title="Apagar" aria-label="Apagar">${I.trash}</button>` : ''}</div>`;
  return `<div class="msg ${grouped ? '' : 'first'} ${mentionsMe(m.content) ? 'hl' : ''}">${reply}
    <div class="gut">${grouped ? `<span class="gt">${new Date(m.created).toLocaleTimeString('pt-BR', { hour: '2-digit', minute: '2-digit' })}</span>` : avatar(author, 38)}</div>
    <div class="body">${grouped ? '' : `<div class="meta"><b data-act="profile" data-id="${m.author.id}" style="color:${esc(roleColor(s, member))}">${esc(nameOf(author))}</b><time>${fmtTime(m.created)}</time></div>`}
      ${body}${m.attachments.map(attachmentHTML).join('')}${rx}</div>${tools}</div>`;
}

function renderMsgs(mode) {
  const box = $('#msgs');
  const c = channel();
  if (!box || !c) return;
  const st = S.msgs.get(c.id) || { list: [], more: false, loading: true };
  const atBottom = box.scrollHeight - box.scrollTop - box.clientHeight < 90;
  const fromBottom = box.scrollHeight - box.scrollTop;
  const s = server();
  let html = st.more
    ? '<button class="more" data-act="older">Carregar mensagens anteriores</button>'
    : st.loading ? '' : `<div class="start"><h3>${c.type === 'dm' ? esc(nameOf(c.user)) : '#' + esc(c.name)}</h3>${c.type === 'dm' ? 'Este é o começo da sua conversa.' : 'Este é o começo do canal.'}</div>`;
  let prev = null, lastDay = '';
  for (const m of st.list) {
    const day = new Date(m.created).toLocaleDateString('pt-BR', { day: 'numeric', month: 'long', year: 'numeric' });
    if (day !== lastDay) {
      html += `<div class="day">${day}</div>`;
      lastDay = day;
      prev = null;
    }
    html += msgHTML(m, prev, s, c);
    prev = m;
  }
  box.innerHTML = html;
  S.stick = mode === 'bottom' || (mode !== 'keep' && atBottom);
  if (S.stick) box.scrollTop = box.scrollHeight;
  else if (mode === 'keep') box.scrollTop = box.scrollHeight - fromBottom;
  const edit = $('textarea.edit', box);
  if (edit) {
    edit.focus();
    edit.setSelectionRange(edit.value.length, edit.value.length);
  }
}

function renderExtras() {
  const el = $('#extras');
  if (!el) return;
  const reply = S.reply ? `<div class="bar">${I.reply}<span>Respondendo a <b>${esc(nameOf(S.reply.author))}</b></span><button data-act="cancelReply" aria-label="Cancelar resposta">${I.x}</button></div>` : '';
  const chips = S.files.length
    ? `<div class="chips">${S.files.map((f, i) => `<span class="chip">${I.file}<span>${esc(f.name)}</span><button data-act="removeFile" data-i="${i}" aria-label="Remover ${esc(f.name)}">${I.x}</button></span>`).join('')}</div>`
    : '';
  el.innerHTML = reply + chips;
}

function renderTyping() {
  const el = $('#typing');
  if (!el) return;
  const who = [...(S.typing.get(S.view.channelId)?.keys() || [])].map(id => nameOf(S.users.get(id)));
  el.textContent = !who.length ? '' : who.length === 1 ? `${who[0]} está digitando…` : who.length < 4 ? `${who.join(', ')} estão digitando…` : 'Várias pessoas estão digitando…';
}

function renderFriends() {
  const all = [...S.friends.values()];
  const tab = S.friendTab;
  const pending = all.filter(f => f.state !== 'accepted');
  const tabs = [['online', 'Online'], ['all', 'Todos'], ['pending', `Pendentes${pending.length ? ` (${pending.length})` : ''}`], ['blocked', 'Bloqueados']]
    .map(([k, label]) => `<button class="tab ${tab === k ? 'on' : ''}" data-act="friendTab" data-tab="${k}">${label}</button>`).join('');
  let body;
  if (tab === 'blocked') {
    const ids = [...S.blocks];
    body = `<h4>Bloqueados — ${ids.length}</h4>` + (ids.length ? ids.map(id => {
      const u = S.users.get(id) || { id, username: '?', display_name: 'Usuário' };
      return `<div class="frow">${avatar(u, 36)}<div class="who"><b>${esc(nameOf(u))}</b><small>@${esc(u.username)}</small></div><button class="btn sm ghost" data-act="unblock" data-id="${id}">Desbloquear</button></div>`;
    }).join('') : '<p style="color:var(--muted)">Você não bloqueou ninguém.</p>');
  } else if (tab === 'add') {
    body = `<h4>Adicionar amigo</h4><p style="color:var(--muted);margin:0">Digite o nome de usuário exato da pessoa.</p>
      <form class="addf" data-form="addFriend"><input class="input" name="username" placeholder="nome.de.usuario" autocomplete="off" required><button class="btn">Enviar pedido</button></form><div class="err"></div>`;
  } else {
    const list = tab === 'pending' ? pending : all.filter(f => f.state === 'accepted' && (tab === 'all' || f.user.status !== 'offline'));
    const label = { online: 'Online', all: 'Todos os amigos', pending: 'Pedidos pendentes' }[tab];
    body = `<h4>${label} — ${list.length}</h4>` + (list.length ? list.map(f => {
      const sub = f.state === 'incoming' ? 'Quer ser seu amigo' : f.state === 'outgoing' ? 'Pedido enviado' : STATUS[f.user.status];
      const acts = f.state === 'accepted'
        ? `<button class="btn sm ghost" data-act="openDm" data-id="${f.user.id}">Mensagem</button><button class="btn sm ghost" data-act="unfriend" data-id="${f.user.id}">Remover</button>`
        : f.state === 'incoming'
          ? `<button class="btn sm" data-act="acceptFriend" data-id="${f.user.id}">Aceitar</button><button class="btn sm ghost" data-act="unfriend" data-id="${f.user.id}">Recusar</button>`
          : `<button class="btn sm ghost" data-act="unfriend" data-id="${f.user.id}">Cancelar</button>`;
      return `<div class="frow">${avatar(f.user, 36, true, 'var(--bg2)')}<div class="who"><b>${esc(nameOf(f.user))}</b><small>@${esc(f.user.username)} · ${esc(sub)}</small></div>${acts}</div>`;
    }).join('') : '<p style="color:var(--muted)">Ninguém por aqui ainda.</p>');
  }
  $('#main').innerHTML = `<div class="head">${NAV}${I.users}<h2>Amigos</h2><div class="tabs">${tabs}<button class="tab add" data-act="friendTab" data-tab="add">Adicionar amigo</button></div></div><div class="page">${body}</div>`;
}

// Voice channel view. Voice calls this with no argument when a track changes,
// so it must do nothing unless a voice channel is actually on screen.
function renderStage(force) {
  const c = channel();
  if (!c || c.type !== 'voice') return;
  if (!force && !$('.stage')) return;
  const s = server();
  const users = s.voice[c.id] || [];
  const joined = Voice.channelId === c.id;
  const tiles = users.map(v => {
    const u = S.users.get(v.userId);
    const showVideo = joined && v.video;
    return `<div class="tile" data-speak="${v.userId}">${showVideo ? `<video data-vid="${v.userId}" data-act="fullscreen" title="Clique para tela cheia" autoplay playsinline muted></video>` : avatar(u, 84)}
      <span class="tag">${v.deaf ? I.headOff : v.mute ? I.micOff : ''}${esc(nameOf(u))}${v.video === 'screen' ? ' · tela' : ''}</span></div>`;
  }).join('');
  const canJoin = c.perms & P.CONNECT;
  $('#main').innerHTML = `<div class="head">${NAV}${I.speaker}<h2>${esc(c.name)}</h2><span class="sp"></span><button class="ib ${S.showMembers ? 'on' : ''}" data-act="toggleMembers" title="Lista de membros" aria-label="Mostrar ou ocultar membros">${I.users}</button></div>
    ${users.length ? `<div class="stage">${tiles}</div>` : `<div class="empty stage" style="display:grid"><div><h3>Ninguém na sala</h3>Entre para começar a conversa.</div></div>`}
    <div class="stagebar">${joined ? voiceButtons() : canJoin ? '<button class="btn" data-act="joinVoice">Entrar na chamada</button>' : '<span style="color:var(--muted)">Você não tem permissão para entrar neste canal.</span>'}</div>`;
  for (const el of document.querySelectorAll('video[data-vid]')) {
    const stream = Voice.streamFor(Number(el.dataset.vid));
    if (stream) el.srcObject = stream;
  }
}

function refreshVoice() {
  renderSidebar();
  renderVoicePanel();
  renderStage();
}

// ------------------------------------------------------------------ overlays
function modal(render, cls = '') {
  closePop();
  S.modal = { render, cls };
  drawModal();
  $('#layer input:not([type=hidden]):not([type=checkbox]):not([type=color]):not([type=file])')?.focus();
}
function drawModal() {
  if (!S.modal) return;
  const html = S.modal.render();
  if (html == null) return closeModal();
  $('#layer').innerHTML = `<div class="scrim" data-act="scrim"><div class="modal ${S.modal.cls}" role="dialog" aria-modal="true">${html}</div></div>`;
}
function closeModal() {
  S.modal = null;
  $('#layer').innerHTML = '';
}
function pop(anchor, html, cls = '') {
  closePop();
  const el = document.createElement('div');
  el.className = 'pop ' + cls;
  el.innerHTML = html;
  document.body.append(el);
  const r = anchor.getBoundingClientRect();
  const w = el.offsetWidth, h = el.offsetHeight;
  el.style.left = Math.max(8, Math.min(r.left, innerWidth - w - 8)) + 'px';
  el.style.top = (r.bottom + h + 8 > innerHeight ? Math.max(8, r.top - h - 6) : r.bottom + 6) + 'px';
  S.pop = el;
}
function closePop() {
  S.pop?.remove();
  S.pop = null;
}
function confirmBox(title, text, label, action) {
  S.confirm = action;
  modal(() => `<h3>${esc(title)}</h3><p class="sub">${esc(text)}</p><div class="foot"><button class="btn ghost" data-act="closeModal">Cancelar</button><button class="btn danger" data-act="confirm">${esc(label)}</button></div>`);
}

const field = (label, attrs, value = '') => `<label class="f">${label}<input ${attrs} value="${esc(value)}"></label>`;

function hostInfo(h) {
  if (!h?.enabled) return '<p class="sub">Desligado. O app se conecta ao endereço abaixo.</p>';
  if (h.error) return `<p class="sub" style="color:var(--danger)">Não foi possível iniciar: ${esc(h.error)}</p>`;
  if (h.external) return `<p class="sub">Já existe um servidor do Babu Tela rodando na porta ${h.port} neste computador; o app está usando ele.</p>`;
  return `<p class="sub">Rodando na porta ${h.port}. ${h.addresses.length
    ? `Quem estiver na mesma rede entra colocando <b>${h.addresses.map(esc).join('</b> ou <b>')}</b> em “Endereço do servidor”. Na primeira vez, permita o Babu Tela no aviso do Firewall do Windows.`
    : 'Nenhuma rede encontrada para outras pessoas se conectarem.'}</p>`;
}

const soundSettings = () => `<details class="sounds"><summary>Sons <small>${Object.keys(SOUNDS).filter(Sounds.enabled).length} de ${Object.keys(SOUNDS).length} ligados</small></summary>
  ${Object.entries(SOUNDS).map(([k, cue]) => `<label class="check"><button type="button" class="ib" data-act="previewSound" data-sound="${k}" title="Ouvir" aria-label="Ouvir: ${esc(cue.label)}">▶</button><span>${esc(cue.label)}</span><input type="checkbox" data-change="sound" data-sound="${k}" ${Sounds.enabled(k) ? 'checked' : ''}></label>`).join('')}</details>`;

// Configurações → Servidor público: this PC answers at https://name.duckdns.org.
const PUBLIC_CHECKS = [['ip', 'IP público'], ['dns', 'Endereço (DuckDNS)'], ['ports', 'Portas do roteador'], ['turn', 'Relé das chamadas'], ['https', 'HTTPS']];
function publicSection(p) {
  if (!p) return '';
  const on = p.enabled;
  const lines = PUBLIC_CHECKS.filter(([k]) => p.checks?.[k] || (k === 'ip' && p.ip)).map(([k, label]) => {
    const c = k === 'ip' ? { ok: true, msg: p.ip } : p.checks[k];
    return `<div class="check" style="align-items:flex-start"><span>${label}<small>${esc(c.msg)}</small></span><b style="color:${c.ok ? 'var(--online)' : 'var(--danger)'}">${c.ok ? 'ok' : 'falhou'}</b></div>`;
  }).join('');
  const state = { off: 'Desligado.', starting: 'Ligando… isso pode levar até 2 minutos.', online: `No ar em <b>${esc(p.url)}</b>.`, error: '<span style="color:var(--danger)">Com problema; veja abaixo.</span>' }[p.state] || '';
  return `<p class="sub">Este computador é o servidor do Babu Tela na internet. Seus amigos instalam a versão deles do app, criam a conta e entram. Deixe este PC ligado com o Babu Tela aberto (pode ficar na bandeja).</p>
    <p class="sub"><b>${on ? state : 'Desligado.'}</b></p>${on ? lines : ''}
    ${p.baked ? "<p class=\"sub\">Endereço e token já vêm configurados neste app.</p>" : `<form data-form="publicServer" style="margin-top:10px">
      ${field('Endereço', 'name="domain" required placeholder="seunome.duckdns.org"', p.domain || '')}
      ${field(p.hasToken ? 'Token do DuckDNS (já guardado; cole só para trocar)' : 'Token do DuckDNS', 'name="token" type="password" autocomplete="off"' + (p.hasToken ? '' : ' placeholder="aparece no topo de duckdns.org"'))}
      ${field('E-mail para avisos do certificado (opcional)', 'name="email" type="email"', p.email || '')}
      <label class="check"><span>Abrir as portas no roteador automaticamente (UPnP)<small>33443 (TCP), 3478 e 50200–50239 (UDP), apontando para este PC</small></span><input type="checkbox" name="upnp" ${p.upnp || !on ? 'checked' : ''}></label>
      <div class="err"></div><button class="btn sm">${on ? 'Salvar e religar' : 'Ligar servidor'}</button></form>`}`;
}

function watchPublic() {
  clearTimeout(S.pubTimer);
  if (!native?.publicServer || !S.modal) return;
  S.pubTimer = setTimeout(async () => {
    S.publicServer = await native.publicServer.status();
    if (S.modal) drawModal();
    if (S.publicServer.state === 'starting') watchPublic();
  }, 2000);
}

const qualitySelect = () => `<label class="f">Qualidade da transmissão<select data-change="quality">${Object.entries(QUALITY).map(([k, q]) => `<option value="${k}" ${Voice.quality === q ? 'selected' : ''}>${q.label}</option>`).join('')}</select></label>`;

const REPORT_REASONS = [['spam', 'Spam'], ['assedio', 'Assédio ou ódio'], ['improprio', 'Conteúdo impróprio'], ['outro', 'Outro motivo']];

const MODALS = {
  report: mid => () => `<h3>Denunciar mensagem</h3><p class="sub">A denúncia vai para a moderação deste servidor. Quem escreveu não fica sabendo quem denunciou.</p>
    <form data-form="report" data-id="${mid}">
      <label class="f">Motivo<select name="reason">${REPORT_REASONS.map(([k, l]) => `<option value="${k}">${l}</option>`).join('')}</select></label>
      <label class="f">Detalhes (opcional)<textarea name="detail" maxlength="300" rows="3"></textarea></label>
      <div class="err"></div><div class="foot"><button type="button" class="btn ghost" data-act="closeModal">Cancelar</button><button class="btn danger">Enviar denúncia</button></div></form>`,

  reports: sid => () => {
    const s = S.servers.get(sid);
    if (!s) return null;
    const list = s.reports || [];
    return `<h3>Denúncias</h3><p class="sub">${esc(s.name)} · ${list.length ? `${list.length} pendente${list.length > 1 ? 's' : ''}` : 'nenhuma pendente'}</p>
      ${list.map(r => `<div class="report">
        <div class="frow" style="border:0;padding:0 0 6px">${avatar(r.message.author, 30)}<div class="who"><b>${esc(nameOf(r.message.author))}</b><small>em #${esc(r.channel || '?')} · ${fmtTime(r.message.created)}</small></div></div>
        <div class="quote">${md(r.message.content) || '<i>anexo</i>'}</div>
        <p class="sub" style="margin:6px 0">Motivo: <b>${esc(r.reason)}</b> · denunciado por ${esc(nameOf(r.reporter))}</p>
        <div class="foot" style="margin-top:6px"><button class="btn sm ghost" data-act="dismissReport" data-id="${r.id}">Ignorar</button>
          ${can(P.BAN, s) && r.message.author.id !== s.owner_id ? `<button class="btn sm danger" data-act="banAuthor" data-id="${r.message.author.id}">Banir autor</button>` : ''}
          <button class="btn sm danger" data-act="deleteReported" data-id="${r.message.id}">Apagar mensagem</button></div></div>`).join('')}
      <div class="foot"><button class="btn ghost" data-act="closeModal">Fechar</button></div>`;
  },

  deleteAccount: () => `<h3>Excluir minha conta</h3>
    <p class="sub">Isso não pode ser desfeito. Serão apagados para sempre:</p>
    <ul class="sub" style="margin:0 0 14px;padding-left:18px"><li>sua conta e seu perfil;</li><li>todas as suas mensagens e arquivos enviados;</li><li>suas conversas diretas;</li><li>os servidores que você criou, com tudo o que há neles.</li></ul>
    <form data-form="deleteAccount">${field('Digite sua senha para confirmar', 'name="password" type="password" autocomplete="current-password" required')}
      <div class="err"></div><div class="foot"><button type="button" class="btn ghost" data-act="closeModal">Cancelar</button><button class="btn danger">Excluir para sempre</button></div></form>`,

  addServer: () => `<h3>Criar um servidor</h3><p class="sub">Um espaço para sua comunidade conversar.</p>
    <form data-form="createServer">${field('Nome do servidor', 'name="name" maxlength="60" required')}<div class="err"></div><button class="btn wide">Criar servidor</button></form>
    <hr><h3>Entrar em um servidor</h3><p class="sub">Cole o código de convite que você recebeu.</p>
    <form data-form="joinServer">${field('Código do convite', 'name="code" required autocomplete="off"')}<div class="err"></div><button class="btn ghost wide">Entrar</button></form>`,

  newChannel: (sid, cat, type) => () => `<h3>Criar canal</h3><p class="sub">em ${esc(S.servers.get(sid)?.name)}</p>
    <form data-form="createChannel" data-sid="${sid}">
      <label class="f">Tipo<select name="type"><option value="text" ${type !== 'voice' ? 'selected' : ''}>Texto</option><option value="voice" ${type === 'voice' ? 'selected' : ''}>Voz</option></select></label>
      ${field('Nome do canal', 'name="name" maxlength="40" required')}
      ${field('Categoria', 'name="category" maxlength="40"', cat || '')}
      <div class="err"></div><div class="foot"><button type="button" class="btn ghost" data-act="closeModal">Cancelar</button><button class="btn">Criar canal</button></div></form>`,

  editChannel: cid => () => {
    const s = server();
    const c = s?.channels.find(x => x.id === cid);
    if (!c) return null;
    const bits = c.type === 'voice' ? [[P.VIEW, 'Ver canal'], [P.CONNECT, 'Conectar']] : [[P.VIEW, 'Ver canal'], [P.SEND, 'Enviar'], [P.ATTACH, 'Anexar']];
    const rows = s.roles.map(r => {
      const o = c.overrides.find(x => x.role_id === r.id) || { allow: 0, deny: 0 };
      const cells = bits.map(([bit]) => {
        const v = o.allow & bit ? 'a' : o.deny & bit ? 'd' : '';
        return `<td><select data-change="override" data-cid="${c.id}" data-rid="${r.id}" data-bit="${bit}" aria-label="Permissão"><option value="" ${v === '' ? 'selected' : ''}>Padrão</option><option value="a" ${v === 'a' ? 'selected' : ''}>Permitir</option><option value="d" ${v === 'd' ? 'selected' : ''}>Negar</option></select></td>`;
      }).join('');
      return `<tr><td><span class="pill" style="--c:${esc(r.color || '')}"><i></i>${esc(r.name)}</span></td>${cells}</tr>`;
    }).join('');
    return `<h3>Editar canal</h3><p class="sub">${c.type === 'voice' ? 'Canal de voz' : 'Canal de texto'}</p>
      <form data-form="editChannel" data-cid="${c.id}">${field('Nome', 'name="name" maxlength="40" required', c.name)}
        ${c.type === 'text' ? field('Assunto', 'name="topic" maxlength="200"', c.topic || '') : ''}${field('Categoria', 'name="category" maxlength="40"', c.category || '')}
        <div class="err"></div><button class="btn">Salvar</button></form>
      ${can(P.MANAGE_ROLES) ? `<hr><h3>Permissões deste canal</h3><p class="sub">“Negar” em @todos esconde o canal de quem não tiver um cargo com “Permitir”.</p>
        <table class="otable"><tr><th>Cargo</th>${bits.map(b => `<th>${b[1]}</th>`).join('')}</tr>${rows}</table>` : ''}
      <div class="foot"><button class="btn danger" data-act="deleteChannel" data-id="${c.id}">Excluir canal</button><span style="flex:1"></span><button class="btn ghost" data-act="closeModal">Fechar</button></div>`;
  },

  invite: code => () => `<h3>Convidar pessoas</h3><p class="sub">Envie este código. Quem receber entra pelo botão “+” na barra lateral.</p>
    <div class="invite"><input class="input" id="invite-code" readonly value="${esc(code)}" aria-label="Código do convite"><button class="btn" data-act="copyInvite">Copiar</button></div>
    <div class="foot"><button class="btn ghost" data-act="closeModal">Fechar</button></div>`,

  serverSettings: sid => () => {
    const s = S.servers.get(sid);
    if (!s) return null;
    const sel = s.roles.find(r => r.id === S.roleSel) || s.roles[0];
    const rolesUI = can(P.MANAGE_ROLES, s) ? `<hr><h3>Cargos</h3><p class="sub">Cargos definem o que cada membro pode fazer.</p>
      <div class="split"><div class="list">${s.roles.map(r => `<button class="row ${r.id === sel.id ? 'on' : ''}" data-act="selectRole" data-id="${r.id}"><span class="pill" style="--c:${esc(r.color || '')}"><i></i>${esc(r.name)}</span></button>`).join('')}
          <form data-form="createRole" data-sid="${s.id}" style="margin-top:8px"><input class="input" name="name" placeholder="Novo cargo" maxlength="32" required aria-label="Nome do novo cargo"><button class="btn sm ghost" style="margin-top:6px;width:100%">Criar cargo</button></form></div>
        <form data-form="saveRole" data-rid="${sel.id}">
          ${sel.is_everyone ? '<p class="sub">Permissões padrão de todos os membros.</p>' : `${field('Nome do cargo', 'name="name" maxlength="32" required', sel.name)}<label class="f">Cor<input type="color" name="color" value="${esc(sel.color || '#969ba8')}" style="height:38px;padding:4px"></label>`}
          ${PERM_LABELS.map(([bit, label, hint]) => `<label class="check"><span>${label}${hint ? `<small>${hint}</small>` : ''}</span><input type="checkbox" name="p${bit}" ${sel.perms & bit ? 'checked' : ''} ${s.perms & bit ? '' : 'disabled'}></label>`).join('')}
          <div class="err"></div><div class="foot">${sel.is_everyone ? '' : `<button type="button" class="btn danger" data-act="deleteRole" data-id="${sel.id}">Excluir cargo</button>`}<button class="btn">Salvar cargo</button></div></form></div>` : '';
    const bansUI = can(P.BAN, s) ? `<hr><h3>Banimentos</h3>${s.bans.length ? s.bans.map(u => `<div class="frow">${avatar(u, 30)}<div class="who"><b>${esc(nameOf(u))}</b><small>@${esc(u.username)}</small></div><button class="btn sm ghost" data-act="unban" data-sid="${s.id}" data-id="${u.id}">Remover banimento</button></div>`).join('') : '<p class="sub">Ninguém foi banido.</p>'}` : '';
    return `<h3>Configurações do servidor</h3>
      ${can(P.MANAGE_SERVER, s) ? `<form data-form="renameServer" data-sid="${s.id}" style="margin-top:12px">${field('Nome do servidor', 'name="name" maxlength="60" required', s.name)}<div class="err"></div><button class="btn">Salvar nome</button></form>` : ''}
      ${rolesUI}${bansUI}
      <div class="foot">${s.owner_id === S.me.id ? `<button class="btn danger" data-act="deleteServer" data-id="${s.id}">Excluir servidor</button><span style="flex:1"></span>` : ''}<button class="btn ghost" data-act="closeModal">Fechar</button></div>`;
  },

  profile: uid => () => {
    const s = server();
    const member = s?.members.find(m => m.id === uid);
    const u = member || S.users.get(uid);
    if (!u) return null;
    const me = uid === S.me.id;
    const friend = S.friends.get(uid);
    const roles = member ? s.roles.filter(r => member.roles.includes(r.id)) : [];
    const manage = member && !me && can(P.MANAGE_ROLES) && s.roles.some(r => !r.is_everyone)
      ? `<hr><div style="text-align:left"><b>Cargos</b>${s.roles.filter(r => !r.is_everyone).map(r => `<label class="check"><span><span class="pill" style="--c:${esc(r.color || '')}"><i></i>${esc(r.name)}</span></span><input type="checkbox" data-change="memberRole" data-uid="${uid}" data-rid="${r.id}" ${member.roles.includes(r.id) ? 'checked' : ''} ${r.perms & ~s.perms ? 'disabled' : ''}></label>`).join('')}</div>` : '';
    const mod = member && !me && uid !== s.owner_id && (can(P.KICK) || can(P.BAN))
      ? `<hr><div class="foot" style="justify-content:center">${can(P.KICK) ? `<button class="btn sm danger" data-act="kick" data-id="${uid}">Expulsar</button>` : ''}${can(P.BAN) ? `<button class="btn sm danger" data-act="ban" data-id="${uid}">Banir</button>` : ''}</div>` : '';
    return `<div class="profile">${avatar(u, 84, true, 'var(--bg2)')}<h3>${esc(nameOf(u))}</h3><span style="color:var(--muted)">@${esc(u.username)} · ${esc(STATUS[u.status] || 'Offline')}</span>
      ${roles.length ? `<div class="roles">${roles.map(r => `<span class="pill" style="--c:${esc(r.color || '')}"><i></i>${esc(r.name)}</span>`).join('')}</div>` : '<div style="height:12px"></div>'}
      ${me ? '' : S.blocks.has(uid)
        ? `<p class="sub">Você bloqueou essa pessoa.</p><div class="foot" style="justify-content:center;margin-top:0"><button class="btn ghost" data-act="unblock" data-id="${uid}">Desbloquear</button></div>`
        : `<div class="foot" style="justify-content:center;margin-top:0"><button class="btn" data-act="openDm" data-id="${uid}">Mensagem</button>${friend ? '' : `<button class="btn ghost" data-act="addFriendUser" data-name="${esc(u.username)}">Adicionar amigo</button>`}<button class="btn ghost" data-act="block" data-id="${uid}">Bloquear</button></div>`}
      ${manage}${mod}</div>`;
  },

  settings: () => {
    const me = S.me;
    const d = S.desktop;
    const toggle = (key, label, hint) => `<label class="check"><span>${label}<small>${hint}</small></span><input type="checkbox" data-change="desktop" data-key="${key}" ${d[key] ? 'checked' : ''}></label>`;
    const desktop = native ? `<hr><h3>Aplicativo</h3>
        ${toggle('notifications', 'Notificações', 'Avisar sobre mensagens diretas e menções')}
        ${toggle('minimizeToTray', 'Minimizar para a bandeja', 'Fechar a janela mantém o app rodando')}
        ${toggle('openOnStartup', 'Iniciar com o Windows', 'Abrir o Babu Tela quando você entrar no computador')}
        ${toggle('startMinimized', 'Iniciar minimizado', 'Abrir direto na bandeja')}
        ${native.public ? '' : `<hr><h3>Servidor</h3>${publicSection(S.publicServer)}`}
        <p class="sub" style="margin-top:14px">Versão ${esc(native.version)} · <button class="link" data-act="checkUpdate">Procurar atualizações</button></p>`
      : `<hr><h3>Notificações</h3><label class="check"><span>Notificações do navegador<small>Avisar sobre mensagens diretas e menções</small></span><input type="checkbox" data-change="webNotify" ${store.get('notify') === '1' ? 'checked' : ''}></label>`;
    const security = `<hr><h3>Segurança</h3>
      <form data-form="password">${field('Senha atual', 'name="current" type="password" autocomplete="current-password" required')}${field('Nova senha', 'name="password" type="password" autocomplete="new-password" minlength="6" required')}<div class="err"></div><button class="btn sm">Trocar senha</button></form>
      <p class="sub" style="margin-top:14px">${S.sessions ? `Sua conta está conectada em <b>${S.sessions}</b> ${S.sessions === 1 ? 'dispositivo' : 'dispositivos'}.` : ''} <button class="link" data-act="logoutOthers">Sair de todos os outros dispositivos</button></p>
      <button class="btn sm danger" data-act="deleteAccount">Excluir minha conta</button>`;
    const media = `<hr><h3>Voz e vídeo</h3>${soundSettings()}${qualitySelect()}<p class="sub">Vale para câmera e compartilhamento de tela, e pode ser trocada durante a chamada. Cada pessoa assistindo recebe uma cópia do seu vídeo, então as opções mais altas pedem mais velocidade de upload.</p>`;
    return `<h3>Configurações</h3><p class="sub">@${esc(me.username)}</p>
      <div style="display:flex;gap:16px;align-items:center;margin-bottom:14px">${avatar(me, 64)}<div><button class="btn sm ghost" data-act="pickAvatar">Trocar foto</button> ${me.avatar ? '<button class="btn sm ghost" data-act="removeAvatar">Remover</button>' : ''}<input type="file" id="avatarpick" accept="image/png,image/jpeg,image/gif,image/webp" data-change="avatar" hidden></div></div>
      <form data-form="profile">${field('Nome de exibição', 'name="displayName" maxlength="32" required', me.display_name)}<div class="err"></div><button class="btn sm">Salvar nome</button></form>
      <label class="f" style="margin-top:14px">Status<select data-change="status">${['online', 'idle', 'dnd', 'invisible'].map(k => `<option value="${k}" ${me.status === k ? 'selected' : ''}>${STATUS[k]}</option>`).join('')}</select></label>
      ${media}${desktop}${security}
      <div class="foot"><button class="btn danger" data-act="logout">Sair da conta</button><span style="flex:1"></span><button class="btn ghost" data-act="closeModal">Fechar</button></div>`;
  },

  screens: sources => () => `<h3>Compartilhar tela</h3><p class="sub">Escolha o que as outras pessoas vão ver.</p>${qualitySelect()}
    <div class="sources">${sources.map(s => `<button data-act="pickScreen" data-id="${esc(s.id)}"><img src="${esc(s.thumb)}" alt=""><span>${esc(s.name)}</span></button>`).join('')}</div>
    <div class="foot"><button class="btn ghost" data-act="pickScreen" data-id="">Cancelar</button></div>`,
};

// ------------------------------------------------------------------ actions
const A = {
  winMin: () => native.window.minimize(),
  winMax: () => native.window.maximize(),
  winClose: () => native.window.close(),
  scrim: (el, e) => e.target === el && (S.modal?.cls.includes('sticky') ? null : closeModal()),
  closeModal,
  confirm: async () => {
    const fn = S.confirm;
    closeModal();
    await fn();
  },
  spoiler: el => el.classList.add('show'),
  authMode: el => showAuth(el.dataset.mode),
  home: () => goTo({ type: 'home', channelId: null }),
  friends: () => {
    goTo({ type: 'home', channelId: null });
    showConversation();
  },
  friendTab: el => {
    S.friendTab = el.dataset.tab;
    renderMain();
    $('.addf input')?.focus();
  },
  dm: el => {
    goTo({ type: 'home', channelId: +el.dataset.id });
    showConversation();
  },
  server: el => goTo({ type: 'server', serverId: +el.dataset.id }),
  channel: el => {
    const c = server().channels.find(x => x.id === +el.dataset.id);
    goTo({ type: 'server', serverId: S.view.serverId, channelId: c.id });
    showConversation();
    if (c.type === 'voice' && c.perms & P.CONNECT) joinVoice(c.id);
  },
  joinVoice: () => joinVoice(S.view.channelId),
  voiceGoto: () => {
    const loc = voiceLocation();
    if (!loc) return;
    goTo({ type: 'server', serverId: loc.s.id, channelId: loc.c.id });
    showConversation();
  },
  mute: () => Voice.toggleMute(),
  deafen: () => Voice.toggleDeaf(),
  camera: () => Voice.setVideo(Voice.video === 'camera' ? null : 'camera'),
  share: () => Voice.setVideo(Voice.video === 'screen' ? null : 'screen'),
  hangup: () => {
    Voice.leave();
    refreshVoice();
  },
  nav: () => showNav(),
  toggleMembers: () => {
    S.showMembers = !S.showMembers;
    renderMain();
    renderMembers();
  },
  addServer: () => modal(MODALS.addServer),
  serverMenu: el => {
    const s = server();
    const items = [];
    if (can(P.INVITE)) items.push(`<button data-act="invite">Convidar pessoas</button>`);
    if (can(P.MANAGE_CHANNELS)) items.push(`<button data-act="newChannel">Criar canal</button>`);
    if (can(P.MANAGE_SERVER) || can(P.MANAGE_ROLES) || can(P.BAN)) items.push(`<button data-act="serverSettings">Configurações do servidor</button>`);
    if (can(P.MANAGE_MESSAGES)) items.push(`<button data-act="reports">Denúncias${s.reports?.length ? ` (${s.reports.length})` : ''}</button>`);
    if (s.owner_id !== S.me.id) items.push(`<button class="danger" data-act="leaveServer">Sair do servidor</button>`);
    pop(el, items.join('') || '<button data-act="closePop">Nada disponível</button>', 'menu');
  },
  closePop,
  invite: async () => {
    closePop();
    const { code } = await api('POST', `/servers/${S.view.serverId}/invites`);
    modal(MODALS.invite(code));
  },
  copyInvite: async () => {
    const input = $('#invite-code');
    input.select();
    try {
      await navigator.clipboard.writeText(input.value);
      toast('Código copiado.');
    } catch {
      toast('Selecione o código e copie com Ctrl+C.');
    }
  },
  newChannel: el => modal(MODALS.newChannel(S.view.serverId, el.dataset.cat, el.dataset.type)),
  editChannel: (el, e) => {
    e.stopPropagation();
    modal(MODALS.editChannel(+el.dataset.id), 'wide');
  },
  deleteChannel: el => confirmBox('Excluir canal', 'O canal e todas as mensagens dele serão apagados. Isso não pode ser desfeito.', 'Excluir canal',
    () => api('DELETE', `/channels/${el.dataset.id}`)),
  serverSettings: () => {
    S.roleSel = null;
    modal(MODALS.serverSettings(S.view.serverId), 'wide');
  },
  selectRole: el => {
    S.roleSel = +el.dataset.id;
    drawModal();
  },
  deleteRole: async el => {
    await api('DELETE', `/roles/${el.dataset.id}`);
    S.roleSel = null;
  },
  unban: el => api('DELETE', `/servers/${el.dataset.sid}/bans/${el.dataset.id}`),
  deleteServer: el => confirmBox('Excluir servidor', 'Todos os canais e mensagens serão apagados para todo mundo. Isso não pode ser desfeito.', 'Excluir servidor',
    () => api('DELETE', `/servers/${el.dataset.id}`)),
  leaveServer: () => {
    const sid = S.view.serverId;
    confirmBox('Sair do servidor', 'Você só volta com um novo convite.', 'Sair', () => api('POST', `/servers/${sid}/leave`));
  },
  profile: el => modal(MODALS.profile(+el.dataset.id)),
  kick: el => {
    const sid = S.view.serverId;
    confirmBox('Expulsar membro', 'A pessoa sai do servidor, mas pode voltar com um convite.', 'Expulsar', () => api('DELETE', `/servers/${sid}/members/${el.dataset.id}`));
  },
  ban: el => {
    const sid = S.view.serverId;
    confirmBox('Banir membro', 'A pessoa sai do servidor e não consegue voltar, mesmo com convite.', 'Banir', () => api('PUT', `/servers/${sid}/bans/${el.dataset.id}`));
  },
  openDm: async el => {
    const dm = await api('POST', '/dms', { userId: +el.dataset.id });
    if (!S.dms.has(dm.id)) S.dms.set(dm.id, dm);
    closeModal();
    goTo({ type: 'home', channelId: dm.id });
    showConversation();
  },
  addFriendUser: async el => {
    await api('POST', '/friends', { username: el.dataset.name });
    toast('Pedido de amizade enviado.');
    closeModal();
  },
  acceptFriend: el => api('PUT', `/friends/${el.dataset.id}`),
  unfriend: el => api('DELETE', `/friends/${el.dataset.id}`),
  older: () => {
    const st = S.msgs.get(S.view.channelId);
    if (st?.list.length) loadMessages(S.view.channelId, st.list[0].id);
  },
  reply: el => {
    S.reply = findMsg(+el.dataset.id);
    renderExtras();
    $('#composer textarea')?.focus();
  },
  cancelReply: () => {
    S.reply = null;
    renderExtras();
  },
  edit: el => {
    S.editing = +el.dataset.id;
    renderMsgs();
  },
  delete: el => confirmBox('Apagar mensagem', 'Essa mensagem será apagada para todo mundo.', 'Apagar', () => api('DELETE', `/messages/${el.dataset.id}`)),
  pickReact: el => pop(el, `<div class="emojis">${EMOJIS.map(e => `<button data-act="react" data-id="${el.dataset.id}" data-emoji="${e}">${e}</button>`).join('')}</div>`),
  react: async el => {
    closePop();
    const m = findMsg(+el.dataset.id);
    const mine = m?.reactions.find(r => r.emoji === el.dataset.emoji)?.users.includes(S.me.id);
    await api(mine ? 'DELETE' : 'PUT', `/messages/${el.dataset.id}/reactions/${encodeURIComponent(el.dataset.emoji)}`);
  },
  emoji: el => pop(el, `<div class="emojis">${EMOJIS.map(e => `<button data-act="emojiInsert" data-emoji="${e}">${e}</button>`).join('')}</div>`),
  emojiInsert: el => {
    const ta = $('#composer textarea');
    ta.setRangeText(el.dataset.emoji, ta.selectionStart, ta.selectionEnd, 'end');
    closePop();
    ta.focus();
  },
  attach: () => $('#filepick').click(),
  removeFile: el => {
    S.files.splice(+el.dataset.i, 1);
    renderExtras();
  },
  settings: async () => {
    if (native) {
      S.desktop = await native.settings.get();
      S.host = await native.hostStatus();
      S.publicServer = native.publicServer ? await native.publicServer.status() : null;
      if (S.publicServer?.state === 'starting') watchPublic();
    }
    S.sessions = (await api('GET', '/sessions').catch(() => null))?.count ?? null;
    modal(MODALS.settings);
  },
  pickAvatar: () => $('#avatarpick').click(),
  removeAvatar: () => api('PATCH', '/me', { avatar: null }),
  logout: () => logout(),
  reveal: el => {
    S.revealed.add(+el.dataset.id);
    renderMsgs();
  },
  report: el => modal(MODALS.report(+el.dataset.id)),
  reports: () => {
    closePop();
    modal(MODALS.reports(S.view.serverId), 'wide');
  },
  dismissReport: el => api('DELETE', `/reports/${el.dataset.id}`),
  deleteReported: el => confirmBox('Apagar mensagem', 'A mensagem denunciada será apagada para todo mundo.', 'Apagar',
    async () => {
      await api('DELETE', `/messages/${el.dataset.id}`);
      modal(MODALS.reports(S.view.serverId), 'wide');
    }),
  banAuthor: el => {
    const sid = S.view.serverId;
    confirmBox('Banir autor', 'A pessoa sai do servidor e não consegue voltar, mesmo com convite.', 'Banir', async () => {
      await api('PUT', `/servers/${sid}/bans/${el.dataset.id}`);
      modal(MODALS.reports(sid), 'wide');
    });
  },
  block: el => confirmBox('Bloquear', 'Você deixa de ver as mensagens dessa pessoa, a amizade é desfeita e nenhum dos dois consegue mandar mensagem direta ou pedido de amizade para o outro.', 'Bloquear',
    () => api('PUT', `/blocks/${el.dataset.id}`)),
  unblock: el => api('DELETE', `/blocks/${el.dataset.id}`),
  logoutOthers: async () => {
    const { ended } = await api('POST', '/sessions/logout-others');
    toast(ended ? `${ended} ${ended === 1 ? 'dispositivo desconectado' : 'dispositivos desconectados'}.` : 'Nenhum outro dispositivo estava conectado.');
    S.sessions = 1;
    drawModal();
  },
  deleteAccount: () => modal(MODALS.deleteAccount),
  previewSound: (el, e) => {
    e.preventDefault();
    Sounds.play(el.dataset.sound, true);
  },
  retryPublic: async () => {
    const p = await native.publicServer.status();
    await native.publicServer.set(true, { domain: p.domain, email: p.email, upnp: p.upnp });
  },
  openUrl: el => native?.openExternal(el.dataset.url),
  retryNow: () => {
    clearTimeout(S.retry);
    S.tries = 0;
    showConnecting(0);
    connect();
  },
  checkUpdate: async () => toast(await native.checkForUpdates()),
  installUpdate: () => native.quitAndInstall(),
  fullscreen: el => (document.fullscreenElement ? document.exitFullscreen() : el.requestFullscreen()),
  pickScreen: el => {
    native.pickScreen(el.dataset.id || null);
    closeModal();
  },
};

const findMsg = id => S.msgs.get(S.view.channelId)?.list.find(m => m.id === id);

async function joinVoice(cid) {
  await Voice.join(cid);
  refreshVoice();
}

// ------------------------------------------------------------------ forms
const F = {
  async login(d) {
    const { token } = await api('POST', '/login', d);
    startSession(token);
  },
  async register(d) {
    const { token } = await api('POST', '/register', d);
    startSession(token);
  },
  async publicServer(d) {
    S.publicServer = await native.publicServer.set(true, { domain: d.domain, token: d.token, email: d.email, upnp: d.upnp === 'on' });
    drawModal();
    watchPublic();
  },
  async createServer(d) {
    const { id } = await api('POST', '/servers', d);
    closeModal();
    arrive(id);
  },
  async joinServer(d) {
    const { id } = await api('POST', `/invites/${encodeURIComponent(d.code.trim())}/join`);
    closeModal();
    arrive(id);
  },
  async createChannel(d, f) {
    await api('POST', `/servers/${f.dataset.sid}/channels`, d);
    closeModal();
  },
  async editChannel(d, f) {
    await api('PATCH', `/channels/${f.dataset.cid}`, d);
    toast('Canal salvo.');
  },
  async renameServer(d, f) {
    await api('PATCH', `/servers/${f.dataset.sid}`, d);
    toast('Nome salvo.');
  },
  async createRole(d, f) {
    const { id } = await api('POST', `/servers/${f.dataset.sid}/roles`, d);
    S.roleSel = id;
    drawModal();
  },
  async saveRole(d, f) {
    const s = server();
    const role = s.roles.find(r => r.id === +f.dataset.rid);
    // Disabled checkboxes are permissions I cannot grant; leave those bits as they are.
    let perms = role.perms & ~s.perms;
    for (const [bit] of PERM_LABELS) if (d['p' + bit]) perms |= bit;
    await api('PATCH', `/roles/${role.id}`, { name: d.name, color: d.color, perms });
    toast('Cargo salvo.');
  },
  async profile(d) {
    await api('PATCH', '/me', d);
    toast('Nome salvo.');
  },
  async report(d, f) {
    await api('POST', `/messages/${f.dataset.id}/report`, d);
    closeModal();
    toast('Denúncia enviada para a moderação do servidor.');
  },
  async password(d, f) {
    await api('PATCH', '/me/password', d);
    f.reset();
    S.sessions = 1;
    toast('Senha trocada. Os outros dispositivos foram desconectados.');
  },
  async deleteAccount(d) {
    await api('DELETE', '/me', d);
    logout(true);
    toast('Sua conta foi excluída.');
  },
  async addFriend(d, f) {
    await api('POST', '/friends', d);
    f.reset();
    toast('Pedido de amizade enviado.');
  },
};

function arrive(sid) {
  if (S.servers.has(sid)) goTo({ type: 'server', serverId: sid });
  else S.pendingGo = sid;
}

// ------------------------------------------------------------------ change handlers
const C = {
  async override(el) {
    let allow = 0, deny = 0;
    for (const sel of el.closest('tr').querySelectorAll('select')) {
      if (sel.value === 'a') allow |= +sel.dataset.bit;
      if (sel.value === 'd') deny |= +sel.dataset.bit;
    }
    await api('PUT', `/channels/${el.dataset.cid}/overrides/${el.dataset.rid}`, { allow, deny });
  },
  memberRole: el => api(el.checked ? 'PUT' : 'DELETE', `/servers/${S.view.serverId}/members/${el.dataset.uid}/roles/${el.dataset.rid}`),
  status: el => sendOp({ op: 'status', status: el.value }),
  sound(el) {
    Sounds.setEnabled(el.dataset.sound, el.checked);
    if (el.checked) Sounds.play(el.dataset.sound);
    const sum = el.closest('details')?.querySelector('summary small');
    if (sum) sum.textContent = `${Object.keys(SOUNDS).filter(Sounds.enabled).length} de ${Object.keys(SOUNDS).length} ligados`;
  },
  quality(el) {
    store.set('quality', el.value);
    Voice.applyQuality();
  },
  async avatar(el) {
    if (!el.files[0]) return;
    const up = await uploadFile(el.files[0]);
    await api('PATCH', '/me', { avatar: up.url });
  },
  async desktop(el) {
    S.desktop = await native.settings.set({ [el.dataset.key]: el.checked });
    if (el.dataset.key === 'hostServer') {
      S.host = await native.hostStatus();
      drawModal();
    }
  },
  async webNotify(el) {
    if (el.checked && Notification.permission !== 'granted' && (await Notification.requestPermission()) !== 'granted') {
      el.checked = false;
      return toast('O navegador bloqueou as notificações.');
    }
    store.set('notify', el.checked ? '1' : null);
  },
};

// ------------------------------------------------------------------ composer
function growComposer(ta) {
  ta.style.height = '36px';
  ta.style.height = Math.min(ta.scrollHeight, 200) + 'px';
}

function addFiles(list) {
  const c = channel();
  if (!c || !(c.type === 'dm' || c.perms & P.ATTACH) || !$('#composer textarea')) return;
  for (const f of list) {
    if (f.size > 25 * 1024 * 1024) toast(`${f.name} passa de 25 MB.`);
    else if (S.files.length < 10) S.files.push(f);
  }
  renderExtras();
}

async function sendMessage() {
  const ta = $('#composer textarea');
  const content = ta.value.trim();
  if (!content && !S.files.length) return;
  const cid = S.view.channelId, files = S.files, reply = S.reply;
  ta.value = '';
  growComposer(ta);
  S.files = [];
  S.reply = null;
  renderExtras();
  try {
    const attachments = [];
    for (const f of files) attachments.push(await uploadFile(f));
    await api('POST', `/channels/${cid}/messages`, { content, replyTo: reply?.id, attachments });
  } catch (e) {
    toast(e.message);
    if (S.view.channelId === cid && !ta.value) {
      ta.value = content;
      S.files = files;
      renderExtras();
    }
  }
}

// ------------------------------------------------------------------ gateway events
function patchUser(id, patch) {
  for (const s of S.servers.values()) for (const m of s.members) if (m.id === id) Object.assign(m, patch);
  for (const d of S.dms.values()) if (d.user.id === id) Object.assign(d.user, patch);
  const f = S.friends.get(id);
  if (f) Object.assign(f.user, patch);
  indexUsers();
}

const EV = {
  AUTH_FAIL: () => logout(true),
  ERROR: d => toast(d.message),
  READY(d) {
    S.tries = 0;
    banner('');
    S.me = d.user;
    S.version = d.version;
    S.servers = new Map(d.servers.map(s => [s.id, s]));
    S.dms = new Map(d.dms.map(x => [x.id, x]));
    S.friends = new Map(d.friends.map(f => [f.user.id, f]));
    S.blocks = new Set(d.blocks || []);
    S.users.clear();
    indexUsers();
    S.msgs.clear();
    S.typing.clear();
    let v = S.view;
    if (!validView(v)) {
      try {
        v = JSON.parse(store.get('view'));
      } catch {
        v = null;
      }
    }
    if (!validView(v)) v = S.servers.size ? { type: 'server', serverId: S.servers.keys().next().value } : { type: 'home', channelId: null };
    $('#auth').hidden = true;
    $('#app').hidden = false;
    goTo(v);
  },
  STATUS(d) {
    S.me.status = d.status;
    indexUsers();
    renderUserPanel();
  },
  SERVER_UPDATE(s) {
    const isNew = !S.servers.has(s.id);
    S.servers.set(s.id, s);
    indexUsers();
    if (Voice.channelId) {
      const mine = Object.entries(s.voice).find(([, users]) => users.some(u => u.userId === S.me.id));
      const here = s.channels.some(c => c.id === Voice.channelId);
      const lost = [...S.servers.values()].every(x => !x.channels.some(c => c.id === Voice.channelId));
      if (lost || (here && !mine)) Voice.leave(true, 'disconnect');
    }
    if (S.pendingGo === s.id) {
      S.pendingGo = null;
      return goTo({ type: 'server', serverId: s.id });
    }
    if (S.view.type === 'server' && S.view.serverId === s.id && !s.channels.some(c => c.id === S.view.channelId)) {
      return goTo({ type: 'server', serverId: s.id });
    }
    renderAll();
    drawModal();
    if (isNew) renderRail();
  },
  SERVER_REMOVE({ id }) {
    const s = S.servers.get(id);
    if (!s) return;
    if (s.channels.some(c => c.id === Voice.channelId)) Voice.leave(true, 'disconnect');
    S.servers.delete(id);
    if (S.view.type === 'server' && S.view.serverId === id) {
      closeModal();
      return goTo({ type: 'home', channelId: null });
    }
    renderAll();
  },
  MESSAGE_CREATE(m) {
    S.users.has(m.author.id) || S.users.set(m.author.id, m.author);
    const st = S.msgs.get(m.channel_id);
    if (st && !st.loading) st.list.push(m);
    S.typing.get(m.channel_id)?.delete(m.author.id);
    const dm = S.dms.get(m.channel_id);
    if (dm) {
      S.dms.delete(dm.id);
      S.dms = new Map([[dm.id, dm], ...S.dms]);
    }
    const current = S.view.channelId === m.channel_id && channel();
    if (current && S.focused) {
      renderMsgs(m.author.id === S.me.id ? 'bottom' : undefined);
      renderTyping();
      if (dm) renderSidebar();
      return;
    }
    if (current) renderMsgs();
    if (m.author.id === S.me.id || S.blocks.has(m.author.id)) return;
    const hot = dm || mentionsMe(m.content);
    const u = S.unread.get(m.channel_id) || { n: 0, hot: 0 };
    u.n++;
    if (hot) u.hot++;
    S.unread.set(m.channel_id, u);
    setBadge(totalHot());
    renderRail();
    renderSidebar();
    if (hot && S.me.status !== 'dnd') {
      Sounds.play('message');
      let view = { type: 'home', channelId: m.channel_id }, where = '';
      if (!dm) {
        for (const s of S.servers.values()) {
          const c = s.channels.find(x => x.id === m.channel_id);
          if (c) {
            view = { type: 'server', serverId: s.id, channelId: c.id };
            where = ` (#${c.name})`;
          }
        }
      }
      notify(nameOf(m.author) + where, m.content || 'Enviou um anexo', view);
    }
  },
  MESSAGE_UPDATE(m) {
    const st = S.msgs.get(m.channel_id);
    const i = st?.list.findIndex(x => x.id === m.id);
    if (i >= 0) {
      st.list[i] = m;
      if (S.view.channelId === m.channel_id && S.editing !== m.id) renderMsgs();
    }
  },
  MESSAGE_DELETE(d) {
    const st = S.msgs.get(d.channel_id);
    if (!st) return;
    st.list = st.list.filter(m => m.id !== d.id);
    if (S.view.channelId === d.channel_id) renderMsgs();
  },
  TYPING(d) {
    if (!S.typing.has(d.channelId)) S.typing.set(d.channelId, new Map());
    S.typing.get(d.channelId).set(d.userId, Date.now() + 6000);
    renderTyping();
  },
  PRESENCE(d) {
    patchUser(d.userId, { status: d.status });
    renderMembers();
    if (S.view.type === 'home') {
      renderSidebar();
      if (!channel()) renderMain();
    }
  },
  USER_UPDATE(u) {
    if (u.id === S.me.id) Object.assign(S.me, u);
    patchUser(u.id, u);
    for (const st of S.msgs.values()) for (const m of st.list) if (m.author.id === u.id) Object.assign(m.author, u);
    renderAll();
    drawModal();
  },
  FRIEND_UPDATE(f) {
    S.friends.set(f.user.id, f);
    indexUsers();
    renderRail();
    renderSidebar();
    if (S.view.type === 'home' && !channel()) renderMain();
  },
  FRIEND_REMOVE({ userId }) {
    S.friends.delete(userId);
    renderRail();
    renderSidebar();
    if (S.view.type === 'home' && !channel()) renderMain();
  },
  BLOCKS({ ids }) {
    S.blocks = new Set(ids);
    S.revealed.clear();
    renderAll();
    drawModal();
  },
  DM_REMOVE({ id }) {
    S.dms.delete(id);
    S.msgs.delete(id);
    S.unread.delete(id);
    if (S.view.type === 'home' && S.view.channelId === id) return goTo({ type: 'home', channelId: null });
    renderRail();
    renderSidebar();
  },
  USER_DELETED({ userId }) {
    S.friends.delete(userId);
    for (const [id, dm] of S.dms) if (dm.user.id === userId) EV.DM_REMOVE({ id });
    for (const st of S.msgs.values()) st.list = st.list.filter(m => m.author.id !== userId);
    renderAll();
  },
  DM_CREATE(dm) {
    if (!S.dms.has(dm.id)) S.dms.set(dm.id, dm);
    indexUsers();
    if (S.view.type === 'home') renderSidebar();
  },
  VOICE_STATE(d) {
    const s = S.servers.get(d.serverId);
    if (!s) return;
    s.voice[d.channelId] = d.users;
    if (Voice.channelId === d.channelId) Voice.sync(d.users);
    refreshVoice();
  },
  SIGNAL: d => Voice.onSignal(d.from, d.data),
  VOICE_KICK() {
    Voice.leave(true, 'disconnect');
    refreshVoice();
    toast('Você entrou na chamada por outro dispositivo.');
  },
};

// ------------------------------------------------------------------ auth
function showAuth(mode = 'login') {
  const reg = mode === 'register';
  $('#auth').hidden = false;
  $('#auth').innerHTML = `<div class="card"><img class="logo" src="icon.png" alt="">
    <h1>${reg ? 'Criar conta' : 'Bem-vindo de volta'}</h1><p class="sub">${reg ? 'Leva menos de um minuto.' : 'Entre para continuar a conversa.'}</p>
    <form data-form="${reg ? 'register' : 'login'}">
      ${field('Usuário', 'name="username" autocomplete="username" required maxlength="20"')}
      ${reg ? field('Nome de exibição', 'name="displayName" maxlength="32"') : ''}
      ${field('Senha', `name="password" type="password" autocomplete="${reg ? 'new-password' : 'current-password'}" required minlength="6"`)}
      <div class="err"></div><button class="btn wide">${reg ? 'Criar conta' : 'Entrar'}</button></form>
    <p style="margin:16px 0 0;color:var(--muted)">${reg ? 'Já tem conta?' : 'Ainda não tem conta?'} <button class="link" data-act="authMode" data-mode="${reg ? 'login' : 'register'}">${reg ? 'Entrar' : 'Criar conta'}</button></p>

  </div>`;
  $('#auth input').focus();
}

function showConnecting(retryIn) {
  $('#auth').hidden = false;
  const msg = !retryIn
    ? 'Conectando ao servidor do Babu Tela…'
    : native && !native.public
      ? `O servidor deste computador não respondeu. Feche o Babu Tela pela bandeja (Sair) e abra de novo. Tentando de novo em ${Math.round(retryIn / 1000)} s.`
      : `Não consegui falar com o servidor do Babu Tela. Confira sua internet; se ela estiver funcionando, o servidor pode estar desligado no momento. Tentando de novo em ${Math.round(retryIn / 1000)} s.`;
  $('#auth').innerHTML = `<div class="card" style="text-align:center"><img class="logo" src="icon.png" alt="" style="margin:0 auto 14px">
    <h1>${retryIn ? 'Servidor fora do ar' : 'Conectando…'}</h1>
    <p class="sub">${msg}</p>
    ${retryIn ? '<button class="btn wide" data-act="retryNow">Tentar agora</button>' : ''}
    <p style="margin:16px 0 0"><button class="link" data-act="logout">Sair da conta</button></p></div>`;
}

function startSession(token) {
  S.token = token;
  tokenStore.save(token);
  S.view = { type: 'none' };
  connect();
}

// ------------------------------------------------------------------ dom events
const guard = async fn => {
  try {
    await fn();
  } catch (e) {
    toast(e.message);
  }
};

// ------------------------------------------------------------------ phone layout
// Up to 720px wide the app shows one screen at a time: the list (servers,
// channels, conversations), marked by body.nav, or the open conversation. The
// back arrow in each header and the phone's own back button return to the list.
const phone = matchMedia('(max-width: 720px)');
document.body.classList.add('nav');

function showConversation() {
  if (!phone.matches || !document.body.classList.contains('nav')) return;
  document.body.classList.remove('nav');
  history.pushState({ conversation: true }, '');
}

function showNav() {
  if (history.state?.conversation) history.back();
  else document.body.classList.add('nav');
}

addEventListener('popstate', () => {
  document.body.classList.add('nav');
  if (phone.matches && S.showMembers) A.toggleMembers();
});

document.addEventListener('click', e => {
  if (S.pop && !S.pop.contains(e.target) && !e.target.closest('[data-act="serverMenu"],[data-act="pickReact"],[data-act="emoji"]')) closePop();
  const el = e.target.closest('[data-act]');
  if (!el || !A[el.dataset.act]) return;
  guard(() => A[el.dataset.act](el, e));
});

document.addEventListener('submit', async e => {
  e.preventDefault();
  const f = e.target;
  const err = $('.err', f) || f.parentElement.querySelector('.err');
  if (err) err.textContent = '';
  const btn = $('button:not([type=button])', f);
  if (btn) btn.disabled = true;
  try {
    await F[f.dataset.form](Object.fromEntries(new FormData(f)), f);
  } catch (ex) {
    if (err) err.textContent = ex.message;
    else toast(ex.message);
  }
  if (btn) btn.disabled = false;
});

document.addEventListener('change', e => {
  const el = e.target;
  if (el.id === 'filepick') {
    addFiles(el.files);
    el.value = '';
  } else if (el.dataset.change) {
    guard(() => C[el.dataset.change](el));
  }
});

document.addEventListener('input', e => {
  const ta = e.target;
  if (!ta.matches('#composer textarea')) return;
  growComposer(ta);
  if (ta.value && Date.now() - S.lastTyping > 3000) {
    S.lastTyping = Date.now();
    sendOp({ op: 'typing', channelId: S.view.channelId });
  }
});

document.addEventListener('keydown', e => {
  const el = e.target;
  if (e.key === 'Escape') {
    if (S.pop) return closePop();
    if (el.matches?.('textarea.edit')) {
      S.editing = null;
      return renderMsgs();
    }
    if (S.modal && !S.modal.cls.includes('sticky')) return closeModal();
    if (S.reply) return A.cancelReply();
  }
  if (e.key !== 'Enter' || e.shiftKey || e.isComposing) return;
  if (el.matches?.('#composer textarea')) {
    e.preventDefault();
    sendMessage();
  } else if (el.matches?.('textarea.edit')) {
    e.preventDefault();
    const id = +el.dataset.id, content = el.value.trim();
    S.editing = null;
    if (!content || content === findMsg(id)?.content) return renderMsgs();
    guard(async () => {
      await api('PATCH', `/messages/${id}`, { content });
    }).then(() => renderMsgs());
  }
});

document.addEventListener('paste', e => {
  if (e.target.matches?.('#composer textarea') && e.clipboardData.files.length) {
    e.preventDefault();
    addFiles(e.clipboardData.files);
  }
});
document.addEventListener('dragover', e => e.preventDefault());
document.addEventListener('drop', e => {
  e.preventDefault();
  if (e.dataTransfer?.files.length) addFiles(e.dataTransfer.files);
});
// Images finish loading after the list is drawn; keep the view pinned to the bottom.
document.addEventListener('load', e => {
  const box = $('#msgs');
  if (S.stick && box?.contains(e.target)) box.scrollTop = box.scrollHeight;
}, true);
document.addEventListener('scroll', e => {
  if (e.target.id === 'msgs') S.stick = e.target.scrollHeight - e.target.scrollTop - e.target.clientHeight < 90;
}, true);

window.addEventListener('focus', () => {
  S.focused = true;
  const c = channel();
  if (c) markRead(c.id);
});
window.addEventListener('blur', () => (S.focused = false));

setInterval(() => {
  let changed = false;
  for (const m of S.typing.values()) {
    for (const [uid, until] of m) {
      if (until < Date.now()) {
        m.delete(uid);
        changed = true;
      }
    }
  }
  if (changed) renderTyping();
  sendOp({ op: 'ping' });
}, 5000);

// ------------------------------------------------------------------ desktop shell
if (native) {
  $('#titlebar').hidden = false;
  native.onTray(action => A[action]?.());
  native.onNavigate(view => goTo(view));
  native.onScreenSources(sources => modal(MODALS.screens(sources), 'wide sticky'));
  native.onUpdate(u => {
    if (u.state === 'downloaded') banner(`Atualização ${esc(u.version || '')} pronta. <button data-act="installUpdate">Reiniciar e instalar</button>`);
  });
}

const sleep = ms => new Promise(r => setTimeout(r, ms));

function checkLines(p) {
  return PUBLIC_CHECKS.map(([k, label]) => {
    const c = k === 'ip' ? (p.ip ? { ok: true, msg: p.ip } : null) : p.checks?.[k];
    const mark = !c ? '<span style="color:var(--muted)">…</span>' : `<b style="color:${c.ok ? 'var(--online)' : 'var(--danger)'}">${c.ok ? 'ok' : 'falhou'}</b>`;
    return `<div class="check" style="align-items:flex-start;text-align:left"><span>${label}${c ? `<small>${esc(c.msg)}</small>` : ''}</span>${mark}</div>`;
  }).join('');
}

// Your app opens only once the public server passed every check: IP,
// address, router ports, call relay and HTTPS. A failed step is shown and
// retried every 30 seconds.
async function waitForPublicServer() {
  let nextRetry = 0;
  for (;;) {
    const p = await native.publicServer.status();
    if (!p.enabled || p.state === 'online') return;
    const failed = p.state === 'error';
    if (failed && !nextRetry) nextRetry = Date.now() + 30000;
    if (failed && Date.now() >= nextRetry) {
      nextRetry = 0;
      await native.publicServer.set(true, { domain: p.domain, email: p.email, upnp: p.upnp });
      continue;
    }
    $('#auth').hidden = false;
    $('#auth').innerHTML = `<div class="card"><img class="logo" src="icon.png" alt="">
      <h1>${failed ? 'O servidor não ficou pronto' : 'Ligando o servidor…'}</h1>
      <p class="sub">${failed ? `Uma etapa falhou. Tentando de novo em ${Math.max(1, Math.round((nextRetry - Date.now()) / 1000))} s.` : `Preparando ${esc(p.url)}. Isso leva até 2 minutos.`}</p>
      ${checkLines(p)}
      ${failed ? '<button class="btn wide" style="margin-top:14px" data-act="retryPublic">Tentar agora</button>' : ''}</div>`;
    await sleep(1500);
  }
}

// The friends' app shows the login only once your server answers.
async function waitForServer() {
  for (let tries = 0; ; tries++) {
    try {
      const res = await fetch(BASE + '/api/version', { signal: AbortSignal.timeout(6000) });
      if (res.ok) return;
    } catch {
      // not reachable yet
    }
    const delay = Math.min(1000 * 2 ** tries, 15000);
    showConnecting(delay);
    await sleep(delay);
  }
}

(async () => {
  if (native && !native.public && native.publicServer) await waitForPublicServer();
  else if (native?.public) {
    showConnecting(0);
    await waitForServer();
  }
  S.token = await tokenStore.load();
  if (S.token) {
    S.view = { type: 'none' };
    showConnecting(0);
    connect();
  } else {
    showAuth();
  }
})();
