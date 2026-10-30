//! `compgen`'s view of this shell: the candidate sources and the two seams a
//! built-in cannot implement itself.
//!
//! [`bsh_runtime::complete`] owns the vocabulary — which action means which
//! list — and asks for the lists through [`bsh_runtime::complete::CompletionSource`].
//! That trait is implemented here, where the state actually is: variables and
//! aliases in [`bsh_runtime::ShellState`], functions in this crate's function
//! table, `PATH` in the filesystem. Splitting it any other way would mean a
//! second copy of the shell's own name resolution.
//!
//! Two things cannot be answered by listing:
//!
//! * `-W` word lists are shell *text*. Splitting them on `IFS` needs to know
//!   which `IFS` bytes are quoted or inside an expansion, and expanding each
//!   word needs the real expansion pipeline, so both live here.
//! * `-F`/`-C` run shell code. `-F` runs a function in this shell (its
//!   `COMPREPLY` is the answer), `-C` runs a command in a child with its
//!   stdout captured, like a command substitution.
//!
//! # `-W` splitting
//!
//! bash splits the word list at `IFS` bytes that are neither quoted nor
//! escaped, then expands each word; the suite pins both halves
//! (`$()` producing fields that are then split, and `\:` surviving with
//! `IFS=':%'`). Splitting and expanding as one pass would get one of them
//! wrong, so they are two passes: [`split_wordlist`] scans the text and quotes
//! the literal bytes a lexer would otherwise treat as separators, and each
//! field is then parsed and expanded by `bsh-lang`.

use bsh_runtime::complete::{CompletionSource, WordList};
use bsh_runtime::{ShellHost, Status};

use super::{Ctx, Interp, Unwind};

impl CompletionSource for Interp {
    fn variables(&mut self, prefix: &[u8]) -> Vec<Vec<u8>> {
        self.visible_names(prefix, |_| true)
    }

    fn exports(&mut self, prefix: &[u8]) -> Vec<Vec<u8>> {
        self.visible_names(prefix, |var| var.attrs.exported)
    }

    fn array_variables(&mut self, prefix: &[u8]) -> Vec<Vec<u8>> {
        self.visible_names(prefix, |var| var.value.shape().is_some())
    }

    fn functions(&mut self, prefix: &[u8]) -> Vec<Vec<u8>> {
        let mut names: Vec<Vec<u8>> = self
            .functions
            .iter()
            .map(|function| function.name.clone())
            .filter(|name| name.starts_with(prefix))
            .collect();
        names.sort_unstable();
        names.dedup();
        names
    }

    fn aliases(&mut self, prefix: &[u8]) -> Vec<Vec<u8>> {
        self.state
            .aliases
            .iter()
            .map(|(name, _)| name.to_vec())
            .filter(|name| name.starts_with(prefix))
            .collect()
    }

    fn builtins(&mut self, prefix: &[u8]) -> Vec<Vec<u8>> {
        let mut names: Vec<Vec<u8>> = bsh_runtime::BUILTINS
            .iter()
            .map(|builtin| builtin.name.as_bytes().to_vec())
            .filter(|name| name.starts_with(prefix))
            .collect();
        names.sort_unstable();
        names.dedup();
        names
    }

    fn keywords(&mut self, prefix: &[u8]) -> Vec<Vec<u8>> {
        super::command::KEYWORDS
            .iter()
            .filter(|keyword| keyword.starts_with(prefix))
            .map(|keyword| keyword.to_vec())
            .collect()
    }

    fn setopts(&mut self, prefix: &[u8]) -> Vec<Vec<u8>> {
        self.state
            .opts
            .names(bsh_runtime::OptionKind::SetO)
            .filter(|name| name.as_bytes().starts_with(prefix))
            .map(|name| name.as_bytes().to_vec())
            .collect()
    }

