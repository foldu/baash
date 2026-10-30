//! The database half: one Turso connection on its own thread.
//!
//! The shell must never wait for a disk. Every write is a message to the
//! worker that owns the connection; the only synchronous calls are
//! [`Store::recent`], [`Store::flush`] and opening, which happen at startup,
//! at exit, or because the user asked for `history`.
//!
//! # Schema
//!
//! ```text
//! sessions(id, started_at, pid)
//! commands(id, session, seq, text, cwd, started_at, duration_ms, status)
//! meta(name, value)
//! ```
//!
//! `text` and `cwd` are `BLOB`: shell text is bytes, not UTF-8. `id` is the
//! row id, and it is also the history *number*: global, assigned by the
//! database, and what `history` prints and `-d` names. `seq` numbers the
//! commands inside one session, which is what `history -c` uses to forget this
//! session without touching the rest of the table.
//!
//! # Deletion is a tombstone
//!
//! `history -d` and `history -c` set `deleted`; reads filter it out. That is
//! deliberate: a row id is a history *number*, and the number must never name
//! a different command. Turso reuses `max(id)+1`, so a real `DELETE` of the
//! newest row would hand the same number to the next command. Hidden rows are
//! therefore kept, and pruned at startup only when a newer row exists
//! (`deleted = 1 AND id < max(id)`), which cannot shrink the maximum and so
//! cannot cause reuse.
//!
//! One row is written when a line is submitted (so a crashed shell still
//! leaves the command behind) and updated when it finishes, with `$?` and the
//! wall time. The insert is the one write the shell waits for, because the id
//! it returns is the number the line will be shown with; the update is
//! fire-and-forget.

use std::fmt;
use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::thread;

use turso::{Builder, Connection, Database, Row, Value};

/// The schema this build writes.
///
/// Version 1 was the first cut of this table, before `deleted` existed; a
/// database carrying it is migrated in place by adding the column.
const SCHEMA_VERSION: i64 = 2;

/// A command read back from the database, in the order it ran.
pub(crate) struct Stored {
    pub(crate) id: i64,
    pub(crate) text: Vec<u8>,
}

enum Request {
    Begin {
        seq: i64,
        text: Vec<u8>,
        cwd: Option<Vec<u8>>,
        started_at: i64,
        reply: SyncSender<Option<i64>>,
    },
    Finish {
        id: i64,
        status: Option<i32>,
        duration_ms: Option<i64>,
    },
    Recent {
        limit: usize,
        reply: SyncSender<Vec<Stored>>,
    },
    Delete {
        id: i64,
    },
    ClearSession {
        session: i64,
    },
    Flush {
        reply: SyncSender<()>,
    },
    Shutdown,
}

/// A history database, shared by every clone of a [`History`](crate::History).
///
/// Dropping the last `Arc` drains the queue (the channel is FIFO, and the
/// worker sees `Shutdown` after every write) and joins the thread, so no
/// command is lost on exit.
pub struct Store {
    session: i64,
    requests: Option<Sender<Request>>,
    worker: Option<thread::JoinHandle<()>>,
}

impl fmt::Debug for Store {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(out, "Store(session {})", self.session)
    }
}

