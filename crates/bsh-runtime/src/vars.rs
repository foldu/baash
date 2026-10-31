//! Variables, scopes, positional parameters and the environment.
//!
//! The store is a scope stack: one global scope, then zero or more frames
//! pushed by function calls or by `local`/`declare` (DESIGN.md §4.6). Lookup
//! walks inward-out: the innermost frame that defines a name wins, then the
//! global scope.
//!
//! Writes go through [`Vars::resolve`], the single write entry point. Reads go
//! through [`Vars::lookup`]/[`Vars::scalar`], so expanding `${unset}` never
//! creates a variable as a side effect.
//!
//! A cell with the `nameref` attribute (`declare -n`) holds the *name* it
//! refers to; [`Vars::resolve_chain`] follows that name for reads and writes,
//! while [`Vars::lookup`] and [`Vars::visible`] stay raw so `declare -p` can
//! print the reference itself.

use crate::Status;
use crate::builtins::{AssignPersistence, Tier};
use crate::errors::{Error, display};
use std::borrow::Cow;
use std::collections::BTreeMap;

/// Variable attributes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Attrs {
    /// Passed to children via the environment.
    pub exported: bool,
    /// Writes are an error.
    pub readonly: bool,
    /// Arithmetic on assignment. The evaluator lives in `bsh-lang`; the store
    /// keeps the attribute and the bytes the VM gives it.
    pub integer: bool,
    /// The cell's scalar value is the *name* of another variable
    /// (`declare -n`, bash's nameref).
    ///
    /// Reads and writes through the cell follow the name; `declare -p` and the
    /// attribute-changing builtins see the cell itself. See
    /// [`Vars::resolve_chain`].
    pub nameref: bool,
}

/// A variable's value. Bash arrays are extensions, hence the extra shapes.
///
/// [`VarValue::Assoc`] preserves insertion order (bash does too), so iterating
/// it is deterministic and a round-trip through `set_assoc` does not reorder.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum VarValue {
    /// Declared but unset.
    #[default]
    Unset,
    /// A scalar.
    Str(Vec<u8>),
    /// An indexed array, stored sparsely: subscript → element.
    ///
    /// A `BTreeMap` gives index order, which `${a[@]}` and `${!a[@]}` promise,
    /// and it costs one entry per *assigned* subscript. `a[100]=x` must not
    /// allocate 101 slots, and `unset 'a[0]'` must leave a hole `${a[@]}`
    /// skips — a `Vec` cannot express either without sentinel elements that
    /// are observable through `${#a[@]}` and `${!a[@]}`.
    ///
    /// The key is `i64`, not a narrower integer: bash subscripts are machine
    /// integers, and the conformance suite exercises `a[0x7FFFFFFFFFFFFFFF]`.
    /// Negative subscripts are resolved against the array before storage.
    Indexed(BTreeMap<i64, Vec<u8>>),
    /// An associative array, kept as insertion-ordered pairs.
    Assoc(Vec<(Vec<u8>, Vec<u8>)>),
}

impl VarValue {
    /// The scalar bytes, if this is a string.
    pub fn as_str(&self) -> Option<&[u8]> {
        match self {
            VarValue::Str(bytes) => Some(bytes),
            _ => None,
        }
    }

    /// Whether the variable is set to anything (including an empty string).
    pub fn is_set(&self) -> bool {
        !matches!(self, VarValue::Unset)
    }

    /// The array shape, if this value has one.
    pub fn shape(&self) -> Option<ArrayShape> {
        match self {
            VarValue::Indexed(_) => Some(ArrayShape::Indexed),
            VarValue::Assoc(_) => Some(ArrayShape::Associative),
            _ => None,
        }
    }
}

/// A variable: value plus attributes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Var {
    /// Current value.
    pub value: VarValue,
    /// Current attributes.
    pub attrs: Attrs,
}

impl Var {
    /// The scalar value, if this variable holds one.
    pub fn scalar(&self) -> Option<&[u8]> {
        self.value.as_str()
    }
}

/// The shape to give a variable declared with `-a` or `-A`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArrayShape {
    /// `declare -a`: an indexed array.
    Indexed,
    /// `declare -A`: an associative array.
    Associative,
}

impl ArrayShape {
    fn empty(self) -> VarValue {
        match self {
            ArrayShape::Indexed => VarValue::Indexed(BTreeMap::new()),
            ArrayShape::Associative => VarValue::Assoc(Vec::new()),
        }
    }
}

/// Which scope a write targets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CreateScope {
    /// Update the variable where it already exists, otherwise create it in the
    /// global scope. This is plain shell assignment (`x=1`), including inside
    /// a function: without `local`, a function writes the global.
    Default,
    /// Update or create in the innermost frame. This is `local`/`declare`
    /// inside a function. Errors with [`Error::FrameUnderflow`] when no frame
    /// is open; the `local` builtin reports its own error at top level.
    Local,
    /// Update or create in the global scope, ignoring any frame
    /// (`declare -g`).
    Global,
}

/// What kind of scope a frame is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameKind {
    /// A function invocation. It saves and restores positional parameters.
    Function,
    /// A `local`/`declare` frame or a temporary block.
    Local,
}

/// Identifies a frame, so a caller can unwind to a known depth.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameId {
    depth: usize,
}

impl FrameId {
    /// Number of frames on the stack after this frame was pushed.
    pub fn depth(self) -> usize {
        self.depth
    }
}

#[derive(Clone, Debug)]
struct Frame {
    kind: FrameKind,
    locals: Vec<(Vec<u8>, Var)>,
    saved_positional: Positional,
}

/// Positional parameters (`$1`, `$2`, …). `$0` is separate (see
/// [`Vars::arg0`]), matching POSIX.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Positional {
    /// The parameters, in order.
    pub argv: Vec<Vec<u8>>,
}

impl Positional {
    /// Positional parameters from a vector of arguments.
    pub fn new(argv: Vec<Vec<u8>>) -> Self {
        Self { argv }
    }

    /// `$#`.
    pub fn count(&self) -> usize {
        self.argv.len()
    }

    /// Whether there are no positional parameters.
    pub fn is_empty(&self) -> bool {
        self.argv.is_empty()
    }

    /// `$n`, with `n` one-based. `$0` is not a positional parameter, so
    /// `get(0)` is `None`.
    pub fn get(&self, n: usize) -> Option<&[u8]> {
        if n == 0 {
            None
        } else {
            self.argv.get(n - 1).map(Vec::as_slice)
        }
    }

    /// Replace all parameters.
    pub fn set(&mut self, argv: Vec<Vec<u8>>) {
        self.argv = argv;
    }

    /// `shift n`. Shifts by `n` (default handling lives in the builtin).
    ///
    /// Returns [`Error::ShiftUnderflow`] and leaves the parameters untouched
    /// when `n > $#`, which is what `set -o shift_verbose` reports and what
    /// plain `shift` treats as a (masked) failure.
    pub fn shift(&mut self, n: usize) -> Result<(), Error> {
        if n > self.argv.len() {
            return Err(Error::ShiftUnderflow {
                requested: n,
                available: self.argv.len(),
            });
        }
        self.argv.drain(..n);
        Ok(())
    }
}

/// Maximum number of nameref hops before [`Error::NamerefCycle`]. Bash uses a
/// similar guard; the exact number is not observable in normal scripts.
const NAMEREF_MAX_DEPTH: usize = 16;

/// Shell variables: the global scope, the frame stack, positional parameters
/// and the special parameters' state.
#[derive(Clone, Debug)]
pub struct Vars {
    global: Vec<(Vec<u8>, Var)>,
    frames: Vec<Frame>,
    positional: Positional,
    arg0: Vec<u8>,
    last_status: Status,
    shell_pid: u32,
    last_bg_pid: Option<i32>,
    /// Bumped on every write to `OPTIND`.
    ///
    /// `getopts` keeps its scan position inside a clustered argument
    /// (`-ab`: after `a`) out of band, and a script resets the scan by
    /// assigning `OPTIND`. The assignment is the signal rather than the value,
    /// because `OPTIND=1` written twice must re-parse the same argument, so
    /// the counter is what makes a reset observable.
    optind_writes: u64,
}

impl Default for Vars {
    fn default() -> Self {
        Self {
            global: Vec::new(),
            frames: Vec::new(),
            positional: Positional::default(),
            arg0: Vec::new(),
            last_status: Status::SUCCESS,
            shell_pid: std::process::id(),
            last_bg_pid: None,
            optind_writes: 0,
        }
    }
}

