//! `PS1`/`PS2` expansion.
//!
//! The prompt is bytes, expanded from a template with the handful of backslash
//! escapes every shell has. The values come from the process environment for
//! this wave: `PS1` and `PS2` themselves, plus `USER`, `HOSTNAME`, `HOME` and
//! `PWD`. `$PWD` changing inside the shell updates the prompt through
//! [`crate::Editor::set_cwd`], because the process environment is not the
//! shell's variable table.
//!
//! Root is detected from `/proc/self`'s owner, which is the process's effective
//! uid on Linux. When that is not readable we fall back to `USER=root`; when
//! neither answers, the prompt is `$ `.

use std::os::unix::ffi::OsStrExt;

/// The values `\u`, `\h`, `\w`, `\!` and `\$` expand from.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PromptState {
    /// `\u`: the user name.
    pub user: Vec<u8>,
    /// `\h` (up to the first dot) and `\H` (whole): the host name.
    pub host: Vec<u8>,
    /// `\w` and `\W`: the current working directory.
    pub cwd: Vec<u8>,
    /// `\w` abbreviates this prefix to `~`.
    pub home: Vec<u8>,
    /// `\$` is `#` for root and `$` otherwise.
    pub root: bool,
    /// `\!`: the number of this command in the history.
    pub history_number: u32,
}

impl PromptState {
    /// Read the values from the process environment.
    pub fn from_env() -> Self {
        let cwd = env_bytes("PWD").unwrap_or_else(|| {
            std::env::current_dir()
                .map(|path| path.as_os_str().as_bytes().to_vec())
                .unwrap_or_default()
        });
        Self {
            user: env_bytes("USER")
                .or_else(|| env_bytes("LOGNAME"))
                .unwrap_or_default(),
            host: env_bytes("HOSTNAME")
                .or_else(read_hostname)
                .unwrap_or_default(),
            cwd,
            home: env_bytes("HOME").unwrap_or_default(),
            root: is_root(),
            history_number: 1,
        }
    }
}

/// `PS1`/`PS2` and the state their escapes read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Prompt {
    /// The raw `PS1` template.
    pub primary: Vec<u8>,
    /// The raw `PS2` template.
    pub secondary: Vec<u8>,
    /// Variables the escapes expand from.
    pub state: PromptState,
}

impl Prompt {
    /// A prompt from explicit templates.
    pub fn new(primary: impl Into<Vec<u8>>, secondary: impl Into<Vec<u8>>) -> Self {
        Self {
            primary: primary.into(),
            secondary: secondary.into(),
            state: PromptState::default(),
        }
    }

    /// Read `PS1` and `PS2` from the environment, with shell defaults.
    pub fn from_env() -> Self {
        let state = PromptState::from_env();
        let default_primary: &[u8] = if state.root { b"# " } else { b"$ " };
        Self {
            primary: env_bytes("PS1").unwrap_or_else(|| default_primary.to_vec()),
            secondary: env_bytes("PS2").unwrap_or_else(|| b"> ".to_vec()),
            state,
        }
    }

    /// The prompt before the first line of a command.
    pub fn primary_text(&self) -> Vec<u8> {
        expand(&self.primary, &self.state)
    }

    /// The prompt before a continuation line.
    pub fn secondary_text(&self) -> Vec<u8> {
        expand(&self.secondary, &self.state)
    }
}

impl Default for Prompt {
    fn default() -> Self {
        Self::new(b"$ ".to_vec(), b"> ".to_vec())
    }
}

/// Expand the backslash escapes in `template`.
///
/// Recognised: `\$ \u \h \H \w \W \! \n \\`. An unrecognised escape keeps its
/// backslash, the way bash does, so a typo is visible rather than silently
/// swallowed. A trailing backslash is literal.
pub fn expand(template: &[u8], state: &PromptState) -> Vec<u8> {
    let mut out = Vec::with_capacity(template.len());
    let mut at = 0;
    while at < template.len() {
        if template[at] != b'\\' {
            out.push(template[at]);
            at += 1;
            continue;
        }
        let Some(&escape) = template.get(at + 1) else {
            out.push(b'\\');
            break;
        };
        match escape {
            b'$' => out.push(if state.root { b'#' } else { b'$' }),
            b'u' => out.extend_from_slice(&state.user),
            b'h' => out.extend_from_slice(short_host(&state.host)),
            b'H' => out.extend_from_slice(&state.host),
            b'w' => out.extend_from_slice(&abbreviate_home(&state.cwd, &state.home)),
            b'W' => out.extend_from_slice(last_component(&state.cwd)),
            b'!' => out.extend_from_slice(state.history_number.to_string().as_bytes()),
            b'n' => out.push(b'\n'),
            b'\\' => out.push(b'\\'),
            other => {
                out.push(b'\\');
                out.push(other);
            }
        }
        at += 2;
    }
    out
}

