//! The SQLite store, now the private database of the Durable Object. The
//! tables are the ones the earlier Node, Go and Rust servers had, except for
//! the accounts, which sign in with Google since 0.11.0 (that version
//! emptied the database once, to leave the password accounts behind).
//!
//! Durable Object SQL is synchronous (`exec` returns rows right away), so
//! every handler stays a plain function, like it was with rusqlite.

use std::cell::RefCell;
use std::rc::Rc;

use serde_json::{Map, Number, Value};
use worker::wasm_bindgen::closure::Closure;
use worker::wasm_bindgen::{JsCast, JsValue};
use worker::{SqlStorage, SqlStorageValue, Storage, js_sys};

use crate::util::{ApiError, ApiResult, INTERNAL, fail};

/// Bumped whenever SCHEMA or the migrations below change, so a warm object
/// only runs one query instead of the whole script.
const SCHEMA_VERSION: &str = "13";

/// Columns that came after their table, added to it where missing (a table
/// made by an older version), as `migrate` does: table, column, its type.
/// A NOT NULL one needs a DEFAULT, for the rows already there.
///
/// - servers.icon: the server's picture ("/uploads/..."), or NULL (9).
/// - servers.private: 1 when joining takes a request that someone in the
///   server accepts (9).
/// - messages.envelope: the encrypted envelope (JSON), for a message from
///   an app with end-to-end encryption; its `content` stays empty (11).
/// - reactions.data: a reaction's encrypted and signed emoji, whose `emoji`
///   is then only its mark (11).
/// - members.certs: the signed certificates that someone in the server let
///   this member in (JSON list, e2e.rs); invites.cert: the creator's
///   signature over the invite's key, whose secret half only the link
///   carries (12).
/// - channels.ttl: how long its messages last, in seconds (0: for good);
///   messages.gone_at: when one goes (ms), for a channel with one (12).
const ADDED_COLUMNS: &[(&str, &str, &str)] = &[
    ("servers", "icon", "TEXT"),
    ("servers", "private", "INTEGER NOT NULL DEFAULT 0"),
    ("messages", "envelope", "TEXT"),
    ("reactions", "data", "TEXT"),
    ("members", "certs", "TEXT"),
    ("invites", "cert", "TEXT"),
    ("channels", "ttl", "INTEGER NOT NULL DEFAULT 0"),
    ("messages", "gone_at", "INTEGER"),
];

