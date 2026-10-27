//! `wait` and `kill`: the minimum process management the trap tests need.
//!
//! `wait` reports the status of children this shell started (with `&` or as a
//! pipeline), including children that finished several commands ago; `kill`
//! sends a signal to a pid or the shell's own job table. Real job control
//! (`fg`, `bg`, `jobs`, `set -m`) is out of scope, but job *specifications*
//! (`%1`, `%%`, `%-`) are supported for the jobs this shell starts, because
//! `wait %%` and `kill -15 %%` are how the suite exercises signal delivery.
//!
//! Both are intrinsic utilities: a bad operand is a status, never a
//! special-built-in failure.

use std::time::Duration;

use bsh_runtime::{ShellHost, Status, signal_number, signal_short_name, signal_table};

use super::Interp;

/// How a wait ended.
#[derive(Clone, Copy)]
enum Waited {
    /// The child's exit status.
    Status(Status),
    /// The pid is not a child of this shell (or was already waited for).
    NoChild,
    /// The operand was not a pid or job specification.
    Invalid,
    /// A job specification that names no job.
    NoSuchJob,
}

fn usage(interp: &mut Interp, argv: &[Vec<u8>], message: &str) -> Status {
    let name = super::traps::builtin_name(argv, "wait");
    interp.report(crate::codes::DEFERRED, 0..0, format!("{name}: {message}"));
    Status(2)
}

impl Interp {
    /// `wait [-n] [pid | %job]…`
    pub(crate) fn bi_wait(&mut self, argv: &[Vec<u8>]) -> Status {
        let mut next = false;
        let mut index = 1;
        while index < argv.len() {
            let argument = &argv[index];
            if argument == b"--" {
                index += 1;
                break;
            }
            if argument.len() > 1 && argument[0] == b'-' {
                for &flag in &argument[1..] {
                    if flag == b'n' {
                        next = true;
                    } else {
                        return usage(self, argv, "invalid option");
                    }
                }
                index += 1;
                continue;
            }
            break;
        }
        let operands = &argv[index..];

        // Signals that arrive while the shell waits behave like the ones that
        // arrive during a foreground command: they run afterwards, and
        // `SIGINT`/`SIGQUIT` are discarded (bash ignores them while waiting).
        let guard = self.ignore_int_quit();
        let status = if next {
            self.wait_next(operands)
        } else if operands.is_empty() {
            self.wait_all()
        } else {
            self.wait_operands(operands)
        };
        drop(guard);

        if let Some(exit) = self.after_foreground_wait() {
            self.state.request_exit(exit);
            return exit;
        }
        status
    }

    /// Resolve one `wait`/`kill` operand to a pid.
    fn operand_pid(&mut self, operand: &[u8], program: &str) -> Result<i32, Waited> {
        if let Some(job) = operand.strip_prefix(b"%") {
            return match self.resolve_job(job) {
                Some(pid) => Ok(pid),
                None => {
                    self.report(
                        crate::codes::DEFERRED,
                        0..0,
                        format!(
                            "{program}: {}: no such job",
                            String::from_utf8_lossy(operand)
                        ),
                    );
                    Err(Waited::NoSuchJob)
                }
            };
        }
        match std::str::from_utf8(operand)
            .ok()
            .and_then(|text| text.parse().ok())
        {
            Some(pid) if pid > 0 => Ok(pid),
            _ => {
                self.report(
                    crate::codes::DEFERRED,
                    0..0,
                    format!(
                        "{program}: {}: arguments must be process or job IDs",
                        String::from_utf8_lossy(operand)
                    ),
                );
                Err(Waited::Invalid)
            }
        }
    }

    /// `%N` / `%%` / `%+` / `%-`: the jobs this shell started, in order.
    fn resolve_job(&self, name: &[u8]) -> Option<i32> {
        let jobs = &self.jobs;
        if name.iter().all(u8::is_ascii_digit) && !name.is_empty() {
            let index: usize = std::str::from_utf8(name).ok()?.parse().ok()?;
            return jobs.get(index.checked_sub(1)?).copied();
        }
        match name {
            b"" | b"%" | b"+" => jobs.last().copied(),
            b"-" => jobs.get(jobs.len().checked_sub(2)?).copied(),
            _ => None,
        }
    }

    /// Report a child that was killed by a signal, the way bash's job
    /// notification does (`Hangup`, `Killed`, …). The suite greps this text.
    fn report_signal_death(&mut self, outcome: bsh_sys::WaitOutcome) {
        if let bsh_sys::WaitOutcome::Signaled(signal) = outcome {
            let mut line = signal_description(signal).as_bytes().to_vec();
            line.push(b'\n');
            let _ = self.write_err(&line);
        }
    }