impl Vars {
    /// Whether `name` is a valid shell variable name: `[A-Za-z_][A-Za-z0-9_]*`.
    ///
    /// Shell names are bytes; non-ASCII bytes are not valid here because they
    /// cannot be spelled portably, and `export 1x=2` must be an error rather
    /// than a silently created variable.
    pub fn is_name(name: &[u8]) -> bool {
        let mut bytes = name.iter();
        match bytes.next() {
            Some(&first) if first == b'_' || first.is_ascii_alphabetic() => {}
            _ => return false,
        }
        bytes.all(|b| *b == b'_' || b.is_ascii_alphanumeric())
    }

    /// Read a variable, walking frames inward-out.
    pub fn lookup(&self, name: &[u8]) -> Option<&Var> {
        for frame in self.frames.iter().rev() {
            if let Some((_, var)) = frame
                .locals
                .iter()
                .find(|(candidate, _)| candidate.as_slice() == name)
            {
                return Some(var);
            }
        }
        self.global
            .iter()
            .find(|(candidate, _)| candidate.as_slice() == name)
            .map(|(_, var)| var)
    }

    /// The variable's value, if it exists.
    ///
    /// Namerefs are followed: `-n r=x; value(r)` is `x`'s value.
    pub fn value(&self, name: &[u8]) -> Option<&VarValue> {
        match self.resolve_chain(name).ok()? {
            Resolved::Var(target) => self.lookup(&target).map(|var| &var.value),
            Resolved::Indexed(..) | Resolved::Assoc(..) => None,
            Resolved::Elements(base) => self.lookup(&base).map(|var| &var.value),
        }
    }

    /// The scalar bytes of a variable, if it exists and is a string.
    ///
    /// Namerefs are followed, including element references: `-n r='a[2]'`
    /// makes `scalar(r)` the bytes of `a[2]`.
    pub fn scalar(&self, name: &[u8]) -> Option<&[u8]> {
        match self.resolve_chain(name).ok()? {
            Resolved::Var(target) => self.lookup(&target).and_then(Var::scalar),
            Resolved::Indexed(base, index) => self.element_indexed(&base, index),
            Resolved::Assoc(base, key) => self.element_assoc(&base, &key),
            Resolved::Elements(_) => None,
        }
    }

    /// Whether the variable exists (even if it is `Unset`).
    pub fn contains(&self, name: &[u8]) -> bool {
        match self.resolve_chain(name) {
            Ok(Resolved::Var(target)) => self.lookup(&target).is_some(),
            Ok(_) => true,
            Err(_) => false,
        }
    }

    /// Whether the variable exists and is set.
    pub fn is_set(&self, name: &[u8]) -> bool {
        match self.resolve_chain(name) {
            Ok(Resolved::Var(target)) => self.lookup(&target).is_some_and(|var| var.value.is_set()),
            Ok(Resolved::Indexed(base, index)) => self.element_indexed(&base, index).is_some(),
            Ok(Resolved::Assoc(base, key)) => self.element_assoc(&base, &key).is_some(),
            Ok(Resolved::Elements(base)) => self.array_len(&base) > 0,
            Err(_) => false,
        }
    }

    /// Whether the variable is readonly.
    pub fn is_readonly(&self, name: &[u8]) -> bool {
        match self.resolve_chain(name) {
            Ok(Resolved::Var(target)) => self.lookup(&target).is_some_and(|var| var.attrs.readonly),
            _ => false,
        }
    }

    /// Whether the variable is exported.
    pub fn is_exported(&self, name: &[u8]) -> bool {
        match self.resolve_chain(name) {
            Ok(Resolved::Var(target)) => self.lookup(&target).is_some_and(|var| var.attrs.exported),
            _ => false,
        }
    }

    /// Overwrite a *derived* variable that scripts cannot assign.
    ///
    /// `SHELLOPTS` and `BASHOPTS` are readonly for scripts, but the shell
    /// recomputes their values after every command (and when options arrive
    /// from the environment), so the interpreter needs a write that bypasses
    /// the readonly check without dropping the attribute.
    pub fn refresh_derived(&mut self, name: &[u8], value: &[u8]) -> Result<(), Error> {
        // Not `resolve`: that goes through the readonly check, which is exactly
        // the one this write is allowed to bypass.
        let target = match self.resolve_chain(name)? {
            Resolved::Var(target) => target,
            _ => {
                return Err(Error::InvalidName {
                    name: name.to_vec(),
                });
            }
        };
        let var = self.cell_mut(&target, CreateScope::Default, false)?;
        var.value = VarValue::Str(value.to_vec());
        Ok(())
    }

    /// Number of open frames.
    pub fn frame_depth(&self) -> usize {
        self.frames.len()
    }

    /// Push a function frame. Positional parameters are captured and restored
    /// by [`Vars::pop_frame`], so a function sees its own `$@` and the caller
    /// keeps theirs.
    pub fn push_frame(&mut self) -> FrameId {
        self.push_kind(FrameKind::Function)
    }

    /// Push a `local`/`declare` or temporary frame. Positional parameters are
    /// captured and restored like a function frame's.
    pub fn push_local_frame(&mut self) -> FrameId {
        self.push_kind(FrameKind::Local)
    }

    fn push_kind(&mut self, kind: FrameKind) -> FrameId {
        self.frames.push(Frame {
            kind,
            locals: Vec::new(),
            saved_positional: self.positional.clone(),
        });
        FrameId {
            depth: self.frames.len(),
        }
    }

    /// Pop the innermost frame, restoring the positional parameters it saved.
    pub fn pop_frame(&mut self) -> Result<(), Error> {
        let frame = self.frames.pop().ok_or(Error::FrameUnderflow)?;
        self.positional = frame.saved_positional;
        Ok(())
    }

    /// The innermost frame's kind, if any.
    pub fn frame_kind(&self) -> Option<FrameKind> {
        self.frames.last().map(|frame| frame.kind)
    }

    /// Positional parameters.
    pub fn positional(&self) -> &Positional {
        &self.positional
    }

    /// Replace the positional parameters (`set -- …`).
    pub fn set_positional(&mut self, argv: Vec<Vec<u8>>) {
        self.positional.set(argv);
    }

    /// `$#`.
    pub fn arg_count(&self) -> usize {
        self.positional.count()
    }

    /// `shift n`.
    pub fn shift(&mut self, n: usize) -> Result<(), Error> {
        self.positional.shift(n)
    }

    /// `$0`.
    pub fn arg0(&self) -> &[u8] {
        &self.arg0
    }

    /// Set `$0`.
    pub fn set_arg0(&mut self, arg0: Vec<u8>) {
        self.arg0 = arg0;
    }

    /// `$?`.
    pub fn last_status(&self) -> Status {
        self.last_status
    }

    /// Set `$?` after a command.
    pub fn set_last_status(&mut self, status: Status) {
        self.last_status = status;
    }

    /// `$$`.
    pub fn shell_pid(&self) -> u32 {
        self.shell_pid
    }

    /// Set `$$` (a subshell gives itself the parent's pid, as POSIX requires).
    pub fn set_shell_pid(&mut self, pid: u32) {
        self.shell_pid = pid;
    }

    /// `$!`, or `None` if no background job has been started.
    pub fn last_bg_pid(&self) -> Option<i32> {
        self.last_bg_pid
    }

    /// Set `$!`.
    pub fn set_last_bg_pid(&mut self, pid: Option<i32>) {
        self.last_bg_pid = pid;
    }

    /// The value of a single-character special parameter.
    ///
    /// `$@` and `$*` return `None`: they are multi-field and need the
    /// expansion layer's quoting rules, not a byte string. `$0`…`$9` are
    /// *not* special in this sense; use [`Vars::positional`].
    pub fn special_scalar(&self, name: u8, opts: &crate::Opts) -> Option<Vec<u8>> {
        match name {
            b'0' => Some(self.arg0.clone()),
            b'?' => Some(self.last_status.exit_code().to_string().into_bytes()),
            b'$' => Some(self.shell_pid.to_string().into_bytes()),
            b'!' => Some(
                self.last_bg_pid
                    .map(|pid| pid.to_string().into_bytes())
                    .unwrap_or_default(),
            ),
            b'#' => Some(self.arg_count().to_string().into_bytes()),
            b'-' => Some(opts.dash_flags()),
            _ => None,
        }
    }

    /// Resolve `name` for writing, creating it if it does not exist.
    ///
    /// This is the single write entry point (DESIGN.md §4.6): writes to a
    /// readonly variable return [`Error::Readonly`] instead of panicking, and
    /// a nameref's chain is followed here. Reads use [`Vars::lookup`].
    ///
    /// Missing variables are created in the global scope, matching plain
    /// assignment inside a function. Use [`Vars::resolve_in`] with
    /// [`CreateScope::Local`] for `local`/`declare`.
    pub fn resolve(&mut self, name: &[u8]) -> Result<&mut Var, Error> {
        self.resolve_in(name, CreateScope::Default)
    }

