'use strict';
// End-to-end check of the Go server (bin/babutela-server.exe, or the path in
// the first argument) against a throwaway database:
//   go build -o bin/babutela-server.exe ./cmd/babutela-server && node tests/smoke.js
const assert = require('assert');
const fs = require('fs');
const os = require('os');
const path = require('path');
const WebSocket = require('ws');
const { spawn } = require('child_process');

const data = fs.mkdtempSync(path.join(os.tmpdir(), 'prosa-smoke-'));
const bin = path.resolve(process.argv[2] || path.join(__dirname, '..', 'bin', 'babutela-server.exe'));
let child;
async function start(port) {
  child = spawn(bin, [], {
    env: { ...process.env, PROSA_DATA: data, PORT: String(port), TURN_URLS: 'turn:turn.example:3478', TURN_SECRET: 'segredo-turn' },
    stdio: ['ignore', 'inherit', 'inherit'],
  });
  for (let i = 0; i < 50; i++) {
    try {
      await fetch(`http://localhost:${port}/api/version`);
      return;
    } catch {
      await new Promise(r => setTimeout(r, 100));
    }
  }
  throw new Error('o servidor não subiu');
}
const BASE = 'http://localhost:3299';

async function call(token, method, url, body) {
  const res = await fetch(BASE + '/api' + url, {
    method,
    headers: { 'Content-Type': 'application/json', ...(token && { Authorization: 'Bearer ' + token }) },
    body: body && JSON.stringify(body),
  });
  return { status: res.status, body: await res.json() };
}

function gateway(token) {
  const ws = new WebSocket('ws://localhost:3299/gateway');
  const events = [];
  const waiters = [];
  ws.on('open', () => ws.send(JSON.stringify({ op: 'identify', token })));
  ws.on('message', raw => {
    const ev = JSON.parse(raw);
    events.push(ev);
    for (const w of [...waiters]) w();
  });
  const next = (t, pred = () => true) =>
    new Promise((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error('timeout esperando ' + t)), 4000);
      const check = () => {
        const i = events.findIndex(e => e.t === t && pred(e.d));
        if (i < 0) return;
        clearTimeout(timer);
        waiters.splice(waiters.indexOf(check), 1);
        resolve(events.splice(i, 1)[0].d);
      };
      waiters.push(check);
      check();
    });
  return { ws, next, op: m => ws.send(JSON.stringify(m)) };
}