impl Store {
    /// Open (or create) the database at `path`.
    pub fn open(path: &Path) -> std::io::Result<Arc<Self>> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        let path = path
            .to_str()
            .ok_or_else(|| std::io::Error::other("the history database path is not valid UTF-8"))?;
        Self::start(path)
    }

    /// An empty database in memory, for tests and for `--no-history`.
    pub fn open_in_memory() -> std::io::Result<Arc<Self>> {
        Self::start(":memory:")
    }

    fn start(path: &str) -> std::io::Result<Arc<Self>> {
        // Two shells share one file. Multiprocess WAL is what makes the
        // second connection possible at all, and the busy timeout turns the
        // instant when the other shell is mid-write into a short wait rather
        // than a failed command.
        let builder = Builder::new_local(path);
        let builder = if path == ":memory:" {
            builder
        } else {
            builder.experimental_multiprocess_wal(true)
        };
        let database = crate::block_on(builder.build()).map_err(error)?;
        let connection = database.connect().map_err(error)?;
        connection
            .busy_timeout(std::time::Duration::from_secs(5))
            .map_err(error)?;
        crate::block_on(initialise(&connection)).map_err(error)?;
        let session = crate::block_on(begin_session(&connection)).map_err(error)?;
        let (requests, queue) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("bsh-history".to_owned())
            .spawn(move || {
                run(
                    Db {
                        _database: database,
                        connection,
                        session,
                    },
                    &queue,
                );
            })
            .map_err(|spawn| {
                std::io::Error::other(format!("cannot start the history thread: {spawn}"))
            })?;
        Ok(Arc::new(Self {
            session,
            requests: Some(requests),
            worker: Some(worker),
        }))
    }

    /// This shell's session number: the `sessions` row opened at startup.
    pub fn session(&self) -> i64 {
        self.session
    }

    /// Record that a line was submitted and return its row id.
    ///
    /// This is the one write the shell waits for: the id *is* the number the
    /// line will be shown with, so it has to exist before the line runs.
    /// `None` means the database refused the write (reported once) and the
    /// caller should number the line itself.
    pub(crate) fn begin(&self, seq: i64, text: &[u8], cwd: Option<Vec<u8>>) -> Option<i64> {
        let (reply, answer) = mpsc::sync_channel(1);
        if self.send(Request::Begin {
            seq,
            text: text.to_vec(),
            cwd,
            started_at: now_ms(),
            reply,
        }) {
            answer.recv().ok().flatten()
        } else {
            None
        }
    }

    /// Fill in the outcome of a recorded line.
    pub(crate) fn finish(&self, id: i64, status: Option<i32>, duration_ms: Option<i64>) {
        self.send(Request::Finish {
            id,
            status,
            duration_ms,
        });
    }

    /// The newest `limit` commands, oldest first.
    pub(crate) fn recent(&self, limit: usize) -> Vec<Stored> {
        let (reply, answer) = mpsc::sync_channel(1);
        if self.send(Request::Recent { limit, reply }) {
            answer.recv().unwrap_or_default()
        } else {
            Vec::new()
        }
    }

    /// Forget one command.
    pub(crate) fn delete(&self, id: i64) {
        self.send(Request::Delete { id });
    }

    /// Forget this session's commands, leaving the older history alone.
    pub(crate) fn clear_session(&self, session: i64) {
        self.send(Request::ClearSession { session });
    }

    /// Wait until every queued write has been applied.
    pub fn flush(&self) {
        let (reply, answer) = mpsc::sync_channel(1);
        if self.send(Request::Flush { reply }) {
            let _ = answer.recv();
        }
    }

    fn send(&self, request: Request) -> bool {
        match &self.requests {
            Some(requests) => requests.send(request).is_ok(),
            None => false,
        }
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        if let Some(requests) = self.requests.take() {
            let _ = requests.send(Request::Shutdown);
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// The worker's copy of the database.
struct Db {
    /// Kept alive: a connection borrows the database's internals.
    _database: Database,
    connection: Connection,
    /// The `sessions` row this shell opened.
    session: i64,
}

impl Db {
    fn apply(&self, request: Request) {
        match request {
            Request::Begin {
                seq,
                text,
                cwd,
                started_at,
                reply,
            } => {
                let sql = "INSERT INTO commands (session, seq, text, cwd, started_at) \
                           VALUES (?1, ?2, ?3, ?4, ?5)";
                let params = (self.session, seq, text, cwd, started_at);
                let id = match crate::block_on(self.connection.execute(sql, params)) {
                    Ok(_) => Some(self.connection.last_insert_rowid()),
                    Err(error) => {
                        report(&error);
                        None
                    }
                };
                let _ = reply.send(id);
            }
            Request::Finish {
                id,
                status,
                duration_ms,
            } => {
                let sql = "UPDATE commands SET status = ?1, duration_ms = ?2 WHERE id = ?3";
                crate::block_on(self.connection.execute(sql, (status, duration_ms, id)))
                    .map_err(|error| report(&error))
                    .ok();
            }
            Request::Recent { limit, reply } => {
                let rows = self.recent(limit).unwrap_or_else(|error| {
                    report(&error);
                    Vec::new()
                });
                let _ = reply.send(rows);
            }
            Request::Delete { id } => {
                crate::block_on(
                    self.connection
                        .execute("UPDATE commands SET deleted = 1 WHERE id = ?1", (id,)),
                )
                .map_err(|error| report(&error))
                .ok();
            }
            Request::ClearSession { session } => {
                crate::block_on(self.connection.execute(
                    "UPDATE commands SET deleted = 1 WHERE session = ?1 AND deleted = 0",
                    (session,),
                ))
                .map_err(|error| report(&error))
                .ok();
            }
            Request::Flush { reply } => {
                let _ = reply.send(());
            }
            Request::Shutdown => {}
        }
    }

    fn recent(&self, limit: usize) -> turso::Result<Vec<Stored>> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let mut rows = crate::block_on(self.connection.query(
            "SELECT id, text FROM commands WHERE deleted = 0 ORDER BY id DESC LIMIT ?1",
            (limit,),
        ))?;
        let mut stored = Vec::new();
        while let Some(row) = crate::block_on(rows.next())? {
            stored.push(Stored {
                id: as_integer(&row, 0).unwrap_or_default(),
                text: as_blob(&row, 1).unwrap_or_default(),
            });
        }
        stored.reverse();
        Ok(stored)
    }
}

fn run(db: Db, queue: &Receiver<Request>) {
    while let Ok(request) = queue.recv() {
        let stop = matches!(request, Request::Shutdown);
        db.apply(request);
        if stop {
            break;
        }
    }
}

/// Report a database failure once per process.
///
/// A broken database would otherwise print a diagnostic for every command the
/// user runs. The first one is the one worth reading.
fn report(error: &turso::Error) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static REPORTED: AtomicBool = AtomicBool::new(false);
    if !REPORTED.swap(true, Ordering::Relaxed) {
        eprintln!("bsh: history: {error}");
    }
}

