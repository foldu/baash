//! Path grammar: the rules for a *name*, not for a file.
//!
//! A logical `cd` (the default, and `-L` explicitly), `PWD`, `pushd`/`popd`
//! and `dirs` all work on the name the user spelled. They collapse `.`, `..`
//! and repeated separators textually and keep that spelling, which is why
//! `cd ..` leaves a symlinked directory by name instead of by the link's
//! target. Doing that needs the platform's notion of a *root*, and the two
//! platforms do not agree on one:
//!
//! * POSIX has a single root, `/`, and `..` there stays at `/`;
//! * Windows has a root per drive (`C:\`) and per network share
//!   (`\\server\share\`), so `..` has to stop at the root it started from, and
//!   `C:foo` — relative to whichever directory drive `C:` was last left on — is
//!   not decidable from a string at all.
//!
//! [`resolve`] therefore returns an `Option`, and `None` means "only the kernel
//! can say": the caller asks it and records what it answered. The walk is
//! written once, parameterised by [`Flavour`], so the Windows rules are
//! testable on a host that is not Windows.
//!
//! Nothing here touches the file system. Whether a component exists, and what
//! a name resolves to through a symlink, are questions for [`std::fs`]; this
//! module only decides what the shell should *say* and which name it should
//! hand to `chdir`.

/// Which platform's path grammar to apply.
///
/// The walk is the same on both; the root and the separator are not. Spelling
/// the grammar as a value rather than as a `cfg` makes the Windows rules
/// reachable from a test running on Linux.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flavour {
    /// One root (`/`), and `/` is the only separator.
    Posix,
    /// A root per drive or network share, and `/` and `\` both separate.
    Windows,
}

impl Flavour {
    /// The grammar of the platform this build runs on.
    pub const fn host() -> Self {
        if cfg!(windows) {
            Self::Windows
        } else {
            Self::Posix
        }
    }

    /// The separator this grammar spells paths with.
    pub const fn separator(self) -> u8 {
        match self {
            Self::Posix => b'/',
            Self::Windows => b'\\',
        }
    }

    /// Whether `byte` separates components here.
    ///
    /// Windows accepts `/` as well as `\` wherever a path is parsed, and the
    /// shell keeps accepting it there: make files, `#!` lines and the output of
    /// every Unix tool are full of forward slashes. POSIX has no such freedom —
    /// `\` is an ordinary byte in a file name, and treating it as a separator
    /// would make `cd my\ dir` enter the wrong directory.
    pub const fn is_separator(self, byte: u8) -> bool {
        match self {
            Self::Posix => byte == b'/',
            Self::Windows => byte == b'/' || byte == b'\\',
        }
    }
}

/// The separator this platform spells paths with.
pub const SEPARATOR: u8 = Flavour::host().separator();

/// The separator this platform spells paths with.
pub fn separator() -> u8 {
    SEPARATOR
}

/// Whether `byte` separates components on this platform.
pub fn is_separator(byte: u8) -> bool {
    Flavour::host().is_separator(byte)
}

/// Whether `path` contains a separator, and so names a file rather than a name
/// to look up in `PATH`.
pub fn has_separator(path: &[u8]) -> bool {
    path.iter().any(|&byte| is_separator(byte))
}

/// Where a path begins.
#[derive(Debug, PartialEq, Eq)]
enum Start {
    /// The path names its own root. The bytes are spelled with the platform's
    /// separator and end with it, so a component can be appended directly.
    Root(Vec<u8>),
    /// Rooted but without a drive (`\foo` on Windows): the current drive's
    /// root, whatever that is.
    CurrentDrive,
    /// Relative to the current directory.
    Relative,
    /// Only the kernel can say. A Windows drive-relative name belongs here:
    /// `C:foo` is relative to the directory that `C:` was last left on, which
    /// is per-process state.
    Kernel,
}

