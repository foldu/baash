//! The variable source expansion reads and (for `${x:=w}`) writes.

use std::collections::BTreeMap;

/// Everything expansion needs to know about shell variables.
///
/// `baash-runtime` implements this over its scope stack. Keeping it this small
/// means the word model and the expansion pipeline never see the runtime, and
/// tests can use a `HashMap`.
///
/// # What the implementation must provide
///
/// * Every ordinary variable, addressed by its name bytes.
/// * The special parameters the runtime owns: `?`, `-`, `$`, `!` and `0`.
///   They are ordinary [`Vars::get`] lookups; expansion never special-cases
///   them.
/// * `IFS`, so field splitting can see whether it is unset (`None`) or set to
///   an empty string (`Some(b"")`) — the two behave differently (POSIX XCU
///   2.5.3).
/// * The positional parameters through [`Vars::positional`]. `$#` is computed
///   from the length; `$1` is index 0, and so on.
///
/// Values are bytes, never `str`: filenames are arbitrary non-NUL bytes and
/// `$'\xff'` exists (DESIGN.md §4.2).
pub trait Vars {
    /// Look up a variable. `None` means *unset*; `Some(b"")` means set to the
    /// empty string. The difference matters for `${x-word}` versus
    /// `${x:-word}`.
    fn get(&self, name: &[u8]) -> Option<&[u8]>;

    /// The positional parameters, starting at `$1`.
    fn positional(&self) -> &[Vec<u8>];

    /// Whether `set -u` (nounset) is in effect.
    ///
    /// Expansion raises [`crate::ErrorKind::ParameterUnset`] instead of
    /// substituting an empty value when this is true and a parameter the
    /// operator requires is unset. Defaults to `false`, which is what a
    /// read-only view (completion, the linter) wants.
    fn nounset(&self) -> bool {
        false
    }

    /// Whether the shell is in a UTF-8 locale, so `${#x}`, `${x:o:l}` and `?`
    /// count characters rather than bytes.
    ///
    /// Defaults to `false` — the C locale — because that is what a shell with
    /// no locale model must assume, and because a read-only view should not
    /// silently disagree with the shell that built it.
    fn utf8(&self) -> bool {
        false
    }

    /// Assign a variable, for `${x:=word}` and `${x=word}`.
    ///
    /// Read-only variables and invalid names are reported with [`SetError`];
    /// expansion turns that into a diagnostic. The default implementation
    /// refuses every assignment, which is the right behaviour for a
    /// read-only view of the environment (e.g. completion).
    fn set(&mut self, name: &[u8], value: Vec<u8>) -> Result<(), SetError> {
        let _ = (name, value);
        Err(SetError::ReadOnly)
    }

    /// The array stored in `name`, if it has array shape.
    ///
    /// `None` means the variable is a scalar, is unset, or is not an array at
    /// all. That distinction matters for subscripts: an indexed array's
    /// subscript is arithmetic, while an associative one's is a literal key,
    /// and a scalar behaves like a one-element indexed array for reads
    /// (`${s[0]}` is `$s`, `${s[1]}` is empty).
    ///
    /// The storage is borrowed, not copied: expansion reads elements and
    /// lengths straight from the scope stack. The default returns `None`,
    /// which is correct for a variable source with no arrays (completion, the
    /// passthrough stub).
    fn array(&self, name: &[u8]) -> Option<ArrayValue<'_>> {
        let _ = name;
        None
    }

    /// Every visible variable name, for `${!prefix@}` and `${!prefix*}`.
    ///
    /// The caller sorts and filters; the default is empty, which is the right
    /// answer for a read-only view that has no name inventory (completion) and
    /// for the passthrough stub.
    fn names(&self) -> Vec<Vec<u8>> {
        Vec::new()
    }

    /// The letters `${x@a}` reports for `name`: `a` indexed array, `A`
    /// associative array, `x` exported, `r` readonly, `i` integer, `n`
    /// nameref. Sorted, and empty for a name with no attributes or no value.
    fn attributes(&self, name: &[u8]) -> Vec<u8> {
        let _ = name;
        Vec::new()
    }
}

/// Why [`Vars::set`] refused an assignment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetError {
    /// The variable exists but is read-only, or the source does not support
    /// assignment at all.
    ReadOnly,
    /// The name cannot be assigned (for example `$1` or `${x?}`).
    InvalidName,
}