    fn shopts(&mut self, prefix: &[u8]) -> Vec<Vec<u8>> {
        self.state
            .opts
            .names(bsh_runtime::OptionKind::Shopt)
            .filter(|name| name.as_bytes().starts_with(prefix))
            .map(|name| name.as_bytes().to_vec())
            .collect()
    }

    fn files(&mut self, prefix: &[u8], directories_only: bool) -> Vec<Vec<u8>> {
        files(prefix, directories_only)
    }

    fn commands(&mut self, prefix: &[u8]) -> Vec<Vec<u8>> {
        // bash's order: aliases, functions, built-ins, reserved words, then
        // `PATH`. `compgen -A command our_` prints the alias before the
        // functions, and a duplicate name is offered once.
        let mut out = self.aliases(prefix);
        out.extend(self.functions(prefix));
        out.extend(self.builtins(prefix));
        out.extend(self.keywords(prefix));
        out.extend(self.path_commands(prefix));
        // One line per name, at the position of its first source: a function
        // that shadows a built-in is one candidate, not two.
        let mut seen = std::collections::BTreeSet::new();
        out.retain(|name| seen.insert(name.clone()));
        out
    }

    fn users(&mut self, prefix: &[u8]) -> Vec<Vec<u8>> {
        // `/etc/passwd`'s first field. A missing or unreadable file simply
        // offers nothing: `compgen -A user` is not a way to diagnose NSS.
        column_names(b"/etc/passwd", 0, prefix)
    }

    fn groups(&mut self, prefix: &[u8]) -> Vec<Vec<u8>> {
        column_names(b"/etc/group", 0, prefix)
    }

    fn hosts(&mut self, prefix: &[u8]) -> Vec<Vec<u8>> {
        let mut names: Vec<Vec<u8>> = Vec::new();
        let Ok(bytes) = std::fs::read("/etc/hosts") else {
            return names;
        };
        for line in bytes.split(|&byte| byte == b'\n') {
            let line = line.split(|&byte| byte == b'#').next().unwrap_or_default();
            let fields: Vec<&[u8]> = line
                .split(|byte| byte.is_ascii_whitespace())
                .filter(|field| !field.is_empty())
                .collect();
            // Address, then one or more names.
            for name in fields.iter().skip(1) {
                if name.starts_with(prefix) {
                    names.push(name.to_vec());
                }
            }
        }
        names.sort_unstable();
        names.dedup();
        names
    }

    fn signals(&mut self, prefix: &[u8]) -> Vec<Vec<u8>> {
        bsh_runtime::SIGNALS
            .iter()
            .map(|(_, name)| name.as_bytes().to_vec())
            .filter(|name| name.starts_with(prefix))
            .collect()
    }

    fn help_topics(&mut self, prefix: &[u8]) -> Vec<Vec<u8>> {
        // Every documented topic is a built-in (this shell has no separate
        // manual index), so `help` and `compgen -A helptopic` agree.
        self.builtins(prefix)
    }

    fn bindings(&mut self, prefix: &[u8]) -> Vec<Vec<u8>> {
        // This shell has no line editor of its own yet, so there are no
        // `bind`-able functions. Offering readline's names would be a lie.
        let _ = prefix;
        Vec::new()
    }

    fn running_jobs(&mut self, prefix: &[u8]) -> Vec<Vec<u8>> {
        // There is no job table, so there are no `%N` specifications to offer.
        let _ = prefix;
        Vec::new()
    }

    fn stopped_jobs(&mut self, prefix: &[u8]) -> Vec<Vec<u8>> {
        // No job control: no stopped state exists to report.
        let _ = prefix;
        Vec::new()
    }

    fn services(&mut self, prefix: &[u8]) -> Vec<Vec<u8>> {
        // `systemctl`/`service` are external programs; asking them here would
        // fork on every TAB. Nothing is offered.
        let _ = prefix;
        Vec::new()
    }

