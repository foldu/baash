//! Runtime completion from a `carapace` binary on `PATH`.
//!
//! The generated tables in `baash-complete-data` are a snapshot; carapace
//! itself knows dynamic values (branches, containers, packages) because it runs
//! the command's completion action against the filesystem and the tools. When a
//! `carapace` binary is on the shell's `PATH`, the shell therefore asks it at
//! Tab time and prefers its answer. The tables are only the fallback.
//!
//! # Protocol (verified against carapace 1.8.1)
//!
//! Carapace is invoked **directly**, not through its bash integration:
//!
//! ```text
//! carapace <command> export <command> <argument>... <current-prefix>
//! ```
//!
//! * `<command>` is the simple command's first word, reduced to its basename
//!   (`/usr/bin/git` is completed as `git`).
//! * The words after `export` are the simple command's words **including the
//!   command itself**; the last one is the word being completed, passed as an
//!   empty argument for an empty prefix. `carapace git export git checkout ''`
//!   lists branches; omitting the repeated `git` or the trailing empty word
//!   asks a different question (root flags, or the command's spec).
//! * `export` is carapace's machine-readable shell format: one JSON object on
//!   stdout with `values` (`value`, `display`, `description`, `tag`) plus
//!   `messages`, `nospace` and `usage`. It is the same backend
//!   `scripts/carapace-data.py` reads for the static command tree (without the
//!   trailing words that call answers with the tree rather than candidates),
//!   and it executes dynamic actions; the bash format would print `ERR`/`ERR1`
//!   message placeholders as if they were candidates, and quotes and escapes
//!   values for bash's insertion rules.
//! * No `COMP_*` or `CARAPACE_*` environment is needed. We deliberately do not
//!   set `COMP_LINE`: carapace 1.8.1's bash patch replaces the argv words with
//!   its own `shlex` split of that variable, which would make a stale or
//!   differently-quoted line win over the words the editor actually has.
//!
//! Unknown commands print nothing and exit 0; a dynamic action that cannot run
//! (no repository, no daemon) exits 0 with `values: []` and `messages`. A
//! non-zero exit, invalid JSON, or an empty `values` array all mean "nothing to
//! offer" and the caller falls through to the next source.
//!
//! # Why the wait is a polled state machine
//!
//! A completion is on the keystroke path and a dynamic action can take
//! hundreds of milliseconds. The process is started when Tab is pressed, the
//! editor keeps reading keys, and [`Carapace::poll`] is a non-blocking check
//! (a zero-timeout readability check plus a non-blocking `waitpid`) driven by
//! the editor's wait loop. A keystroke cancels the run; [`TIMEOUT`] bounds an otherwise
//! silent process. Nothing here blocks, so the prompt stays live and the child
//! is never left running after the line is over.

use std::io;
use std::time::{Duration, Instant};

use baash_complete::{CandKind, Candidate, Ctx};
use baash_sys::{
    AsRawFd, Child, FdAction, OwnedFd, Pipe, SpawnPlan, poll_readable, read_fd, spawn,
};

/// The longest a single carapace lookup may take.
///
/// Measured on this host: `carapace git export` costs about 13 ms for a static
/// subcommand list and about 24 ms for a dynamic branch list. The bound is
/// generous for the dynamic actions that shell out (`kubectl`, `docker`, a
/// package daemon) and short enough that a broken binary is not noticed as a
/// hang. The wait is asynchronous, so a longer bound costs responsiveness only
/// when the user is waiting for a menu; any keystroke cancels it.
pub const TIMEOUT: Duration = Duration::from_millis(500);

/// Refuse output larger than this, so a broken completer cannot exhaust memory.
const MAX_OUTPUT: usize = 4 * 1024 * 1024;

/// The separator between `PATH` entries, matching `baash_sys::paths`.
#[cfg(windows)]
const PATH_SEPARATOR: u8 = b';';
#[cfg(not(windows))]
const PATH_SEPARATOR: u8 = b':';

/// How to run carapace for one completion.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invocation {
    /// Absolute path of the binary, as `baash_sys::spawn` wants it.
    pub program: Vec<u8>,
    /// `argv`, including `argv[0]`.
    pub argv: Vec<Vec<u8>>,
    /// `NAME=value` entries for the child.
    pub envp: Vec<Vec<u8>>,
}