async function main() {
  await start(3299);
  const ok = (r, label) => assert.ok(r.status < 300, `${label}: ${r.status} ${JSON.stringify(r.body)}`);
  const step = name => console.log('ok  ' + name);

  const ana = (await call(null, 'POST', '/register', { username: 'ana', password: 'segredo1', displayName: 'Ana' })).body.token;
  const beto = (await call(null, 'POST', '/register', { username: 'beto', password: 'segredo2' })).body.token;
  assert.ok(ana && beto);
  assert.strictEqual((await call(null, 'POST', '/login', { username: 'ana', password: 'errada' })).status, 401);
  assert.ok((await call(null, 'POST', '/login', { username: 'ana', password: 'segredo1' })).body.token);
  assert.strictEqual((await call('lixo', 'POST', '/servers', { name: 'x' })).status, 401);
  step('cadastro, login e rejeição de token inválido');

  const ice = (await call(ana, 'GET', '/ice')).body;
  const turn = ice.iceServers.find(x => x.username);
  const crypto = require('crypto');
  assert.strictEqual(turn.credential, crypto.createHmac('sha1', 'segredo-turn').update(turn.username).digest('base64'));
  assert.ok(Number(turn.username.split(':')[0]) > Date.now() / 1000 + 11 * 3600);
  assert.strictEqual((await call(null, 'GET', '/ice')).status, 401);
  console.log('ok  credenciais TURN temporárias');

  const A = gateway(ana), B = gateway(beto);
  const readyA = await A.next('READY');
  const readyB = await B.next('READY');
  assert.strictEqual(readyA.user.username, 'ana');

  const sid = (await call(ana, 'POST', '/servers', { name: 'Teste' })).body.id;
  const srvA = await A.next('SERVER_UPDATE', d => d.id === sid);
  assert.strictEqual(srvA.channels.length, 2);
  const textChan = srvA.channels.find(c => c.type === 'text');
  const voiceChan = srvA.channels.find(c => c.type === 'voice');
  step('criação de servidor com canais padrão');

  assert.strictEqual((await call(beto, 'GET', `/channels/${textChan.id}/messages`)).status, 404);
  const code = (await call(ana, 'POST', `/servers/${sid}/invites`)).body.code;
  ok(await call(beto, 'POST', `/invites/${code}/join`), 'entrar');
  const srvB = await B.next('SERVER_UPDATE', d => d.id === sid);
  assert.strictEqual(srvB.members.length, 2);
  step('convite e entrada no servidor');

  ok(await call(beto, 'POST', `/channels/${textChan.id}/messages`, { content: 'olá **mundo**' }), 'mensagem');
  const m = await A.next('MESSAGE_CREATE');
  assert.strictEqual(m.content, 'olá **mundo**');
  ok(await call(ana, 'PUT', `/messages/${m.id}/reactions/${encodeURIComponent('👍')}`), 'reação');
  assert.deepStrictEqual((await B.next('MESSAGE_UPDATE')).reactions, [{ emoji: '👍', users: [readyA.user.id] }]);
  assert.strictEqual((await call(ana, 'PATCH', `/messages/${m.id}`, { content: 'x' })).status, 403);
  ok(await call(beto, 'PATCH', `/messages/${m.id}`, { content: 'editada' }), 'editar');
  assert.ok((await A.next('MESSAGE_UPDATE', d => d.content === 'editada')).edited);
  ok(await call(ana, 'POST', `/channels/${textChan.id}/messages`, { content: 'resposta', replyTo: m.id }), 'responder');
  assert.strictEqual((await B.next('MESSAGE_CREATE', d => d.content === 'resposta')).reply.id, m.id);
  B.op({ op: 'typing', channelId: textChan.id });
  assert.strictEqual((await A.next('TYPING')).userId, readyB.user.id);
  step('mensagens, reações, edição, resposta e digitando');

  const form = new FormData();
  form.append('file', new Blob([Buffer.from('conteúdo')]), 'nota.txt');
  const up = await (await fetch(BASE + '/api/upload', { method: 'POST', headers: { Authorization: 'Bearer ' + beto }, body: form })).json();
  assert.match(up.url, /^\/uploads\/[a-f0-9]{32}\.txt$/);
  ok(await call(beto, 'POST', `/channels/${textChan.id}/messages`, { content: '', attachments: [up] }), 'anexo');
  assert.strictEqual((await A.next('MESSAGE_CREATE', d => d.attachments.length === 1)).attachments[0].name, 'nota.txt');
  const dl = await fetch(BASE + up.url);
  assert.strictEqual(dl.headers.get('content-disposition'), 'attachment');
  assert.strictEqual((await call(beto, 'POST', `/channels/${textChan.id}/messages`, { attachments: [{ url: '/uploads/../prosa.db' }] })).status, 400);
  step('upload de arquivo e validação de anexos');

  assert.strictEqual((await call(beto, 'POST', `/servers/${sid}/channels`, { name: 'x' })).status, 403);
  assert.strictEqual((await call(beto, 'DELETE', `/servers/${sid}/members/${readyA.user.id}`)).status, 403);
  const rid = (await call(ana, 'POST', `/servers/${sid}/roles`, { name: 'Mod', color: '#f2b84b', perms: 1 | 2 | 8 | 16 })).body.id;
  ok(await call(ana, 'PUT', `/servers/${sid}/members/${readyB.user.id}/roles/${rid}`), 'dar cargo');
  ok(await call(beto, 'POST', `/servers/${sid}/channels`, { name: 'Novo Canal' }), 'criar canal com cargo');
  assert.strictEqual((await call(beto, 'PATCH', `/roles/${rid}`, { perms: 4095 })).status, 403);
  assert.strictEqual((await call(beto, 'POST', `/servers/${sid}/roles`, { name: 'Admin', perms: 128 })).status, 403);
  step('cargos, permissões e bloqueio de escalonamento');

  const everyone = srvA.roles.find(r => r.is_everyone);
  ok(await call(ana, 'DELETE', `/servers/${sid}/members/${readyB.user.id}/roles/${rid}`), 'tirar cargo');
  ok(await call(ana, 'PUT', `/channels/${textChan.id}/overrides/${everyone.id}`, { allow: 0, deny: 1 }), 'override');
  const hidden = await B.next('SERVER_UPDATE', d => !d.channels.some(c => c.id === textChan.id));
  assert.ok(hidden);
  assert.strictEqual((await call(beto, 'GET', `/channels/${textChan.id}/messages`)).status, 404);
  ok(await call(ana, 'PUT', `/channels/${textChan.id}/overrides/${everyone.id}`, { allow: 0, deny: 0 }), 'limpar override');
  step('permissões por canal escondem o canal');

  const target = (await call(ana, 'POST', `/channels/${textChan.id}/messages`, { content: 'mensagem denunciável' })).body;
  assert.strictEqual((await call(ana, 'POST', `/messages/${target.id}/report`, { reason: 'spam' })).status, 400);
  ok(await call(beto, 'POST', `/messages/${target.id}/report`, { reason: 'spam', detail: 'propaganda' }), 'denunciar');
  const withReport = await A.next('SERVER_UPDATE', d => d.reports?.length === 1);
  assert.strictEqual(withReport.reports[0].reason, 'Spam: propaganda');
  assert.ok(!(await B.next('SERVER_UPDATE', d => d.id === sid)).reports.length, 'membro comum não vê denúncias');
  ok(await call(ana, 'DELETE', `/reports/${withReport.reports[0].id}`), 'resolver denúncia');
  await A.next('SERVER_UPDATE', d => d.reports?.length === 0);
  step('denúncia chega só para a moderação');

  A.op({ op: 'voice_join', channelId: voiceChan.id });
  B.op({ op: 'voice_join', channelId: voiceChan.id, mute: true });
  const vs = await A.next('VOICE_STATE', d => d.users.length === 2);
  assert.strictEqual(vs.users.find(u => u.userId === readyB.user.id).mute, true);
  A.op({ op: 'signal', to: readyB.user.id, data: { candidate: 'teste' } });
  assert.deepStrictEqual(await B.next('SIGNAL'), { from: readyA.user.id, data: { candidate: 'teste' } });
  B.op({ op: 'voice_leave' });
  await A.next('VOICE_STATE', d => d.users.length === 1);
  step('voz: entrada, estado e sinalização WebRTC');

  ok(await call(ana, 'POST', '/friends', { username: 'beto' }), 'pedido de amizade');
  assert.strictEqual((await B.next('FRIEND_UPDATE')).state, 'incoming');
  ok(await call(beto, 'PUT', `/friends/${readyA.user.id}`), 'aceitar');
  assert.strictEqual((await A.next('FRIEND_UPDATE', d => d.state === 'accepted')).user.username, 'beto');
  const dm = (await call(ana, 'POST', '/dms', { userId: readyB.user.id })).body;
  ok(await call(ana, 'POST', `/channels/${dm.id}/messages`, { content: 'oi no privado' }), 'dm');
  assert.strictEqual((await B.next('MESSAGE_CREATE', d => d.channel_id === dm.id)).content, 'oi no privado');
  step('amigos e mensagens diretas');

  ok(await call(beto, 'PUT', `/blocks/${readyA.user.id}`), 'bloquear');
  assert.deepStrictEqual((await B.next('BLOCKS')).ids, [readyA.user.id]);
  await A.next('FRIEND_REMOVE', d => d.userId === readyB.user.id);
  assert.strictEqual((await call(ana, 'POST', `/channels/${dm.id}/messages`, { content: 'oi?' })).status, 403);
  assert.strictEqual((await call(ana, 'POST', '/friends', { username: 'beto' })).status, 403);
  ok(await call(beto, 'DELETE', `/blocks/${readyA.user.id}`), 'desbloquear');
  ok(await call(ana, 'POST', `/channels/${dm.id}/messages`, { content: 'voltou' }), 'dm após desbloqueio');
  step('bloqueio impede mensagem direta e amizade');

  ok(await call(ana, 'PUT', `/servers/${sid}/bans/${readyB.user.id}`), 'banir');
  assert.strictEqual((await B.next('SERVER_REMOVE')).id, sid);
  assert.strictEqual((await call(beto, 'POST', `/invites/${code}/join`)).status, 403);
  step('banimento impede nova entrada');

  A.ws.close();
  assert.strictEqual((await B.next('PRESENCE', d => d.userId === readyA.user.id)).status, 'offline');
  step('presença offline ao desconectar');
  B.ws.close();


  await call(null, 'POST', '/register', { username: 'caio', password: 'certa123' });
  for (let i = 0; i < 5; i++) assert.strictEqual((await call(null, 'POST', '/login', { username: 'caio', password: 'errada' })).status, 401);
  assert.strictEqual((await call(null, 'POST', '/login', { username: 'caio', password: 'certa123' })).status, 429);
  step('cinco senhas erradas bloqueiam o login por um tempo');

  const codes = [];
  for (let i = 0; i < 12; i++) codes.push((await call(ana, 'POST', `/channels/${dm.id}/messages`, { content: 'flood ' + i })).status);
  assert.ok(codes.includes(429) && codes.filter(c => c === 200).length <= 10, codes.join(','));
  step('limite de mensagens seguidas');

  assert.strictEqual((await call(beto, 'PATCH', '/me/password', { current: 'errada', password: 'novasenha' })).status, 403);
  const beto2 = (await call(null, 'POST', '/login', { username: 'beto', password: 'segredo2' })).body.token;
  ok(await call(beto2, 'PATCH', '/me/password', { current: 'segredo2', password: 'novasenha' }), 'trocar senha');
  assert.strictEqual((await call(beto, 'GET', '/sessions')).status, 401, 'sessão antiga encerrada');
  assert.strictEqual((await call(beto2, 'GET', '/sessions')).body.count, 1);
  assert.ok((await call(null, 'POST', '/login', { username: 'beto', password: 'novasenha' })).body.token);
  step('troca de senha encerra as outras sessões');

  const ana2 = (await call(null, 'POST', '/login', { username: 'ana', password: 'segredo1' })).body.token;
  assert.ok((await call(ana2, 'GET', '/sessions')).body.count >= 3);
  ok(await call(ana2, 'POST', '/sessions/logout-others'), 'sair dos outros');
  assert.strictEqual((await call(ana, 'GET', '/sessions')).status, 401);
  assert.strictEqual((await call(ana2, 'GET', '/sessions')).body.count, 1);
  step('sair de todos os outros dispositivos');

  const C = gateway(ana2);
  await C.next('READY');
  const dan = (await call(null, 'POST', '/register', { username: 'dan', password: 'apagar123' })).body.token;
  ok(await call(dan, 'POST', '/friends', { username: 'ana' }), 'pedido do dan');
  const danId = (await C.next('FRIEND_UPDATE')).user.id;
  ok(await call(dan, 'POST', '/servers', { name: 'Do Dan' }), 'servidor do dan');
  assert.strictEqual((await call(dan, 'DELETE', '/me', { password: 'errada' })).status, 403);
  ok(await call(dan, 'DELETE', '/me', { password: 'apagar123' }), 'excluir conta');
  assert.strictEqual((await C.next('USER_DELETED')).userId, danId);
  assert.strictEqual((await call(dan, 'GET', '/sessions')).status, 401);
  assert.strictEqual((await call(null, 'POST', '/login', { username: 'dan', password: 'apagar123' })).status, 401);
  step('excluir conta remove o usuário e avisa os contatos');
  C.ws.close();
}

main()
  .then(() => console.log('\nTodos os testes passaram.'))
  .catch(e => {
    console.error('\nFALHOU:', e.message);
    process.exitCode = 1;
  })
  .finally(() => {
    child?.kill();
    process.exit();
  });