/// A borrowed view of one array.
///
/// Indexed arrays are *sparse*: the map is keyed by the subscript, so
/// `a[100]=x` costs one entry and `unset 'a[0]'` leaves no trace. Iteration
/// order is index order, which is what `${a[@]}` and `${!a[@]}` promise.
///
/// The index type is `i64`, not `u32`, because bash subscripts are machine
/// integers: the suite exercises `a[0x7FFFFFFFFFFFFFFF]`, and a narrower key
/// would silently alias it with a neighbour. Negative subscripts are resolved
/// before storage, so no key here is negative.
#[derive(Clone, Copy, Debug)]
pub enum ArrayValue<'a> {
    /// An indexed array: subscript → element, in subscript order.
    Indexed(&'a BTreeMap<i64, Vec<u8>>),
    /// An associative array, in insertion order (bash's order).
    Assoc(&'a [(Vec<u8>, Vec<u8>)]),
}

impl<'a> ArrayValue<'a> {
    /// Which kind of array this is.
    pub fn kind(&self) -> ArrayKind {
        match self {
            ArrayValue::Indexed(_) => ArrayKind::Indexed,
            ArrayValue::Assoc(_) => ArrayKind::Associative,
        }
    }

    /// Number of elements (holes are not elements).
    pub fn len(&self) -> usize {
        match self {
            ArrayValue::Indexed(elements) => elements.len(),
            ArrayValue::Assoc(pairs) => pairs.len(),
        }
    }

    /// Whether the array has no elements.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The elements in iteration order, as borrowed slices.
    pub fn values(&self) -> Vec<&[u8]> {
        match self {
            ArrayValue::Indexed(elements) => elements.values().map(Vec::as_slice).collect(),
            ArrayValue::Assoc(pairs) => pairs.iter().map(|(_, value)| value.as_slice()).collect(),
        }
    }

    /// The subscripts in iteration order: decimal index text for an indexed
    /// array, the keys themselves for an associative one.
    pub fn indices(&self) -> Vec<Vec<u8>> {
        match self {
            ArrayValue::Indexed(elements) => elements
                .keys()
                .map(|index| index.to_string().into_bytes())
                .collect(),
            ArrayValue::Assoc(pairs) => pairs.iter().map(|(key, _)| key.clone()).collect(),
        }
    }

    /// One element: an index for an indexed array, a key for an associative
    /// one. A negative indexed subscript counts from the end, where "the end"
    /// is bash's `greatest index + 1` — *not* the number of elements, which
    /// is what makes `${a[-2]}` with holes behave the way bash does.
    pub fn get(&self, subscript: &[u8]) -> Option<&'a [u8]> {
        match self {
            ArrayValue::Indexed(elements) => {
                let index = resolve_index(elements, subscript)?;
                elements.get(&index).map(Vec::as_slice)
            }
            ArrayValue::Assoc(pairs) => pairs
                .iter()
                .find(|(key, _)| key.as_slice() == subscript)
                .map(|(_, value)| value.as_slice()),
        }
    }
}

/// `greatest index + 1`, bash's reference point for a negative subscript.
fn resolve_index(elements: &BTreeMap<i64, Vec<u8>>, subscript: &[u8]) -> Option<i64> {
    let index = parse_index(subscript)?;
    if index >= 0 {
        return Some(index);
    }
    let last = *elements.keys().next_back()?;
    let length = last.saturating_add(1);
    let resolved = length.checked_add(index)?;
    (resolved >= 0).then_some(resolved)
}

/// Parse a decimal or C-style integer subscript. `${a[08]}` is octal in bash's
/// arithmetic, but a *stored* key arriving as text (from `${!a[@]}` or an
/// `unset` operand) is decimal; arithmetic evaluation happens before this in
/// the expansion layer, which is where bash's literal rules live.
fn parse_index(subscript: &[u8]) -> Option<i64> {
    let text = std::str::from_utf8(subscript).ok()?.trim();
    let (negative, digits) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let value: i64 = digits.parse().ok()?;
    Some(if negative { -value } else { value })
}

/// The two array shapes bash distinguishes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArrayKind {
    /// `declare -a`, numeric subscripts.
    Indexed,
    /// `declare -A`, string keys.
    Associative,
}
