'use strict';
const { DatabaseSync } = require('node:sqlite');
const path = require('path');
const fs = require('fs');

const dir = process.env.PROSA_DATA || path.join(__dirname, 'data');
fs.mkdirSync(path.join(dir, 'uploads'), { recursive: true });
fs.mkdirSync(path.join(dir, 'updates'), { recursive: true });

const db = new DatabaseSync(path.join(dir, 'prosa.db'));
db.exec(`
PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;
CREATE TABLE IF NOT EXISTS users(
  id INTEGER PRIMARY KEY, username TEXT UNIQUE NOT NULL, display_name TEXT NOT NULL,
  pass_hash TEXT NOT NULL, salt TEXT NOT NULL, color TEXT NOT NULL, avatar TEXT, created INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS sessions(
  token_hash TEXT PRIMARY KEY, user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE, created INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS servers(
  id INTEGER PRIMARY KEY, name TEXT NOT NULL, owner_id INTEGER NOT NULL, color TEXT NOT NULL, created INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS members(
  server_id INTEGER NOT NULL REFERENCES servers(id) ON DELETE CASCADE, user_id INTEGER NOT NULL, joined INTEGER NOT NULL,
  PRIMARY KEY(server_id, user_id));
CREATE TABLE IF NOT EXISTS roles(
  id INTEGER PRIMARY KEY, server_id INTEGER NOT NULL REFERENCES servers(id) ON DELETE CASCADE, name TEXT NOT NULL,
  color TEXT, perms INTEGER NOT NULL, position INTEGER NOT NULL, is_everyone INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS member_roles(
  server_id INTEGER NOT NULL REFERENCES servers(id) ON DELETE CASCADE, user_id INTEGER NOT NULL,
  role_id INTEGER NOT NULL REFERENCES roles(id) ON DELETE CASCADE, PRIMARY KEY(user_id, role_id));
CREATE TABLE IF NOT EXISTS channels(
  id INTEGER PRIMARY KEY, server_id INTEGER REFERENCES servers(id) ON DELETE CASCADE, name TEXT, type TEXT NOT NULL,
  category TEXT, topic TEXT, position INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS overrides(
  channel_id INTEGER NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
  role_id INTEGER NOT NULL REFERENCES roles(id) ON DELETE CASCADE,
  allow INTEGER NOT NULL, deny INTEGER NOT NULL, PRIMARY KEY(channel_id, role_id));
CREATE TABLE IF NOT EXISTS dm_members(
  channel_id INTEGER NOT NULL REFERENCES channels(id) ON DELETE CASCADE, user_id INTEGER NOT NULL,
  PRIMARY KEY(channel_id, user_id));
CREATE TABLE IF NOT EXISTS messages(
  id INTEGER PRIMARY KEY, channel_id INTEGER NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
  author_id INTEGER NOT NULL, content TEXT NOT NULL, attachments TEXT, reply_to INTEGER,
  created INTEGER NOT NULL, edited INTEGER);
CREATE INDEX IF NOT EXISTS messages_channel ON messages(channel_id, id);
CREATE TABLE IF NOT EXISTS reactions(
  message_id INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE, user_id INTEGER NOT NULL, emoji TEXT NOT NULL,
  PRIMARY KEY(message_id, user_id, emoji));
CREATE TABLE IF NOT EXISTS invites(
  code TEXT PRIMARY KEY, server_id INTEGER NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
  creator_id INTEGER NOT NULL, created INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS bans(
  server_id INTEGER NOT NULL REFERENCES servers(id) ON DELETE CASCADE, user_id INTEGER NOT NULL,
  PRIMARY KEY(server_id, user_id));
CREATE TABLE IF NOT EXISTS friends(
  a INTEGER NOT NULL, b INTEGER NOT NULL, requester INTEGER NOT NULL, status TEXT NOT NULL, PRIMARY KEY(a, b));
CREATE TABLE IF NOT EXISTS blocks(
  user_id INTEGER NOT NULL, blocked_id INTEGER NOT NULL, PRIMARY KEY(user_id, blocked_id));
CREATE TABLE IF NOT EXISTS reports(
  id INTEGER PRIMARY KEY, message_id INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
  server_id INTEGER NOT NULL REFERENCES servers(id) ON DELETE CASCADE, reporter_id INTEGER NOT NULL,
  reason TEXT NOT NULL, created INTEGER NOT NULL, UNIQUE(message_id, reporter_id));
`);

// Columns added after the first release.
for (const sql of ['ALTER TABLE sessions ADD COLUMN last_used INTEGER']) {
  try {
    db.exec(sql);
  } catch {
    // already there
  }
}
db.exec('UPDATE sessions SET last_used = created WHERE last_used IS NULL');

const cache = new Map();
const stmt = sql => {
  let s = cache.get(sql);
  if (!s) cache.set(sql, (s = db.prepare(sql)));
  return s;
};

module.exports = {
  dir,
  get: (sql, ...a) => stmt(sql).get(...a),
  all: (sql, ...a) => stmt(sql).all(...a),
  run: (sql, ...a) => stmt(sql).run(...a),
  insert: (sql, ...a) => Number(stmt(sql).run(...a).lastInsertRowid),
  tx(fn) {
    db.exec('BEGIN');
    try {
      const r = fn();
      db.exec('COMMIT');
      return r;
    } catch (e) {
      db.exec('ROLLBACK');
      throw e;
    }
  },
};