    /// Resolve `name` for writing, creating it in `scope` if missing.
    pub fn resolve_in(&mut self, name: &[u8], scope: CreateScope) -> Result<&mut Var, Error> {
        let target = self.deref_name(name)?;
        if target.as_ref() == b"OPTIND" {
            self.optind_writes = self.optind_writes.wrapping_add(1);
        }
        self.cell_mut(&target, scope, true)
    }

    /// How many times `OPTIND` has been written.
    ///
    /// `getopts` uses this to notice an assignment that was meant as a reset.
    /// It counts writes through [`Vars::resolve_in`], which is the single
    /// choke point for variable creation and scalar assignment.
    pub fn optind_writes(&self) -> u64 {
        self.optind_writes
    }

    /// Assign a complete value.
    ///
    /// A scalar assignment to an indexed array sets element 0 and keeps the
    /// array shape; a scalar assignment to an associative array writes key
    /// `0`, and a nameref writes through to its target. When the target is an
    /// element reference (`-n r='a[2]'`), the value lands in that element.
    pub fn assign(&mut self, name: &[u8], value: VarValue) -> Result<(), Error> {
        match self.resolve_chain(name)? {
            Resolved::Var(target) => {
                let var = self.cell_mut(&target, CreateScope::Default, true)?;
                match value {
                    VarValue::Str(bytes) => set_scalar(var, name, bytes),
                    other => {
                        var.value = other;
                        Ok(())
                    }
                }
            }
            Resolved::Indexed(base, index) => match value {
                VarValue::Str(bytes) => self.set_indexed(&base, index, &bytes),
                _ => Err(Error::NotScalar {
                    name: name.to_vec(),
                }),
            },
            Resolved::Assoc(base, key) => match value {
                VarValue::Str(bytes) => self.set_assoc(&base, &key, &bytes),
                _ => Err(Error::NotScalar {
                    name: name.to_vec(),
                }),
            },
            Resolved::Elements(base) => Err(Error::InvalidName {
                name: whole_array_name(base),
            }),
        }
    }

    /// Assign a scalar, with the array rules of [`Vars::assign`].
    pub fn assign_scalar(&mut self, name: &[u8], value: &[u8]) -> Result<(), Error> {
        self.assign(name, VarValue::Str(value.to_vec()))
    }

    /// Assign a scalar in a specific scope, used for prefix assignments and
    /// `local`.
    pub fn assign_scalar_in(
        &mut self,
        name: &[u8],
        value: &[u8],
        scope: CreateScope,
    ) -> Result<(), Error> {
        match self.resolve_chain(name)? {
            Resolved::Var(target) => {
                let var = self.cell_mut(&target, scope, true)?;
                set_scalar(var, name, value.to_vec())
            }
            Resolved::Indexed(base, index) => self.set_indexed(&base, index, value),
            Resolved::Assoc(base, key) => self.set_assoc(&base, &key, value),
            Resolved::Elements(base) => Err(Error::InvalidName {
                name: whole_array_name(base),
            }),
        }
    }

    /// `x+=suffix`: append to a scalar. Appending to an array requires the
    /// array operation, because the meaning depends on the shape
    /// (`a+=(x)` appends an element; `a+=x` is not the same thing).
    pub fn append(&mut self, name: &[u8], suffix: &[u8]) -> Result<(), Error> {
        match self.resolve_chain(name)? {
            Resolved::Var(target) => {
                let var = self.cell_mut(&target, CreateScope::Default, true)?;
                match &mut var.value {
                    VarValue::Str(bytes) => {
                        bytes.extend_from_slice(suffix);
                        Ok(())
                    }
                    // `a+=text` appends to element zero; bash does not treat it
                    // as "append an element" (that spelling is `a+=(…)`).
                    VarValue::Indexed(elements) => {
                        elements.entry(0).or_default().extend_from_slice(suffix);
                        Ok(())
                    }
                    VarValue::Unset => {
                        var.value = VarValue::Str(suffix.to_vec());
                        Ok(())
                    }
                    VarValue::Assoc(_) => Err(Error::NotScalar {
                        name: name.to_vec(),
                    }),
                }
            }
            Resolved::Indexed(base, index) => self.append_indexed(&base, index, suffix),
            Resolved::Assoc(base, key) => self.append_assoc(&base, &key, suffix),
            Resolved::Elements(base) => Err(Error::InvalidName {
                name: whole_array_name(base),
            }),
        }
    }

    /// Declare a local variable in the innermost frame, without a value.
    ///
    /// Bash compatibility notes, verified against bash 5.3.20:
    ///
    /// * a local inherits the *exported* attribute of the variable it shadows,
    ///   so `export x=G; f() { local x=L; env; }` shows `x=L`;
    /// * it does not inherit `integer` (`declare -i n` + `local n` is a plain
    ///   string);
    /// * it refuses to shadow a readonly variable (bash reports
    ///   `local: x: readonly variable`).
    pub fn declare_local(&mut self, name: &[u8]) -> Result<(), Error> {
        Self::check_name(name)?;
        let index = self
            .frames
            .len()
            .checked_sub(1)
            .ok_or(Error::FrameUnderflow)?;
        if self.frames[index]
            .locals
            .iter()
            .any(|(candidate, _)| candidate.as_slice() == name)
        {
            return Ok(());
        }
        let (exported, readonly) = self
            .lookup(name)
            .map(|var| (var.attrs.exported, var.attrs.readonly))
            .unwrap_or((false, false));
        if readonly {
            return Err(Error::Readonly {
                name: name.to_vec(),
            });
        }
        self.frames[index].locals.push((
            name.to_vec(),
            Var {
                value: VarValue::Unset,
                attrs: Attrs {
                    exported,
                    ..Attrs::default()
                },
            },
        ));
        Ok(())
    }

    /// `local x=value`.
    pub fn local_scalar(&mut self, name: &[u8], value: &[u8]) -> Result<(), Error> {
        self.declare_local(name)?;
        self.assign_scalar_in(name, value, CreateScope::Local)
    }

    /// Declare `name` as an indexed or associative array (`declare -a/-A`).
    ///
    /// Returns [`Error::ArrayShapeMismatch`] when the variable already has the
    /// other shape (bash refuses indexed↔associative conversion). A scalar
    /// becomes an array with the scalar at element/key zero, matching bash.
    pub fn declare_shape(&mut self, name: &[u8], shape: ArrayShape) -> Result<(), Error> {
        let var = self.resolve(name)?;
        if var.value.shape() == Some(shape) {
            return Ok(());
        }
        match &var.value {
            VarValue::Unset => {
                var.value = shape.empty();
                Ok(())
            }
            // A scalar converts either way: bash maps it to element/key zero
            // rather than discarding it.
            VarValue::Str(bytes) => {
                let bytes = bytes.clone();
                var.value = match shape {
                    ArrayShape::Indexed => VarValue::Indexed(BTreeMap::from([(0, bytes)])),
                    ArrayShape::Associative => VarValue::Assoc(vec![(b"0".to_vec(), bytes)]),
                };
                Ok(())
            }
            _ => Err(Error::ArrayShapeMismatch {
                name: name.to_vec(),
            }),
        }
    }

    /// Create `name` in `scope` without a value (`declare x`), leaving an
    /// existing cell — and its value and attributes — alone.
    pub fn declare_empty(&mut self, name: &[u8], scope: CreateScope) -> Result<(), Error> {
        Self::check_name(name)?;
        self.cell_mut(name, scope, false)?;
        Ok(())
    }

    /// Mark a variable exported, creating it in the global scope if it does
    /// not exist.
    ///
    /// The attribute lands on the *named cell*, not on a nameref's target:
    /// `x=foo; typeset -n -x ref=x` exports `ref` with the value `x`, which is
    /// what bash puts in the child environment (the reference's own text, not
    /// the target's value).
    pub fn mark_exported(&mut self, name: &[u8]) -> Result<(), Error> {
        self.cell_mut(name, CreateScope::Default, false)?
            .attrs
            .exported = true;
        Ok(())
    }

    /// Remove the export attribute.
    pub fn unmark_exported(&mut self, name: &[u8]) -> Result<(), Error> {
        if let Some(var) = self.lookup_mut(name) {
            var.attrs.exported = false;
        }
        Ok(())
    }

    /// Mark a variable readonly, creating it in the global scope if it does
    /// not exist. Attribute changes themselves are allowed on readonly
    /// variables (`readonly x` twice is not an error); only value writes are
    /// refused.
    pub fn mark_readonly(&mut self, name: &[u8]) -> Result<(), Error> {
        self.cell_mut(name, CreateScope::Default, false)?
            .attrs
            .readonly = true;
        Ok(())
    }