/// Split `path` into where it starts and the offset of its first component.
fn start(flavour: Flavour, path: &[u8]) -> (Start, usize) {
    match flavour {
        Flavour::Posix => match path.first() {
            Some(b'/') => (Start::Root(vec![b'/']), 1),
            _ => (Start::Relative, 0),
        },
        Flavour::Windows => windows_start(path),
    }
}

fn windows_start(path: &[u8]) -> (Start, usize) {
    let Some(&first) = path.first() else {
        return (Start::Relative, 0);
    };
    fn separator(byte: u8) -> bool {
        Flavour::Windows.is_separator(byte)
    }
    if separator(first) && path.get(1).is_some_and(|&byte| separator(byte)) {
        // A device namespace (`\\?\C:\…`, `\\.\PIPE\…`) is not a share: the
        // share parser below would read `?` as a server and `C:` as its share.
        // Handing these to the kernel is the only honest answer.
        if matches!(path.get(2), Some(b'?' | b'.')) {
            return (Start::Kernel, 0);
        }
        let mut index = 2;
        let server = take(path, &mut index);
        let share = take(path, &mut index);
        if server.is_empty() || share.is_empty() {
            return (Start::Kernel, 0);
        }
        let mut root = vec![b'\\', b'\\'];
        root.extend_from_slice(server);
        root.push(b'\\');
        root.extend_from_slice(share);
        root.push(b'\\');
        return (Start::Root(root), index);
    }
    if path.len() >= 2 && path[0].is_ascii_alphabetic() && path[1] == b':' {
        if path.get(2).is_some_and(|&byte| separator(byte)) {
            return (Start::Root(vec![path[0], b':', b'\\']), 3);
        }
        return (Start::Kernel, 0);
    }
    if separator(first) {
        return (Start::CurrentDrive, 1);
    }
    (Start::Relative, 0)
}

/// The component starting at `*index`, advancing past it and one separator.
fn take<'a>(path: &'a [u8], index: &mut usize) -> &'a [u8] {
    let start = *index;
    while *index < path.len() && !Flavour::Windows.is_separator(path[*index]) {
        *index += 1;
    }
    let component = &path[start..*index];
    if *index < path.len() {
        *index += 1;
    }
    component
}

/// A path walked lexically: a root plus the components below it.
///
/// The root is kept spelled with the platform separator and ending in one, so
/// [`Lexical::spell`] never has to decide whether it needs a separator.
#[derive(Debug, PartialEq, Eq)]
struct Lexical {
    flavour: Flavour,
    root: Vec<u8>,
    components: Vec<Vec<u8>>,
}

impl Lexical {
    fn new(flavour: Flavour, root: Vec<u8>) -> Self {
        Self {
            flavour,
            root,
            components: Vec::new(),
        }
    }

    fn push(&mut self, name: &[u8]) {
        self.components.push(name.to_vec());
    }

    fn pop(&mut self) {
        // `..` above a root stays at the root: `/` on POSIX, and `C:\` or a
        // share's root on Windows. There is no parent of a drive.
        self.components.pop();
    }

    fn spell(&self) -> Vec<u8> {
        let mut out = self.root.clone();
        for (index, component) in self.components.iter().enumerate() {
            if index > 0 {
                out.push(self.flavour.separator());
            }
            out.extend_from_slice(component);
        }
        out
    }
}

