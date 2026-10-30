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
//! `text` and `cwd` are `BLOB`: shell text is bytes, not UTF-8. `seq` numbers
//! the commands inside one session, so `(session, seq)` identifies a command
//! before the row is written and without a round trip for a row id.
//!
//! One row is written when a line is submitted (so a crashed shell still
//! leaves the command behind) and updated when it finishes, with `$?` and the
//! wall time. Both are fire-and-forget.

use std::fmt;
use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::thread;

use turso::{Builder, Connection, Database, Row, Value};

/// The schema this build writes. A database written by another version is
/// refused rather than migrated: there has only ever been one version.
const SCHEMA_VERSION: i64 = 1;

/// A command read back from the database, in the order it ran.
pub(crate) struct Stored {
    pub(crate) session: i64,
    pub(crate) seq: i64,
    pub(crate) text: Vec<u8>,
}

enum Request {
    Begin {
        seq: i64,
        text: Vec<u8>,
        cwd: Option<Vec<u8>>,
        started_at: i64,
    },
    Finish {
        session: i64,
        seq: i64,
        status: Option<i32>,
        duration_ms: Option<i64>,
    },
    Recent {
        limit: usize,
        reply: SyncSender<Vec<Stored>>,
    },
    Delete {
        session: i64,
        seq: i64,
    },
    Clear,
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

    /// Record that a line was submitted, before it runs.
    pub(crate) fn begin(&self, seq: i64, text: &[u8], cwd: Option<Vec<u8>>) {
        self.send(Request::Begin {
            seq,
            text: text.to_vec(),
            cwd,
            started_at: now_ms(),
        });
    }

    /// Fill in the outcome of a recorded line.
    pub(crate) fn finish(
        &self,
        session: i64,
        seq: i64,
        status: Option<i32>,
        duration_ms: Option<i64>,
    ) {
        self.send(Request::Finish {
            session,
            seq,
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
    pub(crate) fn delete(&self, session: i64, seq: i64) {
        self.send(Request::Delete { session, seq });
    }

    /// Forget everything.
    pub(crate) fn clear(&self) {
        self.send(Request::Clear);
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
            } => {
                let sql = "INSERT INTO commands (session, seq, text, cwd, started_at) \
                           VALUES (?1, ?2, ?3, ?4, ?5)";
                let params = (self.session, seq, text, cwd, started_at);
                crate::block_on(self.connection.execute(sql, params))
                    .map_err(|error| report(&error))
                    .ok();
            }
            Request::Finish {
                session,
                seq,
                status,
                duration_ms,
            } => {
                let sql = "UPDATE commands SET status = ?1, duration_ms = ?2 \
                           WHERE session = ?3 AND seq = ?4";
                crate::block_on(
                    self.connection
                        .execute(sql, (status, duration_ms, session, seq)),
                )
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
            Request::Delete { session, seq } => {
                crate::block_on(self.connection.execute(
                    "DELETE FROM commands WHERE session = ?1 AND seq = ?2",
                    (session, seq),
                ))
                .map_err(|error| report(&error))
                .ok();
            }
            Request::Clear => {
                crate::block_on(self.connection.execute("DELETE FROM commands", ()))
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
            "SELECT session, seq, text FROM commands ORDER BY id DESC LIMIT ?1",
            (limit,),
        ))?;
        let mut stored = Vec::new();
        while let Some(row) = crate::block_on(rows.next())? {
            stored.push(Stored {
                session: as_integer(&row, 0).unwrap_or_default(),
                seq: as_integer(&row, 1).unwrap_or_default(),
                text: as_blob(&row, 2).unwrap_or_default(),
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
                 status INTEGER);\
             CREATE UNIQUE INDEX IF NOT EXISTS commands_key ON commands (session, seq);",
        )
        .await?;
    let mut rows = connection
        .query("SELECT value FROM meta WHERE name = 'schema_version'", ())
        .await?;
    match rows.next().await? {
        Some(row) => {
            let found = as_version(&row, 0).unwrap_or_default();
            if found != SCHEMA_VERSION {
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
            store.begin(1, b"echo one", Some(b"/tmp".to_vec()));
            store.finish(store.session(), 1, Some(0), Some(12));
            store.begin(2, b"echo two", None);
            store.flush();
        }
        let store = Store::open(&path).unwrap();
        let recent = store.recent(10);
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].text, b"echo one");
        assert_eq!(recent[1].text, b"echo two");
        assert_eq!(recent[1].seq, 2);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn the_sessions_of_two_shells_are_distinct() {
        let path = path("sessions");
        let first = Store::open(&path).unwrap();
        let second = Store::open(&path).unwrap();
        assert_ne!(first.session(), second.session());
        first.begin(1, b"a", None);
        second.begin(1, b"b", None);
        first.flush();
        second.flush();
        let store = Store::open(&path).unwrap();
        let recent = store.recent(10);
        assert_eq!(recent.len(), 2);
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
    }

    #[test]
    fn deleting_removes_one_row_and_clearing_removes_all() {
        let store = Store::open_in_memory().unwrap();
        store.begin(1, b"one", None);
        store.begin(2, b"two", None);
        store.delete(store.session(), 1);
        store.flush();
        assert_eq!(store.recent(10).len(), 1);
        store.clear();
        store.flush();
        assert!(store.recent(10).is_empty());
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