fn error(error: turso::Error) -> std::io::Error {
    std::io::Error::other(error.to_string())
}

async fn initialise(connection: &Connection) -> turso::Result<()> {
    connection
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS meta (\
                 name TEXT PRIMARY KEY, value TEXT NOT NULL);\
             CREATE TABLE IF NOT EXISTS sessions (\
                 id INTEGER PRIMARY KEY, started_at INTEGER NOT NULL, pid INTEGER NOT NULL);\
             CREATE TABLE IF NOT EXISTS commands (\
                 id INTEGER PRIMARY KEY,\
                 session INTEGER NOT NULL,\
                 seq INTEGER NOT NULL,\
                 text BLOB NOT NULL,\
                 cwd BLOB,\
                 started_at INTEGER NOT NULL,\
                 duration_ms INTEGER,\
                 status INTEGER,\
                 deleted INTEGER NOT NULL DEFAULT 0);\
             CREATE UNIQUE INDEX IF NOT EXISTS commands_key ON commands (session, seq);",
        )
        .await?;
    let mut rows = connection
        .query("SELECT value FROM meta WHERE name = 'schema_version'", ())
        .await?;
    match rows.next().await? {
        Some(row) => {
            let found = as_version(&row, 0).unwrap_or_default();
            if found == 1 {
                // Version 1 had no `deleted` column.
                connection
                    .execute(
                        "ALTER TABLE commands ADD COLUMN deleted INTEGER NOT NULL DEFAULT 0",
                        (),
                    )
                    .await?;
                connection
                    .execute(
                        "UPDATE meta SET value = ?1 WHERE name = 'schema_version'",
                        (SCHEMA_VERSION.to_string(),),
                    )
                    .await?;
            } else if found != SCHEMA_VERSION {
                return Err(turso::Error::Error(format!(
                    "the history database has schema version {found}, but this bsh understands \
                     version {SCHEMA_VERSION}"
                )));
            }
        }
        None => {
            connection
                .execute(
                    "INSERT INTO meta (name, value) VALUES ('schema_version', ?1)",
                    (SCHEMA_VERSION.to_string(),),
                )
                .await?;
        }
    }
    prune(connection).await?;
    Ok(())
}

/// Open a `sessions` row and return its id.
async fn begin_session(connection: &Connection) -> turso::Result<i64> {
    connection
        .execute(
            "INSERT INTO sessions (started_at, pid) VALUES (?1, ?2)",
            (now_ms(), i64::from(std::process::id())),
        )
        .await?;
    Ok(connection.last_insert_rowid())
}

fn as_integer(row: &Row, index: usize) -> Option<i64> {
    match row.get_value(index).ok()? {
        Value::Integer(value) => Some(value),
        _ => None,
    }
}

/// The schema version `meta.value` carries, as text.
fn as_version(row: &Row, index: usize) -> Option<i64> {
    match row.get_value(index).ok()? {
        Value::Text(text) => text.parse().ok(),
        Value::Integer(value) => Some(value),
        _ => None,
    }
}

