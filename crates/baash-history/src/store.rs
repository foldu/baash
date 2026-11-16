//! The database half: one SQLite connection on its own thread.
//!
//! The shell must never wait for a disk. Every write is a message to the
//! worker that owns the connection; the only synchronous calls are
//! [`Store::recent`], [`Store::flush`] and opening, which happen at startup,
//! at exit, or because the user asked for `history`.
//!
//! The worker is also where the async lives. `sqlx` has no synchronous API and
//! no compio backend, so a runtime has to exist somewhere; putting it here
//! keeps it off the shell's thread, where compio owns the terminal
//! (`crates/baash/src/main.rs` explains that seam). Nothing async escapes this
//! module: the queue is `tokio`'s unbounded channel, the replies are ordinary
//! `std` ones, and every caller above is synchronous.
//!
//! # Order and exit
//!
//! One queue, drained in order, is what makes the database see the user's
//! operations in the order they typed them — `history -d 3` and then `history`
//! cannot race. Dropping the last `Arc` sends `Shutdown` behind every write and
//! joins the thread, so the last command's row is on disk before the process
//! leaves.
//!
//! # Schema
//!
//! `migrations/` is compiled into the binary by `sqlx::migrate!` and applied at
//! open, so a database from an older build is brought forward instead of being
//! refused. The first migration is idempotent on purpose: a file written before
//! the shell used SQLite already has these tables, and a user's history is
//! worth more than a strict first step.
//!
//! ```text
//! sessions(id, started_at, pid)
//! commands(id, session, seq, text, cwd, started_at, duration_ms, status, deleted)
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
//! a different command. SQLite reuses `max(id)+1` for a rowid table, so a real
//! `DELETE` of the newest row would hand the same number to the next command.
//! Hidden rows are therefore kept, and pruned at startup only when a newer row
//! exists (`deleted = 1 AND id < max(id)`), which cannot shrink the maximum and
//! so cannot cause reuse.
//!
//! One row is written when a line is submitted (so a crashed shell still
//! leaves the command behind) and updated when it finishes, with `$?` and the
//! wall time. The insert is the one write the shell waits for, because the id
//! it returns is the number the line will be shown with; the update is
//! fire-and-forget.

use std::fmt;
use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc::{self, SyncSender};
use std::thread;
use std::time::Duration;

use sqlx::Row;
use sqlx::sqlite::{
    SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions, SqliteSynchronous,
};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

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
    requests: Option<UnboundedSender<Request>>,
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
        Self::start(Some(path))
    }

    /// An empty database in memory, for tests and for `--no-history`.
    pub fn open_in_memory() -> std::io::Result<Arc<Self>> {
        Self::start(None)
    }

    fn start(path: Option<&str>) -> std::io::Result<Arc<Self>> {
        let path = path.map(<str>::to_owned);
        // The handshake is what keeps opening synchronous from the shell's
        // point of view: the worker reports its session number (or why it could
        // not open) and only then does `open` return.
        let (ready, opened) = mpsc::sync_channel(1);
        let (requests, queue) = unbounded_channel();
        let worker = thread::Builder::new()
            .name("baash-history".to_owned())
            .spawn(move || {
                match tokio::runtime::Builder::new_current_thread()
                    .enable_time()
                    .build()
                {
                    Ok(runtime) => runtime.block_on(worker_main(path, ready, queue)),
                    Err(error) => {
                        let _ =
                            ready.send(Err(format!("cannot start the history runtime: {error}")));
                    }
                }
            })
            .map_err(|error| {
                std::io::Error::other(format!("cannot start the history thread: {error}"))
            })?;
        let session = match opened.recv() {
            Ok(Ok(session)) => session,
            Ok(Err(message)) => {
                let _ = worker.join();
                return Err(std::io::Error::other(message));
            }
            Err(_) => {
                let _ = worker.join();
                return Err(std::io::Error::other(
                    "the history thread stopped before the database opened",
                ));
            }
        };
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
    pool: SqlitePool,
    /// The `sessions` row this shell opened.
    session: i64,
}