    fn disabled_builtins(&mut self, prefix: &[u8]) -> Vec<Vec<u8>> {
        // `enable -n` is not implemented, so nothing is disabled.
        let _ = prefix;
        Vec::new()
    }

    fn enabled_builtins(&mut self, prefix: &[u8]) -> Vec<Vec<u8>> {
        self.builtins(prefix)
    }
}

impl Interp {
    /// Every shell function name, sorted and deduplicated.
    ///
    /// The interactive completion bridge in the binary projects this shell's
    /// state into `bsh-complete` snapshots, and the function table is the one
    /// completion-relevant piece of state it could not read: variables, aliases
    /// and the history ring are in [`bsh_runtime::ShellState`]. Listing names
    /// only keeps the snapshot independent of function bodies.
    pub fn function_names(&self) -> Vec<Vec<u8>> {
        let mut names: Vec<Vec<u8>> = self
            .functions
            .iter()
            .map(|function| function.name.clone())
            .collect();
        names.sort_unstable();
        names.dedup();
        names
    }

    /// Visible variable names, filtered by prefix and by a predicate on the
    /// binding that is actually in effect.
    fn visible_names(
        &self,
        prefix: &[u8],
        keep: impl Fn(&bsh_runtime::Var) -> bool,
    ) -> Vec<Vec<u8>> {
        // `Vars::visible` reports one definition per name (the innermost) in
        // name order, so a `local` hides the global it shadows instead of
        // being listed twice.
        self.state
            .vars
            .visible()
            .into_iter()
            .filter(|(name, var)| name.starts_with(prefix) && keep(var))
            .map(|(name, _)| name.to_vec())
            .collect()
    }

