-- The history schema, in the shape the shell had before it was versioned.
--
-- `IF NOT EXISTS` is deliberate and permanent for this migration: a database
-- written by the previous engine already has these tables and no
-- `_sqlx_migrations` row, and a user's history is worth more than a strict
-- first migration. Later migrations can be strict, because by then every file
-- that opens has been through this one.
--
-- `text` and `cwd` are BLOB: shell text is bytes, not UTF-8. `id` is the
-- history *number*, assigned by the database, and `commands_key` makes a
-- session's sequence unique, which is what `history -c` uses to forget exactly
-- this session.
CREATE TABLE IF NOT EXISTS sessions (
    id INTEGER PRIMARY KEY,
    started_at INTEGER NOT NULL,
    pid INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS commands (
    id INTEGER PRIMARY KEY,
    session INTEGER NOT NULL,
    seq INTEGER NOT NULL,
    text BLOB NOT NULL,
    cwd BLOB,
    started_at INTEGER NOT NULL,
    duration_ms INTEGER,
    status INTEGER,
    deleted INTEGER NOT NULL DEFAULT 0
);

CREATE UNIQUE INDEX IF NOT EXISTS commands_key ON commands (session, seq);
