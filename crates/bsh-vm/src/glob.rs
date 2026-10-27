//! Pathname expansion over the shared matcher.
//!
//! `bsh-pattern` is the one pattern engine (`docs/contracts.md` §bsh-pattern):
//! this module compiles one [`bsh_pattern::Pattern`] per path component with
//! `pathname: true` and matches each directory entry against it. Everything
//! that is a *policy* rather than pattern semantics stays here or in the
//! caller:
//!
//! * Dotfiles: a leading `.` must be written literally unless `dotglob` is on,
//!   and `shopt -u globskipdots` adds `.`/`..` to a pattern that starts with
//!   a literal dot.
//! * `globstar`: a whole `**` component matches zero or more directories.
//!   `d**` is an ordinary `*`-like component, as in bash.
//! * Sorting: results are byte-sorted (the C locale). `nullglob`/`failglob`
//!   and "no match leaves the word alone" are the caller's, in
//!   `Interp::pathname`.
//!
//! `extglob` and `nocaseglob` come in through [`Options::flags`]; `pathname`
//! is forced on so `*`, `?` and brackets never match a `/`.
//!
//! Quoting has no mask in `bsh-pattern`: [`crate::pattern::escaped`] turns
//! quoted bytes into backslash escapes before anything is compiled.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use bsh_pattern::{Flags, Pattern};

/// What the shell options say about pathname expansion.
#[derive(Clone, Copy, Debug)]
pub struct Options {
    /// The pattern dialect (`extglob`) and case folding (`nocaseglob` set as
    /// `nocasematch`). `pathname` is forced on by [`expand`].
    pub flags: Flags,
    /// `shopt -s dotglob`: `*` also matches a leading dot.
    pub dotglob: bool,
    /// `shopt -s globstar`: a whole `**` component matches zero or more
    /// directories.
    pub globstar: bool,
    /// `shopt -s globskipdots`: `.*` omits `.` and `..` (the bash 5.2+
    /// default).
    pub globskipdots: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            flags: Flags::default(),
            dotglob: false,
            globstar: false,
            globskipdots: true,
        }
    }
}

/// Whether the word contains an active glob metacharacter.
///
/// With `extglob`, the five operators (`?(`…`) also count, so a value that
/// comes from a variable can be expanded as an extended glob.
pub fn has_meta(bytes: &[u8], quoted: &[bool], extglob: bool) -> bool {
    for (index, &byte) in bytes.iter().enumerate() {
        if quoted.get(index).copied().unwrap_or(true) {
            continue;
        }
        if matches!(byte, b'*' | b'?' | b'[') {
            return true;
        }
        if extglob
            && matches!(byte, b'?' | b'*' | b'+' | b'@' | b'!')
            && bytes.get(index + 1) == Some(&b'(')
            && !quoted.get(index + 1).copied().unwrap_or(true)
        {
            return true;
        }
    }
    false
}

/// Pathname expansion: sorted matches, or `None` when nothing matched (POSIX's
/// default, and `bsh_lang::Shell::pathname`'s contract).
pub fn expand(bytes: &[u8], quoted: &[bool], options: &Options) -> Option<Vec<Vec<u8>>> {
    let options = Options {
        flags: Flags {
            pathname: true,
            ..options.flags
        },
        ..*options
    };
    let components = components(bytes, quoted);
    let absolute = bytes.first() == Some(&b'/');
    let start = if absolute { b"/".to_vec() } else { Vec::new() };
    let mut out = Vec::new();
    walk(&start, &components, &options, &mut out);
    out.sort();
    out.dedup();
    (!out.is_empty()).then_some(out)
}

/// One path component: bytes plus the matching slice of the quoting mask.
#[derive(Clone, Copy)]
struct Component<'a> {
    bytes: &'a [u8],
    quoted: &'a [bool],
}

