//! A minimal byte-oriented glob: `*`, `?`, `[...]`, nothing else.
//!
//! **Placeholder.** DESIGN.md §4.8 calls for *one* byte-oriented pattern
//! compiler shared by `case`, pathnames, `${x#pat}`, `${x%pat}`,
//! `${x/pat/rep}`, `[[ x == pat ]]` and completion, with `extglob` as a flag
//! and a backtracking budget. This module exists so wave 2 can expand
//! pathnames at all; delete it, do not extend it, when that engine lands.
//! Absent on purpose: `extglob`, `globstar`, braces, `dotglob`, `nocaseglob`,
//! character classes. `nullglob`/`failglob` are the caller's job: they are
//! shell options, not pattern semantics. Quoting is per byte.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// A pattern: bytes plus an optional per-byte quoting mask (`None` = nothing
/// quoted, the shape `case` and `[[ ]]` use).
#[derive(Clone, Copy, Debug)]
pub struct Pattern<'a> {
    /// Pattern bytes.
    pub bytes: &'a [u8],
    /// `quoted[i]` marks `bytes[i]` literal.
    pub quoted: Option<&'a [bool]>,
}

impl<'a> Pattern<'a> {
    /// A pattern with an explicit mask.
    pub fn new(bytes: &'a [u8], quoted: Option<&'a [bool]>) -> Self {
        Self { bytes, quoted }
    }

    fn is_meta(&self, i: usize) -> bool {
        let quoted = self.quoted.is_some_and(|mask| mask[i]);
        !quoted
            && self
                .bytes
                .get(i)
                .is_some_and(|b| matches!(b, b'*' | b'?' | b'['))
    }

    /// Whether any glob metacharacter is active.
    pub fn has_meta(&self) -> bool {
        (0..self.bytes.len()).any(|i| self.is_meta(i))
    }
}

/// Match all of `text` against `pattern`, with no pathname semantics (`*`
/// matches `/` too — what `case` and `[[ ]]` need).
pub fn matches(p: &Pattern<'_>, text: &[u8]) -> bool {
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut star_text) = (None, 0usize);
    while ti < text.len() {
        if p.bytes.get(pi) == Some(&b'*') && p.is_meta(pi) {
            star = Some(pi);
            star_text = ti;
            pi += 1;
            continue;
        }
        match step(p, pi, text[ti]) {
            Some(next) => {
                pi = next;
                ti += 1;
            }
            None => match star {
                Some(s) => {
                    star_text += 1;
                    ti = star_text;
                    pi = s + 1;
                }
                None => return false,
            },
        }
    }
    while p.bytes.get(pi) == Some(&b'*') && p.is_meta(pi) {
        pi += 1;
    }
    pi == p.bytes.len()
}

/// One pattern element (literal, `?` or `[...]`) against one byte.
fn step(p: &Pattern<'_>, pi: usize, byte: u8) -> Option<usize> {
    let pattern = *p.bytes.get(pi)?;
    if p.is_meta(pi) {
        match pattern {
            b'?' => return Some(pi + 1),
            b'[' => {
                if let Some((end, hit)) = bracket(p, pi, byte) {
                    return hit.then_some(end);
                }
            }
            _ => {}
        }
    }
    (pattern == byte).then_some(pi + 1)
}

/// Parse `[...]` at `start`; `None` when unterminated, so `[` is literal.
fn bracket(p: &Pattern<'_>, start: usize, byte: u8) -> Option<(usize, bool)> {
    let quoted = |at: usize| p.quoted.is_some_and(|mask| mask[at]);
    let mut i = start + 1;
    let mut negate = false;
    if i < p.bytes.len() && !quoted(i) && matches!(p.bytes[i], b'!' | b'^') {
        negate = true;
        i += 1;
    }
    let (mut hit, mut first) = (false, true);
    while i < p.bytes.len() {
        if p.bytes[i] == b']' && !first && !quoted(i) {
            return Some((i + 1, hit != negate));
        }
        let ranged = i + 2 < p.bytes.len() && p.bytes[i + 1] == b'-' && !quoted(i + 1);
        if ranged && p.bytes[i + 2] != b']' {
            hit |= p.bytes[i] <= byte && byte <= p.bytes[i + 2];
            i += 3;
        } else {
            hit |= p.bytes[i] == byte;
            i += 1;
        }
        first = false;
    }
    None
}

/// Pathname expansion: sorted matches, or `None` when nothing matched (POSIX's
/// default, and `bsh_lang::Shell::pathname`'s contract).
pub fn expand(p: &Pattern<'_>) -> Option<Vec<Vec<u8>>> {
    if !p.has_meta() {
        return None;
    }
    let absolute = p.bytes.first() == Some(&b'/');
    let mut current: Vec<Vec<u8>> = vec![if absolute { b"/".to_vec() } else { Vec::new() }];
    let mut start = usize::from(absolute);
    while start <= p.bytes.len() {
        let end = p.bytes[start..]
            .iter()
            .position(|&b| b == b'/')
            .map_or(p.bytes.len(), |at| start + at);
        let part = Pattern::new(&p.bytes[start..end], p.quoted.map(|m| &m[start..end]));
        let mut next = Vec::new();
        for base in &current {
            if !part.has_meta() {
                next.extend(join(base, part.bytes).filter(|p| Path::new(os(p)).exists()));
                continue;
            }
            let dir: &[u8] = if base.is_empty() { b"." } else { base };
            let leading_dot = part.bytes.first() == Some(&b'.');
            let entries = std::fs::read_dir(Path::new(os(dir))).ok();
            for name in entries
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.file_name())
            {
                let name = name.as_bytes();
                let hidden = name.starts_with(b".") && !leading_dot;
                if !hidden && matches(&part, name) {
                    next.extend(join(base, name));
                }
            }
        }
        current = next;
        start = end + 1;
    }
    current.sort();
    (!current.is_empty()).then_some(current)
}

/// Join a prefix and a name, keeping `./` and the root slash as written so
/// `./*` expands to `./a`, not `a`. `None` on an embedded NUL.
fn join(base: &[u8], name: &[u8]) -> Option<Vec<u8>> {
    if base.iter().chain(name).any(|&b| b == 0) {
        return None;
    }
    let mut out = base.to_vec();
    if !out.is_empty() && !out.ends_with(b"/") {
        out.push(b'/');
    }
    out.extend_from_slice(name);
    Some(out)
}

fn os(bytes: &[u8]) -> &OsStr {
    OsStr::from_bytes(bytes)
}