// `google_sub` is the Google account a user signs in with (its `sub`, which
// never changes), and `email` its e-mail, which only its owner sees. The
// tests' accounts have no Google account (NULL).
//
// `voice_holds` keeps, for a few seconds, the place in a call of someone
// whose gateway connection closed (state.rs, VOICE_HOLD), and `voice_mail`
// the signalling that arrived for them meanwhile (10). Both outlive the
// object going to sleep, which a field of it would not.
//
// End-to-end encryption (11; the app's e2e/): `e2e_keys` holds each
// account's public keys as its app published them (the identity's
// signature over its X-Wing key, which only the apps check), and
// `e2e_sessions` a sending session's key, sealed by the sender for one
// reader. Neither can be opened here: what opens them never leaves the
// people's PCs.
//
// `e2e_log` (12) is the public record of keys (e2e.rs): every key an
// account publishes and every version of the app, in order, never changed
// nor taken away, which every app keeps a copy of and compares.
// `e2e_prekeys` (12): each account's X-Wing key of the day, for the
// conversations whose messages disappear, whose secret the apps delete a
// few days later (forward secrecy).
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS users(
  id INTEGER PRIMARY KEY, username TEXT UNIQUE NOT NULL, display_name TEXT NOT NULL,
  google_sub TEXT UNIQUE, email TEXT NOT NULL DEFAULT '',
  color TEXT NOT NULL, avatar TEXT, created INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS sessions(
  token_hash TEXT PRIMARY KEY, user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  created INTEGER NOT NULL, last_used INTEGER NOT NULL);
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
  created INTEGER NOT NULL, edited INTEGER, sticker TEXT);
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
CREATE TABLE IF NOT EXISTS limits(k TEXT PRIMARY KEY, start INTEGER NOT NULL, n INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS presence(user_id INTEGER PRIMARY KEY, status TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS uploads(
  name TEXT PRIMARY KEY, user_id INTEGER NOT NULL, original TEXT NOT NULL, size INTEGER NOT NULL,
  upload_id TEXT NOT NULL DEFAULT '', created INTEGER NOT NULL,
  done INTEGER NOT NULL DEFAULT 0, used INTEGER NOT NULL DEFAULT 0);
CREATE INDEX IF NOT EXISTS uploads_unused ON uploads(used, created);
CREATE TABLE IF NOT EXISTS sent(
  user_id INTEGER NOT NULL, nonce TEXT NOT NULL,
  message_id INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE, created INTEGER NOT NULL,
  PRIMARY KEY(user_id, nonce));
CREATE TABLE IF NOT EXISTS stickers(
  user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE, giphy_id TEXT NOT NULL,
  added INTEGER NOT NULL, PRIMARY KEY(user_id, giphy_id));
CREATE TABLE IF NOT EXISTS join_requests(
  server_id INTEGER NOT NULL REFERENCES servers(id) ON DELETE CASCADE, user_id INTEGER NOT NULL,
  created INTEGER NOT NULL, PRIMARY KEY(server_id, user_id));
CREATE INDEX IF NOT EXISTS join_requests_user ON join_requests(user_id);
CREATE TABLE IF NOT EXISTS voice_holds(
  user_id INTEGER PRIMARY KEY, channel_id INTEGER NOT NULL, joined INTEGER NOT NULL,
  mute INTEGER NOT NULL, deaf INTEGER NOT NULL, video TEXT, expires INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS voice_mail(
  id INTEGER PRIMARY KEY, user_id INTEGER NOT NULL, frame TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS voice_mail_user ON voice_mail(user_id, id);
CREATE TABLE IF NOT EXISTS e2e_keys(
  user_id INTEGER PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE, bundle TEXT NOT NULL,
  updated INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS e2e_sessions(
  sid TEXT NOT NULL, user_id INTEGER NOT NULL,
  channel_id INTEGER NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
  sender_id INTEGER NOT NULL, created INTEGER NOT NULL, sealed TEXT NOT NULL,
  PRIMARY KEY(sid, user_id));
CREATE INDEX IF NOT EXISTS e2e_sessions_channel ON e2e_sessions(channel_id);
CREATE TABLE IF NOT EXISTS e2e_log(
  seq INTEGER PRIMARY KEY, kind TEXT NOT NULL, user_id INTEGER NOT NULL,
  leaf TEXT NOT NULL, at INTEGER NOT NULL, statement TEXT);
CREATE INDEX IF NOT EXISTS e2e_log_user ON e2e_log(user_id, seq);
CREATE TABLE IF NOT EXISTS e2e_prekeys(
  user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE, day INTEGER NOT NULL,
  prekey TEXT NOT NULL, PRIMARY KEY(user_id, day));
";

/// One result row keyed by column name, the shape the JSON API sends.
pub type Row = Map<String, Value>;

pub trait RowExt {
    /// An integer column (0 when missing or not a number).
    fn int(&self, key: &str) -> i64;
    /// A text column ("" when missing or not text).
    fn str(&self, key: &str) -> &str;
    /// A column as JSON (null when missing).
    fn val(&self, key: &str) -> Value;
}

impl RowExt for Row {
    fn int(&self, key: &str) -> i64 {
        match self.get(key) {
            Some(Value::Number(n)) => n
                .as_i64()
                .or_else(|| n.as_f64().map(|f| f as i64))
                .unwrap_or(0),
            _ => 0,
        }
    }

    fn str(&self, key: &str) -> &str {
        match self.get(key) {
            Some(Value::String(s)) => s,
            _ => "",
        }
    }

    fn val(&self, key: &str) -> Value {
        self.get(key).cloned().unwrap_or(Value::Null)
    }
}

/// Query parameters, in the order of the `?` placeholders.
#[macro_export]
macro_rules! params {
    () => { ::std::vec::Vec::new() };
    ($($v:expr),+ $(,)?) => { ::std::vec![$(::serde_json::json!($v)),+] };
}

/// A JSON value as an SQL parameter.
fn sql(v: &Value) -> SqlStorageValue {
    match v {
        Value::Null => SqlStorageValue::Null,
        Value::Bool(b) => SqlStorageValue::Integer(i64::from(*b)),
        Value::Number(n) => match n.as_i64() {
            Some(i) => SqlStorageValue::Integer(i),
            None => SqlStorageValue::Float(n.as_f64().unwrap_or(0.0)),
        },
        Value::String(s) => SqlStorageValue::String(s.clone()),
        other => SqlStorageValue::String(other.to_string()),
    }
}

fn to_json(v: SqlStorageValue) -> Value {
    match v {
        SqlStorageValue::Null => Value::Null,
        SqlStorageValue::Boolean(b) => Value::from(b),
        SqlStorageValue::Integer(i) => Value::from(i),
        SqlStorageValue::Float(f) => Number::from_f64(f).map_or(Value::Null, Value::Number),
        SqlStorageValue::String(s) => Value::String(s),
        SqlStorageValue::Blob(b) => Value::String(String::from_utf8_lossy(&b).into_owned()),
    }
}

/// The object's private SQLite database.
pub struct Db {
    sql: SqlStorage,
    storage: Storage,
}

impl Db {
    pub fn new(storage: Storage) -> Db {
        Db {
            sql: storage.sql(),
            storage,
        }
    }

    /// Creates the tables the first time, and afterwards costs one query.
    /// It only adds (CREATE TABLE IF NOT EXISTS, and ALTER TABLE ... ADD
    /// COLUMN for ADDED_COLUMNS), so publishing a new version loses no data,
    /// with one exception, asked for and done once: the messages from
    /// before the encryption go (`purge_plain`).
    pub fn migrate(&self) -> Result<(), String> {
        let setup = || -> ApiResult<()> {
            self.exec_only(
                "CREATE TABLE IF NOT EXISTS meta(k TEXT PRIMARY KEY, v TEXT)",
                vec![],
            )?;
            let at = self
                .get("SELECT v FROM meta WHERE k = 'schema'", vec![])?
                .map(|r| r.str("v").to_owned())
                .unwrap_or_default();
            if at == SCHEMA_VERSION {
                return Ok(());
            }
            self.exec_only(SCHEMA, vec![])?;
            // Each one only where it is missing: SQLite has no ADD COLUMN
            // IF NOT EXISTS, and a run cut short halfway must be able to
            // run again.
            for (table, column, decl) in ADDED_COLUMNS {
                if !self.columns(table)?.iter().any(|c| c == column) {
                    self.exec_only(
                        &format!("ALTER TABLE {table} ADD COLUMN {column} {decl}"),
                        vec![],
                    )?;
                }
            }
            // Keys published before the public record existed go into it.
            crate::e2e::backfill_log(self)?;
            // The one deletion here, asked for: the messages the server
            // could read (from before the end-to-end encryption).
            self.purge_plain()?;
            self.exec_only(
                "INSERT INTO meta(k, v) VALUES('schema', ?)
                 ON CONFLICT(k) DO UPDATE SET v = excluded.v",
                params![SCHEMA_VERSION],
            )?;
            Ok(())
        };
        setup().map_err(|e| format!("preparando o banco: {}", e.msg))
    }

    /// The messages from before the end-to-end encryption, which the server
    /// could read (no envelope): deleted once, with what hangs on them (the
    /// reactions, the reports, the marks of their sends) and their files,
    /// whose uploads the sweep then deletes from R2 as unused ones. The
    /// newest message of all is emptied and hidden instead, so its id is
    /// never given again (state.rs `sweep_expired` deletes it once another
    /// is newer). Cloudflare keeps the object's point-in-time recovery for
    /// 30 days; after that, they are gone for good.
    fn purge_plain(&self) -> ApiResult<()> {
        if self
            .get("SELECT 1 FROM meta WHERE k = 'plain_purged'", vec![])?
            .is_some()
        {
            return Ok(());
        }
        let t = crate::util::now();
        let max = self
            .get("SELECT COALESCE(MAX(id), 0) AS n FROM messages", vec![])?
            .map_or(0, |r| r.int("n"));
        for m in self.all(
            "SELECT attachments FROM messages WHERE envelope IS NULL AND attachments != '[]'",
            vec![],
        )? {
            if let Ok(Value::Array(atts)) = serde_json::from_str::<Value>(m.str("attachments")) {
                for url in atts.iter().filter_map(|a| a.get("url")?.as_str()) {
                    if let Some(name) = url.strip_prefix("/uploads/") {
                        self.run(
                            "UPDATE uploads SET used = 0, created = ? WHERE name = ?",
                            vec![Value::from(t - 24 * 3_600_000), Value::from(name)],
                        )?;
                    }
                }
            }
        }
        for table in ["reactions", "reports", "sent"] {
            self.run(
                &format!(
                    "DELETE FROM {table} WHERE message_id IN
                       (SELECT id FROM messages WHERE envelope IS NULL)"
                ),
                vec![],
            )?;
        }
        let gone = self.run(
            "DELETE FROM messages WHERE envelope IS NULL AND id != ?",
            vec![Value::from(max)],
        )?;
        self.run(
            "UPDATE messages SET content = '', attachments = '[]', sticker = NULL, gone_at = ?
              WHERE id = ? AND envelope IS NULL",
            vec![Value::from(t), Value::from(max)],
        )?;
        worker::console_log!("banco: {gone} mensagens sem criptografia apagadas");
        self.exec_only(
            "INSERT INTO meta(k, v) VALUES('plain_purged', ?)
             ON CONFLICT(k) DO UPDATE SET v = excluded.v",
            vec![Value::from(t.to_string())],
        )?;
        Ok(())
    }

    fn exec(&self, query: &str, p: Vec<Value>) -> ApiResult<worker::SqlCursor> {
        let binds: Vec<SqlStorageValue> = p.iter().map(sql).collect();
        self.sql.exec(query, binds).map_err(|e| {
            worker::console_error!("erro de banco: {e}");
            ApiError {
                status: 500,
                msg: INTERNAL.into(),
            }
        })
    }

    /// Runs a statement whose rows do not matter.
    fn exec_only(&self, query: &str, p: Vec<Value>) -> ApiResult<()> {
        self.exec(query, p)?;
        Ok(())
    }

    /// The columns a table has now (the names a query of it gives back,
    /// even with no rows).
    fn columns(&self, table: &str) -> ApiResult<Vec<String>> {
        Ok(self
            .exec(&format!("SELECT * FROM {table} LIMIT 0"), vec![])?
            .column_names())
    }

    pub fn all(&self, query: &str, p: Vec<Value>) -> ApiResult<Vec<Row>> {
        let cursor = self.exec(query, p)?;
        let cols = cursor.column_names();
        let mut out = Vec::new();
        for row in cursor.raw() {
            let values = row.map_err(|e| {
                worker::console_error!("lendo linha: {e}");
                ApiError {
                    status: 500,
                    msg: INTERNAL.into(),
                }
            })?;
            let mut r = Row::new();
            for (name, v) in cols.iter().zip(values) {
                r.insert(name.clone(), to_json(v));
            }
            out.push(r);
        }
        Ok(out)
    }

    pub fn get(&self, query: &str, p: Vec<Value>) -> ApiResult<Option<Row>> {
        Ok(self.all(query, p)?.into_iter().next())
    }

    /// Runs a statement and returns how many rows it changed.
    pub fn run(&self, query: &str, p: Vec<Value>) -> ApiResult<i64> {
        self.exec_only(query, p)?;
        self.scalar("SELECT changes() AS n")
    }

    /// Runs an INSERT and returns the new row's id.
    pub fn insert(&self, query: &str, p: Vec<Value>) -> ApiResult<i64> {
        self.exec_only(query, p)?;
        self.scalar("SELECT last_insert_rowid() AS n")
    }

    fn scalar(&self, query: &str) -> ApiResult<i64> {
        match self.get(query, vec![])? {
            Some(r) => Ok(r.int("n")),
            None => fail(500, INTERNAL),
        }
    }

    /// Runs `f` in a transaction, rolled back when it fails.
    ///
    /// Durable Objects commit everything an event writes at once, but only
    /// `transactionSync` undoes the writes of a half-finished operation.
    pub fn tx<T: 'static>(&self, f: impl FnOnce() -> ApiResult<T>) -> ApiResult<T> {
        // Handing a closure to JavaScript asks for `'static`, and `f`
        // borrows the caller's locals. `transactionSync` runs the callback
        // once, before it returns, and keeps no reference to it, so pointers
        // into this very frame stay valid for the whole call. They are read
        // only inside the callback, and the result only after the closure is
        // gone, so nothing aliases.
        let out: Rc<RefCell<Option<ApiResult<T>>>> = Rc::new(RefCell::new(None));
        let sink = out.clone();
        let mut f = Some(f);
        let call = move || -> Result<JsValue, JsValue> {
            let r = f.take().expect("transação única")();
            let failed = r.is_err();
            *sink.borrow_mut() = Some(r);
            // Throwing is what rolls the transaction back.
            if failed {
                return Err(JsValue::from_str("rollback"));
            }
            Ok(JsValue::NULL)
        };
        let boxed: Box<dyn FnMut() -> Result<JsValue, JsValue> + '_> = Box::new(call);
        // SAFETY: see above.
        let boxed: Box<dyn FnMut() -> Result<JsValue, JsValue> + 'static> =
            unsafe { std::mem::transmute(boxed) };
        let body = Closure::wrap(boxed);
        let called = transaction_sync(&self.storage, body.as_ref().unchecked_ref());
        drop(body);
        match out.borrow_mut().take() {
            Some(r) => r,
            // The callback never ran, or the commit itself failed.
            None => {
                if let Err(e) = called {
                    worker::console_error!("transação: {e:?}");
                }
                fail(500, INTERNAL)
            }
        }
    }
}

/// `ctx.storage.transactionSync(callback)`, which the Rust SDK does not wrap
/// yet. Only available on SQLite-backed objects, which is what this is.
fn transaction_sync(storage: &Storage, body: &JsValue) -> Result<JsValue, JsValue> {
    let raw: &JsValue = storage.as_raw().as_ref();
    let f = js_sys::Reflect::get(raw, &JsValue::from_str("transactionSync"))?;
    f.dyn_ref::<js_sys::Function>()
        .ok_or_else(|| JsValue::from_str("transactionSync não existe"))?
        .call1(raw, body)
}

/// One integer column of every row.
pub fn ids(rows: &[Row], col: &str) -> Vec<i64> {
    rows.iter().map(|r| r.int(col)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn added_columns_fit_the_rows_already_there() {
        for (table, column, decl) in ADDED_COLUMNS {
            // SQLite refuses ADD COLUMN ... NOT NULL without a default on
            // a table that has rows, and a failed migration stops the chat.
            if decl.contains("NOT NULL") {
                assert!(decl.contains("DEFAULT"), "{table}.{column}: {decl}");
            }
            // Nothing that could make the statement more than one column.
            for name in [table, column] {
                assert!(
                    name.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'),
                    "{name}"
                );
            }
            // Each table already exists by the time its columns come.
            assert!(
                SCHEMA.contains(&format!("CREATE TABLE IF NOT EXISTS {table}(")),
                "{table}"
            );
            // Kept out of the CREATE, so new and old databases get it the
            // same way.
            assert!(!SCHEMA.contains(&format!(" {column} ")), "{column}");
        }
    }
}
