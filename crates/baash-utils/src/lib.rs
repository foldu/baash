//! Embedded coreutils.
//!
//! The standard utilities are regular builtins that deliberately fall through
//! to `PATH` (`DESIGN.md` §2), so an external binary always wins. That works on
//! Unix and is useless on Windows, where a `PATH` has no coreutils at all. This
//! crate is the fallback: when resolution finds nothing, the executor runs the
//! uutils implementation in this process instead of reporting "command not
//! found".
//!
//! Only the utilities that are actually embedded are linked: each `uu_*` crate
//! is a library with one entry point, and the table below is the whole
//! surface. The set is chosen, not accreted — `docs/portability.md` §6 lists it
//! by name, says what is deliberately out and why, and records the measured
//! cost of the table.
//!
//! `clear` is the exception that is not uutils at all: ncurses owns the name and
//! a Windows `PATH` has no `clear.exe`, so it is written here (see `clear.rs`).
//!
//! `printf`, `test`/`[`, `true`, `false`, `echo` and `pwd` are the shell's own
//! bodies — they need the format language, the expression grammar, the shell
//! state or the descriptor table, and a script must not notice the difference
//! between them and a utility. `grep`, `sed`, `awk` and the rest of the
//! non-coreutils toolbox are deliberately absent.
//!
//! Error text needs one more piece. uutils keeps the message catalog in a
//! *thread-local* that is installed once, and it loads exactly one utility's
//! strings, because a real `uu_*` binary is one utility per process. A shell is
//! many in one process, so each utility gets a dedicated worker thread that
//! installs its own catalog (see [`serve`]); the English strings are already
//! compiled into the binary by uucore's build script, so no `locales/`
//! directory is shipped or extracted.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::collections::HashMap;
use std::ffi::OsString;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::thread;

/// A uutils entry point: the arguments, and the status to exit with.
///
/// By value, because that is what `uumain` takes; [`Native`] borrows instead.
type Entry = fn(Vec<OsString>) -> i32;

/// A utility written here rather than linked from uutils.
type Native = fn(&[OsString]) -> i32;

/// A body that runs in a child of the shell rather than in it (see
/// [`ISOLATED`]): shell bytes in, exit status out.
type Body = fn(&[Vec<u8>]) -> i32;

/// The flag the shell passes to its own child before the utility's name.
///
/// `baash --__embedded grep -n x file` is `grep` running as a child of the
/// shell; it is handled before any option parsing, so the child is the utility
/// and nothing else — no rc files, no history, no options of the shell's own.
pub const ISOLATED_FLAG: &str = "--__embedded";

/// Utilities that have to own the process, so they run in a child of the shell.
///
/// Three things make a body unsafe to call in-process, and each of these has at
/// least one:
///
/// * **`process::exit`.** `sed` exits from inside `uumain` when it is given no
///   arguments at all — in-process that ends the *shell*.
/// * **Signal state.** `uucore`'s `#[uucore::main]` turns `SIGPIPE` back to its
///   default on the way into the body (GNU behaviour: die on a broken pipe),
///   and the shell needs it ignored so a write returns `EPIPE` instead.
///   Measured: one in-process `grep` drops bit 12 from the shell's `SigIgn`
///   mask, and the next broken pipe kills the session (`docs/portability.md`
///   §6).
/// * **`argv[0]` identity.** `uucore` derives the name it prints from the
///   *process* argv (`util_name()`), which in-process is `baash`, so a message
///   would be attributed to the shell.
///
/// A child has none of those: it exits, it dies, it prints its own name, and
/// the shell's signal state and session are untouched. It costs one
/// `posix_spawn` on Unix (~0.5 ms) or one `CreateProcess` on Windows (~2–5 ms),
/// which is what running the same tool from `PATH` costs — and on Windows there
/// is nothing on `PATH` to run.
const ISOLATED: &[(&str, Body)] = &[
    ("awk", awk::main),
    ("find", body_find),
    ("grep", body_grep),
    ("sed", body_sed),
    ("xargs", body_xargs),
];

mod awk;
mod clear;