    /// Clear the readonly attribute (`typeset +r`).
    ///
    /// bash's `typeset +r` is documented as doing nothing; the conformance
    /// suite pins the OSH reading, where the attribute is cleared, and the
    /// primary expectation is that one.
    pub fn unmark_readonly(&mut self, name: &[u8]) -> Result<(), Error> {
        if let Some(var) = self.lookup_mut(name) {
            var.attrs.readonly = false;
        }
        Ok(())
    }

    /// Mark a variable integer (`declare -i`).
    pub fn mark_integer(&mut self, name: &[u8]) -> Result<(), Error> {
        self.cell_mut(name, CreateScope::Default, false)?
            .attrs
            .integer = true;
        Ok(())
    }

    /// Clear the integer attribute (`typeset +i`).
    pub fn unmark_integer(&mut self, name: &[u8]) -> Result<(), Error> {
        if let Some(var) = self.lookup_mut(name) {
            var.attrs.integer = false;
        }
        Ok(())
    }

    /// Make `name` a nameref (`declare -n`) with the given reference text.
    ///
    /// Verified against bash 5.3.20:
    ///
    /// * the reference must be a plain name or a `name[subscript]` form; `1`,
    ///   `#`, `@` and `a-b` are rejected with the word
    ///   `invalid variable name for name reference`;
    /// * a direct self-reference is rejected (`r: nameref variable self
    ///   references not allowed`);
    /// * a rejected reference leaves the variable exactly as it was, and does
    ///   not create it;
    /// * with no reference given the cell keeps its current value, which
    ///   becomes the reference text.
    ///
    /// The attribute is set on the cell; the value is stored raw so that a
    /// later `declare -p` round-trips and a later use can resolve it.
    pub fn declare_nameref(
        &mut self,
        name: &[u8],
        reference: Option<&[u8]>,
        scope: CreateScope,
    ) -> Result<(), Error> {
        Self::check_name(name)?;
        // `typeset -n ref` with no value promotes the cell's *current* value to
        // a reference, and bash validates that too: `ref=1; typeset -n ref`
        // reports `1: invalid variable name for name reference` and leaves the
        // variable plain.
        let inherited = reference
            .is_none()
            .then(|| {
                self.lookup(name)
                    .and_then(|var| var.value.as_str())
                    .filter(|value| !value.is_empty())
                    .map(<[u8]>::to_vec)
            })
            .flatten();
        if let Some(candidate) = reference.or(inherited.as_deref()) {
            match parse_reference(candidate) {
                Some(Reference::Plain(base)) if base == name => {
                    return Err(Error::NamerefSelf {
                        name: name.to_vec(),
                    });
                }
                Some(Reference::Element { base, .. }) if base == name => {
                    return Err(Error::NamerefSelf {
                        name: name.to_vec(),
                    });
                }
                Some(_) => {}
                None => {
                    return Err(Error::InvalidName {
                        name: candidate.to_vec(),
                    });
                }
            }
        }
        let var = self.cell_mut(name, scope, false)?;
        if let Some(reference) = reference {
            var.value = VarValue::Str(reference.to_vec());
        }
        var.attrs.nameref = true;
        Ok(())
    }

    /// Clear the nameref attribute (`typeset +n`). The value, which holds the
    /// last reference text, stays put — that is exactly what bash prints after
    /// `typeset +n ref`.
    pub fn unmark_nameref(&mut self, name: &[u8]) -> Result<(), Error> {
        if let Some(var) = self.lookup_mut(name) {
            var.attrs.nameref = false;
        }
        Ok(())
    }

    /// Unset a variable (`unset name`).
    ///
    /// The variable is removed from the scope where it is found; a local
    /// removal reveals the outer variable, which is bash's behavior. Unsetting
    /// a readonly variable is an error. Unsetting a name that does not exist
    /// succeeds. A nameref is followed, so `unset ref` removes the target and
    /// leaves the reference dangling.
    pub fn unset(&mut self, name: &[u8]) -> Result<(), Error> {
        Self::check_name(name)?;
        match self.resolve_chain(name)? {
            Resolved::Var(target) => self.remove_cell(&target),
            Resolved::Indexed(base, index) => self.unset_element_indexed(&base, index),
            Resolved::Assoc(base, key) => self.unset_element_assoc(&base, &key),
            Resolved::Elements(base) => self.unset_all_elements(&base),
        }
    }

    fn remove_cell(&mut self, name: &[u8]) -> Result<(), Error> {
        let Some(location) = self.location(name) else {
            return Ok(());
        };
        if self.var_at(location).attrs.readonly {
            return Err(Error::Readonly {
                name: name.to_vec(),
            });
        }
        match location {
            Location::Global(index) => {
                self.global.remove(index);
            }
            Location::Frame { frame, index } => {
                self.frames[frame].locals.remove(index);
            }
        }
        Ok(())
    }

    /// `a[index]=value`: set an indexed element.
    ///
    /// A negative index counts from `greatest index + 1`, bash's rule; an
    /// index that still lands before zero is [`Error::ArrayOutOfBounds`]. A
    /// scalar is promoted: element 0 keeps the old value when a *different*
    /// index is written, matching `s=hi; s[2]=x`.
    pub fn set_indexed(&mut self, name: &[u8], index: i64, value: &[u8]) -> Result<(), Error> {
        let var = self.resolve(name)?;
        match &mut var.value {
            VarValue::Assoc(_) => {
                return Err(Error::NotIndexed {
                    name: name.to_vec(),
                });
            }
            VarValue::Indexed(elements) => {
                let resolved = resolve_write_index(elements, index, name)?;
                elements.insert(resolved, value.to_vec());
                return Ok(());
            }
            VarValue::Str(_) | VarValue::Unset => {}
        }

        // Promote a scalar (keeping it at element zero) or create the array.
        let old = std::mem::take(&mut var.value);
        let mut elements = match &old {
            VarValue::Str(element) => BTreeMap::from([(0, element.clone())]),
            _ => BTreeMap::new(),
        };
        let resolved = match resolve_write_index(&elements, index, name) {
            Ok(resolved) => resolved,
            Err(error) => {
                // A refused subscript must leave the old value alone.
                var.value = old;
                return Err(error);
            }
        };
        elements.insert(resolved, value.to_vec());
        var.value = VarValue::Indexed(elements);
        Ok(())
    }

    /// `a[index]+=suffix`: append to one indexed element.
    pub fn append_indexed(&mut self, name: &[u8], index: i64, suffix: &[u8]) -> Result<(), Error> {
        let current = self
            .element_indexed(name, index)
            .unwrap_or_default()
            .to_vec();
        let mut joined = current;
        joined.extend_from_slice(suffix);
        self.set_indexed(name, index, &joined)
    }

    /// `a[key]=value`: set an associative element, inserting in order.
    ///
    /// An empty key is refused: bash reports `name[]: bad array subscript`
    /// for `declare -A a; a[""]=x`, and accepting it would make `a[$unset]`
    /// silently create a key no other shell can address.
    pub fn set_assoc(&mut self, name: &[u8], key: &[u8], value: &[u8]) -> Result<(), Error> {
        if key.is_empty() {
            return Err(Error::BadSubscript {
                name: name.to_vec(),
            });
        }
        let var = self.resolve(name)?;
        let old = std::mem::replace(&mut var.value, VarValue::Unset);
        var.value = match old {
            VarValue::Assoc(mut pairs) => {
                if let Some((_, existing)) = pairs
                    .iter_mut()
                    .find(|(candidate, _)| candidate.as_slice() == key)
                {
                    *existing = value.to_vec();
                } else {
                    pairs.push((key.to_vec(), value.to_vec()));
                }
                VarValue::Assoc(pairs)
            }
            VarValue::Unset => VarValue::Assoc(vec![(key.to_vec(), value.to_vec())]),
            VarValue::Indexed(elements) => {
                // Put the array back before failing so the value is unchanged.
                var.value = VarValue::Indexed(elements);
                return Err(Error::NotAssociative {
                    name: name.to_vec(),
                });
            }
            VarValue::Str(element) => {
                var.value = VarValue::Str(element);
                return Err(Error::NotAssociative {
                    name: name.to_vec(),
                });
            }
        };
        Ok(())
    }

    /// `a[key]+=suffix`: append to one associative element.
    pub fn append_assoc(&mut self, name: &[u8], key: &[u8], suffix: &[u8]) -> Result<(), Error> {
        let current = self.element_assoc(name, key).unwrap_or_default().to_vec();
        let mut joined = current;
        joined.extend_from_slice(suffix);
        self.set_assoc(name, key, &joined)
    }