    /// Command names found on `PATH`.
    ///
    /// A `PATH` walk, not the hash table: completion must show a program that
    /// has never run. Duplicates (the same name in two directories) are
    /// offered once, at the position of the first hit.
    fn path_commands(&self, prefix: &[u8]) -> Vec<Vec<u8>> {
        let path = self
            .state
            .vars
            .scalar(b"PATH")
            .map(<[u8]>::to_vec)
            .unwrap_or_else(|| super::command::DEFAULT_PATH.to_vec());
        let mut out: Vec<Vec<u8>> = Vec::new();
        for directory in path.split(|&byte| byte == b':') {
            let directory: &[u8] = if directory.is_empty() {
                b"."
            } else {
                directory
            };
            let Ok(entries) = std::fs::read_dir(path_from_bytes(directory)) else {
                continue;
            };
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = os_bytes(&name);
                if !name.starts_with(prefix) || out.iter().any(|seen| seen == &name) {
                    continue;
                }
                let mut candidate = directory.to_vec();
                candidate.push(b'/');
                candidate.extend_from_slice(&name);
                if super::command::executable(&candidate) {
                    out.push(name);
                }
            }
        }
        out
    }

    /// Split and expand a `-W` word list.
    pub(crate) fn completion_wordlist(&mut self, wordlist: &[u8]) -> WordList {
        let ifs = bsh_lang::Ifs::from_vars(&*self);
        let options = self.parse_opts();
        let mut out = Vec::new();
        for field in split_wordlist(wordlist, &ifs) {
            let word = match bsh_syntax::parse_word(&field, &options) {
                Ok(word) => word,
                Err(_) => return WordList::SyntaxError,
            };
            match bsh_lang::expand_word(&word, self) {
                Ok(words) => out.extend(words),
                Err(error) => {
                    self.diagnose(&error.diagnostic());
                    return WordList::RuntimeError;
                }
            }
        }
        WordList::Words(out)
    }

    /// Whether a `-W` word list parses, without expanding it.
    pub(crate) fn completion_wordlist_parses(&mut self, wordlist: &[u8]) -> bool {
        let ifs = bsh_lang::Ifs::from_vars(&*self);
        let options = self.parse_opts();
        split_wordlist(wordlist, &ifs)
            .iter()
            .all(|field| bsh_syntax::parse_word(field, &options).is_ok())
    }

    /// Call a `-F` completion function. `None` means there is no such function.
    pub(crate) fn completion_call(&mut self, name: &[u8], argv: &[Vec<u8>]) -> Option<Status> {
        let index = self.function_index(name)?;
        let chunk = self.empty_chunk.clone();
        let unwind = self.call_function(&chunk, index, &[], &[], argv, Ctx::default());
        Some(match unwind {
            Unwind::Status(status) => status,
            Unwind::Exit(status) => {
                // bash contains an *error* unwind inside a completion function:
                // `compgen -F f` where `f` divides by zero prints nothing, the
                // shell carries on and `compgen` returns 1. An explicit `exit`
                // is not contained — it announces itself through the state's
                // exit request before the unwind gets here — and neither is
                // errexit, which takes the same path but is the caller's
                // decision to make.
                if self.state.exit_requested().is_none() {
                    // The class that produced the unwind has done its job: it
                    // is what `compgen`'s failure status reports. Leaving it
                    // recorded would make the enclosing command sequence exit
                    // a second time, which is exactly what bash does not do.
                    let _ = self.state.take_error();
                    Status::FAILURE
                } else {
                    self.state.request_exit(status);
                    status
                }
            }
            // A `break`/`continue`/`return` that escapes the completion
            // function is the enclosing context's to handle, exactly as for
            // `builtin`/`command`.
            other => {
                self.pending = Some(other);
                Status::SUCCESS
            }
        })
    }

    /// Run `argv` with its stdout captured, for `-C`.
    ///
    /// The same mechanism as `$(…)`: fork, point fd 1 at a pipe, run the
    /// command in the child and read what it produced. The candidate list has
    /// to be *captured* rather than printed because `-X`/`-P`/`-S` and the
    /// "was anything generated?" status all need to see it first.
    pub(crate) fn completion_capture(&mut self, argv: &[Vec<u8>]) -> Option<(Vec<u8>, Status)> {
        let name = argv.first()?.clone();
        let mut resolution = None;
        self.each_candidate(
            &name,
            bsh_runtime::ResolveQuery::EXECUTION,
            &mut |candidate| {
                resolution = Some(candidate);
                false
            },
        );
        let resolution = resolution?;
        let pipe = bsh_sys::Pipe::new().ok()?;
        let read = pipe.read_fd();
        let write = pipe.write_fd();
        let argv = argv.to_vec();
        let child = bsh_sys::fork_child(|| {
            self.enter_child();
            let status = match bsh_sys::dup2(write, 1) {
                Ok(()) => {
                    let _ = bsh_sys::close(write);
                    let _ = bsh_sys::close(read);
                    self.run_direct(resolution, &argv)
                }
                Err(_) => Status::FAILURE,
            };
            bsh_sys::exit_child(status.exit_code());
        })
        .ok()?;

        drop(pipe.write);
        // Reading the child's output is a foreground wait: `SIGINT`/`SIGQUIT`
        // must not kill the shell in the middle of it. A completion capture is
        // not a command of its own: it never takes the terminal.
        let mut bytes = Vec::new();
        let read_result = {
            use std::io::Read;
            let mut file = std::fs::File::from(pipe.read);
            file.read_to_end(&mut bytes)
        };
        let outcome = self
            .wait_foreground_children(&[child.pid()])
            .ok()?
            .into_iter()
            .next()?;
        let _ = self.after_foreground_wait();
        read_result.ok()?;
        Some((bytes, Status(outcome.exit_status())))
    }
}

