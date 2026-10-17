package server

import (
	"database/sql"
	"fmt"
	"os"
	"path/filepath"

	_ "modernc.org/sqlite"
)

// Same schema as the earlier Node server, so an existing prosa.db (accounts,
// servers, messages) keeps working unchanged.
const schema = `
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
`

// Row is one result row keyed by column name, the shape the JSON API sends.
type Row map[string]any

func (r Row) Int(k string) int64 {
	switch v := r[k].(type) {
	case int64:
		return v
	case float64:
		return int64(v)
	}
	return 0
}

func (r Row) Str(k string) string {
	if v, ok := r[k].(string); ok {
		return v
	}
	return ""
}

type querier interface {
	Exec(query string, args ...any) (sql.Result, error)
	Query(query string, args ...any) (*sql.Rows, error)
}

type store struct {
	db *sql.DB
	q  querier // the open transaction while tx() runs, the database otherwise
}

func openStore(dir string) (*store, error) {
	for _, sub := range []string{"uploads", "updates"} {
		if err := os.MkdirAll(filepath.Join(dir, sub), 0o755); err != nil {
			return nil, err
		}
	}
	dsn := "file:" + filepath.ToSlash(filepath.Join(dir, "prosa.db")) +
		"?_pragma=foreign_keys(1)&_pragma=journal_mode(WAL)&_pragma=busy_timeout(5000)"
	db, err := sql.Open("sqlite", dsn)
	if err != nil {
		return nil, err
	}
	// One connection: foreign_keys is per connection, and every request is
	// already serialized by the server's lock.
	db.SetMaxOpenConns(1)
	if _, err := db.Exec(schema); err != nil {
		db.Close()
		return nil, fmt.Errorf("criando o banco: %w", err)
	}
	// Columns added after the first release.
	db.Exec("ALTER TABLE sessions ADD COLUMN last_used INTEGER")
	if _, err := db.Exec("UPDATE sessions SET last_used = created WHERE last_used IS NULL"); err != nil {
		db.Close()
		return nil, err
	}
	return &store{db: db, q: db}, nil
}

func (s *store) all(query string, args ...any) []Row {
	rows, err := s.q.Query(query, args...)
	if err != nil {
		panic(err)
	}
	defer rows.Close()
	cols, _ := rows.Columns()
	var out []Row
	for rows.Next() {
		vals := make([]any, len(cols))
		ptrs := make([]any, len(cols))
		for i := range vals {
			ptrs[i] = &vals[i]
		}
		if err := rows.Scan(ptrs...); err != nil {
			panic(err)
		}
		row := make(Row, len(cols))
		for i, c := range cols {
			if b, ok := vals[i].([]byte); ok {
				row[c] = string(b)
			} else {
				row[c] = vals[i]
			}
		}
		out = append(out, row)
	}
	if err := rows.Err(); err != nil {
		panic(err)
	}
	return out
}

func (s *store) get(query string, args ...any) Row {
	rows := s.all(query, args...)
	if len(rows) == 0 {
		return nil
	}
	return rows[0]
}

func (s *store) run(query string, args ...any) int64 {
	res, err := s.q.Exec(query, args...)
	if err != nil {
		panic(err)
	}
	n, _ := res.RowsAffected()
	return n
}

func (s *store) insert(query string, args ...any) int64 {
	res, err := s.q.Exec(query, args...)
	if err != nil {
		panic(err)
	}
	id, _ := res.LastInsertId()
	return id
}

func (s *store) tx(fn func()) {
	t, err := s.db.Begin()
	if err != nil {
		panic(err)
	}
	s.q = t
	defer func() {
		s.q = s.db
		if p := recover(); p != nil {
			t.Rollback()
			panic(p)
		}
	}()
	fn()
	if err := t.Commit(); err != nil {
		panic(err)
	}
}

// ids returns one integer column of every row.
func ids(rows []Row, col string) []int64 {
	out := make([]int64, 0, len(rows))
	for _, r := range rows {
		out = append(out, r.Int(col))
	}
	return out
}
