//! Embedded coreutils.
//!
//! `printf`, `test`/`[` and the rest of the standard utilities are regular
//! builtins that deliberately fall through to `PATH` (`DESIGN.md` §2), so an
//! external binary always wins. That works on Unix and is useless on Windows,
//! where a `PATH` has no coreutils at all. This crate is the fallback: when
//! resolution finds nothing, the executor runs the uutils implementation in
//! this process instead of reporting "command not found".
//!
//! Only the utilities that are actually embedded are linked: each `uu_*` crate
//! is a library with one entry point, and the table below is the whole
//! surface. The set is chosen, not accreted: `printf`, `test`/`[`, `cat`,
//! `mkdir`, `rm`, `ls`, `wc`, `head`, `tail`, `tr`, `seq`, `sleep`.
//!
//! `true`, `false`, `echo` and `pwd` stay the VM's own bodies — they are
//! cheaper in-process and bash-shaped, and a script must not notice the
//! difference between them and a utility. `grep`, `sed`, `awk` and the rest of
//! the non-coreutils toolbox are deliberately absent.
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

/// One embedded utility: its name and the uutils entry point.
type Entry = fn(Vec<OsString>) -> i32;

/// The embedded utilities, by the name a command is typed as.
///
/// The first argument passed to [`run`] is the name itself, because that is
/// what `argv[0]` would be and what clap parses.
const UTILITIES: &[(&str, Entry)] = &[
    ("printf", run_printf),
    ("test", run_test),
    ("[", run_bracket),
    ("cat", run_cat),
    ("mkdir", run_mkdir),
    ("rm", run_rm),
    ("ls", run_ls),
    ("wc", run_wc),
    ("head", run_head),
    ("tail", run_tail),
    ("tr", run_tr),
    ("seq", run_seq),
    ("sleep", run_sleep),
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
        .name(format!("bsh-{name}"))
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
    let (name, entry) = *UTILITIES.iter().find(|(utility, _)| *utility == typed)?;
    let args: Vec<OsString> = argv
        .iter()
        .map(|arg| bsh_sys::os::os_from_bytes(arg))
        .collect();
    match attempt(name, entry, args) {
        Ok(status) => Some(status),
        // The worker died with the job (it panics outside `catch_unwind` only
        // if the shell's own dispatch is broken); try once on a fresh thread.
        Err(args) if !args.is_empty() => match attempt(name, entry, args) {
            Ok(status) => Some(status),
            Err(_) => {
                eprintln!("bsh: {name}: embedded utility failed");
                Some(1)
            }
        },
        Err(_) => {
            eprintln!("bsh: {name}: embedded utility failed");
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

/// Whether `name` is embedded.
pub fn contains(name: &[u8]) -> bool {
    std::str::from_utf8(name)
        .ok()
        .is_some_and(|name| UTILITIES.iter().any(|(utility, _)| *utility == name))
}

fn run_printf(args: Vec<OsString>) -> i32 {
    uu_printf::uumain(args.into_iter())
}

fn run_test(args: Vec<OsString>) -> i32 {
    uu_test::uumain(args.into_iter())
}

fn run_cat(args: Vec<OsString>) -> i32 {
    uu_cat::uumain(args.into_iter())
}

fn run_mkdir(args: Vec<OsString>) -> i32 {
    uu_mkdir::uumain(args.into_iter())
}

fn run_rm(args: Vec<OsString>) -> i32 {
    uu_rm::uumain(args.into_iter())
}

fn run_ls(args: Vec<OsString>) -> i32 {
    uu_ls::uumain(args.into_iter())
}

fn run_wc(args: Vec<OsString>) -> i32 {
    uu_wc::uumain(args.into_iter())
}

fn run_head(args: Vec<OsString>) -> i32 {
    uu_head::uumain(args.into_iter())
}

fn run_tail(args: Vec<OsString>) -> i32 {
    uu_tail::uumain(args.into_iter())
}

fn run_tr(args: Vec<OsString>) -> i32 {
    uu_tr::uumain(args.into_iter())
}

fn run_seq(args: Vec<OsString>) -> i32 {
    uu_seq::uumain(args.into_iter())
}

fn run_sleep(args: Vec<OsString>) -> i32 {
    uu_sleep::uumain(args.into_iter())
}

/// `[ … ]` is `test …` with a closing bracket.
///
/// `uu_test` decides bracket mode from the *process* name
/// (`uucore::util_name()`), which is `bsh` here, so an embedded call would see
/// the `]` as an operand and report "extra argument". The bracket is therefore
/// removed here and the utility is entered as `test`, which is the definition.
fn run_bracket(mut args: Vec<OsString>) -> i32 {
    if args.last().is_some_and(|last| last == "]") {
        args.pop();
    } else if args.len() > 1 {
        eprintln!("bsh: [: missing `]'");
        return 2;
    }
    if let Some(first) = args.first_mut() {
        *first = OsString::from("test");
    }
    uu_test::uumain(args.into_iter())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(items: &[&str]) -> Vec<Vec<u8>> {
        items.iter().map(|item| item.as_bytes().to_vec()).collect()
    }

    #[test]
    fn test_is_embedded_and_returns_its_status() {
        assert_eq!(run(&argv(&["test", "1", "=", "1"])), Some(0));
        assert_eq!(run(&argv(&["test", "1", "=", "2"])), Some(1));
    }

    #[test]
    fn bracket_is_test_with_the_closing_bracket_removed() {
        assert_eq!(run(&argv(&["[", "1", "=", "1", "]"])), Some(0));
        assert_eq!(run(&argv(&["[", "1", "=", "2", "]"])), Some(1));
        assert_eq!(run(&argv(&["[", "1", "=", "1"])), Some(2));
    }

    #[test]
    fn an_unknown_name_is_not_embedded() {
        assert_eq!(run(&argv(&["grep", "x"])), None);
        assert!(!contains(b"grep"));
        assert!(contains(b"cat"));
    }

    #[test]
    fn printf_is_embedded() {
        assert!(contains(b"printf"));
        assert_eq!(run(&argv(&["printf", "%s", "x"])), Some(0));
    }
}