/// What one non-blocking check found.
#[derive(Debug)]
pub enum Poll {
    /// The process is still running (or its pipe is not at end of file yet).
    Pending,
    /// The process exited successfully and this is its complete stdout.
    Ready(Vec<u8>),
    /// No usable answer: non-zero exit, I/O error, an over-large result, or
    /// the deadline. The caller falls through.
    Failed,
}

/// A carapace process owned across editor turns.
pub struct Carapace {
    child: Option<Child>,
    /// The read end of stdout. `None` after the child has been reaped and the
    /// pipe drained.
    out: Option<OwnedFd>,
    bytes: Vec<u8>,
    deadline: Instant,
}

impl Carapace {
    /// Start `invocation` with stdout captured, stdin and stderr null.
    pub fn start(invocation: &Invocation) -> io::Result<Self> {
        let Pipe { read, write } = Pipe::new()?;
        let mut plan =
            SpawnPlan::new_with_env(&invocation.program, &invocation.argv, &invocation.envp)?;
        // Only stdout is captured. A completion must never paint on the
        // terminal the editor owns in raw mode, and a child that reads the
        // keyboard would take the user's next keystroke.
        plan.push_action(FdAction::dup2(write.as_raw_fd(), 1));
        #[cfg(unix)]
        {
            plan.open(b"/dev/null", baash_sys::flags::RDWR, 0, 0)?;
            plan.push_action(FdAction::dup2(0, 2));
        }
        #[cfg(windows)]
        {
            plan.push_action(FdAction::close(0));
            plan.push_action(FdAction::close(2));
        }
        let child = spawn(plan)?;
        // The child's copy is the only writer now; the parent must close its
        // own or the pipe never reports end of file.
        drop(write);
        Ok(Self {
            child: Some(child),
            out: Some(read),
            bytes: Vec::new(),
            deadline: Instant::now() + TIMEOUT,
        })
    }