    /// `wait PID`: the child's status, or 127 when it is not this shell's child.
    fn wait_one(&mut self, pid: i32) -> Waited {
        if let Some(slot) = self.background.get_mut(&pid) {
            if let Some(status) = slot.take() {
                return Waited::Status(status);
            }
            return match bsh_sys::wait_pid(pid, false) {
                Ok(outcome) => {
                    self.report_signal_death(outcome);
                    self.background.remove(&pid);
                    Waited::Status(Status(outcome.exit_status()))
                }
                Err(error) if error.raw_os_error() == Some(bsh_sys::ECHILD) => {
                    self.background.remove(&pid);
                    Waited::NoChild
                }
                Err(_) => Waited::NoChild,
            };
        }
        // A child that exists but was never registered (defensive): waiting is
        // still correct, and ECHILD means it is not ours.
        match bsh_sys::wait_pid(pid, false) {
            Ok(outcome) => {
                self.report_signal_death(outcome);
                Waited::Status(Status(outcome.exit_status()))
            }
            Err(_) => Waited::NoChild,
        }
    }

    fn wait_operands(&mut self, operands: &[Vec<u8>]) -> Status {
        let mut status = Status::SUCCESS;
        for operand in operands {
            let pid = match self.operand_pid(operand, "wait") {
                Ok(pid) => pid,
                Err(waited) => {
                    status = status_of(waited);
                    continue;
                }
            };
            let waited = self.wait_one(pid);
            self.forget_job(pid);
            status = status_of(waited);
        }
        status
    }

    /// `wait` with no operand: every child, and the status is 0 even when the
    /// children failed (the suite pins that "the status is lost").
    fn wait_all(&mut self) -> Status {
        loop {
            self.reap_background();
            let pending: Vec<i32> = self
                .background
                .iter()
                .filter_map(|(pid, status)| status.is_none().then_some(*pid))
                .collect();
            if pending.is_empty() {
                return Status::SUCCESS;
            }
            for pid in pending {
                match bsh_sys::wait_pid(pid, false) {
                    Ok(outcome) => {
                        if let Some(slot) = self.background.get_mut(&pid) {
                            *slot = Some(Status(outcome.exit_status()));
                        }
                    }
                    // Not a child any more: drop it so the loop terminates.
                    Err(_) => {
                        self.background.remove(&pid);
                        self.forget_job(pid);
                    }
                }
            }
        }
    }