    /// `${a[index]}` for an indexed array (a scalar behaves like `[0]`).
    pub fn element_indexed(&self, name: &[u8], index: i64) -> Option<&[u8]> {
        let name = self.deref_name(name).ok()?;
        match &self.lookup(&name)?.value {
            VarValue::Indexed(elements) => {
                let index = resolve_read_index(elements, index)?;
                elements.get(&index).map(Vec::as_slice)
            }
            VarValue::Str(bytes) if index == 0 => Some(bytes),
            _ => None,
        }
    }

    /// `${a[key]}` for an associative array.
    pub fn element_assoc(&self, name: &[u8], key: &[u8]) -> Option<&[u8]> {
        let name = self.deref_name(name).ok()?;
        match &self.lookup(&name)?.value {
            VarValue::Assoc(pairs) => pairs
                .iter()
                .find(|(candidate, _)| candidate.as_slice() == key)
                .map(|(_, value)| value.as_slice()),
            _ => None,
        }
    }

    /// Number of elements in an array (a scalar counts as one, an unset name
    /// as zero).
    pub fn array_len(&self, name: &[u8]) -> usize {
        match self.value(name) {
            Some(VarValue::Indexed(elements)) => elements.len(),
            Some(VarValue::Assoc(pairs)) => pairs.len(),
            Some(VarValue::Str(_)) => 1,
            _ => 0,
        }
    }

    /// The greatest indexed subscript, if the variable is a non-empty indexed
    /// array. Appending starts one past it.
    pub fn max_index(&self, name: &[u8]) -> Option<i64> {
        match self.value(name) {
            Some(VarValue::Indexed(elements)) => elements.keys().next_back().copied(),
            _ => None,
        }
    }

    /// The array's shape, if it has one.
    pub fn shape(&self, name: &[u8]) -> Option<ArrayShape> {
        self.value(name).and_then(VarValue::shape)
    }

    /// Clear an array in place, keeping (or setting) its shape.
    pub fn clear_array(&mut self, name: &[u8], shape: ArrayShape) -> Result<(), Error> {
        let var = self.resolve(name)?;
        var.value = shape.empty();
        Ok(())
    }

    /// `unset 'a[@]'` / `unset 'a[*]'`: remove every element.
    ///
    /// Bash 5.3.20 distinguishes the shapes, verified directly:
    ///
    /// * an indexed array keeps its shape and becomes empty
    ///   (`declare -a a=()`), so a later `a[3]=x` is a plain element write;
    /// * an associative array is left untouched (the operand is not a key, and
    ///   unlike an indexed array it is not a request to clear);
    /// * an unset name succeeds without creating anything;
    /// * a scalar is an error (`unset: e: not an array variable`).
    pub fn unset_all_elements(&mut self, name: &[u8]) -> Result<(), Error> {
        match self.value(name) {
            None => Ok(()),
            Some(VarValue::Indexed(_)) => self.clear_array(name, ArrayShape::Indexed),
            Some(VarValue::Assoc(_)) => Ok(()),
            Some(VarValue::Str(_) | VarValue::Unset) => Err(Error::NotArray {
                name: name.to_vec(),
            }),
        }
    }

    /// Remove one indexed element. A negative index counts like a read; an
    /// index that lands before zero is an error, and removing an absent
    /// non-negative index succeeds (bash's `unset`).
    pub fn unset_element_indexed(&mut self, name: &[u8], index: i64) -> Result<(), Error> {
        let var = self.resolve(name)?;
        match &mut var.value {
            VarValue::Indexed(elements) => {
                let resolved =
                    resolve_read_index(elements, index).ok_or_else(|| Error::ArrayOutOfBounds {
                        name: name.to_vec(),
                        index,
                    })?;
                elements.remove(&resolved);
                Ok(())
            }
            VarValue::Str(_) | VarValue::Unset if index == 0 => {
                // `unset 's[0]'` unsets the scalar.
                var.value = VarValue::Unset;
                Ok(())
            }
            VarValue::Assoc(_) => Err(Error::NotIndexed {
                name: name.to_vec(),
            }),
            _ => Err(Error::ArrayOutOfBounds {
                name: name.to_vec(),
                index,
            }),
        }
    }

    /// Remove one associative element. Removing an absent key succeeds.
    pub fn unset_element_assoc(&mut self, name: &[u8], key: &[u8]) -> Result<(), Error> {
        let var = self.resolve(name)?;
        match &mut var.value {
            VarValue::Assoc(pairs) => {
                pairs.retain(|(candidate, _)| candidate.as_slice() != key);
                Ok(())
            }
            VarValue::Indexed(_) => Err(Error::NotAssociative {
                name: name.to_vec(),
            }),
            _ => Ok(()),
        }
    }

    /// Every visible variable, innermost definition winning, sorted by name.
    /// Arrays are included as values; printing decides how to show them.
    pub fn visible(&self) -> Vec<(&[u8], &Var)> {
        let mut out: Vec<(&[u8], &Var)> = Vec::new();
        for frame in self.frames.iter().rev() {
            for (name, var) in &frame.locals {
                if !out
                    .iter()
                    .any(|(candidate, _)| candidate.eq(&name.as_slice()))
                {
                    out.push((name, var));
                }
            }
        }
        for (name, var) in &self.global {
            if !out
                .iter()
                .any(|(candidate, _)| candidate.eq(&name.as_slice()))
            {
                out.push((name, var));
            }
        }
        out.sort_by(|a, b| a.0.cmp(b.0));
        out
    }

    /// The global scope only, sorted by name.
    ///
    /// `declare -g` and `declare -pg` list the global scope even when a local
    /// shadows the name, which `visible` cannot express.
    pub fn globals(&self) -> Vec<(&[u8], &Var)> {
        let mut out: Vec<(&[u8], &Var)> = self
            .global
            .iter()
            .map(|(name, var)| (name.as_slice(), var))
            .collect();
        out.sort_by(|a, b| a.0.cmp(b.0));
        out
    }

    /// The innermost frame's locals, sorted by name (`local`, `local -p`).
    pub fn locals(&self) -> Vec<(&[u8], &Var)> {
        let Some(frame) = self.frames.last() else {
            return Vec::new();
        };
        let mut out: Vec<(&[u8], &Var)> = frame
            .locals
            .iter()
            .map(|(name, var)| (name.as_slice(), var))
            .collect();
        out.sort_by(|a, b| a.0.cmp(b.0));
        out
    }

    /// A cell from the innermost frame only (`local -p name`).
    pub fn lookup_local(&self, name: &[u8]) -> Option<&Var> {
        self.frames
            .last()?
            .locals
            .iter()
            .find_map(|(candidate, var)| (candidate.as_slice() == name).then_some(var))
    }

    /// A global-scope cell by name, ignoring shadowing frames (`declare -g`).
    pub fn lookup_global(&self, name: &[u8]) -> Option<&Var> {
        self.global
            .iter()
            .find(|(candidate, _)| candidate.as_slice() == name)
            .map(|(_, var)| var)
    }

    /// Whether the variable a write to `name` would reach has the integer
    /// attribute (namerefs followed).
    pub fn target_has_integer(&self, name: &[u8]) -> bool {
        match self.resolve_chain(name) {
            Ok(Resolved::Var(target)) => self.lookup(&target).is_some_and(|var| var.attrs.integer),
            _ => false,
        }
    }

    /// Whether `name`'s nameref chain is circular (used to report a read that
    /// `lookup` cannot express as an error).
    pub fn is_nameref_cycle(&self, name: &[u8]) -> bool {
        matches!(self.resolve_chain(name), Err(Error::NamerefCycle { .. }))
    }

