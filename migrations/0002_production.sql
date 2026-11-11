-- Durable Objects' SQLite is the authority for chat membership and messages.
-- D1 only stores revocable sessions and a versioned, eventually consistent chat list.
CREATE TABLE IF NOT EXISTS auth_sessions (
  sid TEXT PRIMARY KEY NOT NULL,
  user_id TEXT NOT NULL,
  created_ms INTEGER NOT NULL,
  expires_ms INTEGER NOT NULL,
  revoked_ms INTEGER
);
CREATE INDEX IF NOT EXISTS auth_sessions_user ON auth_sessions(user_id, expires_ms);

CREATE TABLE IF NOT EXISTS chat_member_index (
  chat_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  revision INTEGER NOT NULL,
  active INTEGER NOT NULL CHECK (active IN (0, 1)),
  PRIMARY KEY (chat_id, user_id)
);
CREATE INDEX IF NOT EXISTS chat_member_index_user ON chat_member_index(user_id, active, chat_id);
