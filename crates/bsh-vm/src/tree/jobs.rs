//! Job control: the job table, job specifications, and every job built-in.
//!
//! DESIGN.md §7 item 4. An interactive shell puts each foreground command (and
//! each `&` command) into its own process group, hands that group the terminal
//! with `tcsetpgrp`, waits with `WUNTRACED`, and takes the terminal back. A
//! child that stops becomes a *job* the user can list, resume, foreground or
//! disown; the shell itself ignores `SIGTTOU`, `SIGTTIN` and `SIGTSTP`, so
//! `Ctrl-Z` reaches the child and never the prompt.
//!
//! The table is also maintained without job control (a script's `&` children
//! are still jobs), but process groups and terminal handover are used only
//! when [`Interp::foreground_job_control`] is true: interactive, `monitor` on,
//! and [`Interp::enable_job_control`] has run. When it is false the old
//! `wait`/`kill` status semantics are unchanged.
//!
//! Formats are measured against bash 5.3.20, not guessed:
//!
//! ```text
//! [1]+  Running                    sleep 100 &
//! [1]+     5 Stopped                    sleep 100
//! [1]+  Done                       sleep 0.1
//! [1]+  Exit 3                     sh -c 'exit 3'
//! ```
//!
//! The status word is left-justified in a 27-column field, so the command
//! starts at column 33 (column 38 with `-l`, after a five-wide pid). A
//! multi-process job's `-l` listing continues on extra lines whose `|` starts
//! at column 36. Notifications use the same marker (`+`/`-`/blank) and status
//! field.

use std::time::Duration;

use bsh_runtime::{ShellHost, Status, signal_number, signal_short_name, signal_table};

use super::Interp;

/// The state of one job.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum JobState {
    /// At least one member has not been reaped.
    Running,
    /// Every member is stopped with this signal; resuming continues the job.
    Stopped {
        /// The signal that stopped the job.
        signal: i32,
    },
    /// Every member has exited; `status` is the last member's status.
    Done {
        /// The status `$?` reports.
        status: Status,
        /// The terminating signal, when the job died by one.
        signal: Option<i32>,
    },
}

/// One job: a pipeline (or a single command) the shell is responsible for.
#[derive(Clone, Debug)]
pub(crate) struct Job {
    /// The number `%N` names (1-based, stable while the job lives).
    pub(crate) number: usize,
    /// The job's process group. Used for signals and the terminal.
    pub(crate) pgid: i32,
    /// Member pids, in pipeline order.
    pub(crate) pids: Vec<i32>,
    /// Per-member display text, parallel to `pids` (for `jobs -l`).
    pub(crate) parts: Vec<Vec<u8>>,
    /// The whole command as the user wrote it, for display.
    pub(crate) command: Vec<u8>,
    /// Current state.
    pub(crate) state: JobState,
    /// How many members are still expected to be reaped before `Done`.
    pub(crate) running: usize,
    /// Whether the last state change has already been reported.
    pub(crate) notified: bool,
    /// Cleared by `disown -h`: the job is still listed but not hung up.
    pub(crate) hup: bool,
}

/// The job table, with the `+`/`-` markers bash gives `jobs`.
///
/// The markers are not a creation order. Measured behaviour:
///
/// * the current job is the most recently *stopped* job; with no stopped job
///   it is the highest-numbered job;
/// * the previous job is the second most recently stopped job; with fewer
///   than two stopped jobs it is the highest-numbered other job.
///
/// That explains every observed case, including `bg` on a middle stopped job
/// demoting it out of both markers.
#[derive(Clone, Debug, Default)]
pub(crate) struct JobTable {
    jobs: Vec<Job>,
    /// Job numbers, oldest stop first.
    stop_order: Vec<usize>,
}

impl JobTable {
    /// Jobs in job-number order, which is how `jobs` prints them.
    pub(crate) fn iter(&self) -> impl Iterator<Item = &Job> {
        self.jobs.iter()
    }

    /// The number of jobs in the table.
    pub(crate) fn len(&self) -> usize {
        self.jobs.len()
    }

    /// The job numbered `number`.
    pub(crate) fn get(&self, number: usize) -> Option<&Job> {
        self.jobs.iter().find(|job| job.number == number)
    }

    /// The job numbered `number`, mutably.
    pub(crate) fn get_mut(&mut self, number: usize) -> Option<&mut Job> {
        self.jobs.iter_mut().find(|job| job.number == number)
    }

    /// The job containing `pid`.
    pub(crate) fn find_by_pid(&self, pid: i32) -> Option<&Job> {
        self.jobs.iter().find(|job| job.pids.contains(&pid))
    }

    /// Insert a job, giving it the lowest free positive number.
    ///
    /// bash reuses a freed number immediately, so a table with one live job
    /// always numbers the next one two, never `max + 1`.
    pub(crate) fn insert(&mut self, mut job: Job) -> usize {
        let mut number = 1;
        while self.get(number).is_some() {
            number += 1;
        }
        job.number = number;
        self.jobs.push(job);
        self.jobs.sort_by_key(|job| job.number);
        number
    }

    /// Remove a job and its rotation history.
    pub(crate) fn remove(&mut self, number: usize) -> Option<Job> {
        let index = self.jobs.iter().position(|job| job.number == number)?;
        self.stop_order.retain(|candidate| *candidate != number);
        Some(self.jobs.remove(index))
    }

    /// Record that `number` just stopped; it becomes the current job.
    pub(crate) fn touch_stop(&mut self, number: usize) {
        self.stop_order.retain(|candidate| *candidate != number);
        self.stop_order.push(number);
    }

    /// Forget that `number` is stopped (it was resumed).
    pub(crate) fn clear_stop(&mut self, number: usize) {
        self.stop_order.retain(|candidate| *candidate != number);
    }

    /// The current job number (`+`).
    pub(crate) fn current(&self) -> Option<usize> {
        if let Some(number) = self.stop_order.last() {
            return Some(*number);
        }
        self.jobs.last().map(|job| job.number)
    }

    /// The previous job number (`-`).
    pub(crate) fn previous(&self) -> Option<usize> {
        if self.stop_order.len() >= 2 {
            return Some(self.stop_order[self.stop_order.len() - 2]);
        }
        let current = self.current();
        self.jobs
            .iter()
            .rev()
            .map(|job| job.number)
            .find(|number| Some(*number) != current)
    }