    /// Build the child environment: `NAME=VALUE` for exported variables only.
    ///
    /// Rules, all verified against bash 5.3.20:
    ///
    /// * a scalar exports its value, including the empty string;
    /// * an exported but unset variable exports nothing;
    /// * arrays export nothing (bash does not put `a=(1 2)` in the
    ///   environment);
    /// * export status is per scope entry: an unexported local shadow does not
    ///   hide an exported global value from children, while an exported local
    ///   replaces it.
    ///
    /// Encoding is bytes, never `String`.
    pub fn envp(&self) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        self.envp_into(&mut out);
        out
    }

    /// [`Vars::envp`] without the allocation, for the spawn path.
    pub fn envp_into(&self, out: &mut Vec<Vec<u8>>) {
        let mut pairs: Vec<(&[u8], Option<&[u8]>)> = Vec::new();
        for (name, var) in &self.global {
            record_export(&mut pairs, name, var);
        }
        for frame in &self.frames {
            for (name, var) in &frame.locals {
                record_export(&mut pairs, name, var);
            }
        }
        for (name, value) in pairs {
            let Some(value) = value else { continue };
            let mut entry = Vec::with_capacity(name.len() + 1 + value.len());
            entry.extend_from_slice(name);
            entry.push(b'=');
            entry.extend_from_slice(value);
            out.push(entry);
        }
    }

    /// Import one `NAME=VALUE` environment entry, marking it exported.
    /// Returns false (and imports nothing) if the entry has no `=` or no valid
    /// name.
    pub fn import_env_entry(&mut self, entry: &[u8]) -> bool {
        let Some(equals) = entry.iter().position(|&byte| byte == b'=') else {
            return false;
        };
        let (name, rest) = entry.split_at(equals);
        let value = &rest[1..];
        if !Self::is_name(name) {
            return false;
        }
        match self
            .global
            .iter_mut()
            .find(|(candidate, _)| candidate.as_slice() == name)
        {
            Some((_, var)) => {
                var.value = VarValue::Str(value.to_vec());
                var.attrs.exported = true;
            }
            None => self.global.push((
                name.to_vec(),
                Var {
                    value: VarValue::Str(value.to_vec()),
                    attrs: Attrs {
                        exported: true,
                        ..Attrs::default()
                    },
                },
            )),
        }
        true
    }

    /// Import an `environ`-style vector. Returns how many entries were valid.
    pub fn import_envp(&mut self, envp: &[Vec<u8>]) -> usize {
        envp.iter()
            .filter(|entry| self.import_env_entry(entry))
            .count()
    }

    /// Apply the `NAME=VALUE` assignments that precede a simple command
    /// (POSIX §2.9.1.4).
    ///
    /// This is the one place the builtin's tier changes runtime behavior, so
    /// it takes the [`Tier`] and cannot be got wrong by the VM:
    ///
    /// * [`Tier::Special`] — assignments persist in the shell. They are also
    ///   marked exported, matching `bash --posix` (`x=1 :; env` shows `x=1`).
    /// * every other tier — the assignments are scoped to the command through
    ///   a fresh frame, are exported for its duration, and disappear when the
    ///   caller invokes [`Vars::finish_prefix_assignments`].
    ///
    /// The returned `env` entries are always supplied, so the caller can add
    /// them to a child's environment regardless of persistence.
    pub fn apply_prefix_assignments(
        &mut self,
        assigns: &[(&[u8], &[u8])],
        tier: Tier,
    ) -> Result<PrefixAssignments, Error> {
        let mut env: Vec<Vec<u8>> = Vec::with_capacity(assigns.len());
        for (name, value) in assigns {
            let mut entry = Vec::with_capacity(name.len() + 1 + value.len());
            entry.extend_from_slice(name);
            entry.push(b'=');
            entry.extend_from_slice(value);
            // `x=1 x=2 cmd` must export one value, the last one, not a
            // duplicate entry that depends on `execve`'s tie-breaking.
            env.retain(|existing| {
                existing.len() <= name.len()
                    || &existing[..name.len()] != *name
                    || existing[name.len()] != b'='
            });
            env.push(entry);
        }
        if assigns.is_empty() {
            return Ok(PrefixAssignments {
                env,
                frame_depth: None,
            });
        }

        let persist = tier.assignment_persistence() == AssignPersistence::Persist;
        let frame_depth = if persist {
            None
        } else {
            // Store the depth *before* the push so `finish` can unwind to it.
            let depth = self.frames.len();
            self.push_local_frame();
            Some(depth)
        };

        for (name, value) in assigns {
            let scope = if persist {
                CreateScope::Default
            } else {
                CreateScope::Local
            };
            let result = (|| -> Result<(), Error> {
                // A temporary assignment shadows, but must still respect a
                // readonly variable it would hide: bash refuses
                // `readonly r; r=2 cmd` even inside a function.
                if !persist && self.is_readonly(name) {
                    return Err(Error::Readonly {
                        name: name.to_vec(),
                    });
                }
                self.assign_scalar_in(name, value, scope)?;
                self.mark_exported(name)?;
                Ok(())
            })();
            if let Err(error) = result {
                if let Some(depth) = frame_depth {
                    while self.frames.len() > depth {
                        // Cannot fail: we are only popping frames we pushed.
                        let _ = self.pop_frame();
                    }
                }
                return Err(error);
            }
        }

        Ok(PrefixAssignments { env, frame_depth })
    }

    /// Undo [`Vars::apply_prefix_assignments`]. Safe to call for a persisted
    /// application: it is a no-op then.
    pub fn finish_prefix_assignments(&mut self, applied: PrefixAssignments) -> Result<(), Error> {
        if let Some(depth) = applied.frame_depth {
            while self.frames.len() > depth {
                self.pop_frame()?;
            }
        }
        Ok(())
    }

    fn check_name(name: &[u8]) -> Result<(), Error> {
        if Self::is_name(name) {
            Ok(())
        } else {
            Err(Error::InvalidName {
                name: name.to_vec(),
            })
        }
    }

    /// Follow `nameref` chains and return the plain variable name at the end.
    ///
    /// This is the form used by operations that address a *cell* — element
    /// assignment (`ref[0]=x`), `unset_element_*` — and by [`Vars::resolve_in`].
    /// A chain that ends in an element reference (`declare -n r='a[2]'`) is not
    /// a name; operations that need one report [`Error::InvalidName`] with the
    /// whole target, which is bash's wording for `r[0]=x` over such a target.
    fn deref_name<'a>(&self, name: &'a [u8]) -> Result<Cow<'a, [u8]>, Error> {
        match self.resolve_chain(name)? {
            Resolved::Var(target) => Ok(if target == name {
                Cow::Borrowed(name)
            } else {
                Cow::Owned(target)
            }),
            Resolved::Indexed(base, index) => Err(Error::InvalidName {
                name: format!("{}[{index}]", display(&base)).into_bytes(),
            }),
            Resolved::Assoc(base, key) => Err(Error::InvalidName {
                name: {
                    let mut out = base;
                    out.push(b'[');
                    out.extend_from_slice(&key);
                    out.push(b']');
                    out
                },
            }),
            Resolved::Elements(base) => Err(Error::InvalidName {
                name: {
                    let mut out = base;
                    out.extend_from_slice(b"[@]");
                    out
                },
            }),
        }
    }

    /// Follow `nameref` chains to the operation's endpoint.
    ///
    /// A nameref cell's scalar value is a *reference*: either a plain name
    /// (`-n r=x`, chains allowed) or an element reference (`-n r='a[2]'`,
    /// `-n r='m[key]'`, `-n r='a[@]'`). Bash expands the reference's subscript
    /// again on every use, so this resolves it here — see
    /// [`expand_reference_text`] for the part of that expansion this crate can
    /// do (parameter references and escapes; command substitution inside a
    /// subscript is not available without the evaluator).
    ///
    /// The chain limit is [`NAMEREF_MAX_DEPTH`]; running out is
    /// [`Error::NamerefCycle`]. An empty nameref value is *not* a reference —
    /// it names nothing, so a write creates the target on demand and a read is
    /// empty, exactly like an unset nameref in bash.
    fn resolve_chain(&self, name: &[u8]) -> Result<Resolved, Error> {
        let mut current = name.to_vec();
        for _ in 0..NAMEREF_MAX_DEPTH {
            let Some(var) = self.lookup(&current) else {
                return Ok(Resolved::Var(current));
            };
            if !var.attrs.nameref {
                return Ok(Resolved::Var(current));
            }
            let Some(target) = var.value.as_str() else {
                return Ok(Resolved::Var(current));
            };
            if target.is_empty() {
                return Ok(Resolved::Var(current));
            }
            match parse_reference(target) {
                Some(Reference::Plain(base)) => {
                    if base == current {
                        return Err(Error::NamerefCycle { name: current });
                    }
                    current = base;
                }
                Some(Reference::Element { base, subscript }) => {
                    return self.resolve_element(&base, &subscript).map_err(|error| {
                        // A bad subscript inside a reference is reported against
                        // the reference itself, the way bash does.
                        match error {
                            Error::BadSubscript { .. } | Error::InvalidName { .. } => {
                                Error::InvalidName {
                                    name: target.to_vec(),
                                }
                            }
                            other => other,
                        }
                    });
                }
                Some(Reference::All(base)) => return Ok(Resolved::Elements(base)),
                // An invalid reference: bash refuses at `declare -n` time, so
                // reaching this means the cell predates that check. Treat the
                // cell as an ordinary variable rather than guessing.
                None => return Ok(Resolved::Var(current)),
            }
        }
        Err(Error::NamerefCycle {
            name: name.to_vec(),
        })
    }

    /// Resolve `base[subscript]` for a reference, using the base's current
    /// shape: an associative array treats the expanded subscript as a key, an
    /// indexed array (or a scalar) evaluates it arithmetically.
    fn resolve_element(&self, base: &[u8], subscript: &[u8]) -> Result<Resolved, Error> {
        let text = expand_reference_text(self, subscript);
        if text.is_empty() {
            return Err(Error::BadSubscript {
                name: base.to_vec(),
            });
        }
        if self.shape(base) == Some(ArrayShape::Associative) {
            return Ok(Resolved::Assoc(base.to_vec(), text));
        }
        match evaluate_subscript(self, &text) {
            Some(index) => Ok(Resolved::Indexed(base.to_vec(), index)),
            None => Err(Error::BadSubscript {
                name: base.to_vec(),
            }),
        }
    }

    fn lookup_mut(&mut self, name: &[u8]) -> Option<&mut Var> {
        let location = self.location(name)?;
        Some(self.var_at_mut(location))
    }

    fn location(&self, name: &[u8]) -> Option<Location> {
        for (frame, scope) in self.frames.iter().enumerate().rev() {
            if let Some(index) = scope
                .locals
                .iter()
                .position(|(candidate, _)| candidate.as_slice() == name)
            {
                return Some(Location::Frame { frame, index });
            }
        }
        self.global
            .iter()
            .position(|(candidate, _)| candidate.as_slice() == name)
            .map(Location::Global)
    }

    fn var_at(&self, location: Location) -> &Var {
        match location {
            Location::Global(index) => &self.global[index].1,
            Location::Frame { frame, index } => &self.frames[frame].locals[index].1,
        }
    }

    fn var_at_mut(&mut self, location: Location) -> &mut Var {
        match location {
            Location::Global(index) => &mut self.global[index].1,
            Location::Frame { frame, index } => &mut self.frames[frame].locals[index].1,
        }
    }

    fn cell_mut(
        &mut self,
        name: &[u8],
        scope: CreateScope,
        refuse_readonly: bool,
    ) -> Result<&mut Var, Error> {
        Self::check_name(name)?;
        match scope {
            CreateScope::Default => {
                if let Some(location) = self.location(name) {
                    let var = self.var_at_mut(location);
                    if refuse_readonly && var.attrs.readonly {
                        return Err(Error::Readonly {
                            name: name.to_vec(),
                        });
                    }
                    return Ok(var);
                }
                self.global.push((name.to_vec(), Var::default()));
                Ok(&mut self.global.last_mut().expect("just pushed a variable").1)
            }
            CreateScope::Global => {
                if let Some(index) = self
                    .global
                    .iter()
                    .position(|(candidate, _)| candidate.as_slice() == name)
                {
                    let var = &mut self.global[index].1;
                    if refuse_readonly && var.attrs.readonly {
                        return Err(Error::Readonly {
                            name: name.to_vec(),
                        });
                    }
                    return Ok(var);
                }
                self.global.push((name.to_vec(), Var::default()));
                Ok(&mut self.global.last_mut().expect("just pushed a variable").1)
            }
            CreateScope::Local => {
                let frame = self
                    .frames
                    .len()
                    .checked_sub(1)
                    .ok_or(Error::FrameUnderflow)?;
                let locals = &mut self.frames[frame].locals;
                if let Some(index) = locals
                    .iter()
                    .position(|(candidate, _)| candidate.as_slice() == name)
                {
                    let var = &mut locals[index].1;
                    if refuse_readonly && var.attrs.readonly {
                        return Err(Error::Readonly {
                            name: name.to_vec(),
                        });
                    }
                    return Ok(var);
                }
                locals.push((name.to_vec(), Var::default()));
                Ok(&mut locals.last_mut().expect("just pushed a local variable").1)
            }
        }
    }
}