/// The utilities written here rather than linked from uutils.
///
/// `clear` is the whole list: it belongs to ncurses, which uutils' coreutils
/// does not cover, and a Windows `PATH` has no `clear.exe` at all (`cls` is a
/// `cmd.exe` built-in). They run on the calling thread — there is no message
/// catalog to install, which is the only reason the uutils entries get a worker.
const NATIVE: &[(&str, Native)] = &[("clear", clear::main)];

/// A reader for a file the shell carries in memory rather than on disk.
///
/// The embedded bash-completion tree is mounted in the shell's virtual file
/// system (`crates/baash/src/bash_completion.rs`), and a *child* cannot read a
/// mount. bash-completion's make helper names its awk script by path —
/// `${BASH_SOURCE%/*}/../helpers-core/make-extract-targets.awk` — so the binary
/// registers a reader here and a body that opens files by name asks it before
/// the file system. `awk` is the only body this shell wrote, and the only one
/// that can ask: the uutils bodies open their own files with `std::fs`.
/// A reader for a mounted file, as the shell registers it.
type FileSource = fn(&[u8]) -> Option<Vec<u8>>;

static IN_MEMORY: std::sync::OnceLock<FileSource> = std::sync::OnceLock::new();

/// Register the in-memory reader. Called by the shell at startup; later calls
/// are ignored, so the first registration wins.
pub fn set_file_source(source: FileSource) {
    let _ = IN_MEMORY.set(source);
}

/// The bytes of `path`, when the shell carries that file in memory.
pub fn mounted_file(path: &[u8]) -> Option<Vec<u8>> {
    IN_MEMORY.get().and_then(|source| source(path))
}

/// The embedded utilities, by the name a command is typed as.
///
/// The first argument passed to [`run`] is the name itself, because that is
/// what `argv[0]` would be and what clap parses.
///
/// The set is the coreutils a script reaches for on a machine with no POSIX
/// userland — not every utility uutils has. What is deliberately *not* here
/// (`chmod`, `chown`, `mkfifo`, `df`, `id`, `uname`, …) has no truthful answer
/// on Windows, and what forks (`env`, `timeout`, `nice`, `nohup`) breaks the
/// rule that an embedded utility is a body in this process; both lists are in
/// `docs/portability.md` §6.
const UTILITIES: &[(&str, Entry)] = &[
    ("base64", run_base64),
    ("basename", run_basename),
    ("cat", run_cat),
    ("cksum", run_cksum),
    ("comm", run_comm),
    ("cp", run_cp),
    ("cut", run_cut),
    ("date", run_date),
    ("dirname", run_dirname),
    ("du", run_du),
    ("expr", run_expr),
    ("head", run_head),
    ("ln", run_ln),
    ("ls", run_ls),
    ("md5sum", run_md5sum),
    ("mkdir", run_mkdir),
    ("mktemp", run_mktemp),
    ("mv", run_mv),
    ("nl", run_nl),
    ("nproc", run_nproc),
    ("od", run_od),
    ("paste", run_paste),
    ("readlink", run_readlink),
    ("realpath", run_realpath),
    ("rm", run_rm),
    ("rmdir", run_rmdir),
    ("seq", run_seq),
    ("sha1sum", run_sha1sum),
    ("sha256sum", run_sha256sum),
    ("shuf", run_shuf),
    ("sleep", run_sleep),
    ("sort", run_sort),
    ("split", run_split),
    ("tac", run_tac),
    ("tail", run_tail),
    ("tee", run_tee),
    ("touch", run_touch),
    ("tr", run_tr),
    ("truncate", run_truncate),
    ("uniq", run_uniq),
    ("wc", run_wc),
];

/// One call handed to a utility's worker thread.
struct Job {
    argv: Vec<OsString>,
    reply: Sender<i32>,
}

/// What waiting for an embedded utility produced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Waited {
    /// The utility finished with this status.
    Status(i32),
    /// [`run_interruptible`]'s caller asked to stop waiting. The utility is
    /// still on its worker and will stop after the call it is in; its status is
    /// discarded, so the caller must supply its own.
    Abandoned,
}

/// The utilities whose wait may be given up.
///
/// Only `sleep`. A utility that *writes* cannot be abandoned safely: its worker
/// would keep printing into the shell's descriptor after the next prompt has
/// been drawn, and a running `uutils` body cannot be stopped from outside. The
/// one whose whole effect is waiting writes nothing, so the shell can stop
/// waiting for it exactly where bash's `SIGINT` stops its `sleep` child.
const INTERRUPTIBLE: &[&str] = &["sleep"];

