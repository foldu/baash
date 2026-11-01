//! Child processes: `wait`, `kill`, and the one foreground-wait path.
//!
//! There is no job control. A background command is a real child remembered by
//! pid (so `$!`, `wait` and `kill` work), and a foreground command is waited
//! for directly. `%` job specifications and the job built-ins are rejected
//! with [`baash_lang::codes::CAPABILITY`] instead of being approximated: the shell
//! has no job table to resolve them against.
//!
//! `wait` keeps the shell's `SIGINT`/`SIGQUIT` ignore around it, and every
//! foreground site (`run_external`, `run_pipeline`, `eval_subshell`,
//! `capture_substitution`) goes through [`Interp::wait_foreground_children`],
//! so `Ctrl-C` kills the child without killing the shell on every path.

use std::time::Duration;

use baash_runtime::{ShellHost, Status, signal_number, signal_short_name, signal_table};

use super::Interp;

impl Interp {
    /// `jobs`, `fg`, `bg`, `disown`: known names, deliberately unavailable.
    ///
    /// They stay registered so `type`/`command -v` still find them and an rc
    /// file does not get "command not found"; running one is an error
    /// (`BAASH-E0302`), not a silent no-op.
    pub(crate) fn bi_job_control_unavailable(&mut self, argv: &[Vec<u8>]) -> Status {
        let name = super::traps::builtin_name(argv, "jobs");
        self.report(
            baash_lang::codes::CAPABILITY,
            0..0,
            format!("{name}: job control is not implemented in this shell"),
        );
        Status::FAILURE
    }

    /// Report a `%` specification to `wait`/`kill` as the capability gap it is.
    fn reject_job_spec(&mut self, program: &str, operand: &[u8]) -> Status {
        self.report(
            baash_lang::codes::CAPABILITY,
            0..0,
            format!(
                "{program}: {}: job control is not implemented in this shell",
                bytes_text(operand)
            ),
        );
        Status::FAILURE
    }

    /// `wait [-n] [pid]…`
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
    ///
    /// A `%` specification is the one operand shape the shell cannot honor:
    /// there is no job table, so it is a capability error (status 1), not a
    /// "no such job" that would suggest a table exists.
    fn operand_pid(&mut self, operand: &[u8], program: &str) -> Result<i32, Status> {
        if operand.first() == Some(&b'%') {
            return Err(self.reject_job_spec(program, operand));
        }
        match std::str::from_utf8(operand)
            .ok()
            .and_then(|text| text.parse().ok())
        {
            Some(pid) if pid > 0 => Ok(pid),
            _ => {
                self.report(
                    baash_runtime::codes::USAGE,
                    0..0,
                    format!(
                        "{program}: {}: arguments must be process IDs",
                        bytes_text(operand)
                    ),
                );
                Err(Status::USAGE)
            }
        }
    }

    /// Report a child that was killed by a signal, the way bash's job
    /// notification does (`Hangup`, `Killed`, …). The suite greps this text.
    fn report_signal_death(&mut self, outcome: baash_sys::WaitOutcome) {
        if let baash_sys::WaitOutcome::Signaled(signal) = outcome {
            let mut line = signal_description(signal).as_bytes().to_vec();
            line.push(b'\n');
            let _ = self.write_err(&line);
        }
    }

    /// `wait PID`: the child's status, or 127 when it is not this shell's child.
    fn wait_one(&mut self, pid: i32) -> Status {
        if let Some(slot) = self.background.get_mut(&pid) {
            if let Some(status) = slot.take() {
                self.background.remove(&pid);
                return status;
            }
        }
        let status = match baash_sys::wait_pid(pid) {
            Ok(outcome) => {
                self.report_signal_death(outcome);
                Some(Status(outcome.exit_status()))
            }
            Err(error) if error.raw_os_error() == Some(baash_sys::ECHILD) => None,
            Err(_) => None,
        };
        // The status is reported once; a second `wait PID` must say 127.
        self.background.remove(&pid);
        status.unwrap_or(Status(127))
    }

    fn wait_operands(&mut self, operands: &[Vec<u8>]) -> Status {
        let mut status = Status::SUCCESS;
        for operand in operands {
            match self.operand_pid(operand, "wait") {
                Err(failure) => status = failure,
                Ok(pid) => status = self.wait_one(pid),
            }
        }
        status
    }

    /// `wait` with no operand: every live child, and the status is 0 even when
    /// the children failed (the suite pins that "the status is lost").
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
                match baash_sys::wait_pid(pid) {
                    Ok(_) => {
                        // `wait` consumed the child: a second `wait PID` must
                        // say 127.
                        self.background.remove(&pid);
                    }
                    // Not a child any more: drop it so the loop terminates.
                    Err(_) => {
                        self.background.remove(&pid);
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
        // `wait -n $! bad` still exits 127. A `%` operand is a capability
        // error and decides the whole command.
        let mut remaining: Vec<i32> = Vec::new();
        for operand in operands {
            match self.operand_pid(operand, "wait") {
                Ok(pid) => remaining.push(pid),
                Err(failure) => {
                    self.reap_background();
                    // A bad pid is 127 here (bash's answer for `wait -n`); a
                    // `%` specification is the capability error in its own
                    // right.
                    return if failure == Status::USAGE {
                        Status(127)
                    } else {
                        failure
                    };
                }
            }
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
                    self.background.remove(pid);
                    return status;
                }
            }
            if candidates.is_empty() {
                return Status(127);
            }
            // Reap whatever finished; a 1 ms poll keeps a shell script's
            // `wait -n` loop responsive without a busy spin.
            match baash_sys::wait_any_nohang() {
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
                    if !operands.is_empty()
                        && !remaining
                            .iter()
                            .any(|pid| self.background.contains_key(pid))
                    {
                        return Status(127);
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
        }
    }

    /// `kill [-s SIG | -SIG | -n NUM | -l | -L] pid …`
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
        let signal = signal.unwrap_or(baash_sys::SIGTERM);
        let mut status = Status::SUCCESS;
        for operand in operands {
            let pid = match self.operand_pid(operand, "kill") {
                Ok(pid) => pid,
                Err(failure) => {
                    status = failure;
                    continue;
                }
            };
            if let Err(error) = baash_sys::send_signal(pid, signal) {
                self.report(
                    crate::codes::DEFERRED,
                    0..0,
                    format!("kill: {}: {error}", bytes_text(operand)),
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
                bytes_text(value)
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

    /// Wait for a foreground command's children.
    ///
    /// This is the single wait path every foreground site uses
    /// (`run_external`, `run_pipeline`, `eval_subshell` and command
    /// substitution), so the `SIGINT`/`SIGQUIT` ignore cannot drift between
    /// them. The shell and its child share a process group, so there is no
    /// terminal handover here.
    pub(crate) fn wait_foreground_children(
        &mut self,
        pids: &[i32],
    ) -> std::io::Result<Vec<baash_sys::WaitOutcome>> {
        let guard = self.ignore_int_quit();
        let mut outcomes = Vec::with_capacity(pids.len());
        for pid in pids {
            match baash_sys::wait_pid(*pid) {
                Ok(outcome) => outcomes.push(outcome),
                Err(error) => {
                    drop(guard);
                    return Err(error);
                }
            }
        }
        drop(guard);
        Ok(outcomes)
    }
}

fn usage(interp: &mut Interp, argv: &[Vec<u8>], message: &str) -> Status {
    let name = super::traps::builtin_name(argv, "wait");
    interp.report(
        baash_runtime::codes::USAGE,
        0..0,
        format!("{name}: {message}"),
    );
    Status::USAGE
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

fn bytes_text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}