/// Whether this process is running as root, as far as we can tell.
///
/// `/proc/<pid>` is owned by the process's effective uid on Linux; reading the
/// owner is a `stat` the standard library can do without `libc`.
pub fn is_root() -> bool {
    use std::os::unix::fs::MetadataExt;
    if let Ok(metadata) = std::fs::metadata("/proc/self") {
        return metadata.uid() == 0;
    }
    env_bytes("USER").is_some_and(|user| user == b"root")
}

fn env_bytes(name: &str) -> Option<Vec<u8>> {
    std::env::var_os(name).map(|value| value.as_bytes().to_vec())
}
fn read_hostname() -> Option<Vec<u8>> {
    let host = std::fs::read("/proc/sys/kernel/hostname").ok()?;
    let trimmed = host.trim_ascii();
    (!trimmed.is_empty()).then(|| trimmed.to_vec())
}

fn short_host(host: &[u8]) -> &[u8] {
    match host.iter().position(|&byte| byte == b'.') {
        Some(dot) => &host[..dot],
        None => host,
    }
}

fn abbreviate_home(cwd: &[u8], home: &[u8]) -> Vec<u8> {
    if home.is_empty() || !cwd.starts_with(home) {
        return cwd.to_vec();
    }
    let rest = &cwd[home.len()..];
    if rest.is_empty() {
        return b"~".to_vec();
    }
    if rest[0] == b'/' {
        let mut out = Vec::with_capacity(rest.len() + 1);
        out.push(b'~');
        out.extend_from_slice(rest);
        return out;
    }
    cwd.to_vec()
}

fn last_component(cwd: &[u8]) -> &[u8] {
    match cwd.iter().rposition(|&byte| byte == b'/') {
        Some(0) => &cwd[..1],
        Some(slash) => &cwd[slash + 1..],
        None => cwd,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> PromptState {
        PromptState {
            user: b"ada".to_vec(),
            host: b"lab.example.com".to_vec(),
            cwd: b"/home/ada/work".to_vec(),
            home: b"/home/ada".to_vec(),
            root: false,
            history_number: 42,
        }
    }

    #[test]
    fn the_common_escapes() {
        assert_eq!(
            expand(b"\\u@\\h:\\w\\$ ", &state()),
            b"ada@lab:~/work$ ".to_vec()
        );
    }

    #[test]
    fn root_and_literal_escapes() {
        let mut state = state();
        state.root = true;
        assert_eq!(expand(b"\\$", &state), b"#");
        assert_eq!(expand(b"\\\\", &state), b"\\");
        assert_eq!(expand(b"a\\nb", &state), b"a\nb");
        assert_eq!(expand(b"\\!", &state), b"42");
    }

    #[test]
    fn full_host_and_basename() {
        assert_eq!(expand(b"\\H", &state()), b"lab.example.com");
        assert_eq!(expand(b"\\W", &state()), b"work");
    }

    #[test]
    fn home_itself_is_a_tilde() {
        let mut state = state();
        state.cwd = b"/home/ada".to_vec();
        assert_eq!(expand(b"\\w", &state), b"~");
    }

    #[test]
    fn a_home_prefix_without_a_slash_is_not_abbreviated() {
        let mut state = state();
        state.cwd = b"/home/adam".to_vec();
        assert_eq!(expand(b"\\w", &state), b"/home/adam");
    }

    #[test]
    fn unknown_escapes_keep_their_backslash() {
        assert_eq!(expand(b"\\q", &state()), b"\\q");
        assert_eq!(expand(b"trailing\\", &state()), b"trailing\\");
    }

    #[test]
    fn root_cwd_is_its_own_basename() {
        assert_eq!(
            expand(
                b"\\W",
                &PromptState {
                    cwd: b"/".to_vec(),
                    ..state()
                }
            ),
            b"/".to_vec()
        );
    }
}