/// The values returned by [`Vars::apply_prefix_assignments`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrefixAssignments {
    /// `NAME=VALUE` entries for the command's environment.
    pub env: Vec<Vec<u8>>,
    /// Frame depth to unwind to, or `None` when the assignments persisted.
    frame_depth: Option<usize>,
}

impl PrefixAssignments {
    /// Whether the caller must call [`Vars::finish_prefix_assignments`].
    pub fn needs_finish(&self) -> bool {
        self.frame_depth.is_some()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Location {
    Global(usize),
    Frame { frame: usize, index: usize },
}

fn set_scalar(var: &mut Var, name: &[u8], value: Vec<u8>) -> Result<(), Error> {
    match &mut var.value {
        // `A=text` on an associative array writes key `0`; bash keeps the
        // other keys, so a scalar assignment is not a conversion.
        VarValue::Assoc(pairs) => {
            if let Some((_, existing)) = pairs
                .iter_mut()
                .find(|(candidate, _)| candidate.as_slice() == b"0")
            {
                *existing = value;
            } else {
                pairs.insert(0, (b"0".to_vec(), value));
            }
            Ok(())
        }
        // A scalar assignment to an indexed array writes element 0 and keeps
        // the rest (`a=(1 2 3); a=x` leaves 1 and 2 in place).
        VarValue::Indexed(elements) => {
            elements.insert(0, value);
            Ok(())
        }
        _ => {
            let _ = name;
            var.value = VarValue::Str(value);
            Ok(())
        }
    }
}

/// Resolve a subscript for a write: negative counts from `greatest index + 1`.
///
/// The arithmetic wraps, because bash's subscripts are C `intmax_t`: after
/// `a[0x7FFFFFFFFFFFFFFF]=x`, `a[-1]` addresses `INTMAX_MAX` again (the
/// reference point overflows to `INTMAX_MIN`), and `a[-2]` addresses
/// `INTMAX_MAX - 1`. Saturating there would silently address a different
/// element. Ordinary arrays never come near the wrap.
fn resolve_write_index(
    elements: &BTreeMap<i64, Vec<u8>>,
    index: i64,
    name: &[u8],
) -> Result<i64, Error> {
    if index >= 0 {
        return Ok(index);
    }
    let length = elements
        .keys()
        .next_back()
        .map_or(0, |last| last.wrapping_add(1));
    let resolved = length.wrapping_add(index);
    if resolved < 0 {
        return Err(Error::ArrayOutOfBounds {
            name: name.to_vec(),
            index,
        });
    }
    Ok(resolved)
}

/// Resolve a subscript for a read; `None` when a negative index falls before
/// the start. The same wrapping rule as [`resolve_write_index`] applies.
fn resolve_read_index(elements: &BTreeMap<i64, Vec<u8>>, index: i64) -> Option<i64> {
    if index >= 0 {
        return Some(index);
    }
    let length = elements
        .keys()
        .next_back()
        .map_or(0, |last| last.wrapping_add(1));
    let resolved = length.wrapping_add(index);
    (resolved >= 0).then_some(resolved)
}

fn record_export<'a>(pairs: &mut Vec<(&'a [u8], Option<&'a [u8]>)>, name: &'a [u8], var: &'a Var) {
    if !var.attrs.exported {
        return;
    }
    let value = match &var.value {
        VarValue::Str(bytes) => Some(bytes.as_slice()),
        _ => None,
    };
    if let Some((_, slot)) = pairs.iter_mut().find(|(candidate, _)| candidate.eq(&name)) {
        *slot = value;
    } else {
        pairs.push((name, value));
    }
}

/// Where a nameref chain ends: the operation's real target.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Resolved {
    /// An ordinary variable cell.
    Var(Vec<u8>),
    /// One element of an indexed array (`-n r='a[3]'`).
    Indexed(Vec<u8>, i64),
    /// One element of an associative array (`-n r='m[key]'`).
    Assoc(Vec<u8>, Vec<u8>),
    /// Every element (`-n r='a[@]'`).
    Elements(Vec<u8>),
}

/// A parsed nameref value.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Reference {
    /// `x`: another variable.
    Plain(Vec<u8>),
    /// `a[subscript]`: one element.
    Element { base: Vec<u8>, subscript: Vec<u8> },
    /// `a[@]` / `a[*]`: the whole array.
    All(Vec<u8>),
}