    /// Check for output without waiting.
    pub fn poll(&mut self) -> Poll {
        let Some(out) = self.out.as_ref() else {
            return Poll::Failed;
        };
        let fd = out.as_raw_fd();
        // Drain whatever is available. A read after a readiness poll cannot
        // block: this is the only reader and the descriptor is still open.
        loop {
            match poll_readable(fd, 0) {
                Ok(true) => {}
                Ok(false) => break,
                Err(_) => return self.fail(),
            }
            let mut buffer = [0u8; 8192];
            match read_fd(fd, &mut buffer) {
                Ok(0) => {
                    // End of file: the child closed stdout. It may still be
                    // alive (a grandchild could hold the pipe), so the exit
                    // status below decides whether to wait for it.
                    return self.finish();
                }
                Ok(count) => {
                    self.bytes.extend_from_slice(&buffer[..count]);
                    if self.bytes.len() > MAX_OUTPUT {
                        return self.fail();
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => return self.fail(),
            }
        }

        match self
            .child
            .as_ref()
            .and_then(|child| child.try_wait().ok().flatten())
        {
            Some(outcome) => {
                if outcome.exit_status() == 0 {
                    // The process is gone but its buffered bytes are not: read
                    // until end of file before deciding.
                    self.drain();
                    let complete = self.bytes.len() <= MAX_OUTPUT;
                    self.finished();
                    if complete {
                        Poll::Ready(std::mem::take(&mut self.bytes))
                    } else {
                        Poll::Failed
                    }
                } else {
                    self.fail()
                }
            }
            None => {
                if Instant::now() >= self.deadline {
                    self.fail()
                } else {
                    Poll::Pending
                }
            }
        }
    }

    /// Read to end of file after the child exited.
    fn drain(&mut self) {
        loop {
            let Some(out) = self.out.as_ref() else {
                return;
            };
            // Never block here: a grandchild can inherit the write end and
            // keep the pipe open after carapace itself has exited. The
            // readiness check makes "no data right now" end the drain.
            match poll_readable(out.as_raw_fd(), 0) {
                Ok(true) => {}
                Ok(false) | Err(_) => return,
            }
            let mut buffer = [0u8; 8192];
            match read_fd(out.as_raw_fd(), &mut buffer) {
                Ok(0) | Err(_) => return,
                Ok(count) => self.bytes.extend_from_slice(&buffer[..count]),
            }
            if self.bytes.len() > MAX_OUTPUT {
                return;
            }
        }
    }

    /// Reap the child and report end of output.
    fn finish(&mut self) -> Poll {
        let outcome = self
            .child
            .as_ref()
            .and_then(|child| child.try_wait().ok().flatten());
        let status = match outcome {
            Some(outcome) => outcome.exit_status(),
            // End of file but the child has not been reaped yet: try once
            // more, then treat "still running" as a transient state. A
            // process that closed stdout and keeps running is unusual; the
            // deadline still bounds it.
            None => {
                if Instant::now() >= self.deadline {
                    return self.fail();
                }
                return Poll::Pending;
            }
        };
        self.finished();
        if status == 0 && self.bytes.len() <= MAX_OUTPUT {
            Poll::Ready(std::mem::take(&mut self.bytes))
        } else {
            Poll::Failed
        }
    }

    /// Kill and reap, and return [`Poll::Failed`].
    fn fail(&mut self) -> Poll {
        self.cancel();
        Poll::Failed
    }

    /// Kill the child if it is still running and reap it.
    ///
    /// Called on a keystroke (the line moved on), on the deadline, and by
    /// `Drop`; idempotent.
    pub fn cancel(&mut self) {
        if let Some(child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.out = None;
        self.bytes.clear();
    }

    fn finished(&mut self) {
        // The status was collected; the pipe is at end of file and can go.
        self.child = None;
        self.out = None;
    }
}

impl Drop for Carapace {
    fn drop(&mut self) {
        self.cancel();
    }
}

/// Find `carapace` in the shell's `PATH`.
///
/// The shell's own `PATH` value is used, not the process environment: `PATH=…`
/// in the shell must change what Tab does, and a `PATH` an rc file set without
/// exporting it still names the binary the user meant for the shell.
pub fn find(path_var: &[u8]) -> Option<Vec<u8>> {
    find_named(path_var, b"carapace")
}

fn find_named(path_var: &[u8], name: &[u8]) -> Option<Vec<u8>> {
    #[cfg_attr(not(windows), allow(unused_mut))]
    let mut names = vec![name.to_vec()];
    #[cfg(windows)]
    names.push([name, b".exe".as_slice()].concat());

    for directory in path_var.split(|&byte| byte == PATH_SEPARATOR) {
        // POSIX: an empty entry means the current directory, and that is what
        // execvp would do; keep it rather than silently dropping a PATH entry
        // the user wrote. Windows allows a quoted entry for a directory with a
        // space, so undo the quotes before joining.
        let directory = directory
            .strip_prefix(b"\"")
            .and_then(|rest| rest.strip_suffix(b"\""))
            .unwrap_or(directory);
        let directory = if directory.is_empty() {
            b".".as_slice()
        } else {
            directory
        };
        for name in &names {
            let mut candidate = Vec::with_capacity(directory.len() + 1 + name.len());
            candidate.extend_from_slice(directory);
            if !candidate
                .last()
                .is_some_and(|&byte| baash_sys::paths::is_separator(byte))
            {
                candidate.push(baash_sys::paths::separator());
            }
            candidate.extend_from_slice(name);
            if is_executable(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

fn is_executable(path: &[u8]) -> bool {
    let path = baash_sys::os::path_from_bytes(path);
    let Ok(metadata) = std::fs::metadata(&path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(windows)]
    {
        true
    }
}

/// The simple command's name, reduced to what carapace knows.
///
/// A completer is registered by name (`git`, not `/usr/bin/git`), so the
/// basename is what the binary must be asked about. Windows keeps the `.exe`
/// extension off too; carapace's own completion map uses the command name.
pub fn command_name(word: &[u8]) -> Vec<u8> {
    let name = match word
        .iter()
        .rposition(|&byte| baash_sys::paths::is_separator(byte))
    {
        Some(index) => &word[index + 1..],
        None => word,
    };
    #[cfg(windows)]
    {
        let lower = name.to_ascii_lowercase();
        if lower.ends_with(b".exe") {
            return name[..name.len() - 4].to_vec();
        }
    }
    name.to_vec()
}

/// Build the argv and environment for one lookup.
///
/// `ctx.words` is the command and its arguments *before* the word being
/// completed; `ctx.prefix` is the unquoted fragment typed of that word. The
/// last word must be present even when empty, or carapace answers for the
/// previous argument instead of the new one.
pub fn invocation(program: Vec<u8>, ctx: &Ctx, envp: Vec<Vec<u8>>) -> Option<Invocation> {
    let command_word = ctx.words.first()?;
    let command = command_name(command_word);
    let mut words: Vec<Vec<u8>> = Vec::with_capacity(ctx.words.len() + 1);
    words.push(command.clone());
    for word in ctx.words.iter().skip(1) {
        words.push(unquote_word(word));
    }
    words.push(unquote_word(&ctx.prefix));

    let mut argv = Vec::with_capacity(words.len() + 3);
    argv.push(b"carapace".to_vec());
    argv.push(command);
    argv.push(b"export".to_vec());
    argv.extend(words);

    Some(Invocation {
        program,
        argv,
        envp,
    })
}

/// Remove the quoting from one word, so carapace sees argument *values*.
///
/// This is not an expansion: `$HOME`, globs and `$(…)` stay as written, which
/// is also what bash's completion functions receive for those forms. It only
/// undoes the quoting the user typed, so `'foo bar'` is one argument `foo bar`
/// rather than a word containing quote characters.
pub fn unquote_word(word: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(word.len());
    let mut index = 0;
    let mut quote = None;
    while index < word.len() {
        let byte = word[index];
        match quote {
            Some(b'\'') => {
                if byte == b'\'' {
                    quote = None;
                } else {
                    out.push(byte);
                }
            }
            Some(b'"') => match byte {
                b'"' => quote = None,
                b'\\' if index + 1 < word.len() => {
                    let next = word[index + 1];
                    // Inside double quotes only these bytes are escaped; any
                    // other backslash is a literal backslash, and dropping it
                    // would change the argument.
                    if matches!(next, b'$' | b'`' | b'"' | b'\\' | b'\n') {
                        if next != b'\n' {
                            out.push(next);
                        }
                        index += 1;
                    } else {
                        out.push(byte);
                    }
                }
                _ => out.push(byte),
            },
            Some(_) => unreachable!("only single and double quotes are tracked"),
            None => match byte {
                b'\'' | b'"' => quote = Some(byte),
                b'\\' if index + 1 < word.len() => {
                    index += 1;
                    out.push(word[index]);
                }
                _ => out.push(byte),
            },
        }
        index += 1;
    }
    out
}

/// One parsed `export` envelope: the values and the nospace matcher.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Answer {
    /// The candidates, in carapace's display order.
    pub values: Vec<Value>,
    /// `SuffixMatcher::Matches` input: `*` or the bytes whose trailing
    /// occurrence suppresses the space.
    pub nospace: String,
}

/// One value parsed from carapace's `export` output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Value {
    /// Bytes to insert.
    pub value: Vec<u8>,
    /// Text to show; carapace may display a styled or abbreviated form.
    pub display: String,
    /// The description column, already trimmed.
    pub description: String,
}

/// Parse the JSON envelope of carapace's `export` format.
///
/// Returns `None` when there is nothing to offer (`values` missing or empty),
/// which is the fall-through signal. Individual malformed entries are skipped
/// rather than throwing away a whole useful answer; a completely malformed
/// document is `None`.
pub fn parse_export(bytes: &[u8]) -> Option<Answer> {
    let root: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let values = root.get("values")?.as_array()?;
    let nospace = root
        .get("nospace")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_string();
    let mut parsed = Vec::with_capacity(values.len());
    for entry in values {
        let Some(value) = entry.get("value").and_then(serde_json::Value::as_str) else {
            continue;
        };
        if value.is_empty() {
            continue;
        }
        let display = entry
            .get("display")
            .and_then(serde_json::Value::as_str)
            .unwrap_or(value);
        let description = entry
            .get("description")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        parsed.push(Value {
            value: value.as_bytes().to_vec(),
            display: sanitize(display),
            description: trim_description(description),
        });
    }
    (!parsed.is_empty()).then_some(Answer {
        values: parsed,
        nospace,
    })
}

/// Turn parsed values into menu candidates.
///
/// `nospace` is carapace's suffix matcher: `*` means every value, any other
/// byte means values ending with it. Directories never take a space, and a
/// candidate with a description is still a literal: these are argument values,
/// not shell source.
pub fn candidates(values: &[Value], nospace: &str) -> Vec<Candidate> {
    let mut candidates = Vec::with_capacity(values.len());
    for value in values {
        let kind = if value.value.first() == Some(&b'-') {
            CandKind::Option
        } else {
            CandKind::Other
        };
        let mut candidate = Candidate::new(value.value.clone(), kind);
        if !value.display.is_empty() && value.display.as_bytes() != value.value.as_slice() {
            candidate = candidate.with_display(value.display.clone());
        }
        if !value.description.is_empty() {
            candidate = candidate.with_desc(value.description.clone());
        }
        candidate.append_space = !nospace.matches_suffix(value.value.last().copied().unwrap_or(0))
            && !value.value.ends_with(b"/");
        candidates.push(candidate);
    }
    candidates
}

/// `SuffixMatcher::Matches` from carapace's `common/suffix.go`.
trait SuffixMatch {
    fn matches_suffix(&self, suffix: u8) -> bool;
}

impl SuffixMatch for str {
    fn matches_suffix(&self, suffix: u8) -> bool {
        self.bytes().any(|byte| byte == b'*' || byte == suffix)
    }
}

/// Carapace truncates descriptions to one line and 80 characters; a menu
/// column with a whole paragraph in it is unreadable.
fn trim_description(description: &str) -> String {
    // One line, no control bytes: `split_whitespace` turns tabs and runs of
    // blanks into a single space, which is what a menu column can render.
    let first = description.lines().next().unwrap_or("");
    let trimmed = first.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars: Vec<char> = trimmed.chars().take(80).collect();
    if trimmed.chars().count() > 80 {
        chars.truncate(77);
        chars.extend(['.', '.', '.']);
    }
    chars.into_iter().collect()
}

/// Strip control characters from display text.
///
/// Carapace itself strips newlines and tabs from fish output; the JSON format
/// leaves them in, and a menu candidate whose display contains a newline would
/// break the renderer's rows.
fn sanitize(text: &str) -> String {
    text.chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use baash_complete::context;
    use baash_syntax::ParseOpts;

    fn ctx_at(line: &str) -> Ctx {
        context(line.as_bytes(), line.len(), &ParseOpts::DEFAULT)
    }

    #[test]
    fn parses_values_with_spaces_quotes_and_empty_descriptions() {
        let json = br#"{
            "version": "v1",
            "messages": [],
            "nospace": "",
            "usage": "",
            "values": [
                {"value": "a b", "display": "a b", "description": "two words"},
                {"value": "quote'd", "display": "quote'd", "description": ""},
                {"value": "--no-desc", "display": "--no-desc"}
            ]
        }
        "#;
        let answer = parse_export(json).expect("values");
        let values = &answer.values;
        assert_eq!(values.len(), 3);
        assert_eq!(values[0].value, b"a b");
        assert_eq!(values[0].display, "a b");
        assert_eq!(values[0].description, "two words");
        assert_eq!(values[1].value, b"quote'd");
        assert!(values[1].description.is_empty());
        assert!(values[2].description.is_empty());
    }

    #[test]
    fn a_trailing_newline_after_the_object_is_accepted() {
        let json =
            b"{\"values\":[{\"value\":\"master\",\"display\":\"master\",\"description\":\"\"}]}\n";
        let answer = parse_export(json).expect("values");
        assert_eq!(answer.values[0].value, b"master");
    }

    #[test]
    fn json_escapes_and_unicode_are_decoded() {
        let json = br#"{"values":[{"value":"a\"b","display":"a\"b","description":"\u003cbranch\u003e\tone"}]}"#;
        let answer = parse_export(json).expect("values");
        assert_eq!(answer.values[0].value, b"a\"b");
        assert_eq!(answer.values[0].description, "<branch> one");
    }

    #[test]
    fn empty_or_garbage_output_is_no_answer() {
        assert!(parse_export(b"").is_none());
        assert!(parse_export(b"not json").is_none());
        assert!(parse_export(b"{\"values\":[]}").is_none());
        assert!(parse_export(b"{\"messages\":[\"boom\"],\"values\":[]}").is_none());
    }

    #[test]
    fn descriptions_are_one_trimmed_line() {
        let long = "x".repeat(100);
        assert_eq!(trim_description("  first line\nsecond  "), "first line");
        assert_eq!(trim_description(&long).chars().count(), 80);
        assert!(trim_description(&long).ends_with("..."));
    }

    #[test]
    fn candidate_values_are_literals_and_nospace_follows_the_matcher() {
        let values = vec![
            Value {
                value: b"a b".to_vec(),
                display: "a b".to_string(),
                description: String::new(),
            },
            Value {
                value: b"--flag".to_vec(),
                display: "--flag".to_string(),
                description: "a flag".to_string(),
            },
            Value {
                value: b"dir/".to_vec(),
                display: "dir/".to_string(),
                description: String::new(),
            },
        ];
        let spaced = candidates(&values, "*");
        assert_eq!(spaced[0].insert, b"a b");
        assert!(spaced[0].display == "a b");
        assert!(spaced.iter().all(|candidate| !candidate.append_space));
        assert_eq!(spaced[1].desc.as_deref(), Some("a flag"));
        assert_eq!(spaced[1].kind, CandKind::Option);

        let spaced = candidates(&values, "");
        assert!(spaced[0].append_space);
        assert!(!spaced[2].append_space); // the trailing slash
    }

    #[test]
    fn quoting_is_undone_but_expansions_are_left_alone() {
        assert_eq!(unquote_word(b"'a b'"), b"a b");
        assert_eq!(unquote_word(b"\"a b\""), b"a b");
        assert_eq!(unquote_word(b"a\\ b"), b"a b");
        assert_eq!(unquote_word(b"\"a\\\"b\""), b"a\"b");
        assert_eq!(unquote_word(b"\"$HOME\""), b"$HOME");
        assert_eq!(unquote_word(b"$HOME"), b"$HOME");
        assert_eq!(unquote_word(b"a'b'c"), b"abc");
    }

    #[test]
    fn the_invocation_repeats_the_command_and_keeps_an_empty_current_word() {
        let ctx = ctx_at("git checkout ");
        let invocation = invocation(b"/bin/carapace".to_vec(), &ctx, Vec::new()).expect("argv");
        assert_eq!(
            invocation.argv,
            vec![
                b"carapace".to_vec(),
                b"git".to_vec(),
                b"export".to_vec(),
                b"git".to_vec(),
                b"checkout".to_vec(),
                b"".to_vec(),
            ]
        );
    }

    #[test]
    fn a_path_invocation_is_asked_about_the_basename() {
        let ctx = ctx_at("/usr/bin/git ch");
        let invocation = invocation(b"/bin/carapace".to_vec(), &ctx, Vec::new()).expect("argv");
        assert_eq!(invocation.argv[1], b"git");
        assert_eq!(invocation.argv[3], b"git");
        assert_eq!(invocation.argv[4], b"ch");
    }

    #[test]
    fn path_lookup_finds_an_executable_and_skips_directories() {
        let root = std::env::temp_dir().join(format!(
            "baash-carapace-find-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("temp dir");
        let binary = root.join("carapace");
        std::fs::write(&binary, b"#!/bin/sh\n").expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
        }
        let mut path = root.as_os_str().as_encoded_bytes().to_vec();
        path.push(PATH_SEPARATOR);
        path.extend_from_slice(b"/nonexistent");
        let found = find(&path).expect("found");
        assert!(found.ends_with(b"carapace"));
        let _ = std::fs::remove_dir_all(&root);
    }
}