/// The names in column `column` of a `:`-separated database such as
/// `/etc/passwd`, sorted and filtered by `prefix`.
fn column_names(path: &[u8], column: usize, prefix: &[u8]) -> Vec<Vec<u8>> {
    let Ok(bytes) = std::fs::read(path_from_bytes(path)) else {
        return Vec::new();
    };
    let mut names: Vec<Vec<u8>> = Vec::new();
    for line in bytes.split(|&byte| byte == b'\n') {
        let Some(field) = line.split(|&byte| byte == b':').nth(column) else {
            continue;
        };
        if !field.is_empty() && field.starts_with(prefix) {
            names.push(field.to_vec());
        }
    }
    names.sort_unstable();
    names.dedup();
    names
}

/// The entries a completion prefix names.
///
/// The prefix is split at its last `/`: the part before it names the directory
/// to list and the part after it filters the entry names. The result keeps the
/// prefix as written, so `subdir/f` offers `subdir/file1` — which is what bash
/// prints and what makes the candidate usable as a command argument.
fn files(prefix: &[u8], directories_only: bool) -> Vec<Vec<u8>> {
    let split = prefix.iter().rposition(|&byte| byte == b'/');
    let (lead, base): (&[u8], &[u8]) = match split {
        Some(at) => (&prefix[..=at], &prefix[at + 1..]),
        None => (&prefix[..0], prefix),
    };
    let directory: &[u8] = if lead.is_empty() {
        b"."
    } else if lead == b"/" {
        b"/"
    } else {
        &lead[..lead.len() - 1]
    };
    let Ok(entries) = std::fs::read_dir(path_from_bytes(directory)) else {
        return Vec::new();
    };
    let mut out: Vec<Vec<u8>> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = os_bytes(&name);
        if !name.starts_with(base) {
            continue;
        }
        if directories_only {
            let is_directory = entry.metadata().map(|meta| meta.is_dir()).unwrap_or(false);
            if !is_directory {
                continue;
            }
        }
        let mut candidate = lead.to_vec();
        candidate.extend_from_slice(&name);
        out.push(candidate);
    }
    out.sort_unstable();
    out
}

/// A path as the filesystem API takes it.
fn path_from_bytes(bytes: &[u8]) -> std::path::PathBuf {
    use std::os::unix::ffi::OsStrExt;
    std::path::PathBuf::from(std::ffi::OsStr::from_bytes(bytes))
}

/// A filename as the shell sees it: bytes, never a lossy string.
fn os_bytes(name: &std::ffi::OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    name.as_bytes().to_vec()
}

/// One open construct inside a `-W` word list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Context {
    /// `$(` or a plain `(` nested inside one.
    Paren,
    /// `${`.
    Brace,
    /// `` ` ``.
    Backtick,
}

/// Bytes a lexer would read as layout or as an operator rather than as part of
/// a word.
///
/// A `-W` field can contain them when `IFS` does not: `IFS=:` and the list
/// `a b` must offer `a b` as one candidate, exactly as bash does, so the
/// literal space has to survive re-lexing.
fn needs_quoting(byte: u8) -> bool {
    byte.is_ascii_whitespace()
        || matches!(byte, b';' | b'&' | b'|' | b'<' | b'>' | b'(' | b')' | b'#')
}