async fn worker_main(
    path: Option<String>,
    ready: SyncSender<Result<i64, String>>,
    queue: UnboundedReceiver<Request>,
) {
    let db = match open(path.as_deref()).await {
        Ok(db) => db,
        Err(message) => {
            let _ = ready.send(Err(message));
            return;
        }
    };
    let _ = ready.send(Ok(db.session));
    run(db, queue).await;
}

/// Open the pool, bring the schema forward, and start a session row.
async fn open(path: Option<&str>) -> Result<Db, String> {
    let mut options = match path {
        Some(path) => SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            // WAL is what lets a second shell open this file while the first
            // is running; `busy_timeout` is what turns the moment they collide
            // into a short wait rather than a failed command.
            .journal_mode(SqliteJournalMode::Wal)
            .busy_timeout(Duration::from_secs(5)),
        // A memory database has no journal to speak of — asking for WAL there
        // is answered with "memory" — so the pragma is only set for a file.
        None => SqliteConnectOptions::new().in_memory(true),
    };
    options = options
        .synchronous(SqliteSynchronous::Normal)
        .foreign_keys(true);
    let pool = SqlitePoolOptions::new()
        .min_connections(1)
        .max_connections(1)
        .connect_with(options)
        .await
        .map_err(|error| format!("cannot open the history database: {error}"))?;
    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .map_err(|error| format!("cannot migrate the history database: {error}"))?;
    prune(&pool)
        .await
        .map_err(|error| format!("cannot prune the history database: {error}"))?;
    let session = begin_session(&pool)
        .await
        .map_err(|error| format!("cannot record a history session: {error}"))?;
    Ok(Db { pool, session })
}

impl Db {
    async fn apply(&self, request: Request) {
        match request {
            Request::Begin {
                seq,
                text,
                cwd,
                started_at,
                reply,
            } => {
                // `RETURNING id` rather than `last_insert_rowid()`: the id has
                // to come from the same statement that wrote the row, because
                // a pool may not give the next call the same connection.
                let sql = "INSERT INTO commands (session, seq, text, cwd, started_at) \
                           VALUES (?, ?, ?, ?, ?) RETURNING id";
                let id = sqlx::query_scalar::<_, i64>(sql)
                    .bind(self.session)
                    .bind(seq)
                    .bind(text)
                    .bind(cwd)
                    .bind(started_at)
                    .fetch_one(&self.pool)
                    .await;
                let id = match id {
                    Ok(id) => Some(id),
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
                let sql = "UPDATE commands SET status = ?, duration_ms = ? WHERE id = ?";
                sqlx::query(sql)
                    .bind(status)
                    .bind(duration_ms)
                    .bind(id)
                    .execute(&self.pool)
                    .await
                    .map_err(|error| report(&error))
                    .ok();
            }
            Request::Recent { limit, reply } => {
                let rows = self.recent(limit).await.unwrap_or_else(|error| {
                    report(&error);
                    Vec::new()
                });
                let _ = reply.send(rows);
            }
            Request::Delete { id } => {
                sqlx::query("UPDATE commands SET deleted = 1 WHERE id = ?")
                    .bind(id)
                    .execute(&self.pool)
                    .await
                    .map_err(|error| report(&error))
                    .ok();
            }
            Request::ClearSession { session } => {
                sqlx::query("UPDATE commands SET deleted = 1 WHERE session = ? AND deleted = 0")
                    .bind(session)
                    .execute(&self.pool)
                    .await
                    .map_err(|error| report(&error))
                    .ok();
            }
            Request::Flush { reply } => {
                let _ = reply.send(());
            }
            Request::Shutdown => {}
        }
    }

    async fn recent(&self, limit: usize) -> sqlx::Result<Vec<Stored>> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows =
            sqlx::query("SELECT id, text FROM commands WHERE deleted = 0 ORDER BY id DESC LIMIT ?")
                .bind(limit)
                .fetch_all(&self.pool)
                .await?;
        let mut stored = Vec::with_capacity(rows.len());
        for row in rows {
            stored.push(Stored {
                id: row.try_get::<i64, _>(0).unwrap_or_default(),
                text: blob(&row, 1).unwrap_or_default(),
            });
        }
        stored.reverse();
        Ok(stored)
    }
}