/// The worker threads, one per utility, created on first use.
///
/// A pool of interchangeable workers would be wrong: the catalog `uucore`
/// installs is per thread and set once, so a worker that ran `cat` and then
/// `mkdir` would still be speaking `cat`. Keying by utility is what makes the
/// reuse correct, and it costs one thread per utility actually used (bounded
/// by the table above).
fn workers() -> &'static Mutex<HashMap<&'static str, Sender<Job>>> {
    static WORKERS: OnceLock<Mutex<HashMap<&'static str, Sender<Job>>>> = OnceLock::new();
    WORKERS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn lock_workers() -> std::sync::MutexGuard<'static, HashMap<&'static str, Sender<Job>>> {
    workers().lock().unwrap_or_else(PoisonError::into_inner)
}

/// The worker for `name`, started if this is the first call.
fn worker(name: &'static str, entry: Entry) -> Option<Sender<Job>> {
    let mut map = lock_workers();
    if let Some(jobs) = map.get(name) {
        return Some(jobs.clone());
    }
    let (jobs, incoming) = channel();
    thread::Builder::new()
        .name(format!("baash-{name}"))
        .stack_size(2 * 1024 * 1024)
        .spawn(move || serve(name, entry, incoming))
        .ok()?;
    map.insert(name, jobs.clone());
    Some(jobs)
}

/// Forget a worker whose channel is gone, so the next call starts a fresh one.
fn forget(name: &'static str) {
    lock_workers().remove(name);
}

/// A utility's thread: install its catalog once, then run jobs.
fn serve(name: &'static str, entry: Entry, jobs: Receiver<Job>) {
    // The English catalog is compiled into `uucore`'s build script; the path
    // argument only names the utility whose strings to load. Nothing is read
    // from disk, which is what keeps this a single binary.
    let _ = uucore::locale::setup_localization(name);
    while let Ok(job) = jobs.recv() {
        let status = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| entry(job.argv)))
            .unwrap_or(101);
        // A utility's own binary flushes at exit; in-process the shell must, or
        // `printf x > file` leaves the bytes in the buffer.
        let _ = std::io::Write::flush(&mut std::io::stdout());
        let _ = std::io::Write::flush(&mut std::io::stderr());
        if job.reply.send(status).is_err() {
            break;
        }
    }
}

/// Run `argv` if `argv[0]` names an embedded utility.
///
/// Returns `None` when the name is not embedded, so the caller keeps whatever
/// diagnostic it would have produced. Blocks until the utility finishes, like a
/// built-in. The utility inherits this process's environment; the shell calls
/// [`run_with`] instead so an isolated utility gets the shell's exported
/// variables rather than the ones `baash` was started with.
pub fn run(argv: &[Vec<u8>]) -> Option<i32> {
    run_with(argv, None)
}

/// [`run`], with the environment an isolated utility's child receives.
///
/// `environment` is the shell's exported variables as `KEY=VALUE` bytes — what
/// an external command is given — and `None` inherits this process's
/// environment (a test harness, the child itself). It reaches a *child* only:
/// the in-process bodies read `std::env`, which no shell command changes, so a
/// utility that runs on this thread sees the environment `baash` started with.
/// That is why `awk` (which reads `ENVIRON`) is isolated, and it is recorded in
/// `docs/portability.md` §6.
pub fn run_with(argv: &[Vec<u8>], environment: Option<&[Vec<u8>]>) -> Option<i32> {
    match dispatch(argv, environment, None)? {
        Waited::Status(status) => Some(status),
        // Only a caller that passed a cancel hook can reach this, and
        // `run_with` passed none.
        Waited::Abandoned => None,
    }
}