impl Component<'_> {
    /// A whole `**` component when `globstar` is on. Any other use of `**`
    /// (`d**`, `**y`) is an ordinary pattern, as in bash.
    fn is_globstar(&self) -> bool {
        self.bytes == b"**" && self.quoted.iter().all(|&quoted| !quoted)
    }

    /// A leading literal `.`: the component may match dotfiles, and with
    /// `globskipdots` off also `.` and `..`.
    fn leading_dot(&self) -> bool {
        self.bytes.first() == Some(&b'.') && !self.quoted.first().copied().unwrap_or(true)
    }
}

/// Split a pattern into `/`-separated components.
fn components<'a>(bytes: &'a [u8], quoted: &'a [bool]) -> Vec<Component<'a>> {
    let mut parts = Vec::new();
    let mut start = usize::from(bytes.first() == Some(&b'/'));
    loop {
        let end = bytes[start..]
            .iter()
            .position(|&byte| byte == b'/')
            .map_or(bytes.len(), |at| start + at);
        parts.push(Component {
            bytes: &bytes[start..end],
            quoted: &quoted[start..end],
        });
        if end == bytes.len() {
            break;
        }
        start = end + 1;
    }
    parts
}

/// Match the components left in `components` underneath `base`, appending each
/// complete result to `out`.
fn walk(base: &[u8], components: &[Component<'_>], options: &Options, out: &mut Vec<Vec<u8>>) {
    let Some((first, rest)) = components.split_first() else {
        out.push(base.to_vec());
        return;
    };
    if options.globstar && first.is_globstar() {
        // Zero directories: `**` matches the empty component list.
        walk(base, rest, options, out);
        for name in entries(base) {
            if name.starts_with(b".") && !options.dotglob {
                continue;
            }
            let child = match join(base, &name) {
                Some(child) => child,
                None => continue,
            };
            // `**` walks real directories only; a symlink to one is not
            // followed (bash's behaviour, unlike zsh's `***/`).
            if !is_directory(&child) {
                continue;
            }
            walk(&child, components, options, out);
        }
        return;
    }
    for candidate in step(base, first, options) {
        walk(&candidate, rest, options, out);
    }
}

/// The entries one component selects under `base`.
fn step(base: &[u8], component: &Component<'_>, options: &Options) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    if !has_meta(component.bytes, component.quoted, options.flags.extglob) {
        // No metacharacter: the component is a literal path segment.
        out.extend(join(base, component.bytes).filter(|path| Path::new(os(path)).exists()));
        return out;
    }
    let compiled = Pattern::new(
        &crate::pattern::escaped(component.bytes, component.quoted),
        options.flags,
    );
    let leading_dot = component.leading_dot();
    let mut names = entries(base);
    if leading_dot && !options.globskipdots {
        // bash 5.2+ omits these unless `globskipdots` is off.
        names.push(b".".to_vec());
        names.push(b"..".to_vec());
    }
    for name in names {
        let hidden = name.starts_with(b".") && !leading_dot && !options.dotglob;
        if hidden || !compiled.matches(&name) {
            continue;
        }
        out.extend(join(base, &name));
    }
    out
}

/// The names in `base`, or nothing when it is not a readable directory.
fn entries(base: &[u8]) -> Vec<Vec<u8>> {
    let dir: &[u8] = if base.is_empty() { b"." } else { base };
    let Ok(entries) = std::fs::read_dir(Path::new(os(dir))) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|entry| entry.file_name().as_bytes().to_vec())
        .collect()
}

/// Whether `path` is a directory itself, not a symlink to one.
fn is_directory(path: &[u8]) -> bool {
    std::fs::symlink_metadata(Path::new(os(path))).is_ok_and(|meta| meta.is_dir())
}

/// Join a prefix and a name, keeping `./` and the root slash as written so
/// `./*` expands to `./a`, not `a`. `None` on an embedded NUL.
fn join(base: &[u8], name: &[u8]) -> Option<Vec<u8>> {
    if base.iter().chain(name).any(|&byte| byte == 0) {
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
