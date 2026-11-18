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

mod clear;

/// The utilities written here rather than linked from uutils.
///
/// `clear` is the whole list: it belongs to ncurses, which uutils' coreutils
/// does not cover, and a Windows `PATH` has no `clear.exe` at all (`cls` is a
/// `cmd.exe` built-in). They run on the calling thread — there is no message
/// catalog to install, which is the only reason the uutils entries get a worker.
const NATIVE: &[(&str, Native)] = &[("clear", clear::main)];

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
/// built-in.
pub fn run(argv: &[Vec<u8>]) -> Option<i32> {
    let (typed, _) = argv.split_first()?;
    let typed = std::str::from_utf8(typed).ok()?;
    let args: Vec<OsString> = argv
        .iter()
        .map(|arg| baash_sys::os::os_from_bytes(arg))
        .collect();
    if let Some((_, main)) = NATIVE.iter().find(|(utility, _)| *utility == typed) {
        return Some(main(&args));
    }
    let (name, entry) = *UTILITIES.iter().find(|(utility, _)| *utility == typed)?;
    match attempt(name, entry, args) {
        Ok(status) => Some(status),
        // The worker died with the job (it panics outside `catch_unwind` only
        // if the shell's own dispatch is broken); try once on a fresh thread.
        Err(args) if !args.is_empty() => match attempt(name, entry, args) {
            Ok(status) => Some(status),
            Err(_) => {
                eprintln!("baash: {name}: embedded utility failed");
                Some(1)
            }
        },
        Err(_) => {
            eprintln!("baash: {name}: embedded utility failed");
            Some(1)
        }
    }
}

/// Hand one call to the worker, giving `argv` back when the worker died.
fn attempt(name: &'static str, entry: Entry, args: Vec<OsString>) -> Result<i32, Vec<OsString>> {
    let Some(jobs) = worker(name, entry) else {
        return Err(args);
    };
    let (reply, answer) = channel();
    let job = match jobs.send(Job { argv: args, reply }) {
        Ok(()) => {
            return match answer.recv() {
                Ok(status) => Ok(status),
                Err(_) => {
                    forget(name);
                    Err(Vec::new())
                }
            };
        }
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
        .chain(UTILITIES.iter().map(|(name, _)| *name))
}

/// Whether `name` is embedded.
pub fn contains(name: &[u8]) -> bool {
    std::str::from_utf8(name).is_ok_and(|name| {
        NATIVE.iter().any(|(utility, _)| *utility == name)
            || UTILITIES.iter().any(|(utility, _)| *utility == name)
    })
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
        assert_eq!(run(&argv(&["grep", "x"])), None);
        assert!(!contains(b"grep"));
        assert!(contains(b"cat"));
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
