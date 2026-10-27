//! Trap state: what `trap` registered, and the signal names it accepts.
//!
//! This module is **data only**. It stores actions, parses signal
//! specifications and renders them re-input-safe, but it never changes a signal
//! disposition and never runs shell code. That split is deliberate: this crate
//! has no syscall layer, and DESIGN.md §4.5 requires that no signal handler ever
//! touch interpreter state. The VM reads this table at command boundaries,
//! keeps the caught signals *blocked* and drains them from a `signalfd`, and
//! applies `SIG_IGN` for [`TrapAction::Ignore`] — that is also what makes an
//! ignored signal survive `exec`.
//!
//! The keys include the four pseudo-signals bash defines: `EXIT` (also written
//! `0`), `ERR`, `DEBUG` and `RETURN`. They are stored in the same table so
//! `trap` lists them in one place, in bash's order: `EXIT` first, then real
//! signals in numeric order, then the pseudo-traps.

use std::collections::BTreeMap;

/// One thing a trap can be attached to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrapKey {
    /// `trap … EXIT` / `trap … 0`: run when the shell exits.
    Exit,
    /// `trap … ERR`: run when a command fails outside an ignore context.
    Err,
    /// `trap … DEBUG`: run before each simple command.
    Debug,
    /// `trap … RETURN`: run when a sourced script returns.
    Return,
    /// A real signal number (1..=64).
    Signal(i32),
}

impl TrapKey {
    /// Sort rank: `EXIT`, then signals by number, then the pseudo-traps in
    /// bash's listing order.
    pub fn rank(self) -> i32 {
        match self {
            TrapKey::Exit => 0,
            TrapKey::Signal(number) => number,
            TrapKey::Err => 1000,
            TrapKey::Debug => 1001,
            TrapKey::Return => 1002,
        }
    }

    /// The spelling `trap` prints, re-input-safe: `EXIT`, `SIGINT`, `ERR`.
    pub fn as_spec(self) -> Vec<u8> {
        match self {
            TrapKey::Exit => b"EXIT".to_vec(),
            TrapKey::Err => b"ERR".to_vec(),
            TrapKey::Debug => b"DEBUG".to_vec(),
            TrapKey::Return => b"RETURN".to_vec(),
            TrapKey::Signal(number) => match signal_short_name(number) {
                Some(name) => format!("SIG{name}").into_bytes(),
                None => number.to_string().into_bytes(),
            },
        }
    }

    /// Parse a `signal_spec`: a name (`INT`, `SIGINT`, case-insensitive), a
    /// pseudo-signal name, or a number (`0` is `EXIT`).
    ///
    /// `None` means the specification is invalid; the caller reports it, and
    /// `trap` is expected to return non-zero without changing the table.
    pub fn parse(spec: &[u8]) -> Option<TrapKey> {
        if spec.is_empty() {
            return None;
        }
        if spec.iter().all(u8::is_ascii_digit) {
            let text = std::str::from_utf8(spec).ok()?;
            let number: i32 = text.parse().ok()?;
            if number == 0 {
                return Some(TrapKey::Exit);
            }
            return signal_short_name(number).map(|_| TrapKey::Signal(number));
        }
        let upper: Vec<u8> = spec.iter().map(u8::to_ascii_uppercase).collect();
        match upper.as_slice() {
            b"EXIT" => return Some(TrapKey::Exit),
            b"ERR" => return Some(TrapKey::Err),
            b"DEBUG" => return Some(TrapKey::Debug),
            b"RETURN" => return Some(TrapKey::Return),
            _ => {}
        }
        let name = upper.strip_prefix(b"SIG").unwrap_or(&upper);
        let number = SIGNALS
            .iter()
            .find(|(_, candidate)| candidate.as_bytes() == name)
            .map(|(number, _)| *number)?;
        Some(TrapKey::Signal(number))
    }
}

impl PartialOrd for TrapKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for TrapKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.rank().cmp(&other.rank())
    }
}

/// What a trap does when it fires.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TrapAction {
    /// Run this shell code.
    Command(Vec<u8>),
    /// `trap '' SIG`: ignore the signal, including in children (`SIG_IGN`
    /// survives `exec`, which is the whole point).
    Ignore,
}

impl TrapAction {
    /// The text a listing prints for this action, before quoting.
    pub fn code(&self) -> &[u8] {
        match self {
            TrapAction::Command(code) => code,
            TrapAction::Ignore => b"",
        }
    }
}