/// [`run_with`], giving up the wait when `cancel` says so.
///
/// A shell that blocks `SIGINT` cannot be interrupted while it waits inside one
/// command, so a blocking utility is the one shape where an interrupt has to be
/// asked for rather than delivered. `cancel` is asked between slices of the
/// wait, on this thread and never while the utility is running; it must only
/// report whether the shell wants the command to stop.
///
/// The answer abandons only an [`INTERRUPTIBLE`] utility: every other one holds
/// a descriptor and may be writing, and there is no way to stop a thread.
pub fn run_interruptible(
    argv: &[Vec<u8>],
    environment: Option<&[Vec<u8>]>,
    cancel: &mut dyn FnMut() -> bool,
) -> Option<Waited> {
    dispatch(argv, environment, Some(cancel))
}

fn dispatch(
    argv: &[Vec<u8>],
    environment: Option<&[Vec<u8>]>,
    cancel: Option<&mut dyn FnMut() -> bool>,
) -> Option<Waited> {
    let (typed, _) = argv.split_first()?;
    let typed = std::str::from_utf8(typed).ok()?;
    let args: Vec<OsString> = argv
        .iter()
        .map(|arg| baash_sys::os::os_from_bytes(arg))
        .collect();
    if let Some((_, body)) = ISOLATED.iter().find(|(utility, _)| *utility == typed) {
        return Some(Waited::Status(isolated(argv, *body, typed, environment)));
    }
    if let Some((_, main)) = NATIVE.iter().find(|(utility, _)| *utility == typed) {
        return Some(Waited::Status(main(&args)));
    }
    let (name, entry) = *UTILITIES.iter().find(|(utility, _)| *utility == typed)?;
    let mut cancel = cancel.filter(|_| INTERRUPTIBLE.contains(&name));
    match attempt(name, entry, args, &mut cancel) {
        Ok(waited) => Some(waited),
        // The worker died with the job (it panics outside `catch_unwind` only
        // if the shell's own dispatch is broken); try once on a fresh thread.
        Err(args) if !args.is_empty() => match attempt(name, entry, args, &mut cancel) {
            Ok(waited) => Some(waited),
            Err(_) => {
                eprintln!("baash: {name}: embedded utility failed");
                Some(Waited::Status(1))
            }
        },
        Err(_) => {
            eprintln!("baash: {name}: embedded utility failed");
            Some(Waited::Status(1))
        }
    }
}

/// Hand one call to the worker, giving `argv` back when the worker died.
///
/// The wait is sliced rather than one `recv`, so `cancel` can give it up: the
/// job's reply channel is then dropped, which is what tells the worker to stop
/// after the call it is in, and the worker is forgotten so the next call starts
/// a fresh one instead of queueing behind a sleep that is still running.
fn attempt(
    name: &'static str,
    entry: Entry,
    args: Vec<OsString>,
    cancel: &mut Option<&mut dyn FnMut() -> bool>,
) -> Result<Waited, Vec<OsString>> {
    let Some(jobs) = worker(name, entry) else {
        return Err(args);
    };
    let (reply, answer) = channel();
    let job = match jobs.send(Job { argv: args, reply }) {
        Ok(()) => loop {
            match answer.recv_timeout(baash_sys::INTERRUPT_SLICE) {
                Ok(status) => return Ok(Waited::Status(status)),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    if cancel.as_mut().is_some_and(|asked| asked()) {
                        forget(name);
                        return Ok(Waited::Abandoned);
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    forget(name);
                    return Err(Vec::new());
                }
            }
        },
        Err(std::sync::mpsc::SendError(job)) => job,
    };
    forget(name);
    Err(job.argv)
}

/// Every embedded utility's name, in table order.
///
/// For the tests that have to hold for *all* of them — each one answers
/// `--help`, each one is listed once — rather than for a sample that a new
/// entry silently is not in.
pub fn names() -> impl Iterator<Item = &'static str> {
    NATIVE
        .iter()
        .map(|(name, _)| *name)
        .chain(ISOLATED.iter().map(|(name, _)| *name))
        .chain(UTILITIES.iter().map(|(name, _)| *name))
}

/// Whether `name` runs in a child of the host rather than in its process.
///
/// The host has to be able to spawn itself for one of these to run at all
/// (`set_host_executable`), which is why a test harness asks this before
/// calling [`run`].
pub fn is_isolated(name: &[u8]) -> bool {
    std::str::from_utf8(name).is_ok_and(|name| ISOLATED.iter().any(|(utility, _)| *utility == name))
}