    /// The marker byte `jobs` prints after `[N]`.
    pub(crate) fn marker(&self, number: usize) -> u8 {
        if self.current() == Some(number) {
            b'+'
        } else if self.previous() == Some(number) {
            b'-'
        } else {
            b' '
        }
    }
}

/// Why a `%` job specification did not resolve.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SpecError {
    /// No job matches.
    NoSuch {
        /// The specification as written, without `%`.
        spec: Vec<u8>,
    },
    /// More than one job's command matches.
    Ambiguous {
        /// The specification as written, without `%`.
        spec: Vec<u8>,
    },
}

impl Interp {
    /// Put the shell in its own process group, take over the terminal, enable
    /// `monitor`, and ignore the job-control signals in the shell itself.
    ///
    /// Called once by the interactive startup path, before the first prompt.
    /// The signal ignore is permanent for the process: it is what lets
    /// `tcsetpgrp` run while the shell is in the background and what makes
    /// `Ctrl-Z` stop a child rather than the prompt. Children get the default
    /// dispositions back in [`Interp::enter_child`] and through
    /// [`Interp::child_reset_signals`].
    pub fn enable_job_control(&mut self) -> std::io::Result<()> {
        let pid = bsh_sys::pid();
        let mut pgid = bsh_sys::process_group();
        if pgid != pid {
            bsh_sys::set_process_group(0, 0)?;
            pgid = pid;
        }
        self.shell_pgid = pgid;
        self.job_control = true;
        self.state.opts.monitor = true;
        // Ignore, not block: `exec` preserves `SIG_IGN`, which is exactly what
        // the shell wants across a fork; `child_reset_signals` undoes it for
        // every child that must be stoppable.
        let guard =
            bsh_sys::ignore_signals(&[bsh_sys::SIGTTOU, bsh_sys::SIGTTIN, bsh_sys::SIGTSTP])?;
        // The guard's `Drop` would restore the shell's ability to be stopped.
        // This is a process-lifetime decision, so keep it.
        std::mem::forget(guard);
        self.ignored_job_signals = vec![bsh_sys::SIGTTOU, bsh_sys::SIGTTIN, bsh_sys::SIGTSTP];
        let _ = bsh_sys::set_foreground_pgrp(pgid);
        Ok(())
    }

    /// Give the terminal back to the shell. Idempotent and error-tolerant: an
    /// interactive shell that cannot get its terminal back is already in
    /// trouble, and a failed `tcsetpgrp` must not abort the prompt loop.
    pub fn restore_terminal(&self) {
        if self.job_control {
            let _ = bsh_sys::set_foreground_pgrp(self.shell_pgid);
        }
    }

    /// Whether a foreground command should get its own process group and the
    /// terminal. `monitor` can be turned off with `set +m`; the job table keeps
    /// working, but no handover happens.
    pub(crate) fn foreground_job_control(&self) -> bool {
        self.job_control && self.state.opts.monitor
    }

    /// Whether a `&` command should start its own process group.
    pub(crate) fn background_job_control(&self) -> bool {
        self.job_control && self.state.opts.monitor
    }

    /// The shell's own process group, after [`Interp::enable_job_control`].
    pub fn shell_pgid(&self) -> i32 {
        self.shell_pgid
    }

    /// Signal a job: its process group when job control gave it one, its first
    /// pid otherwise. A job created before `set -m` has a pid-shaped `pgid`
    /// that is not a real group, so an `ESRCH` falls back to the pid.
    fn signal_job(&self, number: usize, signal: i32) -> std::io::Result<()> {
        let Some(job) = self.job_table.get(number) else {
            return Ok(());
        };
        let first = job.pids.first().copied();
        let result = if self.job_control && job.pgid > 0 {
            bsh_sys::send_group_signal(job.pgid, signal)
        } else if let Some(pid) = first {
            bsh_sys::send_signal(pid, signal)
        } else {
            Ok(())
        };
        match result {
            Ok(()) => Ok(()),
            Err(_) if first.is_some() => bsh_sys::send_signal(first.unwrap_or_default(), signal),
            Err(error) => Err(error),
        }
    }

    /// Print a bash-style warning to stderr (no diagnostic code).
    fn warn(&mut self, text: &str) {
        let mut line = b"bsh: ".to_vec();
        line.extend_from_slice(text.as_bytes());
        line.push(b'\n');
        let _ = self.write_err(&line);
    }

    /// Record a background child as a job, returning its job number.
    pub(crate) fn track_background(
        &mut self,
        pid: i32,
        pgid: i32,
        command: Vec<u8>,
        parts: Vec<Vec<u8>>,
    ) -> usize {
        self.background.insert(pid, None);
        let number = self.job_table.insert(Job {
            number: 0,
            pgid,
            pids: vec![pid],
            parts,
            command,
            state: JobState::Running,
            running: 1,
            notified: false,
            hup: true,
        });
        // `[1] 1234` is bash's interactive start notice.
        if self.interactive {
            let line = format!("[{number}] {pid}\n");
            let _ = self.write_err(line.as_bytes());
        }
        number
    }

    /// Apply one reaped state change to the job table.
    ///
    /// Used by the non-blocking drain; a foreground wait updates its job
    /// directly because it knows whether it is creating or resuming one.
    pub(crate) fn note_reaped(&mut self, pid: i32, outcome: bsh_sys::WaitOutcome) {
        let status = Status(outcome.exit_status());
        match outcome {
            bsh_sys::WaitOutcome::Stopped(signal) => {
                let Some(number) = self.job_table.find_by_pid(pid).map(|job| job.number) else {
                    return;
                };
                let pids_len = self
                    .job_table
                    .get(number)
                    .map(|job| job.pids.len())
                    .unwrap_or(1);
                if let Some(job) = self.job_table.get_mut(number) {
                    if matches!(job.state, JobState::Running) {
                        job.state = JobState::Stopped { signal };
                        job.running = pids_len;
                        job.notified = false;
                    }
                }
                self.job_table.touch_stop(number);
            }
            bsh_sys::WaitOutcome::Exited(_) | bsh_sys::WaitOutcome::Signaled(_) => {
                if let Some(slot) = self.background.get_mut(&pid) {
                    *slot = Some(status);
                }
                let Some(number) = self.job_table.find_by_pid(pid).map(|job| job.number) else {
                    return;
                };
                let signalled = match outcome {
                    bsh_sys::WaitOutcome::Signaled(signal) => Some(signal),
                    _ => None,
                };
                let mut done = false;
                if let Some(job) = self.job_table.get_mut(number) {
                    job.running = job.running.saturating_sub(1);
                    if job.running == 0 || matches!(job.state, JobState::Stopped { .. }) {
                        job.state = JobState::Done {
                            status,
                            signal: signalled,
                        };
                        job.notified = false;
                        done = true;
                    }
                }
                if done {
                    self.job_table.clear_stop(number);
                }
            }
        }
    }