/// Walk `path` from `cwd`, calling `entered` with the name of each component it
/// enters that could be a directory of its own.
///
/// `None` means the walk could not start or could not finish without the
/// kernel: either `cwd` is not absolute, or `path` is a name only the kernel
/// can place.
fn walk(
    flavour: Flavour,
    cwd: &[u8],
    path: &[u8],
    mut entered: impl FnMut(&Lexical),
) -> Option<Lexical> {
    let (cwd_start, consumed) = start(flavour, cwd);
    let Start::Root(root) = cwd_start else {
        // `cwd` comes from `$PWD` or from `getcwd`, and either can be missing
        // or stale; without a root there is nothing to walk from.
        return None;
    };
    let mut lexical = Lexical::new(flavour, root);
    apply(&mut lexical, flavour, &cwd[consumed..], &mut |_| {});
    let (path_start, consumed) = start(flavour, path);
    match path_start {
        Start::Root(root) => {
            lexical.root = root;
            lexical.components.clear();
        }
        // `\foo` names a directory on the current drive, so the root stays and
        // the path replaces what is below it.
        Start::CurrentDrive => lexical.components.clear(),
        Start::Relative => {}
        Start::Kernel => return None,
    }
    apply(&mut lexical, flavour, &path[consumed..], &mut entered);
    Some(lexical)
}

/// Feed one run of components into `lexical`, reporting each one that is a
/// name rather than a `.` or `..`.
fn apply(lexical: &mut Lexical, flavour: Flavour, text: &[u8], entered: &mut impl FnMut(&Lexical)) {
    for component in text.split(|&byte| flavour.is_separator(byte)) {
        match component {
            b"" | b"." => {}
            b".." => lexical.pop(),
            name => {
                lexical.push(name);
                entered(lexical);
            }
        }
    }
}

/// Resolve `path` against `cwd` lexically, or `None` when only the kernel can.
///
/// The answer is absolute and spelled the way this platform spells paths, with
/// repeated separators, `.` and `..` collapsed. It is a *name*, not a canonical
/// path: a component that is a symlink is kept as written, because that is what
/// `PWD` and the prompt show.
pub fn resolve(cwd: &[u8], path: &[u8]) -> Option<Vec<u8>> {
    walk(Flavour::host(), cwd, path, |_| {}).map(|lexical| lexical.spell())
}

/// Every name a logical `cd` would have to be able to enter to reach `path`.
///
/// bash's `-L` resolves `..` textually but still fails when a component it
/// names is not a directory: `cd nonexistent/..` fails while `cd a/b/..`
/// succeeds. The prefixes are therefore per *named* component, against the
/// textual path built so far — never against the raw path, whose `..` the
/// kernel would resolve through a symlink.
///
/// `None` carries the same meaning as in [`resolve`].
pub fn prefixes(cwd: &[u8], path: &[u8]) -> Option<Vec<Vec<u8>>> {
    let mut prefixes = Vec::new();
    walk(Flavour::host(), cwd, path, |lexical| {
        prefixes.push(lexical.spell());
    })?;
    Some(prefixes)
}

/// Whether `path` starts at a root, without reference to the current
/// directory.
///
/// On Windows this includes a rooted name (`\foo`): it depends on the current
/// *drive*, which is not the same thing as the current directory, and the shell
/// never searches a search path for it.
pub fn is_absolute(path: &[u8]) -> bool {
    is_absolute_for(Flavour::host(), path)
}

fn is_absolute_for(flavour: Flavour, path: &[u8]) -> bool {
    matches!(start(flavour, path).0, Start::Root(_) | Start::CurrentDrive)
}

/// Whether `path` is anchored to the current directory by a leading `.` or
/// `..` component (`./x`, `../x`, `.`).
///
/// bash does not search `CDPATH` for these: the user has already said which
/// directory they mean.
pub fn is_dot_anchored(path: &[u8]) -> bool {
    is_dot_anchored_for(Flavour::host(), path)
}

fn is_dot_anchored_for(flavour: Flavour, path: &[u8]) -> bool {
    matches!(
        path.split(|&byte| flavour.is_separator(byte)).next(),
        Some(b".") | Some(b"..")
    )
}