/// Split a `-W` word list into fields, ready to be parsed as one word each.
///
/// Delimiters are the bytes in `IFS` that are neither quoted nor escaped;
/// everything else is copied. A literal byte that would end a word when
/// re-parsed is single-quoted, which is how `a b` with `IFS=:` stays one field.
/// Quoted bytes (`'…'`, `"…"`) and the insides of `$(…)`, `${…}` and `` `…` ``
/// are copied verbatim: a `:` in there is not a delimiter.
fn split_wordlist(text: &[u8], ifs: &bsh_lang::Ifs) -> Vec<Vec<u8>> {
    let mut fields: Vec<Vec<u8>> = Vec::new();
    let mut current: Vec<u8> = Vec::new();
    // 0, `'` or `"`. Inside a `$(…)` the quote is saved on the stack and this
    // resets to 0, because quoting restarts there.
    let mut quote: u8 = 0;
    let mut stack: Vec<(Context, u8)> = Vec::new();
    let mut index = 0;
    while index < text.len() {
        let byte = text[index];

        if quote == b'\'' {
            current.push(byte);
            if byte == b'\'' {
                quote = 0;
            }
            index += 1;
            continue;
        }

        // A backslash protects the next byte everywhere except inside single
        // quotes, which the branch above already handled.
        if byte == b'\\' {
            current.push(byte);
            if let Some(&next) = text.get(index + 1) {
                current.push(next);
            }
            index += 2;
            continue;
        }

        if quote == 0 && stack.is_empty() && ifs.contains(byte) {
            if !current.is_empty() {
                fields.push(std::mem::take(&mut current));
            }
            index += 1;
            continue;
        }

        // A literal byte at the outermost level: protect the ones a lexer
        // would otherwise not read as part of the word.
        if quote == 0 && stack.is_empty() && needs_quoting(byte) {
            current.push(b'\'');
            current.push(byte);
            current.push(b'\'');
            index += 1;
            continue;
        }

        match byte {
            b'\'' => {
                quote = b'\'';
                current.push(byte);
                index += 1;
            }
            b'"' => {
                if quote == b'"' {
                    quote = 0;
                } else if quote == 0 {
                    quote = b'"';
                }
                current.push(byte);
                index += 1;
            }
            b'`' => {
                if matches!(stack.last(), Some((Context::Backtick, _))) {
                    if let Some((_, saved)) = stack.pop() {
                        quote = saved;
                    }
                } else {
                    stack.push((Context::Backtick, quote));
                    quote = 0;
                }
                current.push(byte);
                index += 1;
            }
            b'$' if text.get(index + 1) == Some(&b'(') => {
                let arithmetic = text.get(index + 2) == Some(&b'(');
                let width = if arithmetic { 3 } else { 2 };
                current.extend_from_slice(&text[index..index + width]);
                stack.push((Context::Paren, quote));
                if arithmetic {
                    // `$((` closes with `))`.
                    stack.push((Context::Paren, quote));
                }
                quote = 0;
                index += width;
            }
            b'$' if text.get(index + 1) == Some(&b'{') => {
                current.extend_from_slice(&text[index..index + 2]);
                stack.push((Context::Brace, quote));
                quote = 0;
                index += 2;
            }
            b'(' if quote == 0 && !stack.is_empty() => {
                stack.push((Context::Paren, quote));
                current.push(byte);
                index += 1;
            }
            b')' if quote == 0 && matches!(stack.last(), Some((Context::Paren, _))) => {
                if let Some((_, saved)) = stack.pop() {
                    quote = saved;
                }
                current.push(byte);
                index += 1;
            }
            b'}' if quote == 0 && matches!(stack.last(), Some((Context::Brace, _))) => {
                if let Some((_, saved)) = stack.pop() {
                    quote = saved;
                }
                current.push(byte);
                index += 1;
            }
            _ => {
                current.push(byte);
                index += 1;
            }
        }
    }
    if !current.is_empty() {
        fields.push(current);
    }
    fields
}

#[cfg(test)]
mod tests {
    use bsh_lang::Ifs;

    use super::split_wordlist;

    fn split(text: &[u8], ifs: &[u8]) -> Vec<String> {
        split_wordlist(text, &Ifs::new(ifs))
            .into_iter()
            .map(|field| String::from_utf8_lossy(&field).into_owned())
            .collect()
    }

    #[test]
    fn default_ifs_splits_on_blanks() {
        assert_eq!(split(b"one two three", b" \t\n"), ["one", "two", "three"]);
        assert_eq!(split(b"  a  b  ", b" \t\n"), ["a", "b"]);
        assert_eq!(split(b"", b" \t\n"), Vec::<String>::new());
    }