    /// `wait -n`: the first child to finish, optionally restricted to the
    /// operands.
    ///
    /// "First to finish" is not "first in the list", so this polls with
    /// `wait_any_nohang` (the non-blocking reap) rather than blocking on one
    /// pid: `sleep 0.09 & sleep 0.03 & wait -n` must report the 0.03 one.
    fn wait_next(&mut self, operands: &[Vec<u8>]) -> Status {
        // Validate every operand first: bash reports each bad one, and
        // `wait -n $! bad` still exits 127.
        let mut invalid = false;
        let mut remaining: Vec<i32> = Vec::new();
        for operand in operands {
            match self.operand_pid(operand, "wait") {
                Ok(pid) => remaining.push(pid),
                Err(_) => invalid = true,
            }
        }
        if invalid {
            self.reap_background();
            return Status(127);
        }
        loop {
            self.reap_background();
            let candidates: Vec<i32> = if operands.is_empty() {
                self.background.keys().copied().collect()
            } else {
                remaining.clone()
            };
            for pid in &candidates {
                if let Some(Some(status)) = self.background.get(pid).copied() {
                    self.forget_job(*pid);
                    return status;
                }
            }
            if candidates.is_empty() {
                return Status(127);
            }
            // Reap whatever finished; a 1 ms poll keeps a shell script's
            // `wait -n` loop responsive without a busy spin.
            match bsh_sys::wait_any_nohang() {
                Some((pid, outcome)) => {
                    let status = Status(outcome.exit_status());
                    match self.background.entry(pid) {
                        std::collections::btree_map::Entry::Occupied(mut slot) => {
                            slot.insert(Some(status));
                        }
                        std::collections::btree_map::Entry::Vacant(slot) => {
                            // A child this shell did not record (defensive):
                            // it is still a valid answer for an
                            // unrestricted `-n`.
                            slot.insert(Some(status));
                            if operands.is_empty() {
                                self.background.remove(&pid);
                                return status;
                            }
                        }
                    }
                }
                None => {
                    // Nothing ready: if every listed pid is gone, give up.
                    if !operands.is_empty() {
                        let alive = remaining
                            .iter()
                            .any(|pid| self.background.contains_key(pid));
                        if !alive {
                            return Status(127);
                        }
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
        }
    }

    /// Drop `pid` from the job table once it has been waited for.
    fn forget_job(&mut self, pid: i32) {
        self.jobs.retain(|candidate| *candidate != pid);
        // The status is reported once; a second `wait PID` must say 127.
        self.background.remove(&pid);
    }

    /// `kill [-s SIG | -SIG | -n NUM | -l | -L] pid | %job …`
    pub(crate) fn bi_kill(&mut self, argv: &[Vec<u8>]) -> Status {
        let mut list = false;
        let mut signal: Option<i32> = None;
        let mut index = 1;
        while index < argv.len() {
            let argument = &argv[index];
            if argument == b"--" {
                index += 1;
                break;
            }
            if argument.len() > 1 && argument[0] == b'-' {
                match &argument[1..] {
                    b"l" | b"L" => {
                        list = true;
                        index += 1;
                        continue;
                    }
                    b"s" => {
                        let Some(value) = argv.get(index + 1) else {
                            return usage(self, argv, "option requires an argument");
                        };
                        match signal_number(value) {
                            Some(number) => signal = Some(number),
                            None => {
                                return self.kill_bad_signal(argv, value);
                            }
                        }
                        index += 2;
                        continue;
                    }
                    b"n" => {
                        let Some(value) = argv.get(index + 1) else {
                            return usage(self, argv, "option requires an argument");
                        };
                        match parse_signal_number(value) {
                            Some(number) => signal = Some(number),
                            None => {
                                return self.kill_bad_signal(argv, value);
                            }
                        }
                        index += 2;
                        continue;
                    }
                    option if !option.is_empty() => {
                        // `kill -TERM`, `kill -9`, `kill -Sigterm`.
                        match parse_signal_number(option) {
                            Some(number) => signal = Some(number),
                            None => {
                                return self.kill_bad_signal(argv, option);
                            }
                        }
                        index += 1;
                        continue;
                    }
                    _ => break,
                }
            }
            break;
        }
        let operands = &argv[index..];

        if list {
            return self.kill_list(operands);
        }
        let signal = signal.unwrap_or(bsh_sys::SIGTERM);
        let mut status = Status::SUCCESS;
        for operand in operands {
            let pid = match self.operand_pid(operand, "kill") {
                Ok(pid) => pid,
                Err(waited) => {
                    status = status_of(waited);
                    continue;
                }
            };
            if let Err(error) = bsh_sys::send_signal(pid, signal) {
                self.report(
                    crate::codes::DEFERRED,
                    0..0,
                    format!("kill: {}: {error}", String::from_utf8_lossy(operand)),
                );
                status = Status::FAILURE;
            }
        }
        status
    }

    fn kill_bad_signal(&mut self, argv: &[Vec<u8>], value: &[u8]) -> Status {
        let name = super::traps::builtin_name(argv, "kill");
        self.report(
            crate::codes::DEFERRED,
            0..0,
            format!(
                "{name}: {}: invalid signal specification",
                String::from_utf8_lossy(value)
            ),
        );
        Status::FAILURE
    }

    fn kill_list(&mut self, operands: &[Vec<u8>]) -> Status {
        if operands.is_empty() {
            let table = signal_table();
            return self.write_out(&table);
        }
        let mut out = Vec::new();
        let mut status = Status::SUCCESS;
        for operand in operands {
            let numeric = !operand.is_empty() && operand.iter().all(u8::is_ascii_digit);
            if numeric {
                let Some(value) = std::str::from_utf8(operand)
                    .ok()
                    .and_then(|text| text.parse::<i32>().ok())
                else {
                    status = Status::FAILURE;
                    continue;
                };
                let name = if value == 0 {
                    Some(b"EXIT".to_vec())
                } else if (1..=64).contains(&value) {
                    signal_short_name(value).map(|name| name.as_bytes().to_vec())
                } else if value > 128 {
                    // An exit status: 134 is 128 + SIGABRT.
                    let signal = value - 128;
                    (1..=64)
                        .contains(&signal)
                        .then(|| signal_short_name(signal))
                        .flatten()
                        .map(|name| name.as_bytes().to_vec())
                } else {
                    None
                };
                match name {
                    Some(name) => {
                        out.extend_from_slice(&name);
                        out.push(b'\n');
                    }
                    None => status = Status::FAILURE,
                }
            } else {
                match signal_number(operand) {
                    Some(number) => {
                        out.extend_from_slice(number.to_string().as_bytes());
                        out.push(b'\n');
                    }
                    None => status = Status::FAILURE,
                }
            }
        }
        let write = self.write_out(&out);
        if !write.is_success() { write } else { status }
    }
}

/// A signal for `kill`'s option forms: `0` (the existence check) is accepted
/// and 1..=64 must name a real signal.
fn parse_signal_number(bytes: &[u8]) -> Option<i32> {
    if !bytes.is_empty() && bytes.iter().all(u8::is_ascii_digit) {
        let value: i32 = std::str::from_utf8(bytes).ok()?.parse().ok()?;
        return (0..=64).contains(&value).then_some(value);
    }
    signal_number(bytes)
}

/// The English description of a signal, as the job-status message prints it.
///
/// Only the names bash uses in its "terminated by signal" notification; an
/// unknown signal still gets a description so the message is never empty.
fn signal_description(signal: i32) -> &'static str {
    match signal {
        1 => "Hangup",
        2 => "Interrupt",
        3 => "Quit",
        4 => "Illegal instruction",
        6 => "Aborted",
        8 => "Floating point exception",
        9 => "Killed",
        11 => "Segmentation fault",
        13 => "Broken pipe",
        14 => "Alarm clock",
        15 => "Terminated",
        _ => "Killed",
    }
}

fn status_of(waited: Waited) -> Status {
    match waited {
        Waited::Status(status) => status,
        Waited::NoChild | Waited::NoSuchJob => Status(127),
        Waited::Invalid => Status(2),
    }
}