    /// Report finished and stopped jobs the way bash does before a prompt, to
    /// stderr. `Done` jobs are removed once reported; stopped jobs stay.
    ///
    /// Returns the bytes written (also useful to tests).
    pub fn emit_job_notices(&mut self) -> Vec<u8> {
        self.reap_background();
        let mut numbers: Vec<usize> = self
            .job_table
            .iter()
            .filter(|job| {
                !job.notified
                    && matches!(job.state, JobState::Done { .. } | JobState::Stopped { .. })
            })
            .map(|job| job.number)
            .collect();
        numbers.sort_unstable();
        let mut out = Vec::new();
        for number in numbers {
            let Some(job) = self.job_table.get(number) else {
                continue;
            };
            let line = notice_line(&self.job_table, job);
            out.extend_from_slice(&line);
            out.push(b'\n');
            let done = matches!(
                self.job_table.get(number).map(|job| job.state),
                Some(JobState::Done { .. })
            );
            if let Some(job) = self.job_table.get_mut(number) {
                job.notified = true;
            }
            if done {
                self.remove_job(number);
            }
        }
        if !out.is_empty() {
            let _ = self.write_err(&out);
        }
        out
    }

    /// Drop a job from the table and forget its pids for `wait`.
    fn remove_job(&mut self, number: usize) {
        if let Some(job) = self.job_table.remove(number) {
            for pid in job.pids {
                self.background.remove(&pid);
            }
        }
    }

    /// Resolve a `%` specification to a job number.
    ///
    /// Supports `%N`, `%%`, `%+`, `%-`, `%prefix` and `%?substring`.
    pub(crate) fn resolve_job_spec(&self, spec: &[u8]) -> Result<usize, SpecError> {
        let no_such = || SpecError::NoSuch {
            spec: spec.to_vec(),
        };
        if spec.is_empty() || spec == b"+" {
            return self.job_table.current().ok_or_else(no_such);
        }
        if spec == b"-" {
            return self.job_table.previous().ok_or_else(no_such);
        }
        if spec.iter().all(u8::is_ascii_digit) {
            let number: usize = std::str::from_utf8(spec)
                .ok()
                .and_then(|text| text.parse().ok())
                .ok_or_else(no_such)?;
            return self
                .job_table
                .get(number)
                .map(|job| job.number)
                .ok_or_else(no_such);
        }
        let (needle, contains) = match spec.strip_prefix(b"?") {
            Some(rest) => (rest, true),
            None => (spec, false),
        };
        let matches: Vec<usize> = self
            .job_table
            .iter()
            .filter(|job| {
                if contains {
                    job.command
                        .windows(needle.len())
                        .any(|window| window == needle)
                } else {
                    job.command.starts_with(needle)
                }
            })
            .map(|job| job.number)
            .collect();
        match matches.as_slice() {
            [] => Err(no_such()),
            [only] => Ok(*only),
            _ => Err(SpecError::Ambiguous {
                spec: spec.to_vec(),
            }),
        }
    }

    /// Resolve a `%` specification to a [job number], reporting the bash
    /// message for a failure. Returns `None` when the caller should continue
    /// with status 1.
    fn resolve_job_operand(&mut self, operand: &[u8], program: &str) -> Option<usize> {
        let spec = job_spec(operand);
        match self.resolve_job_spec(spec) {
            Ok(number) => Some(number),
            Err(SpecError::NoSuch { spec }) => {
                self.report(
                    bsh_runtime::codes::NOT_FOUND,
                    0..0,
                    format!("{program}: %{}: no such job", bytes_text(&spec)),
                );
                None
            }
            Err(SpecError::Ambiguous { spec }) => {
                self.report(
                    bsh_runtime::codes::USAGE,
                    0..0,
                    format!("{program}: {}: ambiguous job spec", bytes_text(&spec)),
                );
                None
            }
        }
    }

    /// `jobs [-l] [-p] [-r] [-s] [spec…]`
    pub(crate) fn bi_jobs(&mut self, argv: &[Vec<u8>]) -> Status {
        let mut long = false;
        let mut pids_only = false;
        let mut running_only = false;
        let mut stopped_only = false;
        let mut index = 1;
        while index < argv.len() {
            let argument = &argv[index];
            if argument == b"--" {
                index += 1;
                break;
            }
            if argument.len() > 1 && argument[0] == b'-' {
                for &flag in &argument[1..] {
                    match flag {
                        b'l' => long = true,
                        b'p' => pids_only = true,
                        b'r' => running_only = true,
                        b's' => stopped_only = true,
                        other => {
                            self.report(
                                bsh_runtime::codes::USAGE,
                                0..0,
                                format!("jobs: -{}: invalid option", char::from(other)),
                            );
                            let _ = self.write_err(
                                b"jobs: usage: jobs [-lnprs] [jobspec ...] or jobs -x command [args]\n",
                            );
                            return Status::USAGE;
                        }
                    }
                }
                index += 1;
                continue;
            }
            break;
        }

        let operands = &argv[index..];
        let mut status = Status::SUCCESS;
        let mut selected: Option<Vec<usize>> = if operands.is_empty() {
            None
        } else {
            let mut numbers = Vec::new();
            for operand in operands {
                match self.resolve_job_operand(operand, "jobs") {
                    Some(number) => numbers.push(number),
                    None => status = Status::FAILURE,
                }
            }
            Some(numbers)
        };

        let mut listed: Vec<usize> = Vec::new();
        let mut out = Vec::new();
        let mut numbers: Vec<usize> = self.job_table.iter().map(|job| job.number).collect();
        numbers.sort_unstable();
        if let Some(selected) = selected.as_mut() {
            selected.sort_unstable();
            selected.dedup();
            numbers.retain(|number| selected.contains(number));
        }
        for number in numbers {
            let Some(job) = self.job_table.get(number) else {
                continue;
            };
            let filtered = match job.state {
                JobState::Running => stopped_only,
                JobState::Stopped { .. } => running_only,
                JobState::Done { .. } => running_only || stopped_only,
            };
            if filtered {
                continue;
            }
            if pids_only {
                out.extend_from_slice(job.pgid.to_string().as_bytes());
                out.push(b'\n');
            } else if long {
                out.extend_from_slice(&long_lines(&self.job_table, job));
            } else {
                out.extend_from_slice(&short_line(&self.job_table, job));
                out.push(b'\n');
            }
            listed.push(number);
        }
        let write = self.write_out(&out);
        // bash forgets a finished job once it has been listed.
        for number in listed {
            let done = matches!(
                self.job_table.get(number).map(|job| job.state),
                Some(JobState::Done { .. })
            );
            if done {
                self.remove_job(number);
            }
        }
        if !write.is_success() { write } else { status }
    }