/// A root to use when neither `$PWD` nor the kernel can name the current
/// directory.
///
/// Nothing is likely to be found relative to it; it exists so that a shell in
/// that state still produces an absolute name instead of a relative one, and so
/// that the fallback is the same shape as a real answer.
pub fn root() -> Vec<u8> {
    match Flavour::host() {
        Flavour::Posix => vec![b'/'],
        Flavour::Windows => {
            let drive = std::env::var_os("SystemDrive")
                .map(crate::os::bytes_from_os)
                .filter(|value| value.len() == 2 && value[1] == b':')
                .unwrap_or_else(|| b"C:".to_vec());
            let mut out = drive;
            out.push(b'\\');
            out
        }
    }
}

/// Strip `prefix` from the front of `path`, when it is a whole-component
/// prefix.
///
/// The answer is `None` unless the remainder is empty or begins with a
/// separator: `C:\Users\ada` is a prefix of `C:\Users\ada\work` but not of
/// `C:\Users\adam`. Trailing separators on the prefix are ignored, and on
/// Windows the comparison folds ASCII case, because that is how the file
/// system compares.
pub fn strip_prefix<'a>(path: &'a [u8], prefix: &[u8]) -> Option<&'a [u8]> {
    strip_prefix_for(Flavour::host(), path, prefix)
}