/// Whether `name` is embedded.
pub fn contains(name: &[u8]) -> bool {
    std::str::from_utf8(name).is_ok_and(|name| {
        NATIVE.iter().any(|(utility, _)| *utility == name)
            || ISOLATED.iter().any(|(utility, _)| *utility == name)
            || UTILITIES.iter().any(|(utility, _)| *utility == name)
    })
}

/// The executable the isolated utilities are spawned as.
///
/// Set once by the shell at startup. Without it [`run`] refuses an isolated
/// utility rather than spawning whatever `current_exe` happens to be — in a
/// test harness or another host that would be a different program, and
/// `--__embedded` would mean nothing to it.
static HOST: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

/// Register the executable that knows [`ISOLATED_FLAG`].
///
/// Called by the shell before it runs anything; a second call is ignored.
pub fn set_host_executable(path: std::path::PathBuf) {
    let _ = HOST.set(path);
}

/// The registered executable, if there is one.
fn host_executable() -> Option<std::path::PathBuf> {
    HOST.get().cloned()
}

/// Run `argv` in a child of this process, for a body that must own one.
///
/// The child is this same executable with [`ISOLATED_FLAG`] in front of the
/// utility's name, so the utility runs as itself: its own `argv[0]` (on Unix,
/// where a process may set it), its own signal dispositions, its own exit. The
/// descriptors are inherited, so the redirects the shell applied are the ones
/// the utility writes to.
fn isolated(argv: &[Vec<u8>], body: Body, name: &str, environment: Option<&[Vec<u8>]>) -> i32 {
    // The body is the *child's* to call; keeping it in the table is what lets
    // the flag dispatch by name alone.
    let _ = body;
    let Some(exe) = host_executable() else {
        // Refusing beats spawning something arbitrary: the host has to be able
        // to run `--__embedded` for this to mean anything.
        eprintln!(
            "baash: {name}: no host executable registered \
             (baash_utils::set_host_executable); the isolated utilities need one"
        );
        return 127;
    };
    let mut command = std::process::Command::new(exe);
    command.arg(ISOLATED_FLAG).arg(name);
    command.args(
        argv.iter()
            .skip(1)
            .map(|arg| baash_sys::os::os_from_bytes(arg)),
    );
    // The environment is the shell's exported variables, not this process's:
    // `baash` keeps its `export`s in its own table and passes an explicit
    // environment to every external command, so a child that inherited
    // `std::env` would see the variables `baash` started with. `awk` reads
    // `ENVIRON` (bash-completion's `make` helper exports `prefix` for it), and
    // `LC_ALL=C grep`/`sed` are the same question.
    if let Some(environment) = environment {
        command.env_clear();
        for entry in environment {
            let Some(equals) = entry.iter().position(|byte| *byte == b'=') else {
                continue;
            };
            command.env(
                baash_sys::os::os_from_bytes(&entry[..equals]),
                baash_sys::os::os_from_bytes(&entry[equals + 1..]),
            );
        }
    }
    // `uucore` prints its own name from `argv[0]`, so give the child the
    // utility's name there. Windows has no such override; there a message says
    // `baash`, which is cosmetic and recorded in `docs/portability.md` §6.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.arg0(name);
    }
    match command.status() {
        Ok(status) => status_code(status),
        Err(error) => {
            eprintln!("baash: {name}: {error}");
            127
        }
    }
}

/// A child's exit status as the shell's status.
fn status_code(status: std::process::ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        // A utility killed by a signal reports 128 + the signal, which is what
        // `$?` shows for an external command too.
        if let Some(signal) = status.signal() {
            return 128 + signal;
        }
    }
    1
}

/// Run one isolated body in this process, for the child [`ISOLATED_FLAG`] spawns.
///
/// `argv` is the utility's own command line, its name first. `None` means the
/// name is not isolated — only reachable if the flag is typed by hand.
pub fn run_isolated(argv: &[Vec<u8>]) -> Option<i32> {
    let (name, _) = argv.split_first()?;
    let name = std::str::from_utf8(name).ok()?;
    let (_, body) = ISOLATED
        .iter()
        .find(|(utility, _)| *utility == name)
        .copied()?;
    Some(body(argv))
}