    #[test]
    fn backslash_protects_a_delimiter() {
        // `IFS=':%'`: the escaped colon belongs to the field, which is why the
        // field is kept raw and expanded afterwards. The literal blank in the
        // last field is quoted, because `IFS` has no blank and the field has to
        // survive re-lexing as one word.
        assert_eq!(
            split(b"spam:eggs%ham cheese\\:colon", b":%"),
            ["spam", "eggs", "ham' 'cheese\\:colon"]
        );
    }

    #[test]
    fn a_delimiter_inside_an_expansion_is_not_a_delimiter() {
        assert_eq!(
            split(b"$(echo \"spam:eggs%ham cheese\")", b":%"),
            ["$(echo \"spam:eggs%ham cheese\")"]
        );
    }

    #[test]
    fn a_literal_blank_a_lexer_would_split_on_is_quoted() {
        // `IFS=':'` and a space inside the field: bash offers `ham cheese` as
        // one candidate, so the field must re-parse as one word.
        assert_eq!(
            split(b"a b:c", b":"),
            ["a' 'b", "c"],
            "the literal space is single-quoted for the parser"
        );
    }

    #[test]
    fn quoting_contexts_are_tracked() {
        // A colon in each of these is protected, so the field ends at the
        // delimiter *after* the construct.
        assert_eq!(split(b"'a:b':c", b":"), ["'a:b'", "c"]);
        assert_eq!(split(b"\"a:b\":c", b":"), ["\"a:b\"", "c"]);
        assert_eq!(split(b"${x:-a:b}:c", b":"), ["${x:-a:b}", "c"]);
        assert_eq!(split(b"$((1+2)):c", b":"), ["$((1+2))", "c"]);
        assert_eq!(split(b"`echo a:b`:c", b":"), ["`echo a:b`", "c"]);
        // Quoting restarts inside `$(…)`, so the inner `'…'` protects the
        // colon while the outer field is unquoted.
        assert_eq!(split(b"$(echo 'a:b'):c", b":"), ["$(echo 'a:b')", "c"]);
    }
}

#[cfg(test)]
mod source_tests {
    use bsh_runtime::complete::{CompletionAction, CompletionSource, WordList, candidates};

    use super::super::Interp;

    fn shell() -> Interp {
        let mut interp = Interp::new(false);
        interp.set_arg0("bsh");
        interp
    }

    fn strings(values: Vec<Vec<u8>>) -> Vec<String> {
        values
            .into_iter()
            .map(|value| String::from_utf8_lossy(&value).into_owned())
            .collect()
    }

    #[test]
    fn setopts_and_shopts_are_the_option_tables() {
        let mut interp = shell();
        let set_o = strings(interp.setopts(b""));
        assert!(set_o.contains(&"errexit".to_string()));
        assert!(set_o.contains(&"pipefail".to_string()));
        assert!(
            !set_o.contains(&"nullglob".to_string()),
            "nullglob is a shopt, not a set -o option"
        );
        assert_eq!(strings(interp.setopts(b"verbose")), ["verbose"]);

        let shopt = strings(interp.shopts(b"n"));
        assert!(shopt.contains(&"nullglob".to_string()));
        assert!(shopt.contains(&"nocaseglob".to_string()));
        assert!(!shopt.contains(&"errexit".to_string()));

        // The suite's shape: `compgen -A shopt -P [ -S ] nu`.
        assert_eq!(
            strings(candidates(CompletionAction::Shopt, b"nu", &mut interp)),
            ["nullglob"]
        );
    }

    #[test]
    fn keywords_are_the_words_the_classifier_reports() {
        let mut interp = shell();
        let all = strings(interp.keywords(b""));
        for keyword in [
            "!", "[[", "]]", "{", "}", "case", "do", "done", "elif", "else", "esac", "fi", "for",
            "function", "if", "in", "then", "time", "until", "while",
        ] {
            assert!(
                all.contains(&keyword.to_string()),
                "{keyword} is a reserved word"
            );
        }
        assert_eq!(strings(interp.keywords(b"el")), ["elif", "else"]);
        assert_eq!(strings(interp.keywords(b"do")), ["do", "done"]);
    }