/// The shell's trap table, keyed by [`TrapKey`].
///
/// Iteration is in `TrapKey` order, which is the order `trap` prints: `EXIT`,
/// then real signals ascending, then `ERR`/`DEBUG`/`RETURN`.
#[derive(Clone, Debug, Default)]
pub struct TrapTable {
    entries: BTreeMap<TrapKey, TrapAction>,
    /// Bumped on every mutation, so the VM can notice that the signal
    /// dispositions it maintains need re-syncing without diffing the table
    /// after every command.
    generation: u64,
}

impl TrapTable {
    /// The action registered for `key`.
    pub fn get(&self, key: TrapKey) -> Option<&TrapAction> {
        self.entries.get(&key)
    }

    /// Register `action` for `key`, replacing any previous action.
    pub fn set(&mut self, key: TrapKey, action: TrapAction) {
        self.entries.insert(key, action);
        self.generation = self.generation.wrapping_add(1);
    }

    /// Remove the trap for `key`, returning whether one was registered.
    pub fn remove(&mut self, key: TrapKey) -> bool {
        let removed = self.entries.remove(&key).is_some();
        if removed {
            self.generation = self.generation.wrapping_add(1);
        }
        removed
    }

    /// Drop every caught trap but keep the ignored dispositions.
    ///
    /// A forked child starts with no traps (POSIX §2.11), except that a signal
    /// set to `SIG_IGN` stays ignored — and an ignored signal needs no trap
    /// entry to keep working, so removing the entry is the whole job.
    pub fn clear_for_subshell(&mut self) {
        let before = self.entries.len();
        self.entries
            .retain(|_, action| matches!(action, TrapAction::Ignore));
        if self.entries.len() != before {
            self.generation = self.generation.wrapping_add(1);
        }
    }

    /// Whether the table has no entries at all.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The number of registered traps.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// The mutation counter (see the field docs).
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Every entry, in listing order.
    pub fn entries(&self) -> impl Iterator<Item = (&TrapKey, &TrapAction)> {
        self.entries.iter()
    }

    /// The trap registered for a real signal number, if any.
    pub fn signal_action(&self, signal: i32) -> Option<&TrapAction> {
        self.entries.get(&TrapKey::Signal(signal))
    }

    /// Real signals whose action is to run shell code: the VM keeps these
    /// blocked and reads them from a `signalfd`.
    pub fn caught_signals(&self) -> Vec<i32> {
        self.entries
            .iter()
            .filter_map(|(key, action)| match (key, action) {
                (TrapKey::Signal(number), TrapAction::Command(_)) => Some(*number),
                _ => None,
            })
            .collect()
    }

    /// Real signals whose action is to ignore them: the VM sets `SIG_IGN`, and
    /// children inherit it across `exec`.
    pub fn ignored_signals(&self) -> Vec<i32> {
        self.entries
            .iter()
            .filter_map(|(key, action)| match (key, action) {
                (TrapKey::Signal(number), TrapAction::Ignore) => Some(*number),
                _ => None,
            })
            .collect()
    }
}

/// The signals `trap -l` and `kill -l` list, and the names both accept.
///
/// This is Linux's numbering, which is also the numbering of every platform
/// this shell targets; `signal_short_name` answers `None` for a number the host
/// does not define, so a name can never be invented for an absent signal.
pub const SIGNALS: &[(i32, &str)] = &[
    (1, "HUP"),
    (2, "INT"),
    (3, "QUIT"),
    (4, "ILL"),
    (5, "TRAP"),
    (6, "ABRT"),
    (7, "BUS"),
    (8, "FPE"),
    (9, "KILL"),
    (10, "USR1"),
    (11, "SEGV"),
    (12, "USR2"),
    (13, "PIPE"),
    (14, "ALRM"),
    (15, "TERM"),
    (16, "STKFLT"),
    (17, "CHLD"),
    (18, "CONT"),
    (19, "STOP"),
    (20, "TSTP"),
    (21, "TTIN"),
    (22, "TTOU"),
    (23, "URG"),
    (24, "XCPU"),
    (25, "XFSZ"),
    (26, "VTALRM"),
    (27, "PROF"),
    (28, "WINCH"),
    (29, "IO"),
    (30, "PWR"),
    (31, "SYS"),
    (34, "RTMIN"),
    (64, "RTMAX"),
];

/// The short name of a signal (`INT`, not `SIGINT`), if the host defines it.
pub fn signal_short_name(signal: i32) -> Option<&'static str> {
    SIGNALS
        .iter()
        .find(|(number, _)| *number == signal)
        .map(|(_, name)| *name)
}

