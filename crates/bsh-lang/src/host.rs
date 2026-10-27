//! The runtime services expansion cannot provide itself.
//!
//! Command substitution, arithmetic evaluation and pathname expansion are
//! deliberately *not* implemented here (DESIGN.md §4.2 and §5.2: expansion
//! depends on runtime state, so the VM calls back into it). They are seams:
//! [`Shell`] is the trait a future `bsh-runtime`/`bsh-vm` implements, and
//! [`Passthrough`] is the stub that keeps the pipeline usable (and honest)
//! until then.
//!
//! The seam contract, in one place:
//!
//! * [`Shell::command_substitution`] runs the command and returns its stdout
//!   with trailing newlines removed. The result is field-split and globbed
//!   unless the substitution is inside double quotes.
//! * [`Shell::arithmetic`] evaluates the expression and returns its decimal
//!   text. The result is field-split like any other expansion result.
//! * [`Shell::pathname`] matches a [`Field`] as a pattern and returns the
//!   sorted matches, or `None` when nothing matched (the field is then left
//!   unchanged, which is POSIX's default without `nullglob`). Quoted bytes in
//!   the field are inert; the callback must not treat them as metacharacters.
//! * [`Shell::process_substitution`] returns the `/dev/fd/N` path.
//! * [`Shell::home_dir`] resolves `~user`; `~` itself uses `$HOME` first.

use crate::expand::{Error, Field};
use crate::vars::{SetError, Vars};
use crate::word::UnsupportedOp;
use crate::word::{Arithmetic, CommandSub, ProcessSub};

/// A runtime that can execute command substitutions, evaluate arithmetic,
/// glob and resolve users.
///
/// Implemented by `bsh-runtime` later; tests implement it over maps. It
/// extends [`Vars`] so a single object can be handed to
/// [`crate::expand_word`].
pub trait Shell: Vars {
    /// Run `$(…)` / `` `…` `` and return stdout without trailing newlines.
    fn command_substitution(&mut self, cmd: &CommandSub) -> Result<Vec<u8>, Error>;

    /// Evaluate `$((…))` and return the decimal text.
    fn arithmetic(&mut self, expr: &Arithmetic) -> Result<Vec<u8>, Error>;

    /// Match `pattern` as a pathname pattern.
    ///
    /// Returns `None` when nothing matched (leave the field alone), or the
    /// matches in the order the caller should receive them.
    fn pathname(&mut self, pattern: &Field) -> Result<Option<Vec<Vec<u8>>>, Error>;

    /// Turn `<(…)` / `>(…)` into a path.
    ///
    /// Unsupported by default: a wrong answer here silently changes which file
    /// a command reads, so the default is an error rather than a guess.
    fn process_substitution(&mut self, subs: &ProcessSub) -> Result<Vec<u8>, Error> {
        Err(Error::unsupported(
            UnsupportedOp::ProcessSubstitution,
            subs.span.clone(),
        ))
    }

    /// The home directory of `user`; empty `user` means the current user.
    ///
    /// Returning `None` leaves `~user` unchanged, which is what bash does for
    /// an unknown login name. `~` itself consults `$HOME` before this hook.
    fn home_dir(&mut self, user: &[u8]) -> Option<Vec<u8>> {
        let _ = user;
        None
    }
}

/// A [`Vars`] with no variables at all.
///
/// Useful with [`Passthrough`] and in tests that only exercise quoting.
#[derive(Clone, Copy, Debug, Default)]
pub struct EmptyVars;

impl Vars for EmptyVars {
    fn get(&self, _name: &[u8]) -> Option<&[u8]> {
        None
    }

    fn positional(&self) -> &[Vec<u8>] {
        &[]
    }

    fn set(&mut self, _name: &[u8], _value: Vec<u8>) -> Result<(), SetError> {
        Err(SetError::ReadOnly)
    }
}

/// A [`Shell`] that leaves every expansion it cannot do *unchanged*.
///
/// This is the documented stub for the command-substitution and pathname
/// seams: `$(cmd)` becomes the text `cmd`, `$((1+2))` becomes `1+2`, and
/// globbing leaves the field exactly as it was. Nothing is executed and no
/// filesystem is touched, so the pipeline itself (including IFS splitting and
/// quote removal) can be tested and run before the runtime exists.
///
/// It is **not** a shell: use it for `bsh explain` and tests, never in a
/// running shell.
#[derive(Clone, Debug, Default)]
pub struct Passthrough<V = EmptyVars> {
    /// The variable source to answer lookups from.
    pub vars: V,
}

impl<V> Passthrough<V> {
    /// Wrap a variable source.
    pub fn new(vars: V) -> Self {
        Self { vars }
    }
}

impl Passthrough<EmptyVars> {
    /// A stub with no variables.
    pub fn empty() -> Self {
        Self { vars: EmptyVars }
    }
}

impl<V: Vars> Vars for Passthrough<V> {
    fn get(&self, name: &[u8]) -> Option<&[u8]> {
        self.vars.get(name)
    }

    fn positional(&self) -> &[Vec<u8>] {
        self.vars.positional()
    }

    fn set(&mut self, name: &[u8], value: Vec<u8>) -> Result<(), SetError> {
        self.vars.set(name, value)
    }
}

impl<V: Vars> Shell for Passthrough<V> {
    fn command_substitution(&mut self, cmd: &CommandSub) -> Result<Vec<u8>, Error> {
        Ok(cmd.source.clone())
    }

    fn arithmetic(&mut self, expr: &Arithmetic) -> Result<Vec<u8>, Error> {
        Ok(expr.source.clone())
    }

    fn pathname(&mut self, _pattern: &Field) -> Result<Option<Vec<Vec<u8>>>, Error> {
        Ok(None)
    }
}