fn strip_prefix_for<'a>(flavour: Flavour, path: &'a [u8], prefix: &[u8]) -> Option<&'a [u8]> {
    let mut prefix = prefix;
    while prefix.len() > 1 && flavour.is_separator(prefix[prefix.len() - 1]) {
        prefix = &prefix[..prefix.len() - 1];
    }
    if prefix.is_empty() || path.len() < prefix.len() {
        return None;
    }
    let matches = match flavour {
        Flavour::Posix => &path[..prefix.len()] == prefix,
        Flavour::Windows => path[..prefix.len()].eq_ignore_ascii_case(prefix),
    };
    if !matches {
        return None;
    }
    let rest = &path[prefix.len()..];
    if rest.is_empty() || flavour.is_separator(rest[0]) {
        Some(rest)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{Flavour, Start, start, walk};
    use Flavour::{Posix, Windows};

    fn resolve(flavour: Flavour, cwd: &[u8], path: &[u8]) -> Option<Vec<u8>> {
        walk(flavour, cwd, path, |_| {}).map(|lexical| lexical.spell())
    }

    fn prefixes(flavour: Flavour, cwd: &[u8], path: &[u8]) -> Option<Vec<Vec<u8>>> {
        let mut prefixes = Vec::new();
        walk(flavour, cwd, path, |lexical| prefixes.push(lexical.spell()))?;
        Some(prefixes)
    }

    #[test]
    fn posix_collapses_dots_and_double_slashes() {
        assert_eq!(resolve(Posix, b"/tmp", b"..").as_deref(), Some(&b"/"[..]));
        assert_eq!(resolve(Posix, b"/tmp", b".").as_deref(), Some(&b"/tmp"[..]));
        assert_eq!(
            resolve(Posix, b"/", b"/tmp//oil_test/$HOME").as_deref(),
            Some(&b"/tmp/oil_test/$HOME"[..])
        );
        assert_eq!(
            resolve(Posix, b"/a/b", b"c/./d/../e").as_deref(),
            Some(&b"/a/b/c/e"[..])
        );
        assert_eq!(
            resolve(Posix, b"/a", b"../../..").as_deref(),
            Some(&b"/"[..])
        );
        assert_eq!(resolve(Posix, b"/a", b"").as_deref(), Some(&b"/a"[..]));
    }

    #[test]
    fn posix_keeps_backslashes_in_names() {
        // `cd my\ dir` is one directory with a space in its name on POSIX; a
        // separator rule borrowed from Windows would look for `my`.
        assert_eq!(
            resolve(Posix, b"/a", br"my\ dir").as_deref(),
            Some(&br"/a/my\ dir"[..])
        );
        assert!(matches!(start(Posix, b"/a").0, Start::Root(_)));
        assert!(matches!(start(Posix, b"\\a").0, Start::Relative));
    }

    #[test]
    fn posix_always_ends_up_absolute() {
        assert_eq!(resolve(Posix, b"relative", b"x"), None);
    }

    #[test]
    fn windows_walks_up_to_the_root_it_started_in() {
        assert_eq!(
            resolve(Windows, br"C:\Users\win", b"..").as_deref(),
            Some(&br"C:\Users"[..])
        );
        assert_eq!(
            resolve(Windows, br"C:\Users\win", br"..\..\..").as_deref(),
            Some(&br"C:\"[..])
        );
        assert_eq!(
            resolve(Windows, br"C:\", b"..").as_deref(),
            Some(&br"C:\"[..])
        );
    }

    #[test]
    fn windows_appends_relative_components() {
        assert_eq!(
            resolve(Windows, br"C:\Users\win", b"Windows").as_deref(),
            Some(&br"C:\Users\win\Windows"[..])
        );
        // The separator the user typed is not the one the shell records: a
        // name is a name, and `\` is how this platform spells it.
        assert_eq!(
            resolve(Windows, br"C:\Users\win", b"AppData/Local/Temp").as_deref(),
            Some(&br"C:\Users\win\AppData\Local\Temp"[..])
        );
        assert_eq!(
            resolve(Windows, br"C:\Users\win", br".\..\Public").as_deref(),
            Some(&br"C:\Users\Public"[..])
        );
    }

    #[test]
    fn windows_roots_are_per_drive_and_per_share() {
        // A drive-rooted name replaces the whole path, including the drive.
        assert_eq!(
            resolve(Windows, br"C:\Users\win", br"D:\Data").as_deref(),
            Some(&br"D:\Data"[..])
        );
        assert_eq!(
            resolve(Windows, br"C:\Users\win", b"C:/Windows").as_deref(),
            Some(&br"C:\Windows"[..])
        );
        // A rooted name without a drive keeps the *current drive* and drops
        // everything below it.
        assert_eq!(
            resolve(Windows, br"C:\Users\win", br"\Windows\System32").as_deref(),
            Some(&br"C:\Windows\System32"[..])
        );
        // A network share is its own root, so `..` stops there.
        assert_eq!(
            resolve(Windows, br"\\srv\share\dir", b"..").as_deref(),
            Some(&br"\\srv\share\"[..])
        );
        assert_eq!(
            resolve(Windows, br"\\srv\share\dir", br"..\..\..").as_deref(),
            Some(&br"\\srv\share\"[..])
        );
        assert_eq!(
            resolve(Windows, br"\\srv\share\dir", br"\other").as_deref(),
            Some(&br"\\srv\share\other"[..])
        );
        assert_eq!(
            resolve(Windows, br"C:\Users", br"\\srv\share\x").as_deref(),
            Some(&br"\\srv\share\x"[..])
        );
        // Forward slashes are a spelling of the same share.
        assert_eq!(
            resolve(Windows, br"C:\Users", b"//srv/share/x").as_deref(),
            Some(&br"\\srv\share\x"[..])
        );
    }

    #[test]
    fn windows_leaves_drive_relative_names_to_the_kernel() {
        // `C:foo` is relative to the directory `C:` was last left on, which is
        // process state rather than text, and `\\?\` is a device namespace.
        assert_eq!(resolve(Windows, br"C:\Users", b"C:foo"), None);
        assert_eq!(resolve(Windows, br"C:\Users", b"C:"), None);
        assert_eq!(resolve(Windows, br"C:\Users", br"\\?\C:\x"), None);
        assert_eq!(resolve(Windows, br"\\srv\share", br"\\srv"), None);
    }

    #[test]
    fn windows_ignores_empty_components() {
        assert_eq!(
            resolve(Windows, br"C:\Users", br"C:\\a\\\b\").as_deref(),
            Some(&br"C:\a\b"[..])
        );
        assert_eq!(
            resolve(Windows, br"C:\Users\", b"").as_deref(),
            Some(&br"C:\Users"[..])
        );
        assert_eq!(
            resolve(Windows, br"C:\", b"x").as_deref(),
            Some(&br"C:\x"[..])
        );
    }

    #[test]
    fn windows_absolute_names_start_at_a_root() {
        for absolute in [
            br"C:\x" as &[u8],
            b"C:/x",
            br"\\srv\share\x",
            br"\x",
            br"C:\",
        ] {
            assert!(
                super::is_absolute_for(Windows, absolute),
                "{absolute:?} is absolute"
            );
        }
        for relative in [b"x" as &[u8], b"C:foo", b"", b"foo\\bar"] {
            assert!(
                !super::is_absolute_for(Windows, relative),
                "{relative:?} is not absolute"
            );
        }
        assert!(super::is_absolute_for(Posix, br"/x"));
        assert!(!super::is_absolute_for(Posix, br"x"));
    }

    #[test]
    fn dot_anchored_names_are_not_searched_for() {
        for anchored in [b"." as &[u8], b"./x", b"../x"] {
            assert!(
                super::is_dot_anchored_for(Windows, anchored),
                "{anchored:?}"
            );
        }
        assert!(super::is_dot_anchored_for(Windows, br".\x"));
        assert!(super::is_dot_anchored_for(Windows, br"..\x"));
        assert!(!super::is_dot_anchored_for(Windows, b".x"));
        // Only a leading component anchors the path; `x/./y` is a plain
        // relative name and `CDPATH` is still searched for it.
        assert!(!super::is_dot_anchored_for(Windows, b"x/./y"));
        assert!(super::is_dot_anchored_for(Posix, b"./x"));
        assert!(!super::is_dot_anchored_for(Posix, br".\x"));
    }

    #[test]
    fn prefixes_name_every_component_that_must_be_a_directory() {
        assert_eq!(
            prefixes(Posix, b"/a", b"b/../c").unwrap(),
            [b"/a/b".to_vec(), b"/a/c".to_vec()]
        );
        assert_eq!(
            prefixes(Windows, br"C:\a", br"b\..\c").unwrap(),
            [br"C:\a\b".to_vec(), br"C:\a\c".to_vec()]
        );
    }

    #[test]
    fn strip_prefix_needs_a_component_boundary() {
        assert_eq!(
            strip_home(Posix, b"/home/ada/work", b"/home/ada"),
            Some(&b"/work"[..])
        );
        assert_eq!(
            strip_home(Posix, b"/home/ada", b"/home/ada"),
            Some(&b""[..])
        );
        assert_eq!(strip_home(Posix, b"/home/adam", b"/home/ada"), None);
        // A root is not a home: it has no name below it to abbreviate.
        assert_eq!(strip_home(Posix, b"/home", b"/"), None);
    }

    #[test]
    fn strip_prefix_folds_case_on_windows_only() {
        assert_eq!(
            strip_home(Windows, br"C:\Users\win\x", br"c:\users\WIN"),
            Some(&br"\x"[..])
        );
        assert_eq!(
            strip_home(Windows, br"C:\Users\adam", br"C:\Users\ada"),
            None
        );
        // A trailing separator on the prefix is not a component of it.
        assert_eq!(
            strip_home(Windows, br"C:\Users\win\x", br"C:\Users\win\"),
            Some(&br"\x"[..])
        );
        assert_eq!(
            strip_home(Windows, br"C:\Users", br"C:\"),
            Some(&br"\Users"[..])
        );
        // POSIX is case-sensitive.
        assert_eq!(strip_home(Posix, b"/Home/ada", b"/home/ada"), None);
    }

    /// `strip_prefix` with an explicit flavour, for the tests above.
    fn strip_home<'a>(flavour: Flavour, path: &'a [u8], prefix: &[u8]) -> Option<&'a [u8]> {
        super::strip_prefix_for(flavour, path, prefix)
    }
}