/// Parse a signal name or number for `kill`.
///
/// Unlike [`TrapKey::parse`] this rejects `0` (it is not a signal) and the
/// pseudo-signals, because `kill -s EXIT` is not meaningful. Case-insensitive,
/// `SIG` prefix optional.
pub fn signal_number(spec: &[u8]) -> Option<i32> {
    if spec.is_empty() {
        return None;
    }
    if spec.iter().all(u8::is_ascii_digit) {
        let text = std::str::from_utf8(spec).ok()?;
        let number: i32 = text.parse().ok()?;
        return signal_short_name(number).map(|_| number);
    }
    let upper: Vec<u8> = spec.iter().map(u8::to_ascii_uppercase).collect();
    let name = upper.strip_prefix(b"SIG").unwrap_or(&upper);
    SIGNALS
        .iter()
        .find(|(_, candidate)| candidate.as_bytes() == name)
        .map(|(number, _)| *number)
}

/// Render the two-column `trap -l` / `kill -l` table, five signals per line.
///
/// bash's exact format, because scripts parse it: ` 1) SIGHUP\t 2) SIGINT…`.
pub fn signal_table() -> Vec<u8> {
    let mut out = Vec::new();
    for (index, (number, name)) in SIGNALS.iter().enumerate() {
        if index > 0 && index % 5 == 0 {
            out.push(b'\n');
        } else if index > 0 {
            out.push(b'\t');
        }
        out.extend_from_slice(format!("{number:2}) SIG{name}").as_bytes());
    }
    out.push(b'\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn specs_parse_case_insensitively_and_by_number() {
        assert_eq!(TrapKey::parse(b"INT"), Some(TrapKey::Signal(2)));
        assert_eq!(TrapKey::parse(b"sigint"), Some(TrapKey::Signal(2)));
        assert_eq!(TrapKey::parse(b"SigTerm"), Some(TrapKey::Signal(15)));
        assert_eq!(TrapKey::parse(b"15"), Some(TrapKey::Signal(15)));
        assert_eq!(TrapKey::parse(b"0"), Some(TrapKey::Exit));
        assert_eq!(TrapKey::parse(b"EXIT"), Some(TrapKey::Exit));
        assert_eq!(TrapKey::parse(b"ERR"), Some(TrapKey::Err));
        assert_eq!(TrapKey::parse(b"DEBUG"), Some(TrapKey::Debug));
        assert_eq!(TrapKey::parse(b"RETURN"), Some(TrapKey::Return));
        assert_eq!(TrapKey::parse(b"SIGINVALID"), None);
        assert_eq!(TrapKey::parse(b"-99"), None);
        assert_eq!(TrapKey::parse(b"128"), None);
    }

    #[test]
    fn listing_order_is_exit_then_signals_then_pseudo_traps() {
        let mut table = TrapTable::default();
        table.set(TrapKey::Signal(15), TrapAction::Command(b"echo t".to_vec()));
        table.set(TrapKey::Err, TrapAction::Command(b"echo e".to_vec()));
        table.set(TrapKey::Exit, TrapAction::Command(b"echo 0".to_vec()));
        table.set(TrapKey::Signal(2), TrapAction::Command(b"echo 2".to_vec()));
        let keys: Vec<TrapKey> = table.entries().map(|(key, _)| *key).collect();
        assert_eq!(
            keys,
            vec![
                TrapKey::Exit,
                TrapKey::Signal(2),
                TrapKey::Signal(15),
                TrapKey::Err
            ]
        );
    }

    #[test]
    fn a_subshell_keeps_ignore_and_drops_every_other_trap() {
        let mut table = TrapTable::default();
        table.set(TrapKey::Exit, TrapAction::Command(b"echo bye".to_vec()));
        table.set(
            TrapKey::Signal(2),
            TrapAction::Command(b"echo int".to_vec()),
        );
        table.set(TrapKey::Signal(10), TrapAction::Ignore);
        let before = table.generation();
        table.clear_for_subshell();
        assert!(table.get(TrapKey::Exit).is_none());
        assert!(table.get(TrapKey::Signal(2)).is_none());
        assert_eq!(table.get(TrapKey::Signal(10)), Some(&TrapAction::Ignore));
        assert!(table.generation() > before);
        assert_eq!(table.ignored_signals(), vec![10]);
        assert!(table.caught_signals().is_empty());
    }

    #[test]
    fn the_printable_spec_is_the_canonical_name() {
        assert_eq!(TrapKey::Signal(2).as_spec(), b"SIGINT");
        assert_eq!(TrapKey::Exit.as_spec(), b"EXIT");
        assert_eq!(TrapKey::Return.as_spec(), b"RETURN");
    }

    #[test]
    fn the_signal_table_lists_bash_style_rows() {
        let table = String::from_utf8(signal_table()).expect("ASCII");
        assert!(table.starts_with(" 1) SIGHUP\t 2) SIGINT\t 3) SIGQUIT"));
        assert!(table.ends_with('\n'));
        assert_eq!(table.lines().count(), 7);
    }
}