    #[test]
    fn builtins_functions_aliases_and_variables_come_from_state() {
        let mut interp = shell();
        interp.run_named(
            "<test>",
            b"v_global=0\nexport v_exported=1\nadd_one () { :; }\nalias v_alias=ls\n",
        );
        assert_eq!(strings(interp.builtins(b"getopts")), ["getopts"]);
        assert_eq!(strings(interp.help_topics(b"fal")), ["false"]);
        assert_eq!(strings(interp.functions(b"add")), ["add_one"]);
        assert_eq!(strings(interp.aliases(b"v_")), ["v_alias"]);
        // `visible` is name-ordered and reports the innermost binding.
        assert_eq!(strings(interp.variables(b"v_")), ["v_exported", "v_global"]);
        assert_eq!(strings(interp.exports(b"v_")), ["v_exported"]);
    }

    #[test]
    fn command_names_include_aliases_functions_builtins_and_keywords() {
        let mut interp = shell();
        interp.run_named(
            "<test>",
            b"our_func () { :; }\nour_func2 () { :; }\nalias our_alias=ls\n",
        );
        // Aliases first, then functions, in name order: bash's `-A command`.
        assert_eq!(
            strings(interp.commands(b"our_")),
            ["our_alias", "our_func", "our_func2"]
        );
        assert_eq!(strings(interp.commands(b"eva")), ["eval"]);
        assert_eq!(strings(interp.commands(b"whil")), ["while"]);
    }

    #[test]
    fn files_lists_a_directory_and_can_restrict_it_to_directories() {
        let dir = std::env::temp_dir().join(format!("bsh-complete-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("subdir")).expect("create temp dir");
        std::fs::write(dir.join("one"), b"").expect("write file");
        std::fs::write(dir.join("subdir/inner"), b"").expect("write file");

        let mut interp = shell();
        let mut prefix = dir.display().to_string().into_bytes();
        prefix.push(b'/');
        let all = strings(interp.files(&prefix, false));
        assert_eq!(all.len(), 2, "{all:?}");
        assert!(all.iter().any(|name| name.ends_with("/one")));
        assert!(all.iter().any(|name| name.ends_with("/subdir")));

        let directories = strings(interp.files(&prefix, true));
        assert_eq!(directories.len(), 1);
        assert!(directories[0].ends_with("/subdir"));

        // A directory that does not exist offers nothing; the caller reports
        // the empty list as status 1.
        assert!(interp.files(b"/non-existing-dir/", false).is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn word_lists_split_on_ifs_and_are_expanded() {
        let mut interp = shell();
        assert_eq!(
            interp.completion_wordlist(b"one two three"),
            WordList::Words(vec![b"one".to_vec(), b"two".to_vec(), b"three".to_vec()])
        );

        // `IFS=':%'` and one escaped delimiter: three words, the last holding
        // a literal colon and a literal blank.
        let mut interp = shell();
        interp.run_named("<test>", b"IFS=':%'\n");
        assert_eq!(
            interp.completion_wordlist(b"spam:eggs%ham cheese\\:colon"),
            WordList::Words(vec![
                b"spam".to_vec(),
                b"eggs".to_vec(),
                b"ham cheese:colon".to_vec()
            ])
        );

        // A list that does not parse is a usage error, not an empty list.
        assert_eq!(interp.completion_wordlist(b"${"), WordList::SyntaxError);
        assert!(!interp.completion_wordlist_parses(b"${"));
        assert!(interp.completion_wordlist_parses(b"one two"));
    }

    #[test]
    fn an_expansion_failure_is_reported_separately_from_a_syntax_error() {
        let mut interp = shell();
        assert_eq!(
            interp.completion_wordlist(b"foo $(( 1 / 0 )) bar"),
            WordList::RuntimeError
        );
    }
}