    /// `fg [spec]`
    pub(crate) fn bi_fg(&mut self, argv: &[Vec<u8>]) -> Status {
        let operand = match option_check(argv) {
            Ok(operand) => operand,
            Err(option) => {
                self.report(
                    bsh_runtime::codes::USAGE,
                    0..0,
                    format!("fg: {}: invalid option", bytes_text(&option)),
                );
                return Status::USAGE;
            }
        };
        let number = match operand {
            Some(operand) => match self.resolve_job_operand(operand, "fg") {
                Some(number) => number,
                None => return Status::FAILURE,
            },
            None => match self.job_table.current() {
                Some(number) => number,
                None => {
                    self.report(
                        bsh_runtime::codes::NOT_FOUND,
                        0..0,
                        "fg: current: no such job",
                    );
                    return Status::FAILURE;
                }
            },
        };
        self.foreground_job(number)
    }

    /// `bg [spec]`
    pub(crate) fn bi_bg(&mut self, argv: &[Vec<u8>]) -> Status {
        let operand = match option_check(argv) {
            Ok(operand) => operand,
            Err(option) => {
                self.report(
                    bsh_runtime::codes::USAGE,
                    0..0,
                    format!("bg: {}: invalid option", bytes_text(&option)),
                );
                return Status::USAGE;
            }
        };
        let number = match operand {
            Some(operand) => match self.resolve_job_operand(operand, "bg") {
                Some(number) => number,
                None => return Status::FAILURE,
            },
            None => match self.job_table.current() {
                Some(number) => number,
                None => {
                    self.report(
                        bsh_runtime::codes::NOT_FOUND,
                        0..0,
                        "bg: current: no such job",
                    );
                    return Status::FAILURE;
                }
            },
        };
        self.background_job(number)
    }

    /// Bring a job to the foreground, print its command, and wait for it.
    fn foreground_job(&mut self, number: usize) -> Status {
        let Some(job) = self.job_table.get(number) else {
            return Status::FAILURE;
        };
        match job.state {
            JobState::Done { .. } => {
                let line = notice_line(&self.job_table, job);
                self.report(
                    bsh_runtime::codes::NOT_FOUND,
                    0..0,
                    "fg: job has terminated",
                );
                let _ = self.write_err(&line);
                let _ = self.write_err(b"\n");
                self.remove_job(number);
                return Status::FAILURE;
            }
            JobState::Running | JobState::Stopped { .. } => {}
        }
        let command = job.command.clone();
        let parts = job.parts.clone();
        let pids = job.pids.clone();
        let pgid = job.pgid;
        let mut line = command.clone();
        line.push(b'\n');
        let _ = self.write_out(&line);
        // It is no longer stopped: resume it and drop it from the rotation
        // until it stops again.
        for pid in &pids {
            self.background.insert(*pid, None);
        }
        if let Some(job) = self.job_table.get_mut(number) {
            job.state = JobState::Running;
            job.running = job.pids.len();
        }
        self.job_table.clear_stop(number);
        let _ = self.signal_job(number, bsh_sys::SIGCONT);
        let pgid = self.foreground_job_control().then_some(pgid);
        let outcomes =
            match self.wait_foreground_children(&pids, pgid, &command, parts, Some(number)) {
                Ok(outcomes) => outcomes,
                Err(error) => {
                    self.report(
                        crate::codes::INTERNAL,
                        0..0,
                        format!("fg: cannot wait: {error}"),
                    );
                    return Status::FAILURE;
                }
            };
        outcomes
            .last()
            .map(|outcome| Status(outcome.exit_status()))
            .unwrap_or(Status::FAILURE)
    }

    /// Resume a stopped job in the background.
    fn background_job(&mut self, number: usize) -> Status {
        let Some(job) = self.job_table.get(number) else {
            return Status::FAILURE;
        };
        match job.state {
            JobState::Running => {
                self.report(
                    bsh_runtime::codes::USAGE,
                    0..0,
                    format!("bg: job {number} already in background"),
                );
                return Status::FAILURE;
            }
            JobState::Done { .. } => {
                let line = notice_line(&self.job_table, job);
                self.report(
                    bsh_runtime::codes::NOT_FOUND,
                    0..0,
                    "bg: job has terminated",
                );
                let _ = self.write_err(&line);
                let _ = self.write_err(b"\n");
                self.remove_job(number);
                return Status::FAILURE;
            }
            JobState::Stopped { .. } => {}
        }
        // bash prints the marker the job had before it was resumed.
        let marker = self.job_table.marker(number);
        let command = job.command.clone();
        let pids = job.pids.clone();
        let mut line = format!("[{number}]{} ", char::from(marker)).into_bytes();
        line.extend_from_slice(&command);
        line.extend_from_slice(b" &\n");
        let _ = self.write_out(&line);
        for pid in &pids {
            self.background.insert(*pid, None);
        }
        if let Some(job) = self.job_table.get_mut(number) {
            job.state = JobState::Running;
            job.running = job.pids.len();
            job.notified = true;
        }
        self.job_table.clear_stop(number);
        let _ = self.signal_job(number, bsh_sys::SIGCONT);
        Status::SUCCESS
    }