/// Parse a nameref value, or `None` when it is not a usable reference.
///
/// Bash accepts a plain identifier or `identifier[subscript]`; anything else
/// (`1`, `#`, `@`, `a-b`, an empty string given as a value) is rejected when
/// the attribute is set. The subscript is kept as written: bash expands it
/// again on every use.
fn parse_reference(reference: &[u8]) -> Option<Reference> {
    let Some(open) = reference.iter().position(|&byte| byte == b'[') else {
        return Vars::is_name(reference).then(|| Reference::Plain(reference.to_vec()));
    };
    let base = &reference[..open];
    if !Vars::is_name(base) || reference.last() != Some(&b']') {
        return None;
    }
    let subscript = &reference[open + 1..reference.len() - 1];
    // `a[@]`/`a[*]` is a whole-array reference; a quoted key or an arithmetic
    // expression is an element. An unquoted `]` inside would have made the
    // scan above ambiguous, so require balanced brackets.
    let mut depth = 1usize;
    let mut quote: Option<u8> = None;
    let mut escaped = false;
    for &byte in subscript {
        if escaped {
            escaped = false;
            continue;
        }
        match byte {
            b'\\' => escaped = true,
            b'\'' | b'"' if quote == Some(byte) => quote = None,
            b'\'' | b'"' if quote.is_none() => quote = Some(byte),
            b'[' if quote.is_none() => depth += 1,
            b']' if quote.is_none() => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return None;
                }
            }
            _ => {}
        }
    }
    if quote.is_some() || depth != 1 {
        return None;
    }
    if subscript == b"@" || subscript == b"*" {
        return Some(Reference::All(base.to_vec()));
    }
    Some(Reference::Element {
        base: base.to_vec(),
        subscript: subscript.to_vec(),
    })
}

/// `name[@]`, for an error message about a whole-array reference.
fn whole_array_name(mut base: Vec<u8>) -> Vec<u8> {
    base.extend_from_slice(b"[@]");
    base
}

/// Expand a reference subscript the way bash does before interpreting it.
///
/// Verified against bash 5.3.20: a nameref value keeps the *unexpanded*
/// subscript, and every use expands it again, so `key=K; declare -n
/// r='A[$key]'` follows `key` when it changes. This covers the expansion that
/// does not need the evaluator — `$name`, `${name}`, `${name[i]}`, backslash
/// escapes and quote removal. Command substitution inside a subscript is not
/// available here (it needs the evaluator); `bsh` therefore resolves it only
/// if the declaration itself already expanded it.
fn expand_reference_text(vars: &Vars, text: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len());
    let mut index = 0;
    while index < text.len() {
        match text[index] {
            b'\\' if index + 1 < text.len() => {
                out.push(text[index + 1]);
                index += 2;
            }
            b'\'' => {
                // Single quotes: literal until the closing quote.
                index += 1;
                while index < text.len() && text[index] != b'\'' {
                    out.push(text[index]);
                    index += 1;
                }
                index += 1;
            }
            b'"' => index += 1,
            b'$' => {
                let (value, next) = expand_dollar(vars, text, index);
                out.extend_from_slice(&value);
                index = next;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    out
}

/// Expand one `$…` reference starting at `text[start]`, returning its value and
/// the index just past it. An unrecognised `$` is literal.
fn expand_dollar(vars: &Vars, text: &[u8], start: usize) -> (Vec<u8>, usize) {
    let rest = &text[start + 1..];
    if rest.is_empty() {
        return (vec![b'$'], start + 1);
    }
    if rest[0] == b'{' {
        let Some(close) = rest.iter().position(|&byte| byte == b'}') else {
            return (vec![b'$'], start + 1);
        };
        let inner = &rest[1..close];
        return (expand_braced(vars, inner), start + 2 + close);
    }
    if rest[0] == b'(' {
        // Command substitution: leave it as written; the caller's arithmetic
        // will refuse it rather than silently guessing.
        return (text[start..].to_vec(), text.len());
    }
    if Vars::is_name(&rest[..1]) {
        let mut end = 1;
        while end < rest.len() && (rest[end] == b'_' || rest[end].is_ascii_alphanumeric()) {
            end += 1;
        }
        return (
            vars.scalar(&rest[..end]).unwrap_or_default().to_vec(),
            start + 1 + end,
        );
    }
    (vec![b'$'], start + 1)
}

/// `${…}` inside a reference subscript: only the simple parameter forms are
/// meaningful there, so anything else is kept literally.
fn expand_braced(vars: &Vars, inner: &[u8]) -> Vec<u8> {
    if let Some(open) = inner.iter().position(|&byte| byte == b'[')
        && inner.last() == Some(&b']')
    {
        let base = &inner[..open];
        let subscript = &inner[open + 1..inner.len() - 1];
        if Vars::is_name(base) {
            let subscript = expand_reference_text(vars, subscript);
            if let Some(index) = parse_integer(&subscript) {
                if let Some(value) = vars.element_indexed(base, index) {
                    return value.to_vec();
                }
            }
            if let Some(value) = vars.element_assoc(base, &subscript) {
                return value.to_vec();
            }
            return Vec::new();
        }
    }
    if Vars::is_name(inner) {
        return vars.scalar(inner).unwrap_or_default().to_vec();
    }
    let mut out = vec![b'$', b'{'];
    out.extend_from_slice(inner);
    out.push(b'}');
    out
}

/// Evaluate a nameref subscript as arithmetic, for an indexed array.
///
/// This is the subset of bash arithmetic a subscript needs in practice:
/// decimal integers, variables holding integers, `+ - * / %`, parentheses and
/// whitespace. Anything else (ranges, `**`, base prefixes, side effects)
/// returns `None`, and the read is empty rather than wrong.
fn evaluate_subscript(vars: &Vars, text: &[u8]) -> Option<i64> {
    let mut parser = SubscriptParser {
        vars,
        text,
        index: 0,
    };
    let value = parser.expression()?;
    parser.skip_space();
    (parser.index == parser.text.len()).then_some(value)
}

/// A tiny recursive-descent evaluator for reference subscripts.
struct SubscriptParser<'a> {
    vars: &'a Vars,
    text: &'a [u8],
    index: usize,
}

impl SubscriptParser<'_> {
    fn skip_space(&mut self) {
        while self.index < self.text.len() && self.text[self.index].is_ascii_whitespace() {
            self.index += 1;
        }
    }

    fn expression(&mut self) -> Option<i64> {
        let mut value = self.term()?;
        loop {
            self.skip_space();
            match self.text.get(self.index) {
                Some(b'+') => {
                    self.index += 1;
                    value = value.wrapping_add(self.term()?);
                }
                Some(b'-') => {
                    self.index += 1;
                    value = value.wrapping_sub(self.term()?);
                }
                _ => return Some(value),
            }
        }
    }

    fn term(&mut self) -> Option<i64> {
        let mut value = self.unary()?;
        loop {
            self.skip_space();
            match self.text.get(self.index) {
                Some(b'*') => {
                    self.index += 1;
                    value = value.wrapping_mul(self.unary()?);
                }
                Some(b'/') => {
                    self.index += 1;
                    let divisor = self.unary()?;
                    if divisor == 0 {
                        return None;
                    }
                    value = value.wrapping_div(divisor);
                }
                Some(b'%') => {
                    self.index += 1;
                    let divisor = self.unary()?;
                    if divisor == 0 {
                        return None;
                    }
                    value = value.wrapping_rem(divisor);
                }
                _ => return Some(value),
            }
        }
    }

    fn unary(&mut self) -> Option<i64> {
        self.skip_space();
        match self.text.get(self.index) {
            Some(b'-') => {
                self.index += 1;
                Some(self.unary()?.wrapping_neg())
            }
            Some(b'+') => {
                self.index += 1;
                self.unary()
            }
            _ => self.primary(),
        }
    }

    fn primary(&mut self) -> Option<i64> {
        self.skip_space();
        match self.text.get(self.index)? {
            b'(' => {
                self.index += 1;
                let value = self.expression()?;
                self.skip_space();
                if self.text.get(self.index) != Some(&b')') {
                    return None;
                }
                self.index += 1;
                Some(value)
            }
            byte if byte.is_ascii_digit() => {
                let start = self.index;
                while self.text.get(self.index).is_some_and(u8::is_ascii_digit) {
                    self.index += 1;
                }
                std::str::from_utf8(&self.text[start..self.index])
                    .ok()?
                    .parse()
                    .ok()
            }
            byte if *byte == b'_' || byte.is_ascii_alphabetic() => {
                let start = self.index;
                while self
                    .text
                    .get(self.index)
                    .is_some_and(|byte| *byte == b'_' || byte.is_ascii_alphanumeric())
                {
                    self.index += 1;
                }
                parse_integer(self.vars.scalar(&self.text[start..self.index])?)
            }
            _ => None,
        }
    }
}

/// Parse a decimal integer with bash's surrounding-whitespace tolerance.
fn parse_integer(bytes: &[u8]) -> Option<i64> {
    let bytes = bytes.strip_prefix(b"+").unwrap_or(bytes);
    let text = std::str::from_utf8(bytes).ok()?;
    let text = text.trim();
    if text.is_empty() {
        return Some(0);
    }
    text.parse().ok()
}