/// Install the message catalog a uutils body prints through.
///
/// The uutils binaries do this in their `main`, not in `uumain`: without it an
/// error message is its Fluent id (`clap-error-unexpected-argument`) instead of
/// English. `awk` has no catalog — awk-rs carries its own messages — which is
/// why this belongs to each uutils body rather than to [`run_isolated`].
fn install_catalog(name: &str) {
    let _ = uucore10::locale::setup_localization(name);
}

/// `grep`, in the child the shell spawned for it.
fn body_grep(argv: &[Vec<u8>]) -> i32 {
    install_catalog("grep");
    uu_grep::uumain(argv.iter().map(|arg| baash_sys::os::os_from_bytes(arg)))
}

/// `sed`, in the child the shell spawned for it.
fn body_sed(argv: &[Vec<u8>]) -> i32 {
    install_catalog("sed");
    uused::sed::uumain(argv.iter().map(|arg| baash_sys::os::os_from_bytes(arg)))
}

/// `find`, in the child the shell spawned for it.
fn body_find(argv: &[Vec<u8>]) -> i32 {
    install_catalog("find");
    let Some(args) = utf8_arguments("find", argv) else {
        return 1;
    };
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    findutils::find::find_main(&refs, &findutils::find::StandardDependencies::new())
}

/// `xargs`, in the child the shell spawned for it.
fn body_xargs(argv: &[Vec<u8>]) -> i32 {
    install_catalog("xargs");
    let Some(args) = utf8_arguments("xargs", argv) else {
        return 1;
    };
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    findutils::xargs::xargs_main(&refs)
}

/// `findutils` takes `&str`, so a non-UTF-8 argument is refused here.
///
/// This mirrors what `find`/`xargs` do as binaries, message included: the
/// shell's own paths are bytes everywhere else, and these two crates are the
/// one place the argument has to be text. A name that is not UTF-8 is a
/// refusal, never a lossy conversion that would name a different file.
fn utf8_arguments(name: &str, argv: &[Vec<u8>]) -> Option<Vec<String>> {
    let mut out = Vec::with_capacity(argv.len());
    for argument in argv {
        match std::str::from_utf8(argument) {
            Ok(text) => out.push(text.to_owned()),
            Err(_) => {
                eprintln!(
                    "{name}: invalid (non-UTF-8) argument: {}",
                    String::from_utf8_lossy(argument)
                );
                return None;
            }
        }
    }
    Some(out)
}

fn run_base64(args: Vec<OsString>) -> i32 {
    uu_base64::uumain(args.into_iter())
}

fn run_basename(args: Vec<OsString>) -> i32 {
    uu_basename::uumain(args.into_iter())
}

fn run_cat(args: Vec<OsString>) -> i32 {
    uu_cat::uumain(args.into_iter())
}

fn run_cksum(args: Vec<OsString>) -> i32 {
    uu_cksum::uumain(args.into_iter())
}

fn run_comm(args: Vec<OsString>) -> i32 {
    uu_comm::uumain(args.into_iter())
}

fn run_cp(args: Vec<OsString>) -> i32 {
    uu_cp::uumain(args.into_iter())
}

fn run_cut(args: Vec<OsString>) -> i32 {
    uu_cut::uumain(args.into_iter())
}

fn run_date(args: Vec<OsString>) -> i32 {
    uu_date::uumain(args.into_iter())
}

fn run_dirname(args: Vec<OsString>) -> i32 {
    uu_dirname::uumain(args.into_iter())
}

fn run_du(args: Vec<OsString>) -> i32 {
    uu_du::uumain(args.into_iter())
}

fn run_expr(args: Vec<OsString>) -> i32 {
    uu_expr::uumain(args.into_iter())
}

fn run_head(args: Vec<OsString>) -> i32 {
    uu_head::uumain(args.into_iter())
}

fn run_ln(args: Vec<OsString>) -> i32 {
    uu_ln::uumain(args.into_iter())
}

fn run_ls(args: Vec<OsString>) -> i32 {
    uu_ls::uumain(args.into_iter())
}

fn run_md5sum(args: Vec<OsString>) -> i32 {
    uu_md5sum::uumain(args.into_iter())
}

fn run_mkdir(args: Vec<OsString>) -> i32 {
    uu_mkdir::uumain(args.into_iter())
}