async fn run(db: Db, mut queue: UnboundedReceiver<Request>) {
    while let Some(request) = queue.recv().await {
        let stop = matches!(request, Request::Shutdown);
        db.apply(request).await;
        if stop {
            break;
        }
    }
    // Checkpoint and release the file: the next shell should not have to
    // recover a WAL left behind by this one.
    db.pool.close().await;
}

/// Report a database failure once per process.
///
/// A broken database would otherwise print a diagnostic for every command the
/// user runs. The first one is the one worth reading.
fn report(error: &sqlx::Error) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static REPORTED: AtomicBool = AtomicBool::new(false);
    if !REPORTED.swap(true, Ordering::Relaxed) {
        eprintln!("baash: history: {error}");
    }
}

/// Read one column as bytes, accepting a `TEXT` value too.
///
/// Every row this shell writes stores `BLOB`, but a database written by another
/// tool — or by an earlier build — may hold text, and losing the history to a
/// type mismatch would be worse than reading it as bytes.
fn blob(row: &sqlx::sqlite::SqliteRow, index: usize) -> Option<Vec<u8>> {
    if let Ok(bytes) = row.try_get::<Vec<u8>, _>(index) {
        return Some(bytes);
    }
    row.try_get::<String, _>(index).ok().map(String::into_bytes)
}

/// Drop hidden rows that can no longer be the newest.
///
/// A hidden row at the maximum id must stay until a newer row exists, because
/// deleting it would let the database hand its id to the next insert.
async fn prune(pool: &SqlitePool) -> sqlx::Result<()> {
    let newest: Option<Option<i64>> = sqlx::query_scalar("SELECT max(id) FROM commands")
        .fetch_optional(pool)
        .await?;
    let Some(Some(newest)) = newest else {
        return Ok(());
    };
    sqlx::query("DELETE FROM commands WHERE deleted = 1 AND id < ?")
        .bind(newest)
        .execute(pool)
        .await?;
    Ok(())
}

/// Open a `sessions` row and return its id.
async fn begin_session(pool: &SqlitePool) -> sqlx::Result<i64> {
    sqlx::query_scalar::<_, i64>(
        "INSERT INTO sessions (started_at, pid) VALUES (?, ?) RETURNING id",
    )
    .bind(now_ms())
    .bind(i64::from(std::process::id()))
    .fetch_one(pool)
    .await
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
            "baash-history-store-{tag}-{}-{count}/history.db",
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

    /// The one write the shell waits for, timed.
    ///
    /// `cargo test -p baash-history --release --lib -- --ignored --nocapture`
    #[test]
    #[ignore = "measurement, not a test"]
    fn insert_round_trip() {
        let path = path("cost");
        let store = Store::open(&path).unwrap();
        const WARMUP: usize = 50;
        const RUNS: usize = 2000;
        let mut samples = Vec::with_capacity(RUNS);
        for seq in 0..(WARMUP + RUNS) {
            let text = format!("echo command number {seq}");
            let started = std::time::Instant::now();
            let id = store.begin(seq as i64 + 1, text.as_bytes(), Some(b"/tmp".to_vec()));
            let elapsed = started.elapsed();
            assert!(id.is_some(), "the insert failed");
            if seq >= WARMUP {
                samples.push(elapsed.as_secs_f64() * 1e6);
            }
        }
        store.flush();
        samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let at = |fraction: f64| samples[((samples.len() as f64) * fraction) as usize];
        let mean: f64 = samples.iter().sum::<f64>() / samples.len() as f64;
        println!(
            "insert round trip, release, WAL file, {RUNS} runs: \
             p50 {:.1} us, p90 {:.1} us, p99 {:.1} us, max {:.1} us, mean {:.1} us",
            at(0.50),
            at(0.90),
            at(0.99),
            samples[samples.len() - 1],
            mean,
        );
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
