//! Variables, scopes, positional parameters and the environment.
//!
//! The store is a scope stack: one global scope, then zero or more frames
//! pushed by function calls or by `local`/`declare` (DESIGN.md §4.6). Lookup
//! walks inward-out: the innermost frame that defines a name wins, then the
//! global scope.
//!
//! Writes go through [`Vars::resolve`], the single write entry point. Reads go
//! through [`Vars::lookup`]/[`Vars::scalar`], so expanding `${unset}` never
//! creates a variable as a side effect. `nameref` is the one extension point
//! inside `resolve`; it currently always answers "not a nameref".

use crate::Status;
use crate::builtins::{AssignPersistence, Tier};
use crate::errors::Error;
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
    pub fn value(&self, name: &[u8]) -> Option<&VarValue> {
        self.lookup(name).map(|var| &var.value)
    }

    /// The scalar bytes of a variable, if it exists and is a string.
    pub fn scalar(&self, name: &[u8]) -> Option<&[u8]> {
        self.lookup(name).and_then(Var::scalar)
    }

    /// Whether the variable exists (even if it is `Unset`).
    pub fn contains(&self, name: &[u8]) -> bool {
        self.lookup(name).is_some()
    }

    /// Whether the variable exists and is set.
    pub fn is_set(&self, name: &[u8]) -> bool {
        self.lookup(name).is_some_and(|var| var.value.is_set())
    }

    /// Whether the variable is readonly.
    pub fn is_readonly(&self, name: &[u8]) -> bool {
        self.lookup(name).is_some_and(|var| var.attrs.readonly)
    }

    /// Whether the variable is exported.
    pub fn is_exported(&self, name: &[u8]) -> bool {
        self.lookup(name).is_some_and(|var| var.attrs.exported)
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
    /// this is where `nameref` will land. Reads use [`Vars::lookup`].
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
        self.cell_mut(&target, scope, true)
    }

    /// Assign a complete value.
    ///
    /// A scalar assignment to an indexed array sets element 0 and keeps the
    /// array shape; a scalar assignment to an associative array is an error
    /// (bash: "must use subscript when assigning associative array").
    pub fn assign(&mut self, name: &[u8], value: VarValue) -> Result<(), Error> {
        let var = self.resolve(name)?;
        match value {
            VarValue::Str(bytes) => set_scalar(var, name, bytes),
            other => {
                var.value = other;
                Ok(())
            }
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
        let var = self.resolve_in(name, scope)?;
        set_scalar(var, name, value.to_vec())
    }

    /// `x+=suffix`: append to a scalar. Appending to an array requires the
    /// array operation, because the meaning depends on the shape
    /// (`a+=(x)` appends an element; `a+=x` is not the same thing).
    pub fn append(&mut self, name: &[u8], suffix: &[u8]) -> Result<(), Error> {
        let var = self.resolve(name)?;
        match &mut var.value {
            VarValue::Str(bytes) => {
                bytes.extend_from_slice(suffix);
                Ok(())
            }
            // `a+=text` appends to element zero; bash does not treat it as
            // "append an element" (that spelling is `a+=(…)`).
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

    /// Mark a variable exported, creating it in the global scope if it does
    /// not exist.
    pub fn mark_exported(&mut self, name: &[u8]) -> Result<(), Error> {
        let target = self.deref_name(name)?;
        self.cell_mut(&target, CreateScope::Default, false)?
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
        let target = self.deref_name(name)?;
        self.cell_mut(&target, CreateScope::Default, false)?
            .attrs
            .readonly = true;
        Ok(())
    }

    /// Mark a variable integer (`declare -i`).
    pub fn mark_integer(&mut self, name: &[u8]) -> Result<(), Error> {
        let target = self.deref_name(name)?;
        self.cell_mut(&target, CreateScope::Default, false)?
            .attrs
            .integer = true;
        Ok(())
    }

    /// Unset a variable (`unset name`).
    ///
    /// The variable is removed from the scope where it is found; a local
    /// removal reveals the outer variable, which is bash's behavior. Unsetting
    /// a readonly variable is an error. Unsetting a name that does not exist
    /// succeeds.
    pub fn unset(&mut self, name: &[u8]) -> Result<(), Error> {
        Self::check_name(name)?;
        let target = self.deref_name(name)?.into_owned();
        let Some(location) = self.location(&target) else {
            return Ok(());
        };
        if self.var_at(location).attrs.readonly {
            return Err(Error::Readonly { name: target });
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
    pub fn set_assoc(&mut self, name: &[u8], key: &[u8], value: &[u8]) -> Result<(), Error> {
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
        match &self.lookup(name)?.value {
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
        match &self.lookup(name)?.value {
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

    /// Follow `nameref` chains. The hook currently never answers, so this is
    /// a borrow-free identity function; it exists so `declare -n` is a lookup
    /// flag later rather than a refactor (DESIGN.md §3.4, §4.6).
    fn deref_name<'a>(&self, name: &'a [u8]) -> Result<Cow<'a, [u8]>, Error> {
        let mut current: Cow<'a, [u8]> = Cow::Borrowed(name);
        for _ in 0..NAMEREF_MAX_DEPTH {
            match self.nameref_target(&current) {
                Some(target) => current = Cow::Owned(target),
                None => return Ok(current),
            }
        }
        Err(Error::NamerefCycle {
            name: name.to_vec(),
        })
    }

    /// Extension point for bash `nameref` (`declare -n`). Returns the variable
    /// name this cell refers to, or `None` when `name` is not a nameref.
    ///
    /// Namerefs are deferred (DESIGN.md §3.4); when implemented this reads the
    /// cell's scalar value and requires a `nameref` attribute, with the chain
    /// limit enforced by [`Vars::deref_name`].
    fn nameref_target(&self, name: &[u8]) -> Option<Vec<u8>> {
        let _ = name;
        None
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
        .map_or(0, |last| last.saturating_add(1));
    let resolved = length.saturating_add(index);
    if resolved < 0 {
        return Err(Error::ArrayOutOfBounds {
            name: name.to_vec(),
            index,
        });
    }
    Ok(resolved)
}

/// Resolve a subscript for a read; `None` when a negative index falls before
/// the start.
fn resolve_read_index(elements: &BTreeMap<i64, Vec<u8>>, index: i64) -> Option<i64> {
    if index >= 0 {
        return Some(index);
    }
    let length = elements
        .keys()
        .next_back()
        .map_or(0, |last| last.saturating_add(1));
    let resolved = length.saturating_add(index);
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