fn run_mktemp(args: Vec<OsString>) -> i32 {
    uu_mktemp::uumain(args.into_iter())
}

fn run_mv(args: Vec<OsString>) -> i32 {
    uu_mv::uumain(args.into_iter())
}

fn run_nl(args: Vec<OsString>) -> i32 {
    uu_nl::uumain(args.into_iter())
}

fn run_nproc(args: Vec<OsString>) -> i32 {
    uu_nproc::uumain(args.into_iter())
}

fn run_od(args: Vec<OsString>) -> i32 {
    uu_od::uumain(args.into_iter())
}

fn run_paste(args: Vec<OsString>) -> i32 {
    uu_paste::uumain(args.into_iter())
}

fn run_readlink(args: Vec<OsString>) -> i32 {
    uu_readlink::uumain(args.into_iter())
}

fn run_realpath(args: Vec<OsString>) -> i32 {
    uu_realpath::uumain(args.into_iter())
}

fn run_rm(args: Vec<OsString>) -> i32 {
    uu_rm::uumain(args.into_iter())
}

fn run_rmdir(args: Vec<OsString>) -> i32 {
    uu_rmdir::uumain(args.into_iter())
}

fn run_seq(args: Vec<OsString>) -> i32 {
    uu_seq::uumain(args.into_iter())
}

fn run_sha1sum(args: Vec<OsString>) -> i32 {
    uu_sha1sum::uumain(args.into_iter())
}

fn run_sha256sum(args: Vec<OsString>) -> i32 {
    uu_sha256sum::uumain(args.into_iter())
}

fn run_shuf(args: Vec<OsString>) -> i32 {
    uu_shuf::uumain(args.into_iter())
}

fn run_sleep(args: Vec<OsString>) -> i32 {
    uu_sleep::uumain(args.into_iter())
}

fn run_sort(args: Vec<OsString>) -> i32 {
    uu_sort::uumain(args.into_iter())
}

fn run_split(args: Vec<OsString>) -> i32 {
    uu_split::uumain(args.into_iter())
}

fn run_tac(args: Vec<OsString>) -> i32 {
    uu_tac::uumain(args.into_iter())
}

fn run_tail(args: Vec<OsString>) -> i32 {
    uu_tail::uumain(args.into_iter())
}

fn run_tee(args: Vec<OsString>) -> i32 {
    uu_tee::uumain(args.into_iter())
}

fn run_touch(args: Vec<OsString>) -> i32 {
    uu_touch::uumain(args.into_iter())
}

fn run_tr(args: Vec<OsString>) -> i32 {
    uu_tr::uumain(args.into_iter())
}

fn run_truncate(args: Vec<OsString>) -> i32 {
    uu_truncate::uumain(args.into_iter())
}

fn run_uniq(args: Vec<OsString>) -> i32 {
    uu_uniq::uumain(args.into_iter())
}

fn run_wc(args: Vec<OsString>) -> i32 {
    uu_wc::uumain(args.into_iter())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(items: &[&str]) -> Vec<Vec<u8>> {
        items.iter().map(|item| item.as_bytes().to_vec()).collect()
    }

    #[test]
    fn an_unknown_name_is_not_embedded() {
        // `tar` is on the "not available anywhere" list (docs/portability.md
        // §6): the embedded set is chosen, not "whatever a crate has".
        assert_eq!(run(&argv(&["tar", "x"])), None);
        assert!(!contains(b"tar"));
        assert!(contains(b"cat"));
        // `awk`/`grep` are embedded, but in a child: a harness with no host to
        // spawn gets a refusal rather than a wrong process (`is_isolated`).
        assert!(contains(b"awk"));
        assert!(is_isolated(b"awk"));
        assert!(contains(b"grep"));
        assert!(is_isolated(b"grep"));
        assert!(!is_isolated(b"cat"));
    }

    #[test]
    fn the_shell_owns_printf_and_test() {
        // Both are native built-ins now, so the embedded fallback must not
        // shadow them (and its uutils copies are not linked).
        assert!(!contains(b"printf"));
        assert!(!contains(b"test"));
        assert!(!contains(b"["));
        assert!(contains(b"cat"));
    }
}