    /// `disown [-h] [-a] [-r] [spec…]`
    pub(crate) fn bi_disown(&mut self, argv: &[Vec<u8>]) -> Status {
        let mut keep = false;
        let mut all = false;
        let mut running_only = false;
        let mut index = 1;
        while index < argv.len() {
            let argument = &argv[index];
            if argument == b"--" {
                index += 1;
                break;
            }
            if argument.len() > 1 && argument[0] == b'-' {
                for &flag in &argument[1..] {
                    match flag {
                        b'h' => keep = true,
                        b'a' => all = true,
                        b'r' => running_only = true,
                        other => {
                            self.report(
                                bsh_runtime::codes::USAGE,
                                0..0,
                                format!("disown: -{}: invalid option", char::from(other)),
                            );
                            let _ = self.write_err(
                                b"disown: usage: disown [-h] [-ar] [jobspec ... | pid ...]\n",
                            );
                            return Status::USAGE;
                        }
                    }
                }
                index += 1;
                continue;
            }
            break;
        }
        let operands = &argv[index..];

        let mut status = Status::SUCCESS;
        let mut targets: Vec<usize> = Vec::new();
        if all || (operands.is_empty() && running_only) {
            targets.extend(self.job_table.iter().map(|job| job.number));
        } else if operands.is_empty() {
            match self.job_table.current() {
                Some(number) => targets.push(number),
                None => {
                    self.report(
                        bsh_runtime::codes::NOT_FOUND,
                        0..0,
                        "disown: current: no such job",
                    );
                    return Status::FAILURE;
                }
            }
        } else {
            for operand in operands {
                if operand.first() == Some(&b'%') {
                    match self.resolve_job_operand(operand, "disown") {
                        Some(number) => targets.push(number),
                        None => status = Status::FAILURE,
                    }
                    continue;
                }
                let pid = std::str::from_utf8(operand)
                    .ok()
                    .and_then(|text| text.parse::<i32>().ok());
                let found =
                    pid.and_then(|pid| self.job_table.find_by_pid(pid).map(|job| job.number));
                match found {
                    Some(number) => targets.push(number),
                    None => {
                        self.report(
                            bsh_runtime::codes::NOT_FOUND,
                            0..0,
                            format!("disown: {}: no such job", bytes_text(operand)),
                        );
                        status = Status::FAILURE;
                    }
                }
            }
        }
        targets.sort_unstable();
        targets.dedup();
        for number in targets {
            let filtered = running_only
                && !matches!(
                    self.job_table.get(number).map(|job| job.state),
                    Some(JobState::Running)
                );
            if filtered {
                continue;
            }
            if keep {
                if let Some(job) = self.job_table.get_mut(number) {
                    job.hup = false;
                }
            } else {
                self.remove_job(number);
            }
        }
        status
    }

    /// Wait for a whole job. A stopped job returns `128 + signal` with bash's
    /// warning instead of blocking; a finished one reports its recorded status.
    fn wait_job(&mut self, number: usize) -> Waited {
        let Some(job) = self.job_table.get(number) else {
            return Waited::NoSuchJob;
        };
        match job.state {
            JobState::Done { status, .. } => {
                self.remove_job(number);
                Waited::Status(status)
            }
            JobState::Stopped { signal } => {
                self.warn(&format!("warning: wait_for_job: job {number} is stopped"));
                Waited::Status(Status(128 + signal))
            }
            JobState::Running => {
                let pids = job.pids.clone();
                let mut last = Status::SUCCESS;
                for pid in pids {
                    match bsh_sys::wait_pid(pid, self.foreground_job_control()) {
                        Ok(bsh_sys::WaitOutcome::Stopped(signal)) => {
                            if let Some(job) = self.job_table.get_mut(number) {
                                job.state = JobState::Stopped { signal };
                                job.running = job.pids.len();
                                job.notified = false;
                            }
                            self.job_table.touch_stop(number);
                            self.warn(&format!("warning: wait_for_job: job {number} is stopped"));
                            return Waited::Status(Status(128 + signal));
                        }
                        Ok(outcome) => {
                            last = Status(outcome.exit_status());
                            self.note_reaped(pid, outcome);
                        }
                        Err(error) if error.raw_os_error() == Some(bsh_sys::ECHILD) => {
                            self.remove_job(number);
                            return Waited::NoChild;
                        }
                        Err(_) => {
                            self.remove_job(number);
                            return Waited::NoChild;
                        }
                    }
                }
                self.remove_job(number);
                Waited::Status(last)
            }
        }
    }
}

/// The single operand `fg`/`bg` accept, if any; `Err` carries the bad option.
fn option_check(argv: &[Vec<u8>]) -> Result<Option<&[u8]>, Vec<u8>> {
    match argv.get(1).map(Vec::as_slice) {
        None => Ok(None),
        Some(b"--") => Ok(argv.get(2).map(Vec::as_slice)),
        Some(operand) if operand.first() == Some(&b'-') && operand.len() > 1 => {
            Err(operand.to_vec())
        }
        Some(operand) => Ok(Some(operand)),
    }
}

/// The job-spec text of `%…`: strip one `%`, and treat `%%` as `%+`.
fn job_spec(operand: &[u8]) -> &[u8] {
    let spec = operand.strip_prefix(b"%").unwrap_or(operand);
    if spec == b"%" { b"" } else { spec }
}

/// Lossy text for a diagnostic. Job specs and command text are bytes; the
/// message format is the only place they become text.
fn bytes_text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// The status word for a job, without padding.
fn status_text(state: JobState) -> Vec<u8> {
    match state {
        JobState::Running => b"Running".to_vec(),
        JobState::Stopped { signal } => match signal {
            bsh_sys::SIGTTIN => b"Stopped (tty input)".to_vec(),
            bsh_sys::SIGTTOU => b"Stopped (tty output)".to_vec(),
            bsh_sys::SIGTSTP => b"Stopped".to_vec(),
            _ => b"Stopped (signal)".to_vec(),
        },
        JobState::Done {
            signal: Some(signal),
            ..
        } => signal_description(signal).as_bytes().to_vec(),
        JobState::Done {
            status,
            signal: None,
        } => {
            if status.exit_code() == 0 {
                b"Done".to_vec()
            } else {
                format!("Exit {}", status.exit_code()).into_bytes()
            }
        }
    }
}

/// Left-justify a status word in bash's 27-column field.
fn status_field(state: JobState) -> Vec<u8> {
    let mut field = status_text(state);
    while field.len() < 27 {
        field.push(b' ');
    }
    field
}

/// `[N]M  STATUS…COMMAND` — the short form and the notification line.
fn short_line(table: &JobTable, job: &Job) -> Vec<u8> {
    let mut line =
        format!("[{}]{}  ", job.number, char::from(table.marker(job.number))).into_bytes();
    line.extend_from_slice(&status_field(job.state));
    line.extend_from_slice(&job.command);
    if matches!(job.state, JobState::Running) {
        line.extend_from_slice(b" &");
    }
    line
}

/// A notification line (bash's "Done"/"Stopped" notice). Same shape as the
/// listing line; a Done job is not running, so it never gains `&`.
fn notice_line(table: &JobTable, job: &Job) -> Vec<u8> {
    let mut line =
        format!("[{}]{}  ", job.number, char::from(table.marker(job.number))).into_bytes();
    line.extend_from_slice(&status_field(job.state));
    line.extend_from_slice(&job.command);
    line
}