fn as_blob(row: &Row, index: usize) -> Option<Vec<u8>> {
    match row.get_value(index).ok()? {
        Value::Blob(bytes) => Some(bytes),
        Value::Text(text) => Some(text.into_bytes()),
        _ => None,
    }
}

/// Milliseconds since the Unix epoch.
/// Drop hidden rows that can no longer be the newest.
///
/// A hidden row at the maximum id must stay until a newer row exists, because
/// deleting it would let the database hand its id to the next insert.
async fn prune(connection: &Connection) -> turso::Result<()> {
    let mut rows = connection.query("SELECT max(id) FROM commands", ()).await?;
    let Some(row) = rows.next().await? else {
        return Ok(());
    };
    let Some(newest) = as_integer(&row, 0) else {
        return Ok(());
    };
    connection
        .execute(
            "DELETE FROM commands WHERE deleted = 1 AND id < ?1",
            (newest,),
        )
        .await?;
    Ok(())
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(tag: &str) -> std::path::PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let count = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "bsh-history-store-{tag}-{}-{count}/history.db",
            std::process::id()
        ))
    }

    #[test]
    fn rows_survive_a_close_and_reopen() {
        let path = path("reopen");
        {
            let store = Store::open(&path).unwrap();
            let first = store.begin(1, b"echo one", Some(b"/tmp".to_vec())).unwrap();
            store.finish(first, Some(0), Some(12));
            store.begin(2, b"echo two", None).unwrap();
            store.flush();
        }
        let store = Store::open(&path).unwrap();
        let recent = store.recent(10);
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].text, b"echo one");
        assert_eq!(recent[1].text, b"echo two");
        assert_eq!(recent[1].id, 2, "ids are the database's, not per session");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn ids_are_global_across_sessions() {
        let path = path("sessions");
        let first = Store::open(&path).unwrap();
        let second = Store::open(&path).unwrap();
        assert_ne!(first.session(), second.session());
        assert_eq!(first.begin(1, b"a", None), Some(1));
        assert_eq!(second.begin(1, b"b", None), Some(2));
        let third = Store::open(&path).unwrap();
        assert_eq!(third.begin(1, b"c", None), Some(3));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn recent_is_ordered_and_limited() {
        let store = Store::open_in_memory().unwrap();
        for seq in 1..=10 {
            store.begin(seq, format!("cmd {seq}").as_bytes(), None);
        }
        store.flush();
        let recent = store.recent(3);
        assert_eq!(recent.len(), 3);
        assert_eq!(recent[0].text, b"cmd 8");
        assert_eq!(recent[2].text, b"cmd 10");
        assert_eq!(recent[2].id, 10);
    }

    #[test]
    fn deleting_removes_one_row() {
        let store = Store::open_in_memory().unwrap();
        store.begin(1, b"one", None).unwrap();
        store.begin(2, b"two", None).unwrap();
        store.delete(1);
        store.flush();
        let recent = store.recent(10);
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].text, b"two");
    }

    #[test]
    fn ids_are_not_reused_after_a_delete() {
        let store = Store::open_in_memory().unwrap();
        assert_eq!(store.begin(1, b"one", None), Some(1));
        assert_eq!(store.begin(2, b"two", None), Some(2));
        store.delete(2);
        store.flush();
        // The next id is the table's high-water mark, not a free slot.
        assert_eq!(store.begin(3, b"three", None), Some(3));
        store.clear_session(store.session());
        store.flush();
        assert_eq!(store.begin(4, b"four", None), Some(4));
    }

    #[test]
    fn clearing_removes_only_the_given_session() {
        let path = path("clear-session");
        let other = Store::open(&path).unwrap();
        other.begin(1, b"other", None).unwrap();
        other.flush();
        let mine = Store::open(&path).unwrap();
        mine.begin(1, b"mine", None).unwrap();
        mine.clear_session(mine.session());
        mine.flush();
        assert_eq!(mine.recent(10).len(), 1);
        assert_eq!(mine.recent(10)[0].text, b"other");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_binary_command_is_stored_byte_for_byte() {
        let store = Store::open_in_memory().unwrap();
        let text = b"printf '\xff\xfe'".to_vec();
        store.begin(1, &text, None);
        store.flush();
        assert_eq!(store.recent(1)[0].text, text);
    }
}