/// The `-l` form: one line per member, continuation lines prefixed with `|`.
fn long_lines(table: &JobTable, job: &Job) -> Vec<u8> {
    let mut out = Vec::new();
    let marker = char::from(table.marker(job.number));
    let mut parts: Vec<Vec<u8>> = Vec::with_capacity(job.pids.len());
    for index in 0..job.pids.len() {
        if index == 0 {
            parts.push(job.command.clone());
        } else {
            parts.push(job.parts.get(index).cloned().unwrap_or_default());
        }
    }
    for (index, pid) in job.pids.iter().enumerate() {
        if index == 0 {
            out.extend_from_slice(format!("[{0}]{1} {2:>5} ", job.number, marker, pid).as_bytes());
            out.extend_from_slice(&status_field(job.state));
            out.extend_from_slice(&parts[0]);
        } else {
            out.extend_from_slice(format!("     {pid:>5} ").as_bytes());
            out.extend_from_slice(&[b' '; 25]);
            out.extend_from_slice(b"| ");
            out.extend_from_slice(&parts[index]);
        }
        // Only a background, still-running job ends with ` &`.
        if index + 1 == job.pids.len() && matches!(job.state, JobState::Running) {
            out.extend_from_slice(b" &");
        }
        out.push(b'\n');
    }
    if job.pids.is_empty() {
        out.extend_from_slice(&short_line(table, job));
        out.push(b'\n');
    }
    out
}

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
    interp.report(
        bsh_runtime::codes::USAGE,
        0..0,
        format!("{name}: {message}"),
    );
    Status::USAGE
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

    /// Resolve one `wait`/`kill` operand to a job number or pid.
    fn operand_target(&mut self, operand: &[u8], program: &str) -> Result<Target, Waited> {
        if operand.first() == Some(&b'%') {
            let job = job_spec(operand);
            return match self.resolve_job_spec(job) {
                Ok(number) => Ok(Target::Job(number)),
                Err(SpecError::NoSuch { spec }) => {
                    self.report(
                        bsh_runtime::codes::NOT_FOUND,
                        0..0,
                        format!("{program}: %{}: no such job", bytes_text(&spec)),
                    );
                    Err(Waited::NoSuchJob)
                }
                Err(SpecError::Ambiguous { spec }) => {
                    self.report(
                        bsh_runtime::codes::USAGE,
                        0..0,
                        format!("{program}: {}: ambiguous job spec", bytes_text(&spec)),
                    );
                    Err(Waited::NoSuchJob)
                }
            };
        }
        match std::str::from_utf8(operand)
            .ok()
            .and_then(|text| text.parse().ok())
        {
            Some(pid) if pid > 0 => Ok(Target::Pid(pid)),
            _ => {
                self.report(
                    bsh_runtime::codes::USAGE,
                    0..0,
                    format!(
                        "{program}: {}: arguments must be process or job IDs",
                        bytes_text(operand)
                    ),
                );
                Err(Waited::Invalid)
            }
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
        let wuntraced = self.foreground_job_control();
        if let Some(slot) = self.background.get_mut(&pid) {
            if let Some(status) = slot.take() {
                return Waited::Status(status);
            }
            return match bsh_sys::wait_pid(pid, wuntraced) {
                Ok(outcome) => {
                    self.report_signal_death(outcome);
                    self.note_reaped(pid, outcome);
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
        match bsh_sys::wait_pid(pid, wuntraced) {
            Ok(outcome) => {
                self.note_reaped(pid, outcome);
                self.report_signal_death(outcome);
                Waited::Status(Status(outcome.exit_status()))
            }
            Err(_) => Waited::NoChild,
        }
    }

    fn wait_operands(&mut self, operands: &[Vec<u8>]) -> Status {
        let mut status = Status::SUCCESS;
        for operand in operands {
            match self.operand_target(operand, "wait") {
                Err(waited) => {
                    status = status_of(waited);
                }
                Ok(Target::Job(number)) => {
                    status = status_of(self.wait_job(number));
                }
                Ok(Target::Pid(pid)) => {
                    let waited = self.wait_one(pid);
                    self.forget_job(pid);
                    status = status_of(waited);
                }
            }
        }
        status
    }

    /// `wait` with no operand: every child, and the status is 0 even when the
    /// children failed (the suite pins that "the status is lost").
    fn wait_all(&mut self) -> Status {
        if self.foreground_job_control() {
            return self.wait_all_jobs();
        }
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
                        self.note_reaped(pid, outcome);
                        // `wait` consumed the child: it is no longer a job,
                        // and a second `wait PID` must say 127.
                        self.forget_job(pid);
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

    /// `wait` with job control: running jobs are waited for, stopped ones are
    /// reported once each and skipped, so a shell whose only job is stopped
    /// returns to the prompt instead of hanging.
    fn wait_all_jobs(&mut self) -> Status {
        let mut warned: Vec<usize> = Vec::new();
        loop {
            self.reap_background();
            let mut pending: Vec<(usize, Vec<i32>)> = Vec::new();
            let mut stopped: Vec<(usize, i32, i32)> = Vec::new();
            let mut done: Vec<usize> = Vec::new();
            for job in self.job_table.iter() {
                match job.state {
                    JobState::Running => pending.push((job.number, job.pids.clone())),
                    JobState::Stopped { signal } => {
                        stopped.push((job.number, job.pgid, signal));
                    }
                    JobState::Done { .. } => done.push(job.number),
                }
            }
            for (number, pgid, _signal) in stopped {
                if warned.contains(&number) {
                    continue;
                }
                warned.push(number);
                self.warn(&format!("wait: warning: job {number}[{pgid}] stopped"));
            }
            for number in done {
                self.remove_job(number);
            }
            if pending.is_empty() {
                return Status::SUCCESS;
            }
            for (number, pids) in pending {
                for pid in pids {
                    match bsh_sys::wait_pid(pid, true) {
                        Ok(bsh_sys::WaitOutcome::Stopped(signal)) => {
                            if let Some(job) = self.job_table.get_mut(number) {
                                job.state = JobState::Stopped { signal };
                                job.running = job.pids.len();
                                job.notified = false;
                            }
                            self.job_table.touch_stop(number);
                            break;
                        }
                        Ok(outcome) => self.note_reaped(pid, outcome),
                        Err(_) => {
                            self.background.remove(&pid);
                            self.remove_job(number);
                            break;
                        }
                    }
                }
            }
            // If nothing running is left, the next pass returns.
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
        let mut remaining_jobs: Vec<usize> = Vec::new();
        for operand in operands {
            match self.operand_target(operand, "wait") {
                Ok(Target::Pid(pid)) => remaining.push(pid),
                Ok(Target::Job(number)) => remaining_jobs.push(number),
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
            for number in &remaining_jobs {
                if let Some(job) = self.job_table.get(*number) {
                    if let JobState::Done { status, .. } = job.state {
                        self.remove_job(*number);
                        return status;
                    }
                }
            }
            if candidates.is_empty() && remaining_jobs.is_empty() {
                return Status(127);
            }
            // With no running job left, only stopped jobs remain: bash
            // answers 127 instead of blocking forever.
            let any_running = self
                .job_table
                .iter()
                .any(|job| matches!(job.state, JobState::Running))
                || self.background.values().any(Option::is_none);
            if !any_running {
                return Status(127);
            }
            // Reap whatever finished; a 1 ms poll keeps a shell script's
            // `wait -n` loop responsive without a busy spin.
            let reaped = if self.foreground_job_control() {
                bsh_sys::wait_any_nohang_wuntraced()
            } else {
                bsh_sys::wait_any_nohang()
            };
            match reaped {
                Some((pid, outcome)) => {
                    self.note_reaped(pid, outcome);
                    if let bsh_sys::WaitOutcome::Stopped(_) = outcome {
                        continue;
                    }
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
                            .any(|pid| self.background.contains_key(pid))
                            || remaining_jobs.iter().any(|number| {
                                self.job_table
                                    .get(*number)
                                    .is_some_and(|job| matches!(job.state, JobState::Running))
                            });
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
        if let Some(number) = self.job_table.find_by_pid(pid).map(|job| job.number) {
            self.remove_job(number);
        }
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
            let target = match self.operand_target(operand, "kill") {
                Ok(target) => target,
                Err(waited) => {
                    status = status_of(waited);
                    continue;
                }
            };
            let result = match target {
                Target::Job(number) => {
                    if self.job_table.get(number).is_none() {
                        status = Status(127);
                        continue;
                    }
                    self.signal_job(number, signal)
                }
                Target::Pid(pid) => bsh_sys::send_signal(pid, signal),
            };
            if let Err(error) = result {
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
}

/// A `wait`/`kill` operand resolved to its target.
#[derive(Clone, Copy)]
enum Target {
    /// A pid, as written.
    Pid(i32),
    /// A job number, from a `%` specification.
    Job(usize),
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
        Waited::Invalid => Status::USAGE,
    }
}

impl Interp {
    /// Wait for one foreground group, handing it the terminal when
    /// `pgid` is `Some`, and recording a stopped job when anything stops.
    ///
    /// This is the single wait path every foreground site uses (`run_external`,
    /// `run_pipeline`, `eval_subshell`, `fg`), so `SIGTTOU` handling, the
    /// `SIGINT`/`SIGQUIT` ignore and the stopped-job notice cannot drift
    /// between them. `existing` is the job `fg` is resuming, if any.
    pub(crate) fn wait_foreground_children(
        &mut self,
        pids: &[i32],
        pgid: Option<i32>,
        command: &[u8],
        parts: Vec<Vec<u8>>,
        existing: Option<usize>,
    ) -> std::io::Result<Vec<bsh_sys::WaitOutcome>> {
        let terminal = pgid.map(|pgid| TerminalGuard::enter(pgid, self.shell_pgid));
        let guard = self.ignore_int_quit();
        let mut outcomes = Vec::with_capacity(pids.len());
        for pid in pids {
            let result = bsh_sys::wait_pid(*pid, pgid.is_some());
            match result {
                Ok(outcome) => outcomes.push(outcome),
                Err(error) => {
                    drop(guard);
                    drop(terminal);
                    return Err(error);
                }
            }
        }
        drop(guard);
        drop(terminal);

        let stopped = outcomes.iter().find_map(|outcome| match outcome {
            bsh_sys::WaitOutcome::Stopped(signal) => Some(*signal),
            _ => None,
        });
        if let Some(signal) = stopped {
            let number = match existing {
                Some(number) => {
                    if let Some(job) = self.job_table.get_mut(number) {
                        job.state = JobState::Stopped { signal };
                        job.running = job.pids.len();
                        job.notified = true;
                    }
                    self.job_table.touch_stop(number);
                    number
                }
                None => {
                    let number = self.job_table.insert(Job {
                        number: 0,
                        pgid: pgid.unwrap_or_else(|| pids.first().copied().unwrap_or(0)),
                        pids: pids.to_vec(),
                        parts,
                        command: command.to_vec(),
                        state: JobState::Stopped { signal },
                        running: pids.len(),
                        notified: true,
                        hup: true,
                    });
                    self.job_table.touch_stop(number);
                    number
                }
            };
            for pid in pids {
                self.background.insert(*pid, Some(Status(128 + signal)));
            }
            if let Some(job) = self.job_table.get(number) {
                let line = notice_line(&self.job_table, job);
                let _ = self.write_err(&line);
                let _ = self.write_err(b"\n");
            }
        } else if let Some(number) = existing {
            self.remove_job(number);
        }
        Ok(outcomes)
    }
}

/// Puts the terminal back in the shell's group when a foreground wait ends,
/// including on the error path and on a panic.
struct TerminalGuard {
    shell_pgid: i32,
    active: bool,
}

impl TerminalGuard {
    fn enter(pgid: i32, shell_pgid: i32) -> Self {
        let active = bsh_sys::set_foreground_pgrp(pgid).is_ok();
        Self { shell_pgid, active }
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if self.active {
            let _ = bsh_sys::set_foreground_pgrp(self.shell_pgid);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn interp() -> Interp {
        let mut interp = Interp::new(false);
        interp.set_arg0("bsh");
        interp
    }

    fn fake_job(command: &str, state: JobState) -> Job {
        Job {
            number: 0,
            pgid: 0,
            pids: vec![0],
            parts: vec![command.as_bytes().to_vec()],
            command: command.as_bytes().to_vec(),
            state,
            running: 1,
            notified: false,
            hup: true,
        }
    }

    fn monitor_interp() -> Interp {
        let mut interp = interp();
        interp.job_control = true;
        interp.state.opts.monitor = true;
        interp
    }

    #[test]
    fn numbers_are_reused_and_markers_follow_stop_order() {
        let mut table = JobTable::default();
        assert_eq!(table.insert(fake_job("sleep 100", JobState::Running)), 1);
        assert_eq!(table.insert(fake_job("sleep 200", JobState::Running)), 2);
        // No stopped job: current is the highest number.
        assert_eq!(table.current(), Some(2));
        assert_eq!(table.previous(), Some(1));
        assert_eq!(table.marker(1), b'-');
        assert_eq!(table.marker(2), b'+');
        table.insert(fake_job("sleep 300", JobState::Running));
        assert_eq!(table.current(), Some(3));
        assert_eq!(table.previous(), Some(2));
        assert_eq!(table.marker(1), b' ');
        // Stopping the middle job makes it current and promotes the highest
        // other job to previous (measured against bash).
        table.touch_stop(2);
        assert_eq!(table.marker(2), b'+');
        assert_eq!(table.marker(3), b'-');
        assert_eq!(table.marker(1), b' ');
        // Resuming it demotes it out of both markers.
        table.clear_stop(2);
        assert_eq!(table.marker(3), b'+');
        assert_eq!(table.marker(2), b'-');
        // Removing the current job promotes the previous one.
        table.remove(3);
        assert_eq!(table.current(), Some(2));
        assert_eq!(table.previous(), Some(1));
        // A freed number is reused immediately, as bash does.
        assert_eq!(table.insert(fake_job("sleep 400", JobState::Running)), 3);
    }

    #[test]
    fn job_specs_resolve_by_number_and_text() {
        let mut interp = monitor_interp();
        interp
            .job_table
            .insert(fake_job("sleep 100", JobState::Running));
        interp
            .job_table
            .insert(fake_job("sleep 200", JobState::Running));
        assert_eq!(interp.resolve_job_spec(b""), Ok(2));
        assert_eq!(interp.resolve_job_spec(b"+"), Ok(2));
        assert_eq!(interp.resolve_job_spec(b"-"), Ok(1));
        assert_eq!(interp.resolve_job_spec(b"1"), Ok(1));
        assert_eq!(interp.resolve_job_spec(b"?100"), Ok(1));
        assert_eq!(interp.resolve_job_spec(b"sleep 2"), Ok(2));
        assert!(matches!(
            interp.resolve_job_spec(b"sleep"),
            Err(SpecError::Ambiguous { .. })
        ));
        assert!(matches!(
            interp.resolve_job_spec(b"nope"),
            Err(SpecError::NoSuch { .. })
        ));
        assert!(matches!(
            interp.resolve_job_spec(b"9"),
            Err(SpecError::NoSuch { .. })
        ));
        // `%%` is `%+`; the extra `%` is stripped exactly once.
        assert_eq!(job_spec(b"%%"), b"");
        assert_eq!(job_spec(b"%1"), b"1");
    }

    #[test]
    fn wait_on_a_stopped_job_warns_and_returns_148_without_removing_it() {
        let mut interp = monitor_interp();
        interp.job_table.insert(fake_job(
            "sleep 100",
            JobState::Stopped {
                signal: bsh_sys::SIGTSTP,
            },
        ));
        let status = interp.bi_wait(&[b"wait".to_vec(), b"%1".to_vec()]);
        assert_eq!(status.0, 148);
        assert!(interp.job_table.get(1).is_some());
    }

    #[test]
    fn wait_all_does_not_hang_on_a_stopped_job() {
        let mut interp = monitor_interp();
        interp.job_table.insert(fake_job(
            "sleep 100",
            JobState::Stopped {
                signal: bsh_sys::SIGTSTP,
            },
        ));
        assert_eq!(interp.bi_wait(&[b"wait".to_vec()]).0, 0);
    }

    #[test]
    fn finished_jobs_are_reported_and_removed_once() {
        let mut interp = monitor_interp();
        interp.job_table.insert(fake_job(
            "sleep 0.1",
            JobState::Done {
                status: Status::SUCCESS,
                signal: None,
            },
        ));
        let notice = interp.emit_job_notices();
        assert_eq!(notice, b"[1]+  Done                       sleep 0.1\n");
        assert!(interp.job_table.get(1).is_none());
        assert!(interp.emit_job_notices().is_empty());
    }

    #[test]
    fn fg_and_bg_reject_terminated_and_running_jobs() {
        let mut interp = monitor_interp();
        interp.job_table.insert(fake_job(
            "sleep 0.1",
            JobState::Done {
                status: Status::SUCCESS,
                signal: None,
            },
        ));
        assert_eq!(interp.bi_fg(&[b"fg".to_vec()]).0, 1);
        assert!(interp.job_table.get(1).is_none());

        interp
            .job_table
            .insert(fake_job("sleep 100", JobState::Running));
        assert_eq!(interp.bi_bg(&[b"bg".to_vec()]).0, 1);
        assert!(interp.job_table.get(1).is_some());
    }

    #[test]
    fn disown_removes_or_keeps_and_validates_options() {
        let mut interp = monitor_interp();
        interp
            .job_table
            .insert(fake_job("sleep 100", JobState::Running));
        assert_eq!(interp.bi_disown(&[b"disown".to_vec(), b"-h".to_vec()]).0, 0);
        assert!(interp.job_table.get(1).is_some());
        assert!(!interp.job_table.get(1).unwrap().hup);
        assert_eq!(interp.bi_disown(&[b"disown".to_vec(), b"%1".to_vec()]).0, 0);
        assert!(interp.job_table.get(1).is_none());
        assert_eq!(interp.bi_disown(&[b"disown".to_vec(), b"-x".to_vec()]).0, 2);
        assert_eq!(interp.bi_disown(&[b"disown".to_vec(), b"%9".to_vec()]).0, 1);
    }

    #[test]
    fn jobs_filters_and_rejects_unknown_options() {
        let mut interp = monitor_interp();
        interp
            .job_table
            .insert(fake_job("sleep 100", JobState::Running));
        interp.job_table.insert(fake_job(
            "sleep 200",
            JobState::Stopped {
                signal: bsh_sys::SIGTSTP,
            },
        ));
        assert_eq!(interp.bi_jobs(&[b"jobs".to_vec(), b"-r".to_vec()]).0, 0);
        assert_eq!(interp.bi_jobs(&[b"jobs".to_vec(), b"-s".to_vec()]).0, 0);
        assert_eq!(interp.bi_jobs(&[b"jobs".to_vec(), b"-x".to_vec()]).0, 2);
        assert_eq!(interp.bi_jobs(&[b"jobs".to_vec(), b"%9".to_vec()]).0, 1);
    }
}
